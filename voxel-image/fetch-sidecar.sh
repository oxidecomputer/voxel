#!/bin/bash
#
# fetch-sidecar.sh [DEST] - download the SoftNPU sidecar_lite artifacts (scadm +
# libsidecar_lite.so) for install-cp.sh to bake into voxel-cp.
#
# Pinned to sidecar-lite zl/multicast HEAD (a merge of main): adds multicast
# table programming and underlay NAT-encapsulation. Must be at least the
# forward_v6-capable rev: a4x2-main's b96d785 sidecar_lite panics on the RFC
# 5549 v4-over-v6 routes the image's dendrite emits for unnumbered BGP.
# Override SIDECAR_LITE_REV to build another rev. Cached under
# .sidecar-lite/<rev>/ so repeat builds don't re-download.
# The Helios box reaches buildomat; the builder VM may not, which is why this
# runs on the host and stages into the builder cargo-bay.
#
set -euo pipefail

HERE="$(cd -- "$(dirname "$0")" >/dev/null 2>&1 && pwd -P)"
SIDECAR_LITE_REV="${SIDECAR_LITE_REV:-6f3311e8acd7e7e95c167aab61188355a93afe72}"
SIDECAR_CACHE="${SIDECAR_CACHE:-${HERE}/.sidecar-lite/${SIDECAR_LITE_REV}}"
DEST="${1:-${HERE}/cargo-bay/vbuild/sidecar}"
URL="https://buildomat.eng.oxide.computer/public/file/oxidecomputer/sidecar-lite/release"

mkdir -p "${SIDECAR_CACHE}" "${DEST}"
for a in scadm libsidecar_lite.so; do
    if [[ ! -s "${SIDECAR_CACHE}/${a}" ]]; then
        echo "[fetch-sidecar] fetching ${a} @ ${SIDECAR_LITE_REV}"
        curl -sSfL --retry 10 -o "${SIDECAR_CACHE}/${a}" "${URL}/${SIDECAR_LITE_REV}/${a}"
    fi
    chmod +x "${SIDECAR_CACHE}/${a}"
    cp "${SIDECAR_CACHE}/${a}" "${DEST}/${a}"
done
echo "[fetch-sidecar] staged scadm + libsidecar_lite.so -> ${DEST}"
