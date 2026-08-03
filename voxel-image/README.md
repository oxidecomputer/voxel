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

## What's baked vs applied at launch

The cut line is right after `omicron-package unpack`. Baked (everything up to that
line):

- pinned pkgs (`tofino looker htop jq`)
- `install_runner_prerequisites.sh`
- `omicron-package unpack` (control-plane zones in `/opt/oxide`)

Not baked (per-topology or per-node, applied by voxel at launch):

- `config-rss.toml` injection (generated on the fly per topology)
- `omicron-package activate` / RSS
- sprockets keys and SMBIOS identity
- `xtask virtual-hardware create` (per-node emulated U.2/M.2, kept ephemeral;
  intersects the QoL disk-killer and cold-boot bugs)
- `scadm propolis load-program` (propolis runtime; cannot persist in a disk image)
- `rpool/dump` zvol (per-VM runtime device)

## Files

- `builder/`: generic single-node falcon builder (`voxel-image-builder`). Boots
  `VBUILD_IMAGE`, mounts `VBUILD_CARGO_BAY` at `/opt/cargo-bay` (`mount_linux` for
  debian/ubuntu, `mount` for helios), and on `launch` runs `INSTALL_SCRIPT` via
  `bash` (linux guests get a read-only 9p mount). `VBUILD_SKIP_INSTALL=1` boots
  only, for smoke tests. Inherits falcon's `launch`/`exec`/`hyperstop`/`destroy`/`serial`.
- `install-cp.sh`: control-plane baked install (helios guest).
- `install-frr.sh`: FRR baked install (debian guest). apt-installs FRR, enables
  `bgpd`, persists IP forwarding. No topology config.
- `build-image.sh`: Helios-side orchestrator (stage, build, launch, verify marker
  content, stop, capture, register). Parameterized by
  `BASE_IMAGE`/`INSTALL_SCRIPT`/`IMAGE_NAME`/`CARGO_BAY`; presets for voxel-cp and
  voxel-frr.

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
and calls `build-image.sh` to bake. See `../voxel/docs/build-vs-run.md`.

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
  override. On voxel's isolated segment (no DHCP), export
  `VOXEL_BUILDER_NET="<cidr> <gw>"` and the installer applies it as a static
  address instead.
- **Capture avoids `falcon snapshot`.** That command re-snapshots `source@base`
  instead of creating `img/<name>@base` (`lib/src/cli.rs:498`; a one-line
  `source`/`dest` fix would enable it). Capture instead uses `zfs send/recv`
  (`CAPTURE_MODE=zfs`) or `dd` piped to `xz` with streaming re-import
  (`CAPTURE_MODE=raw`). `import-raw-img.sh` is unused; it cannot decompress `.xz`.
