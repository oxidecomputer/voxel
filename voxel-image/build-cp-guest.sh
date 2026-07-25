#!/bin/bash
#
# build-cp-guest.sh - the omicron control-plane build, IN THE BUILDER VM.
#
# Runs inside the voxel-builder guest (launched by voxel-image-builder). It does
# what build-cp.sh's host half used to do - clone + build + package omicron - but
# on the guest's own disk, so the HOST needs no git/rust/omicron toolchain. It
# then reuses install-cp.sh (the single baking authority) to unpack + bake the
# image, bakes the commit-pinned voxel-rss-gen + schema manifest, and (unless
# PERSIST_SOURCE) scrubs the source + cargo target before capture.
#
# Runs from the downloaded voxel-helpers bundle ($VOXEL_HELPERS), exec'd by
# bootstrap.sh. Inputs:
#   $VOXEL_HELPERS/voxel-image/   install-cp.sh, gen-manifest.sh, build-rss-gen.sh, patches/
#   $VOXEL_HELPERS/{voxel/rss-gen,voxel-config,voxel-init}   crate source (built here)
#   /opt/cargo-bay/               build.env + host-only inputs: smf-staging/, sidecar/
#
set -euo pipefail

CARGO_BAY=/opt/cargo-bay
VOXEL_HELPERS="${VOXEL_HELPERS:-/opt/voxel-helpers}"
source "${CARGO_BAY}/build.env"

COMMIT="${COMMIT:?build.env must set COMMIT}"
GIMLETS="${GIMLETS:-4}"
PERSIST_SOURCE="${PERSIST_SOURCE:-0}"
OMICRON_SRC="${OMICRON_SRC:-/omicron}"

export PATH="${HOME}/.cargo/bin:/opt/ooce/bin:${PATH}"
export PATH="${OMICRON_SRC}/out/cockroachdb/bin:${OMICRON_SRC}/out/clickhouse:${OMICRON_SRC}/out/dendrite-stub/bin:${PATH}"
export RUSTFLAGS="${RUSTFLAGS:---cfg svcadm_autoclear -C link-arg=-R/usr/platform/oxide/lib/amd64 -C link-arg=-Wl,-znocompstrtab --cfg tokio_unstable}"

log() { echo "[build-cp-guest] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: must run in a helios guest"; exit 1; }

# Drop the base image's provision marker so a mid-build failure can't leave a
# stale marker that reads as success (build-image.sh matches READY_MATCH too).
rm -f /var/voxel-image-ready

# --- networking: reach github/crates.io/pkg.oxide.computer ---------------------
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

# --- 1. clone + checkout -------------------------------------------------------
OMICRON_REPO="${OMICRON_REPO:-https://github.com/oxidecomputer/omicron}"
if [[ ! -d "${OMICRON_SRC}/.git" ]]; then
    log "cloning omicron -> ${OMICRON_SRC}"
    git clone "${OMICRON_REPO}" "${OMICRON_SRC}"
fi
log "checking out ${COMMIT}"
git -C "${OMICRON_SRC}" fetch --all --tags -q || true
# -f: the warm base checkout may carry local tracked-file changes (from the
# provision warm build / prerequisites); force past them. out/ + target/ are
# gitignored, so the warm cache survives.
git -C "${OMICRON_SRC}" checkout -f -q "${COMMIT}"
cd "${OMICRON_SRC}"

# --- 2. voxel source patches (re-applied post-checkout) ------------------------
log "patching sled-hardware baseboard (Pc -> Gimlet)"
bash "${VOXEL_HELPERS}/voxel-image/patches/smbios-gimlet.sh" "${OMICRON_SRC}"
log "patching nexus rack-init infra address lot (v6 block)"
python3 "${VOXEL_HELPERS}/voxel-image/patches/nexus-infra-lot-v6.py" "${OMICRON_SRC}"
grep -q 'voxel: add a v6 block' nexus/src/app/rack.rs \
    || { log "FATAL: nexus infra-lot v6 patch did not apply"; exit 1; }

# --- 3. prerequisites + softnpu machinery --------------------------------------
# On a fresh boot a system pkg client may still hold the image lock ("in use by
# another package client"); retry until it frees. install_builder_prerequisites
# is idempotent, so retrying is safe.
log "install_builder_prerequisites.sh -y"
n=0
until ./tools/install_builder_prerequisites.sh -y; do
    n=$((n + 1))
    if [[ $n -ge 20 ]]; then log "FATAL: install_builder_prerequisites failed after ${n} attempts"; exit 1; fi
    log "prerequisites attempt ${n} failed (pkg busy?); retry in 15s"
    sleep 15
