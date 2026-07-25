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

# build-time smf configs (host-rendered from voxel-config; the guest drops them
# into its checkout). `render-smf --out` ignores the positional root.
log "rendering build-time smf configs (gimlets=${GIMLETS})"
"${VOXEL}" image render-smf . --out "${CARGO_BAY}/smf-staging" --gimlets "${GIMLETS}"

# SoftNPU sidecar (buildomat; only the host can reach it).
log "fetching SoftNPU sidecar"
bash "${HERE}/fetch-sidecar.sh" "${CARGO_BAY}/sidecar"

# NB: the sp-emu fleet is NOT baked (runtime-only; see topo.rs stage_sp_emu). NB:
# the guest scripts, patches, and the voxel-config/voxel-init/rss-gen crate SOURCE
# are NOT host-staged - the guest downloads them in the voxel-helpers bundle below
# and builds rss-gen + voxel-init from it. The host ships no repo to the guest.

# --- serve the voxel-helpers bundle (dev artifact origin) ---------------------
# Assemble the bundle from this checkout + serve it over HTTP on a guest-reachable
# box IP. The guest (bootstrap.sh) downloads + checksum-verifies it. This is the
# local stand-in for buildomat/TUF: same download code path, local origin.
SERVE_DIR="${HERE}/cargo-bay/helpers-serve"
rm -rf "${SERVE_DIR}"; mkdir -p "${SERVE_DIR}"
log "assembling voxel-helpers bundle"
read -r BUNDLE_TAR BUNDLE_SHA < <(bash "${HERE}/make-helpers-bundle.sh" "${TESTBED}" "${SERVE_DIR}")
BUNDLE_NAME="$(basename "${BUNDLE_TAR}")"
HOST_IP="${HELPERS_HOST_IP:-$(ipadm show-addr -p -o addr 2>/dev/null | sed 's#/.*##' | grep -E '^[0-9]+\.' | grep -v '^127\.' | head -1)}"
PORT="${HELPERS_PORT:-8778}"
# Clear any stale server squatting on the port (e.g. a leaked prior run), else
# the bind fails and the guest 404s off the old one.
pkill -f "http.server ${PORT} " 2>/dev/null || true
sleep 1
# pipenv-managed server when available (house pref); else plain python3 (the
# server is stdlib-only, so there's nothing for pipenv to isolate).
if command -v pipenv >/dev/null 2>&1; then
    ( cd "${HERE}" && pipenv run python3 -m http.server "${PORT}" --bind "${HOST_IP}" --directory "${SERVE_DIR}" ) >/tmp/voxel-helpers-serve.log 2>&1 &
else
    python3 -m http.server "${PORT}" --bind "${HOST_IP}" --directory "${SERVE_DIR}" >/tmp/voxel-helpers-serve.log 2>&1 &
fi
SERVE_PID=$!
trap 'kill ${SERVE_PID} 2>/dev/null || true' EXIT
HELPERS_URL="http://${HOST_IP}:${PORT}/${BUNDLE_NAME}"

# Verify the server actually serves the bundle before the ~10-min build, so a
# bind failure / wrong dir fails fast here instead of via a guest download loop.
sleep 1
CHECK=/tmp/voxel-helpers-check.tar.gz
if ! curl -fsS "${HELPERS_URL}" -o "${CHECK}" 2>/dev/null \
   || [[ "$(sha256_of "${CHECK}")" != "${BUNDLE_SHA}" ]]; then
    log "FATAL: helpers server not serving ${HELPERS_URL} correctly"
    log "        (see /tmp/voxel-helpers-serve.log)"
    rm -f "${CHECK}"
    exit 1
fi
rm -f "${CHECK}"
log "serving voxel-helpers at ${HELPERS_URL} (pid ${SERVE_PID}, sha ${BUNDLE_SHA})"
cat >> "${CARGO_BAY}/build.env" <<EOF
HELPERS_URL="${HELPERS_URL}"
HELPERS_SHA256="${BUNDLE_SHA}"
EOF

# --- build + capture in the builder VM ----------------------------------------
# INSTALL_SCRIPT=bootstrap.sh: the minimal entrypoint that downloads the bundle
# and execs build-cp-guest.sh from it.
mkdir -p "${STUB}"
log "launching builder ${BUILDER_IMAGE} to build + bake ${IMAGE_NAME}"
VERSION="${IMAGE_VERSION}" IMAGE_NAME="${IMAGE_NAME}" \
    FALCON_DATASET="${FALCON_DATASET}" CAPTURE_MODE=zfs \
    BASE_IMAGE="${BUILDER_IMAGE}" INSTALL_SCRIPT="bootstrap.sh" \
    CARGO_BAY="${CARGO_BAY}" MANIFEST_OUT="${STUB}/voxel-image.toml" \
    KEEP_BUILDER="${PERSIST_SOURCE}" READY_MATCH="voxel-cp version=" \
    bash "${HERE}/build-image.sh"

log "done: ${IMAGE_NAME}"
log "use it: voxel config set image.cp ${IMAGE_NAME} && voxel launch"
if [[ "${PERSIST_SOURCE}" == "1" ]]; then
    log "builder VM left running (PERSIST_SOURCE=1) for source edits"
fi
