use std::process::ExitStatus;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationKind {
    Launch,
    Destroy,
    Route,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaunchPhase {
    Preflight,
    Stage,
    Boot,
    Initialize,
    RackSetup,
    Route,
    Reconcile,
}

impl LaunchPhase {
    pub(crate) const ORDER: [Self; 7] = [
        Self::Preflight,
        Self::Stage,
        Self::Boot,
        Self::Initialize,
        Self::RackSetup,
        Self::Route,
        Self::Reconcile,
    ];

    pub(crate) fn ordinal(self) -> usize {
        Self::ORDER.iter().position(|phase| *phase == self).unwrap()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DestroyPhase {
    OrphanCleanup,
    FalconTeardown,
    StorageCleanup,
    Reconcile,
}

impl DestroyPhase {
    pub(crate) const ORDER: [Self; 4] = [
        Self::OrphanCleanup,
        Self::FalconTeardown,
        Self::StorageCleanup,
        Self::Reconcile,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RoutePhase {
    Validate,
    Apply,
    Reconcile,
}

impl RoutePhase {
    pub(crate) const ORDER: [Self; 3] =
        [Self::Validate, Self::Apply, Self::Reconcile];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationPhase {
    Launch(LaunchPhase),
    Destroy(DestroyPhase),
    Route(RoutePhase),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OperationWarning {
    pub(crate) message: String,
    pub(crate) resource: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OperationEvent {
    Started { kind: OperationKind },
    PhaseStarted { phase: OperationPhase },
    Log { level: LogLevel, message: String },
    Warning(OperationWarning),
    Finished(CommandOutcome),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CommandOutcome {
    Exited { status: ExitStatus, stderr_summary: Vec<String> },
    SpawnFailed { message: String },
    ForceStopped { status: Option<ExitStatus>, kill_error: Option<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_orders_preserve_static_display_sequence() {
        assert_eq!(LaunchPhase::ORDER[0], LaunchPhase::Preflight);
        assert_eq!(DestroyPhase::ORDER[0], DestroyPhase::OrphanCleanup);
        assert_eq!(
            RoutePhase::ORDER,
            [RoutePhase::Validate, RoutePhase::Apply, RoutePhase::Reconcile]
        );
    }
}
