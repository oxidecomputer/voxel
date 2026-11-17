// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pop-ups in wicket's style: a header joined to a body, with the pop-up's
//! actions as boxed buttons along the bottom right, sized to the text they
//! hold rather than to the screen.

use super::{
    colors::{OX_OFF_WHITE, TUI_PURPLE},
    widgets::{
        background, header_style, help_function_style, help_key_style,
        line_style, popup_backdrop, rounded_block, selection_style,
    },
};
use std::ops::Range;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{BorderType, Paragraph, Wrap},
};

/// Rows a button takes: its text between a top and bottom border.
const BUTTON_HEIGHT: u16 = 3;

/// An action the pop-up offers. A button with a key shows it; a selected
/// button is the one Enter activates.
pub(crate) struct Button<'a> {
    pub(crate) label: &'a str,
    pub(crate) key: Option<&'a str>,
    pub(crate) selected: bool,
}

impl Button<'_> {
    fn text(&self) -> Line<'_> {
        let (label, key) = if self.selected {
            let style = selection_style().add_modifier(Modifier::BOLD);
            (style, style)
        } else {
            (help_function_style(), help_key_style())
        };
        let mut spans = vec![Span::styled(format!(" {}", self.label), label)];
        if let Some(text) = self.key {
            spans.push(Span::styled(format!(" <{text}>"), key));
        }
        spans.push(Span::styled(" ", label));
        Line::from(spans)
    }

    fn width(&self) -> u16 {
        self.text().width() as u16 + 2
    }
}

pub(crate) struct Popup<'a> {
    pub(crate) header: Line<'a>,
    pub(crate) body: Text<'a>,
    pub(crate) buttons: Vec<Button<'a>>,
}

/// Where a pop-up and its parts land within the bounds it is drawn in.
pub(crate) struct PopupLayout {
    pub(crate) area: Rect,
    header: Rect,
    divider: u16,
    pub(crate) body: Rect,
    /// Indices of the buttons on each row, top to bottom.
    button_rows: Vec<Range<usize>>,
    buttons: Rect,
}

fn wrapped_height(text: Text<'_>, width: u16) -> u16 {
    Paragraph::new(text).wrap(Wrap { trim: false }).line_count(width) as u16
}

