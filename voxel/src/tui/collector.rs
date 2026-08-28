#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use tokio::net::TcpListener;
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
            nexus: Default::default(),
            sled_serials: Default::default(),
            oximeter_attempts: Default::default(),
            oximeter_diagnostic_attempts: Default::default(),
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
    fn isolated_node_addresses_are_available_without_serial_discovery() {
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 1\nsleds = 3\nrouters = [\"ce\"]\n\
             [external]\nmode = \"isolated\"\n",
        )
        .unwrap();

        let cache =
            IpCache::from_config_and_addresses(&config, BTreeMap::new());

        assert_eq!(
            cache.resolved.get("g0").map(String::as_str),
            Some("172.30.199.10")
        );
        assert_eq!(
            cache.resolved.get("ce").map(String::as_str),
            Some("172.30.199.13")
        );
        assert!(cache.in_flight.is_empty());

        assert!(
            IpCache::from_config_and_addresses(
                &VoxelConfig::default(),
                BTreeMap::new(),
            )
            .resolved
            .is_empty()
        );
        let resumed = IpCache::from_config_and_addresses(
            &VoxelConfig::default(),
            [("g0".to_string(), "192.168.1.184".to_string())]
                .into_iter()
                .collect(),
        );
        assert_eq!(
            resumed.resolved.get("g0").map(String::as_str),
            Some("192.168.1.184")
        );
        assert!(resumed.address_for_probe("g0", true).is_ok());
        assert!(
            IpCache::default()
                .address_for_probe("g0", true)
                .unwrap_err()
                .to_string()
                .contains("unavailable after resuming")
        );
        assert_eq!(
            IpCache::default().address_for_probe("g0", false).unwrap(),
            None
        );
    }

    #[test]
    fn oximeter_attempts_are_throttled_from_completion_per_rack() {
        let start = Instant::now();
        let mut attempts = BTreeMap::new();

        assert!(oximeter_due(&attempts, RackId(0), start));
        attempts.insert(RackId(0), start);
        assert!(!oximeter_due(
            &attempts,
            RackId(0),
            start + OXIMETER_INTERVAL - Duration::from_nanos(1)
        ));
        assert!(oximeter_due(&attempts, RackId(0), start + OXIMETER_INTERVAL));
        assert!(oximeter_due(
            &attempts,
            RackId(1),
            start + Duration::from_secs(1)
        ));
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

    #[tokio::test]
    async fn stalled_nexus_does_not_block_direct_traffic_cadence() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = reqwest::Url::parse(&format!(
            "http://{}/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            while let Ok((connection, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _connection = connection;
                    std::future::pending::<()>().await;
                });
            }
        });
        let fake = Arc::new(FakeExecutor {
            generation: AtomicU64::new(0),
            calls: Default::default(),
        });
        let collector = Arc::new(Collector {
            executor: fake,
            targets: CollectorTargets {
                resources: vec![resource_target("g0", ResourceKind::Sled)],
                rss: vec![],
            },
            baselines: Default::default(),
            nexus: BTreeMap::from([(
                RackId(0),
                Arc::new(
                    NexusClient::new(
                        vec![endpoint],
                        RecoveryLogin {
                            silo: "recovery".into(),
                            username: "recovery".into(),
                            password: "oxide".into(),
                        },
                        Duration::from_secs(5),
                    )
                    .unwrap(),
                ),
            )]),
            sled_serials: Default::default(),
            oximeter_attempts: Default::default(),
            oximeter_diagnostic_attempts: Default::default(),
            concurrency: 1,
        });
        let schedule = SchedulerConfig::new(
            Duration::from_millis(20),
            Duration::from_secs(3_600),
            1,
        )
        .unwrap();
        let (sender, mut receiver) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(collector.run_gated(
            schedule,
            sender,
            cancel.clone(),
            Arc::new(tokio::sync::Semaphore::new(1)),
        ));

        let direct_samples =
            tokio::time::timeout(Duration::from_millis(250), async {
                let mut count = 0;
                while count < 3 {
                    if matches!(
                        receiver.recv().await,
                        Some(AppEvent::Traffic { .. })
                    ) {
                        count += 1;
                    }
                }
                count
            })
            .await
            .expect("direct probes were blocked by the stalled Nexus request");

        assert_eq!(direct_samples, 3);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        server.abort();
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

use anyhow::{Context, anyhow, ensure};
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use futures::{StreamExt, stream};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use voxel_config::VoxelConfig;

