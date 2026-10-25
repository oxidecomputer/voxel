// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Periodic telemetry collection. Probes sleds and routers over Falcon
//! serial and SSH for link counters, addresses, service health and RSS
//! progress, queries Nexus with OxQL for oximeter metrics once the control
//! plane is up, and publishes the samples as [`AppEvent`]s on a bounded
//! schedule.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, ensure};
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use futures::{StreamExt, stream};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use voxel_config::VoxelConfig;

use crate::tui::event::AppEvent;
use crate::tui::nexus::{
    MetricType, NexusClient, OxqlQueryResult, RecoveryLogin, TimeseriesExt,
};
use crate::tui::reconcile::{parse_rss_observation, rss_probe_command};
use crate::tui::telemetry::{
    BidirectionalErrors, BidirectionalRate, CounterSnapshot, HealthDiagnostic,
    OximeterExceptions, RackId, ResourceId, ResourceKind, ResourceTelemetry,
    TrafficSample, TrafficSource, ZfsHeadroom, ZoneCpu, ZoneTraffic,
    parse_chrony_tracking, parse_dladm_zone_vnics, parse_failed_services,
    parse_ipadm_addresses, parse_kstat_link_counters, parse_linux_ip_addresses,
    parse_linux_link_counters, parse_service_state, parse_zone_diagnostics,
    resource_descriptors,
};

const KSTAT: &str = "kstat -p -c net link:::rbytes64 link:::obytes64 link:::ipackets64 link:::opackets64";
const DLADM: &str = "dladm show-linkprop -c -p zone -o link,value";
const IPADM: &str = "ipadm show-addr -p -o addrobj,addr";
const LINUX_COUNTERS: &str = "for n in /sys/class/net/*; do i=${n##*/}; echo $i $(cat $n/statistics/rx_bytes) $(cat $n/statistics/tx_bytes) $(cat $n/statistics/rx_packets) $(cat $n/statistics/tx_packets); done";
const LINUX_ADDRESSES: &str = "ip -o addr show";
const SLED_AGENT_STATE: &str = "svcs -H -o state svc:/oxide/sled-agent:default";
const MAINTENANCE_SERVICES: &str =
    "svcs -H -o state,fmri -a | awk '$1 == \"maintenance\" { print $2 }'";
const OXIMETER_TRAFFIC_METRICS: [&str; 6] = [
    "sled_data_link:bytes_received",
    "sled_data_link:bytes_sent",
    "sled_data_link:packets_received",
    "sled_data_link:packets_sent",
    "sled_data_link:errors_received",
    "sled_data_link:errors_sent",
];
const OXIMETER_INTERVAL: Duration = Duration::from_secs(30);
const NEXUS_READY_TIMEOUT: Duration = Duration::from_secs(10);
const ZONE_CPU_ALIGNMENT: Duration = Duration::from_secs(10);
const ZONE_CPU_QUERY: &str = "get zone:cpu_nsec
    | filter timestamp > @now() - 2m
    | align mean_within(10s)
    | group_by [sled_serial, zone_name, zone_type, state], sum
    | filter datum >= 0.0
    | last 1";
const ZFS_ALLOCATED_QUERY: &str = "get zfs_pool:bytes_allocated | last 1";
const ZFS_TOTAL_QUERY: &str = "get zfs_pool:bytes_total | last 1";
const OXIMETER_EXCEPTIONS_QUERY: &str = r#"{
get oximeter_collector:failed_collections;
get oximeter_collector:database_samples_dropped
} | filter timestamp > @now() - 5m"#;

fn oximeter_traffic_queries(
    rack: RackId,
    sled_serials: &BTreeMap<String, (RackId, String, bool)>,
) -> Vec<String> {
    sled_serials
        .iter()
        .filter(|(_, (sled_rack, _, _))| *sled_rack == rack)
        .flat_map(|(serial, _)| {
            let serial = serde_json::to_string(serial)
                .expect("a string always serializes as JSON");
            OXIMETER_TRAFFIC_METRICS.into_iter().map(move |metric| {
                format!(
                    "get {metric} | filter sled_serial == {serial} | last 1"
                )
            })
        })
        .collect()
}

fn oximeter_due(
    attempts: &BTreeMap<RackId, Instant>,
    rack: RackId,
    now: Instant,
) -> bool {
    !attempts.get(&rack).is_some_and(|previous| {
        now.duration_since(*previous) < OXIMETER_INTERVAL
    })
}

#[derive(Default)]
struct PartialLinkRate {
    rate: BidirectionalRate,
    errors: BidirectionalErrors,
    present: u8,
}

#[derive(Default)]
struct PartialTrafficSample {
    timestamp: Option<DateTime<Utc>>,
    links: BTreeMap<(String, String), PartialLinkRate>,
}

fn oximeter_sample_instant(timestamp: DateTime<Utc>, now: Instant) -> Instant {
    Utc::now()
        .signed_duration_since(timestamp)
        .to_std()
        .ok()
        .and_then(|age| now.checked_sub(age))
        .unwrap_or(now)
}

fn oximeter_resource_ids(
    rack: RackId,
    serial: &str,
    zone: &str,
    sled_serials: &BTreeMap<String, (RackId, String, bool)>,
) -> anyhow::Result<Vec<ResourceId>> {
    let (sled_rack, sled, scrimlet) = sled_serials
        .get(serial)
        .with_context(|| format!("unknown Oximeter sled serial {serial}"))?;
    ensure!(*sled_rack == rack, "Oximeter sled belongs to another rack");
    let mut ids = vec![ResourceId::rack(rack, ResourceKind::Sled, sled)];
    if zone.starts_with("oxz_switch") && *scrimlet {
        ids.push(ResourceId::rack(rack, ResourceKind::SwitchZone, sled));
    }
    Ok(ids)
}

fn parse_oximeter_traffic(
    result: OxqlQueryResult,
    rack: RackId,
    sled_serials: &BTreeMap<String, (RackId, String, bool)>,
    now: Instant,
) -> anyhow::Result<BTreeMap<ResourceId, Vec<(Instant, TrafficSample)>>> {
    const TABLES: [&str; 6] = [
        "sled_data_link:bytes_received",
        "sled_data_link:bytes_sent",
        "sled_data_link:packets_received",
        "sled_data_link:packets_sent",
        "sled_data_link:errors_received",
        "sled_data_link:errors_sent",
    ];
    ensure!(
        TABLES.iter().all(|required| result
            .tables
            .iter()
            .any(|table| table.name == *required)),
        "Oximeter traffic response omitted a required table"
    );
    let mut partial = BTreeMap::<ResourceId, PartialTrafficSample>::new();
    for table in result.tables {
        let field =
            TABLES.iter().position(|name| *name == table.name).ok_or_else(
                || anyhow!("unexpected Oximeter traffic table {}", table.name),
            )?;
        for series in table.timeseries {
            let serial = series
                .field("sled_serial")
                .context("Oximeter link series omitted sled_serial")?;
            let zone = series
                .field("zone_name")
                .context("Oximeter link series omitted zone_name")?;
            let link = series
                .field("link_name")
                .context("Oximeter link series omitted link_name")?;
            let ids = oximeter_resource_ids(rack, serial, zone, sled_serials)?;
            ensure!(
                series.points.values.first().is_some_and(|column| {
                    column.metric_type == MetricType::Delta
                }),
                "Oximeter link counter was not converted to deltas"
            );
            let points = series.integer_points()?;
            ensure!(
                points.len() == 1,
                "Oximeter traffic query did not return the latest sample"
            );
            for (start, timestamp, value) in points {
                let start =
                    start.context("Oximeter link delta omitted start time")?;
                let seconds = timestamp
                    .signed_duration_since(start)
                    .to_std()
                    .context("Oximeter link delta has an invalid interval")?
                    .as_secs_f64();
                ensure!(seconds > 0.0, "Oximeter link delta interval is empty");
                let value = value.context("Oximeter link delta is null")?;
                ensure!(value >= 0, "Oximeter link delta is negative");
                for id in &ids {
                    let sample = partial.entry(id.clone()).or_default();
                    sample.timestamp =
                        Some(sample.timestamp.map_or(timestamp, |current| {
                            current.max(timestamp)
                        }));
                    let rate = sample
                        .links
                        .entry((zone.to_owned(), link.to_owned()))
                        .or_default();
                    let value = value as f64 / seconds;
                    match field {
                        0 => rate.rate.rx_bytes_sec = value,
                        1 => rate.rate.tx_bytes_sec = value,
                        2 => rate.rate.rx_packets_sec = value,
                        3 => rate.rate.tx_packets_sec = value,
                        4 => rate.errors.rx_sec = value,
                        5 => rate.errors.tx_sec = value,
                        _ => unreachable!(),
                    }
                    rate.present |= 1 << field;
                }
            }
        }
    }
    let mut samples =
        BTreeMap::<ResourceId, Vec<(Instant, TrafficSample)>>::new();
    for (id, partial) in partial {
        let timestamp = partial
            .timestamp
            .context("Oximeter traffic sample omitted its timestamp")?;
        let mut sample = TrafficSample {
            source: TrafficSource::Oximeter,
            ..Default::default()
        };
        let mut zones =
            BTreeMap::<String, (BidirectionalRate, BidirectionalErrors)>::new();
        for ((zone, link), rate) in partial.links {
            ensure!(
                rate.present == 0b11_1111,
                "Oximeter link sample is incomplete"
            );
            sample.total += rate.rate;
            sample.errors.rx_sec += rate.errors.rx_sec;
            sample.errors.tx_sec += rate.errors.tx_sec;
            sample.links.insert(format!("{zone}/{link}"), rate.rate);
            if zone != "global" {
                let entry = zones.entry(zone).or_default();
                entry.0 += rate.rate;
                entry.1.rx_sec += rate.errors.rx_sec;
                entry.1.tx_sec += rate.errors.tx_sec;
            }
        }
        sample.zones = zones
            .into_iter()
            .map(|(name, (rate, errors))| ZoneTraffic {
                short_name: name
                    .strip_prefix("oxz_")
                    .unwrap_or(&name)
                    .split('_')
                    .next()
                    .unwrap_or(&name)
                    .to_owned(),
                name,
                rate,
                errors,
            })
            .collect();
        ensure!(!sample.links.is_empty(), "Oximeter traffic sample is empty");
        samples
            .entry(id)
            .or_default()
            .push((oximeter_sample_instant(timestamp, now), sample));
    }
    ensure!(
        !samples.is_empty(),
        "Oximeter traffic response contained no samples"
    );
    Ok(samples)
}

