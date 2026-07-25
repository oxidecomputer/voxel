#!/bin/bash
#
# bootstrap.sh - minimal in-guest entrypoint for the contained build.
#
# The ONLY script the host hands the guest (via the cargo-bay, alongside
# build.env). It brings up networking, downloads + checksum-verifies the
# voxel-helpers bundle from the artifact source (HELPERS_URL: a local dev server
# now, buildomat/TUF later), unpacks it, and execs the real builder from it. No
# voxel repo on the host - the bundle carries the scripts + crate source. (Later
# this script is embedded in the `voxel` binary so nothing but `voxel` is needed.)
#
set -euo pipefail

CARGO_BAY=/opt/cargo-bay
source "${CARGO_BAY}/build.env"

HELPERS_URL="${HELPERS_URL:?build.env must set HELPERS_URL}"
HELPERS_SHA256="${HELPERS_SHA256:?build.env must set HELPERS_SHA256}"
BUNDLE_DIR="${BUNDLE_DIR:-/opt/voxel-helpers}"

log() { echo "[bootstrap] $*"; }

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

# --- networking (reach the artifact server + github/crates.io) ----------------
EXT_IF="${EXT_IF:-$(dladm show-phys -o link -p 2>/dev/null | grep '^vioif' | head -1 || true)}"
EXT_IF="${EXT_IF:-vioif0}"
log "using external interface ${EXT_IF}"
ipadm create-addr -T dhcp "${EXT_IF}/v4" || true
echo 'nameserver 1.1.1.1' > /etc/resolv.conf
for _ in $(seq 1 30); do
    ipadm show-addr "${EXT_IF}/v4" -p -o addr 2>/dev/null | grep -q '/' && break
    sleep 2
done

# --- download + verify --------------------------------------------------------
DL=/tmp/voxel-helpers.tar.gz
log "downloading ${HELPERS_URL}"
n=0
until curl -fsSL "${HELPERS_URL}" -o "${DL}"; do
    n=$((n + 1))
    if [[ $n -ge 20 ]]; then log "FATAL: download failed after ${n} attempts"; exit 1; fi
    log "download attempt ${n} failed; retry in 5s"
    sleep 5
done
GOT="$(sha256_of "${DL}")"
if [[ "${GOT}" != "${HELPERS_SHA256}" ]]; then
    log "FATAL: checksum mismatch (got ${GOT}, want ${HELPERS_SHA256})"
    exit 1
fi
log "checksum ok; unpacking -> ${BUNDLE_DIR}"
# SysV tar (illumos): no -C/-z/--strip-components. The archive's top dir is
# `voxel-helpers`, so extract in the parent and it lands at ${BUNDLE_DIR}.
rm -rf "${BUNDLE_DIR}"
mkdir -p "$(dirname "${BUNDLE_DIR}")"
gzip -dc "${DL}" | ( cd "$(dirname "${BUNDLE_DIR}")" && tar xf - )

# --- hand off to the real builder (from the bundle) ---------------------------
export VOXEL_HELPERS="${BUNDLE_DIR}"
exec bash "${BUNDLE_DIR}/voxel-image/build-cp-guest.sh"
