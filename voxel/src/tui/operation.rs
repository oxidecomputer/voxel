// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Lifecycle operation vocabulary: the launch, destroy and route operations,
//! their ordered phases, and the outcomes and log levels a child's output is
//! classified into.

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
