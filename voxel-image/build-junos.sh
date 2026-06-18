#!/bin/bash
#
# build-junos.sh [version] - build a Juniper cRPD router image.
#
# RUN ON THE HELIOS BOX. Bakes Docker, the cRPD image, and the generic
# container-router helper into a Debian Falcon image. License and topology config
# are intentionally applied at launch, not baked into this image.
#
set -euo pipefail

HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
VERSION="${1:-${JUNOS_VERSION:-23.2R1.13}}"
IMAGE_NAME="${IMAGE_NAME:-junos-23.2}"
FALCON_DATASET="${FALCON_DATASET:-rpool/falcon}"
CAPTURE_MODE="${CAPTURE_MODE:-zfs}"
CARGO_BAY="${HERE}/cargo-bay/vbuild-junos"
TARGET="x86_64-unknown-linux-musl"

export PATH="${HOME}/.cargo/bin:/opt/ooce/bin:${PATH}"
log() { echo "[build-junos] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: run on the Helios box"; exit 1; }

log "cross-compiling voxel-container-router for ${TARGET} (static)"
rustup target add "${TARGET}" >/dev/null 2>&1 || true
( cd "${HERE}/.." && RUSTFLAGS="-C linker=rust-lld -C link-self-contained=yes" \
    cargo build -p voxel-container-router --release --target "${TARGET}" )

mkdir -p "${CARGO_BAY}"
cp "${HERE}/../target/${TARGET}/release/voxel-container-router" \
    "${CARGO_BAY}/voxel-container-router"
chmod +x "${CARGO_BAY}/voxel-container-router"

log "baking ${IMAGE_NAME} via build-image.sh (CAPTURE_MODE=${CAPTURE_MODE})"
VERSION="${VERSION}" IMAGE_NAME="${IMAGE_NAME}" \
    BASE_IMAGE="debian-13.2" INSTALL_SCRIPT="install-junos.sh" \
    FALCON_DATASET="${FALCON_DATASET}" CAPTURE_MODE="${CAPTURE_MODE}" \
    CARGO_BAY="${CARGO_BAY}" VBUILD_DISK_GB="20" \
    bash "${HERE}/build-image.sh"

log "done: ${IMAGE_NAME}"
log "use it as a Falcon image named: ${IMAGE_NAME}"
