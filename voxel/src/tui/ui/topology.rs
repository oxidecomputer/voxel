//! Rack elevation for the Monitor view, after wicket's rack widget: sleds
//! fill their cubbies around the switches and power shelves, coloured by
//! health, with the fleet's routers in an uplink strip above the rack.

use std::collections::BTreeMap;

use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::{
    colors::{
        OX_GRAY, OX_GREEN_LIGHT, OX_OFF_WHITE, OX_RED, OX_WHITE, TUI_BLACK,
        TUI_GREY, TUI_GREY_DARK, TUI_PURPLE, TUI_YELLOW,
    },
    monitor::{health_style, resource_health_state},
    widgets::{fit_terminal_width, line_style, rounded_block, selection_style},
};
use crate::tui::{
    App,
    telemetry::{
        HealthState, ResourceDescriptor, ResourceId, ResourceKind, SLED_CUBBIES,
    },
};

/// The uplink strip: a rounded box around one row of router labels.
const UPLINK_HEIGHT: u16 = 3;
/// The smallest of wicket's elevation tiers: one row per sled pair.
const RACK_MIN_HEIGHT: u16 = 20;
/// Inner Topology height that fits the uplink strip plus wicket's two-row
/// sled tier with full borders; the section asks for this when focused.
pub(crate) const PREFERRED_HEIGHT: u16 = UPLINK_HEIGHT + 41;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Bay {
    Sled(u8),
    Switch(u8),
    PowerShelf(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RackGeometry {
    pub(crate) rack: Rect,
    pub(crate) bays: BTreeMap<Bay, Rect>,
}

/// Wicket's elevation tiers as (rack, sled, switch/shelf) heights. Each of
/// the 16 sled rows and 4 middle bays gets the same share; the two-row sled
/// tiers spend one extra line on the bottom sleds' lower border.
fn tier(height: u16) -> Option<(u16, u16, u16)> {
    Some(match height {
        0..RACK_MIN_HEIGHT => return None,
        20..37 => (20, 1, 1),
        37..41 => (37, 2, 1),
        41..56 => (41, 2, 2),
        56..60 => (56, 3, 2),
        60..80 => (60, 3, 3),
        _ => {
            let share = height / 20;
            (share * 20, share, share)
        }
    })
}

fn make_even(value: u16) -> u16 {
    value + value % 2
}

/// The largest elevation that fits, keeping wicket's 2:3 aspect.
pub(crate) fn layout_rack(area: Rect) -> Option<RackGeometry> {
    let budget = area.height.min(area.width.saturating_mul(3) / 2);
    let (height, sled_height, other_height) = tier(budget)?;
    let width = make_even(height * 2 / 3);
    if width > area.width {
        return None;
    }
    let rack = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let sled_width = width / 2;
    let middle_y = rack.y + 8 * sled_height;
    let mut bays = BTreeMap::new();
    for cubby in 0..SLED_CUBBIES {
        // RFD 200 numbers cubbies from the bottom of the rack, even cubbies
        // on the left, with the switches and power shelves between halves.
        let y = if cubby >= 16 {
            rack.y + u16::from((31 - cubby) / 2) * sled_height
        } else {
            middle_y
                + 4 * other_height
                + u16::from((15 - cubby) / 2) * sled_height
        };
        let x = rack.x + if cubby % 2 == 0 { 0 } else { sled_width };
        let height =
            if cubby < 2 && sled_height == 2 { 3 } else { sled_height };
        bays.insert(Bay::Sled(cubby), Rect::new(x, y, sled_width, height));
    }
    for (offset, bay) in
        [Bay::Switch(1), Bay::PowerShelf(1), Bay::PowerShelf(0), Bay::Switch(0)]
            .into_iter()
            .enumerate()
    {
        let y = middle_y + offset as u16 * other_height;
        bays.insert(bay, Rect::new(rack.x, y, width, other_height));
    }
    Some(RackGeometry { rack, bays })
}

/// Width of the column holding the uplink strip and the rack, given the
/// column's height; padded so the rack does not touch its neighbours.
pub(crate) fn column_width(height: u16) -> u16 {
    let rack_height = height.saturating_sub(UPLINK_HEIGHT);
    let rack_width =
        tier(rack_height).map_or(0, |(height, _, _)| make_even(height * 2 / 3));
    rack_width.max(RACK_MIN_HEIGHT) + 4
}

fn bay_of(descriptor: &ResourceDescriptor) -> Option<Bay> {
    match descriptor.kind {
        ResourceKind::Sled => descriptor.slot.map(Bay::Sled),
        ResourceKind::SwitchZone => descriptor.slot.map(Bay::Switch),
        ResourceKind::Router => None,
    }
}

/// A selectable node's place on the navigation grid. Routers occupy row 0;
/// rack rows follow top to bottom, with a sled pair sharing a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct GridPosition {
    row: u8,
    column: u8,
}

fn grid(descriptors: &[ResourceDescriptor]) -> Vec<(GridPosition, ResourceId)> {
    let mut routers = descriptors
        .iter()
        .filter(|descriptor| descriptor.kind == ResourceKind::Router)
        .map(|descriptor| descriptor.id.clone())
        .collect::<Vec<_>>();
    routers.sort();
    let mut cells = routers
        .into_iter()
        .enumerate()
        .map(|(column, id)| (GridPosition { row: 0, column: column as u8 }, id))
        .collect::<Vec<_>>();
    let mut unplaced = Vec::new();
    for descriptor in descriptors {
        let position = match (descriptor.kind, descriptor.slot) {
            (ResourceKind::Router, _) => continue,
            (ResourceKind::Sled, Some(cubby)) => GridPosition {
                row: 1 + if cubby >= 16 {
                    (31 - cubby) / 2
                } else {
                    12 + (15 - cubby) / 2
                },
                column: cubby % 2,
            },
            (ResourceKind::SwitchZone, Some(slot)) => {
                GridPosition { row: if slot == 1 { 9 } else { 12 }, column: 0 }
            }
            (_, None) => {
                unplaced.push(descriptor.id.clone());
                continue;
            }
        };
        cells.push((position, descriptor.id.clone()));
    }
    // Nodes a real rack has no bay for stay reachable below the rack.
    unplaced.sort();
    cells.extend(unplaced.into_iter().enumerate().map(|(column, id)| {
        (GridPosition { row: u8::MAX, column: column as u8 }, id)
    }));
    cells.sort();
    cells
}

/// Selection order: uplink strip, then the rack top to bottom, left to right.
pub(crate) fn navigation_order(
    descriptors: &[ResourceDescriptor],
) -> Vec<ResourceId> {
    grid(descriptors).into_iter().map(|(_, id)| id).collect()
}

/// The nearest node in the next occupied row above or below, preferring the
/// current column, as wicket's up and down do over a fully populated rack.
pub(crate) fn vertical_neighbor(
    descriptors: &[ResourceDescriptor],
    current: &ResourceId,
    down: bool,
) -> Option<ResourceId> {
    let cells = grid(descriptors);
    let (here, _) = cells.iter().find(|(_, id)| id == current)?;
    let row = cells
        .iter()
        .map(|(position, _)| position.row)
        .filter(|row| if down { *row > here.row } else { *row < here.row })
        .reduce(|best, row| if down { best.min(row) } else { best.max(row) })?;
    cells
        .iter()
        .filter(|(position, _)| position.row == row)
        .min_by_key(|(position, _)| position.column.abs_diff(here.column))
        .map(|(_, id)| id.clone())
}

/// The adjacent node in the same row: a sled's sibling or the next router.
pub(crate) fn horizontal_neighbor(
    descriptors: &[ResourceDescriptor],
    current: &ResourceId,
    right: bool,
) -> Option<ResourceId> {
    let cells = grid(descriptors);
    let (here, _) = cells.iter().find(|(_, id)| id == current)?;
    let row = cells.iter().filter(|(position, _)| position.row == here.row);
    if right {
        row.filter(|(position, _)| position.column > here.column)
            .min_by_key(|(position, _)| position.column)
    } else {
        row.filter(|(position, _)| position.column < here.column)
            .max_by_key(|(position, _)| position.column)
    }
    .map(|(_, id)| id.clone())
}

fn rack_focused(app: &App) -> bool {
    app.session.monitoring_pane == crate::tui::event::MonitoringPane::Topology
        && !app.session.detail_open
}

pub(crate) fn draw(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    descriptors: &[ResourceDescriptor],
) {
    let mut routers = descriptors
        .iter()
        .filter(|descriptor| descriptor.kind == ResourceKind::Router)
        .collect::<Vec<_>>();
    routers.sort_by(|left, right| left.id.cmp(&right.id));
    let mut rack_area = area;
    if !routers.is_empty() && area.height > UPLINK_HEIGHT {
        draw_uplink(
            frame,
            Rect::new(area.x, area.y, area.width, UPLINK_HEIGHT),
            app,
            &routers,
        );
        rack_area.y += UPLINK_HEIGHT;
        rack_area.height -= UPLINK_HEIGHT;
    }
    let Some(geometry) = layout_rack(rack_area) else {
        if rack_area.height > 0 {
            let message = fit_terminal_width(
                &format!("Rack needs {RACK_MIN_HEIGHT} rows · focus to expand"),
                rack_area.width.into(),
            );
            frame.render_widget(
                Paragraph::new(message)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(TUI_GREY)),
                Rect::new(
                    rack_area.x,
                    rack_area.y + rack_area.height / 2,
                    rack_area.width,
                    1,
                ),
            );
        }
        return;
    };
    for (bay, rect) in geometry.bays {
        let occupant = descriptors
            .iter()
            .find(|descriptor| bay_of(descriptor) == Some(bay));
        draw_bay(frame, rect, app, bay, occupant);
    }
}

