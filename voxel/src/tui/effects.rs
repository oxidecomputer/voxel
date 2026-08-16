use std::{sync::Arc, time::Duration};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use super::{
    context::{PublicCommand, TuiContext},
    event::{AppEvent, Effect, OperationRequestId},
    logging::DurableLog,
    operation::{
        CommandOutcome, DestroyPhase, LaunchPhase, LogLevel, OperationEvent,
        OperationKind, OperationPhase, OperationWarning, OutputStream,
        RoutePhase,
    },
    phase::{PhaseClassifier, PhaseHint},
    process::{
        ChildResult, ChildSupervisor, DrainFailure, ForceStopResult, OutputLine,
    },
    reconcile::{self, LifecycleIntent},
};

const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(25);
const OUTPUT_CHANNEL_CAPACITY: usize = 256;
const PARTIAL_STATE_WARNING: &str = "ALARM: force stopping only the direct Voxel child; descendants and Falcon state may be partial";

struct ActiveChild {
    request_id: OperationRequestId,
    kind: OperationKind,
    child: ChildSupervisor,
    output: mpsc::Receiver<OutputLine>,
    output_done: bool,
    classifier: PhaseClassifier,
    forced: Option<ForceStopResult>,
    last_poll_error_log: Option<std::time::Instant>,
    _admission: OwnedSemaphorePermit,
}

/// Owns the sole lifecycle child. Runtime shutdown must keep this future alive
/// until it returns; dropping it uses `kill_on_drop` only as a failure fallback.
pub(crate) struct Effects {
    context: Arc<TuiContext>,
    durable: DurableLog,
    events: mpsc::Sender<AppEvent>,
    executor: Arc<super::collector::FalconExecutor>,
    gate: Arc<Semaphore>,
    shutdown: CancellationToken,
}

impl Effects {
    pub(crate) fn new(
        context: Arc<TuiContext>,
        durable: DurableLog,
        events: mpsc::Sender<AppEvent>,
        executor: Arc<super::collector::FalconExecutor>,
        gate: Arc<Semaphore>,
        shutdown: CancellationToken,
    ) -> Self {
        Self { context, durable, events, executor, gate, shutdown }
    }

