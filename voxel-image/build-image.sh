#!/bin/bash
#
# build-image.sh - Voxel image build (snapshot-first path); control plane or FRR.
#
# RUN ON A HELIOS HOST (needs bhyve/propolis/falcon + zfs). On a Mac this is a
# no-op; it must execute where falcon can launch a node.
#
# Flow:
#   1. Stage INSTALL_SCRIPT into the builder's cargo-bay.
#   2. Build + launch the single-node builder (boots BASE_IMAGE, runs INSTALL_SCRIPT).
#   3. Verify the in-guest ready marker.
#   4. Quiesce + hyperstop the node.
#   5. Capture the node zvol (CAPTURE_MODE: raw .raw.xz, or fast zfs send/recv).
#   6. Destroy the builder.
#
# Capture modes (CAPTURE_MODE):
#   raw  (default)  node zvol -> portable out/<name>_0.raw.xz via `dd | xz`.
#                   REGISTER=1 also imports it back as a falcon base image
#                   (streaming xz decompress into a presized zvol). This .raw.xz
#                   is the distributable / CI / S3 artifact.
#   zfs             box-local fast path: `zfs send | zfs recv` the node zvol
#                   straight into <dataset>/img/<name>@base - only allocated
#                   blocks move (~GBs, not the full 100GiB). No portable artifact.
#
# We do NOT use `falcon snapshot`: it snapshots <source>@base again instead of
# creating <dest>@base, so it never produces the img/<name>@base the boot path
# needs (falcon lib/src/cli.rs:498).
#
# Presets:
#   voxel-cp  : BASE_IMAGE=helios-3.0  INSTALL_SCRIPT=install-cp.sh
#               (needs CARGO_BAY/omicron staged)   IMAGE_NAME=voxel-cp-<ver>
#   voxel-frr : BASE_IMAGE=debian-13.2 INSTALL_SCRIPT=install-frr.sh
#               IMAGE_NAME=voxel-frr-<ver>  VBUILD_DISK_GB=20
#
# Required:
#   VERSION                image version label
# Optional (env, with defaults):
#   FALCON_DATASET         zfs dataset falcon uses (default rpool/falcon, matching
#                          falcon; override per box, e.g. testbed/falcon)
#   BASE_IMAGE             falcon base image to boot        (default helios-3.0)
#   INSTALL_SCRIPT         script in this dir to run + bake (default install-cp.sh)
#   IMAGE_NAME             output image name            (default voxel-cp-<VERSION>)
#   CAPTURE_MODE           raw (default) | zfs
#   REGISTER               raw mode only: "1" also imports as a falcon base image
#   VOXEL_BUILD_NAME       deployment name; MUST match builder (default voxel_build)
#   OUT                    output dir for raw artifact      (default ./out)
#   CARGO_BAY              builder cargo-bay dir            (default ./cargo-bay/vbuild)
#   VBUILD_DISK_GB         node disk reserve, GB (to builder; default 100)
#
set -euo pipefail

: "${VERSION:?set VERSION to the image version label}"
FALCON_DATASET="${FALCON_DATASET:-rpool/falcon}"
CAPTURE_MODE="${CAPTURE_MODE:-raw}"
BASE_IMAGE="${BASE_IMAGE:-helios-3.0}"
INSTALL_SCRIPT="${INSTALL_SCRIPT:-install-cp.sh}"
IMAGE_NAME="${IMAGE_NAME:-voxel-cp-${VERSION}}"
CARGO_BAY="${CARGO_BAY:-./cargo-bay/vbuild}"
DEPLOY="${VOXEL_BUILD_NAME:-voxel_build}"  # falcon names: no hyphens
NODE="vbuild"
OUT="${OUT:-./out}"
HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
BUILDER="${HERE}/../target/debug/voxel-image-builder"
ARTIFACT="${OUT}/${IMAGE_NAME}_0.raw.xz"