fn parse_zone_cpu(
    result: OxqlQueryResult,
    rack: RackId,
    sled_serials: &BTreeMap<String, (RackId, String, bool)>,
) -> anyhow::Result<Vec<ZoneCpu>> {
    let table = result
        .tables
        .into_iter()
        .find(|table| table.name == "zone:cpu_nsec")
        .context("Oximeter response omitted zone:cpu_nsec")?;
    let mut zones = BTreeMap::<(ResourceId, String, String), ZoneCpu>::new();
    for series in table.timeseries {
        let serial = series
            .field("sled_serial")
            .context("zone CPU series omitted sled_serial")?;
        ensure!(
            sled_serials.get(serial).is_some_and(|(r, _, _)| *r == rack),
            "zone CPU series belongs to another rack"
        );
        let name = series
            .field("zone_name")
            .context("zone CPU series omitted zone_name")?;
        let kind = series
            .field("zone_type")
            .context("zone CPU series omitted zone_type")?;
        let state =
            series.field("state").context("zone CPU series omitted state")?;
        let ids = oximeter_resource_ids(rack, serial, name, sled_serials)?;
        let metric_type = series
            .points
            .values
            .first()
            .map(|value| value.metric_type)
            .context("zone CPU series omitted values")?;
        ensure!(
            matches!(metric_type, MetricType::Delta | MetricType::Gauge),
            "zone CPU metric has an unsupported type"
        );
        let mut latest = None;
        for (start, timestamp, value) in series.numeric_points()? {
            let seconds = match metric_type {
                MetricType::Delta => timestamp
                    .signed_duration_since(
                        start.context("zone CPU delta omitted start time")?,
                    )
                    .to_std()
                    .context("zone CPU delta has an invalid interval")?
                    .as_secs_f64(),
                MetricType::Gauge => ZONE_CPU_ALIGNMENT.as_secs_f64(),
                MetricType::Cumulative => unreachable!(),
            };
            ensure!(seconds > 0.0, "zone CPU delta interval is empty");
            let percent = value
                .map(|value| {
                    ensure!(
                        value.is_finite() && value >= 0.0,
                        "zone CPU value is invalid"
                    );
                    Ok(value / 1_000_000_000.0 / seconds * 100.0)
                })
                .transpose()?;
            if latest.as_ref().is_none_or(|(latest_timestamp, _)| {
                timestamp > *latest_timestamp
            }) {
                latest = Some((timestamp, percent));
            }
        }
        let percent = latest
            .context("zone CPU series has no samples")?
            .1
            .context("latest zone CPU delta is null")?;
        for id in ids {
            let zone = zones
                .entry((id.clone(), name.into(), kind.into()))
                .or_insert_with(|| ZoneCpu {
                    id,
                    name: name.into(),
                    kind: kind.into(),
                    user_percent: 0.0,
                    system_percent: 0.0,
                    wait_percent: 0.0,
                });
            match state {
                "user" => zone.user_percent += percent,
                "sys" => zone.system_percent += percent,
                "waitrq" => zone.wait_percent += percent,
                _ => return Err(anyhow!("unknown zone CPU state {state}")),
            }
        }
    }
    Ok(zones.into_values().collect())
}

fn parse_zfs_headroom(
    result: OxqlQueryResult,
    rack: RackId,
    sled_serials: &BTreeMap<String, (RackId, String, bool)>,
) -> anyhow::Result<Vec<ZfsHeadroom>> {
    let mut pools =
        BTreeMap::<(ResourceId, String), (Option<u64>, Option<u64>)>::new();
    for table in result.tables {
        let field = match table.name.as_str() {
            "zfs_pool:bytes_allocated" => 0,
            "zfs_pool:bytes_total" => 1,
            _ => continue,
        };
        for series in table.timeseries {
            ensure!(
                series
                    .points
                    .values
                    .first()
                    .is_some_and(|v| v.metric_type == MetricType::Gauge),
                "ZFS headroom metric is not a gauge"
            );
            let serial = series
                .field("sled_serial")
                .context("ZFS series omitted sled_serial")?;
            let (sled_rack, sled, _) = sled_serials
                .get(serial)
                .context("ZFS series has unknown sled_serial")?;
            ensure!(*sled_rack == rack, "ZFS series belongs to another rack");
            let pool = series
                .field("pool_name")
                .context("ZFS series omitted pool_name")?;
            let value = series
                .integer_points()?
                .into_iter()
                .filter_map(|(_, _, value)| value)
                .next_back()
                .context("ZFS series has no value")?;
            ensure!(value >= 0, "ZFS byte gauge is negative");
            let pair = pools
                .entry((
                    ResourceId::rack(rack, ResourceKind::Sled, sled),
                    pool.into(),
                ))
                .or_default();
            if field == 0 {
                pair.0 = Some(value as u64)
            } else {
                pair.1 = Some(value as u64)
            }
        }
    }
    pools
        .into_iter()
        .map(|((id, pool), (allocated, total))| {
            let allocated_bytes =
                allocated.context("ZFS pool omitted allocated bytes")?;
            let total_bytes = total.context("ZFS pool omitted total bytes")?;
            ensure!(
                allocated_bytes <= total_bytes,
                "ZFS pool allocation exceeds total"
            );
            Ok(ZfsHeadroom { id, pool, allocated_bytes, total_bytes })
        })
        .collect()
}

fn parse_oximeter_exceptions(
    result: OxqlQueryResult,
) -> anyhow::Result<OximeterExceptions> {
    let mut exceptions = OximeterExceptions::default();
    for table in result.tables {
        let target = match table.name.as_str() {
            "oximeter_collector:failed_collections" => {
                &mut exceptions.failed_collections
            }
            "oximeter_collector:database_samples_dropped" => {
                &mut exceptions.dropped_samples
            }
            _ => continue,
        };
        for series in table.timeseries {
            ensure!(
                series
                    .points
                    .values
                    .first()
                    .is_some_and(|v| v.metric_type == MetricType::Delta),
                "Oximeter exception counter was not converted to deltas"
            );
            for (_, _, value) in series.integer_points()? {
                if let Some(value) = value {
                    ensure!(value >= 0, "Oximeter exception delta is negative");
                    *target = target.saturating_add(value as u64);
                }
            }
        }
    }
    Ok(exceptions)
}

