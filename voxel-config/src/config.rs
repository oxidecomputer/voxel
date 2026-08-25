// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The configuration model behind voxel.toml; every per-node config renders from it.

use serde::{Deserialize, Serialize};

use crate::frr::{FrrNeighbor, FrrRouter, StaticUplink};

/// Bootstrap-network IPv6 prefix (first three hextets); see
/// SledDesc::bootstrap_addr. Each sled appends :{2*index+1}::1.
const BOOTSTRAP_NET_PREFIX: &str = "fdb0:a840:2500";

/// Default rack BGP ASN (the switch's local ASN + the uplink peer_asn it
/// references). for_rack offsets it by rack index for multi-rack transit.
const DEFAULT_RACK_ASN: u32 = 65000;

/// FRR transit ASN base: ce is TRANSIT_ASN_BASE; customer router cr{i} is
/// TRANSIT_ASN_BASE + i (i starts at 1). See VoxelConfig::to_frr.
const TRANSIT_ASN_BASE: u32 = 65100;

/// First enp0sN index the fabric routers wire from, mirroring falcon's slot
/// assignment; voxel-init verifies staged names against the node's real links.
const FRR_IFACE_BASE: usize = 8;

// Serial numbers are 8 ascii chars starting with '2'; the 7 char prefix
// leaves one numeral for the sled index.
pub const SLED_SERIAL_PREFIX: &str = "2FAKE00";

/// Part number shared by all fake sleds.
pub const SLED_PART_NUMBER: &str = "913-0000019";

/// Top-level Voxel configuration (voxel.toml).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoxelConfig {
    pub topology: Topology,
    pub image: Image,
    pub network: Network,
    pub recovery_silo: RecoverySiloCfg,
    pub falcon: Falcon,
    pub sp: SpCfg,
    /// Omitted from serialized output while untouched, so a plain LAN rack
    /// doesn't grow a section the operator never set.
    #[serde(default, skip_serializing_if = "External::is_default")]
    pub external: External,
}

/// Provisioning mode for the nodes' external (host-LAN) links.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExternalMode {
    /// Wire external VNICs onto an existing LAN with DHCP (default).
    #[default]
    Lan,
    /// Voxel-managed etherstub: NAT out uplink, static per-node addresses.
    Isolated,
}

/// The rack's external segment. Host-only plumbing; never reaches the rack's
/// RSS config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct External {
    /// lan (default; existing behavior) or isolated.
    pub mode: ExternalMode,
    /// Physical link the isolated subnet NATs out of (e.g. igb0). Required
    /// in isolated mode, and validated before use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uplink: Option<String>,
    /// The isolated segment's subnet.
    pub subnet: String,
    /// Host address on the etherstub and the nodes' default gateway.
    pub host_ip: String,
    /// First static node address; nodes number contiguously from here in
    /// sleds() then routers order.
    pub ip_start: String,
    /// Nameservers handed to the nodes.
    pub dns: Vec<String>,
    /// Etherstub MTU. Launch refuses 9000+: voxel-init classifies underlay
    /// NICs by jumbo acceptance, so the external link must reject jumbo.
    pub mtu: u32,
}

impl Default for External {
    fn default() -> Self {
        Self {
            mode: ExternalMode::Lan,
            uplink: None,
            subnet: "172.30.199.0/24".into(),
            host_ip: "172.30.199.199".into(),
            ip_start: "172.30.199.10".into(),
            dns: vec!["1.1.1.1".into(), "9.9.9.9".into()],
            mtu: 1500,
        }
    }
}

impl External {
    /// True when untouched, so serialization omits the section.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether voxel manages an isolated external segment.
    pub fn isolated(&self) -> bool {
        self.mode == ExternalMode::Isolated
    }

    /// Prefix length parsed from subnet; None if it is not CIDR.
    pub fn prefix_length(&self) -> Option<u32> {
        Some(u32::from(self.subnet_net()?.width()))
    }

    /// subnet parsed as an Ipv4Net, or None if it is not CIDR.
    fn subnet_net(&self) -> Option<oxnet::Ipv4Net> {
        self.subnet.parse().ok()
    }

    /// Whether ip sits strictly between subnet's network and broadcast. /31
    /// and /32 have no usable range here; the segment needs distinct addresses.
    fn ip_is_usable(&self, ip: std::net::Ipv4Addr) -> bool {
        self.subnet_net().is_some_and(|net| {
            match (net.network(), net.broadcast()) {
                (Some(network), Some(broadcast)) => {
                    net.contains(ip) && ip != network && ip != broadcast
                }
                _ => false,
            }
        })
    }

    /// Whether host_ip parses and is a usable address inside subnet.
    pub fn host_ip_is_usable(&self) -> bool {
        self.host_ip.parse().is_ok_and(|ip| self.ip_is_usable(ip))
    }

    /// Static address for the nth node: ip_start + nth. None on a boundary,
    /// outside the subnet, colliding with host_ip, or unparseable inputs.
    pub fn node_ip(&self, nth: usize) -> Option<String> {
        let start: std::net::Ipv4Addr = self.ip_start.parse().ok()?;
        let host: std::net::Ipv4Addr = self.host_ip.parse().ok()?;
        if !self.ip_is_usable(host) {
            return None;
        }
        let base = u32::from(start).checked_add(nth as u32)?;
        let ip = std::net::Ipv4Addr::from(base);
        // A large rack can overrun an operator-set ip_start.
        if !self.ip_is_usable(ip) {
            return None;
        }
        if ip == host {
            return None;
        }
        Some(ip.to_string())
    }

    /// Builder VM address: host_ip - 1 with subnet's prefix length. None if
    /// either address is unusable.
    pub fn builder_net(&self) -> Option<String> {
        let host: std::net::Ipv4Addr = self.host_ip.parse().ok()?;
        if !self.ip_is_usable(host) {
            return None;
        }
        let prev = u32::from(host).checked_sub(1)?;
        let ip = std::net::Ipv4Addr::from(prev);
        if !self.ip_is_usable(ip) {
            return None;
        }
        let prefix = self.prefix_length()?;
        Some(format!("{ip}/{prefix} {}", self.host_ip))
    }
}

/// falcon/runtime settings, each optional and resolved as flag > voxel.toml
/// > env > built-in default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Falcon {
    /// zfs dataset (falcon's FALCON_DATASET). None -> env, else rpool/falcon.
    pub dataset: Option<String>,
    /// Absolute project root that cargo-bay/ and .falcon/ live under. None ->
    /// the directory containing this voxel.toml.
    pub workdir: Option<String>,
    /// Build root for image create (omicron checkouts live here). None -> env,
    /// else $HOME/voxel-builds.
    pub build_root: Option<String>,
    /// `propolis-server` binary the host runs each node under. `None` -> falcon's
    /// own `<falcon_dir>/bin/propolis-server`, which it downloads on demand.
    ///
    /// Set this to run under a locally built propolis, e.g. when a device-model
    /// fix has not reached a release yet. Falcon skips its download when the path
    /// is set (`Runner::set_propolis_binary`), the equivalent of a4x2's
    /// `FALCON_PROPOLIS_BINARY`.
    ///
    /// Applies to rack nodes only. The image-build VM keeps falcon's own
    /// binary.
    pub propolis_binary: Option<String>,
}