impl Popup<'_> {
    fn buttons_width(&self) -> u16 {
        self.buttons
            .iter()
            .map(|button| button.width() + 1)
            .sum::<u16>()
            .saturating_sub(1)
    }

    /// Packs the buttons, in order, into rows no wider than `width`, so a
    /// narrow terminal wraps them rather than hiding any.
    fn button_rows(&self, width: u16) -> Vec<Range<usize>> {
        let mut rows = Vec::new();
        let (mut start, mut used) = (0, 0);
        for (index, button) in self.buttons.iter().enumerate() {
            let needed = button.width() + u16::from(index > start);
            if index > start && used + needed > width {
                rows.push(start..index);
                (start, used) = (index, button.width());
            } else {
                used += needed;
            }
        }
        if start < self.buttons.len() {
            rows.push(start..self.buttons.len());
        }
        rows
    }

    /// Sizes the pop-up to its widest line, wrapping only past four fifths
    /// of the bounds, and centres it there. A body taller than the room left
    /// keeps its full height in the text and scrolls within `body`.
    pub(crate) fn layout(&self, bounds: Rect) -> PopupLayout {
        // Borders take a column on each side and padding another.
        let frame = 4;
        let max_width = (bounds.width * 4 / 5).max(bounds.width.min(40));
        let max_height = (bounds.height * 4 / 5).max(bounds.height.min(12));
        let natural = self
            .body
            .lines
            .iter()
            .map(Line::width)
            .chain([self.header.width()])
            .max()
            .unwrap_or_default() as u16;
        let wanted = natural.max(self.buttons_width()).saturating_add(frame);
        let mut width = wanted.min(max_width);
        // Each wrapped row of buttons costs three rows of height, which a
        // short terminal cannot spare, so widen to the bounds first.
        if self.button_rows(width.saturating_sub(frame)).len() > 1 {
            width = wanted.min(bounds.width);
        }
        let content_width = width.saturating_sub(frame).max(1);
        let header_height =
            wrapped_height(Text::from(self.header.clone()), content_width);
        let body_height = if self.body.lines.is_empty() {
            0
        } else {
            wrapped_height(self.body.clone(), content_width)
        };
        let button_rows = self.button_rows(content_width);
        let rows_height = button_rows.len() as u16 * BUTTON_HEIGHT;
        // Top border, divider and bottom border.
        let base = 3 + header_height + rows_height;
        // The body gives way first, so the header and buttons fit wherever
        // the bounds allow.
        let limit = max_height.max(base).min(bounds.height);
        // A blank row sets the buttons apart from the body, when there is
        // room for it and a row of body.
        let gap =
            u16::from(rows_height > 0 && body_height > 0 && base + 1 < limit);
        let chrome = base + gap;
        let height = chrome.saturating_add(body_height).min(limit);
        let area = Rect::new(
            bounds.x + bounds.width.saturating_sub(width) / 2,
            bounds.y + bounds.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let inner_x = area.x + 2;
        let header =
            Rect::new(inner_x, area.y + 1, content_width, header_height);
        let divider = header.bottom();
        let body = Rect::new(
            inner_x,
            divider + 1,
            content_width,
            height.saturating_sub(chrome).min(body_height),
        );
        let buttons = Rect::new(
            inner_x,
            area.bottom().saturating_sub(1 + rows_height).max(divider + 1),
            content_width,
            rows_height,
        );
        PopupLayout { area, header, divider, body, button_rows, buttons }
    }

    /// Draws the pop-up over a faded screen, its body scrolled down by
    /// `scroll` rows.
    pub(crate) fn render(
        self,
        frame: &mut ratatui::Frame<'_>,
        bounds: Rect,
        scroll: u16,
    ) {
        let layout = self.layout(bounds);
        let area = layout.area;
        if area.width <= 4 || area.height <= 3 {
            return;
        }
        popup_backdrop(frame, area);
        frame.render_widget(
            rounded_block()
                .border_style(line_style(true))
                .style(background().fg(OX_OFF_WHITE)),
            area,
        );
        // Join the header to the body with a rule across the frame.
        let rule = format!("├{}┤", "─".repeat(usize::from(area.width - 2)));
        frame.render_widget(
            Paragraph::new(Line::styled(rule, line_style(true))),
            Rect::new(area.x, layout.divider, area.width, 1),
        );
        frame.render_widget(
            Paragraph::new(self.header)
                .style(header_style(true))
                .wrap(Wrap { trim: false }),
            layout.header,
        );
        frame.render_widget(
            Paragraph::new(self.body)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            layout.body,
        );
        // Each row of buttons lines up from the right.
        for (row, indices) in layout.button_rows.iter().enumerate() {
            let y = layout.buttons.y + row as u16 * BUTTON_HEIGHT;
            let mut right = layout.buttons.right();
            for button in self.buttons[indices.clone()].iter().rev() {
                let width =
                    button.width().min(right.saturating_sub(layout.buttons.x));
                let x = right - width;
                let block = rounded_block().border_style(if button.selected {
                    Style::default().fg(TUI_PURPLE)
                } else {
                    line_style(true)
                });
                // The heavier border marks the selection without relying on
                // colour.
                let block = if button.selected {
                    block.border_type(BorderType::Thick)
                } else {
                    block
                };
                frame.render_widget(
                    Paragraph::new(button.text()).block(block),
                    Rect::new(x, y, width, BUTTON_HEIGHT),
                );
                right = x.saturating_sub(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Button, Popup};
    use crate::tui::test_support::{render, text};
    use ratatui::text::{Line, Text};

    /// The pop-up's frame, border included, cut out of a screen holding
    /// only the pop-up. The frame's left edge is found from the rule under
    /// its header.
    fn popup_frame(screen: &str) -> Vec<String> {
        let rows: Vec<Vec<char>> =
            screen.lines().map(|row| row.chars().collect()).collect();
        let (rule, left) = rows
            .iter()
            .enumerate()
            .find_map(|(y, row)| {
                row.iter().position(|c| *c == '├').map(|x| (y, x))
            })
            .expect("a header rule");
        let right = rows[rule].iter().position(|c| *c == '┤').unwrap();
        let edge = |y: usize, corner| rows[y].get(left) == Some(&corner);
        let top = (0..=rule).rev().find(|y| edge(*y, '╭')).unwrap();
        let bottom = (rule..rows.len()).find(|y| edge(*y, '╰')).unwrap();
        rows[top..=bottom]
            .iter()
            .map(|row| row[left..=right.min(row.len() - 1)].iter().collect())
            .collect()
    }

    /// A header, two body lines, and buttons 20, 29 and 12 columns wide.
    fn detach() -> Popup<'static> {
        let button = |label, key, selected| Button { label, key, selected };
        Popup {
            header: Line::from("Leave the TUI, keep the deployment?"),
            body: Text::from(vec![
                Line::from("Resume later with:"),
                Line::from("voxel tui resume"),
            ]),
            buttons: vec![
                button("Copy command", Some("y"), false),
                button("Leave and keep it running", None, false),
                button("Back", Some("n"), true),
            ],
        }
    }

    /// A pop-up is as big as its text, not the screen, and in the room the
    /// smallest terminal leaves it still holds its header and every button,
    /// wrapping rather than dropping the buttons a row cannot hold.
    #[test]
    fn popups_fit_their_text_and_keep_every_button() {
        // The button row is widest: its buttons and a column between each,
        // plus a border and a column of padding each side. The rows are the
        // header, two body lines, a blank, the buttons three high, and the
        // rule and two borders.
        let popup = popup_frame(&text(&render(160, 50, |frame| {
            detach().render(frame, frame.area(), 0)
        })));
        let size = (popup[0].chars().count(), popup.len());
        assert_eq!(size, (67, 10), "\n{}", popup.join("\n"));

        // The 48 by 16 minimum terminal leaves pop-ups 48 by 10.
        let popup = popup_frame(&text(&render(48, 10, |frame| {
            detach().render(frame, frame.area(), 0)
        })))
        .join("\n");
        for part in [
            "Leave the TUI, keep the deployment?",
            "Copy command <y>",
            "Leave and keep it running",
            "Back <n>",
        ] {
            assert!(popup.contains(part), "{part} missing:\n{popup}");
        }
    }
}
