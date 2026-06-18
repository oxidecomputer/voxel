#!/bin/bash
#
# build-cp.sh <omicron-commit> - build a voxel-cp image for an omicron commit.
#
# Automates the per-commit control-plane build (de-a4x2 #6a). TUF can't be used:
# its control-plane.tar.gz carries only the service zones, not the i86pc global-
# zone software (sled-agent/switch/opte/mgs), so the GZ half must be an i86pc
# omicron build (see docs/voxel-roadmap.md). This is what `voxel image create`
# drives.
#
# Steps:
#   1. clone + checkout omicron at <commit>            (idempotent)
#   2. install_builder_prerequisites + softnpu machinery (npuzone)
#   3. cargo build omicron-package + xtask(+downloader)
#   4. render build-time smf configs from voxel-config  (mgs-sim/sp-sim/sled) -
#      replaces the static configs a4x2 copied in at build time
#   5. omicron-package target create + package          (the long one)
#   6. fetch the SoftNPU sidecar (forward_v6 rev)
#   7. stage the curated omicron dir into the builder cargo-bay
#   8. bake voxel-cp-<version> via build-image.sh
#   9. build the commit-pinned voxel-rss-gen            (BUILD_RSS_GEN=0 to skip)
#
# RUN ON THE HELIOS BOX. Long (~30-45 min). Env overrides have sane defaults.
#
set -euo pipefail

COMMIT="${1:?usage: build-cp.sh <omicron-commit-or-tag>}"
HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
VOXEL="${VOXEL:-${HERE}/../target/debug/voxel}"

OMICRON_REPO="${OMICRON_REPO:-https://github.com/oxidecomputer/omicron}"
BUILD_ROOT="${BUILD_ROOT:-/root/voxel-builds}"
OMICRON_SRC="${OMICRON_SRC:-${BUILD_ROOT}/omicron-${COMMIT}}"
FALCON_DATASET="${FALCON_DATASET:-rpool/falcon}"
CAPTURE_MODE="${CAPTURE_MODE:-zfs}"
IMAGE_VERSION="${IMAGE_VERSION:-${COMMIT}}"
IMAGE_NAME="${IMAGE_NAME:-voxel-cp-${IMAGE_VERSION}}"
GIMLETS="${GIMLETS:-4}"
BUILD_RSS_GEN="${BUILD_RSS_GEN:-1}"
CARGO_BAY="${HERE}/cargo-bay/vbuild"

export PATH="${HOME}/.cargo/bin:/opt/ooce/bin:${PATH}"
# install_builder_prerequisites.sh ci-downloads cockroach/clickhouse/dpd into
# out/ and then `exit 1`s unless they're on PATH (a check that passes in an
# interactive dev shell but not a fresh non-interactive one). Add them up front;
# `which` resolves at the script's final check, after the download populates them.
export PATH="${OMICRON_SRC}/out/cockroachdb/bin:${OMICRON_SRC}/out/clickhouse:${OMICRON_SRC}/out/dendrite-stub/bin:${PATH}"
# pg_config (libpq) comes from /opt/ooce/bin above; these flags match the
# validated recipe for building omicron on Helios.
export RUSTFLAGS="${RUSTFLAGS:---cfg svcadm_autoclear -C link-arg=-R/usr/platform/oxide/lib/amd64 -C link-arg=-Wl,-znocompstrtab --cfg tokio_unstable}"

log() { echo "[build-cp] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: run on the Helios box"; exit 1; }
[[ -x "${VOXEL}" ]] || { log "FATAL: voxel binary not found at ${VOXEL} (set VOXEL=)"; exit 1; }

# --- 1. clone + checkout ------------------------------------------------------
if [[ ! -d "${OMICRON_SRC}/.git" ]]; then
    mkdir -p "${BUILD_ROOT}"
    log "cloning omicron -> ${OMICRON_SRC}"
    git clone "${OMICRON_REPO}" "${OMICRON_SRC}"
fi
log "checking out ${COMMIT}"
git -C "${OMICRON_SRC}" fetch --all --tags -q || true
git -C "${OMICRON_SRC}" checkout -q "${COMMIT}"
cd "${OMICRON_SRC}"

