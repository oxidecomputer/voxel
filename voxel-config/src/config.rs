//! The Voxel configuration model - the single source of truth `voxel` renders
//! every per-node config from, persisted as `voxel.toml`.
//!
//! This replaces both a4x2's static per-sled config files and the hardcoded
//! constants that used to live in `voxel`'s `main.rs` (the `SLEDS` table,
//! `generate_rss`, the router wiring). `voxel config show` renders a
//! [`VoxelConfig`]; `config get`/`set` edit `voxel.toml` in place (format
//! preserving, via `toml_edit`); `config load` imports a prepared TOML file.
//!
//! The `network` section is shaped to map cleanly onto omicron's
//! `wicket_common::rack_setup::PutRssUserConfigInsensitive`, so the typed,
//! release-pinned RSS renderer can consume it directly once it lands.

use serde::{Deserialize, Serialize};

use crate::frr::{FrrNeighbor, FrrRouter};

/// Bootstrap-network IPv6 prefix (first three hextets); see
/// [`SledDesc::bootstrap_addr`]. Each sled appends `:{2*index+1}::1`.
const BOOTSTRAP_NET_PREFIX: &str = "fdb0:a840:2500";

/// Default rack BGP ASN (the switch's local ASN + the uplink `peer_asn` it
/// references). `for_rack` offsets it by rack index for multi-rack transit.
const DEFAULT_RACK_ASN: u32 = 65000;

/// FRR transit ASN base: `ce` is [`TRANSIT_ASN_BASE`]; customer router `cr{i}` is
/// [`TRANSIT_ASN_BASE`]` + i` (i starts at 1). See [`VoxelConfig::to_frr`].
const TRANSIT_ASN_BASE: u32 = 65100;

/// First `enp0sN` index the fabric routers wire from (mirrors `build_topo`'s
/// link-creation order). See [`VoxelConfig::to_frr`].
const FRR_IFACE_BASE: usize = 8;

/// Top-level Voxel configuration (`voxel.toml`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoxelConfig {
    pub topology: Topology,
    pub image: Image,
    pub network: Network,
    pub recovery_silo: RecoverySiloCfg,
    pub falcon: Falcon,
    pub sp: SpCfg,
}

/// falcon/runtime settings (zfs dataset, project workdir, image build root).
/// Each optional - resolved at runtime as **flag > `voxel.toml` > env > built-in
/// default**. The `voxel-rss-gen` path is NOT configured here: it's derived from
/// the image's omicron commit (`image.cp` -> `<build_root>/omicron-<commit>/...`)
/// so the renderer can't drift from the image it renders for. See
/// `resolve_falcon_env`; `--rss-gen` / `$VOXEL_RSS_GEN` still override.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Falcon {
    /// zfs dataset (falcon's `FALCON_DATASET`). `None` -> env, else `rpool/falcon`.
    pub dataset: Option<String>,
    /// Project root that `cargo-bay/` and `.falcon/` live under. Absolute; lets
    /// `voxel` run from anywhere (e.g. installed in `/usr/bin`). `None` -> the
    /// directory containing this `voxel.toml`.
    pub workdir: Option<String>,
    /// Build root for `voxel image create` (the omicron checkout + rss-gen
    /// build dirs live here). Exported as `BUILD_ROOT`. `None` -> env, else
    /// the build script's `$HOME/voxel-builds` default. Lets a non-root user
    /// build images outside `/root`.
    pub build_root: Option<String>,
}

/// SP provider selection: which SPs (if any) run on the real-firmware emulator
/// (`sp-emu`) instead of `sp-sim`. Empty (the default) = every SP on `sp-sim`,
/// i.e. today's behavior. See [`crate::sp::SpFleet::sim_with_emu`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpCfg {
    /// SPs to back with `sp-emu`. Selectors: `"sidecar"`, `"g{index}"` (global
    /// gimlet index), e.g. `["sidecar", "g0"]`.
    pub emu: Vec<String>,
    /// Path to the `sp-emu` binary (illumos) staged into the switch zone.
    /// Required when `emu` is non-empty.
    pub emu_bin: Option<String>,
    /// Hubris image (`.zip`) flashed for the **sidecar** SP (the `sidecar-c-emu`
    /// build). Required when `"sidecar"` is in `emu`.
    pub sidecar_image: Option<String>,
    /// Hubris image (`.zip`) flashed for **gimlet** SPs (the `gimlet-c` build).
    /// Required when any `"g{index}"` is in `emu`.
    pub gimlet_image: Option<String>,
    /// Path to the `faux-mgs` binary (the MGS-side client). Staged into the switch
    /// zone at `--emu` launch so `voxel sp ls/state/exec` can talk to the live SPs
    /// (the same client pilot uses). Optional; operator `sp` commands need it.
    pub faux_mgs: Option<String>,
    /// RoT firmware image (`oxide-rot-1`, raw `.bin` or build archive) run as a
    /// second emulated core alongside the **sidecar** SP — the sprot bridge — so
    /// MGS/Nexus see a real Root of Trust (attestation, CMPA/CFPA, real boot
    /// measurements) instead of the SP's canned fallback. Optional; when set,
    /// `launch --emu` stages it and points the sidecar's `SP_EMU_ROT_FLASH` at it.
    pub rot_image: Option<String>,
}

impl SpCfg {
    /// The hubris image to flash for an SP selector (`"sidecar"` -> sidecar image,
    /// else the gimlet image).
    pub fn image_for(&self, selector: &str) -> Option<&str> {
        match selector {
            "sidecar" => self.sidecar_image.as_deref(),
            _ => self.gimlet_image.as_deref(),
        }
    }
}

