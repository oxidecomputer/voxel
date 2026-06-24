#!/bin/bash
#
# patch-image.sh - fold a single prebuilt component into a NEW pinned @base, the
# safe way: boot the source image as a one-node builder, place the artifact
# in-guest, clean-halt, and capture (mirrors build-image.sh, which deliberately
# never mounts the guest pool on the host). Slower than a host-side overlay but
# safe + reuses the proven capture/quiesce path; far faster than a full
# `image create` since it skips the omicron install entirely.
#
# Driven by `voxel image patch`; not meant to be run by hand. Inputs (env):
#   SRC_IMAGE    image to patch (e.g. voxel-cp-43bb5af-rd)        [required]
#   OUT_IMAGE    new image name to capture into                  [required]
#   ARTIFACT     path to the component <pkg>.tar.gz on the box    [required]
#   PKG          artifact basename (e.g. propolis-server)         [required]
#   PLACE_KIND   zone-image | gz-overlay                          [required]
#   DEST         zone-image only: on-disk path to replace         [zone-image]
#   COMPONENT    component label (for the version marker)         [default: PKG]
#   REF          ref label (for the version marker)               [default: unknown]
#   FALCON_DATASET, VBUILD_MEM_GB (default 6), VBUILD_CORES (default 4)
#
set -euo pipefail
: "${SRC_IMAGE:?set SRC_IMAGE}"; : "${OUT_IMAGE:?set OUT_IMAGE}"
: "${ARTIFACT:?set ARTIFACT}"; : "${PKG:?set PKG}"; : "${PLACE_KIND:?set PLACE_KIND}"
FALCON_DATASET="${FALCON_DATASET:-rpool/falcon}"
COMPONENT="${COMPONENT:-$PKG}"; REF="${REF:-unknown}"
HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
log() { echo "[patch-image] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: must run on a Helios/illumos host"; exit 1; }
[[ -f "$ARTIFACT" ]] || { log "FATAL: artifact $ARTIFACT not found"; exit 1; }

# Stage the artifact into a throwaway cargo-bay (the builder mounts it at
# /opt/cargo-bay; build-image.sh drops the place script in here too). NB
# build-image.sh keys off CARGO_BAY (not VBUILD_CARGO_BAY, which it derives), so
# we must pass CARGO_BAY for both the script copy AND the artifact to land here.
STAGE="$(mktemp -d /var/tmp/voxel-patch-stage.XXXXXX)"
cp "$ARTIFACT" "$STAGE/${PKG}.tar.gz"

# Generate the in-guest place script. build-image.sh requires INSTALL_SCRIPT to
# live under HERE and runs it as `cd /opt/cargo-bay && bash ./<script>`. It then
# validates a `version=` ready marker - so we CLEAR the stale baked marker first
# and (re)write a fresh one only on success, making a failed placement fail loud
# instead of false-passing on the image's original marker. A `set -x` trace +
# the placement log are written to the cargo-bay (host-visible via $STAGE).
PLACE="$HERE/.voxel-patch-place.sh"
{
  echo '#!/bin/bash'
  echo 'set -euxo pipefail'
  echo 'exec > /opt/cargo-bay/.place.log 2>&1'
  echo 'log() { echo "[patch-place] $*"; }'
  echo 'rm -f /var/voxel-image-ready'   # clear stale marker: no false success
  echo "TB=/opt/cargo-bay/${PKG}.tar.gz"
  echo 'ls -la /opt/cargo-bay'
  echo '[[ -f "$TB" ]] || { log "FATAL: $TB missing"; exit 1; }'
  case "$PLACE_KIND" in
    zone-image)
      : "${DEST:?set DEST for zone-image}"
      echo "log 'replacing ${DEST}'"
      echo "cp \"\$TB\" '${DEST}'"
      echo "digest -a sha256 '${DEST}' || true"
      ;;
    gz-overlay)
      echo "log 'overlaying ${PKG} root/ onto /'"
      echo 'T=/var/tmp/voxel-patch-x; rm -rf "$T"; mkdir -p "$T"'
      echo '( cd "$T" && gzcat "$TB" | tar xf - root )'
      echo '( cd "$T/root" && tar cf - . | ( cd / && tar xf - ) )'
      echo 'rm -rf "$T"'
      ;;
    *)
      log "FATAL: unknown PLACE_KIND=$PLACE_KIND"; rm -rf "$STAGE"; rm -f "$PLACE"; exit 1
      ;;
  esac
  # Verifiable sentinel (persists in the captured image) + the ready marker.
  echo 'mkdir -p /opt/oxide'
  echo "printf 'component=%s ref=%s placed=%s\\n' '${COMPONENT}' '${REF}' \"\$(date '+%Y-%m-%dT%H:%M:%S')\" > /opt/oxide/.voxel-patched"
  echo "printf 'version=patch-%s-%s built=%s\\n' '${COMPONENT}' '${REF}' \"\$(date '+%Y-%m-%dT%H:%M:%S')\" > /var/voxel-image-ready"
  echo 'log "patch placed; sentinel + marker written"'
} > "$PLACE"
chmod +x "$PLACE"

cleanup() {
  # Surface the in-guest placement log (written to the host-visible cargo-bay)
  # for debugging, then remove the temp artifacts.
  cp "$STAGE/.place.log" /tmp/voxel-place.log 2>/dev/null || true
  rm -f "$PLACE"; rm -rf "$STAGE"
}
trap cleanup EXIT

log "boot-modify-capture: ${SRC_IMAGE} -> ${OUT_IMAGE} (${COMPONENT} @ ${REF})"
VERSION="patch-${COMPONENT}-${REF}" \
FALCON_DATASET="${FALCON_DATASET}" \
BASE_IMAGE="${SRC_IMAGE}" \
INSTALL_SCRIPT=".voxel-patch-place.sh" \
IMAGE_NAME="${OUT_IMAGE}" \
CAPTURE_MODE="zfs" \
VOXEL_BUILD_NAME="voxel_patch" \
CARGO_BAY="${STAGE}" \
VBUILD_MEM_GB="${VBUILD_MEM_GB:-6}" \
VBUILD_CORES="${VBUILD_CORES:-4}" \
VBUILD_DISK_GB="${VBUILD_DISK_GB:-100}" \
    bash "${HERE}/build-image.sh"

log "done: ${FALCON_DATASET}/img/${OUT_IMAGE}@base  (use: voxel config set image.cp ${OUT_IMAGE})"
