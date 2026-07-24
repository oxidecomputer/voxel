# voxel-image (prototype)

Machinery to build pre-built Voxel images via the snapshot-first path: boot one
node, install baked software onto it, then capture its disk as a falcon base image
(optionally a distributable `<name>_0.raw.xz`). Topologies later boot from these
images and apply topology-specific config at launch, so one image serves every
topology. Two image kinds, same machinery:

- **`voxel-cp`**: Helios plus the version-pinned Oxide control plane (`install-cp.sh`).
- **`voxel-frr`**: Debian plus FRR for the customer/edge routers (`install-frr.sh`).
  Config (unnumbered-BGP `frr.conf`, etc.) is generated on the fly at launch, not
  baked. Arista cEOS routers must be supplied separately (proprietary; not built here).

> Status: prototype. The build must run on a Helios host with
> bhyve/propolis/falcon and zfs.

## Where the omicron build runs

By default `voxel image create` runs the whole omicron build (git clone, cargo,
`omicron-package`) INSIDE a `voxel-builder` VM, so the host needs no git / rust /
omicron toolchain - only falcon/zfs/bhyve. Bake that base image once with
`voxel image builder-create`. `--host-build` keeps the legacy in-place host build
for boxes that already have the full toolchain.

Two tiers:

- **`voxel-builder`** (`build-builder.sh` / `provision-builder.sh`): stock helios
  plus git + rustup + omicron's builder prerequisites and a warmed cargo target.
  One-time; per-commit builds boot from it.
- **`voxel-cp`** (`build-cp-vm.sh` / `build-cp-guest.sh`): boots `voxel-builder`,
  builds + bakes the control plane, captures the disk. `build-cp.sh` is the
  `--host-build` equivalent.

## What's baked vs applied at launch

The cut line is right after `omicron-package unpack`. Baked (everything up to that
line):

- pinned pkgs (`tofino looker htop jq`)
- `install_runner_prerequisites.sh`
- `omicron-package unpack` (control-plane zones in `/opt/oxide`)
- the commit-pinned `voxel-rss-gen` (`/opt/oxide/voxel-rss-gen`) + the schema
  manifest (`/opt/oxide/voxel-image.toml`) - so RSS stays contained (below)

Not baked (per-topology or per-node, applied by voxel at launch):

- `config-rss.toml` injection (generated on the fly per topology)
- `omicron-package activate` / RSS
- sprockets keys and SMBIOS identity
- `xtask virtual-hardware create` (per-node emulated U.2/M.2, kept ephemeral;
  intersects the QoL disk-killer and cold-boot bugs)
- `scadm propolis load-program` (propolis runtime; cannot persist in a disk image)
- `rpool/dump` zvol (per-VM runtime device)

### Contained RSS

`voxel-rss-gen` is pinned per omicron commit, so it's baked into the image and run
IN-GUEST, never on the host. At launch voxel stages only `voxel-effective.toml`
(the resolved config) + `rss-rack` onto the RSS node; `voxel-init` runs the baked
rss-gen to produce + inject `config-rss.toml`. For `--wicket-setup` and held
(rack>0) racks - where the host needs the config-rss text - voxel execs the baked
rss-gen inside the booted RSS node and pulls the text back. The sled-agent config
schema comes from the baked `voxel-image.toml` (mirrored to a host stub at
`<build_root>/omicron-<commit>/voxel-image.toml`), so no omicron source is needed
on the box at launch.

## Files

- `builder/`: generic single-node falcon builder (`voxel-image-builder`). Boots
  `VBUILD_IMAGE`, mounts `VBUILD_CARGO_BAY` at `/opt/cargo-bay` (`mount_linux` for
  debian/ubuntu, `mount` for helios), and on `launch` runs `INSTALL_SCRIPT` via
  `bash` (linux guests get a read-only 9p mount). `VBUILD_SKIP_INSTALL=1` boots
  only, for smoke tests. Inherits falcon's `launch`/`exec`/`hyperstop`/`destroy`/`serial`.
- `install-cp.sh`: control-plane baked install (helios guest). The single baking
  authority - `build-cp-guest.sh` (VM build) also calls it.
- `install-frr.sh`: FRR baked install (debian guest). apt-installs FRR, enables
  `bgpd`, persists IP forwarding. No topology config.
- `build-image.sh`: Helios-side orchestrator (stage, build, launch, verify marker
  content, stop, capture, register). Parameterized by
  `BASE_IMAGE`/`INSTALL_SCRIPT`/`IMAGE_NAME`/`CARGO_BAY`; `MANIFEST_OUT` extracts
  the schema manifest to the host stub, `KEEP_BUILDER=1` leaves the builder up.
