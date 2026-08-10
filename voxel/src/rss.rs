//! RSS bring-up progress (fridge-style): poll the RSS node's bootstrap-agent
//! status API and render `[n/total]` step transitions. Also hosts `strip_ansi`,
//! shared with serial-exec output parsing in [`crate::net`].

use anyhow::anyhow;
use libfalcon::{NodeRef, Runner};
use slog::{info, warn};
use std::time::{Duration, Instant};

/// Extract a `"key":"value"` string field from a flat JSON blob without a JSON
/// dependency - robust to surrounding serial-console noise.
fn json_str_field(s: &str, key: &str) -> String {
    let pat = format!("\"{key}\":\"");
    if let Some(i) = s.find(&pat) {
        let rest = &s[i + pat.len()..];
        if let Some(j) = rest.find('"') {
            return rest[..j].to_string();
        }
    }
    String::new()
}

/// RSS bring-up stages, in order, as omicron's `RssStep` serializes them
/// (snake_case) paired with a human label. Used to render `[n/total]` progress.
const RSS_STEPS: &[(&str, &str)] = &[
    ("requested", "requested"),
    ("starting", "starting"),
    ("load_existing_plan", "loading existing plan"),
    ("create_sled_plan", "creating sled plan"),
    ("init_trust_quorum", "initializing trust quorum"),
    ("initial_network_config_update", "initial network config"),
    ("sled_init", "initializing sleds"),
    ("final_network_config_update", "final network config"),
    ("init_dns", "initializing internal DNS"),
    ("configure_dns", "configuring DNS"),
    ("init_ntp", "initializing NTP"),
    ("wait_for_time_sync", "waiting for time sync"),
    ("wait_for_database", "waiting for database"),
    ("cluster_init", "initializing cluster"),
    ("zones_init", "initializing zones"),
    ("nexus_handoff", "handing off to Nexus"),
];

const NEXUS_HANDOFF_DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(10);
const NEXUS_HANDOFF_DIAGNOSTIC_SENTINEL: &str =
    "__VOXEL_NEXUS_HANDOFF_DIAGNOSTICS_COMPLETE__";
const NEXUS_HANDOFF_DIAGNOSTIC_COMMAND: &str = "log=$(svcs -L svc:/oxide/sled-agent:default 2>/dev/null) && test -n \"$log\" && test -r \"$log\" && recent=$(tail -n 2000 \"$log\" 2>/dev/null) && matches=$(printf '%s\\n' \"$recent\" | grep -Ei 'handoff.*nexus|nexus.*handoff|nexus lockstep address|failed to inject.*rss|rss.*inject.*fail' 2>/dev/null) && matches=$(printf '%s\\n' \"$matches\" | tail -n 80) && test -n \"$matches\" && printf '__VOXEL_NEXUS_HANDOFF_DIAGNOSTICS_COMPLETE__\\nsled-agent log: %s\\n%s\\n' \"$log\" \"$matches\"";
const RSS_COMPLETION_MARKER_SENTINEL: &str = "__VOXEL_RSS_COMPLETION_MARKER__";
const RSS_COMPLETION_MARKER_COMMAND: &str = r#"for f in /pool/int/*/config/rss-plan-completed.marker; do [ -f "$f" ] || continue; contents=$(LC_ALL=C tr -d '[:space:]' < "$f" 2>/dev/null) || continue; if [ "$contents" = '{}' ]; then printf '__VOXEL_RSS_COMPLETION_MARKER__%s\n' "$f"; exit 0; fi; done; exit 1"#;

fn parse_nexus_handoff_diagnostic_output(output: Option<&str>) -> Option<&str> {
    let evidence = output?
        .strip_prefix(NEXUS_HANDOFF_DIAGNOSTIC_SENTINEL)?
        .strip_prefix('\n')?
        .trim();
    let (path, matches) = evidence.split_once('\n')?;
    (path.starts_with("sled-agent log: ") && !matches.trim().is_empty())
        .then_some(evidence)
}

