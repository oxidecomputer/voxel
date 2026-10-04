mod app;
mod collector;
mod context;
mod effects;
mod event;
mod logging;
mod nexus;
mod operation;
mod phase;
mod process;
mod reconcile;
mod session;
mod telemetry;
mod terminal;
mod ui;

use std::{fs::OpenOptions, sync::Arc, time::Duration};

use anyhow::Context;
pub(crate) use app::App;
pub(crate) use context::TuiContext;
#[cfg(test)]
pub(crate) use event::Action;
pub(crate) use event::{AppEvent, Effect};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) fn resume(choose: bool) -> anyhow::Result<()> {
    session::resume(choose)
}

fn prepare_probe_mounts(
    workdir: &camino::Utf8Path,
    config: &voxel_config::VoxelConfig,
) -> anyhow::Result<()> {
    let cargo_bay = workdir.join("cargo-bay");
    for node in config
        .sleds()
        .into_iter()
        .map(|sled| sled.name)
        .chain(config.topology.routers.iter().cloned())
    {
        std::fs::create_dir_all(cargo_bay.join(&node))
            .with_context(|| format!("create TUI probe mount for {node}"))?;
    }
    Ok(())
}

async fn reserve_initial_reconciliation(
    admission: &Arc<tokio::sync::Semaphore>,
) -> anyhow::Result<tokio::sync::OwnedSemaphorePermit> {
    admission
        .clone()
        .acquire_owned()
        .await
        .context("reserve initial TUI reconciliation")
}

/// Whether this deployment already has storage, and so a fixed external mode.
fn deployment_storage_present(context: &TuiContext) -> bool {
    std::process::Command::new("zfs")
        .args(["list", "-H", "-o", "name"])
        .arg(format!("{}/topo/{}", context.dataset, context.name))
        .output()
        .is_ok_and(|output| output.status.success())
}

