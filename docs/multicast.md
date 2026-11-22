# Multicast on a voxel rack

Multicast integration on an Oxide rack uses three API objects: an IP pool,
a group, and group members. The operator creates the pool. Nexus implicitly
creates a group when the first member joins an address covered by the pool and
removes it after the last member leaves.

Each external multicast group maps to an IPv6 multicast address in
our internal, admin-scoped `ff04::/64` block. Switches use this address to
replicate traffic to the member sleds.

Traffic here originates on the **host**. `commtest` tests the
external-to-underlay ingress path, moving packets from host through switch to
sled for each subscribing member. This document does not cover the
guest-sourced egress path (or OPTE's sled-side nexthop selection). A probe's
echo reply is always unicast.

A guest VM may emit an IGMPv3 or MLDv2 membership report while joining a group.
The report does not cause forwarding:

1. A report goes to the protocol's link-local address (`224.0.0.22`,
   `ff02::16`) and never to the group that's being joined.
2. OPTE only encapsulates destinations in its [multicast-to-physical table],
   which holds the admin-scoped underlay addresses of the materialized groups.
3. OPTE drops the report before sled-side nexthop selection.

Forwarding follows the API subscription alone. [RFD 488]'s dynamic group
identification (IGMP snooping and querying) proposes a system for handling
these reports, but this document does not cover that mode.

`voxel commtest --traffic multi` sets up the pools, project, probes, and
probe memberships used by a run. The API sections document those objects and
show instance membership by hand.

## tl;dr: Up and running

To run commtest (aka some load) from a fresh start, with the pinned images
already baked-in, follow these steps:

```sh
# launch and wait for RSS (--init-rss: commtest's client does not handle
# the commission path's self-signed HTTPS at the moment)
pfexec voxel launch --init-rss

# host path in: routes at ce, passive PIM + (S,G) mroutes on cr*, dohwcksum=0
voxel network multicast up --group 239.100.0.1 --group 239.100.0.2 \
    --group 232.100.0.1 --group 239.100.0.9
voxel network multicast check

# pools, project, probes, memberships, then traffic (isolated-mode sources)
# 60s warmup; freshly created probes can take > 30s to start receiving traffic
voxel commtest --source /oxide/workspace/omicron --traffic multi -- run \
    --test-duration 200s --warmup 60s --packet-rate 10 \
    --mcast-group 239.100.0.1 \
    --mcast-group 239.100.0.2@172.30.199.199 \
    --mcast-group 232.100.0.1@172.30.199.199 \
    --mcast-deny-group 239.100.0.9@172.30.199.198

# persist the transcript (cleanup overwrites .falcon/commtest.log)
cp ~/.config/voxel/.falcon/commtest.log \
    ~/.config/voxel/commtest-$(date +%Y%m%d-%H%M).log

# undo (destroy also purges the host routes, so running down is optional)
voxel commtest -- cleanup
voxel network multicast down
```