/// SP provider selection: which SPs run on the real-firmware emulator sp-emu
/// instead of sp-sim. Empty (default) = all sp-sim.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpCfg {
    /// SPs to back with sp-emu. Selectors: "sidecar", "g{index}" (global
    /// gimlet index), e.g. ["sidecar", "g0"].
    pub emu: Vec<String>,
    /// Path to the sp-emu binary (illumos) staged into the switch zone.
    /// Required when emu is non-empty.
    pub emu_bin: Option<String>,
    /// Hubris image flashed for the sidecar SP (the sidecar-c-emu build).
    /// Required when "sidecar" is in emu.
    pub sidecar_image: Option<String>,
    /// Hubris image flashed for gimlet SPs (the gimlet-c build). Required when
    /// any "g{index}" is in emu.
    pub gimlet_image: Option<String>,
    /// Path to the faux-mgs binary, staged into the switch zone at --emu
    /// launch. Optional; operator sp commands need it.
    pub faux_mgs: Option<String>,
    /// RoT firmware image (oxide-rot-1) run as a second emulated core beside
    /// the sidecar SP so MGS/Nexus see a real Root of Trust. Optional.
    pub rot_image: Option<String>,
}

impl SpCfg {
    /// The hubris image for an SP selector.
    pub fn image_for(&self, selector: &str) -> Option<&str> {
        match selector {
            "sidecar" => self.sidecar_image.as_deref(),
            _ => self.gimlet_image.as_deref(),
        }
    }
}

impl VoxelConfig {
    /// Parse a voxel.toml. Missing fields fall back to defaults.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Render this config as voxel.toml (what config show prints / seeds).
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("VoxelConfig serializes")
    }

    /// The computed sled set.
    pub fn sleds(&self) -> Vec<SledDesc> {
        self.topology.sleds()
    }

    /// Every node's static external address, sleds then routers; truncates at
    /// the first address node_ip refuses.
    pub fn static_external_ips(&self) -> Vec<(String, String)> {
        self.sleds()
            .into_iter()
            .map(|s| s.name)
            .chain(self.topology.routers.iter().cloned())
            .enumerate()
            .map_while(|(n, name)| {
                self.external.node_ip(n).map(|ip| (name, ip))
            })
            .collect()
    }
}

/// Which sleds and routers make up the rack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Topology {
    /// Independent racks (default 1); >1 brings up N racks linked by the
    /// shared FRR transit, sleds numbered continuously across racks.
    pub racks: usize,
    /// Sleds per rack, named g{r*sleds + i}.
    pub sleds: usize,
    /// Switch-zone sleds by name; empty auto-derives the first + last RSS
    /// sled.
    pub scrimlets: Vec<String>,
    /// Sleds in RSS / trust quorum (the first N); 0 means all.
    pub rss_sleds: usize,
    /// Customer routers (boot the voxel-frr image).
    pub routers: Vec<String>,
    /// Per-sled guest RAM in GiB (default 8), the knob that gates how many
    /// sleds fit in physical RAM.
    pub sled_memory_gb: u64,
    /// Per-router guest RAM, GiB (default 4).
    pub router_memory_gb: u64,
    /// Static host-LAN address added as a secondary on ce's uplink, giving the
    /// host route a stable nexthop. Unset -> read ce's DHCP lease over serial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ce_external_ip: Option<String>,
}

impl Default for Topology {
    fn default() -> Self {
        Self {
            racks: 1,
            sleds: 4,
            scrimlets: Vec::new(), // auto: first + last RSS sled
            rss_sleds: 0,          // auto: all sleds
            routers: vec!["ce".into(), "cr1".into(), "cr2".into()],
            sled_memory_gb: 8,
            router_memory_gb: 4,
            ce_external_ip: None,
        }
    }
}

impl Topology {
    /// Total guest RAM in GiB; the launch preflight checks it against
    /// physical RAM.
    pub fn guest_memory_gb(&self) -> u64 {
        self.total_sleds() as u64 * self.sled_memory_gb
            + self.routers.len() as u64 * self.router_memory_gb
    }

    /// Racks in this deployment (racks, floored at 1).
    pub fn racks(&self) -> usize {
        self.racks.max(1)
    }

    /// Total sleds across all racks (racks * sleds).
    pub fn total_sleds(&self) -> usize {
        self.racks() * self.sleds
    }
}

impl Topology {
    /// Scrimlet names for rack: explicit list (single-rack only), else first +
    /// last RSS sled. Switch zones must live on RSS sleds; see validate.
    pub fn scrimlet_names_for_rack(&self, rack: usize) -> Vec<String> {
        if self.racks() == 1 && !self.scrimlets.is_empty() {
            return self.scrimlets.clone();
        }
        let base = rack * self.sleds;
        let last = self.rss_count() - 1;
        if last >= 1 {
            vec![format!("g{base}"), format!("g{}", base + last)]
        } else {
            vec![format!("g{base}")]
        }
    }

    /// Scrimlets across all racks.
    pub fn scrimlet_names(&self) -> Vec<String> {
        (0..self.racks())
            .flat_map(|r| self.scrimlet_names_for_rack(r))
            .collect()
    }

    /// Sleds that join RSS: explicit rss_sleds if non-zero, else all sleds.
    pub fn rss_count(&self) -> usize {
        if self.rss_sleds > 0 { self.rss_sleds } else { self.sleds }
    }

    /// Reject a topology whose switch zones land outside the RSS set;
    /// reachable only via explicit scrimlets.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(s) = self.sleds().iter().find(|s| s.scrimlet && !s.rss) {
            return Err(format!(
                "scrimlet {} is not in the RSS set (rss_sleds = {}): switch \
                 zones must live on RSS sleds or nexus handoff cannot find \
                 their switch ports",
                s.name, self.rss_sleds
            ));
        }
        Ok(())
    }

    /// Per-sled descriptors across all racks; scrimlets and RSS membership
    /// derive per rack.
    pub fn sleds(&self) -> Vec<SledDesc> {
        let rss = self.rss_count(); // per-rack count
        let mut out = Vec::new();
        for rack in 0..self.racks() {
            let scrimlets = self.scrimlet_names_for_rack(rack);
            for local in 0..self.sleds {
                let index = rack * self.sleds + local;
                let name = format!("g{index}");
                let serial_number = format!("{SLED_SERIAL_PREFIX}{}", index);
                let part_number = SLED_PART_NUMBER.to_string();
                out.push(SledDesc {
                    rack,
                    scrimlet: scrimlets.iter().any(|s| s == &name),
                    rss: local < rss,
                    name,
                    index,
                    part_number,
                    serial_number,
                });
            }
        }
        out
    }

    /// Cross-rack sidecar link pairs as global scrimlet indices (full mesh
    /// between racks, empty single-rack); each adds a front port to both ends.
    pub fn interconnect_pairs(&self) -> Vec<(usize, usize)> {
        let sleds = self.sleds();
        let scrimlets: Vec<&SledDesc> =
            sleds.iter().filter(|s| s.scrimlet).collect();
        let mut out = Vec::new();
        for i in 0..scrimlets.len() {
            for j in (i + 1)..scrimlets.len() {
                if scrimlets[i].rack != scrimlets[j].rack {
                    out.push((scrimlets[i].index, scrimlets[j].index));
                }
            }
        }
        out
    }

    /// How many interconnects scrimlet index participates in (its front-port bump).
    pub fn interconnect_count_for(&self, index: usize) -> usize {
        self.interconnect_pairs()
            .iter()
            .filter(|(a, b)| *a == index || *b == index)
            .count()
    }
}

