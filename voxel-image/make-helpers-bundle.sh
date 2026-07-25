#!/bin/bash
#
# make-helpers-bundle.sh <voxel-root> <out-dir> - assemble the voxel-helpers bundle.
#
# The bundle is the source the builder guest downloads (checksummed) and compiles
# rss-gen + voxel-init from, so the host ships no repo. Dev: served by a local
# HTTP server (build-cp-vm.sh); prod: published to buildomat/TUF by CI.
#
# Layout MIRRORS the repo (a TESTBED-like root), so the in-guest scripts resolve
# their TESTBED-relative paths unchanged:
#   voxel-helpers/
#     voxel-image/  build-cp-guest.sh install-cp.sh gen-manifest.sh
#                   build-rss-gen.sh  patches/
#     voxel/rss-gen/    voxel-config/    voxel-init/
#
# Prints the tarball path + sha256 (also written to <tar>.sha256).
#
set -euo pipefail

ROOT="${1:?usage: make-helpers-bundle.sh <voxel-root> <out-dir>}"
OUT="${2:?usage: make-helpers-bundle.sh <voxel-root> <out-dir>}"
VER="${BUNDLE_VERSION:-dev}"

log() { echo "[make-bundle] $*" >&2; }

# Portable sha256 (illumos `digest`, else openssl / sha256sum).
sha256_of() {
    if command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        digest -a sha256 "$1"
    fi
}

STAGE_PARENT="$(mktemp -d)"
STAGE="${STAGE_PARENT}/voxel-helpers"
mkdir -p "${STAGE}/voxel-image" "${STAGE}/voxel"

# Scripts + patches the guest runs (repo-relative under voxel-image/).
cp "${ROOT}/voxel-image/build-cp-guest.sh" \
   "${ROOT}/voxel-image/install-cp.sh" \
   "${ROOT}/voxel-image/gen-manifest.sh" \
   "${ROOT}/voxel-image/build-rss-gen.sh" "${STAGE}/voxel-image/"
cp -r "${ROOT}/voxel-image/patches" "${STAGE}/voxel-image/patches"

# Crate source (no build output) - the guest compiles these. Mirror repo paths so
# build-rss-gen.sh's TESTBED-relative deps (voxel/rss-gen, voxel-config) resolve.
rsync -a --exclude target "${ROOT}/voxel-config" "${STAGE}/"
rsync -a --exclude target "${ROOT}/voxel-init" "${STAGE}/"
rsync -a --exclude target "${ROOT}/voxel/rss-gen" "${STAGE}/voxel/"

mkdir -p "${OUT}"
TAR="${OUT}/voxel-helpers-${VER}.tar.gz"
# SysV tar (illumos) has no -C/-z: create from inside the stage dir, gzip via pipe.
( cd "${STAGE_PARENT}" && tar cf - voxel-helpers ) | gzip -c > "${TAR}"
SHA="$(sha256_of "${TAR}")"
printf '%s\n' "${SHA}" > "${TAR}.sha256"
rm -rf "${STAGE_PARENT}"

log "bundle: ${TAR}"
log "sha256: ${SHA}"
# Machine-readable on stdout: "<tarpath> <sha256>".
printf '%s %s\n' "${TAR}" "${SHA}"