done
log "ci_download_softnpu_machinery (out/npuzone)"
./tools/ci_download_softnpu_machinery

# --- 4. build the package tools ------------------------------------------------
log "cargo build --release omicron-package xtask xtask-downloader"
cargo build --release -p omicron-package -p xtask -p xtask-downloader

# --- 5. drop in the rendered build-time smf configs ----------------------------
log "installing staged build-time smf configs"
cp -r "${CARGO_BAY}/smf-staging/smf/." "${OMICRON_SRC}/smf/"

# --- 6. package the control plane ----------------------------------------------
log "omicron-package target create -p a4x2"
./target/release/omicron-package -t default target create -p a4x2
log "omicron-package package (~11 min)"
./target/release/omicron-package package

# --- 7. build the commit-pinned voxel-rss-gen (from the bundle) ----------------
log "building commit-pinned voxel-rss-gen"
bash "${VOXEL_HELPERS}/voxel-image/build-rss-gen.sh" "${OMICRON_SRC}"
cp "${OMICRON_SRC}/target/debug/voxel-rss-gen" "${CARGO_BAY}/voxel-rss-gen"

# --- 8. schema manifest --------------------------------------------------------
log "generating voxel-image.toml manifest"
COMMIT="${COMMIT}" bash "${VOXEL_HELPERS}/voxel-image/gen-manifest.sh" "${OMICRON_SRC}" > "${CARGO_BAY}/voxel-image.toml"

# --- 8b. build voxel-init from the bundle (baked by install-cp.sh) -------------
# Light crate (no libfalcon); native illumos build. Target outside the bundle so
# the bundle stays source-only. install-cp.sh bakes /opt/cargo-bay/voxel-init.
log "building voxel-init from bundle"
( cd "${VOXEL_HELPERS}/voxel-init" && CARGO_TARGET_DIR=/tmp/vinit-target cargo build --release )
cp /tmp/vinit-target/release/voxel-init "${CARGO_BAY}/voxel-init"
rm -rf /tmp/vinit-target

# --- 9. stage the curated omicron dir for install-cp.sh ------------------------
# install-cp.sh cd's into /opt/cargo-bay/omicron and bakes it (omicron CLI dir +
# unpack). Stage the same curated subset build-cp.sh's host half produced, so the
# baked image carries no source / cargo target.
STAGE="${CARGO_BAY}/omicron"
log "staging curated omicron -> ${STAGE}"
rm -rf "${STAGE}"; mkdir -p "${STAGE}"
rsync -a tools out smf package-manifest.toml \
    target/release/omicron-package target/release/xtask target/release/xtask-downloader \
    --exclude out/downloads --exclude out/clickhouse --exclude out/cockroachdb \
    --exclude out/dendrite-stub --exclude out/mgd --exclude out/transceiver-control \
    --exclude out/console-assets "${STAGE}/"

# --- 10. bake via install-cp.sh ------------------------------------------------
# install-cp.sh cd's into the relative `omicron` dir, so run it from the cargo-bay
# (where the curated omicron was staged above), matching the host-build flow.
log "running install-cp.sh (unpack + bake)"
( cd "${CARGO_BAY}" && VOXEL_CP_VERSION="${VERSION:-${COMMIT}}" bash "${VOXEL_HELPERS}/voxel-image/install-cp.sh" )

# Everything above is baked into /opt/oxide now; drop the large build-time
# cargo-bay artifacts so they aren't captured into the image (the staged rss-gen
# alone is ~350MB). Launch re-stages a fresh per-node cargo-bay, so none of this
# is needed at boot. Leave the small scripts (incl. this running one) in place.
log "clearing build cargo-bay artifacts (baked into /opt/oxide already)"
rm -rf "${CARGO_BAY}/omicron" "${CARGO_BAY}/voxel-src" "${CARGO_BAY}/sidecar" \
       "${CARGO_BAY}/sp-emu" "${CARGO_BAY}/smf-staging" "${CARGO_BAY}/voxel-rss-gen" \
       "${CARGO_BAY}/voxel-init" 2>/dev/null || true

# --- 11. scrub source + toolchain (unless persisting) --------------------------
if [[ "${PERSIST_SOURCE}" == "1" ]]; then
    log "PERSIST_SOURCE=1: keeping omicron source + cargo target in the image"
else
    log "scrubbing omicron source + cargo target from the image"
    rm -rf "${OMICRON_SRC}" /rss-gen-* "${HOME}/.cargo/registry" "${HOME}/.cargo/git" 2>/dev/null || true
fi

log "done"
