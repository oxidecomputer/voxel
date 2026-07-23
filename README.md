# voxel

**V**irtual **OX**ide **E**mulation **L**ab. A tool for standing up emulated Oxide
rack deployments on a single Helios host.

Voxel emulates an Oxide rack's control plane;
[omicron](https://github.com/oxidecomputer/omicron) software on
[falcon](https://github.com/oxidecomputer/falcon)-managed propolis VMs, with
SoftNPU switches and FRR routers. Pick a platform version and a topology
(sled count, multi-rack, BGP/static) and launch. It succeeds the `a4x2`
testbed topology, reworked around a first-class CLI and on-the-fly config
generation.

## Layout

- **`voxel/`**: CLI and launcher
  - **`voxel/rss-gen/`**: typed, release-pinned `config-rss.toml` generator, built
    against the image's omicron source (path dependency).
- **`voxel-config/`**: the `VoxelConfig` model (`voxel.toml`) and all per-topology
  config generation (sled-agent, RSS, FRR, MGS/SP-sim). 
- **`voxel-init/`**: the in-guest bring-up agent baked into the images (gimlet/router
  roles). 
- **`voxel-image/`**: image build machinery (`voxel image create`) and the install
  scripts that bake a control-plane image from an omicron commit.

See [`docs/parameters.md`](docs/parameters.md) for the `voxel.toml` reference, there are a LOT of tuning knobs.

## Building

```sh
cargo build
```

`voxel/rss-gen` builds separately against the target image's omicron source. See
[`voxel-image/build-rss-gen.sh`](voxel-image/build-rss-gen.sh). It will auto-run
if you create a new `voxel image`.

## Quickstart

1. `cargo build` builds voxel.
2. `voxel image create 43bb5af` builds omicron (v21), bakes `voxel-cp-43bb5af`, and
   builds the commit-pinned `voxel-rss-gen` (30-45 min).
3. `bash voxel-image/build-frr.sh proto` bakes `voxel-frr-proto` (omicron-independent;
   build once, reuse for any commit).
4. Configure:

```
voxel config set image.cp voxel-cp-43bb5af
voxel config set image.frr voxel-frr-proto
```

5. `pfexec voxel launch`

A few notes: by default, this will all happen under $HOME. If you don't like that or need
to improve performance by using a separate disk, there are some knobs set via `voxel config set`:

* falcon.dataset: Location for built control plane snapshots
* falcon.build_root: Location where omicron will clone and compile for new images
* falcon.workdir: Location where voxel will do its configuration and setup for new launches

## OPTIONAL, Emulated SPs and RoTs (sp-emu)

By default voxel backs each SP with omicron's `sp-sim`. To run real SP and RoT
firmware, voxel uses [sp-emu](https://github.com/oxidecomputer/sp-emu), which boots
unmodified Hubris on emulated STM32H7 and LPC55 cores. sp-emu is a separate binary
run inside the switch zone, not a Cargo dependency, so build it and point voxel at it.

1. Build sp-emu:

   ```
   git clone git@github.com:oxidecomputer/sp-emu.git
   cd sp-emu && cargo build --release        # produces target/release/sp-emu
   ```

2. Point voxel at it in `voxel.toml`, with the Hubris `-c-emu` images to run:

   ```toml
   [sp]
   emu = ["sidecar"]                  # SPs running real firmware: "sidecar", "g0", ...
   emu_bin = "/path/to/sp-emu/target/release/sp-emu"
   sidecar_image = "/path/to/hubris/.../build-sidecar-c-emu-image-default.zip"
   gimlet_image  = "/path/to/hubris/.../build-gimlet-c-emu-image-default.zip"
   rot_image     = "/path/to/hubris/target/oxide-rot-1/dist/a/final.bin"
   faux_mgs      = "/path/to/faux-mgs"   # optional, for `voxel sp` operator commands
   ```

3. Launch with the emulated fleet:

   ```
   voxel launch --emu-sp                  # real SP firmware behind MGS
   voxel launch --emu-rot                 # also a real RoT (implies --emu-sp; needs rot_image)
   voxel launch --emu-rot --wicket-setup  # drive rack setup through wicketd (real operator flow)
   ```

   `--wicket-setup` runs rack setup through wicketd instead of the file-based
   sled-agent auto-init.
