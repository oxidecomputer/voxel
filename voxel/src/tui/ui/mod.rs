pub mod colors;
pub mod confirm_dialog;
pub mod deployment;
pub mod help;
pub mod logs;
pub mod monitor;
pub mod node_detail;
pub mod rack_selector;
pub mod renderer;
#[allow(dead_code)]
// Consumed by the topology renderer and reducer in subsequent tasks.
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
        assert_eq!(layout_mode(Rect::new(0, 0, 0, 0)), LayoutMode::Minimum);
        assert_eq!(
            layout_mode(Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT)),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, WIDE_WIDTH, MIN_HEIGHT)),
            LayoutMode::Compact
        );
        assert_eq!(
            layout_mode(Rect::new(0, 0, WIDE_WIDTH, WIDE_HEIGHT)),
            LayoutMode::Wide
        );
    }
}