fn draw_uplink(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    routers: &[&ResourceDescriptor],
) {
    let mut spans = Vec::new();
    for router in routers {
        let state = resource_health_state(app, &router.id);
        let selected =
            app.session.selected_resource.as_ref() == Some(&router.id);
        let label = Span::raw(format!(" {} ", router.name));
        spans.push(Span::styled(health_glyph(state), health_style(state)));
        spans.push(if selected {
            label.style(selection_style())
        } else {
            label
        });
        spans.push(Span::raw(" "));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(
            rounded_block()
                .border_style(line_style(rack_focused(app)))
                .title(" Uplink "),
        ),
        area,
    );
}

/// Fill colours per health state; the foreground draws the bay texture.
fn bay_style(state: HealthState) -> Style {
    let (bg, fg) = match state {
        HealthState::Healthy => (OX_GREEN_LIGHT, TUI_BLACK),
        HealthState::Degraded | HealthState::Failed => (OX_RED, OX_WHITE),
        HealthState::Checking => (TUI_YELLOW, TUI_BLACK),
        HealthState::Stale => (TUI_GREY, TUI_BLACK),
        HealthState::Unknown
        | HealthState::Unavailable
        | HealthState::Stopped => (TUI_GREY_DARK, OX_OFF_WHITE),
    };
    Style::default().bg(bg).fg(fg)
}

