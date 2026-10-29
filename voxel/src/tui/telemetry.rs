// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Telemetry domain types and parsers.

use std::collections::{BTreeMap, VecDeque};
use std::ops::AddAssign;
use std::time::{Duration, Instant};

use voxel_config::VoxelConfig;

const HISTORY_WINDOW: Duration = Duration::from_secs(60);
pub const SLED_CUBBIES: u8 = 32;
pub const SWITCH_SLOTS: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RackId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    Sled,
    SwitchZone,
    Router,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceScope {
    Fleet,
    Rack(RackId),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceId {
    pub scope: ResourceScope,
    pub kind: ResourceKind,
    pub name: String,
}

impl ResourceId {
    pub fn rack(
        rack: RackId,
        kind: ResourceKind,
        name: impl Into<String>,
    ) -> Self {
        Self { scope: ResourceScope::Rack(rack), kind, name: name.into() }
    }

    pub fn fleet(kind: ResourceKind, name: impl Into<String>) -> Self {
        Self { scope: ResourceScope::Fleet, kind, name: name.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDescriptor {
    pub id: ResourceId,
    pub rack: Option<RackId>,
    pub kind: ResourceKind,
    pub name: String,
    /// Hosting sled for a switch zone.
    pub host: Option<String>,
    /// Physical position in the rack elevation: a sled's cubby or a switch
    /// zone's switch slot. None for routers and for anything a real rack has
    /// no room for.
    pub slot: Option<u8>,
}

pub fn resource_descriptors(config: &VoxelConfig) -> Vec<ResourceDescriptor> {
    let sleds = config.sleds();
    let mut result = Vec::new();
    let mut switch_slots = BTreeMap::<usize, usize>::new();
    for sled in sleds {
        let rack = RackId(sled.rack);
        result.push(ResourceDescriptor {
            id: ResourceId::rack(rack, ResourceKind::Sled, &sled.name),
            rack: Some(rack),
            kind: ResourceKind::Sled,
            name: sled.name.clone(),
            host: None,
            // The SP location MGS reports is the global sled index, so a
            // second rack's sleds sit where the control plane says they do.
            slot: u8::try_from(sled.index)
                .ok()
                .filter(|cubby| *cubby < SLED_CUBBIES),
        });
        if sled.scrimlet {
            let slot = switch_slots.entry(sled.rack).or_default();
            let name = format!("switch{slot}");
            let switch_slot =
                u8::try_from(*slot).ok().filter(|slot| *slot < SWITCH_SLOTS);
            *slot += 1;
            result.push(ResourceDescriptor {
                // The rack-local slot is presentation/targeting data; the hosting
                // scrimlet's configured name remains stable if slots are reordered.
                id: ResourceId::rack(
                    rack,
                    ResourceKind::SwitchZone,
                    &sled.name,
                ),
                rack: Some(rack),
                kind: ResourceKind::SwitchZone,
                name,
                host: Some(sled.name),
                slot: switch_slot,
            });
        }
    }
    for name in &config.topology.routers {
        result.push(ResourceDescriptor {
            id: ResourceId::fleet(ResourceKind::Router, name),
            rack: None,
            kind: ResourceKind::Router,
            name: name.clone(),
            host: None,
            slot: None,
        });
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    result
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkCounters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
}

impl LinkCounters {
    #[cfg(test)]
    fn with_packets(mut self, rx: u64, tx: u64) -> Self {
        self.rx_packets = rx;
        self.tx_packets = tx;
        self
    }

    fn rate_from(
        self,
        previous: Self,
        elapsed: Duration,
    ) -> Option<BidirectionalRate> {
        let seconds = elapsed.as_secs_f64();
        if seconds == 0.0
            || self.rx_bytes < previous.rx_bytes
            || self.tx_bytes < previous.tx_bytes
            || self.rx_packets < previous.rx_packets
            || self.tx_packets < previous.tx_packets
        {
            return None;
        }
        Some(BidirectionalRate {
            rx_bytes_sec: (self.rx_bytes - previous.rx_bytes) as f64 / seconds,
            tx_bytes_sec: (self.tx_bytes - previous.tx_bytes) as f64 / seconds,
            rx_packets_sec: (self.rx_packets - previous.rx_packets) as f64
                / seconds,
            tx_packets_sec: (self.tx_packets - previous.tx_packets) as f64
                / seconds,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CounterSnapshot {
    pub captured_at: Instant,
    pub links: BTreeMap<String, LinkCounters>,
}

impl CounterSnapshot {
    pub fn new(
        captured_at: Instant,
        links: BTreeMap<String, LinkCounters>,
    ) -> Self {
        Self { captured_at, links }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BidirectionalRate {
    pub rx_bytes_sec: f64,
    pub tx_bytes_sec: f64,
    pub rx_packets_sec: f64,
    pub tx_packets_sec: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BidirectionalErrors {
    pub rx_sec: f64,
    pub tx_sec: f64,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd)]
pub enum TrafficSource {
    Oximeter,
    #[default]
    DirectProbe,
}

impl TrafficSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Oximeter => "Oximeter",
            Self::DirectProbe => "direct probe",
        }
    }
}

impl BidirectionalRate {
    pub fn total_bytes_sec(self) -> f64 {
        self.rx_bytes_sec + self.tx_bytes_sec
    }
}

impl AddAssign for BidirectionalRate {
    fn add_assign(&mut self, rhs: Self) {
        self.rx_bytes_sec += rhs.rx_bytes_sec;
        self.tx_bytes_sec += rhs.tx_bytes_sec;
        self.rx_packets_sec += rhs.rx_packets_sec;
        self.tx_packets_sec += rhs.tx_packets_sec;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneInfo {
    pub name: String,
    pub short_name: String,
    pub vnics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZoneTraffic {
    pub name: String,
    pub short_name: String,
    pub rate: BidirectionalRate,
    pub errors: BidirectionalErrors,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrafficSample {
    pub total: BidirectionalRate,
    pub links: BTreeMap<String, BidirectionalRate>,
    pub zones: Vec<ZoneTraffic>,
    pub errors: BidirectionalErrors,
    pub source: TrafficSource,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZoneCpu {
    pub id: ResourceId,
    pub name: String,
    pub kind: String,
    pub user_percent: f64,
    pub system_percent: f64,
    pub wait_percent: f64,
}

impl ZoneCpu {
    pub fn total_percent(&self) -> f64 {
        self.user_percent + self.system_percent
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZfsHeadroom {
    pub id: ResourceId,
    pub pool: String,
    pub allocated_bytes: u64,
    pub total_bytes: u64,
}

impl ZfsHeadroom {
    pub fn available_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.allocated_bytes)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OximeterExceptions {
    pub failed_collections: u64,
    pub dropped_samples: u64,
}

impl OximeterExceptions {
    /// The recent period the counts cover.
    pub const WINDOW: Duration = Duration::from_secs(5 * 60);
}

impl Default for TrafficSample {
    fn default() -> Self {
        Self {
            total: Default::default(),
            links: Default::default(),
            zones: Default::default(),
            errors: Default::default(),
            source: TrafficSource::DirectProbe,
        }
    }
}

impl From<BidirectionalRate> for TrafficSample {
    fn from(total: BidirectionalRate) -> Self {
        Self { total, ..Self::default() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResourceTelemetry {
    pub baseline: Option<CounterSnapshot>,
    pub link_rates: BTreeMap<String, BidirectionalRate>,
    pub zones: Vec<ZoneInfo>,
}

impl ResourceTelemetry {
    pub fn update(&mut self, snapshot: CounterSnapshot) {
        self.link_rates.clear();
        if let Some(previous) = &self.baseline
            && let Some(elapsed) = snapshot
                .captured_at
                .checked_duration_since(previous.captured_at)
        {
            for (name, current) in &snapshot.links {
                if let Some(rate) = previous
                    .links
                    .get(name)
                    .and_then(|old| current.rate_from(*old, elapsed))
                {
                    self.link_rates.insert(name.clone(), rate);
                }
            }
        }
        // Every update, including resets and non-forward timestamps, is the next baseline.
        self.baseline = Some(snapshot);
    }

    pub fn total_rate(&self) -> BidirectionalRate {
        self.link_rates.values().copied().fold(
            BidirectionalRate::default(),
            |mut a, r| {
                a += r;
                a
            },
        )
    }

    pub fn top_zones(&self, limit: usize) -> Vec<ZoneTraffic> {
        let mut owners = BTreeMap::<&str, usize>::new();
        for zone in &self.zones {
            let unique: std::collections::BTreeSet<_> =
                zone.vnics.iter().map(String::as_str).collect();
            for vnic in unique {
                *owners.entry(vnic).or_default() += 1;
            }
        }
        let mut zones: Vec<_> = self
            .zones
            .iter()
            .map(|zone| {
                let rate = zone
                    .vnics
                    .iter()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .filter(|v| owners.get(v).copied() == Some(1))
                    .filter_map(|v| self.link_rates.get(v))
                    .copied()
                    .fold(BidirectionalRate::default(), |mut a, r| {
                        a += r;
                        a
                    });
                ZoneTraffic {
                    name: zone.name.clone(),
                    short_name: zone.short_name.clone(),
                    rate,
                    errors: Default::default(),
                }
            })
            .collect();
        zones.sort_by(|a, b| {
            b.rate
                .total_bytes_sec()
                .total_cmp(&a.rate.total_bytes_sec())
                .then_with(|| a.name.cmp(&b.name))
        });
        zones.truncate(limit);
        zones
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HistoryPoint {
    pub captured_at: Instant,
    pub rate: BidirectionalRate,
}

#[derive(Debug, Clone)]
pub struct TrafficHistory {
    points: VecDeque<HistoryPoint>,
    max_count: usize,
}

impl TrafficHistory {
    pub fn new(max_count: usize) -> Self {
        Self { points: VecDeque::new(), max_count }
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.points.len()
    }
    pub fn points(&self) -> &VecDeque<HistoryPoint> {
        &self.points
    }
    pub fn push(&mut self, point: HistoryPoint) {
        if let Some(last) = self.points.back() {
            if point.captured_at < last.captured_at {
                return;
            }
            if point.captured_at == last.captured_at {
                self.points.pop_back();
            }
        }
        let cutoff = point.captured_at.checked_sub(HISTORY_WINDOW);
        while self
            .points
            .front()
            .is_some_and(|p| cutoff.is_some_and(|c| p.captured_at < c))
        {
            self.points.pop_front();
        }
        self.points.push_back(point);
        while self.points.len() > self.max_count {
            self.points.pop_front();
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResourceState {
    pub descriptor: ResourceDescriptor,
    pub current_rate: BidirectionalRate,
    pub current_sample: TrafficSample,
    /// Sample generation that produced `current_rate`.
    pub current_at: Option<Instant>,
    pub history: TrafficHistory,
}

#[derive(Debug, Clone)]
pub struct TelemetryModel {
    pub resources: BTreeMap<ResourceId, ResourceState>,
    pub fleet_rate: BidirectionalRate,
    pub rack_rates: BTreeMap<RackId, BidirectionalRate>,
    pub fleet_history: TrafficHistory,
    pub rack_histories: BTreeMap<RackId, TrafficHistory>,
}

impl TelemetryModel {
    pub fn new(
        descriptors: Vec<ResourceDescriptor>,
        history_max: usize,
    ) -> Self {
        let mut rack_rates = BTreeMap::new();
        let mut rack_histories = BTreeMap::new();
        let resources = descriptors
            .into_iter()
            .map(|descriptor| {
                if let Some(rack) = descriptor.rack {
                    rack_rates.entry(rack).or_default();
                    rack_histories
                        .entry(rack)
                        .or_insert_with(|| TrafficHistory::new(history_max));
                }
                (
                    descriptor.id.clone(),
                    ResourceState {
                        descriptor,
                        current_rate: Default::default(),
                        current_sample: Default::default(),
                        current_at: None,
                        history: TrafficHistory::new(history_max),
                    },
                )
            })
            .collect();
        Self {
            resources,
            fleet_rate: Default::default(),
            rack_rates,
            fleet_history: TrafficHistory::new(history_max),
            rack_histories,
        }
    }

    pub fn set_current_sample(
        &mut self,
        id: &ResourceId,
        now: Instant,
        sample: TrafficSample,
    ) {
        if let Some(resource) = self.resources.get_mut(id) {
            resource.current_rate = sample.total;
            resource.current_sample = sample;
            resource.current_at = Some(now);
            resource.history.push(HistoryPoint {
                captured_at: now,
                rate: resource.current_rate,
            });
        }
    }

    pub fn set_oximeter_samples(
        &mut self,
        id: &ResourceId,
        generation: Instant,
        samples: impl IntoIterator<Item = (Instant, TrafficSample)>,
    ) {
        let Some(resource) = self.resources.get_mut(id) else {
            return;
        };
        for (captured_at, sample) in samples {
            resource.current_rate = sample.total;
            resource.current_sample = sample;
            resource.history.push(HistoryPoint {
                captured_at,
                rate: resource.current_rate,
            });
        }
        if resource.current_sample.source == TrafficSource::Oximeter {
            resource.current_at = Some(generation);
        }
    }

    #[cfg(test)]
    pub fn set_current_rate(
        &mut self,
        id: &ResourceId,
        now: Instant,
        rate: BidirectionalRate,
    ) {
        self.set_current_sample(id, now, rate.into());
    }

    /// Totals the resources whose rate came from the latest generation of
    /// their rack and source. Oximeter reports each rack at its own time
    /// while direct probes, including those of routers, which belong to no
    /// rack, run on their own cadence, so the latest generation overall would
    /// leave every other rack and source out.
    pub fn rebuild_aggregates(&mut self, now: Instant) {
        let scope = |resource: &ResourceState| {
            (resource.descriptor.rack, resource.current_sample.source)
        };
        let mut latest = BTreeMap::new();
        for resource in self.resources.values() {
            if let Some(at) = resource.current_at {
                let generation = latest.entry(scope(resource)).or_insert(at);
                *generation = (*generation).max(at);
            }
        }
        self.fleet_rate = Default::default();
        for rate in self.rack_rates.values_mut() {
            *rate = Default::default();
        }
        for resource in self.resources.values().filter(|resource| {
            resource.current_at.is_some()
                && resource.current_at == latest.get(&scope(resource)).copied()
        }) {
            self.fleet_rate += resource.current_rate;
            if let Some(rack) = resource.descriptor.rack {
                *self.rack_rates.entry(rack).or_default() +=
                    resource.current_rate;
            }
        }
        self.fleet_history
            .push(HistoryPoint { captured_at: now, rate: self.fleet_rate });
        for (rack, rate) in &self.rack_rates {
            if let Some(history) = self.rack_histories.get_mut(rack) {
                history.push(HistoryPoint { captured_at: now, rate: *rate });
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficSeverity {
    Normal,
    Elevated,
    High,
}

impl TrafficSeverity {
    pub fn for_bytes_per_sec(value: f64) -> Self {
        if value > 5_000_000.0 {
            Self::High
        } else if value > 100_000.0 {
            Self::Elevated
        } else {
            Self::Normal
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeAddresses {
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
}

pub fn parse_kstat_link_counters(
    output: &str,
) -> BTreeMap<String, LinkCounters> {
    #[derive(Default)]
    struct Fields {
        rx_bytes: Option<u64>,
        tx_bytes: Option<u64>,
        rx_packets: Option<u64>,
        tx_packets: Option<u64>,
    }

    let mut fields = BTreeMap::<String, Fields>::new();
    for line in output.lines() {
        let Some((key, raw)) = line.split_once('\t') else {
            continue;
        };
        let parts: Vec<_> = key.split(':').collect();
        if parts.len() != 4 {
            continue;
        }
        let Ok(value) = raw.trim().parse::<u64>() else {
            continue;
        };
        let field = match parts[3] {
            "rbytes64" => 0,
            "obytes64" => 1,
            "ipackets64" => 2,
            "opackets64" => 3,
            _ => continue,
        };
        let entry = fields.entry(parts[2].to_owned()).or_default();
        match field {
            0 => entry.rx_bytes = Some(value),
            1 => entry.tx_bytes = Some(value),
            2 => entry.rx_packets = Some(value),
            _ => entry.tx_packets = Some(value),
        }
    }
    fields
        .into_iter()
        .filter_map(|(name, f)| {
            Some((
                name,
                LinkCounters {
                    rx_bytes: f.rx_bytes?,
                    tx_bytes: f.tx_bytes?,
                    rx_packets: f.rx_packets?,
                    tx_packets: f.tx_packets?,
                },
            ))
        })
        .collect()
}

fn short_zone_name(name: &str) -> String {
    if name == "global" {
        return name.into();
    }
    let stripped = name.strip_prefix("oxz_").unwrap_or(name);
    if let Some((prefix, suffix)) = stripped.rsplit_once('_')
        && suffix.len() > 8
        && suffix.chars().take(8).all(|c| c.is_ascii_hexdigit())
    {
        return prefix.into();
    }
    stripped.into()
}

pub fn parse_dladm_zone_vnics(output: &str) -> Vec<ZoneInfo> {
    let mut mapped =
        BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    let mut owners =
        BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    for line in output.lines() {
        if line.matches(':').count() != 1 || line.contains('\t') {
            continue;
        }
        let Some((link, zone)) = line.split_once(':') else {
            continue;
        };
        let (link, zone) = (link.trim(), zone.trim());
        if link.is_empty() || !(zone == "global" || zone.starts_with("oxz_")) {
            continue;
        }
        mapped.entry(zone.into()).or_default().insert(link.into());
        owners.entry(link.into()).or_default().insert(zone.into());
    }
    mapped
        .into_iter()
        .filter_map(|(name, vnics)| {
            let vnics = vnics
                .into_iter()
                .filter(|vnic| {
                    owners.get(vnic).is_some_and(|zones| zones.len() == 1)
                })
                .collect::<Vec<_>>();
            (!vnics.is_empty()).then(|| ZoneInfo {
                short_name: short_zone_name(&name),
                name,
                vnics,
            })
        })
        .collect()
}

pub fn parse_linux_link_counters(
    output: &str,
) -> BTreeMap<String, LinkCounters> {
    let mut result = BTreeMap::new();
    for line in output.lines() {
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() < 5 {
            continue;
        }
        let Ok(values) = p[1..5]
            .iter()
            .map(|v| v.parse::<u64>())
            .collect::<Result<Vec<_>, _>>()
        else {
            continue;
        };
        result.insert(
            p[0].into(),
            LinkCounters {
                rx_bytes: values[0],
                tx_bytes: values[1],
                rx_packets: values[2],
                tx_packets: values[3],
            },
        );
    }
    result
}

pub fn parse_ipadm_addresses(output: &str) -> NodeAddresses {
    let mut result = NodeAddresses::default();
    for line in output.lines() {
        let Some((object, raw)) = line.split_once(':') else {
            continue;
        };
        if object.starts_with("lo0") || object.ends_with("/ll") {
            continue;
        }
        let address = raw.replace("\\:", ":");
        let target = if address.contains('.') {
            if object.starts_with("vioif") && !result.ipv4.contains(&address) {
                result.ipv4.insert(0, address);
                continue;
            }
            &mut result.ipv4
        } else if address.contains(':') && !address.starts_with("fe80") {
            if (object.contains("sled") || object.contains("underlay"))
                && !result.ipv6.contains(&address)
            {
                result.ipv6.insert(0, address);
                continue;
            }
            &mut result.ipv6
        } else {
            continue;
        };
        if !target.contains(&address) {
            target.push(address);
        }
    }
    result
}

pub fn parse_linux_ip_addresses(output: &str) -> NodeAddresses {
    let mut result = NodeAddresses::default();
    for line in output.lines() {
        if line.contains(": lo ")
            || line.contains("docker")
            || line.contains("veth")
        {
            continue;
        }
        let (marker, target, ipv6) = if line.contains(" inet6 ") {
            (" inet6 ", &mut result.ipv6, true)
        } else if line.contains(" inet ") {
            (" inet ", &mut result.ipv4, false)
        } else {
            continue;
        };
        let Some(raw) = line
            .split_once(marker)
            .and_then(|(_, rest)| rest.split_whitespace().next())
        else {
            continue;
        };
        let valid_family = raw
            .split('/')
            .next()
            .and_then(|address| address.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_ipv6() == ipv6);
        if !valid_family {
            continue;
        }
        if raw.starts_with("127.")
            || raw.starts_with("fe80")
            || raw.starts_with("::1")
        {
            continue;
        }
        let address = raw.to_owned();
        if !target.contains(&address) {
            target.push(address);
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Online,
    Offline,
    Disabled,
    Maintenance,
    Degraded,
}

pub fn parse_service_state(output: &str) -> Option<ServiceState> {
    match output.trim() {
        "online" => Some(ServiceState::Online),
        "offline" => Some(ServiceState::Offline),
        "disabled" => Some(ServiceState::Disabled),
        "maintenance" => Some(ServiceState::Maintenance),
        "degraded" => Some(ServiceState::Degraded),
        _ => None,
    }
}

pub fn parse_failed_services(output: &str) -> Vec<String> {
    let mut services: Vec<_> = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("svc:/"))
        .filter_map(|service| {
            service.rsplit_once(':').map(|(name, _)| name.to_owned())
        })
        .collect();
    services.sort();
    services.dedup();
    services
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZoneDiagnostic {
    pub zones: Vec<String>,
}

pub fn parse_zone_diagnostics(output: &str) -> ZoneDiagnostic {
    let mut zones: Vec<_> = output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_whitespace))
        .map(str::to_owned)
        .collect();
    zones.sort();
    zones.dedup();
    ZoneDiagnostic { zones }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NtpDiagnostic {
    pub stratum: Option<u32>,
    pub synchronized: Option<bool>,
}

pub fn parse_chrony_tracking(output: &str) -> NtpDiagnostic {
    let mut result = NtpDiagnostic::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "stratum" => result.stratum = value.trim().parse().ok(),
            "leap status" => {
                let value = value.trim().to_ascii_lowercase();
                result.synchronized = Some(
                    !(value.contains("not synchron")
                        || value.contains("unsynchron")),
                );
            }
            _ => {}
        }
    }
    if result.synchronized.is_none() {
        result.synchronized = result.stratum.map(|s| s > 0 && s < 16);
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Checking,
    Healthy,
    Degraded,
    Failed,
    Unknown,
    Stale,
    Unavailable,
    Stopped,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthDiagnostic {
    pub sled_agent: Option<ServiceState>,
    pub failed_services: Vec<String>,
    pub ntp: NtpDiagnostic,
    pub zones: ZoneDiagnostic,
    pub notes: Vec<String>,
}

impl HealthDiagnostic {
    pub fn state(&self) -> HealthState {
        if matches!(
            self.sled_agent,
            Some(ServiceState::Offline | ServiceState::Maintenance)
        ) {
            return HealthState::Failed;
        }
        if !self.failed_services.is_empty() {
            return HealthState::Degraded;
        }
        if self.sled_agent.is_none() && self.ntp.synchronized.is_none() {
            return HealthState::Unknown;
        }
        if self.sled_agent != Some(ServiceState::Online)
            || self.ntp.synchronized != Some(true)
        {
            HealthState::Degraded
        } else {
            HealthState::Healthy
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthContext {
    Checking,
    Active,
    Stopped,
}

pub fn derive_health_state(
    context: HealthContext,
    diagnostic: Option<&HealthDiagnostic>,
    freshness: Freshness,
) -> HealthState {
    match context {
        HealthContext::Checking => HealthState::Checking,
        HealthContext::Stopped => HealthState::Stopped,
        HealthContext::Active => match freshness {
            Freshness::Stale => HealthState::Stale,
            Freshness::Unavailable => HealthState::Unavailable,
            Freshness::Fresh => diagnostic
                .map(HealthDiagnostic::state)
                .unwrap_or(HealthState::Unknown),
        },
    }
}

#[derive(Debug, Clone)]
pub struct TimedValue<T> {
    pub captured_at: Instant,
    pub value: T,
}
#[derive(Debug, Clone)]
pub struct CollectionError {
    pub attempted_at: Instant,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct LatestSample<T> {
    pub good: Option<TimedValue<T>>,
    pub last_attempt: Option<Instant>,
    pub latest_error: Option<CollectionError>,
}

impl<T> Default for LatestSample<T> {
    fn default() -> Self {
        Self { good: None, last_attempt: None, latest_error: None }
    }
}

impl<T> LatestSample<T> {
    pub fn record_success(&mut self, captured_at: Instant, value: T) {
        self.last_attempt = Some(captured_at);
        self.good = Some(TimedValue { captured_at, value });
        self.latest_error = None;
    }
    pub fn record_error(
        &mut self,
        attempted_at: Instant,
        message: impl Into<String>,
    ) {
        self.last_attempt = Some(attempted_at);
        self.latest_error =
            Some(CollectionError { attempted_at, message: message.into() });
    }
    /// Boundary semantics: age equal to `stale_after` remains fresh and age equal
    /// to `unavailable_after` remains stale. Threshold order errors degrade safely.
    pub fn freshness(
        &self,
        now: Instant,
        stale_after: Duration,
        unavailable_after: Duration,
    ) -> Freshness {
        let Some(good) = &self.good else {
            return Freshness::Unavailable;
        };
        let age =
            now.checked_duration_since(good.captured_at).unwrap_or_default();
        if age <= stale_after {
            Freshness::Fresh
        } else if age <= unavailable_after.max(stale_after) {
            Freshness::Stale
        } else {
            Freshness::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::{
        config, rate, router, sample, sled, switch,
    };
    use indoc::indoc;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    fn counters(rx: u64, tx: u64) -> LinkCounters {
        LinkCounters {
            rx_bytes: rx,
            tx_bytes: tx,
            rx_packets: rx / 10,
            tx_packets: tx / 10,
        }
    }

    fn links<const N: usize>(
        entries: [(&str, LinkCounters); N],
    ) -> BTreeMap<String, LinkCounters> {
        entries.into_iter().map(|(name, c)| (name.to_owned(), c)).collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|&s| s.to_owned()).collect()
    }

    fn addresses(ipv4: &[&str], ipv6: &[&str]) -> NodeAddresses {
        NodeAddresses { ipv4: strings(ipv4), ipv6: strings(ipv6) }
    }

    fn zone(name: &str, short_name: &str, vnics: &[&str]) -> ZoneInfo {
        ZoneInfo {
            name: name.into(),
            short_name: short_name.into(),
            vnics: strings(vnics),
        }
    }

    fn ntp(stratum: Option<u32>, synchronized: bool) -> NtpDiagnostic {
        NtpDiagnostic { stratum, synchronized: Some(synchronized) }
    }

    #[test]
    fn host_command_output_parses() {
        assert_eq!(
            parse_kstat_link_counters(
                "0:link:vioif0:rbytes64\t1000\n\
                 0:link:vioif0:obytes64\t2000\n\
                 0:link:vioif0:ipackets64\t50\n\
                 0:link:vioif0:opackets64\t60\n\
                 0:link:bad:rbytes64\tnot-a-number\n\
                 0:link:bad:obytes64\t1\n\
                 0:link:bad:ipackets64\t1\n\
                 0:link:bad:opackets64\t1\n"
            ),
            links([("vioif0", counters(1000, 2000).with_packets(50, 60))]),
            "kstat keeps only links whose counters all parse"
        );
        assert_eq!(
            parse_dladm_zone_vnics(
                "vnic1:oxz_ntp_aabbccddeeff\n\
                 vnic1:oxz_ntp_aabbccddeeff\n\
                 vnic0:global\n\
                 shared:oxz_one_deadbeef0\n\
                 shared:oxz_two_deadbeef0\n\
                 :global\n\
                 vnic2:oxz_bad:zone\n"
            ),
            vec![
                zone("global", "global", &["vnic0"]),
                zone("oxz_ntp_aabbccddeeff", "ntp", &["vnic1"]),
            ],
            "dladm groups vnics by zone and drops shared and malformed ones"
        );
        assert_eq!(
            parse_linux_link_counters(
                "eth1 10 20 1 2 ignored extra\n\
                 bad 1 nope 3 4 5\n\
                 short 1\n"
            ),
            links([("eth1", counters(10, 20).with_packets(1, 2))]),
            "linux counters ignore extra columns and skip malformed rows"
        );
        assert_eq!(
            parse_ipadm_addresses(
                "net0/v4:10.0.0.1/24\n\
                 vioif3/v4:192.168.68.122/22\n\
                 net0/v6:2001\\:db8\\:\\:2/64\n\
                 underlay0/sled6:fd00\\:1122\\:3344\\:101\\:\\:1/64\n\
                 vioif1/ll:fe80\\:\\:1/10\n\
                 malformed\n"
            ),
            addresses(
                &["192.168.68.122/22", "10.0.0.1/24"],
                &["fd00:1122:3344:101::1/64", "2001:db8::2/64"],
            ),
            "ipadm puts host LAN and underlay first and skips link-local"
        );
        assert_eq!(
            parse_linux_ip_addresses(
                "5: enp0s11    inet 192.168.68.108/22 scope global enp0s11\n\
                 5: enp0s11    inet6 2001:db8::1/64 scope global\n\
                 6: docker0    inet 172.17.0.1/16 scope global docker0\n\
                 7: enp0s12    inet not-an-address scope global\n"
            ),
            addresses(&["192.168.68.108/22"], &["2001:db8::1/64"]),
            "linux addresses skip bridges and invalid addresses"
        );
        assert_eq!(
            parse_zone_diagnostics(
                "global\noxz_switch\noxz_switch\noxz_ntp_deadbeef0\nbad zone\n"
            ),
            ZoneDiagnostic {
                zones: strings(&["global", "oxz_ntp_deadbeef0", "oxz_switch"]),
            },
            "zone list is sorted, deduplicated and skips malformed names"
        );
        assert_eq!(
            parse_service_state(" online\n"),
            Some(ServiceState::Online),
            "service state is trimmed"
        );
        assert_eq!(
            parse_service_state("garbage"),
            None,
            "service state rejects unknown states"
        );
        assert_eq!(
            parse_failed_services(
                "svc:/oxide/sled-agent:default\n\
                 noise\n\
                 svc:/network/dns/client:default\n"
            ),
            strings(&["network/dns/client", "oxide/sled-agent"]),
            "failed services are sorted FMRI names without noise"
        );
        assert_eq!(
            parse_chrony_tracking(
                "Reference ID : A29FC87B\nStratum : 4\nLeap status : Normal\n"
            ),
            ntp(Some(4), true),
            "chrony synchronised"
        );
        assert_eq!(
            parse_chrony_tracking(
                "Stratum : 0\nLeap status : Not synchronised"
            ),
            ntp(Some(0), false),
            "chrony not synchronised"
        );
        assert_eq!(
            parse_chrony_tracking(
                "Stratum : nope\nmalformed\nLeap status : Unsynchronised"
            ),
            ntp(None, false),
            "chrony malformed stratum"
        );
    }

    #[test]
    fn rates_history_and_totals_are_correct() {
        // Discovery lists every rack's sleds, numbered across the fleet, with
        // two switch zones on each rack's scrimlets, and fleet-wide routers.
        let cfg = config(indoc! {r#"
            [topology]
            racks = 2
            sleds = 3
            routers = ["ce", "cr1"]
        "#});
        assert_eq!(
            resource_descriptors(&cfg),
            vec![
                router("ce"),
                router("cr1"),
                sled(0, 0),
                sled(0, 1),
                sled(0, 2),
                switch(0, 0, "g0"),
                switch(0, 1, "g2"),
                sled(1, 3),
                sled(1, 4),
                sled(1, 5),
                switch(1, 0, "g3"),
                switch(1, 1, "g5"),
            ]
        );

        // Switch slots follow sled order, not the order scrimlets are listed
        // in.
        let reordered = config(indoc! {r#"
            [topology]
            racks = 1
            sleds = 3
            scrimlets = ["g2", "g0"]
            routers = []
        "#});
        assert_eq!(
            resource_descriptors(&reordered),
            vec![
                sled(0, 0),
                sled(0, 1),
                sled(0, 2),
                switch(0, 0, "g0"),
                switch(0, 1, "g2"),
            ]
        );

        // Link rates from successive counter snapshots.
        let start = Instant::now();
        let snapshot = |secs: u64, rx: u64| {
            CounterSnapshot::new(
                start + Duration::from_secs(secs),
                BTreeMap::from([("net0".into(), counters(rx, 2 * rx))]),
            )
        };
        let mut state = ResourceTelemetry::default();
        state.update(snapshot(0, 100));
        // (case, seconds, received bytes, expected received rate)
        for (case, secs, rx, expected) in [
            ("divided by the elapsed four seconds", 4, 500, Some(100.0)),
            ("a counter reset drops the link", 6, 10, None),
            ("rates resume from the post-reset baseline", 8, 110, Some(50.0)),
            ("zero elapsed time has no rate", 8, 210, None),
        ] {
            state.update(snapshot(secs, rx));
            assert_eq!(
                state.link_rates.get("net0").map(|r| r.rx_bytes_sec),
                expected,
                "{case}"
            );
        }

        // Traffic history keeps the trailing sixty seconds, up to a count.
        let mut history = TrafficHistory::new(3);
        let mut push = |secs: u64, rx: f64| {
            history.push(HistoryPoint {
                captured_at: start + Duration::from_secs(secs),
                rate: rate(rx, 0.0),
            });
            history
                .points()
                .iter()
                .map(|p| p.rate.rx_bytes_sec)
                .collect::<Vec<_>>()
        };
        push(0, 0.0);
        // (case, seconds, rate, the rates of the points kept). Each point's
        // rate is unique, so the rates identify the points.
        for (case, secs, rx, expected) in [
            ("a point sixty seconds old stays", 60, 1.0, vec![0.0, 1.0]),
            ("a repeated timestamp replaces the last", 60, 2.0, vec![0.0, 2.0]),
            ("an earlier timestamp adds nothing", 30, 3.0, vec![0.0, 2.0]),
            ("older points leave the window", 61, 4.0, vec![2.0, 4.0]),
            ("points within the window stay", 62, 5.0, vec![2.0, 4.0, 5.0]),
            ("the capacity bounds the history", 63, 6.0, vec![4.0, 5.0, 6.0]),
        ] {
            assert_eq!(push(secs, rx), expected, "{case}");
        }

        // Fleet and rack totals count each current resource once.
        let cfg = config(indoc! {r#"
            [topology]
            racks = 2
            sleds = 3
            routers = ["ce"]
        "#});
        let mut model = TelemetryModel::new(resource_descriptors(&cfg), 64);
        let sled0 = sled(0, 0).id;
        model.set_current_rate(&sled0, start, rate(10.0, 20.0));
        model.set_current_rate(&switch(0, 0, "g0").id, start, rate(1.0, 2.0));
        model.rebuild_aggregates(start);
        assert_eq!(model.fleet_rate, rate(11.0, 22.0));
        assert_eq!(
            model.rack_rates,
            BTreeMap::from([
                (RackId(0), rate(11.0, 22.0)),
                (RackId(1), rate(0.0, 0.0)),
            ])
        );
        // Only the sled reports in the next generation, so the switch's rate
        // is stale and left out.
        let next = start + Duration::from_secs(1);
        model.set_current_rate(&sled0, next, rate(4.0, 0.0));
        model.rebuild_aggregates(next);
        assert_eq!(model.fleet_rate, rate(4.0, 0.0));
        assert_eq!(model.fleet_history.len(), 2, "one point per generation");
        // Oximeter reports the sled at its own time, and a router, which
        // belongs to no rack, is probed later still. Neither generation leaves
        // the other's rate, or the switch's, out of the totals.
        let oximeter = next + Duration::from_secs(1);
        model.set_oximeter_samples(
            &sled0,
            oximeter,
            [(oximeter, sample(4.0, TrafficSource::Oximeter))],
        );
        model.rebuild_aggregates(oximeter);
        let probed = oximeter + Duration::from_secs(1);
        model.set_current_rate(&switch(0, 0, "g0").id, probed, rate(1.0, 0.0));
        model.set_current_rate(&router("ce").id, probed, rate(2.0, 0.0));
        model.rebuild_aggregates(probed);
        assert_eq!(model.rack_rates[&RackId(0)], rate(5.0, 0.0));
        assert_eq!(model.fleet_rate, rate(7.0, 0.0));

        // Top zones rank by rate; a vnic listed twice counts once, and one
        // shared between zones counts for neither.
        let state = ResourceTelemetry {
            link_rates: BTreeMap::from([
                ("a".into(), rate(3.0, 0.0)),
                ("b".into(), rate(8.0, 0.0)),
            ]),
            zones: vec![
                zone("zone-z", "z", &["a"]),
                zone("zone-a", "a", &["b", "b", "a"]),
            ],
            ..Default::default()
        };
        assert_eq!(
            state
                .top_zones(2)
                .iter()
                .map(|z| (z.name.as_str(), z.rate.total_bytes_sec()))
                .collect::<Vec<_>>(),
            vec![("zone-a", 8.0), ("zone-z", 0.0)]
        );
    }

    #[test]
    fn health_state_follows_freshness_and_diagnostics() {
        // Severity boundaries are strict.
        for (bytes_per_sec, severity) in [
            (100_000.0, TrafficSeverity::Normal),
            (100_000.1, TrafficSeverity::Elevated),
            (5_000_000.0, TrafficSeverity::Elevated),
            (5_000_000.1, TrafficSeverity::High),
        ] {
            assert_eq!(
                TrafficSeverity::for_bytes_per_sec(bytes_per_sec),
                severity,
                "{bytes_per_sec} B/s"
            );
        }

        // The latest good sample survives errors, and freshness is measured
        // from it, with both boundaries inclusive.
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let fresh_for = Duration::from_secs(10);
        let available_for = Duration::from_secs(30);
        let mut sample = LatestSample::default();
        assert_eq!(
            sample.freshness(start, fresh_for, available_for),
            Freshness::Unavailable
        );
        sample.record_success(start, 7u32);
        sample.record_error(at(5), "timeout");
        assert_eq!(sample.latest_error.as_ref().unwrap().message, "timeout");
        for (age, freshness) in [
            (10, Freshness::Fresh),
            (11, Freshness::Stale),
            (30, Freshness::Stale),
            (31, Freshness::Unavailable),
        ] {
            assert_eq!(
                sample.freshness(at(age), fresh_for, available_for),
                freshness,
                "{age} s after the good sample"
            );
        }
        sample.record_success(at(32), 8);
        assert_eq!(sample.good.as_ref().map(|g| g.value), Some(8));
        assert!(sample.latest_error.is_none());

        // Health follows the context, then freshness, then the diagnostic.
        // A healthy diagnostic sees only the global zone: the zone count is
        // not evidence of health.
        let healthy = HealthDiagnostic {
            sled_agent: Some(ServiceState::Online),
            ntp: NtpDiagnostic { stratum: None, synchronized: Some(true) },
            zones: ZoneDiagnostic { zones: vec!["global".into()] },
            ..Default::default()
        };
        // A failed service degrades even when nothing else is known.
        let failed_service = HealthDiagnostic {
            failed_services: vec!["oxide/foo".into()],
            ..Default::default()
        };
        let sled_agent_maintenance = HealthDiagnostic {
            sled_agent: Some(ServiceState::Maintenance),
            ..healthy.clone()
        };

        use Freshness::{Fresh, Stale, Unavailable};
        use HealthContext::{Active, Checking, Stopped};
        // (context, diagnostic, freshness, state)
        let cases = [
            (Checking, Some(&failed_service), Stale, HealthState::Checking),
            (Active, Some(&healthy), Fresh, HealthState::Healthy),
            (Active, Some(&failed_service), Fresh, HealthState::Degraded),
            (Active, Some(&sled_agent_maintenance), Fresh, HealthState::Failed),
            (Active, Some(&healthy), Stale, HealthState::Stale),
            (Active, Some(&healthy), Unavailable, HealthState::Unavailable),
            (Active, None, Fresh, HealthState::Unknown),
            (Stopped, Some(&failed_service), Stale, HealthState::Stopped),
        ];
        for (context, diagnostic, freshness, state) in cases {
            assert_eq!(
                derive_health_state(context, diagnostic, freshness),
                state,
                "{context:?}, {:?}, {freshness:?}",
                diagnostic.map(HealthDiagnostic::state),
            );
        }
    }
}
