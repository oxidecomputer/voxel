//! Join a booted rack into an existing cluster through wicketd's commission
//! API (RFD 680).
//!
//! Only rack 0 runs RSS. Every other rack is brought up by the multirack join
//! service, which is far smaller. The join service initializes that rack's
//! trust quorum, publishes its `RackNetworkConfig` to the bootstore and starts
//! its sled-agents. That second step is what enables running DDM routing over
//! the rack's switch front ports. Reconfigurator on the existing Nexuses adopts
//! the rack afterwards.

use anyhow::{Result, bail};
use slog::info;
use std::time::{Duration, Instant};
use voxel_config::VoxelConfig;
use wicketd_commission_client::Client;
use wicketd_commission_types_versions::latest::rack_setup as types;

/// How long to keep retrying the POST while wicketd and the bootstrap agent
/// come up behind it.
const POST_DEADLINE: Duration = Duration::from_secs(600);

/// How long to watch the join before giving up (the rack keeps converging).
const WATCH_DEADLINE: Duration = Duration::from_secs(900);

// How long to wait between polls
const POLL_INTERVAL: Duration = Duration::from_secs(8);

/// Drive `rack`'s cluster join to completion, watching its progress.
/// `scrimlet` is the rack's bootstrap sled - the one whose switch zone serves
/// the commission API, the same sled that would have run RSS.
pub(crate) async fn drive(
    cfg: &VoxelConfig,
    d: &libfalcon::Runner,
    scrimlet: libfalcon::NodeRef,
    scrimlet_name: &str,
    rack: usize,
    tag: &str,
) -> Result<()> {
    let (_tunnel, client) =
        crate::commission::connect_client(cfg, d, scrimlet, scrimlet_name, tag)
            .await?;

    let body = crate::rss_request::multirack_join_request(cfg, rack)?;
    let deadline = Instant::now() + POST_DEADLINE;
    loop {
        match client.post_run_multirack_join(&body).await {
            Ok(_) => break,
            Err(e) if Instant::now() < deadline => {
                // Multirack Join may have started, but the response got lost.
                // If it completes before the next retry, we'll get an error
                // back and fail retries indefinitely until `POST_DEADLINE` is
                // exceeded. We don't want to make voxel users wait for that.
                //
                // Therefore, upon each error we check to see whether there is
                // a MULTIRACK_JOIN in progress and if so, break so we can go
                // to `watch`.
                if let Ok(status) = client.get_rack_setup_state().await
                    && let Some(op) = status.into_inner().operation
                    && op.kind == types::RackOperationKind::MULTIRACK_JOIN
                {
                    break;
                }

                info!(d.log, "{tag}: multirack join not started yet ({e})");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(e) => bail!("start multirack join: {e}"),
        }
    }
    info!(d.log, "{tag}: multirack join requested");

    watch(d, tag, &client).await?;
    Ok(())
}

/// Poll the join until it completes or fails. Like RSS watching, an expired
/// deadline is not fatal: the rack keeps converging on its own.
async fn watch(
    d: &libfalcon::Runner,
    tag: &str,
    client: &Client,
) -> Result<()> {
    let start = Instant::now();
    let mut last = String::new();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if start.elapsed() > WATCH_DEADLINE {
            slog::warn!(
                d.log,
                "{tag}: stopped watching the multirack join after {}m - it may \
                 still be converging; last step: {}",
                WATCH_DEADLINE.as_secs() / 60,
                if last.is_empty() { "unknown" } else { &last }
            );
            return Ok(());
        }
        let Ok(status) = client.get_rack_setup_state().await else {
            continue;
        };
        let Some(op) = status.into_inner().operation else {
            continue;
        };
        if op.kind != types::RackOperationKind::MULTIRACK_JOIN {
            continue;
        }
        match op.state {
            types::RackOperationState::Completed => {
                info!(d.log, "{tag}: multirack join complete");
                return Ok(());
            }
            types::RackOperationState::Failed { message, .. } => {
                let e = format!("{tag}: multirack join failed: {message}");
                slog::warn!(d.log, "{e}");
                bail!("{e}");
            }
            types::RackOperationState::Panicked => {
                let e = format!("{tag}: the multirack join service panicked");
                slog::warn!(d.log, "{e}");
                bail!("{e}");
            }
            types::RackOperationState::InProgress { current_step } => {
                let step = current_step.map_or_else(
                    || "starting".to_string(),
                    |s| {
                        format!(
                            "{}/{} {}",
                            s.step, s.total_steps, s.description
                        )
                    },
                );
                if step != last {
                    info!(d.log, "{tag}: multirack join: {step}");
                    last = step;
                }
            }
        }
    }
}
