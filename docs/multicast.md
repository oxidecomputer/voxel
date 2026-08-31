# Multicast on a voxel rack

Multicast uses three API objects on an Oxide rack: an IP pool, a group, and its
members. An operator creates the pool. Nexus creates a group when the first
member joins an address covered by the pool, and removes it after the last
member leaves. This document also describes the host topology that carries
externally sourced multicast into the rack.

The traffic here originates on the **host**, not in a guest, so these runs
test the external-to-underlay ingress path, from host through switch to sled for
each subscribing member. This document does not cover the
guest-sourced egress path (or OPTE's sled-side next-hop selection). Exercising
that path would take a sender from inside the rack, where, here, the members
only answer: a probe's echo reply is unicast, so no guest originates multicast
data traffic. A joining guest does emit an IGMPv3 or MLDv2 membership report,
which is a multicast frame, but those go to the protocol's own link-local
address (224.0.0.22, ff02::16) rather than to the group, and OPTE encapsulates
only what its multicast-to-physical table maps. That table holds admin-scoped
underlay addresses for materialized groups alone, so a report is denied rather
than encapsulated and the sled-side next-hop selection never runs. The rack
drives forwarding from the API subscription, not from the report. Consuming it
instead is what [RFD 488]'s dynamic group identification (IGMP snooping and
querying) proposes. This document does not cover that mode.

`voxel commtest --traffic multi` performs every API step below automatically.
The API sections document what it creates, and how to create the same objects
by hand when taking a run apart.

## Prerequisites

- A launched rack with RSS complete and the external API answering. Multicast
  needs no configuration knobs of its own, as nothing in `voxel.toml` enables
  it.
- The host-side setup below (routes, mirror, membership). `voxel network
  multicast up` installs it in whichever `[external]` mode is set, and `voxel
  commtest --setup-mcast` does the same before a run. Isolated mode resolves
  ce's and cr1's addresses from config alone, while `lan` mode reads their DHCP
  leases from the running nodes over the falcon console. The plumbing lives in
  [`multicast.rs`], the run wrapper that invokes it in [`commtest.rs`].
- A control-plane image whose Omicron carries multicast. `voxel commtest
  --traffic multi` detects an older, unicast-only commit and fails with that
  diagnosis before building. The detection inspects the `--source` checkout,
  not the commit baked into the running rack's image. A rack image that
  predates probe multicast silently drops a probe's `multicast_groups` and
  surfaces later as no-delivery, so be sure to keep the two commits aligned.