log() { echo "[build-image] $*"; }

# --- preconditions ------------------------------------------------------------
[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: must run on a Helios/illumos host"; exit 1; }
[[ -f "${HERE}/${INSTALL_SCRIPT}" ]] || { log "FATAL: install script ${HERE}/${INSTALL_SCRIPT} not found"; exit 1; }
mkdir -p "${CARGO_BAY}"
# install-cp.sh additionally needs CARGO_BAY/omicron staged; it self-validates.

export VOXEL_BUILD_NAME="${DEPLOY}"
export FALCON_DATASET
export VBUILD_IMAGE="${BASE_IMAGE}"
export INSTALL_SCRIPT
export VBUILD_CARGO_BAY="${CARGO_BAY}"
export VOXEL_CP_VERSION="${VERSION}"
export VOXEL_FRR_VERSION="${VERSION}"

# --- 1. stage install script --------------------------------------------------
log "staging ${INSTALL_SCRIPT} into ${CARGO_BAY}"
cp "${HERE}/${INSTALL_SCRIPT}" "${CARGO_BAY}/${INSTALL_SCRIPT}"
chmod +x "${CARGO_BAY}/${INSTALL_SCRIPT}"

# Isolated-mode static network for the builder VM: the isolated segment runs no
# DHCP server, so voxel (or the operator) passes VOXEL_BUILDER_NET="<cidr> <gw>"
# and we stage it as `builder-net` in the cargo-bay for the in-guest installer
# to apply in place of DHCP.
if [[ -n "${VOXEL_BUILDER_NET:-}" ]]; then
    log "staging builder-net (${VOXEL_BUILDER_NET}) into ${CARGO_BAY}"
    printf '%s\n' "${VOXEL_BUILDER_NET}" > "${CARGO_BAY}/builder-net"
fi

# --- 2. build + launch builder (boots BASE_IMAGE, runs INSTALL_SCRIPT) ---------
log "building voxel-image-builder"
( cd "${HERE}/.." && cargo build -p voxel-image-builder )

log "launching builder (boots ${BASE_IMAGE}, runs ${INSTALL_SCRIPT}; takes a while)"
( cd "${HERE}" && pfexec "${BUILDER}" launch )

# --- 3. verify ready marker ---------------------------------------------------
# falcon `exec` does NOT propagate the guest command's exit code, so we validate
# the marker's CONTENT (written only at the end of a successful install).
log "verifying in-guest ready marker"
MARKER="$( cd "${HERE}" && pfexec "${BUILDER}" exec "${NODE}" "cat /var/voxel-image-ready" 2>/dev/null )"
case "${MARKER}" in
    *version=*) log "ready: ${MARKER}" ;;
    *) log "FATAL: ready marker missing/empty; ${INSTALL_SCRIPT} did not complete"; exit 1 ;;
esac

# --- 4. quiesce + stop --------------------------------------------------------
# Clear the device-instance map as the LAST thing before capture: a snapshot
# image must NOT carry the build VM's NIC/PCI layout, or each deployment node
# mis-binds vioif instances (vioif0 - the SoftNPU pkt_source - goes missing and
# the switch zone won't boot). `touch /reconfigure` + absent path_to_inst makes
# every node regenerate the map for its own hardware on first boot. This is the
# only reliable spot - earlier removals get regenerated by later guest activity.
# Clear the baked device-instance map, then CLEANLY HALT (not hyperstop/SIGKILL)
# so propolis flushes the removal to the zvol - otherwise the last-second write is
# lost and the image keeps the build VM's NIC layout (vioif0 mis-binds -> switch
# zone fails). An absent map makes each deployment node rebuild it for its own
# hardware on first boot. (devfsadmd is stopped so it can't re-create the map.)
log "clearing device-instance map + clean halt (flush to disk)"
( cd "${HERE}" && pfexec "${BUILDER}" exec "${NODE}" \
    "pkill -x devfsadmd 2>/dev/null; rm -f /etc/path_to_inst; sync; sync; (sleep 1; halt) &" ) 2>/dev/null || true