fn parse_rss_completion_marker_output(output: Option<&str>) -> Option<&str> {
    output?.lines().find_map(|line| {
        let path = line.strip_prefix(RSS_COMPLETION_MARKER_SENTINEL)?.trim();
        let rest = path.strip_prefix("/pool/int/")?;
        let (pool, suffix) = rest.split_once('/')?;
        (!pool.is_empty() && suffix == "config/rss-plan-completed.marker")
            .then_some(path)
    })
}

/// `RackOperationStatus::Initializing` nests the current `RssStep` as
/// `"step":{"status":"<snake>"}`, so the step name is the first `status` after
/// `"step"` - not a flat field.
fn json_step(s: &str) -> String {
    match s.find("\"step\"") {
        Some(i) => json_str_field(&s[i..], "status"),
        None => String::new(),
    }
}

/// `(1-based index, human label)` for a snake_case step; index 0 if unknown.
fn rss_step_display(step: &str) -> (usize, String) {
    for (i, (name, label)) in RSS_STEPS.iter().enumerate() {
        if *name == step {
            return (i + 1, label.to_string());
        }
    }
    (0, step.replace('_', " "))
}

fn rss_timeout_error(
    tag: &str,
    cap: Duration,
    last: &str,
    diagnostics: Option<&str>,
) -> anyhow::Error {
    let last_step = if last.is_empty() {
        "no RSS step observed".to_string()
    } else {
        rss_step_display(last).1
    };
    let mut message = format!(
        "{tag}: RSS did not initialize within {}m; last observed step: {last_step}",
        cap.as_secs() / 60
    );
    if last == "nexus_handoff" {
        match diagnostics.filter(|output| !output.trim().is_empty()) {
            Some(output) => {
                message.push_str("; Nexus handoff diagnostics:\n");
                message.push_str(output.trim());
            }
            None => message.push_str(
                "; unable to collect Nexus handoff diagnostics before cleanup",
            ),
        }
    }
    anyhow!(message)
}

#[derive(Debug, Eq, PartialEq)]
enum RssStatus {
    Initializing { id: String, step: String },
    Initialized(String),
    InitializedWithoutId,
    InitializationFailed(String),
    Waiting,
}

fn classify_rss_status(output: &str) -> RssStatus {
    match json_str_field(output, "status").as_str() {
        "initializing" => {
            let id = json_str_field(output, "id");
            if id.trim().is_empty() {
                RssStatus::Waiting
            } else {
                RssStatus::Initializing { id, step: json_step(output) }
            }
        }
        "initialized" => {
            let id = json_str_field(output, "id");
            if !id.trim().is_empty() {
                RssStatus::Initialized(id)
            } else if serde_json::from_str::<serde_json::Value>(output.trim())
                .ok()
                .and_then(|value| value.get("id").cloned())
                .is_some_and(|id| id.is_null())
            {
                RssStatus::InitializedWithoutId
            } else {
                RssStatus::Waiting
            }
        }
        "initialization_failed" => {
            RssStatus::InitializationFailed(json_str_field(output, "message"))
        }
        "initialization_panicked" => RssStatus::InitializationFailed(
            "bootstrap agent panicked during rack initialization".to_string(),
        ),
        _ => RssStatus::Waiting,
    }
}

fn completed_rss_operation_id(
    reported_id: &str,
    observed_id: Option<&str>,
) -> anyhow::Result<String> {
    let reported_id = (!reported_id.trim().is_empty()).then_some(reported_id);
    let observed_id = observed_id.filter(|id| !id.trim().is_empty());
    match (reported_id, observed_id) {
        (Some(reported), Some(observed)) if reported != observed => {
            Err(anyhow!(
                "completed RSS operation {reported} does not match observed operation {observed}"
            ))
        }
        (Some(reported), _) => Ok(reported.to_string()),
        (None, _) => Err(anyhow!("completed RSS status has no operation id")),
    }
}

