#!/bin/bash
#
# gen-manifest.sh <omicron-src> - print the voxel-image.toml schema manifest.
#
# Derives the sled-agent config shapes from the omicron source (the ground truth
# for that commit) and prints them as TOML. Baked into the image at
# /opt/oxide/voxel-image.toml and mirrored to the host stub; voxel reads it at
# launch (topo.rs detect_sled_schema) instead of parsing the source itself.
#
set -euo pipefail

SRC="${1:?usage: gen-manifest.sh <omicron-src>}"
COMMIT="${COMMIT:-unknown}"
CFG="${SRC}/sled-agent/src/config.rs"

dl="list"
dk="vdevs"
grep -q "data_links: DataLinks" "${CFG}" 2>/dev/null && dl="tagged"
grep -q "pub external_disks" "${CFG}" 2>/dev/null && dk="external_disks"

cat <<EOF
commit = "${COMMIT}"
data_links_schema = "${dl}"
disks_schema = "${dk}"
EOF