impl VoxelConfig {
    /// Parse a `voxel.toml`. Missing fields fall back to defaults.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Render this config as `voxel.toml` (what `config show` prints / seeds).
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("VoxelConfig serializes")
    }

    /// Render the config with the auto-derived topology fields made explicit -
    /// `scrimlets` and `rss_sleds` filled in from the sled count. Use this for
    /// the `voxel-effective.toml` handed to `voxel-rss-gen`: that binary is built
    /// separately (pinned to the image's omicron) and may predate the derivation
    /// logic, so it must receive resolved values, not empty `scrimlets`/`rss_sleds
    /// = 0` it would read as "no peers". "Effective" means fully resolved.
    pub fn to_resolved_toml(&self) -> String {
        let mut c = self.clone();
        c.topology.scrimlets = self.topology.scrimlet_names();
        c.topology.rss_sleds = self.topology.rss_count();
        // Drop host-only fields the separately-built (commit-pinned) voxel-rss-gen
        // doesn't know about - its voxel-config has `deny_unknown_fields`, so a key
        // it predates would fail to parse. `ce_external_ip` is purely a voxel host
        // routing detail, irrelevant to RSS config generation.
        c.topology.ce_external_ip = None;
        // Same: switch interconnects are a launch-time topology detail (falcon
        // links + sled-agent front-port budget), invisible to RSS config.
        c.topology.interconnects = Vec::new();
        c.to_toml()
    }

    /// The computed sled set (replaces the old `SLEDS` const).
    pub fn sleds(&self) -> Vec<SledDesc> {
        self.topology.sleds()
    }
}

/// Which sleds and routers make up the rack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Topology {
    /// Independent racks in this deployment (default 1). `>1` brings up N
    /// separate 3+-sled RSS racks in one falcon deployment, linked via a shared
    /// FRR transit (the "one launch, two racks" multi-rack form). Sleds are
    /// numbered continuously (`rack r` owns `g{r*sleds}`..`g{r*sleds+sleds-1}`);
    /// each rack auto-derives its own scrimlets + RSS and gets its addressing
    /// offset by rack index (see [`Network::for_rack`]).
    pub racks: usize,
    /// Gimlets (sleds) PER RACK - each rack is named `g{r*sleds + i}`. (With the
    /// default `racks = 1` this is just the rack's sled count, as before.)
    pub sleds: usize,
    /// Which sleds run a switch zone (scrimlets), by name. **Empty -> auto-derived
    /// as the first + last sled (`g0` + `g{n-1}`)** - which is exactly what the
    /// `voxel-cp` image bakes (`render-smf` uses first+last), so a topology that
    /// leaves this unset always stays consistent with its image. Set it explicitly
    /// only to override (then the image must be built with the matching pair).
    pub scrimlets: Vec<String>,
    /// How many sleds participate in RSS / trust quorum (the first `rss_sleds`).
    /// **`0` -> auto: all sleds.**
    pub rss_sleds: usize,
    /// Customer routers (boot the `voxel-frr` image).
    pub routers: Vec<String>,
    /// Per-sled guest RAM, GiB (default 8). Lower it to fit more sleds on a box -
    /// guest VMs (`VMM Memory`) are the dominant consumer, so this is the knob
    /// that gates how many sleds fit in physical RAM.
    pub sled_memory_gb: u64,
    /// Per-router guest RAM, GiB (default 4).
    pub router_memory_gb: u64,
    /// Static host-LAN address for the shared customer edge (`ce`), e.g.
    /// `"192.168.68.170"`. Unset (`None`) -> `ce` DHCPs as before and voxel reads
    /// the (volatile) lease over the serial console for the host route. Set it to
    /// a fixed, free LAN address and voxel-init adds it as a SECONDARY address on
    /// `ce`'s uplink (DHCP still provides egress/default) so the host route to each
    /// rack's customer prefix has a STABLE nexthop that never churns across
    /// launches - no serial lookup, no stale-route accumulation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ce_external_ip: Option<String>,
    /// Switch-to-switch ASIC interconnects: extra QSFP front ports linking two
    /// scrimlet sidecars directly (falcon `softnpu_links`), carrying the underlay
    /// (DDM) switch-to-switch - e.g. a cross-rack cable, or `switch0`<->`switch1`
    /// within a rack (the DDM PoC). Each entry is a pair of switch selectors
    /// (`switch0` | `switch1` | `switchN` | `rackR/switchS`). Empty -> none. Each
    /// link adds one front port to BOTH endpoint scrimlets (wired after the
    /// fabric-router uplinks, so it lands on the next `qsfp` tfport). The link
    /// itself is plumbed at launch; DDM/routing over it is configured per
    /// `voxel network`. Managed via `voxel network add-port` / `rm-port`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interconnects: Vec<(String, String)>,
}

impl Default for Topology {
    fn default() -> Self {
        Self {
            racks: 1,
            sleds: 4,
            scrimlets: Vec::new(), // auto: g0 + g{n-1}
            rss_sleds: 0,          // auto: all sleds
            routers: vec!["ce".into(), "cr1".into(), "cr2".into()],
            sled_memory_gb: 8,
            router_memory_gb: 4,
            ce_external_ip: None,
            interconnects: Vec::new(),
        }
    }
}

impl Topology {
    /// Total guest RAM (GiB) this topology asks for - sleds + routers. The
    /// dominant term in the box's `VMM Memory`; the launch preflight checks it
    /// against physical RAM.
    pub fn guest_memory_gb(&self) -> u64 {
        self.total_sleds() as u64 * self.sled_memory_gb
            + self.routers.len() as u64 * self.router_memory_gb
    }

    /// Racks in this deployment (`racks`, floored at 1).
    pub fn racks(&self) -> usize {
        self.racks.max(1)
    }

    /// Total sleds across all racks (`racks * sleds`).
    pub fn total_sleds(&self) -> usize {
        self.racks() * self.sleds
    }
}

impl Topology {
    /// Scrimlet sled names for `rack` (0-based): explicit `scrimlets` (honored
    /// only for a single-rack deployment) else auto - the first and last sled of
    /// that rack's contiguous range (`g{base}` + `g{base+sleds-1}`). Auto matches
    /// what `render-smf` bakes, so an unset topology stays image-consistent.
    pub fn scrimlet_names_for_rack(&self, rack: usize) -> Vec<String> {
        if self.racks() == 1 && !self.scrimlets.is_empty() {
            return self.scrimlets.clone();
        }
        let base = rack * self.sleds;
        if self.sleds >= 2 {
            vec![format!("g{base}"), format!("g{}", base + self.sleds - 1)]
        } else {
            vec![format!("g{base}")]
        }
    }

    /// Scrimlets across all racks (every rack's first+last). Single-rack:
    /// equivalent to the old first+last / explicit list.
    pub fn scrimlet_names(&self) -> Vec<String> {
        (0..self.racks()).flat_map(|r| self.scrimlet_names_for_rack(r)).collect()
    }