/// A single sled, expanded from Topology.
#[derive(Debug, Clone, PartialEq)]
pub struct SledDesc {
    pub name: String,
    /// Global sled index; drives vdev/sprockets/bootstrap identity.
    pub index: usize,
    /// Which rack (0-based) this sled belongs to.
    pub rack: usize,
    /// Runs a switch zone.
    pub scrimlet: bool,
    /// Participates in its rack's RSS bootstrap discovery.
    pub rss: bool,
    /// BaseboardId part number.
    pub part_number: String,
    /// BaseboardId serial number.
    pub serial_number: String,
}

impl SledDesc {
    /// Bootstrap address fdb0:a840:2500:{2*index+1}::1. The group must format
    /// DECIMAL to match the underlay viona MAC byte sled-agent derives it from.
    pub fn bootstrap_addr(&self) -> String {
        format!("{BOOTSTRAP_NET_PREFIX}:{}::1", 2 * self.index + 1)
    }

    /// This sled's generated sled-agent config; the counts size the scrimlet
    /// SoftNPU's ports.
    pub fn sled_config(
        &self,
        num_sleds: usize,
        num_fabric_routers: usize,
        data_links: SledDataLinksSchema,
        disks: SledDisksSchema,
    ) -> crate::sled::SledAgentConfig {
        crate::sled::SledAgentConfig::new(
            self.index,
            self.scrimlet,
            data_links,
            disks,
        )
        .with_topology(num_sleds, num_fabric_routers)
    }
}

/// Which image version to boot. Bundles are named voxel-cp-<version> /
/// voxel-frr-<version>; cp/frr override the full name when set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Image {
    pub version: String,
    pub cp: Option<String>,
    pub frr: Option<String>,
    /// Override the detected sled-agent data_links shape; normally unset.
    pub data_links_schema: Option<SledDataLinksSchema>,
    /// Override the detected sled-agent disks shape; normally unset.
    pub disks_schema: Option<SledDisksSchema>,
}

impl Default for Image {
    fn default() -> Self {
        Self {
            version: "proto".into(),
            cp: None,
            frr: None,
            data_links_schema: None,
            disks_schema: None,
        }
    }
}

/// Shape of sled-agent's data_links field across omicron versions; voxel-init
/// substitutes detected NIC names into whichever shape is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SledDataLinksSchema {
    /// Pre-main omicron (e.g. v20 / a3fee0ec): data_links = ["vioif0", "vioif1"].
    List,
    /// omicron main (the DataLinks enum):
    /// data_links = { kind = "virtual", devices = ["vioif0", "vioif1"] }.
    Tagged,
}

/// Shape of sled-agent's disk config across omicron versions, selected
/// independently of SledDataLinksSchema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SledDisksSchema {
    /// Pre-rename omicron (e.g. a3fee0ec / 43bb5af / 99a0aec):
    /// vdevs = ["m2_g0_0.vdev", ...].
    Vdevs,
    /// external_disks = { kind = "virtual", vdevs = [...] } (e.g. cc07512e0).
    ExternalDisks,
    /// external_disks = { kind = "hardcoded", vdevs = [...], disks = [] }
    /// (omicron main).
    Hardcoded,
}

impl SledDataLinksSchema {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Tagged => "tagged",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "list" => Some(Self::List),
            "tagged" => Some(Self::Tagged),
            _ => None,
        }
    }
}

impl SledDisksSchema {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vdevs => "vdevs",
            Self::ExternalDisks => "external_disks",
            Self::Hardcoded => "hardcoded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "vdevs" => Some(Self::Vdevs),
            "external_disks" => Some(Self::ExternalDisks),
            "hardcoded" => Some(Self::Hardcoded),
            _ => None,
        }
    }
}

impl Image {
    pub fn cp_image(&self) -> String {
        self.cp.clone().unwrap_or_else(|| format!("voxel-cp-{}", self.version))
    }

    /// The omicron commit in the cp image name, locating the checkout under
    /// build_root; None if the name is not voxel-cp-<commit>[-variant].
    pub fn cp_commit(&self) -> Option<String> {
        let name = self.cp_image();
        name.strip_prefix("voxel-cp-")
            .and_then(|s| s.split('-').next())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    pub fn frr_image(&self) -> String {
        self.frr
            .clone()
            .unwrap_or_else(|| format!("voxel-frr-{}", self.version))
    }
}

/// Upstream routing mode toward the customer routers.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum RouterMode {
    /// Unnumbered eBGP (default).
    #[default]
    Bgp,
    /// Numbered /30 uplinks with static routes and BFD.
    Static,
}

/// Customer-network / RSS parameters. Maps onto PutRssUserConfigInsensitive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub dns_zone: String,
    pub external_dns_ips: Vec<String>,
    pub ntp_servers: Vec<String>,
    pub dns_servers: Vec<String>,
    /// IPv6 /56. Empty -> not emitted.
    pub rack_subnet: String,
    /// Service IP pool (single range). Rendered as the rack's sole
    /// service_ip_pools entry.
    pub service_pool_first: String,
    pub service_pool_last: String,
    pub bgp_asn: u32,
    /// IPv4 prefix the rack originates upstream.
    pub infra_prefix: String,
    /// Upstream routing mode.
    pub router_mode: RouterMode,
    /// IPv4 /24 carved into per-uplink /30s for Static mode (.1 router, .2 sidecar).
    pub transit_prefix: String,
    /// Static mode: BFD-track the transit routes. Defaults off, matching a4x2.
    pub transit_bfd: bool,
    /// Scrimlet uplink ports (one per switch toward the customer routers).
    pub uplinks: Vec<UplinkCfg>,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            dns_zone: "oxide.test".into(),
            external_dns_ips: vec![
                "198.51.100.20".into(),
                "198.51.100.21".into(),
            ],
            ntp_servers: vec!["time.cloudflare.com".into()],
            dns_servers: vec!["1.1.1.1".into(), "9.9.9.9".into()],
            rack_subnet: "fd00:17:01:d00::/56".into(),
            service_pool_first: "198.51.100.20".into(),
            service_pool_last: "198.51.100.29".into(),
            bgp_asn: DEFAULT_RACK_ASN,
            infra_prefix: "198.51.100.0/24".into(),
            router_mode: RouterMode::Bgp,
            transit_prefix: "198.51.101.0/24".into(),
            transit_bfd: false,
            uplinks: vec![
                UplinkCfg::default_for("switch0", "uplink0"),
                UplinkCfg::default_for("switch1", "uplink1"),
            ],
        }
    }
}

/// Split an addr/prefix (or bare addr) into the address and its /prefix
/// suffix, apply f to the address, and rejoin. If f returns None (the
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

/// Bump an IPv4 address's 3rd octet by rack (preserving any /prefix), so each
/// rack gets a distinct customer/service network. Returns the input unchanged if
/// it doesn't parse or the offset would leave the octet range (checked, not
/// wrapping, so an out-of-range rack can't silently alias another rack's net).
fn offset_v4(s: &str, rack: usize) -> String {
    map_addr(s, |addr| {
        let ip = addr.parse::<std::net::Ipv4Addr>().ok()?;
        let mut o = ip.octets();
        o[2] = o[2].checked_add(u8::try_from(rack).ok()?)?;
        Some(std::net::Ipv4Addr::from(o).to_string())
    })
}