async fn ssh_capture_bounded(
    ip: &str,
    remote: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let askpass = crate::util::temp_dir().join("voxel-tui-empty-askpass.sh");
    if !askpass.exists() {
        std::fs::write(&askpass, "#!/bin/sh\necho\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &askpass,
                std::fs::Permissions::from_mode(0o700),
            )?;
        }
    }
    let mut command = tokio::process::Command::new("ssh");
    command
        .kill_on_drop(true)
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .stdin(Stdio::null())
        .args(crate::net::EPHEMERAL_HOST_OPTS)
        .args([
            "-o",
            "PreferredAuthentications=password",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "NumberOfPasswordPrompts=1",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg(format!("root@{ip}"))
        .arg(remote);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| anyhow!("SSH command on {ip} timed out"))??;
    if !output.status.success() {
        return Err(anyhow!(
            "SSH command on {ip} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(Into::into)
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum NodeTarget {
    Sled { name: String },
    SwitchZone { host: String, zone: String },
    Router { name: String },
}

pub trait NodeExecutor: Send + Sync + 'static {
    fn execute<'a>(
        &'a self,
        target: &'a NodeTarget,
        command: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<String>>;
}

/// Production execution boundary: Falcon is used only for identity and bounded,
/// one-time host-LAN discovery. Recurring illumos probes use bounded SSH.
#[derive(Default)]
struct IpCache {
    resolved: BTreeMap<String, String>,
    in_flight:
        BTreeMap<String, watch::Receiver<Option<Result<String, String>>>>,
}

impl IpCache {
    fn from_config_and_addresses(
        config: &VoxelConfig,
        mut resolved: BTreeMap<String, String>,
    ) -> Self {
        if config.external.isolated() {
            resolved.extend(config.static_external_ips());
        }
        Self { resolved, in_flight: BTreeMap::new() }
    }

    fn address_for_probe(
        &self,
        host: &str,
        resumed: bool,
    ) -> anyhow::Result<Option<String>> {
        match self.resolved.get(host) {
            Some(ip) => Ok(Some(ip.clone())),
            None if resumed => Err(anyhow!(
                "telemetry for {host} is unavailable after resuming without a saved address"
            )),
            None => Ok(None),
        }
    }
}

pub(crate) struct FalconExecutor {
    topo: Arc<crate::topo::Topo>,
    ips: Arc<Mutex<IpCache>>,
    router_commands: Arc<Mutex<BTreeSet<String>>>,
    serial_tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    resumed: bool,
    timeout: Duration,
}

impl FalconExecutor {
    pub(crate) fn new(
        topo: crate::topo::Topo,
        config: &VoxelConfig,
        addresses: BTreeMap<String, String>,
        resumed: bool,
        timeout: Duration,
    ) -> Self {
        Self {
            topo: Arc::new(topo),
            ips: Arc::new(Mutex::new(IpCache::from_config_and_addresses(
                config, addresses,
            ))),
            router_commands: Default::default(),
            serial_tasks: Default::default(),
            resumed,
            timeout,
        }
    }

    fn track_serial_task(&self, task: tokio::task::JoinHandle<()>) {
        // Registration must not yield after spawning: otherwise cancellation
        // could drop the JoinHandle and leave untracked Falcon serial work.
        let mut tasks = self.serial_tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    /// Wait for Falcon serial workers that reached their caller's timeout.
    /// Service supervisors call this before admitting mutating work so a timed
    /// out read-only probe cannot outlive telemetry or reconciliation ownership.
    pub(crate) async fn drain_serial_tasks(&self) {
        loop {
            let tasks = {
                let mut tracked = self.serial_tasks.lock().unwrap();
                if tracked.is_empty() {
                    break;
                }
                std::mem::take(&mut *tracked)
            };
            for task in tasks {
                let _ = task.await;
            }
        }
    }

    pub(crate) async fn resolved_addresses(&self) -> BTreeMap<String, String> {
        self.ips.lock().await.resolved.clone()
    }

    async fn sled_ip(&self, host: &str) -> anyhow::Result<String> {
        let mut cache = self.ips.lock().await;
        if let Some(ip) = cache.address_for_probe(host, self.resumed)? {
            return Ok(ip);
        }
        let node = self
            .topo
            .node_ref(host)
            .ok_or_else(|| anyhow!("unknown Falcon sled {host}"))?;
        let mut result = if let Some(in_flight) = cache.in_flight.get(host) {
            in_flight.clone()
        } else {
            let (sender, receiver) = watch::channel(None);
            cache.in_flight.insert(host.into(), receiver.clone());
            let topo = self.topo.clone();
            let ips = self.ips.clone();
            let host = host.to_owned();
            let task = tokio::spawn(async move {
                let result =
                    crate::net::node_external_ip(&topo.runner, node, false)
                        .await
                        .map_err(|error| error.to_string());
                let mut cache = ips.lock().await;
                cache.in_flight.remove(&host);
                if let Ok(ip) = &result {
                    cache.resolved.insert(host, ip.clone());
                }
                let _ = sender.send(Some(result));
            });
            self.track_serial_task(task);
            receiver
        };
        drop(cache);

        let wait = async {
            loop {
                if let Some(result) = result.borrow().clone() {
                    return result.map_err(|message| anyhow!(message));
                }
                result
                    .changed()
                    .await
                    .map_err(|_| anyhow!("host-LAN discovery task stopped"))?;
            }
        };
        tokio::time::timeout(self.timeout, wait)
            .await
            .map_err(|_| anyhow!("host-LAN discovery for {host} timed out"))?
    }

    async fn router_exec(
        &self,
        name: &str,
        command: &str,
    ) -> anyhow::Result<String> {
        if self.resumed {
            return Err(anyhow!(
                "router telemetry is unavailable after resuming a TUI session"
            ));
        }
        let node = self
            .topo
            .node_ref(name)
            .ok_or_else(|| anyhow!("unknown Falcon router {name}"))?;
        {
            let mut commands = self.router_commands.lock().await;
            if !commands.insert(name.into()) {
                return Err(anyhow!(
                    "a previous router command on {name} is still running"
                ));
            }
        }
        let (sender, receiver) = oneshot::channel();
        let topo = self.topo.clone();
        let commands = self.router_commands.clone();
        let name = name.to_owned();
        let task_name = name.clone();
        let command = command.to_owned();
        let task = tokio::spawn(async move {
            let result = topo
                .runner
                .exec(node, &command)
                .await
                .map_err(|error| error.to_string());
            commands.lock().await.remove(&task_name);
            let _ = sender.send(result);
        });
        self.track_serial_task(task);
        tokio::time::timeout(self.timeout, receiver)
            .await
            .map_err(|_| anyhow!("router command on {name} timed out"))?
            .map_err(|_| anyhow!("router command task on {name} stopped"))?
            .map_err(|error| anyhow!("router command on {name}: {error}"))
    }
}

impl NodeExecutor for FalconExecutor {
    fn execute<'a>(
        &'a self,
        target: &'a NodeTarget,
        command: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            match target {
                NodeTarget::Sled { name } => {
                    let ip = self.sled_ip(name).await?;
                    ssh_capture_bounded(&ip, command, self.timeout).await
                }
                NodeTarget::SwitchZone { host, zone: _ } => {
                    let ip = self.sled_ip(host).await?;
                    ssh_capture_bounded(
                        &ip,
                        &crate::net::zlogin(command),
                        self.timeout,
                    )
                    .await
                }
                NodeTarget::Router { name } => {
                    self.router_exec(name, command).await
                }
            }
        })
    }
}

#[derive(Clone, Debug)]
pub struct ResourceTarget {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub target: NodeTarget,
}

#[derive(Clone, Debug)]
pub struct RssTarget {
    pub rack: RackId,
    pub target: NodeTarget,
    pub bootstrap_addr: String,
}

#[derive(Clone, Debug)]
pub struct CollectorTargets {
    pub resources: Vec<ResourceTarget>,
    pub rss: Vec<RssTarget>,
}

impl CollectorTargets {
    pub fn from_config(config: &VoxelConfig) -> Self {
        let resources = resource_descriptors(config)
            .into_iter()
            .map(|descriptor| ResourceTarget {
                id: descriptor.id,
                kind: descriptor.kind,
                target: match descriptor.kind {
                    ResourceKind::Sled => {
                        NodeTarget::Sled { name: descriptor.name }
                    }
                    ResourceKind::SwitchZone => NodeTarget::SwitchZone {
                        host: descriptor
                            .host
                            .expect("switch descriptor has host"),
                        zone: "oxz_switch".into(),
                    },
                    ResourceKind::Router => {
                        NodeTarget::Router { name: descriptor.name }
                    }
                },
            })
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let rss = config
            .sleds()
            .into_iter()
            .filter(|sled| sled.rss && seen.insert(sled.rack))
            .map(|sled| RssTarget {
                rack: RackId(sled.rack),
                target: NodeTarget::Sled { name: sled.name.clone() },
                bootstrap_addr: sled.bootstrap_addr(),
            })
            .collect();
        Self { resources, rss }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SchedulerConfig {
    pub traffic_interval: Duration,
    pub health_interval: Duration,
    pub concurrency: usize,
}

impl SchedulerConfig {
    pub fn new(
        traffic_interval: Duration,
        health_interval: Duration,
        concurrency: usize,
    ) -> anyhow::Result<Self> {
        if traffic_interval.is_zero()
            || health_interval.is_zero()
            || concurrency == 0
        {
            return Err(anyhow!(
                "collector intervals and concurrency must be nonzero"
            ));
        }
        Ok(Self { traffic_interval, health_interval, concurrency })
    }
}

pub struct Collector<E> {
    executor: Arc<E>,
    targets: CollectorTargets,
    baselines: Arc<Mutex<BTreeMap<ResourceId, ResourceTelemetry>>>,
    nexus: BTreeMap<RackId, Arc<NexusClient>>,
    sled_serials: BTreeMap<String, (RackId, String, bool)>,
    oximeter_attempts: Mutex<BTreeMap<RackId, Instant>>,
    oximeter_diagnostic_attempts: Mutex<BTreeMap<RackId, Instant>>,
}

impl<E: NodeExecutor> Collector<E> {
    pub fn new(executor: Arc<E>, config: &VoxelConfig) -> anyhow::Result<Self> {
        let login = RecoveryLogin::from_config(config);
        let nexus = if cfg!(test) {
            BTreeMap::new()
        } else if let Some(login) = login {
            (0..config.topology.racks())
                .map(|rack| {
                    Ok((
                        RackId(rack),
                        Arc::new(NexusClient::for_rack(
                            config,
                            rack,
                            login.clone(),
                            Duration::from_secs(60),
                        )?),
                    ))
                })
                .collect::<anyhow::Result<_>>()?
        } else {
            BTreeMap::new()
        };
        let sled_serials = config
            .sleds()
            .into_iter()
            .map(|sled| {
                (
                    sled.serial_number,
                    (RackId(sled.rack), sled.name, sled.scrimlet),
                )
            })
            .collect();
        Ok(Self {
            executor,
            targets: CollectorTargets::from_config(config),
            baselines: Default::default(),
            nexus,
            sled_serials,
            oximeter_attempts: Default::default(),
            oximeter_diagnostic_attempts: Default::default(),
        })
    }

    async fn call(
        &self,
        cancel: &CancellationToken,
        target: &NodeTarget,
        command: &str,
    ) -> anyhow::Result<String> {
        if cancel.is_cancelled() {
            return Err(anyhow!("cancelled"));
        }
        if matches!(target, NodeTarget::Router { .. }) {
            // Falcon serial exec is bounded but not cancellation-safe on drop.
            // Finish this safe-boundary call, then observe cancellation before
            // publishing or beginning another command.
            let result = self.executor.execute(target, command).await;
            if cancel.is_cancelled() {
                Err(anyhow!("cancelled"))
            } else {
                result
            }
        } else {
            tokio::select! { biased;
                _ = cancel.cancelled() => Err(anyhow!("cancelled")),
                result = self.executor.execute(target, command) => result,
            }
        }
    }

    async fn send(
        sender: &mpsc::Sender<AppEvent>,
        cancel: &CancellationToken,
        event: AppEvent,
    ) -> bool {
        tokio::select! { biased;
            _ = cancel.cancelled() => false,
            result = sender.send(event) => result.is_ok(),
        }
    }

    async fn collect_traffic_with_limit(
        &self,
        sender: &mpsc::Sender<AppEvent>,
        cancel: &CancellationToken,
        concurrency: usize,
    ) -> bool {
        let at = Instant::now();
        let jobs = self.targets.resources.clone().into_iter().map(
            |resource| async move {
                let result = self.traffic_one(&resource, at, cancel).await;
                (resource.id, result)
            },
        );
        // Finish all command futures before awaiting bounded event sends. This
        // prevents send cancellation/closure from dropping an in-flight Falcon
        // serial future held by the unordered job stream.
        let results = stream::iter(jobs)
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
        for event in results.into_iter().map(|(id, result)| match result {
            Ok(sample) => AppEvent::Traffic { id, at, sample },
            Err(error) => {
                AppEvent::TrafficFailed { id, at, message: error.to_string() }
            }
        }) {
            if !Self::send(sender, cancel, event).await {
                return false;
            }
        }
        true
    }

    async fn collect_oximeter(
        &self,
        sender: &mpsc::Sender<AppEvent>,
        cancel: &CancellationToken,
    ) -> bool {
        let at = Instant::now();
        let racks = self
            .nexus
            .iter()
            .map(|(rack, client)| (*rack, Arc::clone(client)))
            .collect::<Vec<_>>();
        let concurrency = racks.len().max(1);
        let jobs = racks.into_iter().map(|(rack, client)| async move {
            self.collect_oximeter_rack(sender, rack, &client, at, cancel).await
        });
        stream::iter(jobs)
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .all(|keep_running| keep_running)
    }

    async fn collect_oximeter_rack(
        &self,
        sender: &mpsc::Sender<AppEvent>,
        rack: RackId,
        client: &NexusClient,
        at: Instant,
        cancel: &CancellationToken,
    ) -> bool {
        let traffic_due = {
            let attempts = self.oximeter_attempts.lock().await;
            oximeter_due(&attempts, rack, at)
        };
        let diagnostics_due = {
            let attempts = self.oximeter_diagnostic_attempts.lock().await;
            oximeter_due(&attempts, rack, at)
        };
        if !traffic_due && !diagnostics_due {
            return true;
        }

        let readiness =
            tokio::time::timeout(NEXUS_READY_TIMEOUT, client.ready(cancel))
                .await
                .map_err(|_| {
                    anyhow!(
                        "Nexus readiness timed out after {:?}",
                        NEXUS_READY_TIMEOUT
                    )
                })
                .and_then(|result| result);
        if let Err(error) = readiness {
            let completed_at = Instant::now();
            if traffic_due {
                self.oximeter_attempts.lock().await.insert(rack, completed_at);
            }
            if diagnostics_due {
                self.oximeter_diagnostic_attempts
                    .lock()
                    .await
                    .insert(rack, completed_at);
            }
            let message = error.to_string();
            if !Self::send(
                sender,
                cancel,
                AppEvent::NexusUnavailable {
                    rack,
                    at: completed_at,
                    message: message.clone(),
                },
            )
            .await
            {
                return false;
            }
            return !traffic_due
                || Self::send(
                    sender,
                    cancel,
                    AppEvent::OximeterTrafficFailed {
                        rack,
                        at: completed_at,
                        message: format!("Oximeter traffic: {message}"),
                    },
                )
                .await;
        }
        if !Self::send(sender, cancel, AppEvent::NexusAvailable { rack, at })
            .await
        {
            return false;
        }

        let traffic = async {
            !traffic_due
                || self
                    .collect_oximeter_traffic(sender, rack, client, at, cancel)
                    .await
        };
        let diagnostics = async {
            !diagnostics_due
                || self
                    .collect_oximeter_diagnostics(
                        sender, rack, client, at, cancel,
                    )
                    .await
        };
        let (traffic, diagnostics) = tokio::join!(traffic, diagnostics);
        traffic && diagnostics
    }

    async fn collect_oximeter_traffic(
        &self,
        sender: &mpsc::Sender<AppEvent>,
        rack: RackId,
        client: &NexusClient,
        at: Instant,
        cancel: &CancellationToken,
    ) -> bool {
        let result = self.oximeter_traffic(rack, client, at, cancel).await;
        let completed_at = Instant::now();
        self.oximeter_attempts.lock().await.insert(rack, completed_at);
        let events = match result {
            Ok(samples) => samples
                .into_iter()
                .map(|(id, samples)| AppEvent::OximeterTraffic {
                    id,
                    at: completed_at,
                    samples,
                })
                .collect(),
            Err(error) => vec![AppEvent::OximeterTrafficFailed {
                rack,
                at: completed_at,
                message: format!("Oximeter traffic: {error}"),
            }],
        };
        for event in events {
            if !Self::send(sender, cancel, event).await {
                return false;
            }
        }
        true
    }

    async fn oximeter_traffic(
        &self,
        rack: RackId,
        client: &NexusClient,
        now: Instant,
        cancel: &CancellationToken,
    ) -> anyhow::Result<BTreeMap<ResourceId, Vec<(Instant, TrafficSample)>>>
    {
        let queries = oximeter_traffic_queries(rack, &self.sled_serials);
        let mut results = Vec::with_capacity(queries.len());
        // Keeping each response below one sled's data avoids stalled Nexus
        // streams while retaining link and zone detail.
        for query in queries {
            results.push(client.query(&query, cancel).await?);
        }
        let result = OxqlQueryResult {
            tables: results
                .into_iter()
                .flat_map(|result| result.tables)
                .collect(),
        };
        parse_oximeter_traffic(result, rack, &self.sled_serials, now)
    }

    async fn traffic_one(
        &self,
        resource: &ResourceTarget,
        at: Instant,
        cancel: &CancellationToken,
    ) -> anyhow::Result<TrafficSample> {
        let counters = match resource.kind {
            ResourceKind::Router => parse_linux_link_counters(
                &self.call(cancel, &resource.target, LINUX_COUNTERS).await?,
            ),
            _ => parse_kstat_link_counters(
                &self.call(cancel, &resource.target, KSTAT).await?,
            ),
        };
        if counters.is_empty() {
            return Err(anyhow!("counter output contained no complete links"));
        }
        let zones = if resource.kind == ResourceKind::Sled {
            parse_dladm_zone_vnics(
                &self.call(cancel, &resource.target, DLADM).await?,
            )
        } else {
            vec![]
        };
        let mut baselines = self.baselines.lock().await;
        let state = baselines.entry(resource.id.clone()).or_default();
        state.zones = zones;
        state.update(CounterSnapshot::new(at, counters));
        Ok(TrafficSample {
            total: state.total_rate(),
            links: state.link_rates.clone(),
            zones: state.top_zones(usize::MAX),
            ..Default::default()
        })
    }

    async fn collect_health_with_limit(
        &self,
        sender: &mpsc::Sender<AppEvent>,
        cancel: &CancellationToken,
        concurrency: usize,
    ) -> bool {
        let at = Instant::now();
        let resource_jobs = self.targets.resources.clone().into_iter().map(
            |resource| async move {
                self.health_events(&resource, at, cancel).await
            },
        );
        let mut events = stream::iter(resource_jobs)
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let rss_jobs =
            self.targets.rss.clone().into_iter().map(|rss| async move {
                self.rss_event(&rss, at, cancel).await
            });
        events.extend(
            stream::iter(rss_jobs)
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await,
        );
        for event in events {
            if !Self::send(sender, cancel, event).await {
                return false;
            }
        }
        true
    }

    async fn collect_oximeter_diagnostics(
        &self,
        sender: &mpsc::Sender<AppEvent>,
        rack: RackId,
        client: &NexusClient,
        at: Instant,
        cancel: &CancellationToken,
    ) -> bool {
        let mut diagnostics =
            futures::stream::FuturesUnordered::<BoxFuture<'_, AppEvent>>::new();
        diagnostics.push(Box::pin(async {
            match client.query(ZONE_CPU_QUERY, cancel).await.and_then(
                |result| parse_zone_cpu(result, rack, &self.sled_serials),
            ) {
                Ok(zones) => AppEvent::ZoneCpu { rack, at, zones },
                Err(error) => AppEvent::ZoneCpuFailed {
                    rack,
                    at,
                    message: error.to_string(),
                },
            }
        }));
        diagnostics.push(Box::pin(async {
            let zfs = async {
                let (allocated, total) = tokio::join!(
                    client.query(ZFS_ALLOCATED_QUERY, cancel),
                    client.query(ZFS_TOTAL_QUERY, cancel),
                );
                let mut result = allocated?;
                result.tables.extend(total?.tables);
                Ok::<_, anyhow::Error>(result)
            }
            .await
            .and_then(|result| {
                parse_zfs_headroom(result, rack, &self.sled_serials)
            });
            match zfs {
                Ok(pools) => AppEvent::ZfsHeadroom { rack, at, pools },
                Err(error) => AppEvent::ZfsHeadroomFailed {
                    rack,
                    at,
                    message: error.to_string(),
                },
            }
        }));
        diagnostics.push(Box::pin(async {
            match client
                .query(OXIMETER_EXCEPTIONS_QUERY, cancel)
                .await
                .and_then(parse_oximeter_exceptions)
            {
                Ok(exceptions) => {
                    AppEvent::OximeterExceptions { rack, at, exceptions }
                }
                Err(error) => AppEvent::OximeterExceptionsFailed {
                    rack,
                    at,
                    message: error.to_string(),
                },
            }
        }));
        while let Some(event) = diagnostics.next().await {
            if !Self::send(sender, cancel, event).await {
                return false;
            }
        }
        self.oximeter_diagnostic_attempts
            .lock()
            .await
            .insert(rack, Instant::now());
        true
    }

    async fn health_events(
        &self,
        resource: &ResourceTarget,
        at: Instant,
        cancel: &CancellationToken,
    ) -> Vec<AppEvent> {
        let addresses = match resource.kind {
            ResourceKind::Router => self
                .call(cancel, &resource.target, LINUX_ADDRESSES)
                .await
                .map(|s| parse_linux_ip_addresses(&s)),
            _ => self
                .call(cancel, &resource.target, IPADM)
                .await
                .map(|s| parse_ipadm_addresses(&s)),
        };
        let mut events = vec![match addresses {
            Ok(addresses) => {
                AppEvent::Addresses { id: resource.id.clone(), at, addresses }
            }
            Err(error) => AppEvent::AddressesFailed {
                id: resource.id.clone(),
                at,
                message: error.to_string(),
            },
        }];
        if resource.kind == ResourceKind::Sled {
            let diagnostic = self.sled_health(resource, cancel).await;
            events.push(match diagnostic {
                Ok(diagnostic) => {
                    AppEvent::Health { id: resource.id.clone(), at, diagnostic }
                }
                Err(error) => AppEvent::HealthFailed {
                    id: resource.id.clone(),
                    at,
                    message: error.to_string(),
                },
            });
        }
        events
    }

    async fn rss_event(
        &self,
        rss: &RssTarget,
        at: Instant,
        cancel: &CancellationToken,
    ) -> AppEvent {
        let command = rss_probe_command(&rss.bootstrap_addr);
        match self.call(cancel, &rss.target, &command).await {
            Ok(body) if !body.trim().is_empty() => AppEvent::Rss {
                rack: rss.rack,
                at,
                observation: parse_rss_observation(&body),
            },
            Ok(_) => AppEvent::RssFailed {
                rack: rss.rack,
                at,
                message: "empty RSS response".into(),
            },
            Err(error) => AppEvent::RssFailed {
                rack: rss.rack,
                at,
                message: error.to_string(),
            },
        }
    }

    async fn sled_health(
        &self,
        resource: &ResourceTarget,
        cancel: &CancellationToken,
    ) -> anyhow::Result<HealthDiagnostic> {
        let service =
            self.call(cancel, &resource.target, SLED_AGENT_STATE).await?;
        let sled_agent = parse_service_state(&service)
            .ok_or_else(|| anyhow!("invalid sled-agent service state"))?;
        let failed_services = parse_failed_services(
            &self.call(cancel, &resource.target, MAINTENANCE_SERVICES).await?,
        );
        let zones = parse_zone_diagnostics(
            &self
                .call(
                    cancel,
                    &resource.target,
                    "zoneadm list -cp | cut -d: -f2",
                )
                .await?,
        );
        let ntp_zone = zones
            .zones
            .iter()
            .find(|zone| zone.starts_with("oxz_ntp_"))
            .ok_or_else(|| anyhow!("NTP zone not found"))?;
        let ntp = parse_chrony_tracking(
            &self
                .call(
                    cancel,
                    &resource.target,
                    &format!(
                        "zlogin {ntp_zone} chronyc -n tracking 2>/dev/null"
                    ),
                )
                .await?,
        );
        if ntp.synchronized.is_none() {
            return Err(anyhow!("invalid NTP tracking state"));
        }
        Ok(HealthDiagnostic {
            sled_agent: Some(sled_agent),
            failed_services,
            ntp,
            zones,
            notes: vec![],
        })
    }

    pub(crate) async fn run_gated(
        self: Arc<Self>,
        config: SchedulerConfig,
        sender: mpsc::Sender<AppEvent>,
        cancel: CancellationToken,
        gate: Arc<tokio::sync::Semaphore>,
    ) {
        let probes = async {
            let now = tokio::time::Instant::now();
            let mut traffic_due = now;
            let mut health_due = now;
            loop {
                let next_due = traffic_due.min(health_due);
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep_until(next_due) => {},
                }
                let permit = tokio::select! {
                    _ = cancel.cancelled() => break,
                    permit = gate.clone().acquire_owned() => match permit {
                        Ok(permit) => permit,
                        Err(_) => break,
                    },
                };
                let health_cadence = health_due < traffic_due;
                let keep_running = if health_cadence {
                    self.collect_health_with_limit(
                        &sender,
                        &cancel,
                        config.concurrency,
                    )
                    .await
                } else {
                    self.collect_traffic_with_limit(
                        &sender,
                        &cancel,
                        config.concurrency,
                    )
                    .await
                };
                let completed = tokio::time::Instant::now();
                if health_cadence {
                    health_due = completed + config.health_interval;
                } else {
                    traffic_due = completed + config.traffic_interval;
                }
                drop(permit);
                if !keep_running {
                    break;
                }
            }
        };
        let oximeter = async {
            let mut interval = tokio::time::interval(config.traffic_interval);
            interval.set_missed_tick_behavior(
                tokio::time::MissedTickBehavior::Skip,
            );
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => {},
                }
                if sender.is_closed()
                    || !self.collect_oximeter(&sender, &cancel).await
                {
                    break;
                }
            }
        };
        tokio::join!(probes, oximeter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use crate::tui::reconcile::ObservedDeploymentState;
    use crate::tui::telemetry::HealthState;
    use crate::tui::test_support::{config, recovery};
    use crate::tui::ui::monitor::resource_health_state;
    use httptest::matchers::any;
    use httptest::responders::{delay_and_then, status_code};
    use httptest::{Expectation, Server};
    use indoc::indoc;
    use oxide_client::types::{
        FieldValue, OxqlTable, Points, Timeseries, ValueArray, Values,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    /// How long a step may take before it counts as hung. Only a failing
    /// test waits this long.
    const HANG: Duration = Duration::from_secs(5);

    const ZONEADM: &str = "zoneadm list -cp | cut -d: -f2";
    const NTP_TRACKING: &str =
        "zlogin oxz_ntp_deadbeef chronyc -n tracking 2>/dev/null";
    const RSS_PROBE: &str = "curl -s --max-time 5 ";

    /// What a healthy node answers; every counter reads `counter`.
    fn response(counter: u64, command: &str) -> String {
        match command {
            KSTAT => format!(
                "0:link:net0:rbytes64\t{counter}\n0:link:net0:obytes64\t{counter}\n0:link:net0:ipackets64\t{counter}\n0:link:net0:opackets64\t{counter}\n"
            ),
            DLADM => "net0:oxz_nexus_deadbeef0\n".into(),
            LINUX_COUNTERS => {
                format!("eth0 {counter} {counter} {counter} {counter}\n")
            }
            SLED_AGENT_STATE => "online\n".into(),
            ZONEADM => "global\noxz_ntp_deadbeef\n".into(),
            NTP_TRACKING => "Stratum : 3\nLeap status : Normal\n".into(),
            _ if command.starts_with(RSS_PROBE) => {
                r#"{"status":"initializing","step":"sled_init"}"#.into()
            }
            // No addresses, and no service in maintenance.
            _ => String::new(),
        }
    }

    /// What a scripted command does instead of answering with `response`.
    #[derive(Clone, Copy)]
    enum Reply {
        Output(&'static str),
        Fail(&'static str),
        Delay(Duration),
        /// Wait until `FakeExecutor::release` is cancelled.
        Held,
    }

    /// The node it applies to (any if `None`), a command prefix, and the reply.
    type Rule = (Option<&'static str>, &'static str, Reply);

    /// A node executor that answers from `response`, except where a rule
    /// scripts another reply, and records every command it is given.
    #[derive(Default)]
    struct FakeExecutor {
        rules: Vec<Rule>,
        generation: AtomicU64,
        commands: std::sync::Mutex<Vec<String>>,
        release: CancellationToken,
    }

    impl FakeExecutor {
        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    fn fake(rules: Vec<Rule>) -> Arc<FakeExecutor> {
        Arc::new(FakeExecutor { rules, ..Default::default() })
    }

    impl NodeExecutor for FakeExecutor {
        fn execute<'a>(
            &'a self,
            target: &'a NodeTarget,
            command: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<String>> {
            self.commands.lock().unwrap().push(command.to_owned());
            let node = match target {
                NodeTarget::Sled { name } | NodeTarget::Router { name } => name,
                NodeTarget::SwitchZone { host, .. } => host,
            };
            let reply = self
                .rules
                .iter()
                .find(|(on, prefix, _)| {
                    on.is_none_or(|on| on == node)
                        && command.starts_with(prefix)
                })
                .map(|(_, _, reply)| *reply);
            Box::pin(async move {
                match reply {
                    Some(Reply::Output(output)) => return Ok(output.into()),
                    Some(Reply::Fail(message)) => return Err(anyhow!(message)),
                    Some(Reply::Delay(delay)) => {
                        tokio::time::sleep(delay).await
                    }
                    Some(Reply::Held) => self.release.cancelled().await,
                    None => {}
                }
                Ok(response(self.generation.load(Ordering::SeqCst), command))
            })
        }
    }

    /// A Nexus client pointed at a test server.
    fn nexus(server: &Server, timeout: Duration) -> Arc<NexusClient> {
        let endpoint = reqwest::Url::parse(&server.url_str("/")).unwrap();
        Arc::new(
            NexusClient::new(vec![endpoint], None, recovery(), timeout)
                .unwrap(),
        )
    }

    async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(HANG, async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}: not reached within {HANG:?}"));
    }

    /// A collector over explicit targets, with no Nexus and no serials.
    fn collector(
        executor: Arc<FakeExecutor>,
        resources: Vec<ResourceTarget>,
    ) -> Collector<FakeExecutor> {
        Collector {
            executor,
            targets: CollectorTargets { resources, rss: vec![] },
            baselines: Default::default(),
            nexus: Default::default(),
            sled_serials: Default::default(),
            oximeter_attempts: Default::default(),
            oximeter_diagnostic_attempts: Default::default(),
        }
    }

    /// The fleet's resources become typed probe targets with the right
    /// address source. A first traffic round reports every resource at the
    /// round's timestamp; the next turns counter deltas into rates over the
    /// elapsed time. A router whose counters come back empty fails alone.
    #[tokio::test]
    async fn traffic_round_reports_rates_for_every_resource() {
        let fleet = config(indoc! {r#"
            [topology]
            racks = 2
            sleds = 3
            routers = ["ce", "cr1"]
        "#});

        let targets = CollectorTargets::from_config(&fleet);
        let sled = |name: &str| NodeTarget::Sled { name: name.into() };
        let switch = |host: &str| NodeTarget::SwitchZone {
            host: host.into(),
            zone: "oxz_switch".into(),
        };
        let router = |name: &str| NodeTarget::Router { name: name.into() };
        assert_eq!(
            targets
                .rss
                .iter()
                .map(|rss| (rss.rack, rss.target.clone()))
                .collect::<Vec<_>>(),
            [(RackId(0), sled("g0")), (RackId(1), sled("g3"))]
        );
        // Order is not part of the contract, so compare sorted by name.
        let mut probed: Vec<_> = targets
            .resources
            .iter()
            .map(|resource| resource.target.clone())
            .collect();
        probed.sort_by_key(|target| format!("{target:?}"));
        let mut expected_targets = vec![
            sled("g0"),
            switch("g0"),
            sled("g1"),
            sled("g2"),
            switch("g2"),
            sled("g3"),
            switch("g3"),
            sled("g4"),
            sled("g5"),
            switch("g5"),
            router("ce"),
            router("cr1"),
        ];
        expected_targets.sort_by_key(|target| format!("{target:?}"));
        assert_eq!(probed, expected_targets);

        let isolated = config(indoc! {r#"
            [topology]
            racks = 1
            sleds = 3
            routers = ["ce"]
            [external]
            mode = "isolated"
        "#});
        let default = VoxelConfig::default();
        let addresses = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(host, ip)| ((*host).to_owned(), (*ip).to_owned()))
                .collect::<BTreeMap<_, _>>()
        };
        let none = BTreeMap::new;
        let saved = addresses(&[("g0", "192.168.1.184")]);
        // (case, config, saved addresses, resumed, addresses the cache
        // holds, and g0's probe address or the error it gives)
        let cases = [
            (
                "isolated",
                &isolated,
                none(),
                false,
                addresses(&[
                    ("g0", "172.30.199.10"),
                    ("g1", "172.30.199.11"),
                    ("g2", "172.30.199.12"),
                    ("ce", "172.30.199.13"),
                ]),
                Ok(Some("172.30.199.10".to_owned())),
            ),
            ("default", &default, none(), false, none(), Ok(None)),
            (
                "resumed",
                &default,
                saved.clone(),
                true,
                saved,
                Ok(Some("192.168.1.184".to_owned())),
            ),
            (
                "resumed without a saved address",
                &default,
                none(),
                true,
                none(),
                Err("telemetry for g0 is unavailable after resuming without a saved address".to_owned()),
            ),
        ];
        for (case, config, saved, resumed, holds, g0) in cases {
            let cache = IpCache::from_config_and_addresses(config, saved);
            let probe = cache
                .address_for_probe("g0", resumed)
                .map_err(|error| error.to_string());
            assert_eq!((cache.resolved, probe), (holds, g0), "{case}");
        }

        let fake = fake(vec![(Some("cr1"), LINUX_COUNTERS, Reply::Output(""))]);
        let collector = Collector::new(fake.clone(), &fleet).unwrap();
        let cr1 = ResourceId::fleet(ResourceKind::Router, "cr1");
        let (sender, mut receiver) = mpsc::channel(64);
        let cancel = CancellationToken::new();

        assert!(
            collector.collect_traffic_with_limit(&sender, &cancel, 4).await
        );
        let mut round1: Vec<_> =
            std::iter::from_fn(|| receiver.try_recv().ok())
                .map(|event| match event {
                    AppEvent::Traffic { id, at, sample } => {
                        (id, at, Some(sample.total.total_bytes_sec()))
                    }
                    AppEvent::TrafficFailed { id, at, .. } => (id, at, None),
                    other => panic!("unexpected event {other:?}"),
                })
                .collect();
        assert!(
            round1.iter().all(|(_, at, _)| *at == round1[0].1),
            "round 1 events do not share the round's timestamp"
        );
        round1.sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected: Vec<_> = targets
            .resources
            .iter()
            .map(|resource| {
                let id = resource.id.clone();
                // Without a baseline there is no rate yet.
                let rate = (id != cr1).then_some(0.0);
                (id, rate)
            })
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            round1
                .into_iter()
                .map(|(id, _, rate)| (id, rate))
                .collect::<Vec<_>>(),
            expected
        );

        // Round 2 comes twenty seconds after round 1, with every counter
        // 1,000 higher; so long an interval keeps the rate whole however
        // slowly the round runs.
        for state in collector.baselines.lock().await.values_mut() {
            state.baseline.as_mut().unwrap().captured_at -=
                Duration::from_secs(20);
        }
        fake.generation.store(1_000, Ordering::SeqCst);
        assert!(
            collector.collect_traffic_with_limit(&sender, &cancel, 4).await
        );
        let whole = |rate: BidirectionalRate| rate.total_bytes_sec().round();
        let round2: BTreeMap<_, _> =
            std::iter::from_fn(|| receiver.try_recv().ok())
                .filter_map(|event| match event {
                    AppEvent::Traffic { id, sample, .. } => Some((
                        id,
                        (
                            whole(sample.total),
                            sample.links.into_keys().collect::<Vec<_>>(),
                            sample
                                .zones
                                .into_iter()
                                .map(|zone| (zone.name, whole(zone.rate)))
                                .collect::<Vec<_>>(),
                        ),
                    )),
                    _ => None,
                })
                .collect();
        // 50 bytes a second each way, with a sled's VNIC traffic
        // attributed to its zone.
        let expected = targets
            .resources
            .iter()
            .filter(|resource| resource.id != cr1)
            .map(|resource| {
                let (links, zones) = match resource.kind {
                    ResourceKind::Sled => (
                        vec!["net0".to_owned()],
                        vec![("oxz_nexus_deadbeef0".to_owned(), 100.0)],
                    ),
                    ResourceKind::SwitchZone => (vec!["net0".into()], vec![]),
                    ResourceKind::Router => (vec!["eth0".into()], vec![]),
                };
                (resource.id.clone(), (100.0, links, zones))
            })
            .collect();
        assert_eq!(round2, expected);
    }

    /// After a traffic round and a health round, every resource and rack has
    /// been observed and the monitor shows it: rack 0 is healthy, while rack
    /// 1 has a service in maintenance and an unreachable RSS sled, neither of
    /// which hides rack 0's observations.
    #[tokio::test]
    async fn health_round_reaches_the_operator_for_every_resource_and_rack() {
        let config = config(indoc! {r#"
            [topology]
            racks = 2
            sleds = 2
            routers = ["ce"]
        "#});
        let broken = Reply::Output("svc:/oxide/broken:default\n");
        let fake = fake(vec![
            (Some("g2"), MAINTENANCE_SERVICES, broken),
            (Some("g3"), MAINTENANCE_SERVICES, broken),
            (Some("g2"), RSS_PROBE, Reply::Fail("unreachable")),
        ]);
        let collector = Collector::new(fake, &config).unwrap();
        let (sender, mut receiver) = mpsc::channel(64);
        let cancel = CancellationToken::new();

        assert!(
            collector.collect_traffic_with_limit(&sender, &cancel, 2).await
        );
        assert!(collector.collect_health_with_limit(&sender, &cancel, 2).await);
        let events: Vec<_> =
            std::iter::from_fn(|| receiver.try_recv().ok()).collect();

        let resources = resource_descriptors(&config);
        let addressed: BTreeSet<_> = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::Addresses { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            addressed,
            resources.iter().map(|resource| resource.id.clone()).collect(),
            "resources whose addresses are published"
        );
        let rss: BTreeMap<_, _> = events
            .iter()
            .filter_map(|event| match event {
                AppEvent::Rss { rack, .. } => Some((*rack, None)),
                AppEvent::RssFailed { rack, message, .. } => {
                    Some((*rack, Some(message.as_str())))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            rss,
            BTreeMap::from([
                (RackId(0), None),
                (RackId(1), Some("unreachable"))
            ]),
            "RSS failures by rack"
        );

        let mut app = App::new(resources.clone(), 8, 8);
        app.deployment.observed = ObservedDeploymentState::Running;
        for event in events {
            app.update(event);
        }
        app.update(AppEvent::Tick { now: Instant::now() });
        let rack1_sleds = [
            ResourceId::rack(RackId(1), ResourceKind::Sled, "g2"),
            ResourceId::rack(RackId(1), ResourceKind::Sled, "g3"),
        ];
        let shown: Vec<_> = resources
            .iter()
            .map(|resource| {
                (resource.id.clone(), resource_health_state(&app, &resource.id))
            })
            .collect();
        let expected: Vec<_> = resources
            .iter()
            .map(|resource| {
                let state = if rack1_sleds.contains(&resource.id) {
                    HealthState::Degraded
                } else {
                    HealthState::Healthy
                };
                (resource.id.clone(), state)
            })
            .collect();
        assert_eq!(shown, expected, "operator view");
    }

    /// Wherever a round is blocked, cancelling it ends the round promptly
    /// without publishing anything further, except that a Falcon serial call
    /// is not safe to drop, so the round waits for it first. A round also
    /// stops once the UI stops receiving.
    #[tokio::test]
    async fn collection_stops_cleanly_when_cancelled_or_the_ui_closes() {
        let sled = || ResourceTarget {
            id: ResourceId::rack(RackId(0), ResourceKind::Sled, "g0"),
            kind: ResourceKind::Sled,
            target: NodeTarget::Sled { name: "g0".into() },
        };
        let router = || ResourceTarget {
            id: ResourceId::fleet(ResourceKind::Router, "ce"),
            kind: ResourceKind::Router,
            target: NodeTarget::Router { name: "ce".into() },
        };
        // (case, resources, the command held until release, and whether the
        // event channel starts full)
        let rows = [
            ("sending into a full event channel", vec![sled()], None, true),
            ("in-flight SSH probe", vec![sled()], Some(KSTAT), false),
            (
                "in-flight router call",
                vec![router()],
                Some(LINUX_COUNTERS),
                false,
            ),
            (
                "in-flight router call with a full event channel",
                vec![sled(), router()],
                Some(LINUX_COUNTERS),
                true,
            ),
        ];
        for (case, resources, held, queued_event) in rows {
            let fake = fake(
                held.map(|command| (None, command, Reply::Held))
                    .into_iter()
                    .collect(),
            );
            let limit = resources.len();
            let collector = collector(fake.clone(), resources);
            let (sender, mut receiver) = mpsc::channel(1);
            if queued_event {
                sender
                    .send(AppEvent::Tick { now: Instant::now() })
                    .await
                    .unwrap();
            }
            let cancel = CancellationToken::new();
            let task = tokio::spawn({
                let cancel = cancel.clone();
                async move {
                    collector
                        .collect_traffic_with_limit(&sender, &cancel, limit)
                        .await
                }
            });
            // Unheld, the round blocks sending the sample its last command
            // produced.
            let blocked_on = held.unwrap_or(DLADM);
            wait_until(case, || {
                fake.commands().iter().any(|c| c == blocked_on)
            })
            .await;

            cancel.cancel();
            if held == Some(LINUX_COUNTERS) {
                tokio::task::yield_now().await;
                assert!(
                    !task.is_finished(),
                    "{case}: returned before the router call finished"
                );
                fake.release.cancel();
            }

            assert!(
                matches!(tokio::time::timeout(HANG, task).await, Ok(Ok(false))),
                "{case}: the round did not stop and report cancellation"
            );
            assert_eq!(
                std::iter::from_fn(|| receiver.try_recv().ok()).count(),
                usize::from(queued_event),
                "{case}: events left in the channel"
            );
        }

        let collector = collector(fake(vec![]), vec![sled()]);
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let cancel = CancellationToken::new();
        let round = collector.collect_traffic_with_limit(&sender, &cancel, 1);
        assert!(
            matches!(tokio::time::timeout(HANG, round).await, Ok(false)),
            "UI closed: the round did not stop"
        );
    }

    /// Oximeter is asked at most once per interval per rack, and not at all
    /// past an unavailable Nexus. The scheduler keeps its concurrency limit
    /// and never runs the traffic and health cadences at once, yet slow
    /// probes and a stalled Nexus hold up neither cadence.
    #[tokio::test]
    async fn slow_or_failed_sources_do_not_block_the_others() {
        let start = Instant::now();
        let asked = BTreeMap::from([(RackId(0), start)]);
        let almost = start + OXIMETER_INTERVAL - Duration::from_nanos(1);
        assert_eq!(
            [
                oximeter_due(&BTreeMap::new(), RackId(0), start),
                oximeter_due(&asked, RackId(0), almost),
                oximeter_due(&asked, RackId(0), start + OXIMETER_INTERVAL),
                oximeter_due(&asked, RackId(1), start),
            ],
            [true, false, true, true],
            "Oximeter due: never asked, within the interval, after it, \
             another rack"
        );

        let config = config(indoc! {r#"
            [topology]
            racks = 2
            sleds = 2
            routers = ["ce"]
        "#});

        // Exactly one request: the readiness check, and no query after it.
        let mut unavailable = Server::run();
        unavailable.expect(
            Expectation::matching(any()).respond_with(status_code(503)),
        );
        let collector = Collector {
            nexus: BTreeMap::from([(
                RackId(0),
                nexus(&unavailable, Duration::from_secs(1)),
            )]),
            ..Collector::new(fake(vec![]), &config).unwrap()
        };
        let (sender, mut receiver) = mpsc::channel(16);
        assert!(
            collector
                .collect_oximeter(&sender, &CancellationToken::new())
                .await
        );
        let events: Vec<_> =
            std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        assert!(
            matches!(
                events[..],
                [
                    AppEvent::NexusUnavailable { rack: RackId(0), .. },
                    AppEvent::OximeterTrafficFailed { rack: RackId(0), .. },
                ]
            ),
            "unavailable Nexus: {events:?}"
        );
        unavailable.verify_and_clear();

        let gate = || Arc::new(tokio::sync::Semaphore::new(1));

        let held = fake(vec![
            (None, KSTAT, Reply::Held),
            (None, LINUX_COUNTERS, Reply::Held),
        ]);
        let collector =
            Arc::new(Collector::new(held.clone(), &config).unwrap());
        let hour = Duration::from_secs(3_600);
        let (sender, _receiver) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        tokio::spawn(collector.run_gated(
            SchedulerConfig::new(hour, hour, 2).unwrap(),
            sender,
            cancel.clone(),
            gate(),
        ));
        wait_until("two traffic probes running", || held.commands().len() >= 2)
            .await;
        let commands = held.commands();
        assert!(
            commands.len() == 2
                && commands.iter().all(|c| *c == KSTAT || *c == LINUX_COUNTERS),
            "probes beyond the limit, or health during traffic: {commands:?}"
        );
        cancel.cancel();

        // Traffic probes outlast the traffic interval, so traffic is always
        // due, and rack 0's Nexus never answers.
        let stalled = Server::run();
        stalled.expect(Expectation::matching(any()).times(..).respond_with(
            delay_and_then(Duration::from_secs(3_600), status_code(200)),
        ));
        let slow = Reply::Delay(Duration::from_millis(20));
        let collector = Arc::new(Collector {
            // Longer than HANG, so a stall that blocked direct probes would
            // outlast it.
            nexus: BTreeMap::from([(
                RackId(0),
                nexus(&stalled, Duration::from_secs(60)),
            )]),
            ..Collector::new(
                fake(vec![(None, KSTAT, slow), (None, LINUX_COUNTERS, slow)]),
                &config,
            )
            .unwrap()
        });
        let (sender, mut receiver) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(
            collector.run_gated(
                SchedulerConfig::new(
                    Duration::from_millis(5),
                    Duration::from_millis(15),
                    2,
                )
                .unwrap(),
                sender,
                cancel.clone(),
                gate(),
            ),
        );
        // HANG is shorter than NEXUS_READY_TIMEOUT, so a cadence blocked
        // behind the stalled readiness check cannot meet it.
        let mut traffic_rounds = BTreeSet::new();
        let mut health_round = false;
        let _ = tokio::time::timeout(HANG, async {
            while traffic_rounds.len() < 3 || !health_round {
                match receiver.recv().await {
                    Some(AppEvent::Traffic { at, .. }) => {
                        traffic_rounds.insert(at);
                    }
                    Some(AppEvent::Rss { .. }) => health_round = true,
                    Some(_) => {}
                    None => break,
                }
            }
        })
        .await;
        assert!(
            traffic_rounds.len() >= 3,
            "direct traffic lost its cadence while Nexus stalls"
        );
        assert!(health_round, "health starved while traffic is always due");
        cancel.cancel();
        assert!(
            tokio::time::timeout(HANG, task).await.is_ok_and(|r| r.is_ok()),
            "the scheduler did not stop when cancelled"
        );
    }

    // OxQL responses are built from the client's generated types, so a change
    // to the response schema at the pinned omicron rev breaks them at compile
    // time.

    fn string(value: &str) -> FieldValue {
        FieldValue::String(value.into())
    }

    fn points(
        start_times: Option<&[&str]>,
        timestamps: &[&str],
        metric_type: MetricType,
        values: ValueArray,
    ) -> Points {
        let at =
            |times: &[&str]| times.iter().map(|t| t.parse().unwrap()).collect();
        Points {
            start_times: start_times.map(at),
            timestamps: at(timestamps),
            values: vec![Values { metric_type, values }],
        }
    }

    fn series(fields: &[(&str, FieldValue)], points: Points) -> Timeseries {
        Timeseries {
            fields: fields
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
            points,
        }
    }

    fn table(name: &str, timeseries: Vec<Timeseries>) -> OxqlTable {
        OxqlTable { name: name.into(), timeseries }
    }

    /// One delta per traffic metric for sled 2FAKE000, valued by position,
    /// each starting at 00:00:00 and ending a second later than the one
    /// before, from 00:00:10; `null_table` reports a missing sample instead.
    fn traffic_result(null_table: Option<&str>) -> OxqlQueryResult {
        let tables = OXIMETER_TRAFFIC_METRICS
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let value =
                    (null_table != Some(name)).then_some((index + 1) as i64);
                let end = format!("2026-08-17T00:00:{}Z", 10 + index);
                table(
                    name,
                    vec![series(
                        &[
                            ("sled_serial", string("2FAKE000")),
                            ("zone_name", string("oxz_nexus_deadbeef0")),
                            ("link_name", string("net0")),
                        ],
                        points(
                            Some(&["2026-08-17T00:00:00Z"]),
                            &[end.as_str()],
                            MetricType::Delta,
                            ValueArray::Integer(vec![value]),
                        ),
                    )],
                )
            })
            .collect();
        OxqlQueryResult { tables }
    }

    /// Every OxQL query the collector sends, as text, so a query edit shows up
    /// as a reviewed diff. Regenerate with `EXPECTORATE=overwrite`.
    #[test]
    fn oxql_queries_match_their_golden_file() {
        let serials = BTreeMap::from([
            ("2FAKE000".into(), (RackId(0), "g0".into(), true)),
            ("2FAKE001".into(), (RackId(0), "g1".into(), false)),
        ]);
        let mut text = String::from(
            "# traffic, rack 0 with sleds 2FAKE000 and 2FAKE001\n",
        );
        for query in oximeter_traffic_queries(RackId(0), &serials) {
            text.push_str(&query);
            text.push('\n');
        }
        for (label, query) in [
            ("zone CPU", ZONE_CPU_QUERY),
            ("ZFS allocated", ZFS_ALLOCATED_QUERY),
            ("ZFS total", ZFS_TOTAL_QUERY),
            ("oximeter exceptions", OXIMETER_EXCEPTIONS_QUERY),
        ] {
            text.push_str(&format!("\n# {label}\n{query}\n"));
        }
        expectorate::assert_contents("tests/output/oxql-queries.txt", &text);
    }

    /// Traffic queries stay one sled per response, and each kind of Oximeter
    /// response parses into the samples the monitor shows, rejecting a
    /// partial one.
    #[test]
    fn oximeter_responses_parse_into_samples() {
        let serials = BTreeMap::from([
            ("2FAKE000".into(), (RackId(0), "g0".into(), true)),
            ("2FAKE\"001".into(), (RackId(0), "g1".into(), false)),
            ("2FAKE002".into(), (RackId(1), "g2".into(), false)),
        ]);
        let queries = oximeter_traffic_queries(RackId(0), &serials);
        let filters: BTreeSet<_> = queries
            .iter()
            .filter_map(|query| query.split(" | ").nth(1))
            .collect();
        assert_eq!(
            filters,
            BTreeSet::from([
                r#"filter sled_serial == "2FAKE000""#,
                r#"filter sled_serial == "2FAKE\"001""#,
            ]),
            "traffic queries are each filtered to one of rack 0's sleds"
        );

        let g0 = ResourceId::rack(RackId(0), ResourceKind::Sled, "g0");
        let zone = "oxz_nexus_deadbeef0";
        let parse_traffic = |null_table| {
            parse_oximeter_traffic(
                traffic_result(null_table),
                RackId(0),
                &serials,
                Instant::now(),
            )
        };
        let samples: Vec<_> = parse_traffic(None)
            .expect("a complete traffic response parses")
            .into_iter()
            .flat_map(|(id, samples)| {
                samples.into_iter().map(move |(_, sample)| (id.clone(), sample))
            })
            .collect();
        // Each metric's delta is its position over its own interval, and the
        // metrics' different timestamps still make one sample.
        let rate = BidirectionalRate {
            rx_bytes_sec: 1.0 / 10.0,
            tx_bytes_sec: 2.0 / 11.0,
            rx_packets_sec: 3.0 / 12.0,
            tx_packets_sec: 4.0 / 13.0,
        };
        let errors =
            BidirectionalErrors { rx_sec: 5.0 / 14.0, tx_sec: 6.0 / 15.0 };
        let sample = TrafficSample {
            total: rate,
            links: BTreeMap::from([(format!("{zone}/net0"), rate)]),
            zones: vec![ZoneTraffic {
                name: zone.into(),
                short_name: "nexus".into(),
                rate,
                errors,
            }],
            errors,
            source: TrafficSource::Oximeter,
        };
        assert_eq!(samples, [(g0.clone(), sample)]);
        assert!(
            parse_traffic(Some("sled_data_link:bytes_sent")).is_err(),
            "a null metric must reject the partial sample"
        );

        // Each shape's latest point is two seconds of CPU in a ten-second
        // window, so every state reads 20%.
        let delta_pair = points(
            Some(&["2026-08-17T00:00:00Z", "2026-08-17T00:00:10Z"]),
            &["2026-08-17T00:00:10Z", "2026-08-17T00:00:20Z"],
            MetricType::Delta,
            ValueArray::Integer(vec![Some(1_000_000_000), Some(2_000_000_000)]),
        );
        let aligned_gauge = points(
            None,
            &["2026-08-17T00:00:10Z"],
            MetricType::Gauge,
            ValueArray::Double(vec![Some(2_000_000_000.0)]),
        );
        let cpu = ZoneCpu {
            id: g0.clone(),
            name: zone.into(),
            kind: "nexus".into(),
            user_percent: 20.0,
            system_percent: 20.0,
            wait_percent: 20.0,
        };
        for (case, points) in
            [("delta pair", delta_pair), ("aligned gauge", aligned_gauge)]
        {
            let timeseries = ["user", "sys", "waitrq"]
                .into_iter()
                .map(|state| {
                    series(
                        &[
                            ("sled_serial", string("2FAKE000")),
                            ("zone_name", string(zone)),
                            ("zone_type", string("nexus")),
                            ("state", string(state)),
                        ],
                        points.clone(),
                    )
                })
                .collect();
            let parsed = parse_zone_cpu(
                OxqlQueryResult {
                    tables: vec![table("zone:cpu_nsec", timeseries)],
                },
                RackId(0),
                &serials,
            )
            .unwrap_or_else(|error| panic!("zone CPU, {case}: {error:#}"));
            assert_eq!(parsed, std::slice::from_ref(&cpu), "zone CPU, {case}");
        }

        let tables =
            [("zfs_pool:bytes_allocated", 40), ("zfs_pool:bytes_total", 100)]
                .into_iter()
                .map(|(name, value)| {
                    table(
                        name,
                        vec![series(
                            &[
                                ("sled_serial", string("2FAKE000")),
                                ("pool_name", string("oxp_test")),
                            ],
                            points(
                                None,
                                &["2026-08-17T00:00:10Z"],
                                MetricType::Gauge,
                                ValueArray::Integer(vec![Some(value)]),
                            ),
                        )],
                    )
                })
                .collect();
        let zfs =
            parse_zfs_headroom(OxqlQueryResult { tables }, RackId(0), &serials)
                .expect("ZFS allocated and total gauges parse");
        assert_eq!(
            zfs,
            [ZfsHeadroom {
                id: g0,
                pool: "oxp_test".into(),
                allocated_bytes: 40,
                total_bytes: 100,
            }]
        );

        let tables = [
            ("oximeter_collector:failed_collections", 2),
            ("oximeter_collector:database_samples_dropped", 3),
        ]
        .into_iter()
        .map(|(name, value)| {
            table(
                name,
                vec![series(
                    &[("collector_port", FieldValue::U16(12223))],
                    points(
                        Some(&["2026-08-17T00:00:00Z"]),
                        &["2026-08-17T00:00:10Z"],
                        MetricType::Delta,
                        ValueArray::Integer(vec![Some(value)]),
                    ),
                )],
            )
        })
        .collect();
        assert_eq!(
            parse_oximeter_exceptions(OxqlQueryResult { tables })
                .expect("Oximeter exception deltas parse"),
            OximeterExceptions { failed_collections: 2, dropped_samples: 3 }
        );
    }
}
