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

use crate::frr::{FrrNeighbor, FrrRouter, StaticUplink};

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
///
/// NOTE: every `enp0sN` derivation here assumes propolis assigns virtio PCI
/// slots in falcon link-creation order, which systemd then names by PCI
/// geography (`enp0s<slot>`).
///
/// Slot 8 is not a falcon constant. Falcon starts its slot counter at 5 and
/// spends one slot per p9fs mount, then two on the softnpu p9 and pci-port
/// pair, before the first NIC, so 8 holds only while a router carries exactly
/// one cargo-bay mount. That softnpu pair is spent on every node in a
/// deployment that has any softnpu link at all, not just the switch nodes, so
/// the routers pay for it too. Falcon also appends a node's external link after
/// all of its point-to-point links, which is what puts the external NIC last
/// regardless of `build_topo`'s call order.
///
/// See `Deployment::nodes_preflight` and `Node::preflight` in falcon's
/// `lib/src/lib.rs`. This is a contract voxel relies on but does not control,
/// so `voxel-init` checks the staged name against the node's actual links.
const FRR_IFACE_BASE: usize = 8;

// This is a proper V2 serial-number prefix
//
// All serial numbers must be 8 characters and start with the number '2' in
// ascii. We use a 7 character prefix to allow appending a numeral. If we ever
// require more than 9 sleds in a deployment we can change our generation to
// take into account the last two 0s here.
pub const SLED_SERIAL_PREFIX: &str = "2FAKE00";

/// A hardcoded part number for all fake sleds
pub const SLED_PART_NUMBER: &str = "913-0000019";

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
    /// Omitted from serialized output while untouched, so a plain LAN rack
    /// doesn't grow a section the operator never set.
    #[serde(default, skip_serializing_if = "External::is_default")]
    pub external: External,
}

/// Provisioning mode for the nodes' external (host-LAN) links.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExternalMode {
    /// Wire external VNICs onto an existing LAN (the host's default-route
    /// interface, or `$EXT_INTERFACE`) that provides DHCP.
    ///
    /// The default.
    #[default]
    Lan,
    /// Voxel-managed isolated segment.
    ///
    /// A host etherstub with a static host address, NAT out `uplink`,
    /// and static per-node addresses staged into each node's cargo-bay
    /// (no DHCP server).
    Isolated,
}

