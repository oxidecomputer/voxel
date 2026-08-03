# voxel

**V**irtual **OX**ide **E**mulation **L**ab. A tool for standing up emulated Oxide
rack deployments on a single Helios host.

Voxel emulates an Oxide rack's control plane;
[omicron] software on [falcon]-managed propolis VMs, with
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

* falcon.dataset: Location for built control plane snapshots, exported as
  `FALCON_DATASET`, with images and topo zvols under `<ds>/img/...`
* falcon.build_root: Location where omicron will clone and compile for new images,
  exported as `BUILD_ROOT`, holding the omicron checkout and the rss-gen build
* falcon.workdir: Location where voxel will do its configuration and setup for new launches

## Privileges

Voxel commands need different privileges:

- `voxel launch`, `voxel destroy`, and image builds run under `pfexec`. They
  manage zfs datasets, data links, and zones, so they need full root.
- `voxel network external ...` runs unprivileged. Voxel escalates each
  mutating host command through `pfexec` itself, and `--dry-run` prints those
  as `+ pfexec ...` lines.
- `voxel commtest` runs as your login user, with the `net_icmpaccess`
  privilege described below. It refuses effective uid 0 so a root run cannot
  leave root-owned files in the build worktrees and reports. `--allow-root`
  overrides that where a per-user grant is impractical, at the cost of
  root-owned artifacts under the build root.

## omicron commtest

`voxel commtest` builds and runs omicron's `commtest` binary against a launched
rack. The source is the omicron checkout matching the configured control-plane
image, an explicit commit or tag, or the latest upstream `main`. Voxel derives
the selected rack's Nexus API address and takes a test IP pool from the range
directly above the configured service pool.

```sh
# Configured image's omicron commit (unicast is the default).
voxel commtest

# A specific commit (older unicast-only versions are supported).
voxel commtest 43bb5af --traffic unicast

# Latest origin/main.
voxel commtest main --traffic unicast

# Run both phases from a local multicast-capable checkout, unmodified.
voxel commtest --source /oxide/workspace/omicron --traffic both

# Pass commit-specific commtest arguments after `--` (the default multicast
# group is 239.1.1.1 when no --mcast-group is supplied).
voxel commtest --source /oxide/workspace/omicron --traffic multi -- run \
  --test-duration 5m --mcast-group 239.10.0.1

# Cleanup resources created by that commit's commtest.
voxel commtest 43bb5af -- cleanup
```

`--traffic` accepts `unicast`/`uni`, `multicast`/`multi`, or `both`. Voxel
detects whether the selected commit supports multicast and refuses the
multicast modes on older, unicast-only versions. `--api URL` overrides the
derived Nexus API address, and `--no-build` runs an existing
`<omicron>/target/debug/commtest`.

Voxel only injects the arguments commtest has no default for, so everything
after `--` reaches it unchanged:

- `--ip-pool-begin` and `--ip-pool-end` override the derived pool. Pass both.
  Passing one alone is refused, because the half voxel derives would overlap
  the service pool.
- `--mcast-group` (repeatable, `GROUP[@SRC,...]`) replaces voxel's default group
  of `239.1.1.1`. `--mcast-deny-group` on its own, the source-filter negative
  test, also runs the multicast phase, so voxel adds no default group when it
  is present.
- `--test-duration`, `--warmup`, `--packet-rate`, and `--icmp-loss-tolerance`
  keep commtest's defaults of `100s`, `0s`, `10`, and `0`.
- `--api-timeout` (default `60m`) is a top-level argument, so it goes before
  the `run` subcommand.

commtest opens raw ICMP sockets, so it needs `net_icmpaccess` as an effective
privilege. On a dedicated development system, an administrator can add it to a
user's default privileges:

```sh
pfexec usermod -K defaultpriv=basic,net_icmpaccess "$USER"
```

Start a new login session afterward and confirm that `ppriv $$` lists
`net_icmpaccess` in the effective set.

Voxel keeps its omicron mirror under `$BUILD_ROOT/commtest` (or
`~/voxel-builds/commtest`) and checks each commit out into a detached
[Git worktree][Git worktrees], so the checkouts, Cargo output, and commtest
reports stay owned by the invoking user. `--source` builds the given checkout
in place, without fetching or changing its Git state.

## Isolated external network (optional)

By default (`[external] mode = "lan"`), every node's external NIC lands on the
host's default-route interface (or `$EXT_INTERFACE`) and leases an address from
whatever DHCP serves the network that link attaches to. That is option 1 ("an
existing IPv4 network") of omicron's [how-to-run external networking].
On a host without such a network, voxel can instead build the whole external
segment itself, option 2 ("an external network that only exists on your test
machine") of the same doc, which a4x2 required the user to plumb by hand.

```
voxel config set external.mode isolated
voxel config set external.uplink igb0   # physical NAT uplink for the segment
```

`launch` (and `image create`) then stand up the segment with an
etherstub (`voxel_ext_stub0`, capped at `[external].mtu`, which defaults to 1500
like a physical external network so that voxel-init's jumbo probe classifies the
nodes' external NICs correctly), a host VNIC `voxel_ext0` holding the gateway
address (`[external].host_ip`, default `172.30.199.199`), and IPv4 forwarding
plus an ipnat rule out `uplink`.
Node addresses are static because voxel numbers every sled and router
deterministically from `[external].ip_start` (default `172.30.199.10`) and stages
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
- The manually-run `build-frr.sh` doesn't read the config, so on an isolated
  box export `EXT_INTERFACE=voxel_ext_stub0` and
  `VOXEL_BUILDER_NET="172.30.199.198/24 172.30.199.199"` (the builder gets
  `host_ip - 1`) when baking the FRR image. Export `FALCON_DATASET` to match
  `falcon.dataset` for the same reason; otherwise, the bake registers the image
  under falcon's default dataset, `launch` keeps using the old one, and
  nothing reports an error.
- A manually plumbed fake network (`fake_external0` etc.) can coexist because
  voxel's link names are distinct, and `$EXT_INTERFACE` always wins.

## Emulated SPs and RoTs (sp-emu, optional)

By default voxel backs each SP with omicron's `sp-sim`. To run real SP and RoT
firmware, voxel uses [sp-emu], which boots unmodified Hubris on emulated
STM32H7 and LPC55 cores. sp-emu is a separate binary
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

When you build a cp image, voxel bakes the sp-emu binary and per-role firmware into
the image from `[sp]`, so a launched rack is self-contained and `emu_bin` can be
left unset at launch. Setting `emu_bin` at launch stages it on the fly instead,
which is useful for iterating on sp-emu without rebaking.

[omicron]: https://github.com/oxidecomputer/omicron
[falcon]: https://github.com/oxidecomputer/falcon
[how-to-run external networking]: https://github.com/oxidecomputer/omicron/blob/main/docs/how-to-run.adoc#external-networking
[sp-emu]: https://github.com/oxidecomputer/sp-emu
[Git worktrees]: https://git-scm.com/docs/git-worktree