    pub(crate) async fn run(
        mut self,
        mut effects: mpsc::UnboundedReceiver<Effect>,
    ) {
        let mut active: Option<ActiveChild> = None;
        let mut effects_open = true;
        let mut poll = tokio::time::interval(CHILD_POLL_INTERVAL);
        loop {
            tokio::select! {
                biased;
                line = async { active.as_mut().unwrap().output.recv().await }, if active.as_ref().is_some_and(|child| !child.output_done) => {
                    if let Some(line) = line {
                        self.output(active.as_mut().unwrap(), line).await;
                    } else {
                        active.as_mut().unwrap().output_done = true;
                    }
                }
                effect = effects.recv(), if effects_open => match effect {
                    Some(effect) => self.effect(effect, &mut active).await,
                    None if active.is_none() => break,
                    None => effects_open = false,
                },
                _ = poll.tick(), if active.is_some() => {
                    let settled = match active.as_mut().unwrap().child.is_settled() {
                        Ok(settled) => settled,
                        Err(error) => {
                            let child = active.as_mut().unwrap();
                            let now = std::time::Instant::now();
                            if child.last_poll_error_log.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5)) {
                                child.last_poll_error_log = Some(now);
                                self.operation(child.request_id, OperationEvent::Log {
                                    level: LogLevel::Error,
                                    message: format!("child_poll_error request_id={:?} error={:?}", child.request_id, error.message),
                                }).await;
                            }
                            false
                        },
                    };
                    if settled {
                        self.settle(active.take().unwrap()).await;
                        if !effects_open {
                            break;
                        }
                    }
                }
                _ = self.shutdown.cancelled(), if active.is_none() => break,
            }
        }
    }

    async fn effect(
        &mut self,
        effect: Effect,
        active: &mut Option<ActiveChild>,
    ) {
        match effect {
            Effect::Start { request_id, kind } => {
                if active.is_some() {
                    self.start_failed(
                        request_id,
                        kind,
                        "another lifecycle child is active",
                    )
                    .await;
                    return;
                }
                let admission = match self.gate.clone().acquire_owned().await {
                    Ok(admission) => admission,
                    Err(_) => return,
                };
                self.executor.drain_serial_tasks().await;
                let command = match kind {
                    OperationKind::Launch => PublicCommand::Launch,
                    OperationKind::Route => PublicCommand::Route,
                    OperationKind::Destroy => PublicCommand::Destroy,
                };
                let (tx, output) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
                match ChildSupervisor::spawn(
                    self.context.command_spec(command),
                    self.durable.clone(),
                    tx,
                ) {
                    Ok(child) => {
                        *active = Some(ActiveChild {
                            request_id,
                            kind,
                            child,
                            output,
                            output_done: false,
                            classifier: PhaseClassifier::default(),
                            forced: None,
                            last_poll_error_log: None,
                            _admission: admission,
                        });
                        self.operation(
                            request_id,
                            OperationEvent::Started { kind },
                        )
                        .await;
                    }
                    Err(error) => {
                        self.start_failed(request_id, kind, &error.to_string())
                            .await
                    }
                }
            }
            Effect::Cancel { request_id, .. } => {
                // Safe cancellation is intent only. In particular, do not signal
                // an opaque child that may own a Falcon serial operation.
                if active
                    .as_ref()
                    .is_some_and(|child| child.request_id == request_id)
                {
                    self.operation(request_id, OperationEvent::Log { level: LogLevel::Warning, message: "Cancellation pending; waiting for the Voxel command to settle safely".into() }).await;
                }
            }
            Effect::ForceStop { request_id } => {
                let Some(child) = active
                    .as_mut()
                    .filter(|child| child.request_id == request_id)
                else {
                    return;
                };
                if child.forced.is_some() {
                    return;
                }
                self.operation(
                    request_id,
                    OperationEvent::Warning(OperationWarning {
                        message: PARTIAL_STATE_WARNING.into(),
                        resource: None,
                    }),
                )
                .await;
                let result = child.child.force_stop().await;
                if let ForceStopResult::Failed(message) = &result {
                    self.operation(
                        request_id,
                        OperationEvent::Log {
                            level: LogLevel::Error,
                            message: format!(
                                "direct child force-stop attempt failed: {message}"
                            ),
                        },
                    )
                    .await;
                }
                child.forced = Some(result);
            }
            Effect::CopyToClipboard(_) | Effect::Quit => {}
        }
    }

    async fn output(&self, active: &mut ActiveChild, line: OutputLine) {
        self.operation(
            active.request_id,
            OperationEvent::Log {
                level: if line.stream == OutputStream::Stderr {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                },
                message: line.text.clone(),
            },
        )
        .await;
        if let Some(PhaseHint::Started(phase)) =
            active.classifier.classify(active.kind, line.stream, &line.text)
        {
            self.operation(
                active.request_id,
                OperationEvent::PhaseStarted { phase },
            )
            .await;
        }
    }

    async fn settle(&self, active: ActiveChild) {
        let request_id = active.request_id;
        let kind = active.kind;
        let forced = active.forced.clone();
        let result = active.child.wait().await;
        let outcome = match (result, forced) {
            (Ok(result), forced) => {
                for (stream, error) in &result.output_errors {
                    match error {
                        DrainFailure::Output(message) => {
                            self.operation(
                                request_id,
                                OperationEvent::Log {
                                    level: LogLevel::Error,
                                    message: format!(
                                        "{stream:?} drain failed: {message}"
                                    ),
                                },
                            )
                            .await;
                        }
                        DrainFailure::DurableLog(message) => {
                            let _ = self
                                .events
                                .send(AppEvent::DurableLogFailed {
                                    message: message.clone(),
                                })
                                .await;
                        }
                    }
                }
                match forced {
                    Some(kill) => force_outcome(result, kill),
                    None => child_outcome(result),
                }
            }
            (Err(error), Some(kill)) => CommandOutcome::ForceStopped {
                status: None,
                kill_error: match kill {
                    ForceStopResult::Failed(message) => Some(message),
                    _ => Some(error.message),
                },
            },
            (Err(error), None) => CommandOutcome::SpawnFailed {
                message: format!("child wait failed: {}", error.message),
            },
        };
        self.operation(
            request_id,
            OperationEvent::PhaseStarted { phase: reconciliation_phase(kind) },
        )
        .await;
        self.reconcile(kind).await;
        self.operation(request_id, OperationEvent::Finished(outcome)).await;
    }

    async fn reconcile(&self, kind: OperationKind) {
        let at = std::time::Instant::now();
        let _ = self.events.send(AppEvent::ReconciliationStarted { at }).await;
        let intent = match kind {
            OperationKind::Launch => LifecycleIntent::Launch,
            OperationKind::Destroy => LifecycleIntent::Destroy,
            _ => LifecycleIntent::Idle,
        };
        self.executor.drain_serial_tasks().await;
        match reconcile::collect(
            &self.context,
            &self.executor,
            intent,
            &self.shutdown,
        )
        .await
        {
            Ok(evidence) => {
                let routes = evidence.routes;
                let result = reconcile::reduce(&evidence);
                let _ = self
                    .events
                    .send(AppEvent::Reconciled {
                        at: std::time::Instant::now(),
                        result,
                        routes,
                    })
                    .await;
            }
            Err(error) => {
                let _ = self
                    .events
                    .send(AppEvent::ReconciliationFailed {
                        at: std::time::Instant::now(),
                        message: error.to_string(),
                    })
                    .await;
            }
        }
    }

    async fn operation(
        &self,
        request_id: OperationRequestId,
        event: OperationEvent,
    ) {
        let _ =
            self.events.send(AppEvent::Operation { request_id, event }).await;
    }

    async fn start_failed(
        &self,
        request_id: OperationRequestId,
        kind: OperationKind,
        message: &str,
    ) {
        let _ = self
            .events
            .send(AppEvent::OperationStartFailed {
                request_id,
                kind,
                message: message.into(),
            })
            .await;
    }
}

