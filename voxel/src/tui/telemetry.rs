//! Telemetry domain types and parsers.

use std::collections::{BTreeMap, VecDeque};
use std::ops::AddAssign;
use std::time::{Duration, Instant};

use voxel_config::VoxelConfig;

const HISTORY_WINDOW: Duration = Duration::from_secs(60);

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
        });
        if sled.scrimlet {
            let slot = switch_slots.entry(sled.rack).or_default();
            let name = format!("switch{slot}");
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

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
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
        if let Some(previous) = &self.baseline {
            if let Some(elapsed) = snapshot
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

    pub fn rebuild_aggregates(&mut self, now: Instant) {
        self.fleet_rate = Default::default();
        for rate in self.rack_rates.values_mut() {
            *rate = Default::default();
        }
        for resource in self
            .resources
            .values()
            .filter(|resource| resource.current_at == Some(now))
        {
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
    if let Some((prefix, suffix)) = stripped.rsplit_once('_') {
        if suffix.len() > 8
            && suffix.chars().take(8).all(|c| c.is_ascii_hexdigit())
        {
            return prefix.into();
        }
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
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};
    use voxel_config::VoxelConfig;

    fn counters(rx: u64, tx: u64) -> LinkCounters {
        LinkCounters {
            rx_bytes: rx,
            tx_bytes: tx,
            rx_packets: rx / 10,
            tx_packets: tx / 10,
        }
    }

    #[test]
    fn discovers_stable_multi_rack_resources() {
        let cfg = VoxelConfig::from_toml(
            "[topology]\nracks = 2\nsleds = 3\nrouters = [\"ce\", \"cr1\"]\n",
        )
        .unwrap();
        let resources = resource_descriptors(&cfg);
        assert_eq!(
            resources.iter().filter(|r| r.kind == ResourceKind::Sled).count(),
            6
        );
        let switches: Vec<_> = resources
            .iter()
            .filter(|r| r.kind == ResourceKind::SwitchZone)
            .collect();
        assert_eq!(
            switches
                .iter()
                .map(|r| (r.rack, r.name.as_str(), r.host.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                (Some(RackId(0)), "switch0", Some("g0")),
                (Some(RackId(0)), "switch1", Some("g2")),
                (Some(RackId(1)), "switch0", Some("g3")),
                (Some(RackId(1)), "switch1", Some("g5")),
            ]
        );
        assert!(
            resources
                .iter()
                .filter(|r| r.kind == ResourceKind::Router)
                .all(|r| r.rack.is_none())
        );
        assert_eq!(
            resources
                .iter()
                .map(|r| &r.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            resources.len()
        );

        let reordered = VoxelConfig::from_toml(
            "[topology]\nracks = 1\nsleds = 3\nscrimlets = [\"g2\", \"g0\"]\nrouters = []\n",
        )
        .unwrap();
        let switches: BTreeMap<_, _> = resource_descriptors(&reordered)
            .into_iter()
            .filter(|r| r.kind == ResourceKind::SwitchZone)
            .map(|r| (r.host.clone().unwrap(), (r.id, r.name)))
            .collect();
        assert_eq!(switches["g2"].0.name, "g2");
        assert!(switches["g2"].1.starts_with("switch"));
        assert_eq!(switches["g0"].0.name, "g0");
        assert!(switches["g0"].1.starts_with("switch"));
    }

    #[test]
    fn parses_production_counter_and_zone_fixtures_resiliently() {
        let kstat = "0:link:vioif0:rbytes64\t1000\n0:link:vioif0:obytes64\t2000\n0:link:vioif0:ipackets64\t50\n0:link:vioif0:opackets64\t60\n0:link:bad:rbytes64\tnot-a-number\nbroken\n";
        assert_eq!(
            parse_kstat_link_counters(kstat)["vioif0"],
            counters(1000, 2000).with_packets(50, 60)
        );
        assert!(!parse_kstat_link_counters(kstat).contains_key("bad"));

        let zones = parse_dladm_zone_vnics(
            "vnic2:oxz_ntp_aabbccddeeff\nvnic2:oxz_ntp_aabbccddeeff\nvnic1:oxz_nexus_0123456789\nvnic0:global\nshared:oxz_one_deadbeef0\nshared:oxz_two_deadbeef0\n:global\nbad:zone:line\n",
        );
        assert_eq!(
            zones[0],
            ZoneInfo {
                name: "global".into(),
                short_name: "global".into(),
                vnics: vec!["vnic0".into()]
            }
        );
        assert_eq!(zones[1].short_name, "nexus");
        assert_eq!(zones[2].short_name, "ntp");
        assert!(
            zones.iter().all(|zone| !zone.vnics.iter().any(|v| v == "shared"))
        );
        assert_eq!(zones[2].vnics, vec!["vnic2"]);

        let linux = parse_linux_link_counters(
            "eth1 10 20 1 2 ignored extra\neth0 30 40 3 4\nbad 1 nope 3 4 5\nshort 1\n",
        );
        assert_eq!(
            linux.keys().cloned().collect::<Vec<_>>(),
            vec!["eth0", "eth1"]
        );
    }

    #[test]
    fn parses_addresses_and_health_diagnostics() {
        let illumos = parse_ipadm_addresses(
            "net0/v4:10.0.0.1/24\nvioif3/v4:192.168.68.122/22\nnet0/v6:2001\\:db8\\:\\:2/64\nunderlay0/sled6:fd00\\:1122\\:3344\\:101\\:\\:1/64\nvioif1/ll:fe80\\:\\:1/10\nmalformed\n",
        );
        assert_eq!(illumos.ipv4, vec!["192.168.68.122/22", "10.0.0.1/24"]);
        assert_eq!(
            illumos.ipv6,
            vec!["fd00:1122:3344:101::1/64", "2001:db8::2/64"]
        );
        let linux = parse_linux_ip_addresses(
            "1: lo    inet 127.0.0.1/8 scope host lo\n5: enp0s11    inet 192.168.68.108/22 scope global enp0s11\n5: enp0s11    inet6 2001:db8::1/64 scope global\n6: docker0    inet 172.17.0.1/16 scope global docker0\ntruncated inet \n7: enp0s12    inet not-an-address scope global\nmalformed\n",
        );
        assert_eq!(linux.ipv4, vec!["192.168.68.108/22"]);
        assert_eq!(linux.ipv6, vec!["2001:db8::1/64"]);

        assert_eq!(
            parse_zone_diagnostics(
                "global\noxz_switch\noxz_switch\noxz_ntp_deadbeef0\nbad zone\n"
            )
            .zones,
            vec!["global", "oxz_ntp_deadbeef0", "oxz_switch"]
        );
        assert_eq!(
            parse_service_state(" online\n"),
            Some(ServiceState::Online)
        );
        assert_eq!(parse_service_state("garbage"), None);
        assert_eq!(
            parse_failed_services(
                "svc:/oxide/sled-agent:default\nnoise\nsvc:/network/dns/client:default\n"
            ),
            vec!["network/dns/client", "oxide/sled-agent"]
        );
        let ntp = parse_chrony_tracking(
            "Reference ID : A29FC87B\nStratum : 4\nLeap status : Normal\n",
        );
        assert_eq!(
            ntp,
            NtpDiagnostic { stratum: Some(4), synchronized: Some(true) }
        );
        assert_eq!(
            parse_chrony_tracking(
                "Stratum : 0\nLeap status : Not synchronised"
            ),
            NtpDiagnostic { stratum: Some(0), synchronized: Some(false) }
        );
        assert_eq!(
            parse_chrony_tracking(
                "Stratum : nope\nmalformed\nLeap status : Unsynchronised"
            ),
            NtpDiagnostic { stratum: None, synchronized: Some(false) }
        );
    }

    #[test]
    fn rates_use_elapsed_time_reset_baseline_and_recover() {
        let start = Instant::now();
        let mut state = ResourceTelemetry::default();
        state.update(CounterSnapshot::new(
            start,
            BTreeMap::from([("net0".into(), counters(100, 200))]),
        ));
        state.update(CounterSnapshot::new(
            start + Duration::from_secs(4),
            BTreeMap::from([("net0".into(), counters(500, 1000))]),
        ));
        assert_eq!(state.link_rates["net0"].rx_bytes_sec, 100.0);
        state.update(CounterSnapshot::new(
            start + Duration::from_secs(6),
            BTreeMap::from([("net0".into(), counters(10, 20))]),
        ));
        assert!(!state.link_rates.contains_key("net0"));
        state.update(CounterSnapshot::new(
            start + Duration::from_secs(8),
            BTreeMap::from([("net0".into(), counters(110, 220))]),
        ));
        assert_eq!(state.link_rates["net0"].rx_bytes_sec, 50.0);
        state.update(CounterSnapshot::new(
            start + Duration::from_secs(8),
            BTreeMap::from([("net0".into(), counters(210, 420))]),
        ));
        assert!(state.link_rates.is_empty(), "zero elapsed has no rates");
        state.update(CounterSnapshot::new(
            start + Duration::from_secs(10),
            BTreeMap::from([("net0".into(), counters(310, 620))]),
        ));
        assert_eq!(
            state.link_rates["net0"].rx_bytes_sec, 50.0,
            "zero elapsed still advances baseline"
        );
    }

    #[test]
    fn traffic_sample_preserves_total_links_and_zone_attribution() {
        let sample = TrafficSample {
            total: BidirectionalRate {
                rx_bytes_sec: 7.0,
                tx_bytes_sec: 3.0,
                ..Default::default()
            },
            links: BTreeMap::from([(
                "net0".into(),
                BidirectionalRate::default(),
            )]),
            zones: vec![ZoneTraffic {
                name: "oxz_nexus_deadbeef0".into(),
                short_name: "nexus".into(),
                rate: BidirectionalRate::default(),
                errors: Default::default(),
            }],
            ..Default::default()
        };
        assert_eq!(sample.total.total_bytes_sec(), 10.0);
        assert!(sample.links.contains_key("net0"));
        assert_eq!(sample.zones[0].short_name, "nexus");
    }

    #[test]
    fn history_is_trailing_sixty_seconds_and_count_bounded() {
        let start = Instant::now();
        let mut history = TrafficHistory::new(3);
        for (secs, total) in [(0, 1.0), (30, 2.0), (59, 3.0), (61, 4.0)] {
            history.push(HistoryPoint {
                captured_at: start + Duration::from_secs(secs),
                rate: BidirectionalRate {
                    rx_bytes_sec: total,
                    ..Default::default()
                },
            });
        }
        assert_eq!(
            history
                .points()
                .iter()
                .map(|p| p.rate.rx_bytes_sec)
                .collect::<Vec<_>>(),
            vec![2.0, 3.0, 4.0]
        );
        history.push(HistoryPoint {
            captured_at: start + Duration::from_secs(62),
            rate: Default::default(),
        });
        assert_eq!(history.len(), 3);

        let mut ordered = TrafficHistory::new(8);
        for secs in [0, 60] {
            ordered.push(HistoryPoint {
                captured_at: start + Duration::from_secs(secs),
                rate: BidirectionalRate {
                    rx_bytes_sec: secs as f64,
                    ..Default::default()
                },
            });
        }
        assert_eq!(ordered.len(), 2, "exact sixty-second boundary is retained");
        ordered.push(HistoryPoint {
            captured_at: start + Duration::from_secs(60),
            rate: BidirectionalRate {
                rx_bytes_sec: 61.0,
                ..Default::default()
            },
        });
        ordered.push(HistoryPoint {
            captured_at: start + Duration::from_secs(30),
            rate: BidirectionalRate {
                rx_bytes_sec: 30.0,
                ..Default::default()
            },
        });
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered.points().back().unwrap().rate.rx_bytes_sec, 61.0);
        assert!(ordered.points().iter().map(|p| p.captured_at).is_sorted());
    }

    #[test]
    fn aggregates_each_scoped_resource_once_and_ranks_zones() {
        let cfg = VoxelConfig::from_toml(
            "[topology]\nracks = 2\nsleds = 3\nrouters = []\n",
        )
        .unwrap();
        let mut model = TelemetryModel::new(resource_descriptors(&cfg), 64);
        let now = Instant::now();
        let sled0 = ResourceId::rack(RackId(0), ResourceKind::Sled, "g0");
        let switch0 =
            ResourceId::rack(RackId(0), ResourceKind::SwitchZone, "g0");
        model.set_current_rate(
            &sled0,
            now,
            BidirectionalRate {
                rx_bytes_sec: 10.0,
                tx_bytes_sec: 20.0,
                ..Default::default()
            },
        );
        model.set_current_rate(
            &switch0,
            now,
            BidirectionalRate {
                rx_bytes_sec: 1.0,
                tx_bytes_sec: 2.0,
                ..Default::default()
            },
        );
        model.rebuild_aggregates(now);
        assert_eq!(model.fleet_rate.total_bytes_sec(), 33.0);
        assert_eq!(model.rack_rates[&RackId(0)].total_bytes_sec(), 33.0);
        assert_eq!(model.rack_rates[&RackId(1)].total_bytes_sec(), 0.0);
        assert_eq!(model.fleet_history.len(), 1);
        model.rebuild_aggregates(now);
        assert_eq!(
            model.fleet_history.len(),
            1,
            "same generation replaces history"
        );
        let next = now + Duration::from_secs(1);
        model.set_current_rate(
            &sled0,
            next,
            BidirectionalRate { rx_bytes_sec: 4.0, ..Default::default() },
        );
        model.rebuild_aggregates(next);
        assert_eq!(
            model.fleet_rate.total_bytes_sec(),
            4.0,
            "stale switch excluded"
        );

        let state = ResourceTelemetry {
            link_rates: BTreeMap::from([
                (
                    "a".into(),
                    BidirectionalRate {
                        rx_bytes_sec: 3.0,
                        ..Default::default()
                    },
                ),
                (
                    "b".into(),
                    BidirectionalRate {
                        rx_bytes_sec: 8.0,
                        ..Default::default()
                    },
                ),
            ]),
            zones: vec![
                ZoneInfo {
                    name: "zone-z".into(),
                    short_name: "z".into(),
                    vnics: vec!["a".into()],
                },
                ZoneInfo {
                    name: "zone-a".into(),
                    short_name: "a".into(),
                    vnics: vec!["b".into(), "b".into(), "a".into()],
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            state
                .top_zones(2)
                .iter()
                .map(|z| z.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zone-a", "zone-z"]
        );
        assert_eq!(state.top_zones(2)[0].rate.total_bytes_sec(), 8.0);
        assert_eq!(state.top_zones(2)[1].rate.total_bytes_sec(), 0.0);
    }

    #[test]
    fn severity_boundaries_are_strict() {
        assert_eq!(
            TrafficSeverity::for_bytes_per_sec(100_000.0),
            TrafficSeverity::Normal
        );
        assert_eq!(
            TrafficSeverity::for_bytes_per_sec(100_000.1),
            TrafficSeverity::Elevated
        );
        assert_eq!(
            TrafficSeverity::for_bytes_per_sec(5_000_000.0),
            TrafficSeverity::Elevated
        );
        assert_eq!(
            TrafficSeverity::for_bytes_per_sec(5_000_000.1),
            TrafficSeverity::High
        );
    }

    #[test]
    fn latest_good_survives_errors_with_deterministic_freshness() {
        let start = Instant::now();
        let mut sample = LatestSample::default();
        assert_eq!(
            sample.freshness(
                start,
                Duration::from_secs(10),
                Duration::from_secs(30)
            ),
            Freshness::Unavailable
        );
        sample.record_success(start, 7u32);
        sample.record_error(start + Duration::from_secs(5), "timeout");
        assert_eq!(sample.good.as_ref().map(|g| g.value), Some(7));
        assert_eq!(sample.latest_error.as_ref().unwrap().message, "timeout");
        assert_eq!(
            sample.freshness(
                start + Duration::from_secs(10),
                Duration::from_secs(10),
                Duration::from_secs(30)
            ),
            Freshness::Fresh
        );
        assert_eq!(
            sample.freshness(
                start + Duration::from_secs(11),
                Duration::from_secs(10),
                Duration::from_secs(30)
            ),
            Freshness::Stale
        );
        assert_eq!(
            sample.freshness(
                start + Duration::from_secs(30),
                Duration::from_secs(10),
                Duration::from_secs(30)
            ),
            Freshness::Stale
        );
        assert_eq!(
            sample.freshness(
                start + Duration::from_secs(31),
                Duration::from_secs(10),
                Duration::from_secs(30)
            ),
            Freshness::Unavailable
        );
        sample.record_success(start + Duration::from_secs(32), 8);
        assert_eq!(sample.good.as_ref().map(|g| g.value), Some(8));
        assert!(
            sample.latest_error.is_none(),
            "success clears the active error"
        );
    }

    #[test]
    fn health_does_not_use_zone_count_as_truth() {
        let diagnostic = HealthDiagnostic {
            sled_agent: Some(ServiceState::Online),
            failed_services: vec![],
            ntp: NtpDiagnostic { stratum: Some(4), synchronized: Some(true) },
            zones: ZoneDiagnostic { zones: vec!["global".into()] },
            notes: vec![],
        };
        assert_eq!(diagnostic.state(), HealthState::Healthy);
        let mut failed = diagnostic.clone();
        failed.failed_services.push("oxide/foo".into());
        assert_eq!(failed.state(), HealthState::Degraded);

        assert_eq!(
            derive_health_state(
                HealthContext::Checking,
                None,
                Freshness::Unavailable
            ),
            HealthState::Checking
        );
        assert_eq!(
            derive_health_state(
                HealthContext::Checking,
                Some(&failed),
                Freshness::Stale
            ),
            HealthState::Checking
        );
        assert_eq!(
            derive_health_state(
                HealthContext::Active,
                Some(&diagnostic),
                Freshness::Fresh
            ),
            HealthState::Healthy
        );
        assert_eq!(
            derive_health_state(
                HealthContext::Active,
                Some(&failed),
                Freshness::Fresh
            ),
            HealthState::Degraded
        );
        assert_eq!(
            derive_health_state(
                HealthContext::Active,
                Some(&diagnostic),
                Freshness::Stale
            ),
            HealthState::Stale
        );
        assert_eq!(
            derive_health_state(
                HealthContext::Active,
                Some(&diagnostic),
                Freshness::Unavailable
            ),
            HealthState::Unavailable
        );
        assert_eq!(
            derive_health_state(HealthContext::Active, None, Freshness::Fresh),
            HealthState::Unknown
        );
        assert_eq!(
            derive_health_state(
                HealthContext::Stopped,
                Some(&failed),
                Freshness::Fresh
            ),
            HealthState::Stopped
        );
        assert_eq!(
            derive_health_state(
                HealthContext::Stopped,
                Some(&failed),
                Freshness::Stale
            ),
            HealthState::Stopped
        );

        let mut hard_failure = diagnostic;
        hard_failure.sled_agent = Some(ServiceState::Maintenance);
        assert_eq!(
            derive_health_state(
                HealthContext::Active,
                Some(&hard_failure),
                Freshness::Fresh
            ),
            HealthState::Failed
        );

        let only_failed_services = HealthDiagnostic {
            failed_services: vec!["oxide/foo".into()],
            ..Default::default()
        };
        assert_eq!(only_failed_services.state(), HealthState::Degraded);
    }
}