use crate::tui::event::AppEvent;
use crate::tui::nexus::{
    MetricType, NexusClient, OxqlQueryResult, RecoveryLogin,
};
use crate::tui::reconcile::RssObservation;
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
const ZONE_CPU_ALIGNMENT: Duration = Duration::from_secs(10);
const ZONE_CPU_QUERY: &str = "get zone:cpu_nsec
    | filter timestamp > @now() - 2m
    | align mean_within(10s)
    | group_by [sled_serial, zone_name, zone_type, state], sum
    | last 1";
const ZFS_QUERY: &str = r#"{
get zfs_pool:bytes_allocated;
get zfs_pool:bytes_total
} | last 1"#;
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

#[cfg(test)]
mod oximeter_tests {
    use super::*;
    use serde_json::json;

    fn series(value: Option<i64>) -> serde_json::Value {
        json!({
            "fields": {
                "sled_serial": {"type": "string", "value": "2FAKE000"},
                "zone_name": {"type": "string", "value": "oxz_nexus_deadbeef0"},
                "link_name": {"type": "string", "value": "net0"}
            },
            "points": {
                "start_times": ["2026-08-17T00:00:00Z"],
                "timestamps": ["2026-08-17T00:00:10Z"],
                "values": [{
                    "values": {"type": "integer", "values": [value]},
                    "metric_type": "delta"
                }]
            }
        })
    }

