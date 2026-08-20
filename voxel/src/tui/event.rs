use std::time::Instant;

use crate::tui::operation::{OperationEvent, OperationKind};
use crate::tui::reconcile::{
    ReconciliationResult, RouteEvidence, RssObservation,
};
use crate::tui::telemetry::{
    HealthDiagnostic, NodeAddresses, OximeterExceptions, RackId, ResourceId,
    TrafficSample, ZfsHeadroom, ZoneCpu,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum View {
    Deployment,
    Monitor,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DeploymentPane {
    OverallProgress,
    Phases,
    Status,
    CurrentPhase,
    Logs,
}

impl DeploymentPane {
    pub(crate) const ORDER: [Self; 5] = [
        Self::OverallProgress,
        Self::Phases,
        Self::Status,
        Self::CurrentPhase,
        Self::Logs,
    ];
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MonitoringPane {
    RackSummary,
    Topology,
    TopZones,
}

impl MonitoringPane {
    pub(crate) const ORDER: [Self; 3] =
        [Self::RackSummary, Self::Topology, Self::TopZones];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Next,
    Previous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Confirmation {
    Launch,
    Route,
    Detach,
    Quit,
    QuitAndDestroy,
    CancelAndLeave,
    CancelAndDestroy,
    ForceStop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmationOptionAction {
    Confirm,
    Reject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfirmationOption {
    pub label: &'static str,
    pub action: ConfirmationOptionAction,
}

use ConfirmationOptionAction::*;
const LAUNCH_OPTIONS: &[ConfirmationOption] = &[
    ConfirmationOption { label: "Launch deployment", action: Confirm },
    ConfirmationOption { label: "Cancel", action: Reject },
];
const ROUTE_OPTIONS: &[ConfirmationOption] = &[
    ConfirmationOption { label: "Apply routes", action: Confirm },
    ConfirmationOption { label: "Cancel", action: Reject },
];
const DETACH_OPTIONS: &[ConfirmationOption] = &[
    ConfirmationOption { label: "Detach and leave resources", action: Confirm },
    ConfirmationOption { label: "Back", action: Reject },
];
const QUIT_OPTIONS: &[ConfirmationOption] = &[
    ConfirmationOption { label: "Quit Voxel TUI", action: Confirm },
    ConfirmationOption { label: "Back", action: Reject },
];
const QUIT_AND_DESTROY_OPTIONS: &[ConfirmationOption] = &[
    ConfirmationOption {
        label: "Destroy deployment and quit",
        action: Confirm,
    },
    ConfirmationOption { label: "Back", action: Reject },
];
const LEAVE_OPTIONS: &[ConfirmationOption] = &[
    ConfirmationOption { label: "Cancel and leave resources", action: Confirm },
    ConfirmationOption { label: "Back", action: Reject },
];
const DESTROY_CANCEL_OPTIONS: &[ConfirmationOption] = &[
    ConfirmationOption {
        label: "Cancel and destroy resources",
        action: Confirm,
    },
    ConfirmationOption { label: "Back", action: Reject },
];
const FORCE_STOP_OPTIONS: &[ConfirmationOption] = &[
    ConfirmationOption { label: "Force stop direct child", action: Confirm },
    ConfirmationOption { label: "Back", action: Reject },
];
impl Confirmation {
    pub fn options(&self, _can_cancel: bool) -> &'static [ConfirmationOption] {
        match self {
            Self::Launch => LAUNCH_OPTIONS,
            Self::Route => ROUTE_OPTIONS,
            Self::Detach => DETACH_OPTIONS,
            Self::Quit => QUIT_OPTIONS,
            Self::QuitAndDestroy => QUIT_AND_DESTROY_OPTIONS,
            Self::CancelAndLeave => LEAVE_OPTIONS,
            Self::CancelAndDestroy => DESTROY_CANCEL_OPTIONS,
            Self::ForceStop => FORCE_STOP_OPTIONS,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelChoice {
    Leave,
    Destroy,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OperationRequestId(u64);

impl OperationRequestId {
    pub(crate) const FIRST: Self = Self(1);

    pub(crate) fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    Start { request_id: OperationRequestId, kind: OperationKind },
    Cancel { request_id: OperationRequestId, choice: CancelChoice },
    ForceStop { request_id: OperationRequestId },
    CopyToClipboard(String),
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    SwitchView(View),
    NextRack,
    PreviousRack,
    NextItem,
    PreviousItem,
    Activate,
    ToggleSection,
    ToggleHelp,
    ToggleExternalMonitoring,
    CopyExternalMonitoringSelected,
    CopyExternalMonitoringAll,
    CopyExternalMonitoringGuide,
    Close,
    RequestLaunch,
    RequestRoute,
    RequestDetach,
    RequestQuit,
    RequestCancelAndLeave,
    RequestCancelAndDestroy,
    CopyReattachCommand,
    Reject,
    CycleLogFilter,
    Scroll { delta: isize, page: bool },
}

#[derive(Clone, Debug)]
pub enum AppEvent {
    Action(Action),
    Tick {
        now: Instant,
    },
    Resize {
        width: u16,
        height: u16,
    },
    ReconciliationStarted {
        at: Instant,
    },
    Reconciled {
        at: Instant,
        result: ReconciliationResult,
        routes: RouteEvidence,
    },
    ReconciliationFailed {
        at: Instant,
        message: String,
    },
    Operation {
        request_id: OperationRequestId,
        event: OperationEvent,
    },
    OperationStartFailed {
        request_id: OperationRequestId,
        kind: OperationKind,
        message: String,
    },
    Traffic {
        id: ResourceId,
        at: Instant,
        sample: TrafficSample,
    },
    OximeterTraffic {
        id: ResourceId,
        at: Instant,
        samples: Vec<(Instant, TrafficSample)>,
    },
    OximeterTrafficFailed {
        rack: RackId,
        at: Instant,
        message: String,
    },
    TrafficFailed {
        id: ResourceId,
        at: Instant,
        message: String,
    },
    Health {
        id: ResourceId,
        at: Instant,
        diagnostic: HealthDiagnostic,
    },
    HealthFailed {
        id: ResourceId,
        at: Instant,
        message: String,
    },
    ZoneCpu {
        rack: RackId,
        at: Instant,
        zones: Vec<ZoneCpu>,
    },
    ZoneCpuFailed {
        rack: RackId,
        at: Instant,
        message: String,
    },
    ZfsHeadroom {
        rack: RackId,
        at: Instant,
        pools: Vec<ZfsHeadroom>,
    },
    ZfsHeadroomFailed {
        rack: RackId,
        at: Instant,
        message: String,
    },
    OximeterExceptions {
        rack: RackId,
        at: Instant,
        exceptions: OximeterExceptions,
    },
    OximeterExceptionsFailed {
        rack: RackId,
        at: Instant,
        message: String,
    },
    Addresses {
        id: ResourceId,
        at: Instant,
        addresses: NodeAddresses,
    },
    AddressesFailed {
        id: ResourceId,
        at: Instant,
        message: String,
    },
    Rss {
        rack: RackId,
        at: Instant,
        observation: RssObservation,
    },
    RssFailed {
        rack: RackId,
        at: Instant,
        message: String,
    },
    DurableLogFailed {
        message: String,
    },
}

impl From<Action> for AppEvent {
    fn from(value: Action) -> Self {
        Self::Action(value)
    }
}