    /// Sleds that join RSS: explicit `rss_sleds` if non-zero, else all sleds.
    pub fn rss_count(&self) -> usize {
        if self.rss_sleds > 0 {
            self.rss_sleds
        } else {
            self.sleds
        }
    }

    /// Expand into per-sled descriptors across all racks. Rack `r` owns
    /// `g{r*sleds}`..`g{r*sleds+sleds-1}`; scrimlets + RSS membership are derived
    /// per rack (each rack is an independent RSS domain).
    pub fn sleds(&self) -> Vec<SledDesc> {
        let rss = self.rss_count(); // per-rack count
        let mut out = Vec::new();
        for rack in 0..self.racks() {
            let scrimlets = self.scrimlet_names_for_rack(rack);
            for local in 0..self.sleds {
                let index = rack * self.sleds + local;
                let name = format!("g{index}");
                out.push(SledDesc {
                    rack,
                    scrimlet: scrimlets.iter().any(|s| s == &name),
                    rss: local < rss,
                    name,
                    index,
                });
            }
        }
        out
    }

    /// Resolve a switch selector to a scrimlet's GLOBAL sled index. Accepts a
    /// node name (`g3`), a rack-qualified `rackR/switchS` (R 1-based, S the
    /// 0-based slot within the rack), or a bare global `switchN` (the Nth
    /// scrimlet across all racks). Config-time mirror of `access::resolve_switch`
    /// (works off the descriptor list, no live topology).
    pub fn resolve_switch_index(&self, sel: &str) -> Option<usize> {
        let sleds = self.sleds();
        let scrimlets: Vec<&SledDesc> = sleds.iter().filter(|s| s.scrimlet).collect();
        if let Some(s) = scrimlets.iter().find(|s| s.name == sel) {
            return Some(s.index);
        }
        if let Some((r, sw)) = sel.split_once('/') {
            if let (Some(rack), Some(slot)) = (
                r.strip_prefix("rack").and_then(|x| x.parse::<usize>().ok()),
                sw.strip_prefix("switch").and_then(|x| x.parse::<usize>().ok()),
            ) {
                let rack0 = rack.saturating_sub(1);
                return scrimlets.iter().filter(|s| s.rack == rack0).nth(slot).map(|s| s.index);
            }
        }
        if let Some(n) = sel.strip_prefix("switch").and_then(|x| x.parse::<usize>().ok()) {
            return scrimlets.get(n).map(|s| s.index);
        }
        None
    }

    /// Resolved interconnect endpoint index pairs (unresolvable / self pairs dropped).
    pub fn interconnect_pairs(&self) -> Vec<(usize, usize)> {
        self.interconnects
            .iter()
            .filter_map(|(a, b)| {
                let (ai, bi) = (self.resolve_switch_index(a)?, self.resolve_switch_index(b)?);
                (ai != bi).then_some((ai, bi))
            })
            .collect()
    }

    /// How many interconnects scrimlet `index` participates in (its front-port bump).
    pub fn interconnect_count_for(&self, index: usize) -> usize {
        self.interconnect_pairs().iter().filter(|(a, b)| *a == index || *b == index).count()
    }
}

/// A single sled, expanded from [`Topology`].
#[derive(Debug, Clone, PartialEq)]
pub struct SledDesc {
    pub name: String,
    /// Global sled index (`g{index}`) - drives vdev/sprockets/bootstrap identity.
    pub index: usize,
    /// Which rack (0-based) this sled belongs to.
    pub rack: usize,
    /// Runs a switch zone.
    pub scrimlet: bool,
    /// Participates in its rack's RSS bootstrap discovery.
    pub rss: bool,
}

impl SledDesc {
    /// Bootstrap-network address: `fdb0:a840:2500:{2*index+1}::1`
    /// (g0=1, g1=3, g2=5, g3=7, g4=9, g5=11).
    ///
    /// The 4th group mirrors the sled's underlay viona MAC byte, which the
    /// topology formats as a DECIMAL string into hex MAC notation
    /// (`a8:40:25:00:00:{2*index+1:02}`, topo.rs `new_mac`). sled-agent derives
    /// the bootstrap address from that MAC, so it must be formatted DECIMAL here
    /// too - e.g. index 5 -> `...:11::1`, not hex `...:b::1`. Identical for indices
    /// 0-4 (`2*index+1 < 10`); the old `{:x}` silently diverged at index 5,
    /// breaking bootstrap discovery for >4-sled racks.
    pub fn bootstrap_addr(&self) -> String {
        format!("{BOOTSTRAP_NET_PREFIX}:{}::1", 2 * self.index + 1)
    }

    /// This sled's generated sled-agent config (`sled-config.toml`). The rack's
    /// sled + fabric-router counts size the scrimlet SoftNPU's ports (needed for
    /// >4-sled racks).
    pub fn sled_config(
        &self,
        num_sleds: usize,
        num_fabric_routers: usize,
        data_links: SledDataLinksSchema,
    ) -> crate::sled::SledAgentConfig {
        crate::sled::SledAgentConfig::new(self.index, self.scrimlet)
            .with_topology(num_sleds, num_fabric_routers)
            .with_data_links_schema(data_links)
    }
}

/// Which image version to boot. Bundles are named `voxel-cp-<version>` /
/// `voxel-frr-<version>`; `cp`/`frr` override the full name when set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Image {
    pub version: String,
    pub cp: Option<String>,
    pub frr: Option<String>,
    /// Sled-agent `data_links` config shape, which differs by omicron era (the
    /// image's control-plane version). See [`SledDataLinksSchema`].
    pub data_links_schema: SledDataLinksSchema,
}

impl Default for Image {
    fn default() -> Self {
        Self {
            version: "proto".into(),
            cp: None,
            frr: None,
            data_links_schema: SledDataLinksSchema::default(),
        }
    }
}

/// The shape of sled-agent's `data_links` config field, which changed across
/// omicron versions. `voxel-init` (baked into each image) is shape-preserving:
/// it substitutes the detected NIC names into whichever shape this selects, so
/// one agent works on any image. Pick the variant matching the image's omicron.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SledDataLinksSchema {
    /// Pre-main omicron (e.g. v20 / a3fee0ec): `data_links = ["vioif0", "vioif1"]`.
    #[default]
    List,
    /// omicron main (the `DataLinks` enum):
    /// `data_links = { kind = "virtual", devices = ["vioif0", "vioif1"] }`.
    Tagged,
}

