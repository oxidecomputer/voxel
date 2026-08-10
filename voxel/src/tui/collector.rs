#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use voxel_config::VoxelConfig;

    fn response(generation: u64, command: &str) -> String {
        if command == KSTAT {
            format!(
                "0:link:net0:rbytes64\t{}\n0:link:net0:obytes64\t{}\n0:link:net0:ipackets64\t{}\n0:link:net0:opackets64\t{}\n",
                100 + generation,
                200 + generation,
                10 + generation,
                20 + generation
            )
        } else if command == DLADM {
            "net0:oxz_nexus_deadbeef0\n".into()
        } else if command == LINUX_COUNTERS {
            format!(
                "eth0 {} {} {} {}\n",
                100 + generation,
                200 + generation,
                10 + generation,
                20 + generation
            )
        } else if command == IPADM {
            "net0/v4:192.0.2.10/24\nnet0/v6:fd00::10/64\n".into()
        } else if command == LINUX_ADDRESSES {
            "1: lo inet 127.0.0.1/8 scope host lo\n2: eth0 inet6 fd00::20/64 scope global\n".into()
        } else if command == SLED_AGENT_STATE {
            "online\n".into()
        } else if command == MAINTENANCE_SERVICES {
            "svc:/oxide/broken:default\n".into()
        } else if command == "zoneadm list -cp | cut -d: -f2" {
            "global\noxz_ntp_deadbeef\n".into()
        } else if command
            == "zlogin oxz_ntp_deadbeef chronyc -n tracking 2>/dev/null"
        {
            "Stratum : 3\nLeap status : Normal\n".into()
        } else if command.starts_with("curl -s --max-time 5 ") {
            r#"{"status":"initializing","step":"sled_init"}"#.into()
        } else {
            String::new()
        }
    }

    struct FakeExecutor {
        generation: AtomicU64,
        calls: std::sync::Mutex<Vec<(NodeTarget, String)>>,
    }

    impl NodeExecutor for FakeExecutor {
        fn execute<'a>(
            &'a self,
            target: &'a NodeTarget,
            command: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<String>> {
            self.calls
                .lock()
                .unwrap()
                .push((target.clone(), command.to_owned()));
            let n = self.generation.load(Ordering::SeqCst);
            Box::pin(async move { Ok(response(n, command)) })
        }
    }

    struct Gate {
        open: AtomicBool,
        notify: Notify,
    }

    impl Gate {
        fn closed() -> Self {
            Self { open: AtomicBool::new(false), notify: Notify::new() }
        }

        async fn wait(&self) {
            while !self.open.load(Ordering::Acquire) {
                let notified = self.notify.notified();
                if self.open.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        }

        fn release(&self) {
            self.open.store(true, Ordering::Release);
            self.notify.notify_waiters();
        }
    }

    struct ActiveCall<'a> {
        executor: &'a ControlledExecutor,
        traffic: bool,
    }

    impl Drop for ActiveCall<'_> {
        fn drop(&mut self) {
            self.executor.active.fetch_sub(1, Ordering::SeqCst);
            if self.traffic {
                self.executor.traffic_active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    struct ControlledExecutor {
        calls: std::sync::Mutex<Vec<(NodeTarget, String)>>,
        active: AtomicUsize,
        max_active: AtomicUsize,
        traffic_active: AtomicUsize,
        overlap: AtomicBool,
        block_traffic: bool,
        block_router: bool,
        gate: Gate,
    }

    impl ControlledExecutor {
        fn new(block_traffic: bool, block_router: bool) -> Self {
            Self {
                calls: Default::default(),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                traffic_active: AtomicUsize::new(0),
                overlap: AtomicBool::new(false),
                block_traffic,
                block_router,
                gate: Gate::closed(),
            }
        }

        fn has_command(&self, command: &str) -> bool {
            self.calls.lock().unwrap().iter().any(|(_, c)| c == command)
        }
    }

    impl NodeExecutor for ControlledExecutor {
        fn execute<'a>(
            &'a self,
            target: &'a NodeTarget,
            command: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<String>> {
            self.calls
                .lock()
                .unwrap()
                .push((target.clone(), command.to_owned()));
            Box::pin(async move {
                let traffic = command == KSTAT
                    || command == DLADM
                    || command == LINUX_COUNTERS;
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(active, Ordering::SeqCst);
                if traffic {
                    self.traffic_active.fetch_add(1, Ordering::SeqCst);
                } else if self.traffic_active.load(Ordering::SeqCst) > 0 {
                    self.overlap.store(true, Ordering::SeqCst);
                }
                let _active = ActiveCall { executor: self, traffic };
                let should_block = (self.block_traffic && traffic)
                    || (self.block_router
                        && matches!(target, NodeTarget::Router { .. }));
                if should_block {
                    self.gate.wait().await;
                }
                Ok(response(0, command))
            })
        }
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition was not reached");
    }

    fn collector_with_targets<E: NodeExecutor>(
        executor: Arc<E>,
        resources: Vec<ResourceTarget>,
        concurrency: usize,
    ) -> Arc<Collector<E>> {
        Arc::new(Collector {
            executor,
            targets: CollectorTargets { resources, rss: vec![] },
            baselines: Default::default(),
            concurrency,
        })
    }

    fn resource_target(name: &str, kind: ResourceKind) -> ResourceTarget {
        let id = match kind {
            ResourceKind::Router => ResourceId::fleet(kind, name),
            _ => ResourceId::rack(RackId(0), kind, name),
        };
        ResourceTarget {
            id,
            kind,
            target: match kind {
                ResourceKind::Sled => NodeTarget::Sled { name: name.into() },
                ResourceKind::SwitchZone => NodeTarget::SwitchZone {
                    host: name.into(),
                    zone: "oxz_switch".into(),
                },
                ResourceKind::Router => {
                    NodeTarget::Router { name: name.into() }
                }
            },
        }
    }

    #[test]
    fn discovers_typed_targets_and_one_rss_target_per_rack() {
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 2\nsleds = 3\nrouters = [\"ce\", \"cr1\"]\n",
        )
        .unwrap();
        let targets = CollectorTargets::from_config(&config);
        assert_eq!(targets.resources.len(), 12);
        assert_eq!(targets.rss.len(), 2);
        assert_eq!(targets.rss[0].rack, crate::tui::telemetry::RackId(0));
        assert_eq!(
            targets.rss[0].target,
            NodeTarget::Sled { name: "g0".into() }
        );
        assert_eq!(
            targets.rss[1].target,
            NodeTarget::Sled { name: "g3".into() }
        );
        assert!(targets.resources.iter().any(|target| matches!(
            &target.target,
            NodeTarget::SwitchZone { host, zone } if host == "g5" && zone == "oxz_switch"
        )));
        assert!(targets.resources.iter().any(|target| matches!(
            &target.target,
            NodeTarget::Router { name } if name == "ce"
        )));
    }

    #[test]
    fn scheduler_configuration_rejects_zero_values() {
        assert!(
            SchedulerConfig::new(
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(1),
                1,
            )
            .is_err()
        );
        assert!(
            SchedulerConfig::new(
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(1),
                0,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn first_sample_is_zero_with_detail_and_all_resource_kinds_emit() {
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 1\nsleds = 1\nrouters = [\"ce\"]\n",
        )
        .unwrap();
        let fake = Arc::new(FakeExecutor {
            generation: AtomicU64::new(0),
            calls: Default::default(),
        });
        let collector = Collector::new(fake.clone(), &config, 2).unwrap();
        let (sender, mut receiver) = mpsc::channel(8);
        assert!(
            collector.collect_traffic(&sender, &CancellationToken::new()).await
        );
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        assert_eq!(events.len(), 3);
        let mut kinds = std::collections::BTreeSet::new();
        let mut timestamp = None;
        for event in events {
            let AppEvent::Traffic { id, at, sample } = event else {
                panic!("unexpected failure event")
            };
            assert_eq!(sample.total.total_bytes_sec(), 0.0);
            assert!(sample.links.is_empty());
            assert_eq!(timestamp.get_or_insert(at), &at);
            kinds.insert(id.kind);
        }
        assert_eq!(kinds.len(), 3);
        assert!(fake.calls.lock().unwrap().iter().any(
            |(target, _)| matches!(target, NodeTarget::SwitchZone { host, .. } if host == "g0")
        ));
        let calls = fake.calls.lock().unwrap();
        assert!(calls.iter().any(|(target, command)|
            matches!(target, NodeTarget::Sled { name } if name == "g0")
                && command == "kstat -p -c net link:::rbytes64 link:::obytes64 link:::ipackets64 link:::opackets64"));
        assert!(calls.iter().any(
            |(target, command)| matches!(target, NodeTarget::Sled { name } if name == "g0")
                && command == "dladm show-linkprop -c -p zone -o link,value"
        ));
        assert!(calls.iter().any(|(target, command)|
            matches!(target, NodeTarget::SwitchZone { host, zone } if host == "g0" && zone == "oxz_switch")
                && command == "kstat -p -c net link:::rbytes64 link:::obytes64 link:::ipackets64 link:::opackets64"));
        assert!(calls.iter().any(
            |(target, command)| matches!(target, NodeTarget::Router { name } if name == "ce")
                && command == LINUX_COUNTERS
        ));
    }

    #[tokio::test]
    async fn second_generation_uses_elapsed_time_and_retains_link_and_zone_detail()
     {
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 1\nsleds = 1\nrouters = []\n",
        )
        .unwrap();
        let fake = Arc::new(FakeExecutor {
            generation: AtomicU64::new(0),
            calls: Default::default(),
        });
        let collector = Collector::new(fake.clone(), &config, 2).unwrap();
        let (sender, mut receiver) = mpsc::channel(16);
        assert!(
            collector.collect_traffic(&sender, &CancellationToken::new()).await
        );
        while receiver.try_recv().is_ok() {}
        for state in collector.baselines.lock().await.values_mut() {
            state.baseline.as_mut().unwrap().captured_at -=
                Duration::from_secs(2);
        }
        fake.generation.store(100, Ordering::SeqCst);
        assert!(
            collector.collect_traffic(&sender, &CancellationToken::new()).await
        );
        let sample = std::iter::from_fn(|| receiver.try_recv().ok())
            .find_map(|event| match event {
                AppEvent::Traffic { id, sample, .. }
                    if id.kind == ResourceKind::Sled =>
                {
                    Some(sample)
                }
                _ => None,
            })
            .unwrap();
        assert!((90.0..110.0).contains(&sample.total.total_bytes_sec()));
        assert!(sample.links.contains_key("net0"));
        assert_eq!(sample.zones[0].name, "oxz_nexus_deadbeef0");
        assert!(sample.zones[0].rate.total_bytes_sec() > 0.0);
    }

    #[tokio::test]
    async fn addresses_sled_health_and_multi_rack_rss_publish_typed_results_in_order()
     {
        assert_eq!(
            SLED_AGENT_STATE,
            "svcs -H -o state svc:/oxide/sled-agent:default"
        );
        assert_eq!(
            MAINTENANCE_SERVICES,
            "svcs -H -o state,fmri -a | awk '$1 == \"maintenance\" { print $2 }'"
        );
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 2\nsleds = 2\nrouters = [\"ce\"]\n",
        )
        .unwrap();
        let fake = Arc::new(FakeExecutor {
            generation: AtomicU64::new(0),
            calls: Default::default(),
        });
        let collector = Collector::new(fake.clone(), &config, 2).unwrap();
        let (sender, mut receiver) = mpsc::channel(32);
        assert!(
            collector.collect_health(&sender, &CancellationToken::new()).await
        );
        let events: Vec<_> =
            std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        for kind in
            [ResourceKind::Sled, ResourceKind::SwitchZone, ResourceKind::Router]
        {
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, AppEvent::Addresses { id, .. } if id.kind == kind))
            );
        }
        assert_eq!(events.iter().filter(|e| matches!(e, AppEvent::Health { diagnostic, .. } if diagnostic.sled_agent == Some(crate::tui::telemetry::ServiceState::Online) && diagnostic.ntp.synchronized == Some(true))).count(), 4);
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    AppEvent::Health { diagnostic, .. } => Some(diagnostic),
                    _ => None,
                })
                .filter(|diagnostic| {
                    diagnostic.sled_agent
                        == Some(crate::tui::telemetry::ServiceState::Online)
                        && diagnostic.failed_services == ["oxide/broken"]
                        && diagnostic.ntp.synchronized == Some(true)
                })
                .count(),
            4
        );
        assert_eq!(
            events.iter().filter(|e| matches!(e, AppEvent::Rss { .. })).count(),
            2
        );
        let calls = fake.calls.lock().unwrap();
        let zone = calls
            .iter()
            .position(|(_, c)| c == "zoneadm list -cp | cut -d: -f2")
            .unwrap();
        let ntp = calls
            .iter()
            .position(|(_, c)| {
                c == "zlogin oxz_ntp_deadbeef chronyc -n tracking 2>/dev/null"
            })
            .unwrap();
        assert!(zone < ntp);
        assert!(calls.iter().any(|(_, command)| command == SLED_AGENT_STATE));
        assert!(
            calls.iter().any(|(_, command)| command == MAINTENANCE_SERVICES)
        );
        assert!(!calls.iter().any(|(_, command)| command == "svcs -xHo fmri"));
    }

    struct EmptyRouter(FakeExecutor);
    impl NodeExecutor for EmptyRouter {
        fn execute<'a>(
            &'a self,
            target: &'a NodeTarget,
            command: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<String>> {
            if matches!(target, NodeTarget::Router { .. })
                && command == LINUX_COUNTERS
            {
                Box::pin(async { Ok(String::new()) })
            } else {
                self.0.execute(target, command)
            }
        }
    }

    #[tokio::test]
    async fn empty_router_counters_fail_only_router_at_peer_generation_timestamp()
     {
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 1\nsleds = 1\nrouters = [\"ce\"]\n",
        )
        .unwrap();
        let fake = Arc::new(EmptyRouter(FakeExecutor {
            generation: AtomicU64::new(0),
            calls: Default::default(),
        }));
        let collector = Collector::new(fake, &config, 3).unwrap();
        let (sender, mut receiver) = mpsc::channel(8);
        assert!(
            collector.collect_traffic(&sender, &CancellationToken::new()).await
        );
        let events: Vec<_> =
            std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        let times: std::collections::BTreeSet<_> = events
            .iter()
            .map(|e| match e {
                AppEvent::Traffic { at, .. }
                | AppEvent::TrafficFailed { at, .. } => *at,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(times.len(), 1);
        assert_eq!(events.iter().filter(|e| matches!(e, AppEvent::TrafficFailed { id, .. } if id.kind == ResourceKind::Router)).count(), 1);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AppEvent::Traffic { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn cancellation_unblocks_bounded_sender_without_post_cancel_event() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender.send(AppEvent::Tick { now: Instant::now() }).await.unwrap();
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let sender = sender.clone();
            let cancel = cancel.clone();
            async move {
                Collector::<FakeExecutor>::send(
                    &sender,
                    &cancel,
                    AppEvent::Tick { now: Instant::now() },
                )
                .await
            }
        });
        cancel.cancel();
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(matches!(receiver.recv().await, Some(AppEvent::Tick { .. })));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn receiver_closure_terminates_collection_cleanly() {
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 1\nsleds = 1\nrouters = []\n",
        )
        .unwrap();
        let fake = Arc::new(FakeExecutor {
            generation: AtomicU64::new(0),
            calls: Default::default(),
        });
        let collector = Collector::new(fake, &config, 1).unwrap();
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        assert!(
            !tokio::time::timeout(
                Duration::from_secs(1),
                collector.collect_traffic(&sender, &CancellationToken::new())
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn blocked_ssh_work_is_dropped_on_cancellation_without_an_event() {
        let fake = Arc::new(ControlledExecutor::new(true, false));
        let collector = collector_with_targets(
            fake.clone(),
            vec![resource_target("g0", ResourceKind::Sled)],
            1,
        );
        let (sender, mut receiver) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let collector = collector.clone();
            let cancel = cancel.clone();
            async move { collector.collect_traffic(&sender, &cancel).await }
        });
        wait_until(|| fake.active.load(Ordering::SeqCst) == 1).await;

        cancel.cancel();

        assert!(
            !tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
        );
        assert_eq!(fake.active.load(Ordering::SeqCst), 0);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancelled_router_call_finishes_its_safe_boundary_before_returning()
    {
        let fake = Arc::new(ControlledExecutor::new(false, true));
        let collector = collector_with_targets(
            fake.clone(),
            vec![resource_target("ce", ResourceKind::Router)],
            1,
        );
        let (sender, mut receiver) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let collector = collector.clone();
            let cancel = cancel.clone();
            async move { collector.collect_traffic(&sender, &cancel).await }
        });
        wait_until(|| fake.active.load(Ordering::SeqCst) == 1).await;

        cancel.cancel();
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "an in-flight Falcon serial call must reach its timeout/result boundary"
        );
        fake.gate.release();

        assert!(
            !tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn full_sender_cancellation_does_not_drop_an_in_flight_router_call() {
        let fake = Arc::new(ControlledExecutor::new(false, true));
        let collector = collector_with_targets(
            fake.clone(),
            vec![
                resource_target("g0", ResourceKind::Sled),
                resource_target("ce", ResourceKind::Router),
            ],
            2,
        );
        let (sender, mut receiver) = mpsc::channel(1);
        sender.send(AppEvent::Tick { now: Instant::now() }).await.unwrap();
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let collector = collector.clone();
            let cancel = cancel.clone();
            async move { collector.collect_traffic(&sender, &cancel).await }
        });
        wait_until(|| {
            fake.calls.lock().unwrap().iter().any(|(target, command)| {
                matches!(target, NodeTarget::Router { .. })
                    && command == LINUX_COUNTERS
            })
        })
        .await;

        cancel.cancel();
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        fake.gate.release();

        assert!(
            !tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(matches!(receiver.recv().await, Some(AppEvent::Tick { .. })));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn scheduler_uses_its_concurrency_limit_and_never_overlaps_cadences()
    {
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 2\nsleds = 2\nrouters = [\"ce\"]\n",
        )
        .unwrap();
        let fake = Arc::new(ControlledExecutor::new(true, false));
        // Deliberately differ from SchedulerConfig to prove the scheduler's
        // configured limit, rather than the direct-collection default, wins.
        let collector =
            Arc::new(Collector::new(fake.clone(), &config, 8).unwrap());
        let schedule = SchedulerConfig::new(
            Duration::from_secs(3_600),
            Duration::from_secs(3_600),
            2,
        )
        .unwrap();
        let (sender, _receiver) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(collector.run_gated(
            schedule,
            sender,
            cancel.clone(),
            Arc::new(tokio::sync::Semaphore::new(1)),
        ));
        wait_until(|| fake.max_active.load(Ordering::SeqCst) >= 2).await;

        assert_eq!(fake.max_active.load(Ordering::SeqCst), 2);
        assert!(!fake.has_command(IPADM));
        fake.gate.release();
        wait_until(|| fake.has_command(IPADM)).await;

        assert!(
            !fake.overlap.load(Ordering::SeqCst),
            "health cadence began before the traffic run completed"
        );
        assert!(fake.max_active.load(Ordering::SeqCst) <= 2);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    struct FailingSecondRackRss(FakeExecutor);

    impl NodeExecutor for FailingSecondRackRss {
        fn execute<'a>(
            &'a self,
            target: &'a NodeTarget,
            command: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<String>> {
            if matches!(target, NodeTarget::Sled { name } if name == "g2")
                && command.starts_with("curl -s --max-time 5 ")
            {
                Box::pin(async { Err(anyhow!("rack 2 unreachable")) })
            } else {
                self.0.execute(target, command)
            }
        }
    }

    #[tokio::test]
    async fn one_rss_failure_does_not_suppress_another_racks_observation() {
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 2\nsleds = 2\nrouters = []\n",
        )
        .unwrap();
        let fake = Arc::new(FailingSecondRackRss(FakeExecutor {
            generation: AtomicU64::new(0),
            calls: Default::default(),
        }));
        let collector = Collector::new(fake, &config, 2).unwrap();
        let (sender, mut receiver) = mpsc::channel(32);

        assert!(
            collector.collect_health(&sender, &CancellationToken::new()).await
        );
        let events: Vec<_> =
            std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::Rss { rack: RackId(0), .. }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::RssFailed {
                rack: RackId(1),
                message,
                ..
            } if message == "rack 2 unreachable"
        )));
    }
}
use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures::future::BoxFuture;
use futures::{StreamExt, stream};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use voxel_config::VoxelConfig;

