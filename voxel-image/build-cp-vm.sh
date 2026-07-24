#!/bin/bash
#
# build-cp-vm.sh <omicron-commit> - build a voxel-cp image INSIDE a builder VM.
#
# The default `voxel image create` path. The heavy omicron build (git clone,
# cargo, omicron-package) runs in a voxel-builder guest (see build-cp-guest.sh),
# so the HOST needs no git / rust / omicron toolchain - only falcon/zfs/bhyve.
# This script stages the small host-only inputs into the cargo-bay, launches the
# builder (which builds + bakes + captures via build-image.sh), and mirrors the
# schema manifest to the host stub.
#
# Requires the voxel-builder base image (bake it once: `voxel image builder-create`).
#
set -euo pipefail

COMMIT="${1:?usage: build-cp-vm.sh <omicron-commit-or-tag>}"
HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
TESTBED="$(cd "${HERE}/.." >/dev/null 2>&1 && pwd -P)"
VOXEL="${VOXEL:-${TESTBED}/target/debug/voxel}"

FALCON_DATASET="${FALCON_DATASET:-rpool/falcon}"
BUILD_ROOT="${BUILD_ROOT:-${HOME}/voxel-builds}"
GIMLETS="${GIMLETS:-4}"
PERSIST_SOURCE="${PERSIST_SOURCE:-0}"
IMAGE_VERSION="${IMAGE_VERSION:-${COMMIT}}"
IMAGE_NAME="${IMAGE_NAME:-voxel-cp-${IMAGE_VERSION}}"
BUILDER_IMAGE="${BUILDER_IMAGE:-voxel-builder}"
CARGO_BAY="${HERE}/cargo-bay/vbuild"
STUB="${BUILD_ROOT}/omicron-${COMMIT}"

export PATH="${HOME}/.cargo/bin:/opt/ooce/bin:${PATH}"
export RUSTFLAGS="${RUSTFLAGS:---cfg svcadm_autoclear -C link-arg=-R/usr/platform/oxide/lib/amd64 -C link-arg=-Wl,-znocompstrtab --cfg tokio_unstable}"

log() { echo "[build-cp-vm] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: run on the Helios box"; exit 1; }
[[ -x "${VOXEL}" ]] || { log "FATAL: voxel binary not found at ${VOXEL} (set VOXEL=)"; exit 1; }

# The builder base image must exist (bake once with `voxel image builder-create`).
if ! zfs list -t snapshot -H -o name "${FALCON_DATASET}/img/${BUILDER_IMAGE}@base" >/dev/null 2>&1; then
    log "FATAL: base image ${BUILDER_IMAGE} not found under ${FALCON_DATASET}/img."
    log "        Bake it once first:  voxel image builder-create"
    exit 1
fi

# --- stage the cargo-bay ------------------------------------------------------
log "staging cargo-bay -> ${CARGO_BAY}"
rm -rf "${CARGO_BAY}"; mkdir -p "${CARGO_BAY}"

# build.env: the per-build inputs the guest can't infer (no host env crosses the
# VM boundary; the guest sources this).
cat > "${CARGO_BAY}/build.env" <<EOF
COMMIT="${COMMIT}"
GIMLETS="${GIMLETS}"
VERSION="${IMAGE_VERSION}"
PERSIST_SOURCE="${PERSIST_SOURCE}"
EOF

# guest scripts + patches + manifest generator.
cp "${HERE}/build-cp-guest.sh" "${HERE}/install-cp.sh" "${HERE}/gen-manifest.sh" "${CARGO_BAY}/"
mkdir -p "${CARGO_BAY}/patches"
cp "${HERE}/patches/nexus-infra-lot-v6.py" "${HERE}/patches/smbios-gimlet.sh" "${CARGO_BAY}/patches/"

# build-time smf configs (rendered from voxel-config on the host; the guest drops
# them into its checkout). `render-smf --out` ignores the positional root.
log "rendering build-time smf configs (gimlets=${GIMLETS})"
"${VOXEL}" image render-smf . --out "${CARGO_BAY}/smf-staging" --gimlets "${GIMLETS}"

# Full voxel workspace for the in-guest rss-gen build. build-rss-gen.sh copies
# voxel/rss-gen out and points it at voxel-config; voxel-config uses
# workspace-inherited deps (serde.workspace = true), so it needs the workspace
# root Cargo.toml + vendor/ (the [patch] path) present above it. Source only;
# build/output dirs excluded.
log "staging voxel workspace for rss-gen"
rsync -a \
    --exclude target --exclude .git --exclude cargo-bay --exclude .falcon --exclude out \
    "${TESTBED}/" "${CARGO_BAY}/voxel-src/"

# SoftNPU sidecar (buildomat; only the host can reach it).
log "fetching SoftNPU sidecar"
bash "${HERE}/fetch-sidecar.sh" "${CARGO_BAY}/sidecar"

# In-guest bring-up agent (voxel's own crate, not the omicron toolchain). Build
# FRESH when cargo is present so voxel-init edits always take effect - a stale
# prebuilt silently ships an old agent; fall back to a prebuilt only where cargo
# isn't available (voxel ships one).
if command -v cargo >/dev/null 2>&1; then
    log "building voxel-init (native illumos)"
    ( cd "${TESTBED}" && cargo build -p voxel-init --release )
    cp "${TESTBED}/target/release/voxel-init" "${CARGO_BAY}/voxel-init"
elif [[ -x "${TESTBED}/target/release/voxel-init" ]]; then
    log "using prebuilt voxel-init (no cargo on PATH)"
    cp "${TESTBED}/target/release/voxel-init" "${CARGO_BAY}/voxel-init"
else
    log "FATAL: no cargo and no prebuilt target/release/voxel-init"; exit 1
fi
chmod +x "${CARGO_BAY}/voxel-init"

# NB: the sp-emu fleet is NOT baked here. Flashing hubris images is a runtime
# concern: `voxel launch --emu-sp` stages + flashes the fleet per-scrimlet from
# the [sp] config at bring-up (topo.rs stage_sp_emu / voxel-init setup_sp_emu).
# This keeps the control-plane image decoupled from SP firmware.

# --- build + capture in the builder VM ----------------------------------------
mkdir -p "${STUB}"
log "launching builder ${BUILDER_IMAGE} to build + bake ${IMAGE_NAME}"
VERSION="${IMAGE_VERSION}" IMAGE_NAME="${IMAGE_NAME}" \
    FALCON_DATASET="${FALCON_DATASET}" CAPTURE_MODE=zfs \
    BASE_IMAGE="${BUILDER_IMAGE}" INSTALL_SCRIPT="build-cp-guest.sh" \
    CARGO_BAY="${CARGO_BAY}" MANIFEST_OUT="${STUB}/voxel-image.toml" \
    KEEP_BUILDER="${PERSIST_SOURCE}" READY_MATCH="voxel-cp version=" \
    bash "${HERE}/build-image.sh"

log "done: ${IMAGE_NAME}"
log "use it: voxel config set image.cp ${IMAGE_NAME} && voxel launch"
if [[ "${PERSIST_SOURCE}" == "1" ]]; then
    log "builder VM left running (PERSIST_SOURCE=1) for source edits"
fi