fn recovered_rss_completion_evidence(
    observed_id: Option<&str>,
    marker_path: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let observed = observed_id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "status=initialized without an operation id, and no initialization operation was observed during this watch; stale sled state is not a valid initialization"
            )
        })?;
    let marker = marker_path.ok_or_else(|| {
        anyhow!(
            "status=initialized without an operation id after observing RSS operation {observed}, but no valid RSS completion marker was found"
        )
    })?;
    Ok((observed.to_string(), marker.to_string()))
}

async fn select_rss_ip<F, Fut>(
    known_ip: Option<String>,
    discovery_window: Duration,
    retry_interval: Duration,
    mut resolve: F,
) -> anyhow::Result<String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    if let Some(ip) = known_ip {
        return Ok(ip);
    }

    let deadline = Instant::now() + discovery_window;
    loop {
        match resolve().await {
            Ok(ip) => return Ok(ip),
            Err(error) if Instant::now() >= deadline => {
                return Err(anyhow!(
                    "failed to discover RSS node host address within {}s: {error}",
                    discovery_window.as_secs()
                ));
            }
            Err(_) => tokio::time::sleep(retry_interval).await,
        }
    }
}

fn validate_rss_operation_id(
    observed: Option<&str>,
    reported: &str,
) -> anyhow::Result<()> {
    if let Some(observed) = observed
        && observed != reported
    {
        return Err(anyhow!(
            "RSS operation changed from {observed} to {reported} during one watch"
        ));
    }
    Ok(())
}

fn rss_initialization_error(tag: &str, message: &str) -> anyhow::Error {
    let message = if message.trim().is_empty() {
        "bootstrap agent provided no failure message"
    } else {
        message
    };
    anyhow!("{tag}: RSS initialization failed: {message}")
}

fn rss_maintenance_error(tag: &str, diagnostics: &str) -> anyhow::Error {
    anyhow!(
        "{tag}: RSS cannot start because a service on the RSS node is in MAINTENANCE: {}",
        diagnostics.trim()
    )
}