log "waiting for clean shutdown to flush..."
sleep 25
log "stopping hypervisor (cleanup)"
( cd "${HERE}" && pfexec "${BUILDER}" hyperstop "${NODE}" ) 2>/dev/null || true

# --- 5. capture --------------------------------------------------------------
NODE_DS="${FALCON_DATASET}/topo/${DEPLOY}/${NODE}"
ZVOL="/dev/zvol/rdsk/${NODE_DS}"
[[ -e "${ZVOL}" ]] || { log "FATAL: node zvol not found at ${ZVOL}"; exit 1; }
VOLSIZE="$(pfexec zfs get -Hp -o value volsize "${NODE_DS}")"
BS=1048576
IMG_DS="${FALCON_DATASET}/img/${IMAGE_NAME}"   # registered base image dataset (both modes)

case "${CAPTURE_MODE}" in
zfs)
    # Fast box-local path: a full (non-incremental) zfs send is self-contained,
    # so the resulting image has no dependency on the helios base it was cloned
    # from. Only allocated blocks move.
    log "capturing (zfs send/recv) ${NODE_DS} -> ${IMG_DS}@base"
    pfexec zfs destroy -r "${NODE_DS}@base" 2>/dev/null || true
    pfexec zfs destroy -r "${IMG_DS}" 2>/dev/null || true
    pfexec zfs snapshot "${NODE_DS}@base"
    pfexec zfs send "${NODE_DS}@base" | pfexec zfs recv "${IMG_DS}"
    log "registered falcon base image ${IMG_DS}@base"
    ;;
raw)
    # Portable path. count= reads exactly the volsize, avoiding a benign trailing
    # EIO from reading one block past end-of-device.
    mkdir -p "${OUT}"
    COUNT=$(( VOLSIZE / BS ))
    log "capturing (raw) ${ZVOL} (${COUNT} MiB) -> ${ARTIFACT}"
    pfexec dd if="${ZVOL}" bs="${BS}" count="${COUNT}" status=none | xz -T0 -c > "${ARTIFACT}"
    log "wrote ${ARTIFACT} ($(du -h "${ARTIFACT}" | cut -f1))"

    if [[ "${REGISTER:-0}" == "1" ]]; then
        # Stream-decompress back into a presized zvol. import-raw-img.sh can't
        # take .xz, and volsize must equal the uncompressed raw size.
        log "registering ${IMAGE_NAME} (streaming import into ${IMG_DS})"
        pfexec zfs destroy -r "${IMG_DS}" 2>/dev/null || true
        pfexec zfs create -p -V "${VOLSIZE}" -o volblocksize=4k -o compression=lz4 "${IMG_DS}"
        xz -dc -T0 "${ARTIFACT}" | pfexec dd of="/dev/zvol/rdsk/${IMG_DS}" bs="${BS}" status=none
        pfexec zfs snapshot "${IMG_DS}@base"
        log "registered falcon base image ${IMG_DS}@base"
    fi
    ;;
*)
    log "FATAL: unknown CAPTURE_MODE='${CAPTURE_MODE}' (use raw|zfs)"; exit 1
    ;;
esac

# --- 6. cleanup --------------------------------------------------------------
log "destroying builder topology"
( cd "${HERE}" && pfexec "${BUILDER}" destroy ) || true

if [[ "${CAPTURE_MODE}" == "raw" ]]; then
    log "done. artifact: ${ARTIFACT}"
    [[ "${REGISTER:-0}" == "1" ]] && log "registered: ${FALCON_DATASET}/img/${IMAGE_NAME}@base"
else
    log "done. registered falcon base image: ${FALCON_DATASET}/img/${IMAGE_NAME}@base"
fi
log "use it in a topology by referencing node image name: ${IMAGE_NAME}"
