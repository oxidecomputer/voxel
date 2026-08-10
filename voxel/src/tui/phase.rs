use super::operation::{
    DestroyPhase, LaunchPhase, OperationKind, OperationPhase, OutputStream,
    RoutePhase,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PhaseHint {
    Started(OperationPhase),
}

#[derive(Default)]
pub(crate) struct PhaseClassifier {
    launch: Option<usize>,
    destroy: Option<usize>,
    route: Option<usize>,
}

impl PhaseClassifier {
    pub(crate) fn classify(
        &mut self,
        kind: OperationKind,
        _stream: OutputStream,
        line: &str,
    ) -> Option<PhaseHint> {
        let lower = line.to_ascii_lowercase();
        let (phase, ordinal, last) = match kind {
            OperationKind::Launch => {
                let phase = launch_phase(&lower)?;
                (
                    OperationPhase::Launch(phase),
                    phase.ordinal(),
                    &mut self.launch,
                )
            }
            OperationKind::Destroy => {
                let phase = destroy_phase(&lower)?;
                let ordinal = DestroyPhase::ORDER
                    .iter()
                    .position(|item| *item == phase)
                    .unwrap();
                (OperationPhase::Destroy(phase), ordinal, &mut self.destroy)
            }
            OperationKind::Route => {
                let phase = route_phase(&lower)?;
                let ordinal = RoutePhase::ORDER
                    .iter()
                    .position(|item| *item == phase)
                    .unwrap();
                (OperationPhase::Route(phase), ordinal, &mut self.route)
            }
        };
        if last.is_some_and(|previous| ordinal <= previous) {
            return None;
        }
        *last = Some(ordinal);
        Some(PhaseHint::Started(phase))
    }
}

fn launch_phase(line: &str) -> Option<LaunchPhase> {
    if line.contains(" external route:") {
        Some(LaunchPhase::Route)
    } else if line.contains(": watching rss progress on the rss node ...") {
        Some(LaunchPhase::RackSetup)
    } else if line.contains(": launch start") {
        Some(LaunchPhase::Initialize)
    } else {
        None
    }
}

fn destroy_phase(line: &str) -> Option<DestroyPhase> {
    if line.contains("orphaned propolis") {
        Some(DestroyPhase::OrphanCleanup)
    } else {
        None
    }
}

fn route_phase(line: &str) -> Option<RoutePhase> {
    if line.contains("external route") { Some(RoutePhase::Apply) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::operation::{
        LaunchPhase, OperationKind, OperationPhase, OutputStream,
    };

    #[test]
    fn known_current_main_launch_output_advances_monotonically() {
        let mut classifier = PhaseClassifier::default();
        let fixtures = [
            ("gimlet-0: launch start", LaunchPhase::Initialize),
            (
                "rack-init: watching RSS progress on the RSS node ...",
                LaunchPhase::RackSetup,
            ),
            ("rack external route: timed out", LaunchPhase::Route),
        ];
        for (line, phase) in fixtures {
            assert_eq!(
                classifier.classify(
                    OperationKind::Launch,
                    OutputStream::Stdout,
                    line
                ),
                Some(PhaseHint::Started(OperationPhase::Launch(phase)))
            );
        }
        assert_eq!(
            classifier.classify(
                OperationKind::Launch,
                OutputStream::Stdout,
                "gimlet-0: launch start"
            ),
            None
        );
        assert_eq!(
            classifier.classify(
                OperationKind::Launch,
                OutputStream::Stdout,
                "unrecognized output"
            ),
            None
        );
        assert_eq!(
            classifier.classify(
                OperationKind::Launch,
                OutputStream::Stdout,
                "reconcile complete"
            ),
            None
        );
    }

    #[test]
    fn invented_launch_messages_remain_unclassified() {
        for line in
            ["launching topology", "staging node disks", "booting nodes"]
        {
            assert_eq!(
                PhaseClassifier::default().classify(
                    OperationKind::Launch,
                    OutputStream::Stdout,
                    line,
                ),
                None
            );
        }
    }

    #[test]
    fn route_and_destroy_fixtures_are_optional_and_monotonic() {
        let mut classifier = PhaseClassifier::default();
        assert_eq!(
            classifier.classify(
                OperationKind::Route,
                OutputStream::Stdout,
                "external route set",
            ),
            Some(PhaseHint::Started(OperationPhase::Route(
                crate::tui::operation::RoutePhase::Apply
            )))
        );
        assert_eq!(
            classifier.classify(
                OperationKind::Route,
                OutputStream::Stdout,
                "validating route",
            ),
            None
        );
        let mut classifier = PhaseClassifier::default();
        assert_eq!(
            classifier.classify(
                OperationKind::Destroy,
                OutputStream::Stdout,
                "orphaned propolis processes",
            ),
            Some(PhaseHint::Started(OperationPhase::Destroy(
                crate::tui::operation::DestroyPhase::OrphanCleanup
            )))
        );
        assert_eq!(
            classifier.classify(
                OperationKind::Destroy,
                OutputStream::Stdout,
                "destroy complete",
            ),
            None
        );
    }
}