Run the `up` command following every launch, since the mroutes live in the
router VMs. Without `--group`, `up` and commtest both use `239.1.1.1` by
default. In `lan` mode, use the host's address on the external link for the
sources; see [Sending traffic](#sending-traffic).

The rest of this document is long form: what each step installs, the API
calls if done by hand, and how to read back state information in the rack.

## Prerequisites

- A launched rack with RSS already complete and the external API answering.
- For `commtest`, launch the rack with `--init-rss`. The default,
  commission-driven launch is HTTPS-only, using a self-signed certificate
  that commtest's HTTP client cannot validate.
- See [parameters.md](parameters.md) for the `[network.multicast]` settings
  in `voxel.toml`. The default delivery path is the first forwarding router
  toward `switch0`.
- A Helios build with the updated **host viona kernel module** containing the
  MAC-filter table updates from stlouis#986, Gerrit change [775] on
  illumos-gate (merged into `stlouis` at commit `5ffff4b8`). Check the
  loaded host module with
  `pfexec mdb -ke 'viona_ioc_set_mac_filters::nm'`. The output must include
  `viona_ioc_set_mac_filters`.
- The host-side setup described in more detail below (routes, PIM, group
  assignments). `voxel network multicast up` installs this setup for whichever
  `[external]` mode is set; `voxel commtest` won't run the multicast phase(s)
  without this first command. Static addressing (both in isolated mode and
  for a static `lan` segment) resolves every fabric router and the `ce` from
  configuration directly. A DHCP `lan` setup requires Falcon console lookup
  over the running nodes. All of this plumbing lives in [`multicast.rs`];
  the run wrapper that invokes this setup lives in [`commtest.rs`].
- A control-plane image whose Omicron version supports multicast. If `voxel commtest
  --traffic multi` detects an older, unicast-only commit, the run will fail
  before building. This detection inspects the `--source` checkout,
  not the commit baked into the running rack's image.
- The commtest privilege setup documented in [README's Privileges section]; a
  run needs `net_icmpaccess` in the effective set and refuses uid 0.

### TODO: Dependencies

`Cargo.toml` pins Voxel's Omicron dependencies to [`dce8d982`]. A default
`voxel image create` builds that revision. The packaged service revisions below
come from that Omicron commit's `package-manifest.toml`; `sidecar-lite` is
selected by Voxel's `pins.toml`.

Keep Maghemite and Dendrite compatible with Omicron's OPTE revision: `ddmd`
programs OPTE's boundary table through the xde ioctl API.

| Repository | Revision | PR |
| --- | --- | --- |
| `propolis` | [`7c298670`] | [#1262](https://github.com/oxidecomputer/propolis/pull/1262) |
| `omicron` | [`dce8d982`] | [`zl/mcast-build`](https://github.com/oxidecomputer/omicron/tree/zl/mcast-build) |
| `maghemite` | [`c02f1674`](https://github.com/oxidecomputer/maghemite/commit/c02f16749b8d29ff749fa745d4118577cddc44c6) | |
| `dendrite` | [`28789d60`](https://github.com/oxidecomputer/dendrite/commit/28789d607071cc52a60304f5e6bce52fec3d5597) | |
| `sidecar-lite` | [`4d57f3a1`](https://github.com/oxidecomputer/sidecar-lite/commit/4d57f3a1fac7e5ed2a63ab5bc54b9b18b7b2cf73) | |
| `opte` | [`ec15113a`](https://github.com/oxidecomputer/opte/commit/ec15113af6b8b8b04a25ffc30940abbdc0ea7f88) | [#1108](https://github.com/oxidecomputer/opte/pull/1108) |
| `propolis` (host) | [`7c298670`](https://github.com/oxidecomputer/propolis/commit/7c298670530922c53ae3f35cd853c0c82229d1ca) | [#1262](https://github.com/oxidecomputer/propolis/pull/1262) |

The `propolis` row is the server packaged into the control-plane image. The
host server for the rack VMs is selected separately with
`falcon.propolis_binary`. Voxel's own `propolis-client` follows libfalcon at
[`12b00ba8`]; the host server's instance-spec types must match that client.

For a local test run, build the control-plane image from the local Omicron
checkout like so:

```sh
pfexec voxel image create --src /oxide/workspace/omicron
```

## API walkthrough

The commands in this section run on different hosts and in different zones:

- `voxel`-specific commands run on the host that launches the rack.
- `dig` and `curl` commands can be run from `g0` or on another host with a
  route to the rack's service network.
- The `oxide` CLI runs from a host with the CLI installed and a route to
  the API.
- `omdb` can be run through the `oxz_switch` zone.

From the launch host, outside node `g0`, query the service addresses and
external DNS zone through the switch zone:

```sh
voxel tp exec -c "/opt/oxide/omdb/bin/omdb db network list-eips" switch0
voxel tp exec -c "/opt/oxide/omdb/bin/omdb db dns show" switch0
```

The EIP list shows the Nexus addresses; the DNS result gives the external
zone, labelled `rack1.oxide.test` below. Nexus uses `[network].service_pool`
addresses not allocated to external DNS. On an `--init-rss` rack, we should
probe for HTTP starting at `.22`.

Before probing the service pool, enter the `g0` node from the rack host:

```sh
voxel host login g0
```

```sh
for ip in $(seq 22 29); do
    curl -sf -m 2 -o /dev/null "http://198.51.100.$ip/v1/ping" \
        && echo "198.51.100.$ip"
done
```

Exit `g0` after the loop. Run the following `oxide` commands on a host with
the CLI installed and a route to Nexus.

Voxel probes the same service pool for `commtest`'s `--api`, trying external
DNS addresses last. It accepts any HTTP status line from `GET /v1/ping` on
port 80. The `curl -f` example above requires a 2xx response.

Authentication for these examples checks against the recovery silo.

Set `API` to a responding service-pool address. The default pool spans
`198.51.100.20` through `198.51.100.29`; `.23` below is an example for a
rack launched with `--init-rss`.

```sh
API=http://198.51.100.23
oxide auth login --host "$API"
oxide api /v1/multicast-groups | jq
```

Typed CLI:

```sh
oxide experimental multicast-group list
```

Commands for multicast groups, instance membership, and probes are currently
tagged as `experimental`, using the `oxide experimental <...>` prefix. The
`oxide.rs` build must be new enough to include the commands used below.

`source_ips` is passed in group join and member request bodies.
`multicast_groups` and `pool_selector` are passed in probe-create request
bodies. None of these fields has an individual flag; use
`--json-body <file>` instead.

For a commission-driven, HTTPS-connected `voxel` launch, use the recovery silo
hostname so that Nexus can select the right certificate. If the client cannot
resolve this hostname, map the hostname to a Nexus address with `--resolve`:

```sh
HOST=recovery.sys.rack1.oxide.test
NEXUS_IP=198.51.100.22
API=https://$HOST

curl -k -i --resolve "$HOST:443:$NEXUS_IP" "$API/v1/ping"
```

After that ping request returns an HTTP response, keep the `--resolve` and
`--insecure` options on the `oxide` commands:

```sh
oxide --insecure --resolve "$HOST:443:$NEXUS_IP" auth login --host "$API"
oxide --insecure --resolve "$HOST:443:$NEXUS_IP" api /v1/multicast-groups | jq
```

## IP pools

An IP pool has a `pool_type` discriminator, `unicast`
(the default) or `multicast`, and one IP version (`ip_version`, `v4` or `v6`).
The `assignment` field is either `silos` (the default) or `system_services`.
The rack's own `oxide-service-pool-v4` uses `system_services` from RSS onward.
Multicast pools are further constrained:

- Every range in the pool must be entirely Any-Source Multicast (ASM) or
  entirely Source-Specific Multicast (SSM), never both. SSM is `232.0.0.0/8`
  for IPv4 and the per-scope `ff3x::/32` blocks for IPv6 ([RFC 4607]), while
  everything else is ASM. Running an ASM group set and an SSM group set
  together means creating two pools. This split is only about address space;
  an ASM join can still filter on `source_ips` (see [Join forms](#join-forms)).
- A silo may hold at most one default pool per (pool type, IP version) pair,
  so four in all.
- dpd (and, *TODO*, upcoming Nexus work) admits the whole IPv4 multicast range
  apart from the base address of `224.0.0.0` ([RFC 1112] §4) and the SSM null
  address of `232.0.0.0` ([RFC 4607] §4.3). For IPv6, it admits every scope
  except the reserved `0x0`, interface-local `0x1`, and link-local `0x2`,
  which a router must not forward past ([RFC 4291] §2.7); `ff04::/64` stays
  reserved for the internal underlay API.

```sh
oxide api /v1/system/ip-pools --method POST --input - <<'JSON'
{ "name": "mcast-v4-asm", "description": "ASM multicast pool",
  "ip_version": "v4", "pool_type": "multicast" }
JSON

oxide api /v1/system/ip-pools/mcast-v4-asm/silos \
    --method POST --input - <<'JSON'
{ "silo": "recovery", "is_default": false }
JSON

oxide api /v1/system/ip-pools/mcast-v4-asm/ranges/add \
    --method POST --input - <<'JSON'
{ "first": "239.100.0.1", "last": "239.100.0.9" }
JSON
```

The same three calls have typed structures under
`oxide system networking ip-pool` (pools are not an experimental CLI command):

```sh
oxide system networking ip-pool create --name mcast-v4-asm \
    --description "ASM multicast pool" --ip-version v4 --pool-type multicast
oxide system networking ip-pool silo link --pool mcast-v4-asm --silo recovery \
    --is-default false
oxide system networking ip-pool range add --pool mcast-v4-asm \
    --first 239.100.0.1 --last 239.100.0.9
```

The range ends at `239.100.0.9` because the deny group used later allocates
from a pool just like any other group would.

The SSM pool, `mcast-v4-ssm`, takes the same three calls with `232.100.0.1` as
both ends of its range:

```sh
oxide system networking ip-pool create --name mcast-v4-ssm \
    --description "SSM multicast pool" --ip-version v4 --pool-type multicast
oxide system networking ip-pool silo link --pool mcast-v4-ssm --silo recovery \
    --is-default false
oxide system networking ip-pool range add --pool mcast-v4-ssm \
    --first 232.100.0.1 --last 232.100.0.1
```

Members also need an ordinary unicast pool for their external addresses.
`commtest` creates one named `default` over its `--ip-pool-begin/--ip-pool-end`
range and links it to the silo as the default. To create it manually:

```sh
oxide api /v1/system/ip-pools --method POST --input - <<'JSON'
{ "name": "default", "description": "unicast pool for member external IPs",
  "ip_version": "v4", "pool_type": "unicast" }
JSON

oxide api /v1/system/ip-pools/default/silos --method POST --input - <<'JSON'
{ "silo": "recovery", "is_default": true }
JSON

oxide api /v1/system/ip-pools/default/ranges/add \
    --method POST --input - <<'JSON'
{ "first": "198.51.100.30", "last": "198.51.100.45" }
JSON
```

Voxel derives the same range for `commtest`: one past the service pool,
sixteen addresses (`derive_pool` and `DEFAULT_POOL_SIZE` in [`commtest.rs`]).

Both IP pool list endpoints filter by type:

```sh
oxide system networking ip-pool list --pool-type multicast
oxide ip-pool list --pool-type multicast   # silo-scoped view
```

Only the system list filters on `assignment`. The silo-scoped list takes
only the `pool_type` and `ip_version` fields. A silo can never see a service
pool:

```sh
oxide api "/v1/system/ip-pools?assignment=system_services" | jq
```

## Project

Members live in projects. `commtest` creates `classone` by default. Manually,
we can run:

```sh
oxide api /v1/projects --method POST --input - <<'JSON'
{ "name": "classone", "description": "multicast walkthrough" }
JSON
```

## Groups

Groups are not created explicitly (i.e., `POST /v1/multicast-groups` does not
exist). Creation is part of an implicit lifecycle: the first member creates
the group, and Nexus reaps it after the last member leaves. An explicit
address selects the linked multicast pool covering it, whereas a new name
allocates from a linked default multicast pool. The `multicast_reconciler`
[RPW][rpw] (reliable persistent workflow) drives these transitions and the
associated switch programming. In `(S,G)`, S is the sender and G is the group
(source-specific). `(*,G)` accepts any sender (a source wildcard).

```
  ip pool   pool_type = multicast, one ip_version, ASM xor SSM
  mcast-v4-asm : 239.100.0.1 - 239.100.0.9
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
    Deleting --[multicast_reconciler]--> gone (time_deleted field set)
```

The read side is `GET /v1/multicast-groups`, `/v1/multicast-groups/{group}`,
and `/v1/multicast-groups/{group}/members`, where `{group}` is a name, a UUID,
or the multicast IP. The group list returns an empty page until the first join,
which the [Instances](#instances) and [Probes](#probes) sections cover below.
Looking up a nonexistent group returns a `404`.

The group view returns a single object; the group and member lists are
paged (returning `items` and a `next_page` token in the response; `limit` and
`page_token` on the request). The group list has no source filters.

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

A group view contains `multicast_ip`, `ip_pool_id`, `state`, the union
of the explicit source lists supplied by its members (`source_ips`), and
`has_any_source_member`. `has_any_source_member` is `true` when
at least one member has no explicit source list (a wildcard).

The OpenAPI schema types `state` as a free-form string rather than an
enumerated type. Its values can be one of "Creating", "Active", and "Deleting".

### Join forms

Every join event takes the same body:

- an optional `source_ips` list that describes filtering.
- an optional `ip_version` that disambiguates creation by name when both IPv4
  and IPv6 default multicast pools are linked.

The forms differ in what the source list means:

- **ASM**, e.g. `239.100.0.1` with no sources. An any-source `(*, G)` join.
- **SSM**, e.g. `232.100.0.1` with `source_ips`. Nexus will reject an SSM
  join without a source list.
- **Source-bound ASM**, e.g. `239.100.0.2` with a set of `source_ips`. This is
  an `(S,G)` join on an ASM group, using [RFC 3376] source filtering.

Nexus defines a source list maximum of 32 entries
(`MAX_SOURCE_IPS_PER_MEMBER`), and the union across a group's members is capped
at 256 (`MAX_SOURCE_IPS_PER_GROUP`). Neither of these caps comes from the
protocol itself. IGMPv3 ([RFC 3376]) and MLDv2 ([RFC 3810]) leave per-group
source-list sizing to implementation details.

For any source-filtered join event, the list must include whatever sends the
verification traffic; otherwise, the dataplane will drop it. This is also the
recipe for the negative deny case, `commtest --mcast-deny-group GROUP@SRC`,
which supplies a source that's not the actual sender and asserts that no packet
arrives at the probe. The examples below set the `SRC` to the isolated-mode
default, i.e., `172.30.199.199`. For another setup, use the address described
under [Sending traffic](#sending-traffic).

### Instances

An instance joins and leaves by group identifier and can list its own
memberships (subscribers). A join returns a `201` with the new member, while
a leave responds with a `204`. The first join implicitly creates the group
unless the identifier is a UUID, which must name an already existing group.

These examples use an instance named `myvm`. A minimal instance requires a
name, description, hostname, memory, and vCPU count:

```sh
oxide api "/v1/instances?project=classone" --method POST --input - <<'JSON'
{ "name": "myvm", "description": "multicast member", "hostname": "myvm",
  "memory": 2147483648, "ncpus": 2, "start": false }
JSON
```

At this point the instance is still stopped, which is enough to exercise the
membership API. A stopped member has no sled associated with it; therefore,
no traffic can reach it. Delivery is observed only through the probes.

```sh
SRC=172.30.199.199

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
while `--json-body` takes a file path (`/dev/stdin` works too):

```sh
SRC=172.30.199.199

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

A probe runs in a zone on its assigned sled, without a guest VM. It answers
echo requests sent to its joined groups, so a normal `ping` can show delivery.
Memberships are fixed at creation. To change them, recreate the probe.

```sh
SRC=172.30.199.199
sleds=$(oxide api /v1/system/hardware/sleds | jq -r '.items[].id')

i=0
for sled in $sleds; do
    oxide api "/experimental/v1/probes?project=classone" \
        --method POST --input - <<JSON
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

Ping replies come from the probes' external addresses. In the member list
(`/v1/multicast-groups/{group}/members`) `kind` is `instance` or `probe` and
describes what the `parent_id` refers to.

## Control-plane verification

From the rack host, read multicast state with [omdb][omdb] inside `switch0`:

```sh
voxel tp login switch0

export PATH=$PATH:/opt/oxide/omdb/bin

omdb db multicast pools
omdb db multicast groups
omdb db multicast members
omdb db multicast info --ip 239.100.0.1
```

`db multicast groups` reports each group's `STATE` ("Creating", "Active", or
"Deleting"; a deleted group is not part of the listing), its `UNDERLAY_IP`,
the source allowlist, and a `MEMBERS` column of `name@sled`
(`probe:name@sled` for probes).

`db multicast members` adds per-member state ("Joining", "Joined",
"Left") and the assigned sled, and filters on `--group-ip`, `--state`,
`--sled-id`, and `--source-ip`.

Underlay replication is built from the multicast routes DDM exchanges. If a
group is "Active" with "Joined" members but receives no traffic, we can
check the underlay and external ingress path(s):

```sh
omdb nexus multicast ddm-peers --mcast
```

`--mcast` limits the listing to multicast underlay members. Each listed
rear-port interface should have a DDM session in `Exchange`.

The [RPW][rpw] also exposes its own state:

```sh
omdb nexus background-tasks doc
omdb nexus background-tasks show multicast_reconciler
omdb nexus background-tasks print-report multicast_reconciler
```

Its report counts groups created ("Creating" to "Active"), groups deleted,
groups verified on the switches, members processed and deleted, and empty
groups marked for deletion/reaping.

Exit the switch zone before continuing with the host commands below.

## Topology management

The setup below routes traffic from the host into the rack.

`voxel network multicast up` installs the components documented below, and it
must be run after `voxel launch`. The command resolves the `ce` address and
reaches the selected router VMs over SSH, all of which need running nodes.
`up` should be run again after every launch, as each relaunch:

- removes router VM PIM and mroute state.
- keeps the existing host routes.

Any re-run of `up` will replace the route gateway.

### Host routes

The routers speak unicast BGP through FRR and run `pimd` with every relevant
interface passive (no PIM adjacencies). The host needs an explicit route per
group address pointed at the customer edge (`ce`). This applies to SSM groups
as well: the host route uses the group address. The source is specified in
both the join event and the static `(S,G)` mroute.

`ce`'s address maps to `[topology].ce_external_ip` when set. Otherwise,
static external addressing supplies the address, with voxel numbering
node addresses deterministically from `[external].ip_start`: first by sled and
then by routers in `[topology].routers` order. The stock four-sled
`ce, cr1, cr2` topology places `ce` at `172.30.199.14`. DHCP `lan` mode
is the only mode that reads a lease from a running node. To look up that
address manually, run `voxel host login ce` and
`ip -4 -br addr show scope global`. Exit `ce` after reading the address to
return to the rack host.

The `voxel network multicast up` command installs these routes. The commands
below document the manual version. A stale route to a dead customer
edge drops the group's traffic. It's best to delete it before adding it again:

```sh
CE=172.30.199.14

for group in 239.100.0.1 239.100.0.2 232.100.0.1; do
    pfexec route delete -host "$group" 2>/dev/null || true
    pfexec route add -host "$group" "$CE"
done
```

The Helios host's route table is shared across Falcon environments. Voxel
records each group's gateway in
`.falcon/multicast-<hex environment name>.json` to track the routes installed
by this environment.

### Static router assignments

A host route through `ce` selects the external segment. Every fabric router
has an external NIC on that segment and receives the multicast frames there.
Voxel installs `(S,G)` state on each `cr*` to forward them over the configured
rack-facing links.

In v1 of [RFD 488], group membership is API-driven only, i.e., no IGMP or PIM
requests sent toward the customer network. Host proxying is left to a later
stage there. The static assignment setup remains the source of all forwarding
state, with the control plane programming the rack and DDM propagating
replication state through to dpd and the switch(es). A legit customer network in
front of a real rack would set similar state on its own gear. The rack's
external NAT entry maps the external group address to its IPv6 underlay
address. Both switches have this entry, so traffic can enter at either
uplink. The customer network must route it there.

Static forwarding into the rack requires:

- `pimd` lets FRR hold the static `(S,G)` entries. Every interface in this path
  is passive, meaning no PIM adjacencies; voxel **does not** model customer
  PIM policy. `pimd` decides ASM vs SSM shapes by group address (a source on the
  `(S,G)` entry does not count here). Therefore, voxel declares `224.0.0.0/4`
  source-specific except for `224.0.0.0/24`, which must remain any-source for
  local control traffic. The broad range is an emulation byproduct for voxel
  only. The `up` command reapplies the prefix list because a missing list
  turns every group into an ASM one.

- Every interface in the static multicast tree needs an IPv4 address. The
  external interfaces already have one. In BGP mode, the fabric-to-rack links
  are unnumbered ([RFC 5549], IPv6 link-local only, with
  IPv4 attached to the v6 session). PIM cannot build a virtual interface (VIF)
  without an IPv4 address. Voxel assigns each unnumbered PIM link a `/32` from
  `192.0.2.0/24` ([RFC 5737] TEST-NET-1). Nothing else routes to those
  addresses.

  A `/32` is used because PIM only needs a local address. Anything wider
  (e.g., `/30`, `/31`) puts a connected IPv4 route on the interface carrying
  unnumbered eBGP, and from there it can get redistributed or collide with a
  learned route.

  *Note*: `ip mroute` on an unnumbered interface can fail an assertion inside
  pimd (`pim_oil.c`, `mroute_vif_index >= 0`) and take the daemon down. Put the
  address on first.

- The static assignment uses the host's source address, regardless of the
  source given beside the group in `--mcast-group GROUP@SRC`. In
  commtest's deny-group case, that source is not on the wire at all, and an
  mroute keyed on it would let the deny test pass without ever reaching the
  dataplane. Because the sender is known, `(S,G)` needs no rendezvous point or
  shared multicast tree.

In a customer network, each router decides which rack-facing links
can forward a group's traffic. `[network.multicast] delivery` is the default
for a group that is not steered; `--steer` selects complete paths per group.

Falcon's link order determines router NIC names. In the stock single-rack,
four-sled topology, `cr1` has:

- `enp0s8`, toward `ce`
- `enp0s9`, toward `g0` (switch0)
- `enp0s10`, toward `g3` (switch1)
- `enp0s11`, the external interface used to reach `cr1` and the multicast
  incoming interface

A launched rack starts up with the PIM VIFs and `/32` addresses already
configured in `frr.conf`. The `voxel network multicast up` command installs
their per-group static assignments because the multicast groups are not known
at the time of launch. It reapplies the VIF commands before installing the
mroutes.

### Steering a group to one switch

Which switches receive a group is the emulated "customer" network's decision,
outside any rack policy. Voxel takes the steering direction from the `--steer`
flag. The rack itself programs every group on both switches regardless, as
steering only decides which switch the traffic arrives through.

Running `voxel network multicast up --group 239.100.0.1 --steer
239.100.0.1=switch0`, for example, selects the `switch0` path on every
forwarding router. Before applying a selection, voxel withdraws each recorded
source's `(S,G)` entry from every outgoing interface on the fabric routers.
For each router, it sends those withdrawals and the selected additions within
a single `vtysh` session. FRR treats absent entries as no-ops.

A selection can give a router at most one outgoing path. FRR 10.3 withdraws a
static mroute by `(S,G)` and ignores the interface in `no ip mroute`. With
two paths on one router, a narrowing would remove the wrong one. `up` rejects
such a selection before touching the routers.

The commands below run inside `cr1` for a static LAN whose sender is
`192.168.1.199`. From the rack host, enter `cr1` with `voxel host login cr1`.
`up` derives the sender and also updates the other fabric routers.
The blanket `no` commands are safe:

```sh
SENDER=192.168.1.199
vtysh -c 'configure terminal' \
    -c 'interface enp0s11' \
    -c "no ip mroute enp0s9 239.100.0.1 $SENDER" \
    -c "no ip mroute enp0s10 239.100.0.1 $SENDER" \
    -c 'exit' \
    -c 'interface enp0s11' \
    -c "ip mroute enp0s9 239.100.0.1 $SENDER" -c 'exit'
```

Exit `cr1` to return to the rack host.

A group without recorded state or an explicit `--steer` uses the configured
`delivery`. `single`, the default, selects the first forwarding router's path
to `switch0`. Voxel materializes and records both default and explicit
selections. A later `up` reuses the recorded paths unless `--steer` replaces
them.

A selection is `all`, `none`, or a comma-separated list of paths. Paths accept
switch numbers, LLDP labels, and router interface names:

| Form | Selects |
|------|---------|
| `switch0` | switch slot 0 on every forwarding router |
| `uplink0` | the switch slot with this `[network.uplinks].lldp_port_description` |
| `uplink0-cr1` | one path selected by the emitted LLDP label |
| `enp0s9` | that interface on every forwarding router that has it |
| `cr1:switch0`, `cr1:uplink0`, `cr1:enp0s9` | one router and one switch |
| `all` | every router-switch pair; rejected once a rack has more than one switch |
| `none` | an explicit empty selection |

`switch<N>` is how an operator should reason about traffic steering; `check`
prints the interface names that a selection actually resolves to. LLDP labels
come from the "customer-side" link configuration. Voxel adds the router suffix
(see `uplink_ports` in `config.rs`), and `uplink0-cr1` selects a complete path.
The `--steer` flag maps an LLDP description to a single switch slot. The
description can be reused for that same slot in other racks, but assigning
it to a different slot results in an error.

For example, `switch0` selects one path per forwarding router: `cr1:switch0`
pins a single path toward the rack, and `cr1:switch0,cr2:switch1` reaches both
switches of a two-router, two-switch rack (2x2) with one path per router.

`voxel network multicast check` confirms the host route, passive PIM
interfaces, and the current `(S,G)` entries on each fabric router. Each
fabric-router entry selects its rack-facing links.

### The host must not offload checksums

If the host offloads the IPv4 header checksum, a multicast packet looped back
to the `cr1` router VM can have a zero checksum. Linux drops it at IP input
before forwarding. SoftNPU accepts the same packet, but the sidecar parser
will drop it. With offload disabled, the host fills in the checksum before the
packet reaches either path.

`voxel network multicast up` applies this setting before plumbing and persists
it in `/etc/system.d/voxel`. For reference, it runs:

```sh
echo 'dohwcksum/W 0' | pfexec mdb -kw
```

The persistent entry is:

```text
set ip:dohwcksum = 0
```

### Verifying the host path

`voxel network multicast check` reads the host route and PIM state from the
running system, comparing each recorded `(S,G)` assignment with the
router's forwarding cache. It prints one line per item per group and returns
`PASS`/`FAIL` overall. With no `--group` flag provided, the
command validates and prints out all groups recorded by the current Falcon
environment (state stored in `.falcon/`).

Static addressing, including a static `lan`, retrieves router addresses
from configuration. DHCP `lan` reads them from the running routers over
the Falcon console. If address resolution fails, `check` reports the error;
`down` stops right after host-route cleanup, whereas a `down --dry-run`
disregards unresolved routers and prints cleanup for the resolved ones.

The checksum setting and host routes are printed first. A
`host source for GROUP` line then shows the address selected by the host route
and checks it against the recorded source. The router checks follow from this,
displaying one PIM line per router and an mroute line per group.
An mroute line compares the live
`(S,G)` entry against the expected incoming interface and outgoing links. What
follows the `->` symbol is the expected outgoing set, and `none` means the
router should have no entry. Failures are prefixed with `MISSING:`
and the run exits non-zero. For a static `lan` rack with CE `192.168.1.54` and
sending host `192.168.1.199`, the output includes:

```
ok:      host checksum offload disabled
ok:      host route 239.100.0.1 -> 192.168.1.54
ok:      host route 239.100.0.2 -> 192.168.1.54
ok:      host source for 239.100.0.1 -> 192.168.1.199
ok:      host source for 239.100.0.2 -> 192.168.1.199
ok:      pim on cr1:enp0s11
ok:      mroute on cr1:239.100.0.1 -> enp0s9
ok:      mroute on cr1:239.100.0.2 -> enp0s9
ok:      pim on cr2:enp0s11
ok:      mroute on cr2:239.100.0.1 -> none
ok:      mroute on cr2:239.100.0.2 -> none
check: PASS
```

### Sending traffic

`SRC` must be the host's own address on the external segment. In isolated mode,
this is `[external].host_ip`. In either static or DHCP `lan` mode, that setting
is the nodes' gateway and may differ from the sender. Use the address printed
by `voxel network multicast check` as `host source for GROUP`.

`voxel commtest` configures its API resources before sending traffic. It reuses
existing pools and the `classone` project. Existing probes are reused when
they have the requested memberships; otherwise they are recreated.

*Note*: all members of a non-deny group must return at least one response.
`--icmp-loss-tolerance` limits lost ICMP packets per member per group (Voxel's
default is `500`). A membership subscriber must not return more replies than
the number of requests sent by the host. `commtest` temporarily assumes
single-copy delivery (one request, at most one reply per member,
singly-homed). A group with traffic steered into both switches
(`cr1:switch0,cr2:switch1`) fails the run with duplicate replies, even when
both paths deliver correctly.

`--mcast-deny-group` covers the negative case from [Join forms](#join-forms):
the source list excludes the host, and the run expects no packets. Deny groups
still need the host routing setup.

Run `voxel network multicast up` over the full set:

```sh
voxel network multicast up --group 239.100.0.1 --group 239.100.0.2 \
    --group 232.100.0.1 --group 239.100.0.9

voxel commtest --source /oxide/workspace/omicron --traffic multi -- run \
    --test-duration 200s --warmup 60s --packet-rate 10 \
    --mcast-group 239.100.0.1 \
    --mcast-group 239.100.0.2@172.30.199.199 \
    --mcast-group 232.100.0.1@172.30.199.199 \
    --mcast-deny-group 239.100.0.9@172.30.199.198
```

The deny source should be any address that's not the host's. This example uses
`172.30.199.198`.

In `lan` mode, substitute the sending host's external address for the allowed
sources. This example uses `192.168.1.199` on that link and `192.168.1.198`
for the deny source:

```sh
voxel network multicast up --group 239.100.0.1 --group 239.100.0.2 \
    --group 232.100.0.1 --group 239.100.0.9

voxel commtest --source /oxide/workspace/omicron --traffic multi -- run \
    --test-duration 200s --warmup 60s --packet-rate 10 \
    --mcast-group 239.100.0.1 \
    --mcast-group 239.100.0.2@192.168.1.199 \
    --mcast-group 232.100.0.1@192.168.1.199 \
    --mcast-deny-group 239.100.0.9@192.168.1.198
```

The group addresses come from rack-side pools and remain the same in either
networking mode; only the sources change.

After the last group drains, commtest writes
`multicast-connectivity-report.json` to the working directory (see the README's
Omicron commtest section). A run that fails mid-traffic keeps the previous
report in place, so always check the report's `mtime`. To derive a readable,
per-member table, you can run:

```sh
jq -r '["group","expect","member","tx","rx","lost","self_rx"],
  (.v4[] | . as $g | .members[] |
    [$g.group, $g.expect, .member, .tx_count, .rx_count, .lost,
     $g.sender_self_rx])
  | @tsv' ~/.config/voxel/multicast-connectivity-report.json \
  | expand -t 16,25,42,50,58,64
```

Given this display, it outputs columns, per row (one row per group member):

- `group`, `expect`: the group address and whether it came from
  `--mcast-group` (`deliver`) or `--mcast-deny-group` (`deny`)
- `member`: the probe address, seeded from the expected members, plus any
  unexpected responder showing up
- `tx`: requests the host sent to the group
- `rx`: unicast echo replies from a member subscriber, counted through to the
  draining of receives (duplicate copies count too; both-switch traffic
  steering can produce `rx` above `tx` for example)
- `lost`: `tx` minus `rx`, counted from the member's first reply; drops before
  the reply aren't charged as lost, and it floors at 0
- `self_rx`: copies of the host's own request stream that returned to it (per
  group, repeated on each row)

The `check` command does not show the rack-side underlay mapping.
To view this mapping, run `omdb` in the switch zone (see
[Control-plane verification](#control-plane-verification)):

```sh
omdb db multicast info --ip 239.100.0.1
```

Return to the rack host before running the remaining `voxel` and `ping`
commands.

With no `--mcast-group` argument, voxel uses `239.1.1.1`,
which needs its own host route, PIM configuration, and group assignment.
See the commtest section of the [README] for details.

To check on running probes, we can `ping` the groups. An illumos `ping -s` to a
multicast group address prints a reply line per responder; every probe address
from the join event above should return a response. The `-t` flag raises the
multicast TTL past the default of 1, which otherwise expires the request
before it clears `cr1`:

```sh
for group in 239.100.0.1 239.100.0.2 232.100.0.1; do
    echo "== $group =="
    pfexec ping -s -t 16 "$group" 56 10
done
```

### Teardown

If you use `voxel network multicast down`, run it before `voxel destroy`,
while the router VMs are reachable. The `destroy` command removes the recorded host
routes and deletes the `.falcon/` state record after successful teardown;
router state disappears along with the VMs. `down` accepts the same repeated
`--group` argument as the `up` command.

```sh
voxel network multicast down \
    --group 239.100.0.1 \
    --group 239.100.0.2 \
    --group 232.100.0.1 \
    --group 239.100.0.9
```

A `commtest` run leaves the `classone` project and the IP pools behind for
reruns. `voxel commtest -- cleanup` deletes the project, with the pools
remaining.

If cleaning up the rack manually, remove the probes and instances first, then
the default subnet and VPC that a `commtest` run leaves behind. A project
deletion request is rejected while the project still contains resources.

```sh
for probe in $(oxide api "/experimental/v1/probes?project=classone" \
    | jq -r '.items[].name'); do
    oxide api "/experimental/v1/probes/$probe?project=classone" --method DELETE
    # typed: oxide experimental system probe delete \
    #     --probe "$probe" --project classone
done

oxide api "/v1/instances/myvm/stop?project=classone" --method POST
oxide api "/v1/instances/myvm?project=classone" --method DELETE

oxide api "/v1/vpc-subnets/default?project=classone&vpc=default" \
    --method DELETE
oxide api "/v1/vpcs/default?project=classone" --method DELETE
oxide api /v1/projects/classone --method DELETE
```

Wait for the groups from these pools to disappear from
`omdb db multicast groups` (see
[Control-plane verification](#control-plane-verification)). Range removal is
rejected while groups are still allocated. Remove the ranges, then unlink and
delete the pools:

```sh
oxide api /v1/system/ip-pools/mcast-v4-asm/ranges/remove \
    --method POST --input - <<'JSON'
{ "first": "239.100.0.1", "last": "239.100.0.9" }
JSON

oxide api /v1/system/ip-pools/mcast-v4-ssm/ranges/remove \
    --method POST --input - <<'JSON'
{ "first": "232.100.0.1", "last": "232.100.0.1" }
JSON

for pool in mcast-v4-asm mcast-v4-ssm; do
    oxide api "/v1/system/ip-pools/$pool/silos/recovery" --method DELETE
    oxide api "/v1/system/ip-pools/$pool" --method DELETE
done
```

## Troubleshooting

| Symptom | Where to look / what to run |
| --- | --- |
| Group absent | Confirm a linked multicast pool covers the address. |
| A group is stuck in `Creating` or its members are stuck in `Joining` | Inspect `omdb nexus background-tasks show multicast_reconciler`, then run `omdb --destructive nexus background-tasks activate multicast_reconciler` in the switch zone. |
| SSM groups are silent, but ASM groups are fine | The join's `source_ips` must contain the sending host address in this setup. |
| Some members reply, while others don't | Per-sled, run `omdb db multicast members --group-ip <ip>` to return which sled has which members. |

## Future Work

- *TODO*: `commtest` and voxel's host plumbing accept IPv4 multicast only right
  now. Multicast pools, group membership, and the rack dataplane already
  support IPv6 concepts. What's missing is `commtest` verification in Omicron
  and external-ingress plumbing for voxel.
- upstream membership through IGMP host-proxying ([RFC 4605]).
- guest membership snooping and querying from IGMP or MLD reports.

[775]: https://code.oxide.computer/c/illumos-gate/+/775
[`dce8d982`]: https://github.com/oxidecomputer/omicron/commit/dce8d982923f37fd912fd0b22594eddd6447cf61
[`7c298670`]: https://github.com/oxidecomputer/propolis/commit/7c298670530922c53ae3f35cd853c0c82229d1ca
[`12b00ba8`]: https://github.com/oxidecomputer/propolis/commit/12b00ba88d088e0c61b2d4d26f765fc83f6a76b8
[`multicast.rs`]: ../voxel/src/multicast.rs
[`commtest.rs`]: ../voxel/src/commtest.rs
[multicast-to-physical table]: https://github.com/oxidecomputer/opte/blob/931e2ccba7b17d7d98148cf8b36c56d18b256e6b/lib/oxide-vpc/src/engine/overlay.rs#L345-L369
[omdb]: https://github.com/oxidecomputer/omicron/tree/41645034ff180922d19ba0746e6fe250cf5b9c00/dev-tools/omdb
[README]: ../README.md
[README's Privileges section]: ../README.md#privileges
[RFC 1112]: https://datatracker.ietf.org/doc/html/rfc1112
[RFC 3376]: https://datatracker.ietf.org/doc/html/rfc3376
[RFC 3810]: https://datatracker.ietf.org/doc/html/rfc3810
[RFC 4291]: https://datatracker.ietf.org/doc/html/rfc4291
[RFC 4605]: https://datatracker.ietf.org/doc/html/rfc4605
[RFC 4607]: https://datatracker.ietf.org/doc/html/rfc4607
[RFC 5549]: https://datatracker.ietf.org/doc/html/rfc5549
[RFC 5737]: https://datatracker.ietf.org/doc/html/rfc5737
[RFD 488]: https://rfd.shared.oxide.computer/rfd/0488
[rpw]: https://rfd.shared.oxide.computer/rfd/0373
