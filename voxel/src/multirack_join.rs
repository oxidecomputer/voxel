//! Join a booted rack into an existing cluster through wicketd's commission
//! API (RFD 680).
//!
//! Only rack 0 runs RSS. Every other rack is brought up by the multirack join
//! service, which is far smaller: it initializes that rack's trust quorum,
//! starts its sled-agents, and publishes its `RackNetworkConfig` to the
//! bootstore. That last step is what gets the rack's switch front ports -
//! including the cross-rack interconnect - programmed by omicron's own
//! scrimlet reconcilers and dendrite, rather than by hand from voxel.
//! Reconfigurator on the existing Nexuses adopts the rack afterwards.
//!
//! The bootstrap agent serves the join on the bootstrap network, which the
//! host cannot reach. wicketd can, so the request goes through the same
//! tunnelled commission client [`crate::commission`] drives RSS with, and
//! wicketd forwards it over its lockstep client.

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
                info!(d.log, "{tag}: multirack join not started yet ({e})");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(e) => bail!("start multirack join: {e}"),
        }
    }
    info!(d.log, "{tag}: multirack join requested");

    watch(d, tag, &client).await;
    Ok(())
}

/// Poll the join until it completes or fails. Like RSS watching, an expired
/// deadline is not fatal: the rack keeps converging on its own.
///
/// The join reports through the same `GET /rack-setup` the initialize path
/// uses, distinguished by the operation's kind.
async fn watch(d: &libfalcon::Runner, tag: &str, client: &Client) {
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
            return;
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
                return;
            }
            types::RackOperationState::Failed { message, .. } => {
                slog::warn!(d.log, "{tag}: multirack join failed: {message}");
                return;
            }
            types::RackOperationState::Panicked => {
                slog::warn!(
                    d.log,
                    "{tag}: the multirack join service panicked"
                );
                return;
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
