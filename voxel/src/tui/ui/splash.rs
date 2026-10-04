// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Launch animation, after wicket's splash screen: the banner is painted
//! left to right with the `x` in Oxide green, then hands off to the main UI.

use std::time::{Duration, Instant};

use ratatui::{buffer::Buffer, layout::Rect, style::Color};

use super::colors::{OX_GREEN_DARKEST, OX_GREEN_LIGHT, OX_OFF_WHITE};

/// Wicket's frame cadence; the steady-state UI tick is far too coarse to
/// animate with, so a dedicated ticker runs at this period while the splash
/// is up.
pub const FRAME: Duration = Duration::from_millis(30);
const TOTAL_FRAMES: u32 = 100;
pub const DURATION: Duration = Duration::from_millis(30 * TOTAL_FRAMES as u64);
/// Frames to hold the unpainted banner before the sweep starts.
const DELAY_FRAMES: usize = 8;

// The `o`, `x`, and `e` are lifted from wicket's Oxide banner.
const BANNER: &str = concat!(
    "            #####                       ####\n",
    "           ##   ##                        ##\n",
    "##    ##  ##   # ##  ##   ##   ####       ##\n",
    "##    ##  ##  #  ##   ## ##   ##  ##      ##\n",
    " ##  ##   ## #   ##    ###    ########    ##\n",
    "  ####     ##   ##    ## ##   ##          ##\n",
    "   ##       #####    ##   ##   ####     ######",
);
const X_COLUMNS: std::ops::RangeInclusive<usize> = 21..=27;
pub const WIDTH: u16 = 46;
pub const HEIGHT: u16 = 7;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Splash {
    started: Option<Instant>,
}

impl Splash {
    /// Advances the animation clock; returns false once it has run its course.
    pub fn tick(&mut self, now: Instant) -> bool {
        let started = *self.started.get_or_insert(now);
        now.saturating_duration_since(started) < DURATION
    }

    fn frame(&self, now: Option<Instant>) -> usize {
        match (self.started, now) {
            (Some(started), Some(now)) => {
                (now.saturating_duration_since(started).as_millis()
                    / FRAME.as_millis()) as usize
            }
            _ => 0,
        }
    }
}

/// Returns false, leaving the frame untouched, when the banner does not fit.
pub fn draw(
    frame: &mut ratatui::Frame<'_>,
    splash: &Splash,
    now: Option<Instant>,
) -> bool {
    let area = frame.area();
    if area.width < WIDTH || area.height < HEIGHT {
        return false;
    }
    let rect = Rect::new(
        area.x + (area.width - WIDTH) / 2,
        area.y + (area.height - HEIGHT) / 2,
        WIDTH,
        HEIGHT,
    );
    paint(frame.buffer_mut(), rect, splash.frame(now));
    true
}

fn paint(buf: &mut Buffer, rect: Rect, frame: usize) {
    let paint_point = frame.saturating_sub(DELAY_FRAMES);
    for (y, line) in BANNER.lines().enumerate() {
        for (x, c) in line.chars().enumerate() {
            if c != '#' {
                continue;
            }
            let color: Color = if x >= paint_point {
                OX_GREEN_DARKEST
            } else if X_COLUMNS.contains(&x) {
                OX_GREEN_LIGHT
            } else {
                OX_OFF_WHITE
            };
            buf[(rect.x + x as u16, rect.y + y as u16)]
                .set_symbol(" ")
                .set_bg(color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_fits_its_declared_box() {
        let lines = BANNER.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), usize::from(HEIGHT));
        assert!(lines.iter().all(|line| line.len() <= usize::from(WIDTH)));
        assert!(lines.iter().any(|line| line.len() == usize::from(WIDTH)));
    }

    #[test]
    fn splash_yields_to_the_main_ui_when_the_banner_does_not_fit() {
        use ratatui::{Terminal, backend::TestBackend};
        let drawn = |width, height| {
            let mut terminal =
                Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut drawn = false;
            terminal
                .draw(|frame| drawn = draw(frame, &Splash::default(), None))
                .unwrap();
            drawn
        };
        assert!(drawn(80, 24));
        assert!(!drawn(WIDTH - 1, 24));
        assert!(!drawn(80, HEIGHT - 1));
    }

    #[test]
    fn splash_runs_for_its_duration_from_the_first_tick() {
        let start = Instant::now();
        let mut splash = Splash::default();
        assert!(splash.tick(start));
        assert!(splash.tick(start + DURATION - Duration::from_millis(1)));
        assert!(!splash.tick(start + DURATION));
    }

    #[test]
    fn sweep_paints_left_to_right_with_a_green_x() {
        let render = |frame| {
            let area = Rect::new(0, 0, WIDTH, HEIGHT);
            let mut buf = Buffer::empty(area);
            paint(&mut buf, area, frame);
            buf
        };
        // Row 4 crosses the `v` (column 1), the `x` (23), and the `e` (30).
        let start = render(0);
        assert_eq!(start[(1, 4)].bg, OX_GREEN_DARKEST);
        let done = render(DELAY_FRAMES + usize::from(WIDTH));
        assert_eq!(done[(1, 4)].bg, OX_OFF_WHITE);
        assert_eq!(done[(23, 4)].bg, OX_GREEN_LIGHT);
        // Both arms of the `x` are green, and the `o` beside it is not.
        assert_eq!(done[(21, 2)].bg, OX_GREEN_LIGHT);
        assert_eq!(done[(27, 2)].bg, OX_GREEN_LIGHT);
        assert_eq!(done[(18, 2)].bg, OX_OFF_WHITE);
        let halfway = render(DELAY_FRAMES + 23);
        assert_eq!(halfway[(1, 4)].bg, OX_OFF_WHITE);
        assert_eq!(halfway[(30, 4)].bg, OX_GREEN_DARKEST);
    }
}
