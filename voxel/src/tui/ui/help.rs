use super::{colors::*, widgets::overlay_area};
use crate::tui::App;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};

const GLOBAL: &[(&str, &str)] = &[
    ("1 / 2", "Deployment / Monitoring"),
    ("Tab / Shift-Tab", "Next / previous top-level section unconditionally"),
    ("Space", "Fold / expand focused section"),
    ("? / F1", "Open / close Help"),
    ("Esc", "Close the topmost non-confirmation pane or selection"),
    ("d", "Detach and leave resources; resume with voxel tui resume"),
    ("q", "Quit; destroy deployment first unless already stopped"),
];
const DEPLOYMENT: &[(&str, &str)] = &[
    ("Sections", "Overall Progress, Phases, Status, Current Phase, Logs"),
    ("↑ / ↓", "Move nested; at edge/no vertical content, move section"),
    ("PgUp / PgDn", "Page nested content only"),
    ("f", "Cycle log filter"),
    ("l / r", "Launch / route"),
    ("c / x", "Cancel and leave / destroy resources"),
];
const MONITORING: &[(&str, &str)] = &[
    ("Sections", "Rack Summary, Topology, Top Zones by Traffic"),
    ("← / →", "Previous / next rack in Rack Summary"),
    ("↑ / ↓", "Move resource; at edge/no vertical content, move section"),
    ("PgUp / PgDn", "Page Topology resources or Top Zones"),
    ("Enter", "Open selected resource detail"),
    ("Top Zones ↑ / ↓", "Browse the rack-wide traffic ranking"),
    ("Detail ↑ / ↓", "Browse peers; detail stays open; section unchanged"),
    ("Detail Enter/Esc", "Close resource detail"),
];
const DIALOGS: &[(&str, &str)] = &[
    ("↑ / ↓", "Choose a confirmation option"),
    ("Enter", "Confirm selected safe-default option"),
    ("y", "Copy full fallback command from Detach"),
    ("n", "Reject confirmation"),
    ("Esc", "Reject or close topmost dialog"),
    ("Help ↑/↓ / Pg", "Scroll one wrapped line / page"),
];
const TRAFFIC: &[(&str, &str)] =
    &[("Rates", "normal ≤100 KB/s; elevated >100 KB/s–5 MB/s; high >5 MB/s")];
const HEALTH: &[(&str, &str)] = &[
    ("States", "● healthy; ◌ checking status; ! degraded / failed"),
    ("", "? / ◐ / × / ■ checking status"),
];

fn key_line(key: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:<18}"),
            Style::default().fg(TUI_YELLOW).add_modifier(Modifier::BOLD),
        ),
        Span::styled(description, Style::default().fg(OX_OFF_WHITE)),
    ])
}

fn help_text() -> Text<'static> {
    let mut lines = Vec::new();
    for (title, rows) in [
        ("Global", GLOBAL),
        ("Deployment", DEPLOYMENT),
        ("Monitoring", MONITORING),
        ("Dialogs", DIALOGS),
        ("Traffic", TRAFFIC),
        ("Health", HEALTH),
    ] {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            title,
            Style::default().fg(TUI_PURPLE).add_modifier(Modifier::BOLD),
        ));
        lines.extend(
            rows.iter().map(|(key, description)| key_line(key, description)),
        );
    }
    Text::from(lines)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HelpMetrics {
    total: usize,
    capacity: usize,
    max_scroll: usize,
}

fn text_metrics(text: &Text<'_>, area: Rect) -> HelpMetrics {
    let inner = Block::bordered().inner(area);
    let capacity = usize::from(inner.height).max(1);
    let total = Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner.width);
    HelpMetrics { total, capacity, max_scroll: total.saturating_sub(capacity) }
}

fn help_metrics(area: Rect) -> HelpMetrics {
    text_metrics(&help_text(), area)
}

