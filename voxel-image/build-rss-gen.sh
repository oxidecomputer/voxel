#!/bin/bash
#
# build-rss-gen.sh <omicron-src> - build the commit-pinned voxel-rss-gen.
#
# The typed RSS renderer (voxel/rss-gen) depends on omicron's OWN config types,
# so it must be built against the SAME omicron the image was built from. This
# copies the crate out, repoints its path deps (omicron source + voxel-config) at
# this build, and compiles it against omicron's warm target.
#
# Per-commit caveat: a commit that ADDED fields to RackInitializeRequest needs
# those fields added to voxel/rss-gen/src/main.rs - that surfaces here as a
# compile error (a documented manual step; see docs/voxel-roadmap.md).
#
# Output: <omicron-src>/target/debug/voxel-rss-gen  (point VOXEL_RSS_GEN at it).
#
set -euo pipefail

OMICRON_SRC="${1:?usage: build-rss-gen.sh <omicron-src>}"
HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
TESTBED="$(cd "${HERE}/.." >/dev/null 2>&1 && pwd -P)"
RSS_GEN_DIR="${RSS_GEN_DIR:-$(dirname "${OMICRON_SRC}")/rss-gen-$(basename "${OMICRON_SRC}")}"

export PATH="${HOME}/.cargo/bin:/opt/ooce/bin:${PATH}"

echo "[build-rss-gen] staging crate -> ${RSS_GEN_DIR}"
rm -rf "${RSS_GEN_DIR}"
mkdir -p "$(dirname "${RSS_GEN_DIR}")"
cp -r "${TESTBED}/voxel/rss-gen" "${RSS_GEN_DIR}"

# Repoint path deps: /opt/omicron -> this build; ../../voxel-config -> absolute.
sed -i "s#/opt/omicron#${OMICRON_SRC}#g" "${RSS_GEN_DIR}/Cargo.toml"
sed -i "s#path = \"../../voxel-config\"#path = \"${TESTBED}/voxel-config\"#" "${RSS_GEN_DIR}/Cargo.toml"

# Seed the lockfile from the image's omicron so shared deps (notably the
# git-pinned `tufaceous-artifact`, which floats on branch=main) resolve to the
# EXACT revs this omicron was built against. Without this, rss-gen's fresh
# resolution pulls a newer tufaceous than omicron-common@<commit> was written
# for, and omicron-common fails to compile (ArtifactKind/Artifact field errors).
# Plain `cargo build` (not --locked) keeps these pins and adds rss-gen's extras.
cp "${OMICRON_SRC}/Cargo.lock" "${RSS_GEN_DIR}/Cargo.lock"

echo "[build-rss-gen] building against ${OMICRON_SRC}/target"
build_log="$(mktemp)"
set +e
( cd "${RSS_GEN_DIR}" && CARGO_TARGET_DIR="${OMICRON_SRC}/target" cargo build ) 2>&1 | tee "${build_log}"
rc=${PIPESTATUS[0]}
set -e

if [[ "${rc}" -eq 0 ]]; then
    rm -f "${build_log}"
    echo "[build-rss-gen] built: ${OMICRON_SRC}/target/debug/voxel-rss-gen"
    exit 0
fi

# Compile failed. If it's a RackInitializeRequest schema drift, say so the npm
# way: name what's out of date and exactly how to fix it. (Fields ADDED by this
# omicron show as "missing field"; fields it REMOVED/renamed show as "no field".)
added="$(grep -oE "missing field \`[A-Za-z0-9_]+\`" "${build_log}" \
    | sed -E 's/.*`([A-Za-z0-9_]+)`/\1/' | sort -u | paste -sd', ' -)"
removed="$(grep -oE "no field named \`[A-Za-z0-9_]+\`|has no field named \`[A-Za-z0-9_]+\`" "${build_log}" \
    | sed -E 's/.*`([A-Za-z0-9_]+)`/\1/' | sort -u | paste -sd', ' -)"
rm -f "${build_log}"

if [[ -n "${added}" || -n "${removed}" ]]; then
    src="$(cd "${TESTBED}" && pwd)/voxel/rss-gen/src/main.rs"
    echo
    echo "  ┌─────────────────────────────────────────────────────────────────"
    echo "  │ ⚠  voxel-rss-gen is out of date for this omicron version."
    echo "  │"
    echo "  │    omicron's RackInitializeRequest schema has changed:"
    [[ -n "${added}" ]]   && echo "  │      • new field(s) to ADD:     ${added}"
    [[ -n "${removed}" ]] && echo "  │      • field(s) to REMOVE:      ${removed}"
    echo "  │"
    echo "  │    Fix: edit the RackInitializeRequest { ... } block in"
    echo "  │      ${src}"
    echo "  │    then rerun:  voxel image create <commit>   (or build-rss-gen.sh)"
    echo "  └─────────────────────────────────────────────────────────────────"
    echo
fi
exit "${rc}"
