//! Join a booted rack into an existing cluster through the bootstrap agent's
//! `/multirack-join` lockstep endpoint (RFD 680).
//!
//! Only rack 0 runs RSS. Every other rack is brought up by the multirack join
//! service, which is far smaller: it initializes that rack's trust quorum,
//! starts its sled-agents, and publishes its `RackNetworkConfig` to the
//! bootstore. That last step is what gets the rack's switch front ports -
//! including the cross-rack interconnect - programmed by omicron's own
//! scrimlet reconcilers and dendrite, rather than by hand from voxel.
//! Reconfigurator on the existing Nexuses adopts the rack afterwards.
//!
//! The endpoint listens on the bootstrap network, which the host cannot reach,
//! so the request is delivered by `curl` on the joining rack's bootstrap sled -
//! the same path [`crate::rss::watch_rss`] uses to read RSS status.

use anyhow::{Context, Result, anyhow, bail};
use libfalcon::{NodeRef, Runner};
use slog::info;
use std::time::{Duration, Instant};
use voxel_config::VoxelConfig;

use crate::net::{resolve_external_ip, scp_to, ssh_capture};
use crate::rss::json_str_field;

/// The bootstrap agent's lockstep API port (omicron's
/// `BOOTSTRAP_AGENT_LOCKSTEP_PORT`), served on the bootstrap network.
const LOCKSTEP_PORT: u16 = 8080;

/// Where the request body is staged on the joining sled.
const REMOTE_BODY: &str = "/tmp/multirack-join.json";

/// How long to keep retrying the POST while the bootstrap agent comes up.
const POST_DEADLINE: Duration = Duration::from_secs(600);

/// How long to watch the join before giving up (the rack keeps converging).
const WATCH_DEADLINE: Duration = Duration::from_secs(900);

const POLL_INTERVAL: Duration = Duration::from_secs(8);

/// Drive `rack`'s cluster join to completion, watching its progress. `node` is
/// the rack's bootstrap sled - the one that coordinates the join, the same sled
/// that would have run RSS.
pub(crate) async fn drive(
    cfg: &VoxelConfig,
    d: &Runner,
    node: NodeRef,
    sled_name: &str,
    bootstrap_addr: &str,
    rack: usize,
    tag: &str,
) -> Result<()> {
    let ip = resolve_external_ip(cfg, d, sled_name, node, false)
        .await
        .map_err(|e| anyhow!("find {sled_name}'s IP: {e}"))?;

    let body = serde_json::to_string(
        &crate::rss_request::multirack_join_request(cfg, rack)?,
    )
    .context("serialize the multirack join request")?;
    let local =
        crate::util::temp_dir().join(format!("multirack-join-{tag}.json"));
    std::fs::write(&local, &body).with_context(|| format!("write {local}"))?;
    if !scp_to(&ip, local.as_str(), REMOTE_BODY) {
        bail!("copy the join request to {sled_name}");
    }

    let url =
        format!("http://[{bootstrap_addr}]:{LOCKSTEP_PORT}/multirack-join");
    post(d, tag, &ip, &url).await?;
    watch(d, tag, &ip, &url).await;
    Ok(())
}

/// POST the staged request, retrying while the bootstrap agent comes up. The
/// `%{http_code}` trailer separates a rejected request (which carries omicron's
/// own error text) from one that simply hasn't been served yet.
async fn post(d: &Runner, tag: &str, ip: &str, url: &str) -> Result<()> {
    let curl = format!(
        "curl -sS --max-time 30 -X POST -H 'Content-Type: application/json' \
         --data-binary @{REMOTE_BODY} -w '\\n%{{http_code}}' {url} 2>&1"
    );
    let deadline = Instant::now() + POST_DEADLINE;
    let mut last = String::new();
    loop {
        let out = ssh_capture(ip, &curl).unwrap_or_default();
        let (body, code) = match out.trim_end().rsplit_once('\n') {
            Some((body, code)) => (body, code.trim()),
            None => ("", out.trim()),
        };
        let step = match code {
            "200" => {
                info!(d.log, "{tag}: multirack join requested");
                return Ok(());
            }
            // 4xx is omicron refusing the request itself; retrying won't help.
            c if c.starts_with('4') => {
                bail!("multirack join rejected ({c}): {body}")
            }
            "" => "waiting for the bootstrap agent".to_string(),
            c => format!("bootstrap agent returned {c}: {body}"),
        };
        if Instant::now() >= deadline {
            bail!(
                "multirack join not accepted within {POST_DEADLINE:?}: {step}"
            );
        }
        if step != last {
            info!(d.log, "{tag}: {step}");
            last = step;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Poll the join state until it completes or fails. Like RSS watching, an
/// expired deadline is not fatal: the rack keeps converging on its own.
async fn watch(d: &Runner, tag: &str, ip: &str, url: &str) {
    let curl = format!("curl -s --max-time 5 {url} 2>/dev/null");
    let start = Instant::now();
    let mut last = String::new();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if start.elapsed() > WATCH_DEADLINE {
            slog::warn!(
                d.log,
                "{tag}: stopped watching the multirack join after {}m - it may \
                 still be converging; last state: {}",
                WATCH_DEADLINE.as_secs() / 60,
                if last.is_empty() { "unknown" } else { &last }
            );
            return;
        }
        let out = ssh_capture(ip, &curl).unwrap_or_default();
        let state = json_str_field(&out, "state");
        match state.as_str() {
            "" => continue,
            "completed" => {
                info!(d.log, "{tag}: multirack join complete");
                return;
            }
            "failed" | "invalid_membership_size" => {
                slog::warn!(
                    d.log,
                    "{tag}: multirack join {state}: {}",
                    json_str_field(&out, "message")
                );
                return;
            }
            "task_panicked" => {
                slog::warn!(
                    d.log,
                    "{tag}: the multirack join service panicked"
                );
                return;
            }
            s if s != last => {
                info!(d.log, "{tag}: multirack join: {}", s.replace('_', " "));
                last = s.to_string();
            }
            _ => {}
        }
    }
}
