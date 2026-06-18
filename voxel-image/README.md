# voxel-image (prototype)

Machinery to build **pre-built Voxel images** via the **snapshot-first** path:
boot one node, install baked software onto it, then capture its disk as a falcon
base image (and optionally a distributable `<name>_0.raw.xz`). Voxel topologies
later boot from these images and apply *topology-specific* config at launch - so
one image serves every topology. Two image kinds, same machinery:

- **`voxel-cp`** - Helios + the version-pinned Oxide control plane (`install-cp.sh`).
- **`voxel-frr`** - Debian + FRR for the customer/edge routers (`install-frr.sh`).
  Config (unnumbered-BGP `frr.conf`, etc.) is generated on the fly at launch, not
  baked. Arista cEOS routers are bring-your-own (proprietary; not built here).

> Status: prototype. The build itself must run on a **Helios host** with
> bhyve/propolis/falcon + zfs. It cannot run on macOS.

## What's baked vs. applied at launch

The cut line is right after `omicron-package unpack`. We **bake** everything up
to there:

- pinned pkgs (`tofino looker htop jq`)
- `install_runner_prerequisites.sh`
- `omicron-package unpack` -> control-plane zones in `/opt/oxide`

We **do not** bake (these are per-topology / per-node, applied by voxel at launch):

- `config-rss.toml` injection (generated on the fly per topology)
- `omicron-package activate` / RSS
- sprockets keys + SMBIOS identity
- `xtask virtual-hardware create` (per-node emulated U.2/M.2 - kept ephemeral;
  intersects the QoL disk-killer + cold-boot bugs)
- `scadm propolis load-program` (propolis runtime; can't persist in a disk image)
- `rpool/dump` zvol (per-VM runtime device)

## Files

- `builder/` - generic single-node falcon builder (`voxel-image-builder`). Boots
  `VBUILD_IMAGE`, mounts `VBUILD_CARGO_BAY` -> `/opt/cargo-bay` (uses `mount_linux`
  for debian/ubuntu, `mount` for helios), and on `launch` runs `INSTALL_SCRIPT`
  (via `bash`, since linux guests get a read-only 9p mount). `VBUILD_SKIP_INSTALL=1`
  just boots (for smoke tests). Inherits falcon's `launch`/`exec`/`hyperstop`/
  `destroy`/`serial`.
- `install-cp.sh` - control-plane baked install (helios guest).
- `install-frr.sh` - FRR baked install (debian guest): apt-installs FRR, enables
  `bgpd`, persists IP forwarding. No topology config.
- `build-image.sh` - Helios-side orchestrator (stage -> build -> launch -> verify
  marker *content* -> stop -> capture -> register). Parameterized by `BASE_IMAGE` /
  `INSTALL_SCRIPT` / `IMAGE_NAME` / `CARGO_BAY`; presets for voxel-cp & voxel-frr.

## Usage (on the Helios box)

The one-shot path is the `voxel` CLI, which drives `build-cp.sh`:

```sh
voxel image create <omicron-commit>   # clone + build omicron, generate the
                                      # build-time smf configs from voxel-config,
                                      # package, fetch the sidecar, bake voxel-cp-<commit>
```

`build-cp.sh` clones omicron at `<commit>`, generates the build-time
`mgs-sim`/`sp-sim`/`sled-agent` configs from `voxel-config` (no a4x2), runs
`omicron-package package`, stages the result into `cargo-bay/vbuild/{omicron,sidecar}`,
and calls `build-image.sh` to bake. (See `../voxel/docs/build-vs-run.md`.)

`build-image.sh` is the lower-level bake (stage -> build builder -> launch -> verify
marker -> capture), usable standalone once `cargo-bay/vbuild/{omicron,sidecar}` is
staged. `VERSION` is just a label (e.g. the omicron sha); set `FALCON_DATASET` per
box (use the fast pool). Two capture modes:

```sh
cd voxel-image
# a) Fast box-local image (zfs send/recv -> img/<name>@base; allocated blocks only):
FALCON_DATASET=testbed/falcon CAPTURE_MODE=zfs VERSION=<commit> ./build-image.sh
# b) Portable artifact out/<name>_0.raw.xz (REGISTER=1 also re-imports it):
FALCON_DATASET=testbed/falcon REGISTER=1 VERSION=<commit> ./build-image.sh
```

Either way you get falcon base image `img/voxel-cp-<version>@base`, referenceable
from any topology as node image `voxel-cp-<version>`.

### voxel-frr (router image)

No artifact staging needed - `install-frr.sh` apt-installs FRR. Small disk:

```sh
cd voxel-image
FALCON_DATASET=testbed/falcon CAPTURE_MODE=zfs VBUILD_DISK_GB=20 \
  BASE_IMAGE=debian-13.2 INSTALL_SCRIPT=install-frr.sh \
  IMAGE_NAME=voxel-frr-proto CARGO_BAY=./cargo-bay/vfrr \
  VERSION=proto ./build-image.sh
# -> falcon base image img/voxel-frr-proto@base (node image "voxel-frr-proto")
```

## Validated end-to-end (2026-06-14)

Ran on the Helios box: builder installed the full 16-zone control plane (and
**confirmed `omicron-package unpack` works without `virtual-hardware create`**),
captured the image, and a single node booted from `voxel-cp-proto` with all 16
zones intact in `/opt/oxide`. Note a single node can only validate that the
image boots - a live Nexus needs the full multi-sled voxel topology (RSS +
trust-quorum + a scrimlet/switch zone), since validated end-to-end (see
`../docs/voxel-roadmap.md`).

## Notes / gotchas

- **`FALCON_DATASET` per box.** falcon honors it (`lib/src/lib.rs:1818`, default
  `rpool/falcon`). On a box with a slow root NVMe, point it at the fast pool -
  zvol writes were ~750 MB/s on a 990 Pro vs ~2-38 MB/s on the root pool.
- **falcon names reject hyphens** (`^[A-Za-z]?[A-Za-z0-9_]*$`) - deployment is
  `voxel_build`; `hyperstop` needs a node name.
- **External NIC.** A single-ext-link helios node exposes `vioif0`;
  `install-cp.sh` auto-detects the first `vioif`. Override with `EXT_IF` /
  `EXT_INTERFACE` if DHCP doesn't come up.
- **We don't use `falcon snapshot`** - it snapshots `source@base` again instead
  of creating `img/<name>@base` (`lib/src/cli.rs:498`); a one-line upstream fix
  (`source` -> `dest`) would make it usable. We capture via zfs send/recv (mode
  `zfs`) or `dd|xz` + streaming re-import (mode `raw`). `import-raw-img.sh` is
  NOT used (it can't decompress `.xz`).

## Next steps

- **DONE** - the multi-sled voxel topology boots `voxel-cp`, generates
  RSS/sprockets/SMBIOS/sp-sim/mgs/router configs on the fly, and runs RSS -> live
  Nexus; `voxel image create <commit>` builds `voxel-cp` from a fresh omicron clone
  (build-from-source, since TUF lacks the i86pc global-zone software - see
  `../docs/voxel-roadmap.md`). De-a4x2 is complete (a4x2 removed from the workspace).
- Phase 2: move artifact production to the declarative helios `image-builder`
  path for the open-source repo + buildomat CI that tracks omicron releases/main.
