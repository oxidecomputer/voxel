# voxel

**V**irtual **OX**ide **E**mulation **L**ab. A tool for standing up emulated
Oxide rack deployments on a single Helios host.

Voxel emulates an Oxide rack's control plane:
[Omicron] software on [falcon]-managed propolis VMs, with
SoftNPU switches and FRR routers. Pick a platform version and a topology
(sled count, multi-rack, BGP/static) and launch. It succeeds the `a4x2`
testbed topology, reworked around a first-class CLI and on-the-fly config
generation.

## Layout

- **`voxel/`**: CLI and launcher
- **`voxel-config/`**: the `VoxelConfig` model (`voxel.toml`) and all
  per-topology config generation (sled-agent, RSS, FRR, MGS/SP-sim).
- **`voxel-init/`**: the in-guest bring-up agent baked into the images
  (gimlet/router roles).
- **`voxel-image/`**: image build machinery (`voxel image create`) and the
  install scripts that bake a control-plane image from an omicron commit.

See [parameters] for the `voxel.toml` reference and its many tuning knobs.
See [multicast] for the multicast API (pools, groups, members, probes, omdb)
and the host plumbing that carries externally sourced multicast into a rack.

## Building

```sh
cargo build
```

`voxel` links omicron's own RSS config types (the `rack-init-config` crate in
omicron, pinned to a commit), so `config-rss.toml` is rendered in-process and
schema drift surfaces at voxel compile time.

## Quickstart

