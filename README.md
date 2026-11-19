# voxel

**V**irtual **OX**ide **E**mulation **L**ab. A tool for standing up emulated
Oxide rack deployments on a single Helios host.

Voxel emulates an Oxide rack's control plane:
[Omicron] software on [falcon]-managed propolis VMs, with
SoftNPU switches and FRR routers. Pick a platform version and a topology
(sled count, multi-rack, BGP/static) and launch. It succeeds the `a4x2`
testbed topology, with a CLI and on-the-fly config
generation.

## Layout

- **`voxel/`**: CLI and launcher
- **`voxel-config/`**: the `VoxelConfig` model (`voxel.toml`) and all
  per-topology config generation (sled-agent, RSS, FRR, MGS/SP-sim).
- **`voxel-init/`**: the in-guest bring-up agent baked into the images
  (gimlet/router roles).
- **`voxel-image/`**: image build machinery (`voxel image create`) and the
  install scripts that bake a control-plane image from an omicron commit.

See [parameters] for the `voxel.toml` reference.
See [multicast] for the multicast API (pools, groups, members, probes, omdb)
and the host setup for externally sourced multicast ingress.

## Building

```sh
cargo build
```

`voxel` links omicron's own RSS config types (the `rack-init-config` crate in
omicron, pinned to a commit), so `config-rss.toml` is rendered in-process and
schema drift surfaces at voxel compile time.

## Quickstart

For a complete walkthrough, from preparing a Helios host through launching,
accessing, and tearing down a rack, see the
[operator guide](docs/operator-guide.adoc).

> [!NOTE] Whenever the command `voxel` is mentioned, the best way to use it is
> to access the debug build: `./target/debug/voxel <command>`

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

   An unset `image.cp` follows the workspace pin (the image
   `voxel image create` bakes when no commit is given). A repin needs no configuration
   edit. Set it only when selecting a different image:
   `voxel config set image.cp voxel-cp-43bb5af`.

5. `pfexec voxel launch`

Omicron pins OPTE in `tools/opte_version` and installs it as the
`driver/network/opte` package. A new OPTE build can require a newer Helios
kernel than the one in falcon's default guest image, `helios-3.1`. In that
case, `install_opte.sh` installs an older OPTE and the bake stops with an
"opte API mismatch" error.

To update the base image before baking, run:

```
pfexec voxel image create-base helios-3.1-20261119
pfexec voxel image create --base-image helios-3.1-20261119
```

`create-base` boots `helios-3.1`, runs `pkg update` and captures the result as
a new base image. Reuse it for later builds until you want a newer Helios.

A few notes: by default, this will all happen under $HOME. If you don't like that or need
to improve performance by using a separate disk, there are some knobs set via `voxel config set`:

* `falcon.dataset`: Location for built control plane snapshots, exported as
  `FALCON_DATASET`, with images and topo zvols under `<ds>/img/...`
* `falcon.build_root`: Location where omicron will clone and compile for new images,
  exported as `BUILD_ROOT`, holding the omicron checkout
* `falcon.workdir`: Location where voxel will do its configuration and setup for new launches

## Privileges

Voxel commands need different privileges:

- `voxel launch`, `voxel destroy`, and image builds run under `pfexec`. They
  manage zfs datasets, data links, and zones, so they need full root.
- `voxel network external ...` and `voxel network multicast ...` run
  unprivileged. Voxel escalates each mutating host command through `pfexec`
  itself (for multicast: `route`, the `mdb -kw` checksum write, and the
  `/etc/system.d/voxel` install), and `--dry-run` prints those as
  `+ pfexec ...` lines (plus `+ ssh root@...` for the commands multicast runs
  on a router).
- `voxel commtest` runs as your login user, with the `net_icmpaccess`
  privilege described below. It refuses effective uid 0 so a root run cannot
  leave root-owned files in the build worktrees and reports. `--allow-root`
  overrides that where a per-user grant is impractical, at the cost of
  root-owned artifacts under the build root.

Run `voxel network multicast up` after `voxel launch` to set up the multicast
networking configuration. The command must be run again after every launch
because the `(S,G)` paired assignments live inside the routers.

`voxel network multicast down` withdraws the router assignments and host
routes while the rack is still up. You can run it before `voxel destroy`,
but you don't have to, because `destroy` removes the environment's recorded
host routes before teardown and drops the state record once it succeeds, and
router state dies with the router VMs. Run `down` after a `destroy` and it
reports there is nothing to do.

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
voxel commtest d41a8c3a2aa5768255ad00545e53d3077b1b56ce --traffic unicast

# Latest origin/main.
voxel commtest main --traffic unicast

# Multicast phases need the host plumbing set up first
# (commtest won't run without it).
voxel network multicast up

