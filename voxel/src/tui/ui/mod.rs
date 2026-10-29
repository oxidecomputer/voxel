// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rendering. Each submodule draws one view, pane or overlay from an
//! immutable [`App`](crate::tui::App).

pub mod colors;
pub mod confirm_dialog;
pub mod deployment;
pub mod external_monitoring;
pub mod help;
pub mod logs;
pub mod monitor;
pub mod node_detail;
pub(crate) mod popup;
pub mod rack_selector;
pub mod renderer;
pub mod splash;
pub(crate) mod topology;
pub mod widgets;

#[cfg(test)]
mod tests {
    use super::renderer::{
        LayoutMode, MIN_HEIGHT, MIN_WIDTH, WIDE_HEIGHT, WIDE_WIDTH, layout_mode,
    };
    use ratatui::layout::Rect;

    #[test]
    fn responsive_boundaries_are_explicit() {
        for (label, width, height, mode) in [
            (
                "one column short",
                MIN_WIDTH - 1,
                MIN_HEIGHT,
                LayoutMode::Minimum,
            ),
            ("minimum", MIN_WIDTH, MIN_HEIGHT, LayoutMode::Compact),
            ("wide but short", WIDE_WIDTH, MIN_HEIGHT, LayoutMode::Compact),
            (
                "one column short of wide",
                WIDE_WIDTH - 1,
                WIDE_HEIGHT,
                LayoutMode::Compact,
            ),
            ("wide", WIDE_WIDTH, WIDE_HEIGHT, LayoutMode::Wide),
        ] {
            assert_eq!(
                layout_mode(Rect::new(0, 0, width, height)),
                mode,
                "{label}: {width}x{height}"
            );
        }
    }
}