fn draw_bay(
    frame: &mut ratatui::Frame<'_>,
    rect: Rect,
    app: &App,
    bay: Bay,
    occupant: Option<&ResourceDescriptor>,
) {
    // Like wicket, drop the bottom border when a bay has no room for it.
    let borders = if rect.height < 3 {
        Borders::TOP | Borders::LEFT | Borders::RIGHT
    } else {
        Borders::ALL
    };
    let title_width = usize::from(rect.width.saturating_sub(2));
    let (title, fill, symbol) = match (bay, occupant) {
        (Bay::PowerShelf(index), _) => (
            Line::raw(format!("PWR{index}")),
            Some(Style::default().bg(OX_GRAY).fg(OX_OFF_WHITE)),
            "█",
        ),
        (_, Some(node)) => {
            let state = resource_health_state(app, &node.id);
            // The title sits on the border, so the glyph needs its own
            // colour or it inherits the border's grey.
            let glyph = health_glyph(state);
            let name = fit_terminal_width(
                &node.name,
                title_width.saturating_sub(1 + glyph.chars().count()),
            );
            (
                Line::from(vec![
                    Span::raw(format!("{name} ")),
                    Span::styled(glyph, health_style(state)),
                ]),
                Some(bay_style(state)),
                if matches!(bay, Bay::Sled(_)) { "▕" } else { "❒" },
            )
        }
        (Bay::Sled(cubby), None) => (Line::raw(cubby.to_string()), None, " "),
        (Bay::Switch(slot), None) => {
            (Line::raw(format!("SW{slot}")), None, " ")
        }
    };
    let selected = occupant.is_some_and(|node| {
        app.session.selected_resource.as_ref() == Some(&node.id)
    });
    let border = if selected {
        Style::default().fg(TUI_BLACK).bg(TUI_PURPLE)
    } else if fill.is_some() {
        Style::default().fg(OX_GRAY).bg(TUI_BLACK)
    } else {
        Style::default().fg(TUI_GREY_DARK)
    };
    let block = Block::default()
        .borders(borders)
        .title(title)
        .style(fill.unwrap_or_default())
        .border_style(border);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if fill.is_none() {
        return;
    }
    let buffer = frame.buffer_mut();
    for x in inner.left()..inner.right() {
        // Power shelves read as six rectifiers, as in wicket.
        if matches!(bay, Bay::PowerShelf(_))
            && (x - inner.left()) % (inner.width / 6).max(1) == 0
        {
            continue;
        }
        for y in inner.top()..inner.bottom() {
            buffer[(x, y)].set_symbol(symbol);
        }
    }
}

