# voxel

**V**irtual **OX**ide **E**mulation **L**ab - a tool for standing up emulated Oxide rack
deployments on a single Helios host.

Voxel lets you pick an Oxide platform version and a topology (sled count, multi-rack,
customer routers) and launch a faithful emulation of the rack's control plane: the real
[omicron](https://github.com/oxidecomputer/omicron) software running on
[falcon](https://github.com/oxidecomputer/falcon)-managed propolis VMs, with SoftNPU
switches and FRR-based customer routers. It is the successor to the testbed `a4x2`
topology, reworked around a first-class CLI and on-the-fly config generation (no
hand-maintained per-node config files).

## Layout

- **`voxel/`** - the `voxel` CLI (clap-based): `image`, `config`, `launch`/`destroy`,
  `exec`/`serial`/`status`, and pilot-style access (`tp login`, `host login`). Uses
  falcon as a library rather than wrapping it.
  - **`voxel/rss-gen/`** - typed, release-pinned `config-rss.toml` generator, built
    against the image's omicron source (path dependency); excluded from the workspace.
- **`voxel-config/`** - the `VoxelConfig` model (`voxel.toml`) and all per-topology
  config generation (sled-agent, RSS, FRR, MGS / SP-sim). Pure Rust, testable anywhere.
- **`voxel-init/`** - the in-guest bring-up agent baked into the images (gimlet/router
  roles); replaces the old launch shell scripts.
- **`voxel-image/`** - image build machinery (`voxel image create`) and the install
  scripts that bake a control-plane image from an omicron commit.

See [`docs/voxel-roadmap.md`](docs/voxel-roadmap.md) for the engineering roadmap and
[`docs/build-vs-run.md`](docs/build-vs-run.md) for the build-time vs. launch-time split.

## Building

`voxel` and `voxel-image` link [falcon](https://github.com/oxidecomputer/falcon), which
is illumos-only, so they build on a Helios host. `voxel-config` and `voxel-init` build
on any platform:

```sh
cargo build -p voxel-config -p voxel-init   # any platform
cargo build -p voxel                        # Helios (libfalcon)
```

`voxel/rss-gen` is built separately against the target image's omicron source - see
[`voxel-image/build-rss-gen.sh`](voxel-image/build-rss-gen.sh).
