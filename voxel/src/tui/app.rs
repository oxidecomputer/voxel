use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use crate::tui::event::{
    Action, AppEvent, CancelChoice, Confirmation, ConfirmationOptionAction,
    DeploymentPane, Direction, Effect, MonitoringPane, OperationRequestId,
    View,
};
use crate::tui::operation::{
    CommandOutcome, LogLevel, OperationEvent, OperationKind, OperationPhase,
    OperationWarning,
};
use crate::tui::reconcile::{
    ObservedDeploymentState, ReconciliationResult, RouteEvidence,
    RssObservation,
};
use crate::tui::telemetry::{
    HealthDiagnostic, LatestSample, NodeAddresses, RackId, ResourceDescriptor,
    ResourceId, TelemetryModel,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostOperationExit {
    Detach,
    Destroy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionExit {
    Detach,
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    pub view: View,
    pub deployment_pane: DeploymentPane,
    pub monitoring_pane: MonitoringPane,
    pub collapsed_deployment: BTreeSet<DeploymentPane>,
    pub collapsed_monitoring: BTreeSet<MonitoringPane>,
    pub phase_scroll: usize,
    pub subtask_scroll: usize,
    pub monitor_scroll: usize,
    pub top_zones_scroll: usize,
    pub help_scroll: usize,
    pub help_open: bool,
    pub selected_rack: Option<RackId>,
    pub selected_resource: Option<ResourceId>,
    pub terminal: TerminalSize,
    pub detail_open: bool,
    pub confirmation: Option<Confirmation>,
    pub confirmation_selection: usize,
    pub post_operation_exit: Option<PostOperationExit>,
    pub exit: Option<SessionExit>,
    pub quitting: bool,
}

impl SessionState {
    pub(crate) fn deployment_expanded(&self, pane: DeploymentPane) -> bool {
        !self.collapsed_deployment.contains(&pane)
    }

    pub(crate) fn monitoring_expanded(&self, pane: MonitoringPane) -> bool {
        !self.collapsed_monitoring.contains(&pane)
    }
}

#[derive(Clone, Debug)]
pub struct DeploymentState {
    pub topology: Vec<ResourceDescriptor>,
    pub observed: ObservedDeploymentState,
    pub rss: BTreeMap<RackId, LatestSample<RssObservation>>,
    pub routes: RouteEvidence,
    pub last_reconciliation: Option<ReconciliationResult>,
    pub last_reconciliation_at: Option<Instant>,
    pub reconciliation_failure: Option<String>,
    pub reconciliation_in_progress: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Progress {
    pub completed: usize,
    pub total: usize,
    pub message: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogEntry {
    pub source: LogSource,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogSource {
    Application,
    Operation,
}

impl LogEntry {
    pub fn application(level: LogLevel, message: impl Into<String>) -> Self {
        Self { source: LogSource::Application, level, message: message.into() }
    }

    pub fn operation(level: LogLevel, message: impl Into<String>) -> Self {
        Self { source: LogSource::Operation, level, message: message.into() }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogFilter {
    All,
    Info,
    Warning,
    Error,
}
impl LogFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Info,
            Self::Info => Self::Warning,
            Self::Warning => Self::Error,
            Self::Error => Self::All,
        }
    }
    pub fn accepts(self, level: LogLevel) -> bool {
        self == Self::All
            || matches!(
                (self, level),
                (Self::Info, LogLevel::Info)
                    | (Self::Warning, LogLevel::Warning)
                    | (Self::Error, LogLevel::Error)
            )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedLogs {
    pub entries: VecDeque<LogEntry>,
    pub capacity: usize,
    pub scroll: usize,
}
impl BoundedLogs {
    pub fn new(capacity: usize) -> Self {
        Self { entries: VecDeque::new(), capacity, scroll: 0 }
    }
    pub fn push(&mut self, entry: LogEntry) {
        if self.capacity == 0 {
            self.entries.clear();
            self.scroll = 0;
            return;
        }
        let evicted = self.entries.len() == self.capacity;
        if evicted {
            self.entries.pop_front();
        } else if self.scroll > 0 {
            self.scroll = self.scroll.saturating_add(1);
        }
        self.entries.push_back(entry);
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(1));
    }
    pub fn push_filtered(&mut self, entry: LogEntry, filter: LogFilter) {
        let previous_scroll = self.scroll;
        let accepted_append = filter.accepts(entry.level);
        let accepted_eviction = self.entries.len() == self.capacity
            && self
                .entries
                .front()
                .is_some_and(|oldest| filter.accepts(oldest.level));
        self.push(entry);
        if previous_scroll > 0 {
            self.scroll = previous_scroll
                .saturating_add(usize::from(accepted_append))
                .saturating_sub(usize::from(accepted_eviction));
        }
    }
    pub fn clamp(&mut self, visible: usize) {
        self.scroll =
            self.scroll.min(self.entries.len().saturating_sub(visible));
    }
    pub fn scroll(&mut self, delta: isize, visible: usize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        self.clamp(visible);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveOperation {
    pub request_id: OperationRequestId,
    pub kind: OperationKind,
    pub phase: Option<OperationPhase>,
    pub completed_phases: Vec<OperationPhase>,
    pub progress: Option<Progress>,
    pub warnings: Vec<OperationWarning>,
    pub cancelling: bool,
    pub cancel_choice: Option<CancelChoice>,
    pub force_stop_requested: bool,
}
impl ActiveOperation {
    pub fn new(request_id: OperationRequestId, kind: OperationKind) -> Self {
        Self {
            request_id,
            kind,
            phase: None,
            completed_phases: vec![],
            progress: None,
            warnings: vec![],
            cancelling: false,
            cancel_choice: None,
            force_stop_requested: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperation {
    pub request_id: OperationRequestId,
    pub kind: OperationKind,
}
impl PendingOperation {
    pub fn new(request_id: OperationRequestId, kind: OperationKind) -> Self {
        Self { request_id, kind }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationState {
    pub pending: Option<PendingOperation>,
    pub active: Option<ActiveOperation>,
    pub retained_warnings: Vec<OperationWarning>,
    pub outcome: Option<CommandOutcome>,
    pub start_failure: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ObservabilityState {
    pub telemetry: TelemetryModel,
    pub health: BTreeMap<ResourceId, LatestSample<HealthDiagnostic>>,
    pub addresses: BTreeMap<ResourceId, LatestSample<NodeAddresses>>,
    pub traffic_failures: BTreeMap<ResourceId, LatestSample<()>>,
    pub latest_traffic_generation: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct App {
    pub session: SessionState,
    pub deployment: DeploymentState,
    pub operation: OperationState,
    pub observability: ObservabilityState,
    pub logs: BoundedLogs,
    pub logs_filter: LogFilter,
    pub durable_log_path: String,
    pub reattach_command: Option<String>,
    pub clipboard_copied: bool,
    pub now: Option<Instant>,
    next_operation_request_id: Option<OperationRequestId>,
}

impl App {
    pub fn new(
        topology: Vec<ResourceDescriptor>,
        log_capacity: usize,
        history_capacity: usize,
    ) -> Self {
        let topology = Self::normalize_topology(topology);
        let telemetry = TelemetryModel::new(topology.clone(), history_capacity);
        let health = topology
            .iter()
            .map(|d| (d.id.clone(), LatestSample::default()))
            .collect();
        let traffic_failures = topology
            .iter()
            .map(|d| (d.id.clone(), LatestSample::default()))
            .collect();
        let addresses = topology
            .iter()
            .map(|d| (d.id.clone(), LatestSample::default()))
            .collect();
        let rss = topology
            .iter()
            .filter_map(|d| d.rack)
            .map(|rack| (rack, LatestSample::default()))
            .collect();
        let mut app = Self {
            session: SessionState {
                view: View::Deployment,
                deployment_pane: DeploymentPane::Phases,
                monitoring_pane: MonitoringPane::Topology,
                collapsed_deployment: BTreeSet::new(),
                collapsed_monitoring: BTreeSet::new(),
                phase_scroll: 0,
                subtask_scroll: 0,
                monitor_scroll: 0,
                top_zones_scroll: 0,
                help_scroll: 0,
                help_open: false,
                selected_rack: None,
                selected_resource: None,
                terminal: TerminalSize { width: 0, height: 0 },
                detail_open: false,
                confirmation: None,
                confirmation_selection: 0,
                post_operation_exit: None,
                exit: None,
                quitting: false,
            },
            deployment: DeploymentState {
                topology,
                observed: ObservedDeploymentState::Unknown,
                rss,
                routes: RouteEvidence::Unknown,
                last_reconciliation: None,
                last_reconciliation_at: None,
                reconciliation_failure: None,
                reconciliation_in_progress: false,
            },
            operation: OperationState {
                pending: None,
                active: None,
                retained_warnings: vec![],
                outcome: None,
                start_failure: None,
            },
            observability: ObservabilityState {
                telemetry,
                health,
                addresses,
                traffic_failures,
                latest_traffic_generation: None,
            },
            logs: BoundedLogs::new(log_capacity),
            logs_filter: LogFilter::All,
            durable_log_path: "voxel-tui.log".into(),
            reattach_command: None,
            clipboard_copied: false,
            now: None,
            next_operation_request_id: Some(OperationRequestId::FIRST),
        };
        app.repair_selection();
        app
    }

    pub fn update(&mut self, event: AppEvent) -> Vec<Effect> {
        let mut effects = vec![];
        match event {
            AppEvent::Action(action) => return self.action(action),
            AppEvent::Tick { now } => self.now = Some(now),
            AppEvent::Resize { width, height } => {
                self.session.terminal = TerminalSize { width, height }
            }
            AppEvent::ReconciliationStarted { at } => {
                self.deployment.reconciliation_in_progress = true;
                self.deployment.last_reconciliation_at = Some(at);
                self.deployment.reconciliation_failure = None;
            }
            AppEvent::Reconciled { at, result, routes } => {
                self.deployment.reconciliation_in_progress = false;
                self.deployment.observed = result.state;
                self.deployment.last_reconciliation = Some(result);
                self.deployment.last_reconciliation_at = Some(at);
                self.deployment.reconciliation_failure = None;
                self.deployment.routes = routes;
            }
            AppEvent::ReconciliationFailed { at, message } => {
                self.deployment.reconciliation_in_progress = false;
                self.deployment.last_reconciliation_at = Some(at);
                self.deployment.reconciliation_failure = Some(message);
            }
            AppEvent::Operation { request_id, event } => {
                effects = self.operation_event(request_id, event)
            }
            AppEvent::OperationStartFailed { request_id, kind, message } => {
                if self.operation.pending.as_ref().is_some_and(|pending| {
                    pending.request_id == request_id && pending.kind == kind
                }) {
                    self.operation.pending = None;
                    self.operation.start_failure = Some(message);
                    self.session.post_operation_exit = None;
                }
            }
            AppEvent::Traffic { id, at, sample } => {
                if self.observability.telemetry.resources.contains_key(&id)
                    && Self::accept_attempt(
                        &self.observability.traffic_failures[&id],
                        at,
                    )
                    && self.accept_traffic_generation(at)
                {
                    self.observability
                        .telemetry
                        .set_current_sample(&id, at, sample);
                    self.observability.telemetry.rebuild_aggregates(at);
                    self.observability
                        .traffic_failures
                        .entry(id)
                        .or_default()
                        .record_success(at, ());
                }
            }
            AppEvent::TrafficFailed { id, at, message } => {
                if self
                    .observability
                    .traffic_failures
                    .get(&id)
                    .is_some_and(|sample| Self::accept_attempt(sample, at))
                    && self.accept_traffic_generation(at)
                {
                    let sample = self
                        .observability
                        .traffic_failures
                        .get_mut(&id)
                        .unwrap();
                    sample.record_error(at, message);
                }
            }
            AppEvent::Health { id, at, diagnostic } => {
                if self
                    .observability
                    .health
                    .get(&id)
                    .is_some_and(|sample| Self::accept_attempt(sample, at))
                {
                    let sample =
                        self.observability.health.get_mut(&id).unwrap();
                    sample.record_success(at, diagnostic);
                }
            }
            AppEvent::HealthFailed { id, at, message } => {
                if self
                    .observability
                    .health
                    .get(&id)
                    .is_some_and(|sample| Self::accept_attempt(sample, at))
                {
                    let sample =
                        self.observability.health.get_mut(&id).unwrap();
                    sample.record_error(at, message);
                }
            }
            AppEvent::Addresses { id, at, addresses } => {
                if self
                    .observability
                    .addresses
                    .get(&id)
                    .is_some_and(|sample| Self::accept_attempt(sample, at))
                {
                    self.observability
                        .addresses
                        .get_mut(&id)
                        .unwrap()
                        .record_success(at, addresses);
                }
            }
            AppEvent::AddressesFailed { id, at, message } => {
                if self
                    .observability
                    .addresses
                    .get(&id)
                    .is_some_and(|sample| Self::accept_attempt(sample, at))
                {
                    self.observability
                        .addresses
                        .get_mut(&id)
                        .unwrap()
                        .record_error(at, message);
                }
            }
            AppEvent::Rss { rack, at, observation } => {
                if self
                    .deployment
                    .rss
                    .get(&rack)
                    .is_some_and(|sample| Self::accept_attempt(sample, at))
                {
                    let sample = self.deployment.rss.get_mut(&rack).unwrap();
                    sample.record_success(at, observation);
                }
            }
            AppEvent::RssFailed { rack, at, message } => {
                if self
                    .deployment
                    .rss
                    .get(&rack)
                    .is_some_and(|sample| Self::accept_attempt(sample, at))
                {
                    let sample = self.deployment.rss.get_mut(&rack).unwrap();
                    sample.record_error(at, message);
                }
            }
            AppEvent::DurableLogFailed { message } => self.logs.push_filtered(
                LogEntry::application(LogLevel::Error, message),
                self.logs_filter,
            ),
        }
        self.clamp_log_scroll();
        effects
    }

    fn action(&mut self, action: Action) -> Vec<Effect> {
        if let Some(confirmation) = self.session.confirmation.clone() {
            let options = confirmation.options(self.can_cancel());
            return match (confirmation.clone(), action) {
                (_, Action::Close) => {
                    self.session.confirmation = None;
                    vec![]
                }
                (_, Action::Scroll { delta, .. }) => {
                    self.session.confirmation_selection = self
                        .session
                        .confirmation_selection
                        .saturating_add_signed(delta)
                        .min(options.len().saturating_sub(1));
                    vec![]
                }
                (_, Action::Activate) => {
                    self.activate_confirmation_option(confirmation)
                }
                (_, Action::Reject) => {
                    self.session.confirmation = None;
                    vec![]
                }
                (Confirmation::Detach, Action::CopyReattachCommand) => {
                    if self.can_detach()
                        && let Some(command) = self.reattach_command.clone()
                    {
                        self.clipboard_copied = true;
                        vec![Effect::CopyToClipboard(command)]
                    } else {
                        vec![]
                    }
                }
                _ => vec![],
            };
        }
        if self.session.post_operation_exit.is_none() && !self.session.quitting
        {
            match action {
                Action::RequestDetach if self.can_detach() => {
                    self.open_confirmation(Confirmation::Detach);
                    return vec![];
                }
                Action::RequestQuit => {
                    let confirmation = if self.resources_may_exist() {
                        Confirmation::QuitAndDestroy
                    } else {
                        Confirmation::Quit
                    };
                    self.open_confirmation(confirmation);
                    return vec![];
                }
                _ => {}
            }
        }
        if self.session.help_open {
            return match action {
                Action::ToggleHelp | Action::Close => {
                    self.session.help_open = false;
                    vec![]
                }
                Action::Scroll { delta, page } => {
                    let amount = if page {
                        crate::tui::ui::help::page_capacity(self)
                    } else {
                        1
                    } as isize;
                    self.session.help_scroll = self
                        .session
                        .help_scroll
                        .saturating_add_signed(delta.saturating_mul(amount));
                    vec![]
                }
                _ => vec![],
            };
        }
        if self.session.detail_open {
            return match action {
                Action::Activate | Action::Close => {
                    self.session.detail_open = false;
                    vec![]
                }
                Action::ToggleHelp => {
                    self.session.help_open = true;
                    self.session.help_scroll = 0;
                    vec![]
                }
                Action::Scroll { delta, page: false } => {
                    if delta != 0 {
                        self.move_resource(if delta > 0 {
                            Direction::Next
                        } else {
                            Direction::Previous
                        });
                    }
                    vec![]
                }
                _ => vec![],
            };
        }
        match action {
            Action::SwitchView(view) => {
                self.session.view = view;
                if view == View::Deployment {
                    self.session.detail_open = false;
                }
                self.repair_selection();
            }
            Action::NextRack
                if self.session.view == View::Monitor
                    && self.session.monitoring_pane
                        == MonitoringPane::RackSummary
                    && self
                        .session
                        .monitoring_expanded(MonitoringPane::RackSummary) =>
            {
                self.move_rack(Direction::Next)
            }
            Action::PreviousRack
                if self.session.view == View::Monitor
                    && self.session.monitoring_pane
                        == MonitoringPane::RackSummary
                    && self
                        .session
                        .monitoring_expanded(MonitoringPane::RackSummary) =>
            {
                self.move_rack(Direction::Previous)
            }
            Action::NextItem => self.move_item(Direction::Next),
            Action::PreviousItem => self.move_item(Direction::Previous),
            Action::ToggleSection => match self.session.view {
                View::Deployment => {
                    let pane = self.session.deployment_pane;
                    if !self.session.collapsed_deployment.remove(&pane) {
                        self.session.collapsed_deployment.insert(pane);
                    }
                }
                View::Monitor => {
                    let pane = self.session.monitoring_pane;
                    if !self.session.collapsed_monitoring.remove(&pane) {
                        self.session.collapsed_monitoring.insert(pane);
                    }
                }
            },
            Action::Activate
                if self.session.view == View::Monitor
                    && self.session.monitoring_pane
                        == MonitoringPane::Topology
                    && self
                        .session
                        .monitoring_expanded(MonitoringPane::Topology)
                    && self.session.selected_resource.is_some()
                    && !self.session.help_open
                    && self.session.confirmation.is_none() =>
            {
                self.session.detail_open = !self.session.detail_open
            }
            Action::ToggleHelp if self.session.confirmation.is_none() => {
                self.session.help_open = !self.session.help_open;
                if self.session.help_open {
                    self.session.help_scroll = 0;
                }
            }
            Action::Close => {
                if self.session.confirmation.take().is_some() {
                    return vec![];
                }
                if self.session.help_open {
                    self.session.help_open = false
                } else if self.session.detail_open {
                    self.session.detail_open = false
                } else {
                    self.session.selected_resource = None
                }
            }
            Action::Reject => self.session.confirmation = None,
            Action::RequestLaunch
                if self.session.view == View::Deployment
                    && self.can_start(OperationKind::Launch) =>
            {
                self.open_confirmation(Confirmation::Launch)
            }
            Action::RequestRoute
                if self.session.view == View::Deployment
                    && self.can_start(OperationKind::Route) =>
            {
                self.open_confirmation(Confirmation::Route)
            }
            Action::RequestCancelAndLeave
                if self.session.view == View::Deployment
                    && self.can_cancel() =>
            {
                self.open_confirmation(Confirmation::CancelAndLeave)
            }
            Action::RequestCancelAndLeave
                if self.session.view == View::Deployment
                    && self.can_force_stop() =>
            {
                self.open_confirmation(Confirmation::ForceStop)
            }
            Action::RequestCancelAndDestroy
                if self.session.view == View::Deployment
                    && (self.can_cancel()
                        || self.can_upgrade_cancel_to_destroy()) =>
            {
                self.open_confirmation(Confirmation::CancelAndDestroy)
            }
            Action::CycleLogFilter
                if self.session.view == View::Deployment
                    && self.session.deployment_pane == DeploymentPane::Logs
                    && self
                        .session
                        .deployment_expanded(DeploymentPane::Logs)
                    && !self.session.help_open
                    && !self.session.detail_open
                    && self.session.confirmation.is_none() =>
            {
                self.logs_filter = self.logs_filter.next();
                self.logs.scroll = 0;
            }
            Action::Scroll { delta, page: false } => {
                if !self.try_move_focused_content(delta) && delta != 0 {
                    self.move_item(if delta > 0 {
                        Direction::Next
                    } else {
                        Direction::Previous
                    });
                }
            }
            Action::Scroll { delta, page: true } => match self.session.view {
                View::Deployment
                    if self
                        .session
                        .deployment_expanded(self.session.deployment_pane) =>
                {
                    match self.session.deployment_pane {
                        DeploymentPane::OverallProgress
                        | DeploymentPane::Status => {}
                        DeploymentPane::Phases => {
                            let amount = crate::tui::ui::deployment::phase_content_height(self)
                                .max(1) as isize;
                            self.session.phase_scroll =
                                self.session.phase_scroll.saturating_add_signed(
                                    delta.saturating_mul(amount),
                                )
                        }
                        DeploymentPane::CurrentPhase => {
                            let amount = crate::tui::ui::deployment::subtask_content_height(self)
                                .max(1) as isize;
                            self.session.subtask_scroll = self
                                .session
                                .subtask_scroll
                                .saturating_add_signed(
                                    delta.saturating_mul(amount),
                                )
                        }
                        DeploymentPane::Logs => {
                            let visible =
                                crate::tui::ui::deployment::log_content_height(
                                    self,
                                );
                            let amount = visible.max(1) as isize;
                            self.logs.scroll(
                                delta.saturating_mul(amount).saturating_neg(),
                                visible,
                            );
                        }
                    }
                }
                View::Deployment => {}
                View::Monitor
                    if self.session.monitoring_pane
                        == MonitoringPane::Topology
                        && self
                            .session
                            .monitoring_expanded(MonitoringPane::Topology) =>
                {
                    let amount = crate::tui::ui::monitor::page_capacity(self)
                        .max(1) as isize;
                    let resources = self.resources();
                    let current =
                        self.session.selected_resource.as_ref().and_then(
                            |selected| {
                                resources.iter().position(|id| id == selected)
                            },
                        );
                    let index = current.map_or_else(
                        || {
                            if delta < 0 {
                                resources.len().saturating_sub(1)
                            } else {
                                0
                            }
                        },
                        |current| {
                            current
                                .saturating_add_signed(
                                    delta.saturating_mul(amount),
                                )
                                .min(resources.len().saturating_sub(1))
                        },
                    );
                    self.session.selected_resource =
                        resources.get(index).cloned();
                    self.session.monitor_scroll = index;
                }
                View::Monitor
                    if self.session.monitoring_pane
                        == MonitoringPane::TopZones
                        && self
                            .session
                            .monitoring_expanded(MonitoringPane::TopZones) =>
                {
                    let amount =
                        crate::tui::ui::monitor::top_zones_page_capacity(self)
                            as isize;
                    if amount > 0 {
                        let maximum = crate::tui::ui::monitor::top_zones_len(
                            self,
                            self.session.selected_rack,
                        )
                        .saturating_sub(amount as usize);
                        let current =
                            self.session.top_zones_scroll.min(maximum);
                        self.session.top_zones_scroll = current
                            .saturating_add_signed(delta.saturating_mul(amount))
                            .min(maximum);
                    }
                }
                View::Monitor => {}
            },
            _ => {}
        }
        vec![]
    }
    pub(crate) fn can_start(&self, kind: OperationKind) -> bool {
        !self.session.quitting
            && self.operation.active.is_none()
            && self.operation.pending.is_none()
            && match kind {
                OperationKind::Launch => {
                    self.deployment.observed == ObservedDeploymentState::Stopped
                }
                OperationKind::Route => matches!(
                    self.deployment.observed,
                    ObservedDeploymentState::Running
                        | ObservedDeploymentState::Degraded
                ),
                OperationKind::Destroy => {
                    self.deployment.observed != ObservedDeploymentState::Stopped
                }
            }
    }
    pub(crate) fn resources_may_exist(&self) -> bool {
        self.deployment.observed != ObservedDeploymentState::Stopped
            || self.operation.pending.is_some()
            || self.operation.active.is_some()
    }
    pub(crate) fn can_cancel(&self) -> bool {
        self.operation.active.as_ref().is_some_and(|active| !active.cancelling)
    }
    fn can_upgrade_cancel_to_destroy(&self) -> bool {
        self.operation.active.as_ref().is_some_and(|active| {
            active.cancelling
                && active.cancel_choice == Some(CancelChoice::Leave)
        })
    }
    fn can_force_stop(&self) -> bool {
        self.operation
            .active
            .as_ref()
            .is_some_and(|active| !active.force_stop_requested)
    }
    fn open_confirmation(&mut self, confirmation: Confirmation) {
        self.clipboard_copied = false;
        self.session.confirmation_selection =
            confirmation.options(self.can_cancel()).len().saturating_sub(1);
        self.session.confirmation = Some(confirmation);
    }
    fn activate_confirmation_option(
        &mut self,
        confirmation: Confirmation,
    ) -> Vec<Effect> {
        let action = confirmation
            .options(self.can_cancel())
            .get(self.session.confirmation_selection)
            .map(|option| option.action)
            .unwrap_or(ConfirmationOptionAction::Reject);
        match action {
            ConfirmationOptionAction::Confirm => {
                self.session.confirmation = None;
                self.confirm(confirmation)
            }
            ConfirmationOptionAction::Reject => {
                self.session.confirmation = None;
                vec![]
            }
        }
    }
    fn confirm(&mut self, c: Confirmation) -> Vec<Effect> {
        match c {
            Confirmation::Launch if self.can_start(OperationKind::Launch) => {
                self.start(OperationKind::Launch)
            }
            Confirmation::Route if self.can_start(OperationKind::Route) => {
                self.start(OperationKind::Route)
            }
            Confirmation::Detach
                if !self.session.quitting && self.can_detach() =>
            {
                self.detach()
            }
            Confirmation::Quit
                if !self.session.quitting && !self.resources_may_exist() =>
            {
                self.session.exit = Some(SessionExit::Quit);
                self.session.quitting = true;
                vec![Effect::Quit]
            }
            Confirmation::QuitAndDestroy
                if !self.session.quitting && self.resources_may_exist() =>
            {
                self.destroy_and_quit()
            }
            Confirmation::CancelAndLeave if self.can_cancel() => {
                vec![self.cancel(CancelChoice::Leave)]
            }
            Confirmation::CancelAndDestroy
                if self.can_cancel()
                    || self.can_upgrade_cancel_to_destroy() =>
            {
                vec![self.cancel(CancelChoice::Destroy)]
            }
            Confirmation::ForceStop if self.can_force_stop() => {
                let active = self.operation.active.as_mut().unwrap();
                active.force_stop_requested = true;
                let request_id = active.request_id;
                vec![Effect::ForceStop { request_id }]
            }
            _ => vec![],
        }
    }
    fn detach(&mut self) -> Vec<Effect> {
        if !self.can_detach() {
            return vec![];
        }
        if let Some(active) = self.operation.active.as_ref() {
            self.session.post_operation_exit = Some(PostOperationExit::Detach);
            if active.cancelling {
                return vec![];
            }
            return vec![self.cancel(CancelChoice::Leave)];
        }
        if self.operation.pending.is_some() {
            self.session.post_operation_exit = Some(PostOperationExit::Detach);
            return vec![];
        }
        self.session.exit = Some(SessionExit::Detach);
        self.session.quitting = true;
        vec![Effect::Quit]
    }
    fn can_detach(&self) -> bool {
        self.reattach_command.is_some()
            && self.session.post_operation_exit.is_none()
            && !self.operation.active.as_ref().is_some_and(|active| {
                active.kind == OperationKind::Destroy
                    || active.cancel_choice == Some(CancelChoice::Destroy)
            })
            && !self
                .operation
                .pending
                .as_ref()
                .is_some_and(|pending| pending.kind == OperationKind::Destroy)
    }
    fn destroy_and_quit(&mut self) -> Vec<Effect> {
        self.session.post_operation_exit = Some(PostOperationExit::Destroy);
        if let Some(active) = self.operation.active.as_ref() {
            if active.kind == OperationKind::Destroy
                || active.cancel_choice == Some(CancelChoice::Destroy)
            {
                return vec![];
            }
            return vec![self.cancel(CancelChoice::Destroy)];
        }
        if self.operation.pending.is_some() {
            return vec![];
        }
        let effects = self.start(OperationKind::Destroy);
        if effects.is_empty() {
            self.session.post_operation_exit = None;
        }
        effects
    }
    fn racks(&self) -> Vec<RackId> {
        self.deployment
            .topology
            .iter()
            .filter_map(|d| d.rack)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    fn resources(&self) -> Vec<ResourceId> {
        let descriptors = self
            .deployment
            .topology
            .iter()
            .filter(|d| {
                self.session.view != View::Monitor
                    || d.rack == self.session.selected_rack
                    || d.kind == crate::tui::telemetry::ResourceKind::Router
            })
            .cloned()
            .collect::<Vec<_>>();
        if self.session.view == View::Monitor {
            crate::tui::ui::topology::semantic_order(&descriptors)
        } else {
            descriptors.into_iter().map(|descriptor| descriptor.id).collect()
        }
    }
    fn moved<T: Clone + Eq>(
        items: &[T],
        current: Option<&T>,
        direction: Direction,
    ) -> Option<T> {
        if items.is_empty() {
            return None;
        }
        let Some(index) =
            current.and_then(|c| items.iter().position(|v| v == c))
        else {
            return Some(match direction {
                Direction::Next => items[0].clone(),
                Direction::Previous => items[items.len() - 1].clone(),
            });
        };
        Some(
            items[match direction {
                Direction::Next => (index + 1) % items.len(),
                Direction::Previous => (index + items.len() - 1) % items.len(),
            }]
            .clone(),
        )
    }
    fn move_rack(&mut self, direction: Direction) {
        self.session.selected_rack = Self::moved(
            &self.racks(),
            self.session.selected_rack.as_ref(),
            direction,
        );
        self.session.monitor_scroll = 0;
        self.session.top_zones_scroll = 0;
        self.repair_resource();
    }
    fn move_resource(&mut self, direction: Direction) {
        self.session.selected_resource = Self::moved(
            &self.resources(),
            self.session.selected_resource.as_ref(),
            direction,
        );
    }
    fn try_move_focused_content(&mut self, delta: isize) -> bool {
        if delta == 0 {
            return false;
        }
        match self.session.view {
            View::Deployment
                if self
                    .session
                    .deployment_expanded(self.session.deployment_pane) =>
            {
                match self.session.deployment_pane {
                    DeploymentPane::Phases => {
                        let Some(operation) = self.operation.active.as_ref()
                        else {
                            return false;
                        };
                        let phases = crate::tui::ui::deployment::phase_order(
                            operation.kind,
                        );
                        if phases.is_empty() {
                            return false;
                        }
                        let visible =
                            crate::tui::ui::deployment::phase_content_height(
                                self,
                            );
                        let active = operation.phase.and_then(|phase| {
                            phases.iter().position(|item| *item == phase)
                        });
                        let current =
                            crate::tui::ui::deployment::phase_window_start(
                                self.session.phase_scroll,
                                visible,
                                phases.len(),
                                active,
                            );
                        let candidate =
                            crate::tui::ui::deployment::phase_window_start(
                                current.saturating_add_signed(delta),
                                visible,
                                phases.len(),
                                active,
                            );
                        self.session.phase_scroll = candidate;
                        candidate != current
                    }
                    DeploymentPane::CurrentPhase => false,
                    DeploymentPane::Logs => {
                        let visible =
                            crate::tui::ui::deployment::log_content_height(
                                self,
                            );
                        let len = crate::tui::ui::logs::filtered_len(self);
                        if len == 0 {
                            return false;
                        }
                        let current = crate::tui::ui::logs::effective_scroll(
                            self.logs.scroll,
                            len,
                            visible,
                        );
                        let candidate = crate::tui::ui::logs::effective_scroll(
                            current
                                .saturating_add_signed(delta.saturating_neg()),
                            len,
                            visible,
                        );
                        self.logs.scroll = candidate;
                        candidate != current
                    }
                    DeploymentPane::OverallProgress
                    | DeploymentPane::Status => false,
                }
            }
            View::Monitor
                if self.session.monitoring_pane == MonitoringPane::Topology
                    && self
                        .session
                        .monitoring_expanded(MonitoringPane::Topology) =>
            {
                let resources = self.resources();
                if resources.is_empty() {
                    return false;
                }
                let current = self.session.selected_resource.as_ref().and_then(
                    |selected| resources.iter().position(|id| id == selected),
                );
                let candidate = current.map_or_else(
                    || if delta < 0 { resources.len() - 1 } else { 0 },
                    |index| {
                        index
                            .saturating_add_signed(delta)
                            .min(resources.len().saturating_sub(1))
                    },
                );
                self.session.selected_resource =
                    resources.get(candidate).cloned();
                self.session.monitor_scroll = candidate;
                current != Some(candidate)
            }
            View::Monitor
                if self.session.monitoring_pane == MonitoringPane::TopZones
                    && self
                        .session
                        .monitoring_expanded(MonitoringPane::TopZones) =>
            {
                let capacity =
                    crate::tui::ui::monitor::top_zones_page_capacity(self);
                if capacity == 0 {
                    return false;
                }
                let maximum = crate::tui::ui::monitor::top_zones_len(
                    self,
                    self.session.selected_rack,
                )
                .saturating_sub(capacity);
                let current = self.session.top_zones_scroll.min(maximum);
                let candidate =
                    current.saturating_add_signed(delta).min(maximum);
                self.session.top_zones_scroll = candidate;
                candidate != current
            }
            _ => false,
        }
    }
    fn move_item(&mut self, direction: Direction) {
        match self.session.view {
            View::Deployment => {
                self.session.deployment_pane = Self::moved(
                    &DeploymentPane::ORDER,
                    Some(&self.session.deployment_pane),
                    direction,
                )
                .unwrap();
            }
            View::Monitor => {
                self.session.monitoring_pane = Self::moved(
                    &MonitoringPane::ORDER,
                    Some(&self.session.monitoring_pane),
                    direction,
                )
                .unwrap();
            }
        }
    }
    fn repair_selection(&mut self) {
        let racks = self.racks();
        if self
            .session
            .selected_rack
            .as_ref()
            .is_none_or(|r| !racks.contains(r))
        {
            self.session.selected_rack = racks.first().copied();
        }
        self.repair_resource();
    }
    fn repair_resource(&mut self) {
        let resources = self.resources();
        if self
            .session
            .selected_resource
            .as_ref()
            .is_some_and(|r| !resources.contains(r))
        {
            self.session.selected_resource = resources.first().cloned();
        }
        if resources.is_empty() {
            self.session.selected_resource = None;
            self.session.detail_open = false;
        }
    }
    fn filtered_log_count(&self) -> usize {
        self.logs
            .entries
            .iter()
            .filter(|entry| self.logs_filter.accepts(entry.level))
            .count()
    }

    fn clamp_log_scroll(&mut self) {
        let visible = crate::tui::ui::deployment::log_content_height(self);
        let maximum = self.filtered_log_count().saturating_sub(visible);
        self.logs.scroll = self.logs.scroll.min(maximum);
    }
    fn operation_event(
        &mut self,
        request_id: OperationRequestId,
        event: OperationEvent,
    ) -> Vec<Effect> {
        let mut effects = vec![];
        // Stateful events are correlated to pending/active requests below. Logs remain an
        // intentionally inclusive display stream, including delayed entries from old requests.
        match event {
            OperationEvent::Started { kind } => {
                if self.operation.active.is_none()
                    && self.operation.pending.as_ref().is_some_and(|pending| {
                        pending.request_id == request_id && pending.kind == kind
                    })
                {
                    self.operation.pending = None;
                    self.operation.active =
                        Some(ActiveOperation::new(request_id, kind));
                    self.operation.outcome = None;
                    let deferred = match self.session.post_operation_exit {
                        Some(PostOperationExit::Detach) => {
                            Some(CancelChoice::Leave)
                        }
                        Some(PostOperationExit::Destroy)
                            if kind != OperationKind::Destroy =>
                        {
                            Some(CancelChoice::Destroy)
                        }
                        _ => None,
                    };
                    if let Some(choice) = deferred {
                        effects.push(self.cancel(choice));
                    }
                }
            }
            OperationEvent::PhaseStarted { phase } => {
                if let Some(active) = self.matching_active_mut(request_id) {
                    active.phase = Some(phase);
                }
            }
            OperationEvent::Log { level, message } => self.logs.push_filtered(
                LogEntry::operation(level, message),
                self.logs_filter,
            ),
            OperationEvent::Warning(warning) => {
                if let Some(active) = self.matching_active_mut(request_id) {
                    if !active.warnings.contains(&warning) {
                        active.warnings.push(warning);
                    }
                }
            }
            OperationEvent::Finished(outcome) => {
                if self
                    .operation
                    .active
                    .as_ref()
                    .is_some_and(|active| active.request_id == request_id)
                {
                    let active = self.operation.active.take().unwrap();
                    let active_kind = active.kind;
                    let cancel_choice = active.cancel_choice;
                    self.operation.retained_warnings = active.warnings;
                    let should_start_destroy = active_kind
                        != OperationKind::Destroy
                        && (self.session.post_operation_exit
                            == Some(PostOperationExit::Destroy)
                            || (self.session.post_operation_exit.is_none()
                                && cancel_choice
                                    == Some(CancelChoice::Destroy)));
                    let should_quit = match self.session.post_operation_exit {
                        Some(PostOperationExit::Detach) => true,
                        Some(PostOperationExit::Destroy)
                            if active_kind == OperationKind::Destroy =>
                        {
                            command_succeeded(&outcome)
                        }
                        Some(PostOperationExit::Destroy) => false,
                        None => false,
                    };
                    let exit = match self.session.post_operation_exit {
                        Some(PostOperationExit::Detach) => SessionExit::Detach,
                        Some(PostOperationExit::Destroy) => SessionExit::Quit,
                        None => SessionExit::Quit,
                    };
                    self.operation.outcome = Some(outcome);
                    if should_start_destroy {
                        let destroy_effects =
                            self.start(OperationKind::Destroy);
                        if destroy_effects.is_empty() {
                            self.session.post_operation_exit = None;
                        }
                        effects.extend(destroy_effects);
                    } else {
                        self.session.post_operation_exit = None;
                        if should_quit {
                            self.session.exit = Some(exit);
                            self.session.quitting = true;
                            effects.push(Effect::Quit);
                        }
                    }
                }
            }
        }
        effects
    }

    fn start(&mut self, kind: OperationKind) -> Vec<Effect> {
        let Some(request_id) = self.next_operation_request_id else {
            self.operation.start_failure = Some(
                "operation request ID space exhausted; restart the application before retrying"
                    .into(),
            );
            return vec![];
        };
        self.next_operation_request_id = request_id.next();
        self.operation.pending = Some(PendingOperation::new(request_id, kind));
        self.operation.start_failure = None;
        vec![Effect::Start { request_id, kind }]
    }

    fn cancel(&mut self, choice: CancelChoice) -> Effect {
        let active = self.operation.active.as_mut().unwrap();
        active.cancelling = true;
        if active.cancel_choice != Some(CancelChoice::Destroy) {
            active.cancel_choice = Some(choice);
        }
        Effect::Cancel {
            request_id: active.request_id,
            choice: active.cancel_choice.expect("cancellation intent is set"),
        }
    }

    fn matching_active_mut(
        &mut self,
        request_id: OperationRequestId,
    ) -> Option<&mut ActiveOperation> {
        self.operation
            .active
            .as_mut()
            .filter(|active| active.request_id == request_id)
    }

    fn accept_attempt<T>(sample: &LatestSample<T>, at: Instant) -> bool {
        sample.last_attempt.is_none_or(|latest| at >= latest)
            && sample.good.as_ref().is_none_or(|good| at >= good.captured_at)
    }

    fn accept_traffic_generation(&mut self, at: Instant) -> bool {
        if self
            .observability
            .latest_traffic_generation
            .is_some_and(|latest| at < latest)
        {
            return false;
        }
        self.observability.latest_traffic_generation = Some(at);
        true
    }

    fn normalize_topology(
        mut topology: Vec<ResourceDescriptor>,
    ) -> Vec<ResourceDescriptor> {
        topology.sort_by(|a, b| {
            a.id.cmp(&b.id)
                .then_with(|| a.rack.cmp(&b.rack))
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.host.cmp(&b.host))
        });
        topology.dedup_by(|a, b| a.id == b.id);
        topology
    }
}

fn command_succeeded(outcome: &CommandOutcome) -> bool {
    matches!(outcome, CommandOutcome::Exited { status, .. } if status.success())
}

#[cfg(test)]
mod factual_outcome_tests {
    use super::*;

    fn active_app() -> App {
        let mut app = App::new(vec![], 8, 8);
        app.operation.active = Some(ActiveOperation::new(
            OperationRequestId::FIRST,
            OperationKind::Launch,
        ));
        app.next_operation_request_id = OperationRequestId::FIRST.next();
        app
    }

    fn failed_outcome() -> CommandOutcome {
        CommandOutcome::SpawnFailed {
            message: "cancelled child settled".into(),
        }
    }

    #[test]
    fn force_stop_is_separate_and_defaults_to_back() {
        let options = Confirmation::ForceStop.options(true);
        assert!(options[0].label.contains("Force stop"));
        assert_eq!(options[1].action, ConfirmationOptionAction::Reject);
        assert!(
            crate::tui::ui::confirm_dialog::prompt_text(
                Confirmation::ForceStop
            )
            .contains("Force stop")
        );
    }

    #[test]
    fn direct_cancel_destroy_starts_destroy_only_after_finished_and_stays_open()
    {
        let mut app = active_app();

        assert_eq!(
            app.update(AppEvent::Action(Action::RequestCancelAndDestroy)),
            vec![]
        );
        app.update(AppEvent::Action(Action::Scroll { delta: -1, page: false }));
        assert_eq!(
            app.update(AppEvent::Action(Action::Activate)),
            vec![Effect::Cancel {
                request_id: OperationRequestId::FIRST,
                choice: CancelChoice::Destroy,
            }]
        );
        assert_eq!(
            app.update(AppEvent::ReconciliationStarted { at: Instant::now() }),
            vec![]
        );
        assert!(app.operation.pending.is_none());

        let effects = app.update(AppEvent::Operation {
            request_id: OperationRequestId::FIRST,
            event: OperationEvent::Finished(failed_outcome()),
        });
        let destroy_request = OperationRequestId::FIRST.next().unwrap();
        assert_eq!(
            effects,
            vec![Effect::Start {
                request_id: destroy_request,
                kind: OperationKind::Destroy,
            }]
        );
        assert!(!app.session.quitting);
        assert_eq!(app.session.exit, None);

        app.update(AppEvent::Operation {
            request_id: destroy_request,
            event: OperationEvent::Started { kind: OperationKind::Destroy },
        });
        assert_eq!(
            app.update(AppEvent::Operation {
                request_id: destroy_request,
                event: OperationEvent::Finished(failed_outcome()),
            }),
            vec![]
        );
        assert!(!app.session.quitting);
        assert_eq!(app.session.exit, None);
    }

    #[test]
    fn cancellation_intent_only_escalates() {
        let mut app = active_app();
        app.cancel(CancelChoice::Leave);
        assert!(app.can_upgrade_cancel_to_destroy());
        assert_eq!(
            app.update(AppEvent::Action(Action::RequestCancelAndDestroy)),
            vec![]
        );
        app.update(AppEvent::Action(Action::Scroll { delta: -1, page: false }));
        assert_eq!(
            app.update(AppEvent::Action(Action::Activate)),
            vec![Effect::Cancel {
                request_id: OperationRequestId::FIRST,
                choice: CancelChoice::Destroy,
            }]
        );
        assert!(!app.can_upgrade_cancel_to_destroy());
        assert_eq!(
            app.cancel(CancelChoice::Leave),
            Effect::Cancel {
                request_id: OperationRequestId::FIRST,
                choice: CancelChoice::Destroy,
            }
        );

        assert_eq!(
            app.operation.active.unwrap().cancel_choice,
            Some(CancelChoice::Destroy)
        );
    }

    #[test]
    fn force_stop_can_be_requested_once_while_safe_waiting() {
        let mut app = active_app();
        app.cancel(CancelChoice::Leave);

        app.open_confirmation(Confirmation::ForceStop);
        app.update(AppEvent::Action(Action::Scroll { delta: -1, page: false }));
        assert_eq!(
            app.update(AppEvent::Action(Action::Activate)),
            vec![Effect::ForceStop { request_id: OperationRequestId::FIRST }]
        );
        app.open_confirmation(Confirmation::ForceStop);
    }
}