/// The rack's external segment, i.e., the host-LAN side every node's external
/// NIC lands on. `lan` (default) uses an existing network, while `isolated`
/// has voxel stand the segment up itself at launch, replicating option 2
/// ("external" network that only exists on the test machine) of omicron's
/// [how-to-run external networking] guide: the host owns `host_ip` on an
/// etherstub, NATs `subnet` out `uplink`, and every node gets a deterministic
/// static address from `ip_start` staged into its cargo-bay for voxel-init
/// to apply.
///
/// The nodes' addresses stay in use after bring-up (RSS progress is polled
/// over SSH to them and each router NATs rack egress out its own external
/// address), which is why the segment must exist before launch.
///
/// This is host-only plumbing and never reaches the rack's RSS config.
///
/// [how-to-run external networking]: https://github.com/oxidecomputer/omicron/blob/main/docs/how-to-run.adoc#external-networking
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct External {
    /// `lan` (default; existing behavior) or `isolated`.
    pub mode: ExternalMode,
    /// Physical link the isolated subnet NATs out of (e.g. `igb0`). Required
    /// in isolated mode, and validated before use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uplink: Option<String>,
    /// The isolated segment's subnet.
    pub subnet: String,
    /// Host address on the etherstub and the nodes' default gateway.
    ///
    /// Traffic arriving here is forwarded to `uplink`, where ipnat rewrites it.
    pub host_ip: String,
    /// First static node address.
    ///
    /// Nodes are numbered contiguously from here in `sleds()` order, then in
    /// `topology.routers` order.
    pub ip_start: String,
    /// Nameservers handed to the nodes.
    pub dns: Vec<String>,
    /// Etherstub MTU.
    ///
    /// Launch refuses 9000 or above: voxel-init classifies a sled NIC as
    /// underlay iff it accepts mtu=9000, so the external link has to reject
    /// jumbo for classification to work. The 1500 default mirrors a physical
    /// external network.
    ///
    /// Raising it (e.g. to 8900) exercises jumbo external ingress, which only
    /// matters for external-to-external forwarding through the switch, whereas
    /// guest delivery is capped by the VPC MTU regardless.
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
    /// This is `true` when the section is untouched, i.e., when a section is
    /// omitted from serialized output.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether or not voxel manages an isolated external segment.
    pub fn isolated(&self) -> bool {
        self.mode == ExternalMode::Isolated
    }

    /// Prefix length parsed from `subnet` (`a.b.c.d/N`).
    ///
    /// Returns `None` if `subnet` is not parseable CIDR.
    pub fn prefix_length(&self) -> Option<u32> {
        Some(u32::from(self.subnet_net()?.width()))
    }

    /// `subnet` parsed as an `Ipv4Net`, or `None` if it is not CIDR.
    fn subnet_net(&self) -> Option<oxnet::Ipv4Net> {
        self.subnet.parse().ok()
    }

    /// Whether `ip` is a usable host address inside `subnet` (strictly between
    /// its network and broadcast addresses). oxnet treats /31 and /32 as
    /// all-host subnets (RFC 3021), but the segment needs distinct host,
    /// builder, and node addresses, so those widths have no usable range here.
    fn ip_is_usable(&self, ip: std::net::Ipv4Addr) -> bool {
        self.subnet_net()
            .is_some_and(|net| match (net.network(), net.broadcast()) {
                (Some(network), Some(broadcast)) => {
                    net.contains(ip) && ip != network && ip != broadcast
                }
                _ => false,
            })
    }

    /// Whether `host_ip` parses and is a usable address inside `subnet`.
    pub fn host_ip_is_usable(&self) -> bool {
        self.host_ip.parse().is_ok_and(|ip| self.ip_is_usable(ip))
    }

    /// Static address for the `nth` node (0-based): `ip_start + nth`.
    ///
    /// Returns `None` if the address would step past the subnet's broadcast,
    /// land on the network or broadcast address, collide with `host_ip`, or
    /// if `subnet`, `host_ip`, or `ip_start` are invalid.
    pub fn node_ip(&self, nth: usize) -> Option<String> {
        let start: std::net::Ipv4Addr = self.ip_start.parse().ok()?;
        let host: std::net::Ipv4Addr = self.host_ip.parse().ok()?;
        if !self.ip_is_usable(host) {
            return None;
        }
        let base = u32::from(start).checked_add(nth as u32)?;
        let ip = std::net::Ipv4Addr::from(base);
        // Refuse the network + broadcast boundaries and anything outside the
        // subnet (an operator-set ip_start plus a large rack can overrun it).
        if !self.ip_is_usable(ip) {
            return None;
        }
        if ip == host {
            return None;
        }
        Some(ip.to_string())
    }

    /// A builder for the VM address: `host_ip - 1`. This is used by the image
    /// build path to give the builder a fixed static address on the isolated
    /// segment. Same prefix length as `subnet`.
    ///
    /// Returns `None` if `host_ip` is not usable within `subnet`, or if the
    /// derived builder address would underflow or land on a reserved boundary.
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

/// falcon/runtime settings (zfs dataset, project workdir, image build root).
/// Each optional, resolved as flag > voxel.toml > env > built-in default.
/// The omicron checkout path is derived from image.cp's commit in
/// resolve_falcon_env; $VOXEL_OMICRON_SRC overrides.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Falcon {
    /// zfs dataset (falcon's `FALCON_DATASET`). `None` -> env, else `rpool/falcon`.
    pub dataset: Option<String>,
    /// Project root that `cargo-bay/` and `.falcon/` live under. Absolute; lets
    /// `voxel` run from anywhere (e.g. installed in `/usr/bin`). `None` -> the
    /// directory containing this `voxel.toml`.
    pub workdir: Option<String>,
    /// Build root for `voxel image create` (the omicron checkouts live here).
    /// Exported as `BUILD_ROOT`. `None` -> env, else the `$HOME/voxel-builds`
    /// default. Lets a non-root user build images outside `/root`.
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

    /// The computed sled set (replaces the old `SLEDS` const).
    pub fn sleds(&self) -> Vec<SledDesc> {
        self.topology.sleds()
    }

    /// Gather every node's static external address (isolated mode's single
    /// source of assignment).
    ///
    /// Sleds first (in `sleds()` order), then routers (in `topology.routers`
    /// order). The list truncates at the first address `node_ip` refuses
    /// (overflow past `.254` or a `host_ip` collision).
    pub fn static_external_ips(&self) -> Vec<(String, String)> {
        self.sleds()
            .into_iter()
            .map(|s| s.name)
            .chain(self.topology.routers.iter().cloned())
            .enumerate()
            .map_while(|(n, name)| self.external.node_ip(n).map(|ip| (name, ip)))
            .collect()
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
        (0..self.racks())
            .flat_map(|r| self.scrimlet_names_for_rack(r))
            .collect()
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

    /// Cross-rack sidecar interconnect pairs, as GLOBAL scrimlet index pairs
    /// (`ai < bi`): every scrimlet links to every scrimlet in a DIFFERENT rack
    /// (full cross-rack mesh), directly meshing a multi-rack deployment's sidecars
    /// for the shared-/48 underlay. Empty for a single rack. Each pair is a
    /// `softnpu_links` sidecar<->sidecar (see `topo::build_topo`) and adds one
    /// front port to both endpoints.
    pub fn interconnect_pairs(&self) -> Vec<(usize, usize)> {
        let sleds = self.sleds();
        let scrimlets: Vec<&SledDesc> = sleds.iter().filter(|s| s.scrimlet).collect();
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

    /// How many interconnects scrimlet `index` participates in (its front-port bump).
    pub fn interconnect_count_for(&self, index: usize) -> usize {
        self.interconnect_pairs()
            .iter()
            .filter(|(a, b)| *a == index || *b == index)
            .count()
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
    /// Part number of a given `BaseboardId`
    pub part_number: String,
    /// Serial number of a given `BaseboardId`
    pub serial_number: String,
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
        disks: SledDisksSchema,
    ) -> crate::sled::SledAgentConfig {
        crate::sled::SledAgentConfig::new(self.index, self.scrimlet)
            .with_topology(num_sleds, num_fabric_routers)
            .with_data_links_schema(data_links)
            .with_disks_schema(disks)
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
    /// Override the sled-agent `data_links` config shape. Normally leave this
    /// unset (`None`): voxel auto-detects it from the image's omicron source at
    /// launch. Set it only to force a shape. See [`SledDataLinksSchema`].
    pub data_links_schema: Option<SledDataLinksSchema>,
    /// Override the sled-agent disks config shape (`vdevs` vs `external_disks`).
    /// Normally leave unset (`None`) - auto-detected per image. See
    /// [`SledDisksSchema`].
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

/// The shape of sled-agent's disk config, which changed across omicron versions:
/// the flat `vdevs = [...]` list became a tagged `external_disks` enum. Selected
/// independently of [`SledDataLinksSchema`] (they drifted at different commits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SledDisksSchema {
    /// Pre-rename omicron (e.g. a3fee0ec / 43bb5af / 99a0aec):
    /// `vdevs = ["m2_g0_0.vdev", ...]`.
    #[default]
    Vdevs,
    /// The `ExternalDisks` enum, `#[serde(tag = "kind")]`, while it still had a
    /// `Virtual` variant (e.g. cc07512e0):
    /// `external_disks = { kind = "virtual", vdevs = ["m2_g0_0.vdev", ...] }`.
    ExternalDisks,
    /// omicron main, after `Virtual { vdevs }` and `HardcodedPhysical { disks }`
    /// merged into one variant:
    /// `external_disks = { kind = "hardcoded", vdevs = [...], disks = [] }`.
    Hardcoded,
}

impl Image {
    pub fn cp_image(&self) -> String {
        self.cp
            .clone()
            .unwrap_or_else(|| format!("voxel-cp-{}", self.version))
    }

    /// The omicron commit encoded in the cp image name (`voxel-cp-<commit>` with
    /// an optional `-<variant>` suffix like `-rd`). Used to locate the matching
    /// checkout under `<build_root>/omicron-<commit>`. `None` if the name
    /// doesn't follow the `voxel-cp-` convention.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RouterMode {
    /// Unnumbered eBGP (default).
    #[default]
    Bgp,
    /// Numbered /30 uplinks with static routes and BFD.
    Static,
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
    /// Upstream routing mode.
    pub router_mode: RouterMode,
    /// IPv4 /24 carved into per-uplink /30s for `Static` mode (.1 router, .2 sidecar).
    pub transit_prefix: String,
    /// `Static` mode: BFD-track the transit routes (FRR `ip route ... bfd` +
    /// peers + rss BFD). Requires a dataplane where softnpu BFD establishes;
    /// a4x2 ships this off and uses plain static routes, so it defaults off.
    pub transit_bfd: bool,
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

/// Offset an IPv6 rack subnet by `rack` /56s WITHIN its /48 (bits 48-55, the high
/// byte of hextet 3), so every rack shares one /48 AZ (omicron's AZ=/48, rack=/56
/// scheme) and cross-rack underlay is a single aggregate prefix. rack 0 is
/// unchanged. Preserves any `/prefix`. Returns the input unchanged if it doesn't
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
        // Rack index as an ASN offset; saturating so an absurd rack count can't
        // wrap an ASN back onto a lower rack's. (racks() is tiny in practice.)
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
            // The uplink's `peer_asn` is the switch's *local* BGP ASN for that
            // session (it references a `[[bgp]]` entry, whose `asn` is offset
            // above); the actual customer-router ASN is auto-discovered via
            // unnumbered `remote-as external`. So it must track the rack's ASN -
            // otherwise rack 1's peer config names an ASN with no `[[bgp]]` block.
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

    /// Base address of `transit_prefix` (its `.0`). None if it doesn't parse.
    fn transit_base(&self) -> Option<std::net::Ipv4Addr> {
        self.transit_prefix.split('/').next()?.parse().ok()
    }

    /// The `/30` for transit `block` (0-based), carved from `transit_prefix`:
    /// `(router gateway, sidecar)` as bare IPv4 (`.1` router, `.2` sidecar, the
    /// a4x2 scheme). None if `transit_prefix` doesn't parse.
    pub fn transit_slash30(&self, block: usize) -> Option<(String, String)> {
        let block = u32::try_from(block).ok()?;
        let b = u32::from(self.transit_base()?).checked_add(block.checked_mul(4)?)?;
        let gateway = std::net::Ipv4Addr::from(b.checked_add(1)?);
        let sidecar = std::net::Ipv4Addr::from(b.checked_add(2)?);
        Some((gateway.to_string(), sidecar.to_string()))
    }

    /// The transit `/30` for the uplink from fabric router `router_index` (0-based)
    /// to the switch in slot `switch_slot`, with `n_switches` per rack. Block =
    /// `router*n_switches + slot`. The single source both the sidecar side
    /// (`uplink_ports`) and router side (`to_frr`) use, so their /30s always agree.
    pub fn transit_slash30_for(
        &self,
        router_index: usize,
        switch_slot: usize,
        n_switches: usize,
    ) -> Option<(String, String)> {
        self.transit_slash30(router_index * n_switches + switch_slot)
    }

    /// `Static`-mode infra address lot `(first, last)`, spanning the `nblocks`
    /// per-uplink `/30`s from `transit_prefix`. Every numbered switch-port address
    /// must fall inside it or Nexus rejects the handoff ("address not in lot").
    /// Range `.1` to `.{nblocks*4 - 1}` (a4x2 uses `.1`/`.15` for 2x2). None if
    /// `transit_prefix` doesn't parse or `nblocks` is 0.
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

/// One generated scrimlet uplink port toward a specific fabric router (the
/// 2-way fanout: every switch gets one port per fabric router). Derived, not a
/// config knob; consumed by voxel's RSS request builder (sidecar side) and
/// mirrored by `to_frr` (router side).
#[derive(Debug, Clone, PartialEq)]
pub struct UplinkPort {
    pub switch: String,
    pub switch_slot: usize,
    pub router_index: usize,
    /// `qsfp{router_index}` (fabric uplinks take the first front ports, in
    /// router link-creation order; see `build_topo`).
    pub port: String,
    pub peer_asn: u32,
    pub router_lifetime: u16,
    pub port_speed: String,
    pub lldp: String,
    /// `Static`-mode sidecar side, `addr/30`.
    pub sidecar_addr: String,
    /// `Static`-mode router side (nexthop + BFD peer), bare addr.
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

// ---------------------------------------------------------------------------
// Generation. The FRR router configs are generated here; the RSS
// `config-rss.toml` in voxel's rss_request module.
// ---------------------------------------------------------------------------

impl VoxelConfig {
    /// Number of fabric (transit) routers, i.e. routers other than `ce`.
    fn fabric_router_count(&self) -> usize {
        self.topology
            .routers
            .iter()
            .filter(|r| r.as_str() != "ce")
            .count()
    }

    /// Number of scrimlets (switches) in `rack`.
    fn scrimlets_in_rack(&self, rack: usize) -> usize {
        self.sleds()
            .into_iter()
            .filter(|s| s.scrimlet && s.rack == rack)
            .count()
    }

    /// Generated uplink ports for `rack` (0-based): every switch fans out to
    /// every fabric router (`qsfp{router}`), so a switch reaches upstream via
    /// either router. `Static`-mode /30 addressing follows the datacenter
    /// scheme: block = `router * n_switches + switch`, `.1` router / `.2`
    /// sidecar. One `[[network.uplinks]]` entry per switch supplies the shared
    /// per-port settings (asn, speed, ...).
    pub fn uplink_ports(&self, rack: usize) -> Vec<UplinkPort> {
        let net = self.network.for_rack(rack);
        let n_cr = self.fabric_router_count();
        let n_sc = self.scrimlets_in_rack(rack);
        let mut out = Vec::new();
        for (sc, u) in net.uplinks.iter().enumerate() {
            for c in 0..n_cr {
                let (gateway, sidecar) = net.transit_slash30_for(c, sc, n_sc).unwrap_or_default();
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

    /// Cross-rack interconnect ports on `rack`'s switches, as `(switch, port)`.
    /// Each cross-rack sidecar link (see `Topology::interconnect_pairs`) lands on
    /// a front port after the fabric uplinks - `qsfp{n_cr + k}`, in
    /// `interconnect_pairs` order, matching `build_topo`'s per-switch front-port
    /// assignment. Emitted as link-local (`AddrConf`) cluster ports so
    /// DDM can run the cross-rack underlay over the mesh. Empty for a single rack.
    pub fn interconnect_ports(&self, rack: usize) -> Vec<(String, String)> {
        let n_cr = self.fabric_router_count();
        let pairs = self.topology.interconnect_pairs();
        let sleds = self.sleds();
        let scrimlets: Vec<&SledDesc> = sleds.iter().filter(|s| s.scrimlet).collect();
        let mut out = Vec::new();
        for (slot, s) in scrimlets.iter().filter(|s| s.rack == rack).enumerate() {
            let mut k = 0;
            for (a, b) in &pairs {
                if *a == s.index || *b == s.index {
                    out.push((format!("switch{slot}"), format!("qsfp{}", n_cr + k)));
                    k += 1;
                }
            }
        }
        out
    }

    /// The `enp0sN` name of a router's external (host-LAN) NIC.
    ///
    /// This is derived rather than discovered, for two reasons: the consumer
    /// needs the name before the node exists because `stage_config` writes it
    /// into the router's cargo-bay `external-net` file at launch, so voxel-init
    /// has an interface to place the static address on.
    ///
    /// Isolated mode leaves the router with nothing to discover from, i.e., it
    /// runs no DHCP server, so the default-route poll `lan` mode relies on
    /// never resolves, and the sleds' jumbo probe (the underlay is MTU 9000,
    /// the external NICs are not) has no router-side equivalent.
    ///
    /// The derivation mirrors the wiring in `build_topo`: `ce`'s falcon links
    /// go fabric-router first (cr1=enp0s8, cr2=enp0s9, ...) then its external
    /// NIC, so `ce`'s external NIC is
    /// `enp0s{FRR_IFACE_BASE + fabric_router_count}`. A fabric router's falcon
    /// links go `ce` (enp0s8) then every scrimlet across every rack in
    /// `sleds()` order (enp0s9, enp0s10, ...) then its external NIC, so a
    /// fabric router's external NIC is
    /// `enp0s{FRR_IFACE_BASE + 1 + total_scrimlet_count}`. Encoded here so it
    /// cannot drift from `to_frr`'s link ordering. The residual risk is the
    /// base itself: falcon assigns NIC slots sequentially after its fixed
    /// pre-NIC devices, so a falcon change that adds a device ahead of the
    /// NICs shifts every derived name (see the note on [`FRR_IFACE_BASE`]).
    /// Nothing here can catch that, so `voxel-init` checks the staged name
    /// against the router's actual links at bring-up and reports the mismatch.
    pub fn router_ext_iface(&self, router: &str) -> String {
        let fabric_router_count = self
            .topology
            .routers
            .iter()
            .filter(|r| r.as_str() != "ce")
            .count();
        let total_scrimlet_count = self.sleds().into_iter().filter(|s| s.scrimlet).count();
        let n = if router == "ce" {
            FRR_IFACE_BASE + fabric_router_count
        } else {
            FRR_IFACE_BASE + 1 + total_scrimlet_count
        };
        format!("enp0s{n}")
    }

    /// Build each customer router's `frr.conf` as `(name, FrrRouter)` pairs.
    /// `cr*` are the **shared transit**: each peers `ce` plus *every* scrimlet
    /// across *all* racks, and originates nothing. eBGP (`no bgp
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
        let fabric: Vec<&String> = self
            .topology
            .routers
            .iter()
            .filter(|r| r.as_str() != "ce")
            .collect();
        // Scrimlets across all racks, in falcon softnpu-link order (= `sleds()`
        // order), each labelled with its rack + in-rack switch slot.
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
                        FrrNeighbor::new(format!("enp0s{}", FRR_IFACE_BASE + k), format!("to {r}"))
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
                let ce_nb = FrrNeighbor::new(format!("enp0s{FRR_IFACE_BASE}"), "to ce");
                match self.network.router_mode {
                    RouterMode::Bgp => {
                        let mut neighbors = vec![ce_nb];
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
                            static_uplinks: vec![],
                            track_bfd: false,
                        }
                    }
                    // 2-way fanout: this cr has a numbered /30 to EVERY scrimlet
                    // (matching the physical mesh), so both routers reach every
                    // switch. Block = router*n_switches + slot (the datacenter
                    // scheme). Keeps eBGP to ce, redistributing the static routes.
                    RouterMode::Static => {
                        let c = (cr_index - 1) as usize;
                        let mut static_uplinks = Vec::new();
                        for (k, (_, rack, slot)) in scrimlets.iter().enumerate() {
                            let net = self.network.for_rack(*rack);
                            let n_sc = self.scrimlets_in_rack(*rack);
                            if let Some((gateway, sidecar)) =
                                net.transit_slash30_for(c, *slot, n_sc)
                            {
                                static_uplinks.push(StaticUplink {
                                    interface: format!("enp0s{}", FRR_IFACE_BASE + 1 + k),
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

// ---------------------------------------------------------------------------
// `config get` / `config set` - format-preserving edits over `voxel.toml`
// text (comments + layout survive), validated against the typed model.
// ---------------------------------------------------------------------------

/// Read a dotted key (`network.bgp_asn`) out of a `voxel.toml`. Returns the
/// value's TOML rendering, or `None` if the path doesn't exist.
pub fn get(doc_text: &str, key: &str) -> Result<Option<String>, String> {
    use toml_edit::{DocumentMut, Item};
    let doc: DocumentMut = doc_text
        .parse()
        .map_err(|e| format!("parse voxel.toml: {e}"))?;
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

    let doc: DocumentMut = doc_text
        .parse()
        .map_err(|e| format!("parse voxel.toml: {e}"))?;

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
        vec![value.parse::<Value>().map_err(|e| {
            format!(
                "{key}: '{value}' is not a valid TOML array/table ({e}); e.g. '[\"g0\", \"g3\"]'"
            )
        })?]
    } else {
        match existing {
            Some(Value::Integer(_)) => vec![
                value
                    .parse::<i64>()
                    .map(Value::from)
                    .map_err(|_| format!("{key} is an integer; '{value}' is not"))?,
            ],
            Some(Value::Boolean(_)) => vec![
                value
                    .parse::<bool>()
                    .map(Value::from)
                    .map_err(|_| format!("{key} is a boolean; '{value}' is not"))?,
            ],
            Some(Value::Float(_)) => vec![
                value
                    .parse::<f64>()
                    .map(Value::from)
                    .map_err(|_| format!("{key} is a float; '{value}' is not"))?,
            ],
            Some(Value::Array(_)) | Some(Value::InlineTable(_)) => {
                return Err(format!(
                    "{key} is a collection; pass a TOML array/table, e.g. '[\"g0\", \"g3\"]'"
                ));
            }
            Some(Value::String(_)) => vec![Value::from(value)],
            // Absent (or an exotic type): infer. A bare TOML scalar (int/bool/
            // float) first, then a string fallback - validation picks the winner.
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
        assert!(
            out.contains("sled_memory_gb = 7"),
            "expected an int, got: {out}"
        );
        assert_eq!(
            VoxelConfig::from_toml(&out)
                .unwrap()
                .topology
                .sled_memory_gb,
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
        // A `[...]` value is parsed as a TOML array, and the result must still
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
        // An auto config leaves `scrimlets` empty and `rss_sleds` 0; the derived
        // values are what the RSS bootstrap set and switch placement come from,
        // so an empty set here would mean "no peers".
        let cfg = VoxelConfig::from_toml("[topology]\nsleds = 4\n").unwrap();
        assert_eq!(
            cfg.topology.scrimlet_names(),
            vec!["g0".to_string(), "g3".to_string()]
        );
        assert_eq!(cfg.topology.rss_count(), 4);
        // Spelling the derived values out explicitly describes the same rack.
        let explicit = VoxelConfig::from_toml(
            "[topology]\nsleds = 4\nscrimlets = [\"g0\", \"g3\"]\nrss_sleds = 4\n",
        )
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
        assert!(
            d.sp.emu_bin.is_none() && d.sp.sidecar_image.is_none() && d.sp.gimlet_image.is_none()
        );
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
    fn external_section_parses_defaults_and_set_round_trips() {
        // Default: `lan` mode. The untouched section is omitted from output.
        let d = VoxelConfig::default();
        assert!(!d.external.isolated());
        assert!(!d.to_toml().contains("[external]"));
        // Populated section parses; unset fields keep the guide defaults.
        let cfg =
            VoxelConfig::from_toml("[external]\nmode = \"isolated\"\nuplink = \"igb0\"\n").unwrap();
        assert!(cfg.external.isolated());
        assert_eq!(cfg.external.host_ip, "172.30.199.199");
        assert_eq!(cfg.external.ip_start, "172.30.199.10");
        // `voxel config set` auto-creates the table and round-trips.
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
        let t = Topology {
            racks: 2,
            sleds: 3,
            ..Topology::default()
        };
        assert_eq!(t.total_sleds(), 6);
        let s = t.sleds();
        assert_eq!(s.len(), 6);
        // rackA = g0,g1,g2 (scrimlets g0,g2); rackB = g3,g4,g5 (scrimlets g3,g5).
        let scr: Vec<(usize, &str, bool)> = s
            .iter()
            .map(|d| (d.rack, d.name.as_str(), d.scrimlet))
            .collect();
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
        // Every rack gets its own 1-based external DNS zone, so silos are
        // addressable per rack (recovery.sys.rack{N}.oxide.test) - including a
        // single-rack deploy, whose rack 0 is rack1 (matches rack 1 of a
        // multi-rack deploy, so it can grow a 2nd rack with no DNS churn).
        assert_eq!(net.for_rack(0).dns_zone, "rack1.oxide.test");
        assert_eq!(net.for_rack(1).dns_zone, "rack2.oxide.test");
    }

    #[test]
    fn interconnects_auto_mesh_cross_rack() {
        // Single rack: no cross-rack interconnects.
        assert!(Topology::default().interconnect_pairs().is_empty());

        // 2 racks x 3 sleds -> scrimlets g0,g2 (rack0), g3,g5 (rack1). Full
        // cross-rack mesh: every rack-0 scrimlet <-> every rack-1 scrimlet.
        let t = Topology {
            racks: 2,
            sleds: 3,
            ..Topology::default()
        };
        assert_eq!(t.interconnect_pairs(), vec![(0, 3), (0, 5), (2, 3), (2, 5)]);
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
        let cr = |name: &str| frr.iter().find(|(n, _)| n == name).unwrap().1.render();
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
        let cr1b = cfg
            .to_frr()
            .iter()
            .find(|(n, _)| n == "cr1")
            .unwrap()
            .1
            .render();
        assert!(cr1b.contains("ip route 198.51.100.0/24 198.51.101.2 bfd"));
        assert!(cr1b.contains("peer 198.51.101.2"));
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
        assert_eq!(
            ifaces,
            vec!["enp0s8", "enp0s9", "enp0s10", "enp0s11", "enp0s12"]
        );
        // Descriptions carry the rack/switch identity for each peered scrimlet.
        let descs: Vec<&str> = cr1
            .neighbors
            .iter()
            .map(|n| n.description.as_str())
            .collect();
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
        // index 5 -> 2*5+1 = 11, which must render as decimal "11" (matching the
        // viona MAC byte sled-agent derives the address from), NOT hex "b".
        let sleds = Topology {
            sleds: 6,
            ..Topology::default()
        }
        .sleds();
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

        let outside_gateway = External {
            host_ip: "10.21.1.1".into(),
            ..x.clone()
        };
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

        let outside_gateway = External {
            host_ip: "198.51.100.1".into(),
            ..first_usable_gateway
        };
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
        let cfg = VoxelConfig::from_toml("[topology]\nracks = 2\nsleds = 3\n").unwrap();
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