/// Stream RSS bring-up: poll the RSS node's bootstrap-agent `/rack-initialize`
/// endpoint and log each step transition until the rack initializes or fails.
///
/// We poll over SSH, NOT the serial console. The bootstrap-agent listens on the
/// bootstrap net (the host can't reach it), so the curl runs *on* the RSS node -
/// but driving it over the serial console is fatally fragile under RSS load: the
/// single-user console gets contended during zone-init, and a stalled exec (or a
/// timed-out/cancelled one) leaves a shell logged in on it that poisons every
/// later poll. So we discover the node's host-LAN IP once, up front while the
/// console is still quiet, then `ssh root@<ip> 'curl ...'` each poll - no console
/// involvement, no wedge, no poisoning. Isolated mode supplies its known static
/// address and skips serial discovery. LAN discovery retries transient failures
/// within a bounded window. (`setup_ssh` has enabled empty-password root login
/// by the time launch completes.) `cap` bounds how long we watch one
/// rack's RSS before failing. The caller sizes it: a single sp-sim rack settles
/// in ~12m, but emulated SPs slow every MGS RPC and a multi-rack launch runs the
/// racks' bring-up under each other's load, so those need a bigger budget (see
/// the callers in `rack.rs`).
pub(crate) async fn watch_rss(
    d: &Runner,
    rss: NodeRef,
    bootstrap_addr: &str,
    tag: &str,
    cap: Duration,
    known_ip: Option<String>,
) -> anyhow::Result<()> {
    let curl = format!(
        "curl -s --max-time 5 http://[{bootstrap_addr}]:8080/rack-initialize 2>/dev/null"
    );
    const POLL_INTERVAL: Duration = Duration::from_secs(8);
    const HEARTBEAT: Duration = Duration::from_secs(90); // re-affirm liveness this often

    info!(d.log, "{tag}: watching RSS progress on the RSS node ...");

    let rss_ip = select_rss_ip(
        known_ip,
        Duration::from_secs(60),
        Duration::from_secs(5),
        || async {
            tokio::time::timeout(
                crate::net::SERIAL_RESOLVE_TIMEOUT,
                crate::net::node_external_ip(d, rss, false),
            )
            .await
            .map_err(|_| anyhow!("serial address resolution timed out"))?
        },
    )
    .await
    .map_err(|error| anyhow!("{tag}: {error}"))?;
    info!(d.log, "{tag}: polling RSS status via ssh root@{rss_ip}");

    let start = Instant::now();
    let mut last = String::new();
    let mut observed_operation_id: Option<String> = None;
    let mut last_emit = Instant::now();
    let mut step_start = Instant::now(); // when the CURRENT step began (for in-step timing)
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if start.elapsed() > cap {
            let diagnostic_output = if last == "nexus_handoff" {
                crate::net::ssh_output_timeout(
                    &rss_ip,
                    NEXUS_HANDOFF_DIAGNOSTIC_COMMAND,
                    NEXUS_HANDOFF_DIAGNOSTIC_TIMEOUT,
                )
            } else {
                None
            };
            let diagnostics = parse_nexus_handoff_diagnostic_output(
                diagnostic_output.as_deref(),
            );
            return Err(rss_timeout_error(tag, cap, &last, diagnostics));
        }
        let out = match crate::net::ssh_capture(&rss_ip, &curl) {
            Some(s) if !s.trim().is_empty() => s,
            // ssh failed, or the agent isn't answering yet - retry. Heartbeat so a
            // quiet stretch still shows the watcher is alive.
            _ => {
                // Fail fast: if RSS hasn't started yet and a service on the RSS
                // node has crash-looped into MAINTENANCE, it never will - surface
                // it (with the sled-agent log tail, the usual culprit: a config
                // schema drift) and stop, instead of the 15-minute hang.
                if last.is_empty()
                    && start.elapsed() > Duration::from_secs(20)
                    && let Some(x) =
                        crate::net::ssh_capture(&rss_ip, "svcs -x 2>/dev/null")
                    && x.contains("maintenance")
                {
                    warn!(
                        d.log,
                        "{tag}: RSS will not start - a service on the RSS node is in \
                                 MAINTENANCE. `svcs -x`:\n{}",
                        x.trim()
                    );
                    if let Some(t) = crate::net::ssh_capture(
                        &rss_ip,
                        "tail -6 /var/svc/log/oxide-sled-agent:default.log 2>/dev/null",
                    ) && !t.trim().is_empty()
                    {
                        warn!(
                            d.log,
                            "{tag}: sled-agent log tail:\n{}",
                            t.trim()
                        );
                    }
                    warn!(
                        d.log,
                        "{tag}: not waiting further - fix the service above, then relaunch."
                    );
                    return Err(rss_maintenance_error(tag, &x));
                }
                if last_emit.elapsed() >= HEARTBEAT {
                    // Can't know if the step advanced - report total watch time +
                    // the last step we did see.
                    let mins = start.elapsed().as_secs() / 60;
                    let where_ = if last.is_empty() {
                        "waiting for RSS to start".to_string()
                    } else {
                        format!("last seen: {}", rss_step_display(&last).1)
                    };
                    info!(
                        d.log,
                        "{tag}: still watching, {mins}m elapsed - {where_}"
                    );
                    last_emit = Instant::now();
                }
                continue;
            }
        };
        match classify_rss_status(&out) {
            RssStatus::Initializing { id, step } => {
                validate_rss_operation_id(
                    observed_operation_id.as_deref(),
                    &id,
                )
                .map_err(|error| anyhow!("{tag}: {error}"))?;
                if observed_operation_id.is_none() {
                    observed_operation_id = Some(id);
                }
                if !step.is_empty() && step != last {
                    let (idx, label) = rss_step_display(&step);
                    info!(
                        d.log,
                        "{tag} [{}/{}]: {}",
                        idx,
                        RSS_STEPS.len(),
                        label
                    );
                    last = step;
                    last_emit = Instant::now();
                    step_start = Instant::now();
                } else if !last.is_empty() && last_emit.elapsed() >= HEARTBEAT {
                    // A genuinely slow step (e.g. waiting for the CockroachDB
                    // cluster to form) must not look like a freeze. Report time in
                    // THIS step, not total watch time.
                    let (idx, label) = rss_step_display(&last);
                    let mins = step_start.elapsed().as_secs() / 60;
                    info!(
                        d.log,
                        "{tag} [{}/{}]: {} ... still working ({mins}m in this step)",
                        idx,
                        RSS_STEPS.len(),
                        label
                    );
                    last_emit = Instant::now();
                }
            }
            RssStatus::Initialized(id) => {
                let id = completed_rss_operation_id(
                    &id,
                    observed_operation_id.as_deref(),
                )
                .map_err(|error| anyhow!("{tag}: {error}"))?;
                info!(
                    d.log,
                    "{tag}: complete - rack initialized (RSS operation {id})"
                );
                return Ok(());
            }
            RssStatus::InitializedWithoutId => {
                let marker_output = crate::net::ssh_capture(
                    &rss_ip,
                    RSS_COMPLETION_MARKER_COMMAND,
                );
                let marker_path = parse_rss_completion_marker_output(
                    marker_output.as_deref(),
                );
                let (id, marker_path) = recovered_rss_completion_evidence(
                    observed_operation_id.as_deref(),
                    marker_path,
                )
                .map_err(|error| anyhow!("{tag}: {error}"))?;
                warn!(
                    d.log,
                    "{tag}: bootstrap agent reports initialized without its operation id after this watch observed RSS operation {id}; accepting completion proven by {marker_path}"
                );
                info!(
                    d.log,
                    "{tag}: complete - rack initialized (observed RSS operation {id})"
                );
                return Ok(());
            }
            RssStatus::InitializationFailed(message) => {
                return Err(rss_initialization_error(tag, &message));
            }
            RssStatus::Waiting => {} // not serving yet / other - keep waiting
        }
    }
}