/// Offset an IPv6 rack subnet by rack /56s WITHIN its /48 (bits 48-55, the high
/// byte of hextet 3), so every rack shares one /48 AZ (omicron's AZ=/48, rack=/56
/// scheme) and cross-rack underlay is a single aggregate prefix. rack 0 is
/// unchanged. Preserves any /prefix. Returns the input unchanged if it doesn't
/// parse or the offset overflows hextet 3 (checked, not wrapping).
fn offset_v6_rack56(s: &str, rack: usize) -> String {
    map_addr(s, |addr| {
        let ip = addr.parse::<std::net::Ipv6Addr>().ok()?;
        let mut seg = ip.segments();
        let delta = u16::try_from(rack).ok()?.checked_mul(256)?; // rack << 8
        seg[3] = seg[3].checked_add(delta)?;
        Some(std::net::Ipv6Addr::from(seg).to_string())
    })
}

impl Network {
    /// The network for rack N: v4 nets shift by rack in octet 3, the subnet by
    /// /56, the ASN by rack; the DNS zone becomes rack{N+1}.<zone> (1-based).
    pub fn for_rack(&self, rack: usize) -> Network {
        // Saturating so an absurd rack count cannot wrap onto a lower rack's ASN.
        let rack_asn = u32::try_from(rack).unwrap_or(u32::MAX);
        let dns_zone = format!("rack{}.{}", rack + 1, self.dns_zone);
        Network {
            dns_zone,
            external_dns_ips: self
                .external_dns_ips
                .iter()
                .map(|ip| offset_v4(ip, rack))
                .collect(),
            ntp_servers: self.ntp_servers.clone(),
            dns_servers: self.dns_servers.clone(),
            rack_subnet: offset_v6_rack56(&self.rack_subnet, rack),
            service_pool_first: offset_v4(&self.service_pool_first, rack),
            service_pool_last: offset_v4(&self.service_pool_last, rack),
            bgp_asn: self.bgp_asn.saturating_add(rack_asn),
            infra_prefix: offset_v4(&self.infra_prefix, rack),
            router_mode: self.router_mode,
            transit_prefix: offset_v4(&self.transit_prefix, rack),
            transit_bfd: self.transit_bfd,
            // peer_asn references the switch's local [[bgp]] entry, whose asn
            // is offset above, so it must track the rack's ASN.
            uplinks: self
                .uplinks
                .iter()
                .map(|u| UplinkCfg {
                    peer_asn: u.peer_asn.saturating_add(rack_asn),
                    ..u.clone()
                })
                .collect(),
        }
    }

    /// Base address of transit_prefix (its .0). None if it doesn't parse.
    fn transit_base(&self) -> Option<std::net::Ipv4Addr> {
        self.transit_prefix.split('/').next()?.parse().ok()
    }

    /// The /30 for transit block: (router gateway .1, sidecar .2). None if
    /// transit_prefix does not parse.
    pub fn transit_slash30(&self, block: usize) -> Option<(String, String)> {
        let block = u32::try_from(block).ok()?;
        let b = u32::from(self.transit_base()?)
            .checked_add(block.checked_mul(4)?)?;
        let gateway = std::net::Ipv4Addr::from(b.checked_add(1)?);
        let sidecar = std::net::Ipv4Addr::from(b.checked_add(2)?);
        Some((gateway.to_string(), sidecar.to_string()))
    }

    /// The transit /30 for router_index to switch_slot; block = router *
    /// n_switches + slot. Single source for both the sidecar and router side.
    pub fn transit_slash30_for(
        &self,
        router_index: usize,
        switch_slot: usize,
        n_switches: usize,
    ) -> Option<(String, String)> {
        self.transit_slash30(router_index * n_switches + switch_slot)
    }

    /// Static-mode infra address lot (first, last) spanning nblocks /30s; every
    /// numbered switch-port address must fall inside it or Nexus rejects handoff.
    pub fn infra_ip_range(
        &self,
        nblocks: usize,
    ) -> Option<(std::net::Ipv4Addr, std::net::Ipv4Addr)> {
        if nblocks == 0 {
            return None;
        }
        let nblocks = u32::try_from(nblocks).ok()?;
        let b = u32::from(self.transit_base()?);
        let first = std::net::Ipv4Addr::from(b.checked_add(1)?);
        let span = nblocks.checked_mul(4)?.checked_sub(1)?;
        let last = std::net::Ipv4Addr::from(b.checked_add(span)?);
        Some((first, last))
    }
}

/// One generated scrimlet uplink port toward a fabric router. Derived, not a
/// config knob.
#[derive(Debug, Clone, PartialEq)]
pub struct UplinkPort {
    pub switch: String,
    pub switch_slot: usize,
    pub router_index: usize,
    /// qsfp{router_index}; fabric uplinks take the first front ports.
    pub port: String,
    pub peer_asn: u32,
    pub router_lifetime: u16,
    pub port_speed: String,
    pub lldp: String,
    /// Static-mode sidecar side, addr/30.
    pub sidecar_addr: String,
    /// Static-mode router side (nexthop + BFD peer), bare addr.
    pub gateway: String,
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

// Generation: FRR router configs. The RSS config renders in rss_request.

impl VoxelConfig {
    /// Routers other than ce.
    fn fabric_router_count(&self) -> usize {
        self.topology.routers.iter().filter(|r| r.as_str() != "ce").count()
    }

    /// Number of scrimlets (switches) in rack.
    fn scrimlets_in_rack(&self, rack: usize) -> usize {
        self.sleds()
            .into_iter()
            .filter(|s| s.scrimlet && s.rack == rack)
            .count()
    }

    /// Generated uplink ports for rack: every switch fans out to every fabric
    /// router (qsfp{router}); static block = router * n_switches + switch.
    pub fn uplink_ports(&self, rack: usize) -> Vec<UplinkPort> {
        let net = self.network.for_rack(rack);
        let n_cr = self.fabric_router_count();
        let n_sc = self.scrimlets_in_rack(rack);
        let mut out = Vec::new();
        for (sc, u) in net.uplinks.iter().enumerate() {
            for c in 0..n_cr {
                let (gateway, sidecar) =
                    net.transit_slash30_for(c, sc, n_sc).unwrap_or_default();
                out.push(UplinkPort {
                    switch: u.switch.clone(),
                    switch_slot: sc,
                    router_index: c,
                    port: format!("qsfp{c}"),
                    peer_asn: u.peer_asn,
                    router_lifetime: u.router_lifetime,
                    port_speed: u.port_speed.clone(),
                    lldp: format!("{}-cr{}", u.lldp_port_description, c + 1),
                    sidecar_addr: format!("{sidecar}/30"),
                    gateway,
                });
            }
        }
        out
    }

    /// Cross-rack interconnect ports on rack's switches as (switch, port),
    /// landing after the fabric uplinks at qsfp{n_cr + k}. Empty single-rack.
    pub fn interconnect_ports(&self, rack: usize) -> Vec<(String, String)> {
        let n_cr = self.fabric_router_count();
        let pairs = self.topology.interconnect_pairs();
        let sleds = self.sleds();
        let scrimlets: Vec<&SledDesc> =
            sleds.iter().filter(|s| s.scrimlet).collect();
        let mut out = Vec::new();
        for (slot, s) in scrimlets.iter().filter(|s| s.rack == rack).enumerate()
        {
            let mut k = 0;
            for (a, b) in &pairs {
                if *a == s.index || *b == s.index {
                    out.push((
                        format!("switch{slot}"),
                        format!("qsfp{}", n_cr + k),
                    ));
                    k += 1;
                }
            }
        }
        out
    }

    /// The enp0sN name of a router's external NIC, derived from build_topo's
    /// link order; voxel-init verifies it against the node's actual links.
    pub fn router_ext_iface(&self, router: &str) -> String {
        let fabric_router_count =
            self.topology.routers.iter().filter(|r| r.as_str() != "ce").count();
        let total_scrimlet_count =
            self.sleds().into_iter().filter(|s| s.scrimlet).count();
        let n = if router == "ce" {
            FRR_IFACE_BASE + fabric_router_count
        } else {
            FRR_IFACE_BASE + 1 + total_scrimlet_count
        };
        format!("enp0s{n}")
    }