# Run both phases from a local multicast-capable checkout, unmodified.
voxel commtest --source /oxide/workspace/omicron --traffic both

# Pass commit-specific commtest arguments after `--` (the default multicast
# group is 239.1.1.1 when no --mcast-group is supplied).
voxel network multicast up --group 239.10.0.1
voxel commtest --source /oxide/workspace/omicron --traffic multi -- run \
  --test-duration 5m --mcast-group 239.10.0.1

# Cleanup resources created by that commit's commtest.
voxel commtest d41a8c3a2aa5768255ad00545e53d3077b1b56ce -- cleanup
```

`--traffic` accepts `unicast`/`uni`, `multicast`/`multi`, or `both`. Voxel
detects whether the selected commit supports multicast and refuses the
multicast modes on older, unicast-only versions. The detection inspects the
selected checkout, not the commit baked into the running rack's image. A rack
image that predates probe multicast silently drops a probe's
`multicast_groups` and surfaces later as no-delivery. Be sure to keep the
two commits aligned. `--api URL` overrides the derived Nexus API address, and
`--no-build` runs an existing `<omicron>/target/debug/commtest`.

Voxel supplies arguments for which commtest has no usable default. It passes
everything after `--` through unchanged:

- `--ip-pool-begin` and `--ip-pool-end` override the derived pool. Pass both;
  voxel rejects one passed on its own, because pairing a caller's
  address with one derived from `[network]` can yield a range that overlaps the
  service pool or is inverted.
- `--mcast-group` (repeatable, `GROUP[@SRC,...]`) replaces voxel's default group
  of `239.1.1.1`. `--mcast-deny-group` on its own, the source-filter negative
  test, also runs the multicast phase, so voxel adds no default group when it
  is present.
- `--icmp-loss-tolerance` overrides voxel's default of `500`. Commtest's own
  default of `0` suits real hardware, but a virtual rack shares one host across
  every sled VM and can shed packets at the virtio rings under burst.
  Pass `--icmp-loss-tolerance 0` to restore the strictest threshold.
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
network") of Omicron's [how-to-run external networking]. Given a LAN under
test that is not the default-route network (e.g., a lab segment on a second
NIC), you can pin this link with `voxel config set external.link igb1`
(`$EXT_INTERFACE` overrides both).

For a LAN without DHCP, set
`voxel config set external.addressing static`. Voxel stages each node's
address from `[external].ip_start` instead of leasing one. `subnet` and
`host_ip` become the LAN's values. This is still
[option 1][how-to-run external networking]: the network exists, and voxel only
stages the node addresses without owning the network.

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
after bring-up (i.e., the RSS watch polls sleds over SSH at those addresses;
each router NATs rack egress out of its own external address, and the host route
to each rack points at the customer-edge router, `ce`), which is why the segment
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
- Image builds (`voxel image create` and `create-frr`) read `[external]` config
  and `falcon.dataset` like `launch` does. On an isolated box, the builder VM
  takes `ip_start - 1` (`172.30.199.9` by default) with `host_ip` as its
  gateway; `VOXEL_BUILDER_NET="<cidr> <gw>"` overrides that address if set.
- A manually plumbed fake network (`fake_external0` etc.) can coexist because
  voxel's link names are distinct, and `$EXT_INTERFACE` always wins.

## Multicast host plumbing (optional)

This setup is specific to the emulated environment. A real rack sits
behind a customer network that already routes multicast (either
configured as static multicast routes, or via PIM upstream with IGMP toward
hosts). That network forwards externally sourced multicast to the rack's
uplinks. Voxel uses FRR routers with unicast BGP for the customer network and
requires the host setup below for multicast ingress. Per [RFD 488], the rack
signals nothing upstream
by design in v1: assignment is static and API-driven, with IGMP host-proxying
([RFC 4605]) proposed atop it. The [multicast] doc records a production
equivalent of each component below.

The host and fabric routers need:

- a host route pointing the group at the customer-edge router.
- static PIM VIFs on the fabric routers. The rack-facing links are passive
  because a sidecar answers no PIM Hello ([RFC 7761]). Each link gets a
  multicast forwarding interface, and because the rack-facing links are
  (BGP-)unnumbered and PIM needs an IPv4 address to create a VIF, voxel numbers
  them with `/32`s from `192.0.2.0/24` (unique per router and link). The
  generated `frr.conf` includes these settings.
- a static multicast route on each selected fabric router selecting its
  rack-facing links, with the host as the source. Receiver state never
  changes the path.

*Note* for the host: it must stop offloading checksums.
`voxel network multicast up` applies `dohwcksum=0` and adds
`set ip:dohwcksum = 0` to `/etc/system.d/voxel`, a setting that survives reboot.
Otherwise, the looped-back multicast packet arrives with a zero checksum and
gets dropped at the receiver router's IP stack. The `down` command keeps the
checksum offload disabled.

```
voxel network multicast up      # host route + mroutes (--dry-run to preview)
voxel network multicast check   # one line per item, PASS/FAIL check
voxel network multicast down    # remove the assignments and routes
```

The `up` command defaults to `239.1.1.1`, the same group
`voxel commtest --traffic multicast` uses. We can pass
`--group` args (repeatable) for others. The `check` and `down` commands default
to the groups this environment recorded in its `.falcon/` state directory.
Groups observed on a router that are not stored in the recorded state and
host routes belonging to another environment are never touched.

Note: a `down` dry-run previews each router's commands when its address resolves
and, instead of failing, skips a router it cannot resolve with a notice.
Without a rack launched, only the host-side commands are previewed.

All three commands work in whichever `[external]` mode is currently set. A
router's address comes from the configuration under static addressing (both
for isolated mode and for a static `lan` segment). Only a DHCP `lan`
configuration reads the lease over the falcon console; if that lookup fails,
`check` reports the error and `down` stops right after the host-route cleanup.

Notes:
- A group takes exactly one path by default (`delivery="single"`), which is the
  first forwarding router toward `switch0`. `--steer` allows for other paths,
  but only one switch per router at most (FRR withdraws a static mroute just by
  `(S,G)`); see [multicast] for the valid options.
- Each static mroute includes the host's source address.
- `voxel commtest --traffic multicast` (and `both`) are rejected at startup
  when any of the necessary plumbing items are missing or if no selected path
  enters the target rack.
- The `check` command compares the host route and selected source with the
  recorded state. It also checks PIM and each router's live `(S,G)` assignment.
  To verify delivery past the switch, join a member to the group.
  The rack-side setup (e.g., pools, groups, members, probes) is
  documented in the [multicast] doc. Test it with a probe joined to the group and
  `pfexec ping -s -t 16 <group>` from the host.

## Emulated SPs and RoTs (sp-emu, optional)

By default voxel backs each SP with Omicron's `sp-sim`. To run real SP and RoT
firmware, voxel uses [sp-emu], which boots unmodified Hubris on emulated
STM32H7 and LPC55 cores.

1. Build sp-emu:

   ```
   git clone git@github.com:oxidecomputer/sp-emu.git
   cd sp-emu && cargo build --release        # produces target/release/sp-emu
   ```

2. Point voxel at it in `voxel.toml`:

   ```toml
   [sp]
   emu = ["sidecar", "g0", "g1", "g2"]   # SPs running real firmware
   emu_bin = "/path/to/sp-emu/target/release/sp-emu"
   faux_mgs = "/path/to/faux-mgs"        # optional, for `voxel sp` operator commands
   ```

3. Launch the emulated fleet:

   ```
   voxel launch --emu
   ```

   `--emu` runs stock SP and RoT firmware behind MGS. Rack setup goes
   through wicketd's commission API on every
   launch, `--emu` or not; `launch --init-rss` is the sp-sim-only shortcut
   that stages a config-rss.toml for sled-agent to initialize the rack
   itself.

The firmware itself comes from the image's own TUF repo: `image create
--from-tuf` extracts the gimlet and sidecar SP archives, the RoT slot A image
and the RoT bootloader, and stamps their location on the image, so a rack
cannot boot firmware that disagrees with the release it reports.

To boot *different* firmware - which is how you give a firmware update
something to do, or test a hubris change, name the images in `[sp]` and they
win over the image's own:

```toml
[sp]
gimlet_image  = "/path/to/hubris/.../build-gimlet-image-c.zip"
sidecar_image = "/path/to/hubris/.../build-sidecar-image-c.zip"
rot_image     = "/path/to/rot-bart-a.zip"
bootleby_image = "/path/to/bootleby-bart.zip"
```

When you build a cp image, voxel bakes the sp-emu binary into it, so a launched
rack is self-contained and `emu_bin` can be left unset at launch. Setting
`emu_bin` at launch stages it on the fly instead, which is useful for iterating
on sp-emu without rebaking.

[multicast]: docs/multicast.md
[parameters]: docs/parameters.md
[Omicron]: https://github.com/oxidecomputer/omicron
[falcon]: https://github.com/oxidecomputer/falcon
[how-to-run external networking]: https://github.com/oxidecomputer/omicron/blob/e086187a226158599864bd8674b588948126697b/docs/how-to-run.adoc#external-networking
[sp-emu]: https://github.com/oxidecomputer/sp-emu
[Git worktrees]: https://git-scm.com/docs/git-worktree
[RFD 488]: https://rfd.shared.oxide.computer/rfd/0488
[RFC 4605]: https://www.rfc-editor.org/rfc/rfc4605
[RFC 7761]: https://www.rfc-editor.org/rfc/rfc7761