- The commtest privilege setup from the [README's Privileges section]: the run
  needs `net_icmpaccess` in the effective set and refuses uid 0.
- `curl` and `jq` for the API examples below. Voxel installs neither, and
  neither is needed for a `voxel commtest` run.

### Dependencies

> TODO: These branches will return to their main branches as they land upstream.
> Until then, the Omicron pin is the pushed leaf
> of `zl/mcast-build`, held by the workspace `rack-init-config` dependency in
> `Cargo.toml`; `build.rs` surfaces this bookmark to `voxel image create`,
> so that a commitless `voxel image create` builds the pin.

The multicast stack spans multiple repositories, but voxel itself pins only two
of them: the Omicron commit handed to `voxel image create` (`OMICRON_REPO`
selects the clone source) and the sidecar-lite artifact rev the build fetches
(`SIDECAR_LITE_REV`, already defaulted to the multicast rev). Everything else
comes from the chosen Omicron commit's own pins. Selecting the correct Omicron
commit selects the rest of the stack.

| Repository | Branch / rev | PR | Carried by | Merge path |
| --- | --- | --- | --- | --- |
| omicron | `zl/mcast-build` leaf, [`a4183214`](https://github.com/oxidecomputer/omicron/pull/11128/commits/a41832147bb7fe1517ad8409983b1a9a03ae6691) | #11128 open, top of the PR stack below | workspace `rack-init-config` pin, via `voxel image create` | stack lands bottom-up into `main`, starting at #9912 |
| dendrite | `multicast-e2e`, `447aa04f` | #224 open | Omicron `tools/dendrite_*` pins | #224 to `main` |
| maghemite | `zl/ddm-mcast`, `36e43651` | #696 open, stacked on `zl/mrib`; related #402 (`zl/mgd-ddm-meta`) | Omicron `tools/maghemite_*` pins | #696 to `main` after `zl/mrib` |
| opte | `master`, `0525f2f95` (0.41.506) | #1012 merged 2026-07-25; #1049 (`zl/mcast-source-validation`, `3e2615e5`) open | Omicron `Cargo.toml` / `tools/opte_version` | landed; Omicron pins 0.41.506 intentionally (master's later #1040 is a comment-only xde change). #1049 aligns source-address validation with dpd, nexus and mgd, and is not pinned yet |
| propolis (rack image) | `zl/multicast`, `3c07d60a` | [#1093] open | Omicron `package-manifest.toml` (guest propolis-server) | [#1093] to `master` |
| thundermuffin | `zl/multicast-joiner`, `486559bc` | #14 open | Omicron `package-manifest.toml` (probe zone, prebuilt) | #14 to `main` |
| sidecar-lite | `zl/multicast`, `461cbe19` | #152 open | `voxel image create` (`SIDECAR_LITE_REV`) | #152 to `main` |
| softnpu | `zl/multicast`, `284c6830` | #183 open | Omicron `tools/softnpu_version` (xtask `SOFTNPU_COMMIT`) | #183 to `main` |
| p4 | `zl/multicast` (`p4rs`) | #240 open | transitive, via softnpu and sidecar-lite lockfiles | #240 to `main`, then #183/#152 repoint |

The propolis row above describes only the server shipped inside the rack image.
The host-side propolis falcon runs is a separate integration checkout, the local
`mcast-smbios-test` branch (`8e283bee`), selected by `[falcon].propolis_binary`
rather than by the Omicron package manifest. What it carries and how to build it
are covered with the host-side pieces below.

The omicron work is a stack of PRs, each based on the branch below it. Voxel
pins the leaf, so one commit carries the whole chain:

| PR | Branch | Base |
| --- | --- | --- |
| [omicron#11128] | `zl/mcast-build` (voxel's pin) | `zl/mcast-e2e-commtest` |
| [omicron#11118] | `zl/mcast-e2e-commtest` | `zl/probe-multicast` |
| [omicron#10520] | `zl/probe-multicast` | `zl/multicast-mgd-ddm` |
| [omicron#10346] | `zl/multicast-mgd-ddm` | `zl/multicast-m2p-forwarding` |
| [omicron#10070] | `zl/multicast-m2p-forwarding` | `multicast-e2e` |
| [omicron#9912] | `multicast-e2e` | `main` |

Thundermuffin's [multicast joiner] is the receiver-side prerequisite for probe
tests, shipped in the image by Omicron's probe package. It runs inside each
probe zone and holds the ASM or SSM socket membership that lets the zone's IP
stack accept and answer multicast, standing in for a guest application.
Voxel's `ip maddress` entry on `cr1` is independent sender-path plumbing: it
admits the group's Ethernet address to the mirror router so `tc` can copy the
frame toward the rack. Probe-based commtest requires both.

Two pieces sit outside the image, on the host itself:

- The **host propolis** needs the `mcast-smbios-test` integration branch listed
  above. It combines propolis `zl/multicast`'s viona MAC-filter wiring (PR
  [#1093]), the SMBIOS type 1 fix from [#1200], and the softnpu management-UART
  deadlock fix from [#1206]. Without the MAC-filter wiring, the sled VMs
  receive no multicast at all.

  This is the falcon VM boundary, not the rack's guest instances: each sled
  is itself a propolis VM whose illumos `vioif` negotiates
  `VIRTIO_NET_F_CTRL_RX` but never programs a multicast table, so viona
  narrows the link to no-multicast at feature negotiation and drops every
  group frame. The SMBIOS fix is not multicast-specific: without it falcon's
  a4x2 identity never reaches the sled VM and RSS fails trust quorum validation
  on any voxel rack. The integration branch is local and is never itself PR'd,
  so each fix is proposed upstream from its own branch off `master`.

  Build it with the `falcon` feature:

  ```sh
  git checkout mcast-smbios-test  # zl/multicast + propolis#1200 + #1206
  cargo build --release --bin propolis-server --features falcon
  ```

  and point `[falcon].propolis_binary` at the resulting
  `target/release/propolis-server`.
- The **host viona kernel module** needs the MAC-filter table ioctls from
  stlouis#986, Gerrit change [775] on illumos-gate, merged into `stlouis` as
  `5ffff4b8` on 2026-08-19. Run the host on an illumos build at or past that
  commit. From a [helios] checkout, pull the latest `stlouis`, build it, and
  install it onto a new boot environment (see [`helios-build onu`]):

  ```sh
  git -C projects/illumos pull    # latest stlouis, includes 5ffff4b8
  ./helios-build build-illumos -q
  ./helios-build onu -t viona-mac-filters
  ```

  then reboot into the new BE.

  Hosts without the change fall back to unfiltered RX, which still delivers
  a single copy but without filtering.

  Verify a host with `nm /usr/kernel/drv/amd64/viona | grep set_mac_filters`.

## Reaching the API

Nexus answers on the rack's `[network].service_pool` addresses that external DNS
does not hold. With the defaults (pool `198.51.100.20-.29`, external DNS on
`.20` and `.21`), probe upward from `.22`:

```sh
for ip in $(seq 22 29); do
    curl -sf -m 2 -o /dev/null "http://198.51.100.$ip/v1/ping" \
        && echo "198.51.100.$ip"
done
```

`voxel commtest` derives `--api` from the same address sweep, though with
plain TCP connection checks on ports 80 and 443 (external DNS addresses as a
fallback) rather than an HTTP ping.

Authentication for these examples uses the recovery silo. Its `voxel.toml`
defaults are silo `recovery`, user `recovery`, and password `oxide`, matching
the login `commtest` performs. `oxide auth login` runs a device flow and stores
a token the later calls reuse. On a headless host, `--no-browser` prints a URL
that can be opened elsewhere.

```sh
API=http://198.51.100.23
oxide auth login --host "$API"
oxide api /v1/multicast-groups
```
The typed CLI makes the same call:
```sh
oxide experimental multicast-group list
```

*Note*: every multicast endpoint currently carries the `experimental` tag, so
the typed commands land under `oxide experimental ...` in a CLI build whose
spec includes them. Even then the structured request fields, a join's
`source_ips` and a probe's `multicast_groups` and `pool_selector`, get no
flag of their own and are reachable only through `--json-body <file>`. The
examples below use `oxide api`, the raw passthrough, which takes that same
JSON on stdin and works regardless of how much of the surface the installed
CLI knows about. The typed form follows each call where one exists.

Without a CLI, `curl` does the same work. A local login returns a session cookie
that stands in for the token:

```sh
SESSION=$(curl -si -X POST "$API/v1/login/recovery/local" \
    -H 'content-type: application/json' \
    -d '{"username":"recovery","password":"oxide"}' \
    | sed -n 's/^set-cookie: \(session=[^;]*\).*/\1/p')

curl -s -H "Cookie: $SESSION" "$API/v1/multicast-groups" | jq
```

## IP pools

A pool carries a `pool_type` discriminator, `unicast` (the default) or
`multicast`. Multicast pools are further constrained:

- One IP version per pool (`ip_version`, `v4` or `v6`).
- Every range in the pool must be entirely Any-Source Multicast (ASM) or
  entirely Source-Specific Multicast (SSM), never both. SSM is `232.0.0.0/8`
  for IPv4 and the per-scope `ff3x::/32` blocks for IPv6 ([RFC 4607]);
  everything else is ASM. Within the v4 range, `232.0.0.0/24` is refused,
  reserved by [RFC 4607] §4.3, so a v4 SSM pool starts at `232.0.1.0`. An
  ASM group set and an SSM group set therefore need two pools. The split is
  about address space, not filtering: joins on ASM addresses may still carry
  `source_ips` (see [Join forms](#join-forms)).
- A silo may hold at most one default pool per (pool type, IP version) pair,
  four in total. A multicast pool linked non-default is still usable; the group
  is then resolved by address rather than by the silo default.

```sh
oxide api /v1/system/ip-pools --method POST --input - <<'JSON'
{ "name": "mcast-v4-asm", "description": "ASM multicast pool",
  "ip_version": "v4", "pool_type": "multicast" }
JSON

oxide api /v1/system/ip-pools/mcast-v4-asm/silos --method POST --input - <<'JSON'
{ "silo": "recovery", "is_default": false }
JSON

oxide api /v1/system/ip-pools/mcast-v4-asm/ranges/add --method POST --input - <<'JSON'
{ "first": "239.100.0.1", "last": "239.100.0.2" }
JSON
```
The typed equivalents live under plain `oxide ip-pool`, since pools are stable
CLI surface, not experimental:
```sh
oxide ip-pool create --name mcast-v4-asm --description "ASM multicast pool" \
    --ip-version v4 --pool-type multicast
oxide ip-pool silo link --pool mcast-v4-asm --silo recovery --is-default false
oxide ip-pool range add --pool mcast-v4-asm \
    --first 239.100.0.1 --last 239.100.0.2
```

The SSM pool, `mcast-v4-ssm`, is the same three calls with `232.100.0.1` as
both ends of the range. Members also need an ordinary unicast pool for their
external addresses. `commtest` creates one named `default` over its
`--ip-pool-begin/--ip-pool-end` range and links it to the silo as the default.

Both pool list endpoints filter by type, which is how to pick the multicast
pools out of a mixed fleet. The installed CLI's `oxide ip-pool list` predates
the filter flags, so this one stays raw:

```sh
oxide api "/v1/system/ip-pools?pool_type=multicast"
oxide api "/v1/ip-pools?pool_type=multicast"   # silo-scoped view
```

## Groups

A group is not created directly. There is no `POST /v1/multicast-groups`, for
example. A group comes into existence when the first member joins an address
(or a name) that a linked multicast pool covers, and Nexus reaps it once its
last member leaves. The `multicast_reconciler` background task drives its
transitions and the switch programming behind them.

```
  ip pool   pool_type = multicast, one ip_version, ASM xor SSM
  mcast-v4-asm : 239.100.0.1 - 239.100.0.2
       |
       |  linked to the silo
       v
  first join of an address the pool covers
  (instance PUT .../multicast-groups/G, or probe create)
       |
       v
  group  239.100.0.1
    Creating --[multicast_reconciler]--> Active
       ^                                    |
       |  later joins attach as members     |
       |                                    v
  members  myvm (*,G),  probe0@g0 (S,G)
    Joining --[multicast_reconciler]--> Joined --> Left
       |
       |  last member leaves
       v
  group empty
    Deleting --[multicast_reconciler]--> Deleted
```

The read side is `GET /v1/multicast-groups`, `/v1/multicast-groups/{group}`, and
`/v1/multicast-groups/{group}/members`, where `{group}` is a name, a UUID, or
the multicast IP.

```sh
oxide api /v1/multicast-groups \
    | jq -r '.items[] | "\(.name) \(.multicast_ip) \(.state)"'
oxide api /v1/multicast-groups/239.100.0.1/members
```
The typed CLI makes the same calls:
```sh
oxide experimental multicast-group list
oxide experimental multicast-group view --multicast-group 239.100.0.1
oxide experimental multicast-group member list --multicast-group 239.100.0.1
```

A group view carries `multicast_ip`, `ip_pool_id`, `state`, the deduplicated
union of its members' `source_ips`, and `has_any_source_member`. The union is
contributed to only by members that joined with an explicit source list, so a
non-empty `source_ips` does not imply that every member filters by source;
`has_any_source_member` is what answers that.

### Join forms

Every join takes the same body: an optional `source_ips` array and an optional
`ip_version` (needed only when a join creates a group by name and both an
IPv4 and an IPv6 default multicast pool are linked). The forms differ in
what the source list means:

- **ASM**, e.g. `239.100.0.1` with no sources. An any-source `(*, G)` join.
- **SSM**, e.g. `232.100.0.1` with `source_ips`. SSM builds no shared `(*, G)`
  tree, so every join must carry a source list, and Nexus rejects a bare SSM
  join.
- **Source-bound ASM**, e.g. `239.100.0.2` with `source_ips`. An ASM group
  joined `(S, G)`, which exercises source filtering ([RFC 3376]) on an address
  that does not require it.

A member's source list has a defined maximum of 32 entries
(`MAX_SOURCE_IPS_PER_MEMBER`), and the union across a group's members is capped
at 256 (`MAX_SOURCE_IPS_PER_GROUP`). Neither bound comes from the protocol.
IGMPv3 ([RFC 3376]) and MLDv2 ([RFC 3810]) leave per-group source-list size
implementation-defined, and implementations diverge accordingly: Linux defaults
to 10 (`igmp_max_msf`), FreeBSD to 128 (`maxsocksrc`). 32 covers the typical
one to eight sources per channel while keeping a single member's fan-out from
dominating the shared `(S, G)` forwarding state, and 256 bounds what one group
can install in the dataplane. For any source-filtered join, the list must
include whatever sends the verification traffic, or the dataplane correctly
drops it. That is also the recipe for the negative case:
join with a source list that excludes the host and assert nothing arrives
(`commtest --mcast-deny-group GROUP@SRC`). The examples below write the
sending address as `$SRC`, resolved under [Sending traffic](#sending-traffic).

### Instances

An instance joins and leaves by group identifier, and lists its own
memberships. The first join implicitly creates the group unless the
identifier is a UUID, which must name an existing group.

```sh
oxide api "/v1/instances/myvm/multicast-groups/239.100.0.1?project=classone" \
    --method PUT --input - <<'JSON'
{}
JSON

oxide api "/v1/instances/myvm/multicast-groups/232.100.0.1?project=classone" \
    --method PUT --input - <<JSON
{ "source_ips": ["$SRC"] }
JSON

oxide api "/v1/instances/myvm/multicast-groups/239.100.0.1?project=classone" \
    --method DELETE

oxide api "/v1/instances/myvm/multicast-groups?project=classone"
```
The typed CLI covers the same four operations. `source_ips` has no flag,
and `--json-body` takes a file path, not stdin:
```sh
oxide experimental instance multicast-group join --project classone \
    --instance myvm --multicast-group 239.100.0.1
cat > ssm-join.json <<JSON
{ "source_ips": ["$SRC"] }
JSON
oxide experimental instance multicast-group join --project classone \
    --instance myvm --multicast-group 232.100.0.1 --json-body ssm-join.json
oxide experimental instance multicast-group leave --project classone \
    --instance myvm --multicast-group 239.100.0.1
oxide experimental instance multicast-group list --project classone \
    --instance myvm
```

### Probes

A probe is the lightest member primitive: it needs no guest, its create
request pins it to a named sled, and it auto-replies to an echo request sent
to a group it has joined, so that a plain `ping` observes delivery. This is why
`commtest` uses probes, and why they are the easiest member to exercise
manually. *Note* that memberships are fixed at creation. To change them,
recreate the probe.

```sh
sleds=$(oxide api /v1/system/hardware/sleds | jq -r '.items[].id')

i=0
for sled in $sleds; do
    oxide api "/experimental/v1/probes?project=classone" --method POST --input - <<JSON
{
  "name": "probe$i",
  "description": "multicast probe $i",
  "sled": "$sled",
  "pool_selector": { "type": "explicit", "pool": "default" },
  "multicast_groups": [
    { "group": "239.100.0.1" },
    { "group": "239.100.0.2", "source_ips": ["$SRC"] },
    { "group": "232.100.0.1", "source_ips": ["$SRC"] }
  ]
}
JSON
    i=$((i + 1))
done

oxide api "/experimental/v1/probes?project=classone" \
    | jq -r '.items[].external_ips[] | select(.ip | test("\\.")) | .ip'
```
For the typed CLI, `--json-body` carries the whole request, so no other body
flags are needed. It takes a file path, and `/dev/stdin` lets a heredoc
stand in for one:
```sh
oxide experimental system probe create --project classone \
    --json-body /dev/stdin <<JSON
{
  "name": "probe0",
  "description": "multicast probe 0",
  "sled": "$sled",
  "pool_selector": { "type": "explicit", "pool": "default" },
  "multicast_groups": [
    { "group": "239.100.0.1" },
    { "group": "239.100.0.2", "source_ips": ["$SRC"] },
    { "group": "232.100.0.1", "source_ips": ["$SRC"] }
  ]
}
JSON
oxide experimental system probe list --project classone
```

The external addresses those probes report are the responders a group ping
should draw. A member view (`/v1/multicast-groups/{group}/members`) reports
`kind` as `instance` or `probe`, which is what selects the interpretation of
`parent_id`.

## Control-plane verification

Reading group and member state apart from the dataplane is the difference
between "the group was never realized" and "the group is fine and the frames are
not arriving". `omdb` lives in the switch zone, which `voxel tp exec` enters
directly:

```sh
omdb() { voxel tp exec -c "/opt/oxide/omdb/bin/omdb $*" switch0; }

omdb db multicast pools
omdb db multicast groups
omdb db multicast members
omdb db multicast info --ip 239.100.0.1
```

`db multicast groups` reports each group's `STATE` ("Creating", "Active",
"Deleting", or the terminal "Deleted"), its `UNDERLAY_IP`, the source
allowlist, and a `MEMBERS` column of `name@sled` (`probe:name@sled` for
probes). `db multicast members` adds per-member state ("Joining", "Joined",
"Left") and the assigned sled, and filters on `--group-ip`, `--state`,
`--sled-id`, and `--source-ip`.

Underlay replication is built from the multicast routes DDM exchanges, so a
group that is "Active" with "Joined" members, but no delivery, points at the
underlay or the ingress path. Missing host setup or a source filter that
excludes the sender leaves the same state, so rule the host side out first, and
then check the underlay:

```sh
omdb nexus multicast ddm-peers --mcast
```

`--mcast` narrows the listing to the peers that actually become multicast
underlay members: a DDM session in "Exchange" on a switch rear-port interface.

The reconciler itself:

```sh
omdb nexus background-tasks doc
omdb nexus background-tasks show multicast_reconciler
omdb nexus background-tasks print-report multicast_reconciler
```

Its report counts groups created ("Creating" to "Active"), groups verified,
empty groups marked for deletion, members processed, external entries
misplaced, and groups re-elected when a switch owner moves. Transitions
happen on the reconciler's periodic cadence. `omdb -w nexus background-tasks
activate multicast_reconciler` forces a pass immediately instead of waiting
for it. Anything that mutates requires the `-w`/`--destructive` flag. A group
that stays stuck across repeated passes is not waiting on that cadence: the
task's report carries the switch, DDM, or DPD errors that block the
transition.

## Topology management

Everything above deals with rack state. None of it moves a packet from the host
into the rack, because the segment between them is a pair of plain Debian
routers.

`voxel network multicast up` installs all three pieces below, and it runs after
`voxel launch`, not before: it resolves `ce`'s address and reaches `cr1` over
SSH, both of which need running nodes. **Run it again after every launch**. The
mirror filter and the link-layer membership are runtime state inside the `cr1`
VM and go away when it does, so only the host routes carry across, which is what
the ownership record described below exists to track. The command is idempotent,
so re-running it over the same groups costs nothing.

### Host routes

The routers speak unicast BGP through FRR and carry no multicast routing daemon,
so the host needs an explicit route per group address pointed at the customer
edge, `ce`. This applies to SSM groups too: the route uses the bare group
address, and the source only matters to the join.

`ce`'s address is `[topology].ce_external_ip` when that is set. Otherwise, in
isolated mode, voxel numbers node addresses deterministically from
`[external].ip_start`: sleds in order, then routers in `[topology].routers`
order. The stock four-sled `ce, cr1, cr2` topology puts `ce` at
`172.30.199.14`. In `lan` mode it holds a DHCP lease. We read it off the guest
with `voxel host login ce` and `ip -4 -br addr show scope global`.

`voxel network multicast up` installs these routes, so the commands below are
the manual equivalent, useful when breaking a run apart. A stale route to a
previous, now-dead `ce` silently drops the group's traffic, so delete before
adding:

```sh
CE=172.30.199.14

for group in 239.100.0.1 239.100.0.2 232.100.0.1; do
    pfexec route delete -host "$group" 2>/dev/null || true
    pfexec route add -host "$group" "$CE"
done
```

The route table belongs to the Helios host rather than to any one, specific
Falcon environment, so voxel records each group's gateway in
`.falcon/multicast-<hex environment name>.json` and treats that record, not the
gateway address, as proof of ownership. `up` writes the record before adding
the route. An interrupted run then leaves a record with no route, which the
next `up` overwrites and a groupless `down` reads as nothing to remove. A
reverse ordering would leave a route that nothing afterwards could prove was
voxel's to begin with. Both commands stop rather than guess when a group's
route is not in the record: `up` refuses the group instead of displacing the
route, and `down` leaves it alone and names it. Deleting the record while its
routes exist, therefore, locks voxel out of those groups until they are removed
by hand with `pfexec route delete -host <group> <gateway>`.

Two isolated-mode environments sharing an `[external]` subnet are the one case
the record cannot separate. They place a given group on the same `ce` address,
so the two records describe a single kernel route, and a `down` in either
environment takes it away from both.

### The `cr1` mirror

Host routes only put the frames on the external segment, addressed toward
`ce`, and no router forwards them across the transit path into a switch. The
naming matches the customer-edge and transit split of [RFC 4364]: `ce` is the
edge, and only the `cr*` transit routers hold switch-facing links. `cr1` sits
on the same host-facing segment and sees the flood directly, so rather than
introducing a multicast routing daemon, a stock iproute2 `tc` ingress filter
mirrors each group from `cr1`'s host-facing NIC to both switch-facing NICs
via the [`mirred`][tc-mirred] action.

Router NIC names follow falcon's link ordering, the same derivation
`VoxelConfig::router_ext_iface` encodes: a fabric router's links are `ce` first,
then every scrimlet across every rack, then its own external NIC. For the stock
single-rack, this is the four-sled topology that makes up `cr1`:

- `enp0s8`, toward `ce`
- `enp0s9`, toward `g0` (switch0)
- `enp0s10`, toward `g3` (switch1)
- `enp0s11`, host-facing, where the multicast flood arrives

The filter mirrors both switch-facing NICs. External multicast ingresses at
whichever switch holds the group's external NAT entry, and Omicron's
designated-forwarder election ([omicron#11128]) gates that entry to a single
switch, a designation made in the control plane, not visible to the mirror. The
elected switch ingests and replicates to the underlay, while the other has no
entry and drops its copy, so mirroring to both costs only the second copy and
guarantees the elected one is reached. The mirror does not choose the ingress
switch.

`voxel network multicast up` installs these filters itself, and `voxel
commtest --setup-mcast` does the same before a run. The manual equivalent,
run on `cr1` (`voxel host login cr1`; `host exec` covers sleds only):

```sh
IIF=enp0s11

# Ensure the shared clsact qdisc exists without recreating it. Deleting it
# would drop every ingress filter on the device, including ones this guide
# does not own. Per-group filters below use `replace`, which is idempotent.
tc qdisc add dev $IIF clsact 2>/dev/null || true

# Both sidecars must be chained actions within one filter per group. Separate
# per-sidecar filters do not work: the first matching filter ends flower
# classification, so the second never fires and a group elected to that switch
# goes undelivered. Chained actions all run, since mirred's default control is
# pipe.
pref=100
for group in 239.100.0.1 239.100.0.2 232.100.0.1; do
    # The explicit handle keeps `replace` idempotent. Left at 0, the kernel
    # treats a re-run as a fresh insert and flower returns EEXIST for the
    # duplicate key.
    tc filter replace dev $IIF ingress handle 1 pref $pref protocol ip \
        flower dst_ip $group \
        action mirred egress mirror dev enp0s9 \
        action mirred egress mirror dev enp0s10
    pref=$((pref + 1))
done

tc filter show dev $IIF ingress
```

### Group membership on `cr1`

The mirror only sees frames the NIC accepts, so until the NIC accepts a
group's frames, the filter above matches nothing. An IPv4 group's frames
arrive under a derived Ethernet address: the group's low-order 23 bits placed
into `01:00:5e:00:00:00` ([RFC 1112] section 6.4). The prefix is IANA's OUI,
`00-00-5E`, and of the 2^24 multicast identifiers one OUI provides, only the
lower half is allotted to IPv4 ([RFC 7042] section 2.1.1), so a group's 28
significant bits fold onto 23 and 32 groups alias each Ethernet address. The
example groups above demonstrate the aliasing: `239.100.0.1` and `232.100.0.1`
both map to `01:00:5e:64:00:01`.

A NIC accepts a multicast address only while some form of membership holds it.
Voxel's FRR router speaks no multicast protocol directly and nothing else on
that emulated router joins, so without a membership the NIC drops the flood
before the tc ingress hook and the mirror counters stay at zero. A static
link-layer membership stands in for the missing join:

```sh
ip maddress add 01:00:5e:64:00:01 dev $IIF   # 239.100.0.1 and 232.100.0.1
ip maddress add 01:00:5e:64:00:02 dev $IIF   # 239.100.0.2
```

`voxel network multicast up` derives and pins these itself, one per distinct
Ethernet address, recording each one it adds under `/run` on `cr1`. `down`
removes a membership only when no remaining group still maps onto it and the
record shows `up` created it, so one the router already held (the kernel's
all-hosts mapping, or a join inside the router) is left alone. The record
lives and dies with the router, exactly like the memberships themselves.

This static membership is part of voxel's test scaffolding, not a customer or
rack configuration requirement. Customers do not run these Linux `ip maddress`
commands on the rack. They do still configure the upstream network to deliver
the group toward both rack uplinks, whether via PIM/IGMP, static multicast
routes and joins on the upstream routers, or an equivalent mechanism.

### Verifying the host path

`voxel network multicast check` reads all of the above back from the live
state, printing one line per item (host route, mirror filter, link-layer
membership) per group and `PASS`/`FAIL` overall. With no `--group`, it covers
everything this Falcon environment has plumbed thus far, using its `.falcon/`
state and the selected router's owned filters. A route belonging to another
environment is left alone. This is the same set a groupless `down` tears down.

If the router address is known but SSH cannot reach it, `check` and `down`
count or verify host routes only and warn that router state could not be
inspected. In `lan` mode, the address must first be read from the running
router over the falcon console. If that lookup fails, `check` reports an error
and live `down` stops after host-route cleanup instead of assuming that router
state is gone; a dry-run skips the router preview.

```
ok:      host route 232.100.0.1 -> 172.30.199.14
ok:      host route 239.100.0.1 -> 172.30.199.14
ok:      mirror of 232.100.0.1 on enp0s11 -> enp0s9 enp0s10
ok:      mirror of 239.100.0.1 on enp0s11 -> enp0s9 enp0s10
ok:      membership 01:00:5e:64:00:01 (232.100.0.1) on enp0s11
ok:      membership 01:00:5e:64:00:01 (239.100.0.1) on enp0s11
underlay: 232.100.0.1 -> ff04::e864:1 on switch0 (g0)
underlay: 239.100.0.1 -> ff04::ef64:1 on switch0 (g0)
underlay: 232.100.0.1 not programmed on switch1 (g3)
underlay: 239.100.0.1 not programmed on switch1 (g3)
check: PASS
```

*Note* that the two groups above share a membership address. [RFC 1112]'s
mapping keeps only the low 23 bits of the group address, so 32 groups alias
each Ethernet address and these two differ only in the discarded bits. The
aliasing is confined to the link layer on `cr1`, where one `ip maddress`
entry admits both. The underlay groups stay distinct (`ff04::e864:1` against
`ff04::ef64:1`), because that mapping embeds the full v4 address, so nothing
downstream of the switch conflates them. Teardown accounts for the aliasing
as well, dropping a membership only when no remaining group still maps to it.

Trailing `underlay:` lines connect that external path to the rack. Each
switch zone's `swadm multicast list` names the NAT target an external group
maps onto, the admin-scoped underlay group the switch replicates toward the
members. Those entries exist only once the group exists in the control plane
(a commtest run or an API join creates it), so `not programmed` on freshly
plumbed groups just means not yet, and the lines' output never affects the
check's result.

### Sending traffic

The source address the members must permit, `$SRC` above, is the host's address
on the external segment: `[external].host_ip` in isolated mode (default
`172.30.199.199`), or the host's LAN address otherwise.

`voxel commtest` drives the whole thing, creating the pools, project, and probes
before pinging each group and asserting every member replies within tolerance.
`--mcast-deny-group` adds the negative case from the join forms above: the
member's source list excludes the host, so the run asserts nothing arrives.
Every group needs the host-side setup first, deny groups included. No delivery
is also what a missing route or mirror produces, meaning that a deny-only run
would otherwise pass without ever reaching the dataplane, and voxel's
preflight refuses to start until the pieces are in place. Run
`voxel network multicast up` over the full set, or pass `--setup-mcast` to
have commtest do it:

```sh
voxel network multicast up --group 239.100.0.1 --group 239.100.0.2 \
    --group 232.100.0.1 --group 239.100.0.9

voxel commtest --source /oxide/workspace/omicron --traffic multi -- run \
    --test-duration 200s --warmup 10s --packet-rate 10 \
    --mcast-group 239.100.0.1 \
    --mcast-group 239.100.0.2@172.30.199.199 \
    --mcast-group 232.100.0.1@172.30.199.199 \
    --mcast-deny-group 239.100.0.9@172.30.199.198
```

The deny source is any address other than the host's, here `172.30.199.198`,
so the joined filter excludes the actual sender and the dataplane must drop
the traffic.

With no `--mcast-group`, voxel supplies `239.1.1.1`, which then needs its own
host route, mirror filter, and link-layer membership. See the commtest
section of the [README] for the build and privilege details.

Against probes created manually, `ping` is enough. illumos `ping -s` to a group
address prints a reply line per responder, so every probe address from the join
above should answer. `-t` raises the multicast TTL past the default of 1, which
otherwise expires the request before it clears `cr1` (see the TTL caveat
below):

```sh
for group in 239.100.0.1 239.100.0.2 232.100.0.1; do
    echo "== $group =="
    pfexec ping -s -t 16 "$group" 56 10
done
```

### Teardown

Run `voxel network multicast down` before `voxel destroy`, while the rack is
reachable. The host routes outlive `voxel destroy` and otherwise have to be
removed explicitly. Voxel keeps a per-environment host-route record under
`.falcon/`, while router-state discovery remains tied to the selected router
VM. Destroy leaves multicast state alone because router-state cleanup requires
that explicit target. The mirror and memberships normally go with `cr1`, since
they are runtime state inside the router. `voxel network multicast down` covers
all of the host-side state and takes the same repeated `--group` as `up`:

```sh
voxel network multicast down \
    --group 239.100.0.1 \
    --group 239.100.0.2 \
    --group 232.100.0.1
```

A `commtest` run leaves its `classone` project and the IP pools in place.
The separate cleanup pass (`voxel commtest -- cleanup`) deletes the project
but not the pools. Manually created objects unwind in dependency order: probes,
then the project's default subnet and VPC (a project cannot be deleted while
it holds a VPC), then the project, then each multicast pool after unlinking
it from the silo. Groups need no step of their own, as Nexus reaps a group
once its last member leaves.

```sh
for probe in $(oxide api "/experimental/v1/probes?project=classone" | jq -r '.items[].name'); do
    oxide api "/experimental/v1/probes/$probe?project=classone" --method DELETE
    # typed: oxide experimental system probe delete \
    #     --probe "$probe" --project classone
done

oxide api "/v1/vpc-subnets/default?project=classone&vpc=default" --method DELETE
oxide api "/v1/vpcs/default?project=classone" --method DELETE
oxide api /v1/projects/classone --method DELETE

for pool in mcast-v4-asm mcast-v4-ssm; do
    oxide api "/v1/system/ip-pools/$pool/silos/recovery" --method DELETE
    oxide api "/v1/system/ip-pools/$pool" --method DELETE
done
```

## Static multicast assignment and delivery without PIM or IGMP

The `cr1` arrangement above is voxel's answer to a question customer networks
face too: how does externally sourced multicast reach the rack when nothing on
the path runs PIM and no receiver sends IGMP reports? Per [RFD 488], v1 uses
static, API-driven assignment: the control plane programs the rack's multicast
state, but the rack neither signals membership upstream nor learns it from
guests. [RFD 488] proposes one addition per direction, each toggleable per
availability zone and usable alone or together. IGMP host-proxying has the rack
advertise its membership upstream per [RFC 4605]. Dynamic group identification
has the rack snoop and query guest IGMP or MLD, deriving membership from
reports rather than from the API. The external NAT entry accepts a group's
traffic at whichever uplink delivers it. Getting the traffic to an uplink is the
customer network's job, and every mechanism voxel uses has a static production
equivalent. [RFD 488] also notes that a network with pre-configured static
multicast routes to the rack needs neither of those additions.

**Across a flat L2 segment.** A switch with no IGMP snooping floods multicast
out every port, which delivers without any configuration. A snooping switch
constrains flooding to reported ports, but [RFC 4541] section 2.1.2 requires
unregistered groups be forwarded toward router ports and permits forwarding
on all ports, so vendor defaults often still deliver. To pin it down
deterministically, most switches accept static snooping entries binding a group
address to the rack-facing ports, the same shape as voxel's static membership
plus mirror.

**Across a routed hop.** A router between source and rack needs forwarding
state it would normally learn from PIM or IGMP. Two static substitutes:

- A static group membership on the rack-facing interface (Cisco's
  `ip igmp static-group`, with counterparts on most vendors). The router then
  forwards the group out that interface as if a receiver had joined there.
- A static `(S, G)` forwarding-cache entry. On a Linux router, [smcroute]
  manages exactly this, writing static multicast routes into the kernel's
  multicast forwarding cache (MFC) with no signaling protocol at all, the
  routed analogue of voxel's tc mirror.

**Caveats that apply to any static setup.**

- Static delivery takes the receiver out of the loop because the state exists
  whether or not anyone subscribes, so that any active source reaches the rack.
  Idle groups cost nothing, but a live one keeps flowing until the operator
  removes the configuration, since no leave or prune ever fires, and a
  `(*, G)` entry takes every source sending to that group.
- The sender's TTL must clear every routed hop. [RFC 1112] specifies a default
  TTL of 1 for multicast, so a source that works on its own segment silently
  dies at the first router until the application raises it.
- Deliver to **both** rack uplinks. The rack elects which switch holds a
  group's external NAT entry, and the election can move. The non-elected
  switch drops its copy in the dataplane, so duplicating toward both costs
  only the second uplink's bandwidth and is correct, exactly as the `cr1`
  mirror does. The bandwidth-saving alternative is dynamic signaling: with
  [RFD 488]'s proposed IGMP host-proxying ([RFC 4605]), the rack would
  advertise its membership upstream and traffic would follow the election
  instead of being duplicated.

What voxel does not model is the dynamic case: a customer network where PIM
and IGMP are running end to end, with the upstream building its distribution
tree from receiver membership. That is the direction [RFD 488]'s IGMP
host-proxying (future work) targets, and nothing here exercises or validates
that signaling.

> TODO: when [RFD 488]'s host-proxying lands, exercising it here means
> replacing the static scaffolding for that mode. The emulated upstream would
> have to listen rather than be pinned: an IGMP querier on the external
> segment and forwarding driven by the rack's proxied reports (a snooping
> bridge or [smcroute] on `cr1`), in place of a membership and mirror that
> deliver whether or not anyone signals.

## Troubleshooting

| Symptom | Where to look |
| --- | --- |
| Group absent from `GET /v1/multicast-groups` | The join never resolved a pool. Confirm a multicast pool is linked to the silo and its range covers the address. |
| Group stuck in "Creating", or members in "Joining" | `omdb nexus background-tasks show multicast_reconciler`, then activate it. |
| "Active" group, "Joined" members, no replies | Underlay or ingress. Check `omdb nexus multicast ddm-peers --mcast`, then the host route and `tc filter show` on `cr1`. |
| One group delivers, another does not | A per-group artifact: its host route, its `tc` filter, or its membership. |
| Frames reach `cr1`'s host-facing NIC (tcpdump sees them) but the mirror counters stay zero | The group's link-layer membership is absent, so the NIC drops the frames before the tc hook. `voxel network multicast check` reports it; `ip maddress show dev enp0s11` should hold the group's `01:00:5e` address. Note that tcpdump masks this by putting the NIC in promiscuous mode. |
| SSM group silent, ASM groups fine | The join's `source_ips` must contain the sending host address. |
| Some members reply, others do not | Per-sled. `omdb db multicast members --group-ip <ip>` shows which sled each member landed on. |
| `up` reports a host route "is not recorded for Falcon environment" | The route predates this environment's `.falcon/` record, either from another environment or from a record that was deleted while its routes remained. Voxel will not displace it. Remove it with `pfexec route delete -host <group> <gateway>` if it is stale. |
| Every per-group artifact checks out and delivery still fails | The sled dataplane. Run opte's [`opte-mcast-delivery.d`] in a sled's global zone, where `xde` is loaded. `NOFWD` names a missing forwarding entry, `FILTERED` a source-filter drop, and the delivery matrix reports which ports took a copy. The script is not in the sled image, so copy it from an opte checkout. |

*TODO*: IPv6 multicast is not wired up in `commtest` yet, and the isolated
external segment voxel creates is v4-only. The API objects are not the gap: a
`v6` multicast pool and its groups work like their v4 counterparts. `lan` mode
also inherits whatever v6 the LAN carries, so what is missing is voxel's
wiring, not the rack or the topology. Until then, v6 groups are out of reach
from this host-sourced path.

[#1093]: https://github.com/oxidecomputer/propolis/pull/1093
[#1200]: https://github.com/oxidecomputer/propolis/pull/1200
[#1206]: https://github.com/oxidecomputer/propolis/pull/1206
[omicron#11128]: https://github.com/oxidecomputer/omicron/pull/11128
[omicron#11118]: https://github.com/oxidecomputer/omicron/pull/11118
[omicron#10520]: https://github.com/oxidecomputer/omicron/pull/10520
[omicron#10346]: https://github.com/oxidecomputer/omicron/pull/10346
[omicron#10070]: https://github.com/oxidecomputer/omicron/pull/10070
[omicron#9912]: https://github.com/oxidecomputer/omicron/pull/9912
[`helios-build onu`]: https://github.com/oxidecomputer/helios#installing-locally-on-your-build-machine
[helios]: https://github.com/oxidecomputer/helios
[775]: https://code.oxide.computer/c/illumos-gate/+/775
[`multicast.rs`]: ../voxel/src/multicast.rs
[`commtest.rs`]: ../voxel/src/commtest.rs
[`opte-mcast-delivery.d`]: https://github.com/oxidecomputer/opte/blob/master/dtrace/opte-mcast-delivery.d
[README]: ../README.md
[README's Privileges section]: ../README.md#privileges
[RFC 1112]: https://datatracker.ietf.org/doc/html/rfc1112
[RFC 3376]: https://datatracker.ietf.org/doc/html/rfc3376
[RFC 3810]: https://datatracker.ietf.org/doc/html/rfc3810
[RFC 4364]: https://datatracker.ietf.org/doc/html/rfc4364
[RFC 4541]: https://datatracker.ietf.org/doc/html/rfc4541
[RFC 4605]: https://datatracker.ietf.org/doc/html/rfc4605
[RFC 4607]: https://datatracker.ietf.org/doc/html/rfc4607
[RFC 7042]: https://datatracker.ietf.org/doc/html/rfc7042
[RFD 488]: https://rfd.shared.oxide.computer/rfd/0488
[smcroute]: https://github.com/troglobit/smcroute
[tc-mirred]: https://man7.org/linux/man-pages/man8/tc-mirred.8.html
[multicast joiner]: https://github.com/oxidecomputer/thundermuffin/pull/14