    /// Each customer router's frr.conf. cr* peer ce plus every scrimlet across
    /// all racks and originate nothing; ce originates the default route.
    pub fn to_frr(&self) -> Vec<(String, FrrRouter)> {
        // Everything except the customer edge.
        let fabric: Vec<&String> = self
            .topology
            .routers
            .iter()
            .filter(|r| r.as_str() != "ce")
            .collect();
        // Scrimlets in falcon softnpu-link order, labelled rack + switch slot.
        let mut scrimlets: Vec<(String, usize, usize)> = Vec::new();
        let mut per_rack: std::collections::BTreeMap<usize, usize> =
            std::collections::BTreeMap::new();
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
                    .map(|(k, r)| {
                        FrrNeighbor::new(
                            format!("enp0s{}", FRR_IFACE_BASE + k),
                            format!("to {r}"),
                        )
                    })
                    .collect();
                FrrRouter {
                    hostname: "ce".into(),
                    asn: TRANSIT_ASN_BASE,
                    neighbors,
                    originate4: vec!["0.0.0.0/0".into()],
                    originate6: vec!["::/0".into()],
                    static_uplinks: vec![],
                    track_bfd: false,
                }
            } else {
                cr_index += 1;
                let ce_nb =
                    FrrNeighbor::new(format!("enp0s{FRR_IFACE_BASE}"), "to ce");
                match self.network.router_mode {
                    RouterMode::Bgp => {
                        let mut neighbors = vec![ce_nb];
                        for (k, (sname, rack, slot)) in
                            scrimlets.iter().enumerate()
                        {
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
                            static_uplinks: vec![],
                            track_bfd: false,
                        }
                    }
                    // Numbered /30 to every scrimlet, block = router *
                    // n_switches + slot; keeps eBGP to ce for redistribution.
                    RouterMode::Static => {
                        let c = (cr_index - 1) as usize;
                        let mut static_uplinks = Vec::new();
                        for (k, (_, rack, slot)) in scrimlets.iter().enumerate()
                        {
                            let net = self.network.for_rack(*rack);
                            let n_sc = self.scrimlets_in_rack(*rack);
                            if let Some((gateway, sidecar)) =
                                net.transit_slash30_for(c, *slot, n_sc)
                            {
                                static_uplinks.push(StaticUplink {
                                    interface: format!(
                                        "enp0s{}",
                                        FRR_IFACE_BASE + 1 + k
                                    ),
                                    address: format!("{gateway}/30"),
                                    peer: sidecar,
                                    peer_asn: net.bgp_asn,
                                    route: net.infra_prefix.clone(),
                                });
                            }
                        }
                        FrrRouter {
                            hostname: name.clone(),
                            asn: TRANSIT_ASN_BASE + cr_index,
                            neighbors: vec![ce_nb],
                            originate4: vec![],
                            originate6: vec![],
                            static_uplinks,
                            track_bfd: self.network.transit_bfd,
                        }
                    }
                }
            };
            out.push((name.clone(), router));
        }
        out
    }
}

// config get / config set: format-preserving edits over voxel.toml text,
// validated against the typed model.

