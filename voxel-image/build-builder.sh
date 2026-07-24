#!/bin/bash
#
# build-builder.sh - bake the reusable voxel-builder base image (`voxel image
# builder-create`). Boots stock helios, runs provision-builder.sh, captures the
# disk as <dataset>/img/voxel-builder@base. One-time; `build-cp-vm.sh` boots from
# it. FORCE=1 rebuilds even if it already exists.
#
set -euo pipefail

HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
FALCON_DATASET="${FALCON_DATASET:-rpool/falcon}"
IMAGE_NAME="${IMAGE_NAME:-voxel-builder}"
FORCE="${FORCE:-0}"

log() { echo "[build-builder] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: run on the Helios box"; exit 1; }

if [[ "${FORCE}" != "1" ]] \
   && zfs list -t snapshot -H -o name "${FALCON_DATASET}/img/${IMAGE_NAME}@base" >/dev/null 2>&1; then
    log "${IMAGE_NAME} already exists (set FORCE=1 to rebuild); nothing to do"
    exit 0
fi

log "baking ${IMAGE_NAME} (boots helios-3.0, provisions toolchain; slow, one-time)"
VERSION=builder IMAGE_NAME="${IMAGE_NAME}" FALCON_DATASET="${FALCON_DATASET}" \
    CAPTURE_MODE=zfs BASE_IMAGE=helios-3.0 INSTALL_SCRIPT=provision-builder.sh \
    CARGO_BAY="${HERE}/cargo-bay/vbuilder" VBUILD_DISK_GB="${VBUILD_DISK_GB:-150}" \
    bash "${HERE}/build-image.sh"

log "done: ${IMAGE_NAME}"
