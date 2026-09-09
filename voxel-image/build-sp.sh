#!/bin/bash
#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# build-sp.sh <hubris-commit> - build the gimlet-c + sidecar-c-emu v25 SP images
# for the sp-emu `--emu` backend. Parallel to build-cp.sh, but for real Hubris
# firmware instead of the control plane.
#
# v1 (box-local): builds from the configured hubris checkout (HUBRIS_SRC, default
# ~/oxide/hubris), which MUST already carry the `sidecar-c-emu` emulator variant.
# For a portable build, point HUBRIS_SRC at a branch with that variant committed.
#
# The images must speak the same gateway-messages wire protocol as the omicron MGS
# they'll face (the v23-vs-v25 skew is the #1 gotcha) - this reports hubris's pinned
# rev so you can confirm the match.
#
# RUN ON THE HELIOS BOX.
#
set -euo pipefail

COMMIT="${1:?usage: build-sp.sh <hubris-commit>}"
HUBRIS_SRC="${HUBRIS_SRC:-${HOME}/oxide/hubris}"
export PATH="${HOME}/.cargo/bin:/opt/ooce/bin:${PATH}"
log() { echo "[build-sp] $*"; }

[[ "$(uname -s)" == "SunOS" ]] || { log "FATAL: run on the Helios box"; exit 1; }
[[ -d "${HUBRIS_SRC}/.git" ]] || { log "FATAL: no hubris checkout at ${HUBRIS_SRC} (set HUBRIS_SRC)"; exit 1; }
cd "${HUBRIS_SRC}"
[[ -f app/sidecar/rev-c-emu.toml ]] || {
    log "FATAL: ${HUBRIS_SRC} lacks app/sidecar/rev-c-emu.toml (the emulator variant)."
    log "       Apply/commit the sp-emu hubris changes there first (v1 is box-local)."
    exit 1
}

log "checking out ${COMMIT}"
git checkout -q "${COMMIT}" 2>/dev/null || log "WARN: couldn't checkout ${COMMIT}; building HEAD ($(git rev-parse --short=8 HEAD))"
rustup target add thumbv7em-none-eabihf >/dev/null 2>&1 || true

log "cargo xtask dist app/gimlet/rev-c.toml"
cargo xtask dist app/gimlet/rev-c.toml
log "cargo xtask dist app/sidecar/rev-c-emu.toml"
cargo xtask dist app/sidecar/rev-c-emu.toml

GIMLET="${HUBRIS_SRC}/target/gimlet-c/dist/default/build-gimlet-c-image-default.zip"
SIDECAR="${HUBRIS_SRC}/target/sidecar-c-emu/dist/default/build-sidecar-c-emu-image-default.zip"
[[ -f "${GIMLET}" && -f "${SIDECAR}" ]] || { log "FATAL: expected images were not produced"; exit 1; }

GM_REV="$(grep -oE 'management-gateway-service[^#]*rev=[0-9a-f]+' Cargo.lock 2>/dev/null | grep -oE '[0-9a-f]{8,}' | head -1 || true)"
log "hubris gateway-messages rev: ${GM_REV:-<unknown>} - MUST be wire-compatible with omicron's MGS"

log "done. set these in voxel.toml [sp]:"
log "  voxel config set sp.gimlet_image ${GIMLET}"
log "  voxel config set sp.sidecar_image ${SIDECAR}"
log "  voxel config set sp.emu_bin <path-to-sp-emu binary>"