/// Read a dotted key out of a voxel.toml as its TOML rendering; None if the
/// path does not exist.
pub fn get(doc_text: &str, key: &str) -> Result<Option<String>, String> {
    use toml_edit::{DocumentMut, Item};
    let doc: DocumentMut =
        doc_text.parse().map_err(|e| format!("parse voxel.toml: {e}"))?;
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

/// Set a dotted key in a voxel.toml, preserving formatting. Scalars coerce to
/// the existing type; [ or { parses as TOML; the result must validate.
pub fn set(doc_text: &str, key: &str, value: &str) -> Result<String, String> {
    use toml_edit::{DocumentMut, Item, Value};

    let doc: DocumentMut =
        doc_text.parse().map_err(|e| format!("parse voxel.toml: {e}"))?;

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
    // Candidates best-typed first; the first that validates wins, which infers
    // the type of a key absent from the file.
    let candidates: Vec<Value> = if trimmed.starts_with('[')
        || trimmed.starts_with('{')
    {
        // Parse the literal as TOML.
        vec![value.parse::<Value>().map_err(|e| {
            format!(
                "{key}: '{value}' is not a valid TOML array/table ({e}); e.g. '[\"g0\", \"g3\"]'"
            )
        })?]
    } else {
        match existing {
            Some(Value::Integer(_)) => {
                vec![value.parse::<i64>().map(Value::from).map_err(|_| {
                    format!("{key} is an integer; '{value}' is not")
                })?]
            }
            Some(Value::Boolean(_)) => {
                vec![value.parse::<bool>().map(Value::from).map_err(|_| {
                    format!("{key} is a boolean; '{value}' is not")
                })?]
            }
            Some(Value::Float(_)) => {
                vec![value.parse::<f64>().map(Value::from).map_err(|_| {
                    format!("{key} is a float; '{value}' is not")
                })?]
            }
            Some(Value::Array(_)) | Some(Value::InlineTable(_)) => {
                return Err(format!(
                    "{key} is a collection; pass a TOML array/table, e.g. '[\"g0\", \"g3\"]'"
                ));
            }
            Some(Value::String(_)) => vec![Value::from(value)],
            // Absent: try a bare TOML scalar first, then a string.
            _ => {
                let mut c = Vec::new();
                if let Ok(v) = value.parse::<Value>()
                    && !matches!(v, Value::Array(_) | Value::InlineTable(_))
                {
                    c.push(v);
                }
                c.push(Value::from(value));
                c
            }
        }
    };

    // Keep the first candidate that still models a valid VoxelConfig.
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
    use indoc::{formatdoc, indoc};

    #[test]
    fn get_reads_dotted_keys() {
        let text = VoxelConfig::default().to_toml();
        assert_eq!(
            get(&text, "network.bgp_asn").unwrap().as_deref(),
            Some("65000")
        );
        assert_eq!(get(&text, "topology.sleds").unwrap().as_deref(), Some("4"));
        assert_eq!(
            get(&text, "network.dns_zone").unwrap().as_deref(),
            Some("\"oxide.test\"")
        );
        assert_eq!(get(&text, "nope.missing").unwrap(), None);
    }

    #[test]
    fn set_preserves_type_and_validates() {
        let text = indoc! {"
            # my rack
            [network]
            bgp_asn = 65000
        "}
        .to_string();
        let out = set(&text, "network.bgp_asn", "65010").unwrap();
        assert!(out.contains("# my rack"), "comment preserved");
        assert!(out.contains("bgp_asn = 65010"));
        // Wrong type is rejected.
        assert!(set(&text, "network.bgp_asn", "notanumber").is_err());
    }

    #[test]
    fn set_infers_type_for_absent_default_field() {
        // An absent defaulted field has no type to coerce to; it must still be
        // written as an integer.
        let text = indoc! {"
            [topology]
            sleds = 6
        "}
        .to_string();
        let out = set(&text, "topology.sled_memory_gb", "7").unwrap();
        assert!(
            out.contains("sled_memory_gb = 7"),
            "expected an int, got: {out}"
        );
        assert_eq!(
            VoxelConfig::from_toml(&out).unwrap().topology.sled_memory_gb,
            7
        );
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
        // A [...] value is parsed as a TOML array, and the result must still
        // model a VoxelConfig (so the override actually takes effect).
        let text = VoxelConfig::default().to_toml();
        let out = set(&text, "topology.scrimlets", "[\"g1\", \"g7\"]").unwrap();
        let cfg = VoxelConfig::from_toml(&out).unwrap();
        assert_eq!(
            cfg.topology.scrimlets,
            vec!["g1".to_string(), "g7".to_string()]
        );
        // A scalar against a collection key is still rejected.
        assert!(set(&text, "topology.routers", "ce").is_err());
        // Malformed array -> clear error, not a silent string.
        assert!(set(&text, "topology.scrimlets", "[\"g1\"").is_err());
    }

    #[test]
    fn auto_topology_derives_scrimlets_and_rss_peers() {
        // Derived values feed the RSS bootstrap set and switch placement.
        let cfg = VoxelConfig::from_toml(indoc! {"
            [topology]
            sleds = 4
        "})
        .unwrap();
        assert_eq!(
            cfg.topology.scrimlet_names(),
            vec!["g0".to_string(), "g3".to_string()]
        );
        assert_eq!(cfg.topology.rss_count(), 4);
        // Spelling the derived values out explicitly describes the same rack.
        let explicit = VoxelConfig::from_toml(indoc! {r#"
            [topology]
            sleds = 4
            scrimlets = ["g0", "g3"]
            rss_sleds = 4
        "#})
        .unwrap();
        assert_eq!(explicit.sleds(), cfg.sleds());
    }

    #[test]
    fn memory_knobs_default_and_total() {
        let t = Topology::default();
        assert_eq!(t.sled_memory_gb, 8);
        assert_eq!(t.router_memory_gb, 4);
        assert_eq!(t.guest_memory_gb(), 44); // 4*8 + 3*4
        // Shrinking per-sled RAM to fit a bigger rack.
        let cfg = VoxelConfig::from_toml(indoc! {"
            [topology]
            sleds = 6
            sled_memory_gb = 6
        "})
        .unwrap();
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
            let cfg = VoxelConfig::from_toml(&formatdoc! {"
                [topology]
                sleds = {n}
            "})
            .unwrap();
            let s = cfg.sleds();
            assert!(s[0].scrimlet, "{n}: g0 scrimlet");
            assert!(s[n - 1].scrimlet, "{n}: g{} scrimlet", n - 1);
            assert_eq!(
                s.iter().filter(|x| x.scrimlet).count(),
                2,
                "{n}: exactly 2 scrimlets"
            );
            assert_eq!(
                s.iter().filter(|x| x.rss).count(),
                n,
                "{n}: all sleds in RSS"
            );
        }
        // Explicit scrimlets/rss_sleds still override the auto choice.
        let cfg = VoxelConfig::from_toml(indoc! {r#"
            [topology]
            sleds = 4
            scrimlets = ["g1", "g2"]
            rss_sleds = 3
        "#})
        .unwrap();
        let s = cfg.sleds();
        assert!(
            s[1].scrimlet && s[2].scrimlet && !s[0].scrimlet && !s[3].scrimlet
        );
        assert_eq!(s.iter().filter(|x| x.rss).count(), 3);
    }

    #[test]
    fn sp_section_parses_and_defaults_empty() {
        // Default: no emu, no artifact paths (all-sim, zero-config).
        let d = VoxelConfig::default();
        assert!(d.sp.emu.is_empty());
        assert!(
            d.sp.emu_bin.is_none()
                && d.sp.sidecar_image.is_none()
                && d.sp.gimlet_image.is_none()
        );
        // Populated [sp] parses, and image_for routes by selector.
        let cfg = VoxelConfig::from_toml(indoc! {r#"
            [sp]
            emu = ["sidecar", "g0"]
            emu_bin = "/x/sp-emu"
            sidecar_image = "/x/sc.zip"
            gimlet_image = "/x/g.zip"
        "#})
        .unwrap();
        assert_eq!(cfg.sp.emu, vec!["sidecar".to_string(), "g0".to_string()]);
        assert_eq!(cfg.sp.image_for("sidecar"), Some("/x/sc.zip"));
        assert_eq!(cfg.sp.image_for("g0"), Some("/x/g.zip"));
        assert_eq!(cfg.sp.emu_bin.as_deref(), Some("/x/sp-emu"));
    }

    #[test]
    fn falcon_section_parses_and_defaults_to_none() {
        let cfg = VoxelConfig::from_toml(indoc! {r#"
            [falcon]
            dataset = "testbed/falcon"
            build_root = "/x/builds"
        "#})
        .unwrap();
        assert_eq!(cfg.falcon.dataset.as_deref(), Some("testbed/falcon"));
        assert_eq!(cfg.falcon.build_root.as_deref(), Some("/x/builds"));
        // Absent section -> both None (env/default fallback at runtime).
        let d = VoxelConfig::default();
        assert!(d.falcon.dataset.is_none() && d.falcon.build_root.is_none());
    }

    #[test]
    fn external_section_parses_defaults_and_set_round_trips() {
        // Default: lan mode. The untouched section is omitted from output.
        let d = VoxelConfig::default();
        assert!(!d.external.isolated());
        assert!(!d.to_toml().contains("[external]"));
        // Populated section parses; unset fields keep the guide defaults.
        let cfg = VoxelConfig::from_toml(indoc! {r#"
            [external]
            mode = "isolated"
            uplink = "igb0"
        "#})
        .unwrap();
        assert!(cfg.external.isolated());
        assert_eq!(cfg.external.host_ip, "172.30.199.199");
        assert_eq!(cfg.external.ip_start, "172.30.199.10");
        // voxel config set auto-creates the table and round-trips.
        let out = set(&d.to_toml(), "external.mode", "isolated").unwrap();
        let out = set(&out, "external.uplink", "igb0").unwrap();
        let cfg = VoxelConfig::from_toml(&out).unwrap();
        assert!(cfg.external.isolated());
        assert_eq!(cfg.external.uplink.as_deref(), Some("igb0"));
        // deny_unknown_fields catches typos.
        assert!(set(&out, "external.uplnk", "igb0").is_err());
    }

    #[test]
    fn cp_commit_strips_prefix_and_variant_suffix() {
        let mut img = Image {
            cp: Some("voxel-cp-43bb5af-rd".into()),
            ..Default::default()
        };
        assert_eq!(img.cp_commit().as_deref(), Some("43bb5af"));
        img.cp = Some("voxel-cp-99a0aec".into());
        assert_eq!(img.cp_commit().as_deref(), Some("99a0aec"));
        // Falls back to voxel-cp-<version> when cp is unset.
        img.cp = None;
        img.version = "abc1234".into();
        assert_eq!(img.cp_commit().as_deref(), Some("abc1234"));
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Only override the sled count; everything else defaults.
        let cfg = VoxelConfig::from_toml(indoc! {"
            [topology]
            sleds = 3
        "})
        .unwrap();
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
                (0, "g0", true),
                (0, "g1", false),
                (0, "g2", true),
                (1, "g3", true),
                (1, "g4", false),
                (1, "g5", true),
            ]
        );
        assert!(s.iter().all(|d| d.rss), "all 3 sleds per rack join RSS");
        // Per-rack addressing offset: rack 1 shifts the customer/service nets.
        let net = Network::default();
        assert_eq!(net.for_rack(0).infra_prefix, "198.51.100.0/24");
        assert_eq!(net.for_rack(1).infra_prefix, "198.51.101.0/24");
        assert_eq!(net.for_rack(1).service_pool_first, "198.51.101.20");
        assert_eq!(net.for_rack(1).external_dns_ips[0], "198.51.101.20");
        // Shared /48 (fd00:17:1::/48), per-rack /56: rack 0 = d00, rack 1 = e00.
        assert_eq!(net.for_rack(0).rack_subnet, "fd00:17:1:d00::/56");
        assert_eq!(net.for_rack(1).rack_subnet, "fd00:17:1:e00::/56");
        assert_eq!(net.for_rack(1).bgp_asn, 65001);
        // The uplink peer_asn tracks the rack's local ASN (rack 0 unchanged).
        assert_eq!(net.for_rack(0).uplinks[0].peer_asn, 65000);
        assert_eq!(net.for_rack(1).uplinks[0].peer_asn, 65001);
        // Per-rack 1-based DNS zones; single-rack rack 0 is rack1 so a second
        // rack can be added with no DNS churn.
        assert_eq!(net.for_rack(0).dns_zone, "rack1.oxide.test");
        assert_eq!(net.for_rack(1).dns_zone, "rack2.oxide.test");
    }

    #[test]
    fn interconnects_auto_mesh_cross_rack() {
        // Single rack: no cross-rack interconnects.
        assert!(Topology::default().interconnect_pairs().is_empty());

        // 2 racks x 3 sleds -> scrimlets g0,g2 (rack0), g3,g5 (rack1). Full
        // cross-rack mesh: every rack-0 scrimlet <-> every rack-1 scrimlet.
        let t = Topology { racks: 2, sleds: 3, ..Topology::default() };
        assert_eq!(
            t.interconnect_pairs(),
            vec![(0, 3), (0, 5), (2, 3), (2, 5)]
        );
        // Each scrimlet is on (other-rack scrimlet count) = 2 links; g1 on none.
        assert_eq!(t.interconnect_count_for(0), 2);
        assert_eq!(t.interconnect_count_for(3), 2);
        assert_eq!(t.interconnect_count_for(1), 0); // a non-scrimlet sled
    }

    #[test]
    fn static_fanout_matches_datacenter_scheme() {
        let mut cfg = VoxelConfig::default(); // 2 switches, cr1 + cr2
        cfg.network.router_mode = RouterMode::Static;
        // Transit /30s (gateway .1, sidecar .2 per block), datacenter.png layout.
        let n = &cfg.network;
        assert_eq!(
            n.transit_slash30(0),
            Some(("198.51.101.1".into(), "198.51.101.2".into()))
        );
        assert_eq!(
            n.transit_slash30(1),
            Some(("198.51.101.5".into(), "198.51.101.6".into()))
        );
        assert_eq!(
            n.transit_slash30(2),
            Some(("198.51.101.9".into(), "198.51.101.10".into()))
        );
        assert_eq!(
            n.transit_slash30(3),
            Some(("198.51.101.13".into(), "198.51.101.14".into()))
        );
        // per-rack /24 offset.
        assert_eq!(
            n.for_rack(1).transit_slash30(0),
            Some(("198.51.102.1".into(), "198.51.102.2".into()))
        );

        // Infra address lot spans the 4 uplink /30s (matches a4x2's .1 .. .15).
        assert_eq!(
            n.infra_ip_range(4),
            Some((
                "198.51.101.1".parse().unwrap(),
                "198.51.101.15".parse().unwrap()
            ))
        );
        assert_eq!(
            n.for_rack(1).infra_ip_range(4),
            Some((
                "198.51.102.1".parse().unwrap(),
                "198.51.102.15".parse().unwrap()
            ))
        );
        assert_eq!(n.infra_ip_range(0), None);

        // Sidecar side: every switch fans out to both routers (block = c*n_sc + sc).
        let ports = cfg.uplink_ports(0);
        assert_eq!(ports.len(), 4);
        let find = |sw: &str, port: &str| {
            ports
                .iter()
                .find(|p| p.switch == sw && p.port == port)
                .unwrap()
                .clone()
        };
        assert_eq!(find("switch0", "qsfp0").sidecar_addr, "198.51.101.2/30"); // sc0 -> cr1
        assert_eq!(find("switch0", "qsfp1").sidecar_addr, "198.51.101.10/30"); // sc0 -> cr2
        assert_eq!(find("switch1", "qsfp0").sidecar_addr, "198.51.101.6/30"); // sc1 -> cr1
        assert_eq!(find("switch1", "qsfp1").sidecar_addr, "198.51.101.14/30"); // sc1 -> cr2
        assert_eq!(find("switch0", "qsfp1").gateway, "198.51.101.9");

        // Router side: cr1 -> both scrimlets (.1, .5); cr2 -> both (.9, .13).
        // Default is plain static routes (no BFD, matching a4x2).
        let frr = cfg.to_frr();
        let cr = |name: &str| {
            frr.iter().find(|(n, _)| n == name).unwrap().1.render()
        };
        let cr1 = cr("cr1");
        assert!(cr1.contains("ip address 198.51.101.1/30"));
        assert!(cr1.contains("ip address 198.51.101.5/30"));
        assert!(cr1.contains("ip route 198.51.100.0/24 198.51.101.2\n"));
        assert!(cr1.contains("ip route 198.51.100.0/24 198.51.101.6\n"));
        assert!(!cr1.contains(" bfd"));
        assert!(cr1.contains("redistribute static"));
        let cr2 = cr("cr2");
        assert!(cr2.contains("ip address 198.51.101.9/30"));
        assert!(cr2.contains("ip address 198.51.101.13/30"));
        // ce keeps unnumbered eBGP (no static uplinks).
        let ce = &frr.iter().find(|(n, _)| n == "ce").unwrap().1;
        assert!(ce.static_uplinks.is_empty());

        // With transit_bfd on, routes are BFD-tracked and peers appear.
        cfg.network.transit_bfd = true;
        let cr1b =
            cfg.to_frr().iter().find(|(n, _)| n == "cr1").unwrap().1.render();
        assert!(cr1b.contains("ip route 198.51.100.0/24 198.51.101.2 bfd"));
        assert!(cr1b.contains("peer 198.51.101.2"));
    }

    #[test]
    fn frr_transit_peers_every_scrimlet_across_racks() {
        // Single rack: cr1 peers ce + the rack's 2 scrimlets (g0,g3), edge first.
        let cfg = VoxelConfig::from_toml(indoc! {"
            [topology]
            sleds = 4
        "})
        .unwrap();
        let frr = cfg.to_frr();
        let cr1 = &frr.iter().find(|(n, _)| n == "cr1").unwrap().1;
        assert_eq!(cr1.asn, 65101);
        let ifaces: Vec<&str> =
            cr1.neighbors.iter().map(|n| n.interface.as_str()).collect();
        assert_eq!(ifaces, vec!["enp0s8", "enp0s9", "enp0s10"]); // ce, g0, g3
        assert!(cr1.originate4.is_empty(), "transit originates nothing");

        // a3x2x2: cr1 peers ce + all 4 scrimlets (rack0 g0,g2; rack1 g3,g5).
        let cfg = VoxelConfig::from_toml(indoc! {"
            [topology]
            racks = 2
            sleds = 3
        "})
        .unwrap();
        let frr = cfg.to_frr();
        let cr1 = &frr.iter().find(|(n, _)| n == "cr1").unwrap().1;
        let ifaces: Vec<&str> =
            cr1.neighbors.iter().map(|n| n.interface.as_str()).collect();
        assert_eq!(
            ifaces,
            vec!["enp0s8", "enp0s9", "enp0s10", "enp0s11", "enp0s12"]
        );
        // Descriptions carry the rack/switch identity for each peered scrimlet.
        let descs: Vec<&str> =
            cr1.neighbors.iter().map(|n| n.description.as_str()).collect();
        assert_eq!(descs[1], "to g0 (rack0 switch0)");
        assert_eq!(descs[2], "to g2 (rack0 switch1)");
        assert_eq!(descs[3], "to g3 (rack1 switch0)");
        assert_eq!(descs[4], "to g5 (rack1 switch1)");
        // ce still peers both fabric routers and originates the default.
        let ce = &frr.iter().find(|(n, _)| n == "ce").unwrap().1;
        assert_eq!(
            ce.neighbors
                .iter()
                .map(|n| n.interface.as_str())
                .collect::<Vec<_>>(),
            vec!["enp0s8", "enp0s9"]
        );
        assert_eq!(ce.originate4, vec!["0.0.0.0/0".to_string()]);
    }

    #[test]
    fn bootstrap_addr_is_decimal_past_four_sleds() {
        // index 5 -> 11 must render decimal, not hex b.
        let sleds = Topology { sleds: 6, ..Topology::default() }.sleds();
        assert_eq!(sleds[5].bootstrap_addr(), "fdb0:a840:2500:11::1");
        assert_eq!(sleds[4].bootstrap_addr(), "fdb0:a840:2500:9::1");
    }

    #[test]
    fn scrimlets_come_from_the_rss_set() {
        // 5 sleds, 4 in RSS, g4 added post-init: switch zones must land on
        // RSS sleds.
        let cfg = VoxelConfig::from_toml(indoc! {"
            [topology]
            sleds = 5
            rss_sleds = 4
        "})
        .unwrap();
        let sleds = cfg.sleds();
        let scrimlets: Vec<&str> = sleds
            .iter()
            .filter(|s| s.scrimlet)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(scrimlets, ["g0", "g3"]);
        assert!(sleds.iter().filter(|s| s.scrimlet).all(|s| s.rss));
        assert_eq!(sleds.len(), 5);
        assert!(!sleds[4].rss && !sleds[4].scrimlet);
        assert!(cfg.topology.validate().is_ok());

        // Explicit scrimlets can still name a non-RSS sled; validate rejects.
        let cfg = VoxelConfig::from_toml(indoc! {r#"
            [topology]
            sleds = 5
            scrimlets = ["g0", "g4"]
            rss_sleds = 4
        "#})
        .unwrap();
        let err = cfg.topology.validate().unwrap_err();
        assert!(err.contains("g4"), "{err}");
    }

    #[test]
    fn three_node_drops_rss_membership() {
        let cfg = VoxelConfig::from_toml(indoc! {"
            [topology]
            sleds = 4
            rss_sleds = 3
        "})
        .unwrap();
        let rss: Vec<_> = cfg.sleds().into_iter().filter(|s| s.rss).collect();
        // Only the first 3 sleds join RSS; the 4th is dropped from the bootstrap set.
        assert_eq!(rss.len(), 3);
        assert!(rss.iter().all(|s| s.index < 3));
    }

    #[test]
    fn node_ip_arithmetic() {
        let x = External::default();
        assert_eq!(x.prefix_length(), Some(24));
        assert_eq!(x.node_ip(0).as_deref(), Some("172.30.199.10"));
        assert_eq!(x.node_ip(5).as_deref(), Some("172.30.199.15"));
        // Rolls past host_ip (.199) - skipped, not silently reused.
        let hit_host = External {
            ip_start: "172.30.199.199".into(),
            ..External::default()
        };
        assert_eq!(hit_host.node_ip(0), None);
        let hit_bcast = External {
            ip_start: "172.30.199.255".into(),
            ..External::default()
        };
        assert_eq!(hit_bcast.node_ip(0), None);
    }

    #[test]
    fn external_addresses_stay_within_subnet() {
        let x = External {
            subnet: "10.20.0.0/16".into(),
            host_ip: "10.20.1.1".into(),
            ip_start: "10.20.0.255".into(),
            ..External::default()
        };
        // Unlike the old last-octet check, .255 is usable in a /16.
        assert_eq!(x.node_ip(0).as_deref(), Some("10.20.0.255"));

        let outside_gateway =
            External { host_ip: "10.21.1.1".into(), ..x.clone() };
        assert!(!outside_gateway.host_ip_is_usable());
        assert_eq!(outside_gateway.node_ip(0), None);

        let small = External {
            subnet: "192.0.2.0/29".into(),
            host_ip: "192.0.2.6".into(),
            ip_start: "192.0.2.2".into(),
            ..External::default()
        };
        assert!(small.host_ip_is_usable());
        assert_eq!(small.node_ip(0).as_deref(), Some("192.0.2.2"));
        assert_eq!(small.node_ip(5), None); // 192.0.2.7 is broadcast.
    }

    #[test]
    fn static_external_ips_orders_sleds_then_routers() {
        // Default 4-sled, 3-router topology.
        let cfg = VoxelConfig::default();
        let ips = cfg.static_external_ips();
        let expected: Vec<(String, String)> = vec![
            ("g0", "172.30.199.10"),
            ("g1", "172.30.199.11"),
            ("g2", "172.30.199.12"),
            ("g3", "172.30.199.13"),
            ("ce", "172.30.199.14"),
            ("cr1", "172.30.199.15"),
            ("cr2", "172.30.199.16"),
        ]
        .into_iter()
        .map(|(n, i)| (n.to_string(), i.to_string()))
        .collect();
        assert_eq!(ips, expected);
    }

    #[test]
    fn builder_net_is_host_ip_minus_one() {
        let x = External::default();
        assert_eq!(
            x.builder_net().as_deref(),
            Some("172.30.199.198/24 172.30.199.199")
        );

        let first_usable_gateway = External {
            subnet: "192.0.2.0/24".into(),
            host_ip: "192.0.2.1".into(),
            ..External::default()
        };
        assert_eq!(first_usable_gateway.builder_net(), None);

        let outside_gateway =
            External { host_ip: "198.51.100.1".into(), ..first_usable_gateway };
        assert_eq!(outside_gateway.builder_net(), None);
    }

    #[test]
    fn router_ext_iface_default_topology() {
        // 4 sleds -> 2 scrimlets; routers = [ce, cr1, cr2] -> 2 fabric routers.
        // ce external NIC = enp0s{8 + 2} = enp0s10.
        // cr1/cr2 external NIC = enp0s{8 + 1 + 2} = enp0s11.
        let cfg = VoxelConfig::default();
        assert_eq!(cfg.router_ext_iface("ce"), "enp0s10");
        assert_eq!(cfg.router_ext_iface("cr1"), "enp0s11");
        assert_eq!(cfg.router_ext_iface("cr2"), "enp0s11");
    }

    #[test]
    fn router_ext_iface_multi_rack() {
        // 2 racks * 3 sleds -> 4 scrimlets total and 2 fabric routers.
        // ce external = enp0s{8 + 2} = enp0s10.
        // cr1/cr2 external = enp0s{8 + 1 + 4} = enp0s13.
        let cfg = VoxelConfig::from_toml(indoc! {"
            [topology]
            racks = 2
            sleds = 3
        "})
        .unwrap();
        assert_eq!(cfg.router_ext_iface("ce"), "enp0s10");
        assert_eq!(cfg.router_ext_iface("cr1"), "enp0s13");
        assert_eq!(cfg.router_ext_iface("cr2"), "enp0s13");
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