# --- 2. prerequisites + softnpu machinery -------------------------------------
log "install_builder_prerequisites.sh -y"
./tools/install_builder_prerequisites.sh -y
log "ci_download_softnpu_machinery (out/npuzone)"
./tools/ci_download_softnpu_machinery

# --- 3. build the package tools -----------------------------------------------
log "cargo build --release omicron-package xtask xtask-downloader"
cargo build --release -p omicron-package -p xtask -p xtask-downloader

# --- 4. render build-time smf configs (de-a4x2) -------------------------------
log "rendering build-time smf configs from voxel-config (gimlets=${GIMLETS})"
"${VOXEL}" image render-smf "${OMICRON_SRC}" --gimlets "${GIMLETS}"

# --- 5. package the control plane ---------------------------------------------
# NB: "-p a4x2" is OMICRON's own package preset (a build target in omicron's
# package-manifest), NOT the (removed) a4x2 testbed crate. Leave it as-is.
log "omicron-package target create -p a4x2 (omicron's package preset)"
./target/release/omicron-package -t default target create -p a4x2
log "omicron-package package (~11 min)"
./target/release/omicron-package package

# --- 6. fetch the SoftNPU sidecar ---------------------------------------------
log "fetching SoftNPU sidecar"
bash "${HERE}/fetch-sidecar.sh" "${CARGO_BAY}/sidecar"

# --- 7. stage curated omicron dir into the builder cargo-bay ------------------
STAGE="${CARGO_BAY}/omicron"
log "staging omicron build -> ${STAGE}"
rm -rf "${STAGE}"
mkdir -p "${STAGE}"
rsync -a tools out smf package-manifest.toml \
    target/release/omicron-package target/release/xtask target/release/xtask-downloader \
    --exclude out/downloads --exclude out/clickhouse --exclude out/cockroachdb \
    --exclude out/dendrite-stub --exclude out/mgd --exclude out/transceiver-control \
    --exclude out/console-assets "${STAGE}/"

# --- 7b. build + stage the in-guest agent (voxel-init) ------------------------
# Native illumos build (this box is the gimlet's OS); install-cp.sh bakes it to
# /opt/oxide/voxel-init, which `voxel launch` runs in place of gimlet-launch.sh.
log "building voxel-init (native illumos) for the gimlet image"
( cd "${HERE}/.." && cargo build -p voxel-init --release )
cp "${HERE}/../target/release/voxel-init" "${CARGO_BAY}/voxel-init"
chmod +x "${CARGO_BAY}/voxel-init"

# --- 8. bake the image --------------------------------------------------------
log "baking ${IMAGE_NAME} via build-image.sh (CAPTURE_MODE=${CAPTURE_MODE})"
VERSION="${IMAGE_VERSION}" IMAGE_NAME="${IMAGE_NAME}" \
    FALCON_DATASET="${FALCON_DATASET}" CAPTURE_MODE="${CAPTURE_MODE}" \
    CARGO_BAY="${CARGO_BAY}" \
    bash "${HERE}/build-image.sh"

# --- 9. commit-pinned voxel-rss-gen -------------------------------------------
if [[ "${BUILD_RSS_GEN}" == "1" ]]; then
    log "building commit-pinned voxel-rss-gen"
    if bash "${HERE}/build-rss-gen.sh" "${OMICRON_SRC}"; then
        log "rss-gen ready: ${OMICRON_SRC}/target/debug/voxel-rss-gen"
        log "  launch with: VOXEL_RSS_GEN=${OMICRON_SRC}/target/debug/voxel-rss-gen"
    else
        log "WARN: voxel-rss-gen didn't build (see the boxed note above for the"
        log "      fix). The IMAGE (${IMAGE_NAME}) is built and usable; only the"
        log "      typed RSS renderer needs the schema update before launch."
    fi
fi

log "done: ${IMAGE_NAME}"
log "use it: voxel config set image.cp ${IMAGE_NAME} && voxel launch"
