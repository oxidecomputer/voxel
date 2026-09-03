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
            let command = format!(
                "curl -s --max-time 5 http://[{}]:8080/rack-initialize 2>/dev/null",
                sled.bootstrap_addr()
            );
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
    fn successful_pid_probe_means_running() {
        assert_eq!(classify_pid_probe(Ok(())), NodeEvidence::Running);
    }

    #[test]
    fn pid_probe_permission_denied_still_means_running() {
        assert_eq!(
            classify_pid_probe(Err(std::io::Error::from_raw_os_error(
                libc::EPERM
            ))),
            NodeEvidence::Running
        );
    }

    #[test]
    fn pid_probe_missing_process_means_missing() {
        assert_eq!(
            classify_pid_probe(Err(std::io::Error::from_raw_os_error(
                libc::ESRCH
            ))),
            NodeEvidence::Missing
        );
    }

    #[test]
    fn pid_probe_unexpected_error_is_unknown() {
        assert_eq!(
            classify_pid_probe(Err(std::io::Error::from_raw_os_error(
                libc::EINVAL
            ))),
            NodeEvidence::Unknown
        );
    }

    #[test]
    fn reduces_complete_live_evidence_to_running() {
        assert_eq!(reduce(&base()).state, ObservedDeploymentState::Running);
    }

    #[test]
    fn covers_stopped_starting_stopping_degraded_and_unknown() {
        let mut evidence = base();
        evidence
            .nodes
            .iter_mut()
            .for_each(|node| node.state = NodeEvidence::Missing);
        assert_eq!(reduce(&evidence).state, ObservedDeploymentState::Stopped);

        evidence.nodes[0].state = NodeEvidence::Booting;
        evidence.intent = LifecycleIntent::Launch;
        assert_eq!(reduce(&evidence).state, ObservedDeploymentState::Starting);

        evidence.nodes[0].state = NodeEvidence::Running;
        evidence.intent = LifecycleIntent::Destroy;
        assert_eq!(reduce(&evidence).state, ObservedDeploymentState::Stopping);

        evidence.intent = LifecycleIntent::Idle;
        evidence.nodes[0].state = NodeEvidence::Failed;
        assert_eq!(reduce(&evidence).state, ObservedDeploymentState::Degraded);

        evidence.nodes[0].state = NodeEvidence::Running;
        evidence.routes = RouteEvidence::Unknown;
        assert_eq!(reduce(&evidence).state, ObservedDeploymentState::Unknown);
    }

    #[test]
    fn only_required_rss_racks_affect_running_state() {
        for observation in [
            RssObservation::Unavailable,
            RssObservation::StaleInitialized,
            RssObservation::Failed { message: "held rack failed".into() },
        ] {
            let mut evidence = base();
            evidence.required_rss_rack_ids = vec![0];
            evidence.racks[1].observation = observation;
            let result = reduce(&evidence);
            assert_eq!(result.state, ObservedDeploymentState::Running);
            assert!(
                !result
                    .summary
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("rack 1 RSS"))
            );
        }
    }

    #[test]
    fn malformed_required_rss_racks_are_unknown() {
        for required in [vec![0, 99], vec![0, 0]] {
            let mut evidence = base();
            evidence.required_rss_rack_ids = required;
            evidence.nodes[0].state = NodeEvidence::Failed;
            assert_eq!(
                reduce(&evidence).state,
                ObservedDeploymentState::Unknown
            );
        }
    }

    #[test]
    fn live_evidence_overrides_successful_launch_outcome() {
        let mut stopped_after_successful_launch = base();
        stopped_after_successful_launch
            .nodes
            .iter_mut()
            .for_each(|node| node.state = NodeEvidence::Missing);
        assert_eq!(
            reduce(&stopped_after_successful_launch).state,
            ObservedDeploymentState::Stopped
        );
    }

    #[test]
    fn live_evidence_overrides_failed_destroy_outcome() {
        let mut stopped_after_failed_destroy = base();
        stopped_after_failed_destroy
            .nodes
            .iter_mut()
            .for_each(|node| node.state = NodeEvidence::Missing);
        assert_eq!(
            reduce(&stopped_after_failed_destroy).state,
            ObservedDeploymentState::Stopped
        );
    }
}
