# Voxel configuration parameters

## Model

`voxel.toml` holds overrides only. Any key you omit inherits its default.

`voxel config show` renders the fully resolved config (your overrides plus all
defaults). It is a view, never written to disk.

Location: `~/.config/voxel/voxel.toml`, then `/etc/voxel/voxel.toml`. Override
with `--config` or `$VOXEL_CONFIG`.

Edit with `voxel config set <key> <value>` (format-preserving, adds one key).
An empty or absent file is valid: every field defaults.

Falcon settings resolve as: flag, then `voxel.toml`, then env, then built-in.

## [topology]

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `racks` | int | `1` | Independent RSS racks in one deployment. `>1` links them via shared FRR transit. |
| `sleds` | int | `4` | Gimlets per rack. |
| `scrimlets` | list | `[]` | Switch-zone sleds. Empty auto-derives first + last sled. Honored only for single-rack. |
| `rss_sleds` | int | `0` | Sleds in RSS/trust quorum. `0` auto-derives all sleds. |
| `routers` | list | `["ce", "cr1", "cr2"]` | Customer routers (boot the frr image). `ce` is the edge; `cr*` are transit. |
| `sled_memory_gb` | int | `8` | Per-sled guest RAM. Gates how many sleds fit in physical RAM. |
| `router_memory_gb` | int | `4` | Per-router guest RAM. |
| `ce_external_ip` | string | unset | Static host-LAN address for `ce`. Unset means `ce` DHCPs and voxel reads the lease over serial. In isolated mode keep it outside the static node range (see `[external]`). |

