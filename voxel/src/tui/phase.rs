// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Infers lifecycle operation phases from child output lines, so the
//! deployment view can show progress through launch, destroy and route.

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
    if line.contains("external route set:") || line.contains(" external route:")
    {
        Some(LaunchPhase::Route)
    } else if line.contains(": watching rss progress on the rss node ...") {
        Some(LaunchPhase::RackSetup)
    } else if line.contains(": launch start") {
        Some(LaunchPhase::Initialize)
    } else if line.contains("creating nodes") {
        Some(LaunchPhase::Boot)
    } else if line.contains("creating links") {
        Some(LaunchPhase::Stage)
    } else if line.contains("starting preflight for deployment") {
        Some(LaunchPhase::Preflight)
    } else {
        None
    }
}

fn destroy_phase(line: &str) -> Option<DestroyPhase> {
    if line.contains("orphaned propolis") {
        Some(DestroyPhase::OrphanCleanup)
    } else if line.contains("destroying deployment") {
        Some(DestroyPhase::FalconTeardown)
    } else if line.contains("destroying images") {
        Some(DestroyPhase::StorageCleanup)
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

    fn classify(
        classifier: &mut PhaseClassifier,
        kind: OperationKind,
        line: &str,
    ) -> Option<OperationPhase> {
        classifier
            .classify(kind, OutputStream::Stdout, line)
            .map(|PhaseHint::Started(phase)| phase)
    }

    #[test]
    fn known_current_main_launch_output_advances_monotonically() {
        let mut classifier = PhaseClassifier::default();
        let fixtures = [
            (
                "starting preflight for deployment vtui_demo2",
                LaunchPhase::Preflight,
            ),
            ("creating links", LaunchPhase::Stage),
            ("creating nodes", LaunchPhase::Boot),
            ("g0: launch start", LaunchPhase::Initialize),
            (
                "rack-init: watching RSS progress on the RSS node ...",
                LaunchPhase::RackSetup,
            ),
            (
                "external route set: 198.51.100.0/24 -> 172.30.199.14 (ce)",
                LaunchPhase::Route,
            ),
        ];
        for (line, phase) in fixtures {
            assert_eq!(
                classify(&mut classifier, OperationKind::Launch, line),
                Some(OperationPhase::Launch(phase)),
                "{line}"
            );
        }
        // A later node's start line must not move progress backwards.
        assert_eq!(
            classify(
                &mut classifier,
                OperationKind::Launch,
                "gimlet-0: launch start"
            ),
            None
        );
    }
}
