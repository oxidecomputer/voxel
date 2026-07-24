#!/bin/bash
#
# provision-builder.sh - bake the voxel-builder base image (runs in-guest).
#
# Turns a stock helios guest into a warm omicron builder: git + rustup + omicron's
# builder prerequisites (ci-downloaded cockroach/clickhouse/dpd) + a warmed cargo
# target. `voxel image create` then boots from the captured image and does the
# whole omicron build in the VM, so the HOST needs no toolchain. One-time; slow.
#
# The warm checkout + target are LEFT in place (at /omicron): build-cp-guest.sh
# reuses that path, so per-commit builds hit a warm cache instead of a cold one.
#
set -euo pipefail

READY_MARKER=/var/voxel-image-ready
WARM_SRC="${WARM_SRC:-/omicron}"
OMICRON_REPO="${OMICRON_REPO:-https://github.com/oxidecomputer/omicron}"

log() { echo "[provision-builder] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: must run in a helios guest"; exit 1; }

# --- networking ---------------------------------------------------------------
EXT_IF="${EXT_IF:-$(dladm show-phys -o link -p 2>/dev/null | grep '^vioif' | head -1 || true)}"
EXT_IF="${EXT_IF:-vioif0}"
log "using external interface ${EXT_IF}"
ipadm create-addr -T dhcp "${EXT_IF}/v4" || true
echo 'nameserver 1.1.1.1' > /etc/resolv.conf
for _ in $(seq 1 30); do
    ipadm show-addr "${EXT_IF}/v4" -p -o addr 2>/dev/null | grep -q '/' && break
    sleep 2
done
for _ in $(seq 1 15); do
    getent hosts github.com >/dev/null 2>&1 && break
    sleep 2
done

# --- packages: git + the pinned runtime pkgs ----------------------------------
# pkg exit 4 = "nothing to do" (already installed); treat it as success.
install_packages() {
    pkg install git jq tofino looker htop
    local rc=$?
    [[ $rc -eq 0 || $rc -eq 4 ]]
}
n=0
until install_packages; do
    n=$((n + 1))
    if [[ $n -ge 25 ]]; then log "FATAL: pkg install failed after ${n} attempts"; exit 1; fi
    log "pkg install attempt ${n} failed; retrying"
    sleep 2
done

# --- rustup (omicron pins its toolchain via rust-toolchain.toml) ---------------
export PATH="${HOME}/.cargo/bin:/opt/ooce/bin:${PATH}"
if ! command -v rustup >/dev/null 2>&1; then
    log "installing rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi

# --- warm the omicron build cache ---------------------------------------------
export RUSTFLAGS="--cfg svcadm_autoclear -C link-arg=-R/usr/platform/oxide/lib/amd64 -C link-arg=-Wl,-znocompstrtab --cfg tokio_unstable"
if [[ ! -d "${WARM_SRC}/.git" ]]; then
    log "cloning omicron (warm) -> ${WARM_SRC}"
    git clone "${OMICRON_REPO}" "${WARM_SRC}" || log "WARN: warm clone failed"
fi
if [[ -d "${WARM_SRC}/.git" ]]; then
    export PATH="${WARM_SRC}/out/cockroachdb/bin:${WARM_SRC}/out/clickhouse:${WARM_SRC}/out/dendrite-stub/bin:${PATH}"
    # Retry: a boot-time pkg client may still hold the image lock ("in use by
    # another package client"). Bake the prereqs properly so per-build is fast.
    n=0
    until ( cd "${WARM_SRC}" && ./tools/install_builder_prerequisites.sh -y ); do
        n=$((n + 1))
        if [[ $n -ge 20 ]]; then log "WARN: prerequisites warm failed after ${n} (per-build will retry)"; break; fi
        log "prerequisites warm attempt ${n} failed (pkg busy?); retry in 15s"
        sleep 15
    done
    ( cd "${WARM_SRC}" && cargo build --release -p omicron-package -p xtask -p xtask-downloader ) \
        || log "WARN: warm build failed (per-build will retry)"
fi

sync
printf 'voxel-builder version=1 built=%s\n' "$(date '+%Y-%m-%dT%H:%M:%S')" > "${READY_MARKER}"
log "builder image ready: $(cat "${READY_MARKER}")"
