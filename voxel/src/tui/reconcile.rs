// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reconciles the TUI with what actually exists. Probes storage, node
//! processes, host routes and RSS state, and reduces that evidence to the
//! deployment state the TUI starts from or returns to after an operation.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::collector::{FalconExecutor, NodeExecutor, NodeTarget};
use super::context::TuiContext;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservedDeploymentState {
    Unknown,
    Stopped,
    Starting,
    Running,
    Degraded,
    Stopping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LifecycleIntent {
    Idle,
    Launch,
    Destroy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NodeEvidence {
    Missing,
    Booting,
    Running,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeObservation {
    pub(crate) id: String,
    pub(crate) state: NodeEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteEvidence {
    Applied,
    NotRequired,
    Unavailable,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RssObservation {
    Initializing { step: Option<String> },
    Initialized { id: String },
    StaleInitialized,
    Failed { message: String },
    Unavailable,
    UnknownResponse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TopologyEvidence {
    pub(crate) node_ids: Vec<String>,
    pub(crate) rack_ids: Vec<usize>,
    pub(crate) available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RackRssEvidence {
    pub(crate) rack: usize,
    pub(crate) observation: RssObservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReconciliationEvidence {
    pub(crate) topology: TopologyEvidence,
    pub(crate) nodes: Vec<NodeObservation>,
    pub(crate) racks: Vec<RackRssEvidence>,
    pub(crate) required_rss_rack_ids: Vec<usize>,
    pub(crate) routes: RouteEvidence,
    pub(crate) intent: LifecycleIntent,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReconciliationSummary {
    pub(crate) expected_nodes: usize,
    pub(crate) running_nodes: usize,
    pub(crate) live_nodes: usize,
    pub(crate) configured_racks: usize,
    pub(crate) initialized_racks: usize,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReconciliationResult {
    pub(crate) state: ObservedDeploymentState,
    pub(crate) summary: ReconciliationSummary,
}

pub(crate) fn reduce(
    evidence: &ReconciliationEvidence,
) -> ReconciliationResult {
    let node_ids: HashSet<_> = evidence.topology.node_ids.iter().collect();
    let rack_ids: HashSet<_> = evidence.topology.rack_ids.iter().collect();
    let seen_nodes: HashSet<_> =
        evidence.nodes.iter().map(|node| &node.id).collect();
    let seen_racks: HashSet<_> =
        evidence.racks.iter().map(|rack| &rack.rack).collect();
    let required_rss_ids: HashSet<_> =
        evidence.required_rss_rack_ids.iter().collect();
    let malformed_nodes = node_ids.len() != evidence.topology.node_ids.len()
        || seen_nodes.len() != evidence.nodes.len()
        || seen_nodes != node_ids;
    let malformed_racks = rack_ids.len() != evidence.topology.rack_ids.len()
        || seen_racks.len() != evidence.racks.len()
        || seen_racks != rack_ids;
    let malformed_required_racks = required_rss_ids.len()
        != evidence.required_rss_rack_ids.len()
        || !required_rss_ids.is_subset(&rack_ids);
    let running = evidence
        .nodes
        .iter()
        .filter(|node| node.state == NodeEvidence::Running)
        .count();
    let live = evidence
        .nodes
        .iter()
        .filter(|node| {
            !matches!(node.state, NodeEvidence::Missing | NodeEvidence::Unknown)
        })
        .count();
    let initialized = evidence
        .racks
        .iter()
        .filter(|rack| {
            required_rss_ids.contains(&rack.rack)
                && matches!(
                    rack.observation,
                    RssObservation::Initialized { .. }
                )
        })
        .count();
    let fatal =
        evidence.nodes.iter().any(|node| node.state == NodeEvidence::Failed)
            || evidence.racks.iter().any(|rack| {
                required_rss_ids.contains(&rack.rack)
                    && matches!(rack.observation, RssObservation::Failed { .. })
            })
            || evidence.routes == RouteEvidence::Failed;
    let invalid_topology = !evidence.topology.available
        || malformed_nodes
        || malformed_racks
        || malformed_required_racks;
    let node_uncertain =
        evidence.nodes.iter().any(|node| node.state == NodeEvidence::Unknown);
    let route_satisfied = matches!(
        evidence.routes,
        RouteEvidence::Applied | RouteEvidence::NotRequired
    );
    let rss_uncertain = evidence.racks.iter().any(|rack| {
        required_rss_ids.contains(&rack.rack)
            && matches!(
                rack.observation,
                RssObservation::Unavailable | RssObservation::UnknownResponse
            )
    });
    let mut warnings = Vec::new();
    for id in node_ids.difference(&seen_nodes) {
        warnings.push(format!("node {id} observation is missing"));
    }
    if seen_nodes.len() != evidence.nodes.len() {
        warnings.push("duplicate node observations".into());
    }
    for id in rack_ids.difference(&seen_racks) {
        warnings.push(format!("rack {id} observation is missing"));
    }
    if seen_racks.len() != evidence.racks.len() {
        warnings.push("duplicate rack observations".into());
    }
    if required_rss_ids.len() != evidence.required_rss_rack_ids.len() {
        warnings.push("duplicate required RSS rack IDs".into());
    }
    for id in required_rss_ids.difference(&rack_ids) {
        warnings.push(format!("required RSS rack {id} is not in topology"));
    }
    for node in &evidence.nodes {
        if node.state != NodeEvidence::Running {
            warnings.push(format!("node {} is {:?}", node.id, node.state));
        }
    }
    for rack in &evidence.racks {
        if required_rss_ids.contains(&rack.rack)
            && !matches!(rack.observation, RssObservation::Initialized { .. })
        {
            warnings.push(format!(
                "rack {} RSS is {:?}",
                rack.rack, rack.observation
            ));
        }
    }
    if !route_satisfied {
        warnings.push("required routes are not confirmed applied".into());
    }

    let state = if malformed_required_racks {
        ObservedDeploymentState::Unknown
    } else if fatal {
        ObservedDeploymentState::Degraded
    } else if invalid_topology {
        ObservedDeploymentState::Unknown
    } else if live == 0 && !node_uncertain {
        ObservedDeploymentState::Stopped
    } else if evidence.intent == LifecycleIntent::Destroy && live > 0 {
        ObservedDeploymentState::Stopping
    } else if evidence.intent == LifecycleIntent::Launch
        && evidence.nodes.iter().any(|node| {
            matches!(node.state, NodeEvidence::Booting | NodeEvidence::Running)
        })
    {
        ObservedDeploymentState::Starting
    } else if node_uncertain
        || (evidence.intent == LifecycleIntent::Idle
            && (rss_uncertain
                || matches!(
                    evidence.routes,
                    RouteEvidence::Unavailable | RouteEvidence::Unknown
                )))
    {
        ObservedDeploymentState::Unknown
    } else if running == evidence.topology.node_ids.len()
        && initialized == required_rss_ids.len()
        && route_satisfied
    {
        ObservedDeploymentState::Running
    } else {
        ObservedDeploymentState::Degraded
    };

    ReconciliationResult {
        state,
        summary: ReconciliationSummary {
            expected_nodes: evidence.topology.node_ids.len(),
            running_nodes: running,
            live_nodes: live,
            configured_racks: evidence.topology.rack_ids.len(),
            initialized_racks: initialized,
            warnings,
        },
    }
}

async fn bounded_output(
    mut command: tokio::process::Command,
    label: &str,
    shutdown: &CancellationToken,
) -> anyhow::Result<std::process::Output> {
    command.kill_on_drop(true).stdin(Stdio::null());
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => anyhow::bail!("{label} cancelled"),
        result = tokio::time::timeout(PROBE_TIMEOUT, command.output()) => {
            result
                .map_err(|_| anyhow::anyhow!("{label} timed out after {PROBE_TIMEOUT:?}"))?
                .map_err(|error| anyhow::anyhow!("{label}: {error}"))
        }
    }
}

async fn deployment_storage_exists(
    context: &TuiContext,
    shutdown: &CancellationToken,
) -> anyhow::Result<Option<bool>> {
    let dataset = format!("{}/topo/{}", context.dataset, context.name);
    let mut command = tokio::process::Command::new("zfs");
    command.args(["list", "-H", "-o", "name", &dataset]);
    match bounded_output(command, "deployment storage probe", shutdown).await {
        Ok(output) if output.status.success() => Ok(Some(true)),
        Ok(output)
            if String::from_utf8_lossy(&output.stderr)
                .to_ascii_lowercase()
                .contains("does not exist") =>
        {
            Ok(Some(false))
        }
        Ok(_) => Ok(None),
        Err(error) if shutdown.is_cancelled() => Err(error),
        Err(_) => Ok(None),
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

/// Asks a sled's bootstrap agent, over SSH, how rack setup is going.
pub(crate) fn rss_probe_command(
    bootstrap_addr: impl std::fmt::Display,
) -> String {
    format!(
        "curl -s --max-time 5 http://[{bootstrap_addr}]:8080/rack-initialize 2>/dev/null"
    )
}

pub(crate) fn parse_rss_observation(body: &str) -> RssObservation {
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
        "initialized" => {
            let id = json_string_field(body, "id");
            if id.is_empty() {
                RssObservation::StaleInitialized
            } else {
                RssObservation::Initialized { id }
            }
        }
        "initialization_failed" => RssObservation::Failed {
            message: json_string_field(body, "message"),
        },
        _ => RssObservation::UnknownResponse,
    }
}

fn route_evidence_from_table(
    context: &TuiContext,
    table: &str,
) -> RouteEvidence {
    let applied = (0..context.config.topology.racks()).all(|rack| {
        let prefix = context.config.network.for_rack(rack).infra_prefix;
        let destination = prefix.split('/').next().unwrap_or(&prefix);
        table
            .lines()
            .any(|line| line.split_whitespace().next() == Some(destination))
    });
    if applied { RouteEvidence::Applied } else { RouteEvidence::Unknown }
}

fn classify_pid_probe(result: io::Result<()>) -> NodeEvidence {
    match result {
        Ok(()) => NodeEvidence::Running,
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
            NodeEvidence::Running
        }
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
            NodeEvidence::Missing
        }
        Err(_) => NodeEvidence::Unknown,
    }
}

fn probe_pid(raw: &str) -> NodeEvidence {
    let Ok(pid) = raw.trim().parse::<libc::pid_t>() else {
        return NodeEvidence::Unknown;
    };
    if pid <= 0 {
        return NodeEvidence::Unknown;
    }

    // SAFETY: signal 0 does not deliver a signal; a positive pid limits the
    // probe to the one process named by Falcon's pid file.
    let result = if unsafe { libc::kill(pid, 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    classify_pid_probe(result)
}

pub(crate) async fn collect(
    context: &TuiContext,
    executor: &FalconExecutor,
    intent: LifecycleIntent,
    shutdown: &CancellationToken,
) -> anyhow::Result<ReconciliationEvidence> {
    let sleds = context.config.sleds();
    let nodes_and_targets: Vec<_> = sleds
        .iter()
        .map(|sled| {
            (sled.name.clone(), NodeTarget::Sled { name: sled.name.clone() })
        })
        .chain(context.config.topology.routers.iter().map(|name| {
            (name.clone(), NodeTarget::Router { name: name.clone() })
        }))
        .collect();
    let node_ids =
        nodes_and_targets.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
    let rack_ids: Vec<_> = (0..context.config.topology.racks()).collect();
    let mut pid_states = Vec::with_capacity(node_ids.len());
    let mut any_live = false;
    let mut all_pid_files_absent = !node_ids.is_empty();
    for id in &node_ids {
        let path = context.workdir.join(".falcon").join(format!("{id}.pid"));
        let state = match tokio::fs::read_to_string(path).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                NodeEvidence::Missing
            }
            Err(_) => {
                all_pid_files_absent = false;
                NodeEvidence::Unknown
            }
            Ok(pid) => {
                all_pid_files_absent = false;
                let state = probe_pid(&pid);
                if state == NodeEvidence::Running {
                    any_live = true;
                }
                state
            }
        };
        pid_states.push(state);
    }

    let mut nodes = Vec::with_capacity(node_ids.len());
    for ((id, target), mut state) in nodes_and_targets
        .iter()
        .zip(pid_states)
        .map(|(node, state)| ((node.0.clone(), &node.1), state))
    {
        if state == NodeEvidence::Running {
            state = match executor.execute(target, "true").await {
                Ok(_) => NodeEvidence::Running,
                Err(_) if intent == LifecycleIntent::Launch => {
                    NodeEvidence::Booting
                }
                Err(_) => NodeEvidence::Unknown,
            };
            if shutdown.is_cancelled() {
                anyhow::bail!("node observation cancelled");
            }
        }
        nodes.push(NodeObservation { id, state });
    }
    if all_pid_files_absent
        && deployment_storage_exists(context, shutdown).await? != Some(false)
        && !nodes.is_empty()
    {
        nodes[0].state = NodeEvidence::Unknown;
    }

    let mut rack_observations: BTreeMap<_, _> = rack_ids
        .iter()
        .map(|rack| (*rack, RssObservation::Unavailable))
        .collect();
    let mut seen_racks = HashSet::new();
    let rss_sleds = sleds
        .iter()
        .filter(|sled| sled.rss && seen_racks.insert(sled.rack))
        .collect::<Vec<_>>();
    let mut observed_rss_rack_ids =
        rss_sleds.iter().map(|sled| sled.rack).collect::<Vec<_>>();
    if any_live {
        for sled in rss_sleds {
            let command = rss_probe_command(sled.bootstrap_addr());
            let target = NodeTarget::Sled { name: sled.name.clone() };
            let observation = match executor.execute(&target, &command).await {
                Ok(body) => parse_rss_observation(&body),
                Err(_) => RssObservation::Unavailable,
            };
            if shutdown.is_cancelled() {
                anyhow::bail!("RSS observation cancelled");
            }
            rack_observations.insert(sled.rack, observation);
        }
    }
    observed_rss_rack_ids.sort_unstable();
    let required_rss_rack_ids =
        observed_rss_rack_ids.into_iter().take(1).collect();

    let routes =
        if !context.config.topology.routers.iter().any(|router| router == "ce")
        {
            RouteEvidence::NotRequired
        } else {
            let mut command = tokio::process::Command::new("netstat");
            command.args(["-rn", "-f", "inet"]);
            match bounded_output(command, "route table probe", shutdown).await {
                Ok(output) if output.status.success() => {
                    route_evidence_from_table(
                        context,
                        &String::from_utf8_lossy(&output.stdout),
                    )
                }
                Ok(_) => RouteEvidence::Failed,
                Err(error) if shutdown.is_cancelled() => return Err(error),
                Err(error) if error.to_string().contains("No such file") => {
                    RouteEvidence::Unavailable
                }
                Err(_) => RouteEvidence::Unknown,
            }
        };

    executor.drain_serial_tasks().await;
    Ok(ReconciliationEvidence {
        topology: TopologyEvidence { node_ids, rack_ids, available: true },
        nodes,
        racks: rack_observations
            .into_iter()
            .map(|(rack, observation)| RackRssEvidence { rack, observation })
            .collect(),
        required_rss_rack_ids,
        routes,
        intent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, state: NodeEvidence) -> NodeObservation {
        NodeObservation { id: id.into(), state }
    }

    fn rack(rack: usize, observation: RssObservation) -> RackRssEvidence {
        RackRssEvidence { rack, observation }
    }

    fn base() -> ReconciliationEvidence {
        ReconciliationEvidence {
            topology: TopologyEvidence {
                node_ids: vec!["n1".into(), "n2".into()],
                rack_ids: vec![0, 1],
                available: true,
            },
            nodes: vec![
                node("n1", NodeEvidence::Running),
                node("n2", NodeEvidence::Running),
            ],
            racks: vec![
                rack(0, RssObservation::Initialized { id: "r0".into() }),
                rack(1, RssObservation::Initialized { id: "r1".into() }),
            ],
            required_rss_rack_ids: vec![0, 1],
            routes: RouteEvidence::Applied,
            intent: LifecycleIntent::Idle,
        }
    }

    #[test]
    fn reduce_applies_state_precedence_to_each_evidence_shape() {
        type Mutate = fn(&mut ReconciliationEvidence);
        let cases: [(&str, Mutate, ObservedDeploymentState); 5] = [
            (
                "every node missing",
                |evidence| {
                    evidence
                        .nodes
                        .iter_mut()
                        .for_each(|node| node.state = NodeEvidence::Missing);
                },
                ObservedDeploymentState::Stopped,
            ),
            (
                "launch with a node booting",
                |evidence| {
                    evidence.intent = LifecycleIntent::Launch;
                    evidence.nodes[0].state = NodeEvidence::Booting;
                },
                ObservedDeploymentState::Starting,
            ),
            (
                "destroy with a node still live",
                |evidence| {
                    evidence.intent = LifecycleIntent::Destroy;
                    evidence.nodes[1].state = NodeEvidence::Missing;
                },
                ObservedDeploymentState::Stopping,
            ),
            (
                "idle with a failed node",
                |evidence| evidence.nodes[0].state = NodeEvidence::Failed,
                ObservedDeploymentState::Degraded,
            ),
            (
                "idle with unknown routes",
                |evidence| evidence.routes = RouteEvidence::Unknown,
                ObservedDeploymentState::Unknown,
            ),
        ];
        for (case, mutate, expected) in cases {
            let mut evidence = base();
            mutate(&mut evidence);
            assert_eq!(reduce(&evidence).state, expected, "{case}");
        }
    }

    #[test]
    fn only_required_rss_racks_affect_running_state() {
        let mut healthy = base();
        healthy.required_rss_rack_ids = vec![0];
        assert_eq!(reduce(&healthy).state, ObservedDeploymentState::Running);
        // An uncertain held rack must not make the state Unknown, nor a
        // failed one make it Degraded.
        for observation in [
            RssObservation::Unavailable,
            RssObservation::Failed { message: "held rack failed".into() },
        ] {
            let mut evidence = healthy.clone();
            evidence.racks[1].observation = observation.clone();
            assert_eq!(reduce(&evidence), reduce(&healthy), "{observation:?}");
        }
    }

    #[test]
    fn pid_probes_map_to_node_evidence() {
        let errno = std::io::Error::from_raw_os_error;
        for (case, probe, expected) in [
            ("probe succeeded", Ok(()), NodeEvidence::Running),
            // kill(pid, 0) on another user's live process: still running.
            (
                "permission denied",
                Err(errno(libc::EPERM)),
                NodeEvidence::Running,
            ),
            ("no such process", Err(errno(libc::ESRCH)), NodeEvidence::Missing),
            (
                "unexpected error",
                Err(errno(libc::EINVAL)),
                NodeEvidence::Unknown,
            ),
        ] {
            assert_eq!(classify_pid_probe(probe), expected, "{case}");
        }
        // A pid file that is garbage, or names no single process, proves nothing.
        for raw in ["", "abc", "0", "-1"] {
            assert_eq!(probe_pid(raw), NodeEvidence::Unknown, "pid {raw:?}");
        }
        let own = format!("{}\n", std::process::id());
        assert_eq!(probe_pid(&own), NodeEvidence::Running, "this process");
    }

    #[test]
    fn rss_responses_and_route_tables_become_evidence() {
        for (body, expected) in [
            ("", RssObservation::Unavailable),
            (
                r#"{"status":"initializing","step":{"status":"sled_init"}}"#,
                RssObservation::Initializing { step: Some("sled_init".into()) },
            ),
            (
                r#"{"status":"initialized","id":"a1b2"}"#,
                RssObservation::Initialized { id: "a1b2".into() },
            ),
            (r#"{"status":"initialized"}"#, RssObservation::StaleInitialized),
            (
                r#"{"status":"initialization_failed","message":"no sleds"}"#,
                RssObservation::Failed { message: "no sleds".into() },
            ),
            (r#"<html>bad gateway</html>"#, RssObservation::UnknownResponse),
        ] {
            assert_eq!(parse_rss_observation(body), expected, "{body:?}");
        }

        let context = TuiContext::new(
            "/cfg".into(),
            "/work".into(),
            "demo".into(),
            "rpool/falcon".into(),
            "/build".into(),
            crate::tui::test_support::config("[topology]\nracks = 2\n"),
            "/bin/voxel".into(),
            vec![],
        );
        let route = |rack: usize| {
            let prefix = context.config.network.for_rack(rack).infra_prefix;
            let destination = prefix.split('/').next().unwrap().to_owned();
            format!("{destination}  172.30.199.13  UG  2  0")
        };
        let both =
            format!("default 192.168.1.1 UG 10 0\n{}\n{}", route(0), route(1));
        assert_eq!(
            route_evidence_from_table(&context, &both),
            RouteEvidence::Applied,
            "a route to every rack"
        );
        assert_eq!(
            route_evidence_from_table(&context, &route(0)),
            RouteEvidence::Unknown,
            "rack 1 missing"
        );
        assert_eq!(
            route_evidence_from_table(&context, "default 192.168.1.1 UG 10 0"),
            RouteEvidence::Unknown,
            "only unrelated routes"
        );
    }
}