1. `cargo build` builds voxel.
2. `pfexec voxel image create` builds the workspace's pinned omicron commit and
   bakes `voxel-cp-<pin>` (30-45 min). Pass a commit to build another version,
   e.g. `pfexec voxel image create 43bb5af`. Image builds boot a builder VM,
   so they need `pfexec` (see [Privileges](#privileges)).
3. `pfexec voxel image create-frr proto` bakes `voxel-frr-proto`
   (omicron-independent; build once, reuse for any commit).
4. Configure:

```
voxel config set image.frr voxel-frr-proto
```

   An unset `image.cp` follows the workspace pin, the image a commitless
   `voxel image create` bakes, so a repin needs no config edit. Set it only
   to select a different image: `voxel config set image.cp voxel-cp-43bb5af`.

5. `pfexec voxel launch`

A few notes: by default, this will all happen under $HOME. If you don't like
that or need to improve performance by using a separate disk, there are some
knobs set via `voxel config set`:

* falcon.dataset: Location for built control plane snapshots, exported as
  `FALCON_DATASET`, with images and topo zvols under `<ds>/img/...`
* falcon.build_root: Location where omicron will clone and compile for new
  images, exported as `BUILD_ROOT`, holding the omicron checkout
* falcon.workdir: Location where voxel will do its configuration and setup for
  new launches
* falcon.ssh_pubkey: SSH public key staged into every node, so `ssh root@<node>`
  authenticates by key rather than the images' empty root password. Defaults to
  the first of `~/.ssh/id_ed25519.pub`, `id_ecdsa.pub`, `id_rsa.pub`; set it
  when the key has a non-standard name:
  `voxel config set falcon.ssh_pubkey ~/.ssh/github_ed25519.pub`

## Privileges

Voxel commands need different privileges:

- `voxel launch`, `voxel destroy`, and image builds run under `pfexec`. They
  manage zfs datasets, data links, and zones, so they need full root.
- `voxel network external ...` and `voxel network multicast ...` run
  unprivileged. Voxel escalates each mutating host command through `pfexec`
  itself, and `--dry-run` prints those as `+ pfexec ...` lines (plus
  `+ ssh root@...` for the commands multicast runs on a router).
- `voxel commtest` runs as your login user, with the `net_icmpaccess`
  privilege described below. It refuses effective uid 0 so a root run cannot
  leave root-owned files in the build worktrees and reports. `--allow-root`
  overrides that where a per-user grant is impractical, at the cost of
  root-owned artifacts under the build root.

Host-side multicast plumbing brackets the rack's lifetime rather than just 
sharing it. Run `voxel network multicast up` after `voxel launch`, since it 
reaches the running `ce` and `cr1`, and run it again after every launch 
because the mirror and the memberships live inside `cr1`. **Run
`voxel network multicast down`** before `voxel destroy`. "Destroy" leaves that
state alone because router-state cleanup requires the explicit router target,
as the per-environment host-route record remains available for the later
`down`.

## Omicron commtest

`voxel commtest` builds and runs Omicron's `commtest` binary against a launched
rack. The source is the Omicron checkout matching the configured control-plane
image, an explicit commit or tag, or the latest upstream `main`. Voxel derives
the selected rack's Nexus API address and takes a test IP pool from the range
directly above the configured service pool.

```sh
# Configured image's Omicron commit (unicast is the default).
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
multicast modes on older, unicast-only versions. The detection inspects the
selected checkout, not the commit baked into the running rack's image. A rack
image that predates probe multicast silently drops a probe's
`multicast_groups` and surfaces later as no-delivery, so be sure to keep the
two commits aligned. `--api URL` overrides the derived Nexus API address, and
`--no-build` runs an existing `<omicron>/target/debug/commtest`.

Voxel injects the arguments commtest has no usable default for, so everything
after `--` reaches it unchanged:

- `--ip-pool-begin` and `--ip-pool-end` override the derived pool. Pass both,
  as voxel rejects a lone bound: pairing a caller's address with one derived
  from `[network]` yields a range that either overlaps the service pool or
  becomes inverted.
- `--mcast-group` (repeatable, `GROUP[@SRC,...]`) replaces voxel's default group
  of `239.1.1.1`. `--mcast-deny-group` on its own, the source-filter negative
  test, also runs the multicast phase, so voxel adds no default group when it
  is present.
- `--icmp-loss-tolerance` overrides voxel's default of `500`, the value
  omicron's a4x2 CI uses. Commtest's own default of `0` suits real hardware,
  but a virtual rack shares one host across every sled VM and sheds a few
  packets at the virtio rings under burst. Pass `--icmp-loss-tolerance 0` to
  restore the strict threshold.
- `--test-duration`, `--warmup`, and `--packet-rate` keep commtest's defaults
  of `100s`, `0s`, and `10`.
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

Voxel keeps its Omicron mirror under `$BUILD_ROOT/commtest` (or
`~/voxel-builds/commtest`) and checks each commit out into a detached
[Git worktree][Git worktrees], so the checkouts, Cargo output, and commtest
reports stay owned by the invoking user. `--source` builds the given checkout
in place, without fetching or changing its Git state.

## Isolated external network (optional)

By default (`[external] mode = "lan"`), every node's external NIC lands on the
host's default-route interface and leases an address from whatever DHCP serves
the network that link attaches to. That is option 1 ("an existing IPv4
network") of Omicron's [how-to-run external networking]. When the LAN under
test is not the default-route network (say, a lab segment on a second NIC),
pin the link with `voxel config set external.link igb1` (`$EXT_INTERFACE`
overrides both).

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
deterministically from `[external].ip_start` (default `172.30.199.10`) and
stages `<addr>/<prefix>` + gateway + DNS into that node's cargo-bay
(`external-net`).

The in-guest agent (`voxel-init`) applies the staged address on both sleds and
routers. No DHCP server runs on the segment. The nodes' addresses stay in use
after bring-up (the RSS watch polls sleds over SSH at those addresses, each
router NATs rack egress out its own external address, and the host route to
each rack points at the customer-edge router, `ce`), which is why the segment
must exist before boot.

Operator commands (the same code paths launch uses):

```
voxel network external up      # stand the segment up (--dry-run to preview)
voxel network external check   # PASS/FAIL per item (uplink, links, NAT)
voxel network external down    # remove VNIC + etherstub + NAT rules
```

Notes:
- `down` removes voxel's two map rules with `ipnat -r`, which deletes only
  the matching rules, so unrelated rules survive. ipv4-forwarding stays
  enabled, as it is a host-global setting.
- Unlike the how-to-run recipe, voxel never persists the NAT rules to
  `/etc/ipf/ipnat.conf`: the rules live in the kernel only, so voxel doesn't
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

## Multicast host plumbing (optional)

This is scaffolding for the emulated environment, not something a rack needs in
production. A real rack sits behind a customer network that already routes
multicast (static multicast routes, or PIM upstream with IGMP toward hosts), so
an externally sourced group reaches the rack's uplinks on its own and the rack
takes it from there. Voxel's customer network is a few FRR boxes carrying
unicast BGP, so the host has to stand in for that upstream. Per [RFD 488], the
rack signals nothing upstream by design in v1: assignment is static and
API-driven, with IGMP host-proxying ([RFC 4605]) proposed atop it. The
[multicast] doc records the production equivalent of each piece below, along
with the TODO for reworking this scaffolding into a listening upstream once
host-proxying lands.

Standing in for it needs three things the rack cannot arrange itself:

- a host route pointing the group at the customer-edge router.
- a mirror on the transit router that copies the group's flood from its
  host-facing NIC onto every scrimlet-facing NIC. The routers run FRR for
  unicast BGP and no multicast routing daemon, so without the mirror a group's
  frames stop at `ce`. We use a mirror rather than a daemon because the only
  job here is getting frames onto the switch ports. The rack replicates to
  members itself, so a router that forwarded properly would duplicate that
  work, and the FRR image stays as it is.
- a static link-layer membership for the group's Ethernet address on the
  router's host-facing NIC. Nothing on the router joins the group, so without
  it, the NIC drops the frames before the mirror ever sees them. This exact
  `ip maddress` entry is solely a workaround for voxel's mirror-only Linux
  router. In actual environments, customers would have to arrange upstream
  delivery toward both rack uplinks, whether via PIM/IGMP, static multicast
  routes and joins on the upstream routers, or an equivalent mechanism.

```
voxel network multicast up      # route + tc/mirred mirror + membership (--dry-run to preview)
voxel network multicast check   # one line per item, PASS/FAIL overall
voxel network multicast down    # remove the mirror, memberships, and routes
```

`up` defaults to `239.1.1.1`, the group `voxel commtest --traffic multicast`
uses; pass `--group` (repeatable) for others. `check` and `down` default to
whatever is currently plumbed for this Falcon environment, discovered from its
`.falcon/` state and voxel's own `tc` filters on the mirror router. A host route
belonging to another environment is left alone. An unreachable router with a
known address narrows that to this environment's host routes alone, with a
warning. In `lan` mode, the address must first be read from the running router
over the falcon console. If that lookup fails, `check` reports an error and
live `down` stops after host-route cleanup rather than treating the router as
gone; a dry-run skips the router preview. `check` closes with each group's
underlay mapping, read from `swadm multicast list` in both switch zones; the
lines are informational and appear once the rack has programmed the group. All
three work in whichever `[external]` mode is set: isolated mode derives the
mirror router's address from config, while `lan` mode reads its DHCP lease over
the falcon console.

Notes:
- Every scrimlet is a mirror target because external multicast ingresses at
  whichever switch holds the group's external NAT entry, which the rack elects
  and the host cannot see. The switches without the entry drop their copy, so
  there is no duplicate replication.
- The mirror is one `tc` filter per group with the targets chained as `action
  mirred` clauses. One filter per scrimlet does not work: the first matching
  filter ends flower classification, whereas chained actions all run.
- `voxel commtest --traffic multicast` (and `both`) refuses to start when a
  route or `tc` filter is missing, since the symptom is otherwise a receive
  timeout minutes into the run. `--setup-mcast` plumbs the missing pieces
  for the groups that run uses instead of refusing.
- `check` asserts only the host side, the route, the filter, and the
  membership. Proving delivery past the switch needs a member in the group.
  The rack side (pools, groups, members, probes) is in [multicast]. Short of
  a full `commtest` run, the cheapest confirmation is a probe joined to the
  group plus `ping -s <group>` from the host, which prints a reply line per
  responder.

## Emulated SPs and RoTs (sp-emu, optional)

By default voxel backs each SP with Omicron's `sp-sim`. To run real SP and RoT
firmware, voxel uses [sp-emu], which boots unmodified Hubris on emulated
STM32H7 and LPC55 cores. sp-emu runs as its own binary inside the switch zone
rather than as a Cargo dependency, so build it and point voxel at it.

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

When you build a cp image, voxel bakes the sp-emu binary and per-role firmware
into the image from `[sp]`, so a launched rack is self-contained and `emu_bin`
can be left unset at launch. Setting `emu_bin` at launch stages it on the fly
instead, which is useful for iterating on sp-emu without rebaking.

[multicast]: docs/multicast.md
[parameters]: docs/parameters.md
[Omicron]: https://github.com/oxidecomputer/omicron
[falcon]: https://github.com/oxidecomputer/falcon
[how-to-run external networking]: https://github.com/oxidecomputer/omicron/blob/main/docs/how-to-run.adoc#external-networking
[sp-emu]: https://github.com/oxidecomputer/sp-emu
[Git worktrees]: https://git-scm.com/docs/git-worktree
[RFD 488]: https://rfd.shared.oxide.computer/rfd/0488
[RFC 4605]: https://www.rfc-editor.org/rfc/rfc4605