use crate::tui::event::AppEvent;
use crate::tui::reconcile::RssObservation;
use crate::tui::telemetry::{
    CounterSnapshot, HealthDiagnostic, RackId, ResourceId, ResourceKind,
    ResourceTelemetry, TrafficSample, parse_chrony_tracking,
    parse_dladm_zone_vnics, parse_failed_services, parse_ipadm_addresses,
    parse_kstat_link_counters, parse_linux_ip_addresses,
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

fn json_string_field(input: &str, key: &str) -> String {
    let pattern = format!("\"{key}\":\"");
    input
        .find(&pattern)
        .and_then(|start| input[start + pattern.len()..].split('"').next())
        .unwrap_or_default()
        .to_string()
}

fn parse_rss_observation(body: &str) -> RssObservation {
    if body.trim().is_empty() {
        return RssObservation::Unavailable;
    }
    match json_string_field(body, "status").as_str() {
        "initializing" => {
            let step = body
                .find("\"step\"")
                .map(|start| json_string_field(&body[start..], "status"))
                .filter(|step| !step.is_empty());
            RssObservation::Initializing { step }
        }
        "initialized" => match json_string_field(body, "id") {
            id if id.is_empty() => RssObservation::StaleInitialized,
            id => RssObservation::Initialized { id },
        },
        "initialization_failed" => RssObservation::Failed {
            message: json_string_field(body, "message"),
        },
        _ => RssObservation::UnknownResponse,
    }
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

pub(crate) struct FalconExecutor {
    topo: Arc<crate::topo::Topo>,
    ips: Arc<Mutex<IpCache>>,
    router_commands: Arc<Mutex<BTreeSet<String>>>,
    serial_tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    timeout: Duration,
}

impl FalconExecutor {
    pub(crate) fn new(topo: crate::topo::Topo, timeout: Duration) -> Self {
        Self {
            topo: Arc::new(topo),
            ips: Default::default(),
            router_commands: Default::default(),
            serial_tasks: Default::default(),
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

    pub(crate) fn topology(&self) -> Arc<crate::topo::Topo> {
        self.topo.clone()
    }

    async fn sled_ip(&self, host: &str) -> anyhow::Result<String> {
        let mut cache = self.ips.lock().await;
        if let Some(ip) = cache.resolved.get(host) {
            return Ok(ip.clone());
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
    #[cfg(test)]
    concurrency: usize,
}

impl<E: NodeExecutor> Collector<E> {
    pub fn new(
        executor: Arc<E>,
        config: &VoxelConfig,
        concurrency: usize,
    ) -> anyhow::Result<Self> {
        if concurrency == 0 {
            return Err(anyhow!("collector concurrency must be nonzero"));
        }
        Ok(Self {
            executor,
            targets: CollectorTargets::from_config(config),
            baselines: Default::default(),
            #[cfg(test)]
            concurrency,
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

    #[cfg(test)]
    async fn collect_traffic(
        &self,
        sender: &mpsc::Sender<AppEvent>,
        cancel: &CancellationToken,
    ) -> bool {
        self.collect_traffic_with_limit(sender, cancel, self.concurrency).await
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
        for (id, result) in results {
            let event = match result {
                Ok(sample) => AppEvent::Traffic { id, at, sample },
                Err(error) => AppEvent::TrafficFailed {
                    id,
                    at,
                    message: error.to_string(),
                },
            };
            if !Self::send(sender, cancel, event).await {
                return false;
            }
        }
        true
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
        })
    }

    #[cfg(test)]
    async fn collect_health(
        &self,
        sender: &mpsc::Sender<AppEvent>,
        cancel: &CancellationToken,
    ) -> bool {
        self.collect_health_with_limit(sender, cancel, self.concurrency).await
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
        let command = format!(
            "curl -s --max-time 5 http://[{}]:8080/rack-initialize 2>/dev/null",
            rss.bootstrap_addr
        );
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
        let mut traffic = tokio::time::interval(config.traffic_interval);
        let mut health = tokio::time::interval(config.health_interval);
        traffic.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let health_cadence = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = traffic.tick() => false,
                _ = health.tick() => true,
            };
            let permit = tokio::select! {
                _ = cancel.cancelled() => break,
                permit = gate.clone().acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
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
            drop(permit);
            if !keep_running {
                break;
            }
        }
    }
}
