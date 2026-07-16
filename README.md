# voxel

**V**irtual **OX**ide **E**mulation **L**ab - a tool for standing up emulated Oxide rack
deployments on a single Helios host.

Voxel lets you pick an Oxide platform version and a topology (sled count, multi-rack,
customer routers) and launch a faithful emulation of the rack's control plane: the real
[omicron] software running on [falcon]-managed propolis VMs, with SoftNPU switches and
FRR-based customer routers. It is the successor to the testbed `a4x2` topology, reworked
around a first-class CLI and on-the-fly config generation (no hand-maintained per-node
config files).

## Layout

- **`voxel/`** - the `voxel` CLI (clap-based): `image`, `config`, `launch`/`destroy`
  (incl. `--emu-sp`/`--emu-rot` real-firmware SPs and `--wicket-setup`), `rack` and
  `network` (surgical patching + link/port ops), `sp-emu` (emulator artifacts),
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
```

5. `pfexec voxel launch`

Notes: steps 2-4 default the build/dataset under `$HOME/voxel-builds` and `rpool/falcon`; if you use a non-default dataset, set
`falcon.dataset` (and pass `FALCON_DATASET=` to `build-frr.sh`, which doesn't read the config). The `voxel-rss-gen` path and the
sled-agent config shape (`vdevs`/`external_disks`, `data_links`) are both **auto-derived from the image's omicron commit** - no
version knobs to set. Everything is version-independent from the operator's side.

Additionally, `BUILD_ROOT` can be used to choose a different location for building omicron. Like other parameters, you can set it as a flag, voxel config, or environment variable.

```
voxel --build-root /data/builds image create 43bb5af   # flag
voxel config set falcon.build_root /data/builds        # config ([falcon].build_root)
BUILD_ROOT=/data/builds voxel image create 43bb5af     # env
```

So the two build-location knobs are:
- `FALCON_DATASET` where images/topo zvols live (`<ds>/img/...`).
- `BUILD_ROOT` where the omicron checkout + rss-gen build live.

## Isolated external network

By default (`[external] mode = "lan"`), every node's external NIC lands on the
host's default-route interface (or `$EXT_INTERFACE`) and leases an address from
whatever DHCP serves the network that link attaches to—option 1 ("an
existing IPv4 network") of omicron's [how-to-run external networking].
On a host without such a network, voxel can instead build the whole external
segment itself—option 2 ("an external network that only exists on your test
machine") of the same doc, which a4x2 required the user to plumb by hand.

```
voxel config set external.mode isolated
voxel config set external.uplink igb0   # physical NAT uplink for the segment
```

`launch` (and `image create`) then stand up the segment with an
etherstub (`voxel_ext_stub0`, capped at `[external].mtu`—defaulted to 1500 like
a physical external network—so that voxel-init's jumbo probe classifies the
nodes' external NICs correctly), a host VNIC `voxel_ext0` holding the gateway
address (`[external].host_ip`, default `192.168.1.199`), and IPv4 forwarding
plus an ipnat rule out `uplink`.
Node addresses are static because voxel numbers every sled and router
deterministically from `[external].ip_start` (default `192.168.1.10`) and stages
`<addr>/<prefix>` + gateway + DNS into that node's cargo-bay (`external-net`).

The in-guest agent (`voxel-init`) applies the staged address on both sleds and
routers. No DHCP server runs on the segment. The nodes' addresses stay in use
after bring-up (RSS progress is polled over SSH to them, each router NATs rack
egress out its own external address, and the host route to each rack points at
the customer-edge router, `ce`), which is why the segment must exist before
boot.

Operator commands (the same code paths launch uses):

```
voxel network external up      # stand the segment up (--dry-run to preview)
voxel network external check   # PASS/FAIL per item (uplink, links, NAT)
voxel network external down    # remove VNIC + etherstub
```

Notes:
- `down` leaves the ipnat rule and ipv4-forwarding in place: ipnat has no
  single-rule delete, and flushing would drop unrelated rules.
- Unlike the how-to-run recipe, voxel never persists the NAT rules to
  `/etc/ipf/ipnat.conf`: they are loaded at runtime only, so voxel doesn't
  own a shared system file. They don't survive a reboot, and the next
  `launch` (or `up`) reloads them.
- `[external].mtu` must stay below 9000: voxel-init classifies a sled NIC as
  underlay iff it accepts mtu=9000, so the external link has to reject jumbo
  for classification to work. Isolated mode needs the explicit cap because an
  etherstub comes up at MTU 9000, the same as the underlay links; `lan` mode
  inherits a sub-9000 MTU from the physical link for free (launch still
  refuses a >=9000 external link). Raising the mtu (e.g. to 8900) exercises
  jumbo external ingress, which only matters for external-to-external
  forwarding through the switch, whereas guest delivery is capped by the VPC MTU
  regardless.
- If you set `[topology].ce_external_ip`, keep it outside the static node
  range (sleds count up from `ip_start`, then routers in `topology.routers`
  order).
- The manually-run `build-frr.sh` doesn't read the config; so, on an isolated
  box, export `EXT_INTERFACE=voxel_ext_stub0` and
  `VOXEL_BUILDER_NET="192.168.1.198/24 192.168.1.199"` (the builder gets
  `host_ip - 1`) when baking the FRR image.
- A manually plumbed fake network (`fake_external0` etc.) can coexist because
  voxel's link names are distinct, and `$EXT_INTERFACE` always wins.

## Emulated SPs and RoTs (sp-emu)

By default voxel backs each SP with omicron's `sp-sim`. To run the real SP and RoT
firmware instead, voxel uses [sp-emu], which boots unmodified Hubris on emulated
STM32H7 and LPC55 cores. sp-emu is a separate binary that voxel runs inside the
switch zone; it is not a Cargo dependency, so you build it and point voxel at it.

1. Build sp-emu:

   ```
   git clone git@github.com:oxidecomputer/sp-emu.git
   cd sp-emu && cargo build --release        # produces target/release/sp-emu
   ```

2. Point voxel at it in `voxel.toml`, along with the Hubris `-c-emu` images you
   want to run:

   ```toml
   [sp]
   emu = ["sidecar"]                  # which SPs run real firmware: "sidecar", "g0", ...
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

   `--wicket-setup` runs rack setup through wicketd the way an operator would,
   instead of the file-based sled-agent auto-init: it uploads the config, a
   self-signed cert, and the recovery password, starts RSS, and fully populates
   wicket's RACK SETUP page. It needs the emulated SP/RoT fleet, so run it with
   `--emu-rot`. Because it posts a cert, the console then comes up over https
   rather than http.

When you build a cp image, voxel bakes the sp-emu binary and per-role firmware into
the image from `[sp]`, so a launched rack is self-contained and `emu_bin` can be
left unset at launch. Setting `emu_bin` at launch stages it on the fly instead,
which is useful for iterating on sp-emu without rebaking.

[omicron]: https://github.com/oxidecomputer/omicron
[falcon]: https://github.com/oxidecomputer/falcon
[how-to-run external networking]: https://github.com/oxidecomputer/omicron/blob/main/docs/how-to-run.adoc#external-networking
[sp-emu]: https://github.com/oxidecomputer/sp-emu