/// Strip ANSI/VT escape sequences (CSI `ESC [ ... final-byte`) from serial-exec
/// output - `ip(8)` colorizes on a tty, which corrupts parsed tokens.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('\x40'..='\x7e').contains(&n) {
                        break; // final byte ends the sequence
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[tokio::test]
    async fn known_ip_skips_rss_ip_resolver() {
        let mut calls = 0;
        let ip = select_rss_ip(
            Some("172.30.199.10".to_string()),
            Duration::ZERO,
            Duration::ZERO,
            || {
                calls += 1;
                std::future::ready(Err(anyhow!("resolver must not run")))
            },
        )
        .await
        .unwrap();
        assert_eq!(ip, "172.30.199.10");
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn lan_discovery_retries_transient_failure() {
        let mut results = VecDeque::from([
            Err(anyhow!("DHCP lease not ready")),
            Ok("192.0.2.10".to_string()),
        ]);
        let ip =
            select_rss_ip(None, Duration::from_secs(1), Duration::ZERO, || {
                std::future::ready(results.pop_front().unwrap())
            })
            .await
            .unwrap();
        assert_eq!(ip, "192.0.2.10");
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn lan_discovery_failure_is_bounded() {
        let error = select_rss_ip(None, Duration::ZERO, Duration::ZERO, || {
            std::future::ready(Err(anyhow!("no address")))
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("failed to discover RSS node host address"));
    }

    #[test]
    fn operation_id_change_is_terminal() {
        let error = validate_rss_operation_id(Some("first"), "second")
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed from first to second"));
    }

    #[test]
    fn initialization_failure_maintenance_and_timeout_are_errors() {
        assert!(
            rss_initialization_error("rack", "trust quorum failed")
                .to_string()
                .contains("trust quorum failed")
        );
        assert!(
            rss_maintenance_error("rack", "svc:/oxide/sled-agent")
                .to_string()
                .contains("MAINTENANCE")
        );
        assert!(
            rss_timeout_error(
                "rack",
                Duration::from_secs(60),
                "starting",
                None
            )
            .to_string()
            .contains("did not initialize")
        );
    }

    #[test]
    fn rss_timeout_diagnostic_output_accepts_framed_evidence() {
        let output = "__VOXEL_NEXUS_HANDOFF_DIAGNOSTICS_COMPLETE__\nsled-agent log: /var/log/sled agent.log\nNexus handoff failed";
        assert_eq!(
            parse_nexus_handoff_diagnostic_output(Some(output)),
            Some(
                "sled-agent log: /var/log/sled agent.log\nNexus handoff failed"
            )
        );
    }

    #[test]
    fn rss_timeout_diagnostic_output_rejects_missing_or_empty_evidence() {
        assert_eq!(parse_nexus_handoff_diagnostic_output(None), None);
        assert_eq!(parse_nexus_handoff_diagnostic_output(Some("")), None);
        assert_eq!(
            parse_nexus_handoff_diagnostic_output(Some(
                "__VOXEL_NEXUS_HANDOFF_DIAGNOSTICS_COMPLETE__\n"
            )),
            None
        );
    }

    #[test]
    fn rss_timeout_diagnostic_output_rejects_unframed_path_or_error() {
        assert_eq!(
            parse_nexus_handoff_diagnostic_output(Some(
                "__VOXEL_NEXUS_HANDOFF_DIAGNOSTICS_COMPLETE__\nsled-agent log: /var/svc/log/oxide-sled-agent:default.log"
            )),
            None
        );
        assert_eq!(
            parse_nexus_handoff_diagnostic_output(Some(
                "grep: invalid regular expression"
            )),
            None
        );
    }

    #[test]
    fn rss_timeout_nexus_handoff_includes_captured_diagnostics() {
        let error = rss_timeout_error(
            "rack-init",
            Duration::from_secs(1800),
            "nexus_handoff",
            Some("Nexus lockstep address: [fd00::1]:12232\nFailed to handoff to nexus: connection refused"),
        )
        .to_string();
        assert!(error.contains("RSS did not initialize within 30m"), "{error}");
        assert!(
            error.contains("last observed step: handing off to Nexus"),
            "{error}"
        );
        assert!(error.contains("[fd00::1]:12232"), "{error}");
        assert!(error.contains("connection refused"), "{error}");
    }

    #[test]
    fn rss_timeout_nexus_handoff_retains_primary_error_when_collection_fails() {
        let error = rss_timeout_error(
            "rack-init",
            Duration::from_secs(1800),
            "nexus_handoff",
            None,
        )
        .to_string();
        assert!(error.contains("RSS did not initialize within 30m"), "{error}");
        assert!(
            error.contains("unable to collect Nexus handoff diagnostics"),
            "{error}"
        );
    }

    #[test]
    fn rss_timeout_non_nexus_names_last_step_without_nexus_diagnostics() {
        let error = rss_timeout_error(
            "rack-init",
            Duration::from_secs(1800),
            "wait_for_database",
            None,
        )
        .to_string();
        assert!(
            error.contains("last observed step: waiting for database"),
            "{error}"
        );
        assert!(!error.contains("Nexus handoff diagnostics"), "{error}");
    }

    #[test]
    fn extracts_nested_rss_step() {
        // RackOperationStatus::Initializing nests RssStep as {"status":...}.
        let s = r#"{"status":"initializing","id":"abc-123","step":{"status":"create_sled_plan"}}"#;
        assert_eq!(json_str_field(s, "status"), "initializing");
        assert_eq!(json_step(s), "create_sled_plan");
        let (idx, label) = rss_step_display("create_sled_plan");
        assert_eq!(idx, 4);
        assert_eq!(label, "creating sled plan");
        assert_eq!(RSS_STEPS.len(), 16);
    }

    #[test]
    fn initialized_has_no_step() {
        let s = r#"{"status":"initialized","id":"abc-123"}"#;
        assert_eq!(json_str_field(s, "status"), "initialized");
        assert_eq!(json_step(s), "");
    }

    #[test]
    fn unknown_step_humanizes() {
        let (idx, label) = rss_step_display("some_new_step");
        assert_eq!(idx, 0);
        assert_eq!(label, "some new step");
    }

    #[test]
    fn classifies_rss_progress_and_valid_completion() {
        assert_eq!(
            classify_rss_status(
                r#"{"status":"initializing","id":"abc-123","step":{"status":"create_sled_plan"}}"#
            ),
            RssStatus::Initializing {
                id: "abc-123".to_string(),
                step: "create_sled_plan".to_string(),
            }
        );
        assert_eq!(
            classify_rss_status(r#"{"status":"initialized","id":"abc-123"}"#),
            RssStatus::Initialized("abc-123".to_string())
        );
    }

    #[test]
    fn null_completed_id_requires_current_operation_and_completion_marker() {
        assert_eq!(
            recovered_rss_completion_evidence(
                Some("abc-123"),
                Some("/pool/int/pool-id/config/rss-plan-completed.marker")
            )
            .unwrap(),
            (
                "abc-123".to_string(),
                "/pool/int/pool-id/config/rss-plan-completed.marker"
                    .to_string()
            )
        );
        let error = recovered_rss_completion_evidence(
            None,
            Some("/pool/int/pool-id/config/rss-plan-completed.marker"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no initialization operation was observed"));
        let error = recovered_rss_completion_evidence(Some("abc-123"), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no valid RSS completion marker"));
    }

    #[test]
    fn completed_operation_id_must_match_the_observed_operation() {
        assert_eq!(
            completed_rss_operation_id("abc-123", Some("abc-123")).unwrap(),
            "abc-123"
        );
        let error = completed_rss_operation_id("other", Some("abc-123"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match observed operation abc-123"));
    }

    #[test]
    fn completion_marker_output_requires_a_framed_internal_config_path() {
        let path = "/pool/int/pool-id/config/rss-plan-completed.marker";
        assert_eq!(
            parse_rss_completion_marker_output(Some(&format!(
                "{RSS_COMPLETION_MARKER_SENTINEL}{path}\n"
            ))),
            Some(path)
        );
        assert_eq!(parse_rss_completion_marker_output(None), None);
        assert_eq!(parse_rss_completion_marker_output(Some(path)), None);
        assert_eq!(
            parse_rss_completion_marker_output(Some(&format!(
                "{RSS_COMPLETION_MARKER_SENTINEL}/tmp/rss-plan-completed.marker\n"
            ))),
            None
        );
    }

    #[test]
    fn classifies_rss_terminal_failures() {
        assert_eq!(
            classify_rss_status(r#"{"status":"initialized","id":null}"#),
            RssStatus::InitializedWithoutId
        );
        assert_eq!(
            classify_rss_status(
                r#"{"status":"initialization_failed","message":"trust quorum failed"}"#
            ),
            RssStatus::InitializationFailed("trust quorum failed".to_string())
        );
        assert_eq!(
            classify_rss_status(
                r#"{"status":"initialization_panicked","id":"abc-123"}"#
            ),
            RssStatus::InitializationFailed(
                "bootstrap agent panicked during rack initialization"
                    .to_string()
            )
        );
        assert_eq!(
            classify_rss_status(r#"{"status":"something_new"}"#),
            RssStatus::Waiting
        );
    }

    #[test]
    fn malformed_status_polls_are_retryable_not_terminal() {
        assert_eq!(
            classify_rss_status(r#"{"status":"initializing","id":"truncated"#),
            RssStatus::Waiting
        );
        assert_eq!(
            classify_rss_status(r#"{"status":"initialized"}"#),
            RssStatus::Waiting
        );
        assert_eq!(
            classify_rss_status(r#"{"status":"initialized","id":"truncated"#),
            RssStatus::Waiting
        );
    }

    #[test]
    fn strip_ansi_yields_clean_ip() {
        // ip(8) colorizes: ESC[36menp0s10ESC[0m ... ESC[35m192.168.68.171ESC[0m/22
        let colored = "\x1b[36menp0s10\x1b[0m \x1b[32mUP\x1b[0m \x1b[35m192.168.68.171\x1b[0m/22 metric 100";
        let clean = strip_ansi(colored);
        let ip = clean
            .split_whitespace()
            .find(|t| t.contains('.') && t.contains('/'))
            .and_then(|t| t.split('/').next())
            .unwrap();
        assert_eq!(ip, "192.168.68.171");
    }
}
