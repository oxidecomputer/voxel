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


```sh
cargo build
```

`voxel/rss-gen` is built separately against the target image's omicron source. See
[`voxel-image/build-rss-gen.sh`](voxel-image/build-rss-gen.sh).

## Quickstart

1. `cargo build` # build the CLI 
2. `voxel image create 43bb5af` # builds omicron v21 (as of this writing), bakes voxel-cp-43bb5af, builds the commit-pinned voxel-rss-gen (~30-45 min)
3. `bash voxel-image/build-frr.sh proto` # bakes voxel-frr-proto (omicron-independent; build once, reuse for any commit)
4. Configure:

```
voxel config set image.cp voxel-cp-43bb5af
voxel config set image.frr voxel-frr-proto
voxel config set image.data_links_schema tagged # Only required for Omicron v21+ for now
voxel config set falcon.rss_gen ~/voxel-builds/omicron-43bb5af/target/debug/voxel-rss-gen
```

5. pfexec voxel launch

Notes: steps 2-4 default the build/dataset under $HOME/voxel-builds and rpool/falcon; if you use a non-default dataset, set
falcon.dataset (and pass FALCON_DATASET= to build-frr.sh, which doesn't read the config). data_links_schema is the one knob that
tracks the omicron version. Everything else is version-independent.

Additionally, BUILD_ROOT can be used to choose a different location for building omicron. Like other parameters, you can set it as a flag, voxel config, or environment variable.

voxel --build-root /data/builds image create 43bb5af   # flag
voxel config set falcon.build_root /data/builds        # config ([falcon].build_root)
BUILD_ROOT=/data/builds voxel image create 43bb5af     # env

So the two build-location knobs are:
- FALCON_DATASET where images/topo zvols live (<ds>/img/...).
- BUILD_ROOT where the omicron checkout + rss-gen build live.