impl Image {
    pub fn cp_image(&self) -> String {
        self.cp.clone().unwrap_or_else(|| format!("voxel-cp-{}", self.version))
    }

    /// The omicron commit encoded in the cp image name (`voxel-cp-<commit>` with
    /// an optional `-<variant>` suffix like `-rd`). Used to locate the matching
    /// `voxel-rss-gen` build under `<build_root>/omicron-<commit>/`. `None` if the
    /// name doesn't follow the `voxel-cp-` convention.
    pub fn cp_commit(&self) -> Option<String> {
        let name = self.cp_image();
        name.strip_prefix("voxel-cp-")
            .and_then(|s| s.split('-').next())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    pub fn frr_image(&self) -> String {
        self.frr.clone().unwrap_or_else(|| format!("voxel-frr-{}", self.version))
    }
}

/// Customer-network / RSS parameters. Maps onto `PutRssUserConfigInsensitive`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub dns_zone: String,
    pub external_dns_ips: Vec<String>,
    pub ntp_servers: Vec<String>,
    pub dns_servers: Vec<String>,
    /// IPv6 `/56`. Empty -> not emitted.
    pub rack_subnet: String,
    /// `internal_services_ip_pool_ranges` (single range).
    pub service_pool_first: String,
    pub service_pool_last: String,
    pub bgp_asn: u32,
    /// IPv4 prefix the rack originates upstream.
    pub infra_prefix: String,
    /// Scrimlet uplink ports (one per switch toward the customer routers).
    pub uplinks: Vec<UplinkCfg>,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            dns_zone: "oxide.test".into(),
            external_dns_ips: vec!["198.51.100.20".into(), "198.51.100.21".into()],
            ntp_servers: vec!["time.cloudflare.com".into()],
            dns_servers: vec!["1.1.1.1".into(), "9.9.9.9".into()],
            rack_subnet: "fd00:17:01:d00::/56".into(),
            service_pool_first: "198.51.100.20".into(),
            service_pool_last: "198.51.100.29".into(),
            bgp_asn: DEFAULT_RACK_ASN,
            infra_prefix: "198.51.100.0/24".into(),
            uplinks: vec![
                UplinkCfg::default_for("switch0", "uplink0"),
                UplinkCfg::default_for("switch1", "uplink1"),
            ],
        }
    }
}

/// Split an `addr/prefix` (or bare `addr`) into the address and its `/prefix`
/// suffix, apply `f` to the address, and rejoin. If `f` returns `None` (the
/// address didn't parse), the original input is returned unchanged.
fn map_addr(s: &str, f: impl FnOnce(&str) -> Option<String>) -> String {
    let (addr, suffix) = match s.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (s, String::new()),
    };
    match f(addr) {
        Some(out) => format!("{out}{suffix}"),
        None => s.to_string(),
    }
}

/// Bump an IPv4 address's 3rd octet by `rack` (preserving any `/prefix`), so each
/// rack gets a distinct customer/service network. Returns the input unchanged if
/// it doesn't parse.
fn offset_v4(s: &str, rack: u8) -> String {
    map_addr(s, |addr| {
        addr.parse::<std::net::Ipv4Addr>().ok().map(|ip| {
            let mut o = ip.octets();
            o[2] = o[2].wrapping_add(rack);
            std::net::Ipv4Addr::from(o).to_string()
        })
    })
}

/// Bump an IPv6 prefix's 3rd hextet by `rack` (preserving any `/prefix`).
fn offset_v6_prefix(s: &str, rack: u16) -> String {
    map_addr(s, |addr| {
        addr.parse::<std::net::Ipv6Addr>().ok().map(|ip| {
            let mut seg = ip.segments();
            seg[2] = seg[2].wrapping_add(rack);
            std::net::Ipv6Addr::from(seg).to_string()
        })
    })
}

impl Network {
    /// This network projected for `rack` (0-based) of a `racks`-rack deployment,
    /// so independent racks don't collide: the customer prefix / service pool /
    /// external DNS IPs shift by rack in the IPv4 3rd octet, the rack subnet in
    /// its IPv6 3rd hextet, and the BGP ASN by rack (so the shared transit can
    /// peer with and route between each rack). The IP/ASN offsets are by rack
    /// index, so **rack 0 keeps the base addressing** (`198.51.100/24`).
    ///
    /// Each rack gets its own external DNS **zone** (`rack{N}.<dns_zone>`,
    /// **1-based** - so racks read as rack1, rack2, ...), making each rack's silo
    /// addressable by a distinct name (`recovery.sys.rack1.oxide.test`). This
    /// holds even for a **single-rack** deployment (rack 0 -> `rack1.oxide.test`):
    /// the addressing is identical to rack 1 of a multi-rack deployment, so a
    /// single rack can later grow a second rack with no renaming / DNS churn, and
    /// one split-DNS / silo-URL convention covers every deployment size. Upstream
    /// NTP/DNS + uplink ports are left as-is.
    pub fn for_rack(&self, rack: usize) -> Network {
        let r8 = rack as u8;
        let dns_zone = format!("rack{}.{}", rack + 1, self.dns_zone);
        Network {
            dns_zone,
            external_dns_ips: self.external_dns_ips.iter().map(|ip| offset_v4(ip, r8)).collect(),
            ntp_servers: self.ntp_servers.clone(),
            dns_servers: self.dns_servers.clone(),
            rack_subnet: offset_v6_prefix(&self.rack_subnet, rack as u16),
            service_pool_first: offset_v4(&self.service_pool_first, r8),
            service_pool_last: offset_v4(&self.service_pool_last, r8),
            bgp_asn: self.bgp_asn + rack as u32,
            infra_prefix: offset_v4(&self.infra_prefix, r8),
            // The uplink's `peer_asn` is the switch's *local* BGP ASN for that
            // session (it references a `[[bgp]]` entry, whose `asn` is offset
            // above); the actual customer-router ASN is auto-discovered via
            // unnumbered `remote-as external`. So it must track the rack's ASN -
            // otherwise rack 1's peer config names an ASN with no `[[bgp]]` block.
            uplinks: self
                .uplinks
                .iter()
                .map(|u| UplinkCfg { peer_asn: u.peer_asn + rack as u32, ..u.clone() })
                .collect(),
        }
    }
}

