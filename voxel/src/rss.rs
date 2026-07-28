//! RSS bring-up progress (fridge-style): poll the RSS node's bootstrap-agent
//! status API and render `[n/total]` step transitions. Also hosts `strip_ansi`,
//! shared with serial-exec output parsing in [`crate::net`].

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

/// Stream RSS bring-up: poll the RSS node's bootstrap-agent `/rack-initialize`
/// endpoint and log each step transition until the rack initializes or fails.
///
/// We poll over SSH, not the serial console. The bootstrap-agent listens on
/// the bootstrap net (the host can't reach it), so the curl runs *on* the
/// RSS node - but driving it over the serial console is fatally fragile
/// under RSS load: the single-user console gets contended during zone-init,
/// and a stalled exec (or a timed-out/cancelled one) leaves a shell logged
/// in on it that poisons every later poll. In `lan` mode we discover the
/// node's host-LAN IP once, up front while the console is still quiet, then
/// `ssh root@<ip> 'curl ...'` each poll - no console involvement, no wedge,
/// no poisoning. In isolated mode, the caller passes the node's known static
/// IP as `known_ip`, so we skip discovery entirely (there's no DHCP race:
/// the address is deterministic). The polls need no credentials: voxel-init
/// runs `setup_ssh` at the start of bring-up, enabling empty-password root
/// login before any poll fires. This always returns within the `cap` so
/// `cmd_launch` proceeds to re-point the host route at ce.
/// `cap` bounds how long we watch one rack's RSS before giving up (the rack keeps
/// converging regardless). The caller sizes it: a single sp-sim rack settles in
/// ~12m, but emulated SPs slow every MGS RPC and a multi-rack launch runs the
/// racks' bring-up under each other's load, so those need a bigger budget (see
/// the callers in `rack.rs`).
pub(crate) async fn watch_rss(
    d: &Runner,
    rss: NodeRef,
    bootstrap_addr: &str,
    tag: &str,
    cap: Duration,
    known_ip: Option<String>,
) {
    let curl =
        format!("curl -s --max-time 5 http://[{bootstrap_addr}]:8080/rack-initialize 2>/dev/null");

    info!(d.log, "{tag}: watching RSS progress on the RSS node ...");

    // Isolated mode dictates the RSS node's IP up front (static, staged), so
    // the caller passes it in and we skip discovery entirely. Otherwise: serial
    // reads up front only (console still quiet here, before zone-init spam) to
    // find the LAN IP, retrying within the window because the DHCP lease can
    // land a few seconds after bring-up reports done. Each read is bounded so a
    // wedged console can't hang us; if the IP never shows we stop watching -
    // the rack keeps converging.
    let rss_ip = if let Some(ip) = known_ip {
        ip
    } else {
        match discover_rss_ip(d, rss, tag).await {
            Some(ip) => ip,
            None => return,
        }
    };
    info!(d.log, "{tag}: polling RSS status via ssh root@{rss_ip}");
    watch_rss_loop(d, tag, &curl, rss_ip, cap).await;
}

/// Serial-console IP discovery (`lan` mode). Bounded; `None` if the IP never
/// appears within the window - the caller stops watching and the rack keeps
/// converging on its own.
async fn discover_rss_ip(d: &Runner, rss: NodeRef, tag: &str) -> Option<String> {
    let ip_deadline = Instant::now() + Duration::from_secs(60);
    let rss_ip = loop {
        match tokio::time::timeout(
            crate::net::SERIAL_RESOLVE_TIMEOUT,
            crate::net::node_external_ip(d, rss, false),
        )
        .await
        {
            Ok(Ok(ip)) => break ip,
            Ok(Err(e)) if Instant::now() >= ip_deadline => {
                warn!(
                    d.log,
                    "{tag}: can't find the RSS node's IP to watch over SSH ({e}); \
                    bring-up continues - check `voxel status` / the console"
                );
                return None;
            }
            Err(_) if Instant::now() >= ip_deadline => {
                warn!(
                    d.log,
                    "{tag}: timed out finding the RSS node's IP; bring-up \
                    continues - check `voxel status` / the console"
                );
                return None;
            }
            _ => tokio::time::sleep(Duration::from_secs(5)).await,
        }
    };
    Some(rss_ip)
}

/// Poll the bootstrap-agent's `/rack-initialize` over SSH until it initializes,
/// fails, or the cap expires. Emits step transitions + a periodic heartbeat.
async fn watch_rss_loop(d: &Runner, tag: &str, curl: &str, rss_ip: String, cap: Duration) {
    const POLL_INTERVAL: Duration = Duration::from_secs(8);
    const HEARTBEAT: Duration = Duration::from_secs(90);
    let start = Instant::now();
    let mut last = String::new();
    let mut last_emit = Instant::now();
    let mut step_start = Instant::now(); // when the current step began (for in-step timing)
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if start.elapsed() > cap {
            warn!(
                d.log,
                "{tag}: stopped watching after {}m - the rack may still be \
                 converging; check the console or re-run `voxel status`. Not failing \
                 the launch.",
                cap.as_secs() / 60
            );
            break;
        }
        let out = match crate::net::ssh_capture(&rss_ip, curl) {
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
                    && let Some(x) = crate::net::ssh_capture(&rss_ip, "svcs -x 2>/dev/null")
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
                        warn!(d.log, "{tag}: sled-agent log tail:\n{}", t.trim());
                    }
                    warn!(
                        d.log,
                        "{tag}: not waiting further - fix the service above, then relaunch."
                    );
                    break;
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
                    info!(d.log, "{tag}: still watching, {mins}m elapsed - {where_}");
                    last_emit = Instant::now();
                }
                continue;
            }
        };
        match json_str_field(&out, "status").as_str() {
            "initializing" => {
                let step = json_step(&out);
                if !step.is_empty() && step != last {
                    let (idx, label) = rss_step_display(&step);
                    info!(d.log, "{tag} [{}/{}]: {}", idx, RSS_STEPS.len(), label);
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
            "initialized" => {
                // A real init returns the rack's id; a null id means the
                // bootstrap-agent is reporting a stale/leftover "initialized"
                // ledger (e.g. emulated vdevs not wiped on relaunch) - not a real
                // bring-up. Don't celebrate it.
                let id = json_str_field(&out, "id");
                if id.is_empty() {
                    warn!(
                        d.log,
                        "{tag}: status=initialized but rack id is null - stale \
                         sled state, NOT a real init. Destroy and relaunch from clean \
                         storage (the emulated vdevs must be wiped)."
                    );
                } else {
                    info!(d.log, "{tag}: complete - rack initialized (rack {id})");
                }
                break;
            }
            "initialization_failed" => {
                warn!(d.log, "{tag} FAILED: {}", json_str_field(&out, "message"));
                break;
            }
            _ => {} // not serving yet / other - keep waiting
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
    fn strip_ansi_yields_clean_ip() {
        // ip(8) colorizes: ESC[36menp0s10ESC[0m ... ESC[35m192.168.68.171ESC[0m/22
        let colored =
            "\x1b[36menp0s10\x1b[0m \x1b[32mUP\x1b[0m \x1b[35m192.168.68.171\x1b[0m/22 metric 100";
        let clean = strip_ansi(colored);
        let ip = clean
            .split_whitespace()
            .find(|t| t.contains('.') && t.contains('/'))
            .and_then(|t| t.split('/').next())
            .unwrap();
        assert_eq!(ip, "192.168.68.171");
    }
}