pub(crate) async fn run(mut context: TuiContext) -> anyhow::Result<()> {
    let resumed_claim = session::claim_from_environment()?;

    // Everything that must survive terminal setup failure is created first.
    std::fs::create_dir_all(&context.workdir)?;
    let durable = logging::DurableLog::open(
        context.workdir.join("voxel-tui.log").as_std_path(),
    )
    .context("open durable TUI log")?;
    // A running deployment keeps the external mode it was launched with.
    let external_note = if resumed_claim.is_none()
        && !deployment_storage_present(&context)
    {
        context.default_isolated_external(crate::rack::default_route_iface())?
    } else {
        None
    };
    if let Some((level, message)) = &external_note {
        let tag = match level {
            operation::LogLevel::Warning => "WARN",
            _ => "INFO",
        };
        durable
            .write_line(&format!("{tag} {message}"))
            .context("write durable TUI log")?;
    }
    let (mut detached, reattach) = session::prepare_direct(&context)?;
    let topology = telemetry::resource_descriptors(&context.config);
    prepare_probe_mounts(&context.workdir, &context.config)?;
    let mut topo = crate::topo::build_topo(&context.config, &context.name)?;
    topo.runner.log = durable.falcon_logger();
    let executor = Arc::new(collector::FalconExecutor::new(
        topo,
        &context.config,
        resumed_claim
            .as_ref()
            .map(|claim| claim.addresses().clone())
            .unwrap_or_default(),
        resumed_claim.is_some(),
        Duration::from_secs(10),
    ));
    let collector = Arc::new(collector::Collector::new(
        executor.clone(),
        &context.config,
        8,
    )?);
    let schedule = collector::SchedulerConfig::new(
        Duration::from_secs(2),
        Duration::from_secs(10),
        8,
    )?;
    let context = Arc::new(context);
    let (events_tx, mut events_rx) = terminal::event_channel();
    let (effects_tx, effects_rx) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    // Reserve admission before telemetry can contend for it. If collection
    // wins startup, a timed-out Falcon serial probe can leave a tracked task
    // that initial reconciliation then waits on indefinitely while holding
    // this gate, preventing telemetry from ever publishing another sample.
    let initial_permit = reserve_initial_reconciliation(&admission).await?;

    let terminal_writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("open controlling terminal")?;
    let clipboard_writer = terminal_writer.try_clone()?;
    let mut terminal = terminal::TerminalSession::enter(terminal_writer)
        .context("enter terminal UI")?;
    let mut app = App::new(topology, 500, 120);
    app.durable_log_path = context.workdir.join("voxel-tui.log").to_string();
    if let Some((level, message)) = external_note {
        app.logs.push(app::LogEntry::application(level, message));
    }
    app.reattach_command = Some(reattach);
    app.external_monitoring_endpoints = (0..context.config.topology.racks())
        .filter_map(|rack| {
            nexus::rack_endpoints(&context.config, rack)
                .ok()?
                .into_iter()
                .find(|endpoint| endpoint.scheme() == "https")
                .map(|endpoint| (telemetry::RackId(rack), endpoint.to_string()))
        })
        .collect();

    let mut input = terminal::spawn_input(events_tx.clone(), shutdown.clone());
    let mut ticks = terminal::spawn_ticks(
        events_tx.clone(),
        shutdown.clone(),
        Duration::from_millis(250),
    )?;
    // The launch animation needs wicket's 30 ms cadence; run a dedicated
    // ticker for its duration rather than speeding up the steady-state tick.
    app.splash = Some(ui::splash::Splash::default());
    let splash_ticks = shutdown.child_token();
    terminal::spawn_ticks(
        events_tx.clone(),
        splash_ticks.clone(),
        ui::splash::FRAME,
    )?;
    tokio::spawn(async move {
        tokio::time::sleep(ui::splash::DURATION + ui::splash::FRAME).await;
        splash_ticks.cancel();
    });
    let mut collection = tokio::spawn(collector.run_gated(
        schedule,
        events_tx.clone(),
        shutdown.clone(),
        admission.clone(),
    ));
    let executor_drain = executor.clone();
    let services = effects::Effects::new(
        context.clone(),
        durable,
        events_tx.clone(),
        executor,
        admission.clone(),
        shutdown.clone(),
    );
    let mut effect_task = tokio::spawn(services.run(effects_rx));
    let initial_context = context.clone();
    let initial_events = events_tx.clone();
    let initial_shutdown = shutdown.clone();
    let initial_executor = executor_drain.clone();
    let mut initial = tokio::spawn(async move {
        initial_executor.drain_serial_tasks().await;
        let at = std::time::Instant::now();
        let _ =
            initial_events.send(AppEvent::ReconciliationStarted { at }).await;
        match reconcile::collect(
            &initial_context,
            &initial_executor,
            reconcile::LifecycleIntent::Idle,
            &initial_shutdown,
        )
        .await
        {
            Ok(evidence) => {
                let routes = evidence.routes;
                let _ = initial_events
                    .send(AppEvent::Reconciled {
                        at: std::time::Instant::now(),
                        result: reconcile::reduce(&evidence),
                        routes,
                    })
                    .await;
            }
            Err(error) => {
                let _ = initial_events
                    .send(AppEvent::ReconciliationFailed {
                        at: std::time::Instant::now(),
                        message: error.to_string(),
                    })
                    .await;
            }
        }
        drop(initial_permit);
        initial_shutdown.cancelled().await;
    });
    drop(events_tx);

    let mut renderer = ui::renderer::Renderer::new(Duration::from_millis(33));
    let mut clipboard = std::io::BufWriter::new(clipboard_writer);
    let mut loop_future = Box::pin(ui::renderer::run_loop(
        terminal.terminal_mut(),
        &mut clipboard,
        &mut renderer,
        &mut app,
        &mut events_rx,
        &effects_tx,
        shutdown.clone(),
    ));
    let mut input_result = None;
    let mut ticks_result = None;
    let mut collection_result = None;
    let mut effects_result = None;
    let mut initial_result = None;
    let mut primary = None;
    let mut loop_result = tokio::select! {
        result = &mut loop_future => Some(result),
        result = &mut input => { input_result = Some(result); primary = Some("terminal input stopped unexpectedly".to_string()); None },
        result = &mut ticks => { ticks_result = Some(result); primary = Some("terminal ticks stopped unexpectedly".to_string()); None },
        result = &mut collection => { collection_result = Some(result); primary = Some("TUI collector stopped unexpectedly".to_string()); None },
        result = &mut effect_task => { effects_result = Some(result); primary = Some("TUI effects task stopped unexpectedly".to_string()); None },
        result = &mut initial => { initial_result = Some(result); primary = Some("initial reconciliation stopped unexpectedly".to_string()); None },
    };
    shutdown.cancel();
    if loop_result.is_none() {
        loop_result = Some(loop_future.as_mut().await);
    }
    drop(loop_future);
    drop(effects_tx);

    match loop_result.as_ref().expect("renderer result captured") {
        Err(error) => {
            primary.get_or_insert_with(|| {
                format!("terminal renderer failed: {error}")
            });
        }
        Ok(ui::renderer::LoopExit::ConfirmedExit) => {}
        Ok(ui::renderer::LoopExit::RenderFailed { message, .. }) => {
            primary.get_or_insert_with(|| {
                format!("render terminal UI: {message}")
            });
        }
        Ok(ui::renderer::LoopExit::EffectChannelClosed { .. }) => {
            primary.get_or_insert_with(|| "TUI effect channel closed".into());
        }
        Ok(ui::renderer::LoopExit::EventChannelClosed) => {
            primary.get_or_insert_with(|| "TUI event channel closed".into());
        }
        Ok(ui::renderer::LoopExit::Cancelled { .. }) => {
            primary.get_or_insert_with(|| {
                "TUI renderer cancelled unexpectedly".into()
            });
        }
    }

    // Restore before Falcon or a lifecycle child can make shutdown wait without
    // a bound. The receiver remains live below so every producer can settle.
    if let Err(error) = terminal.restore() {
        let cleanup = format!("restore terminal: {error}");
        primary = Some(primary.map_or(cleanup.clone(), |error| {
            format!("{error}; cleanup error: {cleanup}")
        }));
    }

    while input_result.is_none()
        || ticks_result.is_none()
        || collection_result.is_none()
        || effects_result.is_none()
        || initial_result.is_none()
    {
        tokio::select! {
            result = &mut input, if input_result.is_none() => input_result = Some(result),
            result = &mut ticks, if ticks_result.is_none() => ticks_result = Some(result),
            result = &mut collection, if collection_result.is_none() => collection_result = Some(result),
            result = &mut effect_task, if effects_result.is_none() => effects_result = Some(result),
            result = &mut initial, if initial_result.is_none() => initial_result = Some(result),
            event = events_rx.recv() => if let Some(event) = event { let _ = app.update(event); },
        }
    }
    while let Ok(event) = events_rx.try_recv() {
        let _ = app.update(event);
    }
    executor_drain.drain_serial_tasks().await;
    let mut cleanup = Vec::new();
    match &input_result {
        Some(Ok(Err(error))) => {
            cleanup.push(format!("terminal input failed: {error}"))
        }
        Some(Err(error)) => {
            cleanup.push(format!("terminal input task failed: {error}"))
        }
        _ => {}
    }
    if let Some(Err(error)) = ticks_result {
        cleanup.push(format!("terminal ticks task failed: {error}"));
    }
    if let Some(Err(error)) = collection_result {
        cleanup.push(format!("TUI collector task failed: {error}"));
    }
    if let Some(Err(error)) = effects_result {
        cleanup.push(format!("TUI effects task failed: {error}"));
    }
    if let Some(Err(error)) = initial_result {
        cleanup.push(format!("initial reconciliation task failed: {error}"));
    }
    if !cleanup.is_empty() {
        let cleanup = cleanup.join("; ");
        primary = Some(primary.map_or(cleanup.clone(), |error| {
            format!("{error}; cleanup error: {cleanup}")
        }));
    }
    if let Some(error) = primary {
        return Err(anyhow::anyhow!(error));
    }
    match app.session.exit {
        Some(app::SessionExit::Detach) => {
            if resumed_claim.is_none() {
                detached
                    .set_addresses(executor_drain.resolved_addresses().await);
                session::detach(&detached)?;
            }
            Ok(())
        }
        Some(app::SessionExit::Quit) => {
            if let Some(claim) = resumed_claim {
                claim.finish()?;
            }
            Ok(())
        }
        None => Err(anyhow::anyhow!("TUI stopped without a confirmed exit")),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use camino::Utf8PathBuf;
    use tokio::sync::TryAcquireError;
    use voxel_config::VoxelConfig;

    #[tokio::test]
    async fn initial_reconciliation_reserves_admission_before_collection() {
        let admission = std::sync::Arc::new(tokio::sync::Semaphore::new(1));

        let initial =
            super::reserve_initial_reconciliation(&admission).await.unwrap();

        assert!(matches!(
            admission.clone().try_acquire_owned(),
            Err(TryAcquireError::NoPermits)
        ));
        drop(initial);
        assert!(admission.try_acquire_owned().is_ok());
    }

    #[test]
    fn probe_mount_preparation_creates_only_empty_node_directories() {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let workdir = Utf8PathBuf::from_path_buf(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "voxel-tui-probe-mounts-{}-{nonce}",
                std::process::id()
            ));
        std::fs::create_dir_all(&workdir).unwrap();
        let config = VoxelConfig::from_toml(
            "[topology]\nracks = 1\nsleds = 2\nrouters = [\"ce\", \"cr1\"]\n",
        )
        .unwrap();

        super::prepare_probe_mounts(&workdir, &config).unwrap();

        let mut entries = std::fs::read_dir(workdir.join("cargo-bay"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries, ["ce", "cr1", "g0", "g1"]);
        for node in entries {
            assert!(
                std::fs::read_dir(workdir.join("cargo-bay").join(node))
                    .unwrap()
                    .next()
                    .is_none()
            );
        }
        assert!(!workdir.join(".falcon").exists());
        std::fs::remove_dir_all(workdir).unwrap();
    }
}