    fn traffic_result(null_table: Option<&str>) -> OxqlQueryResult {
        let tables = [
            "sled_data_link:bytes_received",
            "sled_data_link:bytes_sent",
            "sled_data_link:packets_received",
            "sled_data_link:packets_sent",
            "sled_data_link:errors_received",
            "sled_data_link:errors_sent",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            json!({
                "name": name,
                "timeseries": [series((null_table != Some(name)).then_some((index + 1) as i64))]
            })
        })
        .collect::<Vec<_>>();
        serde_json::from_value(json!({"tables": tables})).unwrap()
    }

    #[test]
    fn traffic_queries_bound_each_response_to_one_sled() {
        let serials = BTreeMap::from([
            ("2FAKE000".into(), (RackId(0), "g0".into(), true)),
            ("2FAKE\"001".into(), (RackId(0), "g1".into(), false)),
            ("2FAKE002".into(), (RackId(1), "g2".into(), false)),
        ]);

        let queries = oximeter_traffic_queries(RackId(0), &serials);

        assert_eq!(queries.len(), 12);
        assert!(queries.iter().any(|query| {
            query
                == "get sled_data_link:bytes_received | filter sled_serial == \"2FAKE000\" | last 1"
        }));
        assert!(queries.iter().any(|query| {
            query
                == "get sled_data_link:errors_sent | filter sled_serial == \"2FAKE\\\"001\" | last 1"
        }));
        assert!(queries.iter().all(|query| !query.contains("2FAKE002")));
    }

    #[test]
    fn traffic_parser_maps_deltas_and_rejects_partial_samples() {
        let serials = BTreeMap::from([(
            "2FAKE000".into(),
            (RackId(0), "g0".into(), true),
        )]);
        let parsed = parse_oximeter_traffic(
            traffic_result(None),
            RackId(0),
            &serials,
            Instant::now(),
        )
        .unwrap();
        let sample = &parsed
            [&ResourceId::rack(RackId(0), ResourceKind::Sled, "g0")][0]
            .1;
        assert_eq!(sample.source, TrafficSource::Oximeter);
        assert_eq!(sample.total.rx_bytes_sec, 0.1);
        assert_eq!(sample.errors.tx_sec, 0.6);
        assert_eq!(sample.zones[0].name, "oxz_nexus_deadbeef0");

        assert!(
            parse_oximeter_traffic(
                traffic_result(Some("sled_data_link:bytes_sent")),
                RackId(0),
                &serials,
                Instant::now(),
            )
            .is_err()
        );
    }

    #[test]
    fn traffic_parser_combines_latest_metrics_with_different_timestamps() {
        let serials = BTreeMap::from([(
            "2FAKE000".into(),
            (RackId(0), "g0".into(), true),
        )]);
        let mut result = traffic_result(None);
        for (index, table) in result.tables.iter_mut().enumerate() {
            table.timeseries[0].points.timestamps[0] =
                "2026-08-17T00:00:10Z".parse::<DateTime<Utc>>().unwrap()
                    + chrono::Duration::seconds(index as i64);
        }

        let parsed =
            parse_oximeter_traffic(result, RackId(0), &serials, Instant::now())
                .unwrap();

        assert_eq!(
            parsed[&ResourceId::rack(RackId(0), ResourceKind::Sled, "g0")]
                .len(),
            1
        );
    }

    #[test]
    fn diagnostic_parsers_aggregate_cpu_pair_zfs_and_count_exceptions() {
        let fields = |extra: serde_json::Value| {
            let mut fields = serde_json::Map::from_iter([(
                "sled_serial".into(),
                json!({"type": "string", "value": "2FAKE000"}),
            )]);
            fields.extend(extra.as_object().unwrap().clone());
            fields
        };
        let points =
            |metric_type: &str, start_times: serde_json::Value, value| {
                json!({
                    "start_times": start_times,
                    "timestamps": ["2026-08-17T00:00:10Z"],
                    "values": [{
                        "values": {"type": "integer", "values": [value]},
                        "metric_type": metric_type
                    }]
                })
            };
        let serials = BTreeMap::from([(
            "2FAKE000".into(),
            (RackId(0), "g0".into(), true),
        )]);
        let cpu_tables = ["user", "sys", "waitrq"]
            .into_iter()
            .map(|state| {
                json!({
                    "fields": fields(json!({
                        "zone_name": {"type": "string", "value": "oxz_nexus_deadbeef0"},
                        "zone_type": {"type": "string", "value": "nexus"},
                        "state": {"type": "string", "value": state}
                    })),
                    "points": {
                        "start_times": [
                            "2026-08-17T00:00:00Z",
                            "2026-08-17T00:00:10Z"
                        ],
                        "timestamps": [
                            "2026-08-17T00:00:10Z",
                            "2026-08-17T00:00:20Z"
                        ],
                        "values": [{
                            "values": {
                                "type": "integer",
                                "values": [
                                    1_000_000_000_i64,
                                    2_000_000_000_i64
                                ]
                            },
                            "metric_type": "delta"
                        }]
                    }
                })
            })
            .collect::<Vec<_>>();
        let cpu: OxqlQueryResult = serde_json::from_value(json!({
            "tables": [{"name": "zone:cpu_nsec", "timeseries": cpu_tables}]
        }))
        .unwrap();
        let cpu = parse_zone_cpu(cpu, RackId(0), &serials).unwrap();
        assert_eq!(cpu[0].user_percent, 20.0);
        assert_eq!(cpu[0].system_percent, 20.0);
        assert_eq!(cpu[0].wait_percent, 20.0);

        let zfs_tables = [
            ("zfs_pool:bytes_allocated", 40_i64),
            ("zfs_pool:bytes_total", 100_i64),
        ]
        .into_iter()
        .map(|(name, value)| {
            json!({
                "name": name,
                "timeseries": [{
                    "fields": fields(json!({
                        "pool_name": {"type": "string", "value": "oxp_test"}
                    })),
                    "points": points("gauge", serde_json::Value::Null, value)
                }]
            })
        })
        .collect::<Vec<_>>();
        let zfs = parse_zfs_headroom(
            serde_json::from_value(json!({"tables": zfs_tables})).unwrap(),
            RackId(0),
            &serials,
        )
        .unwrap();
        assert_eq!(zfs[0].available_bytes(), 60);

        let exception_tables = [
            ("oximeter_collector:failed_collections", 2_i64),
            ("oximeter_collector:database_samples_dropped", 3_i64),
        ]
        .into_iter()
        .map(|(name, value)| json!({
            "name": name,
            "timeseries": [{
                "fields": {
                    "collector_id": {
                        "type": "uuid",
                        "value": "00000000-0000-0000-0000-000000000001"
                    },
                    "collector_ip": {"type": "ip_addr", "value": "192.0.2.1"},
                    "collector_port": {"type": "u16", "value": 12223}
                },
                "points": points("delta", json!(["2026-08-17T00:00:00Z"]), value)
            }]
        }))
        .collect::<Vec<_>>();
        let exceptions = parse_oximeter_exceptions(
            serde_json::from_value(json!({"tables": exception_tables}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(exceptions.failed_collections, 2);
        assert_eq!(exceptions.dropped_samples, 3);
    }

    #[test]
    fn zone_cpu_parser_accepts_aligned_grouped_values() {
        let serials = BTreeMap::from([(
            "2FAKE000".into(),
            (RackId(0), "g0".into(), true),
        )]);
        let timeseries = ["user", "sys", "waitrq"]
            .into_iter()
            .map(|state| {
                json!({
                    "fields": {
                        "sled_serial": {"type": "string", "value": "2FAKE000"},
                        "zone_name": {"type": "string", "value": "oxz_nexus_deadbeef0"},
                        "zone_type": {"type": "string", "value": "nexus"},
                        "state": {"type": "string", "value": state}
                    },
                    "points": {
                        "timestamps": ["2026-08-17T00:00:10Z"],
                        "values": [{
                            "values": {"type": "double", "values": [2_000_000_000.0]},
                            "metric_type": "gauge"
                        }]
                    }
                })
            })
            .collect::<Vec<_>>();
        let result = serde_json::from_value(json!({
            "tables": [{"name": "zone:cpu_nsec", "timeseries": timeseries}]
        }))
        .unwrap();

        let cpu = parse_zone_cpu(result, RackId(0), &serials).unwrap();

        assert_eq!(cpu[0].user_percent, 20.0);
        assert_eq!(cpu[0].system_percent, 20.0);
        assert_eq!(cpu[0].wait_percent, 20.0);
    }
}

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
        let login = RecoveryLogin::from_config(config);
        let nexus = if cfg!(test) {
            BTreeMap::new()
        } else if let Some(login) = login {
            (0..config.topology.racks())
                .map(|rack| {
                    Ok((
                        RackId(rack),
                        Arc::new(NexusClient::new(
                            crate::tui::nexus::rack_endpoints(config, rack)?,
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
        for (rack, client) in &self.nexus {
            let due = {
                let attempts = self.oximeter_attempts.lock().await;
                oximeter_due(&attempts, *rack, at)
            };
            if !due {
                continue;
            }
            let result = self.oximeter_traffic(*rack, client, at, cancel).await;
            let completed_at = Instant::now();
            self.oximeter_attempts.lock().await.insert(*rack, completed_at);
            match result {
                Ok(samples) => {
                    for (id, samples) in samples {
                        if !Self::send(
                            sender,
                            cancel,
                            AppEvent::OximeterTraffic {
                                id,
                                at: completed_at,
                                samples,
                            },
                        )
                        .await
                        {
                            return false;
                        }
                    }
                }
                Err(error) => {
                    if !Self::send(
                        sender,
                        cancel,
                        AppEvent::OximeterTrafficFailed {
                            rack: *rack,
                            at: completed_at,
                            message: format!("Oximeter traffic: {error}"),
                        },
                    )
                    .await
                    {
                        return false;
                    }
                }
            }

            let diagnostics_due = {
                let attempts = self.oximeter_diagnostic_attempts.lock().await;
                oximeter_due(&attempts, *rack, at)
            };
            if diagnostics_due {
                for event in self
                    .oximeter_diagnostic_events(*rack, client, at, cancel)
                    .await
                {
                    if !Self::send(sender, cancel, event).await {
                        return false;
                    }
                }
                self.oximeter_diagnostic_attempts
                    .lock()
                    .await
                    .insert(*rack, Instant::now());
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

    async fn oximeter_diagnostic_events(
        &self,
        rack: RackId,
        client: &NexusClient,
        at: Instant,
        cancel: &CancellationToken,
    ) -> Vec<AppEvent> {
        // A slow high-cardinality query must not prevent independent rack
        // diagnostics from reaching the UI during the same collection cycle.
        let (cpu, zfs, exceptions) = tokio::join!(
            client.query(ZONE_CPU_QUERY, cancel),
            client.query(ZFS_QUERY, cancel),
            client.query(OXIMETER_EXCEPTIONS_QUERY, cancel),
        );
        let cpu = cpu.and_then(|result| {
            parse_zone_cpu(result, rack, &self.sled_serials)
        });
        let zfs = zfs.and_then(|result| {
            parse_zfs_headroom(result, rack, &self.sled_serials)
        });
        let exceptions = exceptions.and_then(parse_oximeter_exceptions);
        vec![
            match cpu {
                Ok(zones) => AppEvent::ZoneCpu { rack, at, zones },
                Err(error) => AppEvent::ZoneCpuFailed {
                    rack,
                    at,
                    message: error.to_string(),
                },
            },
            match zfs {
                Ok(pools) => AppEvent::ZfsHeadroom { rack, at, pools },
                Err(error) => AppEvent::ZfsHeadroomFailed {
                    rack,
                    at,
                    message: error.to_string(),
                },
            },
            match exceptions {
                Ok(exceptions) => {
                    AppEvent::OximeterExceptions { rack, at, exceptions }
                }
                Err(error) => AppEvent::OximeterExceptionsFailed {
                    rack,
                    at,
                    message: error.to_string(),
                },
            },
        ]
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
        let probes = async {
            let mut traffic = tokio::time::interval(config.traffic_interval);
            let mut health = tokio::time::interval(config.health_interval);
            traffic.set_missed_tick_behavior(
                tokio::time::MissedTickBehavior::Skip,
            );
            health.set_missed_tick_behavior(
                tokio::time::MissedTickBehavior::Skip,
            );
            loop {
                let health_cadence = tokio::select! {
                    biased;
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
