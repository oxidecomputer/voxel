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
BUILD_ROOT="${BUILD_ROOT:-${HOME}/voxel-builds}"
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
# SRC_ASIS=1 (`voxel image create --src`): build OMICRON_SRC in place, do NOT
# clone or checkout - the dev's working-tree edits are what we build.
if [[ "${SRC_ASIS:-0}" == "1" ]]; then
    [[ -d "${OMICRON_SRC}" ]] || { log "FATAL: --src ${OMICRON_SRC} not found"; exit 1; }
    log "building ${OMICRON_SRC} as-is (--src; no clone/checkout)"
else
    if [[ ! -d "${OMICRON_SRC}/.git" ]]; then
        mkdir -p "${BUILD_ROOT}"
        log "cloning omicron -> ${OMICRON_SRC}"
        git clone "${OMICRON_REPO}" "${OMICRON_SRC}"
    fi
    log "checking out ${COMMIT}"
    git -C "${OMICRON_SRC}" fetch --all --tags -q || true
    git -C "${OMICRON_SRC}" checkout -q "${COMMIT}"
fi
cd "${OMICRON_SRC}"

# --- 1b. voxel patch: report a Gimlet baseboard (de-a4x2 wicket fix) -----------
# sled-hardware's parse_smbios_output returns a *Pc* baseboard for the a4x2/voxel
# i86pc sleds, but wicketd's RACK SETUP correlates each sled's bootstrap address by
# matching the SP's *Gimlet* baseboard (serial/model/revision from MGS). A Pc can
# never equal a Gimlet, so every sled shows "bootstrap address UNKNOWN". Patch it
# to return a Gimlet (revision 2, matching the emulated SP VPD `0XV2:...:002:`) so
# the two baseboards correlate. `populate_smbios` (voxel topo.rs) bakes manufacturer
# `a4x2` + serial `BRM4422000{i}` so this path is taken and the strings match the
# SP. The checkout above resets the tree each build, so this re-applies every time.
log "patching sled-hardware parse_smbios_output: Pc -> Gimlet baseboard"
perl -pi -e 's/Some\(Baseboard::new_pc\(serial_number, product\)\)/Some(Baseboard::new_gimlet(serial_number, product, 2))/' \
    sled-hardware/src/illumos/mod.rs
grep -q 'new_gimlet(serial_number, product, 2)' sled-hardware/src/illumos/mod.rs \
    || { log "FATAL: smbios baseboard patch did not apply"; exit 1; }

# --- 1c. voxel patch: v6 block in the infra address lot ------------------------
# Nexus rack-init lot-validates every switch-port address against the single-block
# infra lot. In Static mode that lot is v4 (numbered uplinks), so voxel's v6
# addrconf sidecar-interconnect ports can't reserve (handoff 400 "address not in
# lot"). Add a v6 block so they reserve, matching BGP mode's :: lot. Re-applied
# post-checkout (the checkout resets the tree).
# Guard so re-running on a persistent --src tree doesn't insert the block twice
# (the smbios perl above is already idempotent; this python insert is not).
if grep -q 'voxel: add a v6 block' nexus/src/app/rack.rs; then
    log "nexus infra-lot v6 patch already present"
else
    log "patching nexus rack-init: add v6 block to the infra address lot"
    python3 "${HERE}/patches/nexus-infra-lot-v6.py" "${OMICRON_SRC}"
    grep -q 'voxel: add a v6 block' nexus/src/app/rack.rs \
        || { log "FATAL: nexus infra-lot v6 patch did not apply"; exit 1; }
fi

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

# NB: the sp-emu fleet is NOT baked here. Flashing hubris images is a runtime
# concern: `voxel launch --emu-sp` stages + flashes the fleet per-scrimlet from
# the [sp] config at bring-up (topo.rs stage_sp_emu / voxel-init setup_sp_emu),
# keeping the control-plane image decoupled from SP firmware.

# --- 7d. commit-pinned voxel-rss-gen + schema manifest ------------------------
# Both are baked INTO the image (launch runs rss-gen in-guest, reads the manifest
# from the host stub), so build + stage them into the cargo-bay before the bake.
# The host stub mirror lets `voxel launch` pick up the schema without any source
# on the box.
log "building commit-pinned voxel-rss-gen"
bash "${HERE}/build-rss-gen.sh" "${OMICRON_SRC}"
cp "${OMICRON_SRC}/target/debug/voxel-rss-gen" "${CARGO_BAY}/voxel-rss-gen"
log "generating voxel-image.toml manifest (baked + host stub)"
# Host stub goes to <build_root>/omicron-<IMAGE_VERSION>/, where topo.rs
# detect_sled_schema reads it at launch (keyed on the image label). For a normal
# build that's the same dir as OMICRON_SRC; for --src it's a separate stub dir.
STUB="${BUILD_ROOT}/omicron-${IMAGE_VERSION}"
mkdir -p "${STUB}"
COMMIT="${IMAGE_VERSION}" bash "${HERE}/gen-manifest.sh" "${OMICRON_SRC}" \
    | tee "${CARGO_BAY}/voxel-image.toml" > "${STUB}/voxel-image.toml"

# --- 8. bake the image --------------------------------------------------------
log "baking ${IMAGE_NAME} via build-image.sh (CAPTURE_MODE=${CAPTURE_MODE})"
VERSION="${IMAGE_VERSION}" IMAGE_NAME="${IMAGE_NAME}" \
    FALCON_DATASET="${FALCON_DATASET}" CAPTURE_MODE="${CAPTURE_MODE}" \
    CARGO_BAY="${CARGO_BAY}" \
    bash "${HERE}/build-image.sh"

log "done: ${IMAGE_NAME}"
log "use it: voxel config set image.cp ${IMAGE_NAME} && voxel launch"