pub(crate) fn help_area(root: Rect) -> Rect {
    let horizontal_margin = if root.width >= 52 { 4 } else { 0 };
    let vertical_margin = if root.height >= 12 { 2 } else { 0 };
    let width = root.width.saturating_sub(horizontal_margin).min(76);
    let height = root.height.saturating_sub(vertical_margin).min(18);
    Rect::new(
        root.x + root.width.saturating_sub(width) / 2,
        root.y + root.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub fn draw(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    _mode: super::renderer::LayoutMode,
) {
    let area = help_area(overlay_area(frame.area()));
    // The application renderer returns its minimum-size fallback before Help.
    // Keep direct helper draws for degenerate test/backend areas empty as well.
    if area.width <= 2 || area.height <= 2 {
        return;
    }
    frame.render_widget(Clear, area);

    let body = help_text();
    let metrics = text_metrics(&body, area);
    let start = app.session.help_scroll.min(metrics.max_scroll);
    let end = (start + metrics.capacity).min(metrics.total);
    let title = format!(
        " Help {}-{} of {} ",
        start.saturating_add(1).min(metrics.total),
        end,
        metrics.total
    );
    let block = Block::bordered()
        .border_style(Style::default().fg(TUI_GREY))
        .title(title)
        .title_style(
            Style::default().fg(TUI_YELLOW).add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(OX_GREEN_DARKEST));
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((start as u16, 0))
            .style(Style::default().bg(OX_GREEN_DARKEST))
            .block(block),
        area,
    );
}

pub(crate) fn page_capacity(app: &App) -> usize {
    let terminal = Rect::new(
        0,
        0,
        app.session.terminal.width,
        app.session.terminal.height,
    );
    help_metrics(help_area(overlay_area(terminal))).capacity
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::{Buffer, Cell},
        widgets::Widget,
    };

    const HELP_SECTIONS: &[(&str, &[(&str, &str)])] = &[
        ("Global", GLOBAL),
        ("Deployment", DEPLOYMENT),
        ("Monitoring", MONITORING),
        ("Dialogs", DIALOGS),
        ("Traffic", TRAFFIC),
        ("Health", HEALTH),
    ];

    fn rendered_help(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                super::draw(
                    frame,
                    app,
                    super::super::renderer::LayoutMode::Compact,
                )
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let area = buffer.area;
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn panel_rows(buffer: &Buffer, area: Rect) -> Vec<Vec<Cell>> {
        let inner = Block::bordered().inner(area);
        (inner.y..inner.bottom())
            .map(|y| {
                (inner.x..inner.right())
                    .map(|x| buffer[(x, y)].clone())
                    .collect()
            })
            .collect()
    }

    fn rendered_lines_at_all_offsets(
        width: u16,
        height: u16,
    ) -> (HelpMetrics, Vec<Vec<Cell>>) {
        let area = help_area(overlay_area(Rect::new(0, 0, width, height)));
        let metrics = help_metrics(area);
        let mut app = App::new(vec![], 8, 8);
        app.session.terminal.width = width;
        app.session.terminal.height = height;
        let mut lines = vec![None; metrics.total];

        for offset in 0..=metrics.max_scroll {
            app.session.help_scroll = offset;
            let buffer = rendered_help(&app, width, height);
            for (index, row) in
                panel_rows(&buffer, area).into_iter().enumerate()
            {
                let rendered_index = offset + index;
                if rendered_index < metrics.total {
                    if let Some(previous) = &lines[rendered_index] {
                        assert_eq!(
                            previous, &row,
                            "line {rendered_index} changed by offset"
                        );
                    } else {
                        lines[rendered_index] = Some(row);
                    }
                }
            }
        }

        (
            metrics,
            lines
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    line.unwrap_or_else(|| {
                        panic!("line {index} was unreachable")
                    })
                })
                .collect(),
        )
    }

    fn normalized_cells(lines: &[Vec<Cell>]) -> Vec<&Cell> {
        lines
            .iter()
            .flatten()
            .filter(|cell| {
                !cell.symbol().is_empty()
                    && !cell.symbol().chars().all(char::is_whitespace)
            })
            .collect()
    }

    fn normalized_symbols(text: &str) -> Vec<String> {
        text.chars()
            .filter(|character| !character.is_whitespace())
            .map(|character| character.to_string())
            .collect()
    }

    fn find_symbols(
        cells: &[&Cell],
        expected: &str,
        from: usize,
    ) -> std::ops::Range<usize> {
        let expected = normalized_symbols(expected);
        let start = (from..=cells.len().saturating_sub(expected.len()))
            .find(|&start| {
                cells[start..start + expected.len()]
                    .iter()
                    .zip(&expected)
                    .all(|(cell, symbol)| cell.symbol() == symbol)
            })
            .unwrap_or_else(|| {
                panic!(
                    "rendered panel did not contain {expected:?} after {from}"
                )
            });
        start..start + expected.len()
    }

    fn assert_range_style(
        cells: &[&Cell],
        range: std::ops::Range<usize>,
        foreground: ratatui::style::Color,
        modifier: Option<Modifier>,
        label: &str,
    ) {
        assert!(!range.is_empty(), "empty rendered range for {label}");
        for cell in &cells[range] {
            assert_eq!(cell.fg, foreground, "wrong foreground in {label}");
            if let Some(modifier) = modifier {
                assert!(
                    cell.modifier.contains(modifier),
                    "missing {modifier:?} in {label}"
                );
            }
        }
    }

    #[test]
    fn help_is_bounded_and_uses_full_minimum_content_only_when_needed() {
        assert_eq!(
            help_area(Rect::new(0, 0, 160, 34)),
            Rect::new(42, 8, 76, 18)
        );
        assert_eq!(help_area(Rect::new(0, 0, 48, 11)), Rect::new(0, 0, 48, 11));
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 2, 2),
            Rect::new(17, 23, 160, 34),
        ] {
            let help = help_area(area);
            assert!(help.x >= area.x && help.y >= area.y);
            assert!(
                help.right() <= area.right() && help.bottom() <= area.bottom()
            );
        }
        assert_eq!(
            help_area(Rect::new(17, 23, 160, 34)),
            Rect::new(59, 31, 76, 18)
        );
    }

    #[test]
    fn help_contract_contains_every_exact_category_row_and_legend() {
        let rows = HELP_SECTIONS;
        assert_eq!(
            rows.iter().map(|(category, _)| *category).collect::<Vec<_>>(),
            [
                "Global",
                "Deployment",
                "Monitoring",
                "Dialogs",
                "Traffic",
                "Health"
            ]
        );
        let expected = [
            ("1 / 2", "Deployment / Monitoring"),
            (
                "Tab / Shift-Tab",
                "Next / previous top-level section unconditionally",
            ),
            ("Space", "Fold / expand focused section"),
            ("? / F1", "Open / close Help"),
            ("Esc", "Close the topmost non-confirmation pane or selection"),
            ("d", "Detach and leave resources; resume with voxel tui resume"),
            ("q", "Quit; destroy deployment first unless already stopped"),
            (
                "Sections",
                "Overall Progress, Phases, Status, Current Phase, Logs",
            ),
            ("↑ / ↓", "Move nested; at edge/no vertical content, move section"),
            ("PgUp / PgDn", "Page nested content only"),
            ("f", "Cycle log filter"),
            ("l / r", "Launch / route"),
            ("c / x", "Cancel and leave / destroy resources"),
            ("Sections", "Rack Summary, Topology, Top Zones by Traffic"),
            ("← / →", "Previous / next rack in Rack Summary"),
            (
                "↑ / ↓",
                "Move resource; at edge/no vertical content, move section",
            ),
            ("PgUp / PgDn", "Page Topology resources or Top Zones"),
            ("Enter", "Open selected resource detail"),
            ("Top Zones ↑ / ↓", "Browse the rack-wide traffic ranking"),
            (
                "Detail ↑ / ↓",
                "Browse peers; detail stays open; section unchanged",
            ),
            ("Detail Enter/Esc", "Close resource detail"),
            ("↑ / ↓", "Choose a confirmation option"),
            ("Enter", "Confirm selected safe-default option"),
            ("y", "Copy full fallback command from Detach"),
            ("n", "Reject confirmation"),
            ("Esc", "Reject or close topmost dialog"),
            ("Help ↑/↓ / Pg", "Scroll one wrapped line / page"),
            (
                "Rates",
                "normal ≤100 KB/s; elevated >100 KB/s–5 MB/s; high >5 MB/s",
            ),
            ("States", "● healthy; ◌ checking status; ! degraded / failed"),
            ("", "? / ◐ / × / ■ checking status"),
        ];
        assert_eq!(
            rows.iter()
                .flat_map(|(_, rows)| rows.iter().copied())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(!DEPLOYMENT.iter().any(|(key, _)| *key == "Enter"));
    }

    #[test]
    fn tiny_direct_draws_are_empty_and_supported_capacity_contract_stays_one() {
        for (width, height) in [(0, 0), (1, 1), (2, 2)] {
            let mut app = App::new(vec![], 8, 8);
            app.session.terminal.width = width;
            app.session.terminal.height = height;
            let buffer = rendered_help(&app, width, height);
            assert!(buffer_text(&buffer).trim().is_empty());
            assert_eq!(page_capacity(&app), 1);
        }
    }

    #[test]
    fn shared_wrapped_metrics_cover_ranges_and_clamp() {
        let mut app = App::new(vec![], 8, 8);
        app.session.terminal.width = 48;
        app.session.terminal.height = 16;
        let area = help_area(overlay_area(Rect::new(0, 0, 48, 16)));
        let metrics = help_metrics(area);
        assert_eq!(metrics.capacity, page_capacity(&app));
        assert!(metrics.total > metrics.capacity * 3);

        for requested in [0, metrics.max_scroll / 2, usize::MAX] {
            app.session.help_scroll = requested;
            let buffer = rendered_help(&app, 48, 16);
            let text = buffer_text(&buffer);
            let start = requested.min(metrics.max_scroll);
            let end = (start + metrics.capacity).min(metrics.total);
            assert!(text.contains(&format!(
                "Help {}-{} of {}",
                start + 1,
                end,
                metrics.total
            )));
        }
    }

    #[test]
    fn every_help_row_is_reachable_and_exactly_styled_at_every_supported_size()
    {
        for (width, height) in [(160, 38), (100, 22), (80, 20), (48, 16)] {
            let (metrics, lines) = rendered_lines_at_all_offsets(width, height);
            assert_eq!(lines.len(), metrics.total);
            let cells = normalized_cells(&lines);
            let mut cursor = 0;

            for (heading, rows) in HELP_SECTIONS {
                let heading_range = find_symbols(&cells, heading, cursor);
                assert_range_style(
                    &cells,
                    heading_range.clone(),
                    TUI_PURPLE,
                    Some(Modifier::BOLD),
                    heading,
                );
                cursor = heading_range.end;

                for (key, description) in *rows {
                    let rendered_row = format!("{key:<18}{description}");
                    let row_range = find_symbols(&cells, &rendered_row, cursor);
                    let key_len = normalized_symbols(key).len();
                    if key_len > 0 {
                        assert_range_style(
                            &cells,
                            row_range.start..row_range.start + key_len,
                            TUI_YELLOW,
                            Some(Modifier::BOLD),
                            key,
                        );
                    }
                    assert_range_style(
                        &cells,
                        row_range.start + key_len..row_range.end,
                        OX_OFF_WHITE,
                        None,
                        description,
                    );
                    cursor = row_range.end;
                }
            }
        }
    }

    #[test]
    fn wrapping_uses_display_width_and_preserves_continuation_styles() {
        let text = Text::from(Line::from(vec![
            Span::styled("界e\u{301}界 ", Style::default().fg(TUI_YELLOW)),
            Span::styled(
                "continuation words",
                Style::default().fg(OX_OFF_WHITE),
            ),
        ]));
        let metrics = text_metrics(&text, Rect::new(0, 0, 10, 8));
        assert_eq!(metrics.total, 4);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 8));
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .render(Rect::new(0, 0, 10, 8), &mut buffer);
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "界" && cell.fg == TUI_YELLOW)
        );
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let word = normalized_symbols("words");
        let (row, start) = rows
            .iter()
            .enumerate()
            .find_map(|(row, cells)| {
                (0..=cells.len() - word.len())
                    .find(|&start| {
                        cells[start..start + word.len()]
                            .iter()
                            .zip(&word)
                            .all(|(cell, symbol)| cell.symbol() == symbol)
                    })
                    .map(|start| (row, start))
            })
            .expect("wrapped continuation token must be rendered");
        assert!(row > 0, "words must be on a wrapped continuation row");
        for cell in &rows[row][start..start + word.len()] {
            assert_eq!(cell.fg, OX_OFF_WHITE);
        }
    }
}