- `build-builder.sh` / `provision-builder.sh`: bake the `voxel-builder` base image
  (host driver / in-guest provisioner).
- `build-cp-vm.sh` / `build-cp-guest.sh`: the default VM build (host driver stages
  the cargo-bay + launches; guest clones/builds omicron, then calls install-cp.sh,
  bakes rss-gen + manifest, and scrubs the source unless `PERSIST_SOURCE=1`).
- `build-cp.sh`: the `--host-build` fallback (in-place host build).
- `gen-manifest.sh`: derive `voxel-image.toml` (schema shapes) from omicron source.
- `patches/`: omicron source patches applied at build (`nexus-infra-lot-v6.py`,
  `smbios-gimlet.sh`).

## Usage (on the Helios box)

The one-shot path is the `voxel` CLI:

```sh
voxel image builder-create            # one-time: bake the voxel-builder base image
voxel image create <omicron-commit>   # build voxel-cp-<commit> in a builder VM
voxel image create <commit> --persist-source   # keep source in image + VM up for edits
voxel image create <commit> --host-build       # legacy in-place host build
```

By default `voxel image create` drives `build-cp-vm.sh`: it stages the small
host-only inputs (guest scripts, patches, rendered smf configs, sidecar,
voxel-init) into `cargo-bay/vbuild`, boots the `voxel-builder` VM running
`build-cp-guest.sh` (clone + build + package + `install-cp.sh` bake + rss-gen +
manifest, scrub), then `build-image.sh` captures the disk to
`img/voxel-cp-<commit>@base` and mirrors the schema manifest to the host stub.
`--host-build` drives `build-cp.sh` instead (same bake, built in-place on the host).

`build-image.sh` is the lower-level bake (stage, build builder, launch, verify
marker, capture), usable standalone once `cargo-bay/vbuild/{omicron,sidecar}` is
staged. `VERSION` is a label (e.g. the omicron sha); set `FALCON_DATASET` per box
(use the fast pool). Two capture modes:

```sh
cd voxel-image
# a) Fast box-local image (zfs send/recv to img/<name>@base; allocated blocks only):
FALCON_DATASET=testbed/falcon CAPTURE_MODE=zfs VERSION=<commit> ./build-image.sh
# b) Portable artifact out/<name>_0.raw.xz (REGISTER=1 also re-imports it):
FALCON_DATASET=testbed/falcon REGISTER=1 VERSION=<commit> ./build-image.sh
```

Either mode produces falcon base image `img/voxel-cp-<version>@base`, referenceable
from any topology as node image `voxel-cp-<version>`.

### voxel-frr (router image)

No artifact staging needed; `install-frr.sh` apt-installs FRR. Small disk:

```sh
cd voxel-image
FALCON_DATASET=testbed/falcon CAPTURE_MODE=zfs VBUILD_DISK_GB=20 \
  BASE_IMAGE=debian-13.2 INSTALL_SCRIPT=install-frr.sh \
  IMAGE_NAME=voxel-frr-proto CARGO_BAY=./cargo-bay/vfrr \
  VERSION=proto ./build-image.sh
# produces falcon base image img/voxel-frr-proto@base (node image "voxel-frr-proto")
```

## Notes

- **Set `FALCON_DATASET` per box.** falcon reads it (default `rpool/falcon`,
  `lib/src/lib.rs:1818`). Point it at a fast pool when the root NVMe is slow;
  measured zvol writes were ~750 MB/s on a 990 Pro versus as low as ~2 MB/s on the
  root pool.
- **Name with underscores, not hyphens.** falcon rejects hyphens in names
  (`^[A-Za-z]?[A-Za-z0-9_]*$`), so the deployment is `voxel_build`. `hyperstop`
  takes a node name.
- **Override the external NIC if DHCP fails.** A single-ext-link helios node exposes
  `vioif0`, which `install-cp.sh` auto-detects. Set `EXT_IF`/`EXT_INTERFACE` to
  override.
- **Capture avoids `falcon snapshot`.** That command re-snapshots `source@base`
  instead of creating `img/<name>@base` (`lib/src/cli.rs:498`; a one-line
  `source`/`dest` fix would enable it). Capture instead uses `zfs send/recv`
  (`CAPTURE_MODE=zfs`) or `dd` piped to `xz` with streaming re-import
  (`CAPTURE_MODE=raw`). `import-raw-img.sh` is unused; it cannot decompress `.xz`.