pub(crate) fn health_glyph(state: HealthState) -> &'static str {
    match state {
        HealthState::Healthy => "●",
        HealthState::Checking => "◌",
        HealthState::Degraded | HealthState::Failed => "!",
        HealthState::Stale => "◐",
        HealthState::Unknown => "?",
        HealthState::Unavailable => "×",
        HealthState::Stopped => "■",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        event::View,
        telemetry::{RackId, ResourceKind},
    };
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn sled(cubby: u8) -> ResourceDescriptor {
        let name = format!("g{cubby}");
        ResourceDescriptor {
            id: ResourceId::rack(RackId(0), ResourceKind::Sled, &name),
            rack: Some(RackId(0)),
            kind: ResourceKind::Sled,
            name,
            host: None,
            slot: Some(cubby),
        }
    }

    fn switch(slot: u8, host: &str) -> ResourceDescriptor {
        ResourceDescriptor {
            id: ResourceId::rack(RackId(0), ResourceKind::SwitchZone, host),
            rack: Some(RackId(0)),
            kind: ResourceKind::SwitchZone,
            name: format!("switch{slot}"),
            host: Some(host.into()),
            slot: Some(slot),
        }
    }

    fn router(name: &str) -> ResourceDescriptor {
        ResourceDescriptor {
            id: ResourceId::fleet(ResourceKind::Router, name),
            rack: None,
            kind: ResourceKind::Router,
            name: name.into(),
            host: None,
            slot: None,
        }
    }

    /// A small Voxel rack: four sleds, two scrimlets, two routers.
    fn topology() -> Vec<ResourceDescriptor> {
        vec![
            sled(0),
            sled(1),
            sled(2),
            sled(3),
            switch(0, "g0"),
            switch(1, "g3"),
            router("ce"),
            router("cr1"),
        ]
    }

    fn render(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, frame.area(), app, &app.deployment.topology)
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &Buffer) -> String {
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    }

    #[test]
    fn every_tier_lays_out_disjoint_bays_inside_a_centered_rack() {
        for height in [20, 36, 37, 41, 56, 60, 80, 101] {
            let area = Rect::new(3, 5, 200, height);
            let geometry = layout_rack(area).unwrap();
            let rack = geometry.rack;
            assert_eq!(geometry.bays.len(), 36, "height {height}");
            assert_eq!(rack.width % 2, 0);
            assert!(rack.height <= height && rack.width <= area.width);
            assert_eq!(rack.x - area.x, area.right() - rack.right());
            let rects = geometry.bays.values().collect::<Vec<_>>();
            for (index, rect) in rects.iter().enumerate() {
                assert!(area.contains(rect.as_position()), "{rect:?}");
                assert!(rect.bottom() <= area.bottom(), "{rect:?}");
                assert!(rect.right() <= rack.right(), "{rect:?}");
                for other in &rects[index + 1..] {
                    assert!(!rect.intersects(**other), "{rect:?} {other:?}");
                }
            }
        }
    }

    #[test]
    fn elevations_that_do_not_fit_are_refused() {
        assert_eq!(
            layout_rack(Rect::new(0, 0, 200, RACK_MIN_HEIGHT - 1)),
            None
        );
        assert_eq!(layout_rack(Rect::new(0, 0, 13, 80)), None);
        // A narrow column steps down to a tier whose width fits.
        let geometry = layout_rack(Rect::new(0, 0, 14, 80)).unwrap();
        assert_eq!(geometry.rack.height, 20);
    }

    #[test]
    fn cubbies_follow_rfd_200_numbering() {
        let geometry = layout_rack(Rect::new(0, 0, 100, 41)).unwrap();
        let bay = |bay| geometry.bays[&bay];
        let rack = geometry.rack;
        assert_eq!(bay(Bay::Sled(30)).as_position(), rack.as_position());
        assert_eq!(bay(Bay::Sled(31)).y, rack.y);
        assert!(bay(Bay::Sled(31)).x > rack.x);
        assert_eq!(bay(Bay::Sled(0)).x, rack.x);
        assert_eq!(bay(Bay::Sled(0)).bottom(), rack.bottom());
        assert_eq!(bay(Bay::Sled(1)).y, bay(Bay::Sled(0)).y);
        assert!(bay(Bay::Sled(16)).bottom() <= bay(Bay::Switch(1)).y);
        assert!(bay(Bay::Switch(1)).bottom() <= bay(Bay::PowerShelf(1)).y);
        assert!(bay(Bay::PowerShelf(0)).bottom() <= bay(Bay::Switch(0)).y);
        assert!(bay(Bay::Switch(0)).bottom() <= bay(Bay::Sled(15)).y);
    }

    #[test]
    fn navigation_runs_from_the_uplink_down_the_rack() {
        let topology = topology();
        let names = navigation_order(&topology)
            .into_iter()
            .map(|id| id.name)
            .collect::<Vec<_>>();
        // Switch zones are identified by their hosting scrimlet.
        assert_eq!(names, ["ce", "cr1", "g3", "g0", "g2", "g3", "g0", "g1"]);
        let unplaced = ResourceDescriptor { slot: None, ..sled(9) };
        let order = navigation_order(&[sled(0), unplaced.clone()]);
        assert_eq!(order.last(), Some(&unplaced.id));
    }

    #[test]
    fn arrows_move_between_occupied_rows_and_siblings() {
        let topology = topology();
        let id = |descriptor: ResourceDescriptor| descriptor.id;
        let down = |from| vertical_neighbor(&topology, &from, true);
        let up = |from| vertical_neighbor(&topology, &from, false);
        let right = |from| horizontal_neighbor(&topology, &from, true);
        let left = |from| horizontal_neighbor(&topology, &from, false);

        assert_eq!(down(id(router("cr1"))), Some(id(switch(1, "g3"))));
        assert_eq!(down(id(switch(1, "g3"))), Some(id(switch(0, "g0"))));
        // Rows of empty cubbies are skipped, keeping to the current column.
        assert_eq!(down(id(switch(0, "g0"))), Some(id(sled(2))));
        assert_eq!(down(id(sled(3))), Some(id(sled(1))));
        assert_eq!(down(id(sled(1))), None);
        assert_eq!(up(id(sled(0))), Some(id(sled(2))));
        assert_eq!(up(id(switch(1, "g3"))), Some(id(router("ce"))));
        assert_eq!(up(id(router("ce"))), None);

        assert_eq!(right(id(sled(2))), Some(id(sled(3))));
        assert_eq!(right(id(sled(3))), None);
        assert_eq!(left(id(sled(3))), Some(id(sled(2))));
        assert_eq!(right(id(router("ce"))), Some(id(router("cr1"))));
        assert_eq!(left(id(switch(0, "g0"))), None);
    }

    #[test]
    fn rack_draws_occupants_by_health_and_marks_the_selection() {
        let mut app = App::new(topology(), 4, 4);
        app.session.view = View::Monitor;
        app.session.selected_resource = Some(sled(3).id);
        let buffer = render(&app, 60, 3 + 41);
        let geometry =
            layout_rack(Rect::new(0, UPLINK_HEIGHT, 60, 41)).unwrap();
        let text = text(&buffer);

        assert!(text.contains("Uplink"), "{text}");
        assert!(text.contains("ce") && text.contains("cr1"), "{text}");
        assert!(text.contains("switch1"), "{text}");
        assert!(text.contains("PWR0"), "{text}");
        // Empty cubbies carry only their number.
        assert!(text.contains("31"), "{text}");

        let unselected = geometry.bays[&Bay::Sled(2)];
        let (x, y) = (unselected.x + 1, unselected.y + 1);
        // Before the first tick every node is still being checked.
        assert_eq!(buffer[(x, y)].bg, TUI_YELLOW);
        assert_eq!(buffer[(x, y)].symbol(), "▕");
        assert_eq!(buffer[(unselected.x, unselected.y)].fg, OX_GRAY);
        let selected = geometry.bays[&Bay::Sled(3)];
        assert_eq!(buffer[(selected.x, selected.y)].bg, TUI_PURPLE);
        // "g2 ◌": the glyph carries its health colour, not the border's.
        let glyph = (unselected.x + 4, unselected.y);
        assert_eq!(buffer[glyph].symbol(), "◌");
        assert_eq!(buffer[glyph].fg, TUI_YELLOW);
        let empty = geometry.bays[&Bay::Sled(31)];
        assert_eq!(buffer[(empty.x + 1, empty.y + 1)].symbol(), " ");

        app.deployment.observed =
            crate::tui::reconcile::ObservedDeploymentState::Stopped;
        let stopped = render(&app, 60, 3 + 41);
        assert_eq!(stopped[(x, y)].bg, TUI_GREY_DARK);
    }

    #[test]
    fn short_areas_explain_how_to_see_the_rack() {
        let app = App::new(topology(), 4, 4);
        let text = text(&render(&app, 60, UPLINK_HEIGHT + 10));
        assert!(text.contains("Uplink"), "{text}");
        assert!(text.contains("Rack needs 20 rows"), "{text}");
    }

    #[test]
    fn column_width_tracks_the_rack_tier() {
        assert_eq!(column_width(0), RACK_MIN_HEIGHT + 4);
        assert_eq!(column_width(UPLINK_HEIGHT + 41), 28 + 4);
    }
}