fn child_outcome(result: ChildResult) -> CommandOutcome {
    CommandOutcome::Exited {
        status: result.status,
        stderr_summary: result.stderr_summary,
    }
}

fn reconciliation_phase(kind: OperationKind) -> OperationPhase {
    match kind {
        OperationKind::Launch => OperationPhase::Launch(LaunchPhase::Reconcile),
        OperationKind::Destroy => {
            OperationPhase::Destroy(DestroyPhase::Reconcile)
        }
        OperationKind::Route => OperationPhase::Route(RoutePhase::Reconcile),
    }
}

fn force_outcome(result: ChildResult, kill: ForceStopResult) -> CommandOutcome {
    if matches!(
        kill,
        ForceStopResult::AlreadyExited | ForceStopResult::Failed(_)
    ) {
        return child_outcome(result);
    }
    CommandOutcome::ForceStopped {
        status: Some(result.status),
        kill_error: match kill {
            ForceStopResult::Failed(message) => Some(message),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::operation::{
        DestroyPhase, LaunchPhase, OperationPhase, RoutePhase,
    };

    #[test]
    fn every_operation_has_a_tui_owned_reconciliation_phase() {
        assert_eq!(
            reconciliation_phase(OperationKind::Launch),
            OperationPhase::Launch(LaunchPhase::Reconcile)
        );
        assert_eq!(
            reconciliation_phase(OperationKind::Destroy),
            OperationPhase::Destroy(DestroyPhase::Reconcile)
        );
        assert_eq!(
            reconciliation_phase(OperationKind::Route),
            OperationPhase::Route(RoutePhase::Reconcile)
        );
    }
}