## [image]

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `version` | string | `"proto"` | Shorthand suffix for both images (`voxel-cp-<version>`, `voxel-frr-<version>`). Ignored when `cp`/`frr` are set. |
| `cp` | string | unset | Full cp image name. Overrides `version`. Unset follows the workspace's omicron pin (`voxel-cp-<pin>`, the image a commitless `voxel image create` bakes). Keep the `voxel-cp-<commit>` form so the matching omicron checkout is found. |
| `frr` | string | unset | Full frr image name. Overrides `version`. |
| `data_links_schema` | enum | unset | `list` or `tagged`. Unset auto-detects from the image. |
| `disks_schema` | enum | unset | `vdevs`, `external_disks`, or `hardcoded` (omicron#10948). Unset auto-detects from the image. |

## [network]

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `dns_zone` | string | `"oxide.test"` | External DNS zone. |
| `external_dns_ips` | list | `["198.51.100.20", "198.51.100.21"]` | External DNS server addresses. |
| `ntp_servers` | list | `["time.cloudflare.com"]` | Upstream NTP. |
| `dns_servers` | list | `["1.1.1.1", "9.9.9.9"]` | Upstream DNS. |
| `rack_subnet` | string | `"fd00:17:01:d00::/56"` | IPv6 /56 rack subnet. |
| `service_pool_first` | string | `"198.51.100.20"` | Internal service IP pool start. |
| `service_pool_last` | string | `"198.51.100.29"` | Internal service IP pool end. |
| `bgp_asn` | int | `65000` | Rack BGP ASN. |
| `infra_prefix` | string | `"198.51.100.0/24"` | IPv4 prefix the rack originates upstream. |
| `router_mode` | enum | `bgp` | `bgp` (unnumbered eBGP) or `static` (numbered /30 uplinks, static routes, BFD). |
| `transit_prefix` | string | `"198.51.101.0/24"` | IPv4 /24 carved into per-uplink /30s for `static` mode. |
| `transit_bfd` | bool | `false` | `static` mode: BFD-track transit routes. Needs a dataplane where softnpu BFD establishes. |
| `uplinks` | list of tables | two entries (see below) | Scrimlet uplink ports toward the customer routers. |

### [[network.uplinks]]

One block per switch. Defaults: `switch0`/`uplink0` and `switch1`/`uplink1`.

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `switch` | string | required | `switch0`, `switch1`, ... |
| `port` | string | `"qsfp0"` | Front port. |
| `peer_asn` | int | `65000` | Local BGP ASN for the session. |
| `router_lifetime` | int | `300` | Router advertisement lifetime, seconds. |
| `port_speed` | string | `"40G"` | Link speed. |
| `lldp_port_description` | string | required | LLDP port description. |

## [external]

Host-side external segment. This is host-only plumbing and never reaches the
rack's RSS config. See the README's "Isolated external network" section.

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `mode` | enum | `"lan"` | `lan` attaches node external NICs to `link`, or the host's default-route link (`$EXT_INTERFACE` overrides both). `isolated` builds the segment on a host etherstub with NAT out `uplink`. |
| `link` | string | unset | Lan-mode external link (e.g. `igb1`), for hosts whose default-route interface is not the LAN under test. Ignored in isolated mode. |
| `uplink` | string | unset | Physical link the isolated segment NATs out of (e.g. `igb0`). Required in isolated mode. |
| `subnet` | string | `"172.30.199.0/24"` | The isolated segment's subnet, chosen to avoid common home/office LANs. `up` refuses if it overlaps a host address. |
| `host_ip` | string | `"172.30.199.199"` | Host address on the segment: the nodes' default gateway and NAT inside address. Image builds also use `host_ip - 1` for the builder VM. |
| `ip_start` | string | `"172.30.199.10"` | First static node address. Nodes number contiguously, sleds then `topology.routers`. |
| `dns` | list | `["1.1.1.1", "9.9.9.9"]` | Nameservers handed to the nodes. |
| `mtu` | int | `1500` | Etherstub MTU. Must stay below 9000 so voxel-init's jumbo probe classifies external NICs correctly. |

## [recovery_silo]

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `silo_name` | string | `"recovery"` | Initial silo name. |
| `user_name` | string | `"recovery"` | Initial user name. |
| `user_password_hash` | string | argon2id for password `oxide` | Recovery user password hash. |

## [falcon]

Runtime paths. Each unset value resolves via env then built-in default.

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `dataset` | string | `$FALCON_DATASET`, else `rpool/falcon` | ZFS dataset. |
| `workdir` | string | directory of `voxel.toml` | Root that `cargo-bay/` and `.falcon/` live under. Absolute. |
| `build_root` | string | `$BUILD_ROOT`, else `$HOME/voxel-builds` | Root for `voxel image create` (omicron checkouts). |
| `propolis_binary` | string | unset | `propolis-server` the host runs each node under. Unset leaves falcon's own binary, which it downloads on demand. Set it to run a locally built propolis, e.g. for a device-model fix that has not reached a release. Rack nodes only, as the image-build VM keeps falcon's binary. |
| `ssh_pubkey` | string | first of `~/.ssh/id_ed25519.pub`, `id_ecdsa.pub`, `id_rsa.pub` | SSH public key staged into every node's cargo-bay as `root_authorized_keys`; voxel-init appends it to root's `authorized_keys`, so `ssh root@<node>` authenticates by key instead of the empty password. Content-validated before staging (a private key is refused). When unset and no default key exists, staging is skipped. |

## [sp]

SP emulation. Empty `emu` runs every SP on `sp-sim` (default behavior).

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `emu` | list | `[]` | SPs to back with `sp-emu`. Selectors: `"sidecar"`, `"g{index}"`. |
| `emu_bin` | string | unset | Path to the `sp-emu` binary. Required when `emu` is non-empty. |
| `sidecar_image` | string | unset | Hubris `.zip` for the sidecar SP (`sidecar-c-emu`). Required when `"sidecar"` in `emu`. |
| `gimlet_image` | string | unset | Hubris `.zip` for gimlet SPs (`gimlet-c`). Required when any `"g{index}"` in `emu`. |
| `rot_image` | string | unset | RoT firmware (`oxide-rot-1`) for the sidecar sprot bridge. Optional. |
| `faux_mgs` | string | unset | Path to `faux-mgs`. Needed for `voxel sp` operator commands. |

## Minimal example

```toml
[image]
cp  = "voxel-cp-<commit>"
frr = "voxel-frr-<version>"
```

Everything else defaults. Add keys only to override.
