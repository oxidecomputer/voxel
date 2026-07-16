#!/bin/bash
#
# build-frr.sh [version] - build a voxel-frr router image with voxel-init baked.
#
# Cross-compiles the in-guest agent for linux (fully static musl, since the
# router guest is debian), stages it into the FRR builder cargo-bay, and bakes
# the debian image via build-image.sh (install-frr.sh installs FRR + copies the
# agent to /opt/oxide/voxel-init). Counterpart to build-cp.sh for the gimlet.
#
# RUN ON THE HELIOS BOX. ~10 min (apt install + capture; no omicron build).
#
# On a box using voxel's isolated external segment (no LAN DHCP), export
#   EXT_INTERFACE=voxel_ext_stub0
#   VOXEL_BUILDER_NET="192.168.1.198/24 192.168.1.199"
# before running (adjust for your [external] subnet/host_ip). build-image.sh
# stages that as `builder-net` in the cargo-bay, and install-frr.sh applies it
# as a static address on the builder VM in place of DHCP.
#
set -euo pipefail

HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
VERSION="${1:-${VOXEL_FRR_VERSION:-proto}}"
IMAGE_NAME="${IMAGE_NAME:-voxel-frr-${VERSION}}"
FALCON_DATASET="${FALCON_DATASET:-rpool/falcon}"
CAPTURE_MODE="${CAPTURE_MODE:-zfs}"
CARGO_BAY="${HERE}/cargo-bay/vbuild-frr"
TARGET="x86_64-unknown-linux-musl"

export PATH="${HOME}/.cargo/bin:/opt/ooce/bin:${PATH}"
log() { echo "[build-frr] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: run on the Helios box"; exit 1; }

# --- 1. cross-compile the agent (static linux-musl) ---------------------------
log "cross-compiling voxel-init for ${TARGET} (static)"
rustup target add "${TARGET}" >/dev/null 2>&1 || true
( cd "${HERE}/.." && RUSTFLAGS="-C linker=rust-lld -C link-self-contained=yes" \
    cargo build -p voxel-init --release --target "${TARGET}" )

# --- 2. stage it into the FRR builder cargo-bay -------------------------------
mkdir -p "${CARGO_BAY}"
cp "${HERE}/../target/${TARGET}/release/voxel-init" "${CARGO_BAY}/voxel-init"
chmod +x "${CARGO_BAY}/voxel-init"

# --- 3. bake the image --------------------------------------------------------
log "baking ${IMAGE_NAME} via build-image.sh (CAPTURE_MODE=${CAPTURE_MODE})"
VERSION="${VERSION}" IMAGE_NAME="${IMAGE_NAME}" \
    BASE_IMAGE="debian-13.2" INSTALL_SCRIPT="install-frr.sh" \
    FALCON_DATASET="${FALCON_DATASET}" CAPTURE_MODE="${CAPTURE_MODE}" \
    CARGO_BAY="${CARGO_BAY}" VBUILD_DISK_GB="20" \
    bash "${HERE}/build-image.sh"

log "done: ${IMAGE_NAME}"
log "use it: voxel config set image.frr ${IMAGE_NAME} && voxel launch"