/// One scrimlet uplink port that peers (unnumbered) with a customer router.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UplinkCfg {
    pub switch: String,
    pub port: String,
    pub peer_asn: u32,
    pub router_lifetime: u16,
    pub port_speed: String,
    pub lldp_port_description: String,
}

impl UplinkCfg {
    fn default_for(switch: &str, description: &str) -> Self {
        Self {
            switch: switch.into(),
            port: "qsfp0".into(),
            peer_asn: DEFAULT_RACK_ASN,
            router_lifetime: 300,
            port_speed: "40G".into(),
            lldp_port_description: description.into(),
        }
    }
}

/// Recovery (initial) silo identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecoverySiloCfg {
    pub silo_name: String,
    pub user_name: String,
    pub user_password_hash: String,
}

impl Default for RecoverySiloCfg {
    fn default() -> Self {
        Self {
            silo_name: "recovery".into(),
            user_name: "recovery".into(),
            // a4x2's "recovery" password hash (password: "oxide").
            user_password_hash: "$argon2id$v=19$m=98304,t=23,p=1$Effh/p6M2ZKdnpJFeGqtGQ$ZtUwcVODAvUAVK6EQ5FJMv+GMlUCo9PQQsy9cagL+EU".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Generation. The RSS `config-rss.toml` is produced by the separate, typed
// `voxel-rss-gen` binary (which consumes a `VoxelConfig`); the FRR router
// configs are generated here.
// ---------------------------------------------------------------------------

impl VoxelConfig {
    /// Build each customer router's `frr.conf` as `(name, FrrRouter)` pairs.
    /// `cr*` are the **shared transit**: each peers `ce` plus *every* scrimlet
    /// across *all* racks, and originates nothing - eBGP (`no bgp
    /// ebgp-requires-policy`) re-advertises each rack's customer prefix to the
    /// other rack's switches, so a 2-rack deployment routes between
    /// `198.51.100/24` and `198.51.101/24` with no extra config. `ce` originates
    /// the default route toward the fabric. ASNs are `65100` (ce) and `65101 + i`
    /// (cr`i`).
    ///
    /// The `enp0sN` interface names mirror `build_topo`'s link-creation order:
    /// `ce` links each fabric router (cr1=enp0s8, cr2=enp0s9, ...); each fabric
    /// router links `ce` first (enp0s8) then every scrimlet in `sleds()` order
    /// (enp0s9, enp0s10, ...).
    pub fn to_frr(&self) -> Vec<(String, FrrRouter)> {
        // Fabric (transit) routers - everything except the customer edge `ce`.
        let fabric: Vec<&String> =
            self.topology.routers.iter().filter(|r| r.as_str() != "ce").collect();
        // Scrimlets across all racks, in falcon softnpu-link order (= `sleds()`
        // order), each labelled with its rack + in-rack switch slot.
        let mut scrimlets: Vec<(String, usize, usize)> = Vec::new();
        let mut per_rack: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
        for s in self.sleds().into_iter().filter(|s| s.scrimlet) {
            let slot = per_rack.entry(s.rack).or_insert(0);
            scrimlets.push((s.name, s.rack, *slot));
            *slot += 1;
        }

        let mut out = Vec::new();
        let mut cr_index = 0u32;
        for name in &self.topology.routers {
            let router = if name == "ce" {
                let neighbors = fabric
                    .iter()
                    .enumerate()
                    .map(|(k, r)| FrrNeighbor::new(format!("enp0s{}", FRR_IFACE_BASE + k), format!("to {r}")))
                    .collect();
                FrrRouter {
                    hostname: "ce".into(),
                    asn: TRANSIT_ASN_BASE,
                    neighbors,
                    originate4: vec!["0.0.0.0/0".into()],
                    originate6: vec!["::/0".into()],
                }
            } else {
                cr_index += 1;
                let mut neighbors =
                    vec![FrrNeighbor::new(format!("enp0s{FRR_IFACE_BASE}"), "to ce")];
                for (k, (sname, rack, slot)) in scrimlets.iter().enumerate() {
                    neighbors.push(FrrNeighbor::new(
                        format!("enp0s{}", FRR_IFACE_BASE + 1 + k),
                        format!("to {sname} (rack{rack} switch{slot})"),
                    ));
                }
                FrrRouter {
                    hostname: name.clone(),
                    asn: TRANSIT_ASN_BASE + cr_index,
                    neighbors,
                    originate4: vec![],
                    originate6: vec![],
                }
            };
            out.push((name.clone(), router));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// `config get` / `config set` - format-preserving edits over `voxel.toml`
// text (comments + layout survive), validated against the typed model.
// ---------------------------------------------------------------------------

/// Read a dotted key (`network.bgp_asn`) out of a `voxel.toml`. Returns the
/// value's TOML rendering, or `None` if the path doesn't exist.
pub fn get(doc_text: &str, key: &str) -> Result<Option<String>, String> {
    use toml_edit::{DocumentMut, Item};
    let doc: DocumentMut = doc_text.parse().map_err(|e| format!("parse voxel.toml: {e}"))?;
    let mut item: &Item = doc.as_item();
    for part in key.split('.') {
        match item.get(part) {
            Some(next) => item = next,
            None => return Ok(None),
        }
    }
    let rendered = match item {
        Item::Value(v) => v.to_string().trim().to_string(),
        other => other.to_string().trim().to_string(),
    };
    Ok(Some(rendered))
}

/// Set a dotted key (`network.bgp_asn = 65001`) in a `voxel.toml`, preserving
/// formatting and comments. Scalars are coerced to the existing value's type
/// (int/bool/float, else string). A value that begins with `[` or `{` is parsed
/// as a TOML array or inline table, so collection keys are settable too - e.g.
/// `config set topology.scrimlets '["g0", "g3"]'`. The result is validated
/// against [`VoxelConfig`] so typos, bad types, and malformed collections are
/// rejected. Returns the updated document text.
pub fn set(doc_text: &str, key: &str, value: &str) -> Result<String, String> {
    use toml_edit::{DocumentMut, Item, Value};

    let doc: DocumentMut = doc_text.parse().map_err(|e| format!("parse voxel.toml: {e}"))?;

    let parts: Vec<&str> = key.split('.').collect();
    let (leaf, parents) = parts.split_last().ok_or("empty key")?;

    // Coerce to the existing leaf's type if present, else infer a string.
    let existing = {
        let mut item: Option<&Item> = Some(doc.as_item());
        for p in &parts {
            item = item.and_then(|i| i.get(p));
        }
        item.and_then(|i| i.as_value()).cloned()
    };
    let trimmed = value.trim_start();
    // Candidate value(s) for the leaf, best-typed first. We try each against the
    // schema below and keep the first that validates - this lets us infer the
    // type of a key that's ABSENT from the file (a default, e.g. `sled_memory_gb`,
    // which has no existing value to coerce to) without quoting numbers/bools.
    let candidates: Vec<Value> = if trimmed.starts_with('[') || trimmed.starts_with('{') {
        // Array or inline table - parse the literal as TOML.
        vec![value
            .parse::<Value>()
            .map_err(|e| format!("{key}: '{value}' is not a valid TOML array/table ({e}); e.g. '[\"g0\", \"g3\"]'"))?]
    } else {
        match existing {
            Some(Value::Integer(_)) => vec![value
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| format!("{key} is an integer; '{value}' is not"))?],
            Some(Value::Boolean(_)) => vec![value
                .parse::<bool>()
                .map(Value::from)
                .map_err(|_| format!("{key} is a boolean; '{value}' is not"))?],
            Some(Value::Float(_)) => vec![value
                .parse::<f64>()
                .map(Value::from)
                .map_err(|_| format!("{key} is a float; '{value}' is not"))?],
            Some(Value::Array(_)) | Some(Value::InlineTable(_)) => {
                return Err(format!(
                    "{key} is a collection; pass a TOML array/table, e.g. '[\"g0\", \"g3\"]'"
                ))
            }
            Some(Value::String(_)) => vec![Value::from(value)],
            // Absent (or an exotic type): infer. A bare TOML scalar (int/bool/
            // float) first, then a string fallback - validation picks the winner.
            _ => {
                let mut c = Vec::new();
                if let Ok(v) = value.parse::<Value>() {
                    if !matches!(v, Value::Array(_) | Value::InlineTable(_)) {
                        c.push(v);
                    }
                }
                c.push(Value::from(value));
                c
            }
        }
    };

    // Insert each candidate into a fresh copy of the doc; keep the first that
    // still models a valid VoxelConfig.
    let mut last_err = String::new();
    for cand in candidates {
        let mut doc = doc.clone();
        let mut table = doc.as_table_mut();
        for p in parents {
            let entry = table.entry(p).or_insert_with(toml_edit::table);
            table = entry
                .as_table_mut()
                .ok_or_else(|| format!("{p} is not a table"))?;
        }
        table.insert(leaf, Item::Value(cand));
        let text = doc.to_string();
        match VoxelConfig::from_toml(&text) {
            Ok(_) => return Ok(text),
            Err(e) => last_err = format!("invalid after set: {e}"),
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_reads_dotted_keys() {
        let text = VoxelConfig::default().to_toml();
        assert_eq!(get(&text, "network.bgp_asn").unwrap().as_deref(), Some("65000"));
        assert_eq!(get(&text, "topology.sleds").unwrap().as_deref(), Some("4"));
        assert_eq!(get(&text, "network.dns_zone").unwrap().as_deref(), Some("\"oxide.test\""));
        assert_eq!(get(&text, "nope.missing").unwrap(), None);
    }

    #[test]
    fn set_preserves_type_and_validates() {
        let text = "# my rack\n[network]\nbgp_asn = 65000\n".to_string();
        let out = set(&text, "network.bgp_asn", "65010").unwrap();
        assert!(out.contains("# my rack"), "comment preserved");
        assert!(out.contains("bgp_asn = 65010"));
        // Wrong type is rejected.
        assert!(set(&text, "network.bgp_asn", "notanumber").is_err());
    }

    #[test]
    fn set_infers_type_for_absent_default_field() {
        // A defaulted numeric field isn't written in a minimal toml, so there's
        // no existing type to coerce to - it must still be written as an integer,
        // not a quoted string (which would fail the u64 schema).
        let text = "[topology]\nsleds = 6\n".to_string();
        let out = set(&text, "topology.sled_memory_gb", "7").unwrap();
        assert!(out.contains("sled_memory_gb = 7"), "expected an int, got: {out}");
        assert_eq!(VoxelConfig::from_toml(&out).unwrap().topology.sled_memory_gb, 7);
    }

    #[test]
    fn set_rejects_unknown_key() {
        let text = VoxelConfig::default().to_toml();
        // deny_unknown_fields -> validation fails on typos.
        assert!(set(&text, "network.bgp_nasn", "1").is_err());
    }

    #[test]
    fn set_string_key() {
        let text = VoxelConfig::default().to_toml();
        let out = set(&text, "network.dns_zone", "lab.example").unwrap();
        assert_eq!(
            get(&out, "network.dns_zone").unwrap().as_deref(),
            Some("\"lab.example\"")
        );
    }

    #[test]
    fn set_array_key() {
        // A `[...]` value is parsed as a TOML array, and the result must still
        // model a VoxelConfig (so the override actually takes effect).
        let text = VoxelConfig::default().to_toml();
        let out = set(&text, "topology.scrimlets", "[\"g1\", \"g7\"]").unwrap();
        let cfg = VoxelConfig::from_toml(&out).unwrap();
        assert_eq!(cfg.topology.scrimlets, vec!["g1".to_string(), "g7".to_string()]);
        // A scalar against a collection key is still rejected.
        assert!(set(&text, "topology.routers", "ce").is_err());
        // Malformed array -> clear error, not a silent string.
        assert!(set(&text, "topology.scrimlets", "[\"g1\"").is_err());
    }

    #[test]
    fn resolved_toml_materializes_auto_derived_topology() {
        // Auto config (empty scrimlets, rss_sleds 0) must serialize the DERIVED
        // values, so rss-gen sees explicit peers instead of an empty set.
        let cfg = VoxelConfig::from_toml("[topology]\nsleds = 4\n").unwrap();
        let resolved = VoxelConfig::from_toml(&cfg.to_resolved_toml()).unwrap();
        assert_eq!(resolved.topology.scrimlets, vec!["g0".to_string(), "g3".to_string()]);
        assert_eq!(resolved.topology.rss_sleds, 4);
        // Same sled set as the original - resolution is behavior-preserving.
        assert_eq!(resolved.sleds(), cfg.sleds());
    }

    #[test]
    fn memory_knobs_default_and_total() {
        let t = Topology::default();
        assert_eq!(t.sled_memory_gb, 8);
        assert_eq!(t.router_memory_gb, 4);
        assert_eq!(t.guest_memory_gb(), 44); // 4*8 + 3*4
        // Shrinking per-sled RAM to fit a bigger rack.
        let cfg = VoxelConfig::from_toml("[topology]\nsleds = 6\nsled_memory_gb = 6\n").unwrap();
        assert_eq!(cfg.topology.guest_memory_gb(), 48); // 6*6 + 3*4
        assert_eq!(cfg.topology.router_memory_gb, 4); // unset -> default
    }

    #[test]
    fn default_round_trips() {
        let cfg = VoxelConfig::default();
        let toml = cfg.to_toml();
        let back = VoxelConfig::from_toml(&toml).expect("round-trips");
        assert_eq!(cfg, back);
    }

    #[test]
    fn scrimlets_and_rss_auto_derive_from_sled_count() {
        // Unset -> scrimlets are first + last, all sleds in RSS - at any size.
        for n in [3usize, 4, 6, 10] {
            let cfg = VoxelConfig::from_toml(&format!("[topology]\nsleds = {n}\n")).unwrap();
            let s = cfg.sleds();
            assert!(s[0].scrimlet, "{n}: g0 scrimlet");
            assert!(s[n - 1].scrimlet, "{n}: g{} scrimlet", n - 1);
            assert_eq!(s.iter().filter(|x| x.scrimlet).count(), 2, "{n}: exactly 2 scrimlets");
            assert_eq!(s.iter().filter(|x| x.rss).count(), n, "{n}: all sleds in RSS");
        }
        // Explicit scrimlets/rss_sleds still override the auto choice.
        let cfg = VoxelConfig::from_toml(
            "[topology]\nsleds = 4\nscrimlets = [\"g1\", \"g2\"]\nrss_sleds = 3\n",
        )
        .unwrap();
        let s = cfg.sleds();
        assert!(s[1].scrimlet && s[2].scrimlet && !s[0].scrimlet && !s[3].scrimlet);
        assert_eq!(s.iter().filter(|x| x.rss).count(), 3);
    }

    #[test]
    fn sp_section_parses_and_defaults_empty() {
        // Default: no emu, no artifact paths (all-sim, zero-config).
        let d = VoxelConfig::default();
        assert!(d.sp.emu.is_empty());
        assert!(d.sp.emu_bin.is_none() && d.sp.sidecar_image.is_none() && d.sp.gimlet_image.is_none());
        // Populated [sp] parses, and image_for routes by selector.
        let cfg = VoxelConfig::from_toml(
            "[sp]\nemu = [\"sidecar\", \"g0\"]\nemu_bin = \"/x/sp-emu\"\nsidecar_image = \"/x/sc.zip\"\ngimlet_image = \"/x/g.zip\"\n",
        )
        .unwrap();
        assert_eq!(cfg.sp.emu, vec!["sidecar".to_string(), "g0".to_string()]);
        assert_eq!(cfg.sp.image_for("sidecar"), Some("/x/sc.zip"));
        assert_eq!(cfg.sp.image_for("g0"), Some("/x/g.zip"));
        assert_eq!(cfg.sp.emu_bin.as_deref(), Some("/x/sp-emu"));
    }

    #[test]
    fn falcon_section_parses_and_defaults_to_none() {
        let cfg = VoxelConfig::from_toml(
            "[falcon]\ndataset = \"testbed/falcon\"\nbuild_root = \"/x/builds\"\n",
        )
        .unwrap();
        assert_eq!(cfg.falcon.dataset.as_deref(), Some("testbed/falcon"));
        assert_eq!(cfg.falcon.build_root.as_deref(), Some("/x/builds"));
        // Absent section -> both None (env/default fallback at runtime).
        let d = VoxelConfig::default();
        assert!(d.falcon.dataset.is_none() && d.falcon.build_root.is_none());
    }

    #[test]
    fn cp_commit_strips_prefix_and_variant_suffix() {
        let mut img = Image::default();
        img.cp = Some("voxel-cp-43bb5af-rd".into());
        assert_eq!(img.cp_commit().as_deref(), Some("43bb5af"));
        img.cp = Some("voxel-cp-99a0aec".into());
        assert_eq!(img.cp_commit().as_deref(), Some("99a0aec"));
        // Falls back to `voxel-cp-<version>` when `cp` is unset.
        img.cp = None;
        img.version = "abc1234".into();
        assert_eq!(img.cp_commit().as_deref(), Some("abc1234"));
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Only override the sled count; everything else defaults.
        let cfg = VoxelConfig::from_toml("[topology]\nsleds = 3\n").unwrap();
        assert_eq!(cfg.topology.sleds, 3);
        assert_eq!(cfg.network.bgp_asn, 65000);
    }

    #[test]
    fn sled_expansion_and_bootstrap_addrs() {
        let t = Topology::default();
        let sleds = t.sleds();
        assert_eq!(sleds.len(), 4);
        assert!(sleds[0].scrimlet && sleds[3].scrimlet);
        assert!(!sleds[1].scrimlet);
        assert_eq!(sleds[0].bootstrap_addr(), "fdb0:a840:2500:1::1");
        assert_eq!(sleds[2].bootstrap_addr(), "fdb0:a840:2500:5::1");
    }

    #[test]
    fn two_racks_expand_with_per_rack_scrimlets_and_offset_addressing() {
        let t = Topology { racks: 2, sleds: 3, ..Topology::default() };
        assert_eq!(t.total_sleds(), 6);
        let s = t.sleds();
        assert_eq!(s.len(), 6);
        // rackA = g0,g1,g2 (scrimlets g0,g2); rackB = g3,g4,g5 (scrimlets g3,g5).
        let scr: Vec<(usize, &str, bool)> =
            s.iter().map(|d| (d.rack, d.name.as_str(), d.scrimlet)).collect();
        assert_eq!(
            scr,
            vec![
                (0, "g0", true), (0, "g1", false), (0, "g2", true),
                (1, "g3", true), (1, "g4", false), (1, "g5", true),
            ]
        );
        assert!(s.iter().all(|d| d.rss), "all 3 sleds per rack join RSS");
        // Per-rack addressing offset: rack 1 shifts the customer/service nets.
        let net = Network::default();
        assert_eq!(net.for_rack(0).infra_prefix, "198.51.100.0/24");
        assert_eq!(net.for_rack(1).infra_prefix, "198.51.101.0/24");
        assert_eq!(net.for_rack(1).service_pool_first, "198.51.101.20");
        assert_eq!(net.for_rack(1).external_dns_ips[0], "198.51.101.20");
        assert_eq!(net.for_rack(1).rack_subnet, "fd00:17:2:d00::/56");
        assert_eq!(net.for_rack(1).bgp_asn, 65001);
        // The uplink peer_asn tracks the rack's local ASN (rack 0 unchanged).
        assert_eq!(net.for_rack(0).uplinks[0].peer_asn, 65000);
        assert_eq!(net.for_rack(1).uplinks[0].peer_asn, 65001);
        // Every rack gets its own 1-based external DNS zone, so silos are
        // addressable per rack (recovery.sys.rack{N}.oxide.test) - including a
        // single-rack deploy, whose rack 0 is rack1 (matches rack 1 of a
        // multi-rack deploy, so it can grow a 2nd rack with no DNS churn).
        assert_eq!(net.for_rack(0).dns_zone, "rack1.oxide.test");
        assert_eq!(net.for_rack(1).dns_zone, "rack2.oxide.test");
    }

    #[test]
    fn interconnects_resolve_and_count() {
        // Default single-rack 4-sled: scrimlets g0 (switch0) + g3 (switch1).
        let mut t = Topology::default();
        t.interconnects = vec![("switch0".into(), "switch1".into())];
        assert_eq!(t.resolve_switch_index("switch0"), Some(0));
        assert_eq!(t.resolve_switch_index("switch1"), Some(3));
        assert_eq!(t.resolve_switch_index("g3"), Some(3));
        assert_eq!(t.resolve_switch_index("rack1/switch1"), Some(3));
        assert_eq!(t.resolve_switch_index("bogus"), None);
        assert_eq!(t.interconnect_pairs(), vec![(0, 3)]);
        assert_eq!(t.interconnect_count_for(0), 1);
        assert_eq!(t.interconnect_count_for(3), 1);
        assert_eq!(t.interconnect_count_for(1), 0); // a non-scrimlet sled
        // A self / unresolvable pair is dropped from the resolved pairs.
        t.interconnects = vec![("switch0".into(), "switch0".into()), ("switch0".into(), "nope".into())];
        assert!(t.interconnect_pairs().is_empty());
    }

    #[test]
    fn frr_transit_peers_every_scrimlet_across_racks() {
        // Single rack: cr1 peers ce + the rack's 2 scrimlets (g0,g3), edge first.
        let cfg = VoxelConfig::from_toml("[topology]\nsleds = 4\n").unwrap();
        let frr = cfg.to_frr();
        let cr1 = &frr.iter().find(|(n, _)| n == "cr1").unwrap().1;
        assert_eq!(cr1.asn, 65101);
        let ifaces: Vec<&str> = cr1.neighbors.iter().map(|n| n.interface.as_str()).collect();
        assert_eq!(ifaces, vec!["enp0s8", "enp0s9", "enp0s10"]); // ce, g0, g3
        assert!(cr1.originate4.is_empty(), "transit originates nothing");

        // a3x2x2: cr1 peers ce + all 4 scrimlets (rack0 g0,g2; rack1 g3,g5).
        let cfg = VoxelConfig::from_toml("[topology]\nracks = 2\nsleds = 3\n").unwrap();
        let frr = cfg.to_frr();
        let cr1 = &frr.iter().find(|(n, _)| n == "cr1").unwrap().1;
        let ifaces: Vec<&str> = cr1.neighbors.iter().map(|n| n.interface.as_str()).collect();
        assert_eq!(ifaces, vec!["enp0s8", "enp0s9", "enp0s10", "enp0s11", "enp0s12"]);
        // Descriptions carry the rack/switch identity for each peered scrimlet.
        let descs: Vec<&str> = cr1.neighbors.iter().map(|n| n.description.as_str()).collect();
        assert_eq!(descs[1], "to g0 (rack0 switch0)");
        assert_eq!(descs[2], "to g2 (rack0 switch1)");
        assert_eq!(descs[3], "to g3 (rack1 switch0)");
        assert_eq!(descs[4], "to g5 (rack1 switch1)");
        // ce still peers both fabric routers and originates the default.
        let ce = &frr.iter().find(|(n, _)| n == "ce").unwrap().1;
        assert_eq!(
            ce.neighbors.iter().map(|n| n.interface.as_str()).collect::<Vec<_>>(),
            vec!["enp0s8", "enp0s9"]
        );
        assert_eq!(ce.originate4, vec!["0.0.0.0/0".to_string()]);
    }

    #[test]
    fn bootstrap_addr_is_decimal_past_four_sleds() {
        // index 5 -> 2*5+1 = 11, which must render as decimal "11" (matching the
        // viona MAC byte sled-agent derives the address from), NOT hex "b".
        let sleds = Topology { sleds: 6, ..Topology::default() }.sleds();
        assert_eq!(sleds[5].bootstrap_addr(), "fdb0:a840:2500:11::1");
        assert_eq!(sleds[4].bootstrap_addr(), "fdb0:a840:2500:9::1");
    }

    #[test]
    fn three_node_drops_rss_membership() {
        let cfg = VoxelConfig::from_toml("[topology]\nsleds = 4\nrss_sleds = 3\n").unwrap();
        let rss: Vec<_> = cfg.sleds().into_iter().filter(|s| s.rss).collect();
        // Only the first 3 sleds join RSS; the 4th is dropped from the bootstrap set.
        assert_eq!(rss.len(), 3);
        assert!(rss.iter().all(|s| s.index < 3));
    }

    #[test]
    fn image_names() {
        let mut img = Image::default();
        assert_eq!(img.cp_image(), "voxel-cp-proto");
        img.version = "v19.1".into();
        assert_eq!(img.frr_image(), "voxel-frr-v19.1");
        img.cp = Some("custom-cp".into());
        assert_eq!(img.cp_image(), "custom-cp");
    }
}
