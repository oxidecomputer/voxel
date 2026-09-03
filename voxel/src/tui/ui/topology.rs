use std::collections::BTreeMap;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::{
    colors::OX_GREEN_LIGHT,
    monitor::{health_style, resource_health_state},
    renderer::LayoutMode,
    widgets::{fit_terminal_width, selection_style, terminal_width},
};
use crate::tui::{
    App,
    telemetry::{HealthState, ResourceDescriptor, ResourceId, ResourceKind},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TopologyTier {
    Routers,
    SwitchZones,
    Sleds,
}

impl TopologyTier {
    const ALL: [Self; 3] = [Self::Routers, Self::SwitchZones, Self::Sleds];

    fn kind(self) -> ResourceKind {
        match self {
            Self::Routers => ResourceKind::Router,
            Self::SwitchZones => ResourceKind::SwitchZone,
            Self::Sleds => ResourceKind::Sled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OverflowRange {
    /// Half-open range in the tier's stable-ID ordered resources.
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TierScene {
    pub(crate) tier: TopologyTier,
    pub(crate) label_area: Rect,
    pub(crate) node_area: Rect,
    pub(crate) range_area: Option<Rect>,
    pub(crate) visible_ids: Vec<ResourceId>,
    pub(crate) overflow: OverflowRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectorCell {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) symbol: &'static str,
    /// The tier whose membership in the shared fabric this cell describes.
    pub(crate) tier: TopologyTier,
    /// Junctions are the only connector cells that intentionally occupy bus cells.
    pub(crate) fabric_junction: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FabricBus {
    pub(crate) area: Rect,
    pub(crate) symbol: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TopologyScene {
    pub(crate) node_rects: BTreeMap<ResourceId, Rect>,
    pub(crate) tiers: Vec<TierScene>,
    pub(crate) connectors: Vec<ConnectorCell>,
    pub(crate) fabric_buses: Vec<FabricBus>,
    pub(crate) dividers: Vec<Rect>,
    pub(crate) visible_ids: Vec<ResourceId>,
    pub(crate) navigation_order: Vec<ResourceId>,
}

pub(crate) fn draw(
    frame: &mut ratatui::Frame<'_>,
    scene: &TopologyScene,
    app: &App,
) {
    let structural = Style::default().fg(OX_GREEN_LIGHT);
    for tier in &scene.tiers {
        let (label, empty) = match tier.tier {
            TopologyTier::Routers => ("Routers", "No routers"),
            TopologyTier::SwitchZones => ("Switch zones", "No switch zones"),
            TopologyTier::Sleds => ("Sleds", "No sleds"),
        };
        if tier.label_area.area() > 0 {
            frame.render_widget(
                Paragraph::new(if tier.overflow.total == 0 {
                    empty
                } else {
                    label
                })
                .style(Style::default().add_modifier(Modifier::BOLD)),
                tier.label_area,
            );
        }
        if let Some(area) = tier.range_area {
            let (visible_start, visible_end) =
                if tier.overflow.start == tier.overflow.end {
                    (0, 0)
                } else {
                    (tier.overflow.start + 1, tier.overflow.end)
                };
            frame.render_widget(
                Paragraph::new(format!(
                    "{visible_start}-{visible_end} of {}",
                    tier.overflow.total
                ))
                .right_aligned(),
                area,
            );
        }
    }
    for bus in &scene.fabric_buses {
        frame.render_widget(
            Paragraph::new(bus.symbol.repeat(bus.area.width.into()))
                .style(structural),
            bus.area,
        );
        let label = " Switch fabric ";
        let label_width = terminal_width(label);
        if usize::from(bus.area.width) >= label_width + 2 {
            frame.render_widget(
                Paragraph::new(label).style(structural),
                Rect::new(bus.area.x + 2, bus.area.y, label_width as u16, 1),
            );
        }
    }
    for connector in &scene.connectors {
        frame.render_widget(
            Paragraph::new(connector.symbol).style(structural),
            Rect::new(connector.x, connector.y, 1, 1),
        );
    }
    for id in &scene.visible_ids {
        let Some(area) = scene.node_rects.get(id).copied() else {
            continue;
        };
        let Some(descriptor) =
            app.deployment.topology.iter().find(|item| item.id == *id)
        else {
            continue;
        };
        match descriptor.kind {
            ResourceKind::Router => {
                draw_router_node(frame, area, app, descriptor)
            }
            ResourceKind::SwitchZone => {
                draw_switch_zone_node(frame, area, app, descriptor)
            }
            ResourceKind::Sled => draw_sled_node(frame, area, app, descriptor),
        }
    }
}

fn draw_router_node(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    descriptor: &ResourceDescriptor,
) {
    draw_node_primitive(frame, area, app, descriptor, "RTR");
}

fn draw_switch_zone_node(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    descriptor: &ResourceDescriptor,
) {
    draw_node_primitive(frame, area, app, descriptor, "SWZ");
}

fn draw_sled_node(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    descriptor: &ResourceDescriptor,
) {
    draw_node_primitive(frame, area, app, descriptor, "SLD");
}

fn draw_node_primitive(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &App,
    descriptor: &ResourceDescriptor,
    kind: &str,
) {
    let selected =
        app.session.selected_resource.as_ref() == Some(&descriptor.id);
    let state = resource_health_state(app, &descriptor.id);
    let glyph = health_glyph(state);
    let pointer = if selected { "▶ " } else { "" };
    let bordered = area.height >= 3 && area.width >= 3;
    let title_width =
        usize::from(area.width.saturating_sub(u16::from(bordered) * 2));
    let reserved =
        terminal_width(pointer) + terminal_width(" ") + terminal_width(glyph);
    let identity = fit_terminal_width(
        &format!("{kind} {}", descriptor.name),
        title_width.saturating_sub(reserved),
    );
    let title = Line::from(vec![
        Span::styled(
            pointer,
            selection_style().add_modifier(Modifier::REVERSED),
        ),
        Span::styled(
            identity,
            if selected {
                selection_style().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            },
        ),
        Span::raw(" "),
        Span::styled(glyph, health_style(state)),
    ]);
    if bordered {
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(health_style(state))
                .title(title),
            area,
        );
    } else {
        frame.render_widget(Paragraph::new(title), area);
    }
}

fn health_glyph(state: HealthState) -> &'static str {
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

pub(crate) fn semantic_order(
    descriptors: &[ResourceDescriptor],
) -> Vec<ResourceId> {
    TopologyTier::ALL
        .into_iter()
        .flat_map(|tier| {
            let mut ids = tier_ids(descriptors, tier);
            ids.sort();
            ids
        })
        .collect()
}

fn empty_scene(descriptors: &[ResourceDescriptor]) -> TopologyScene {
    TopologyScene {
        node_rects: BTreeMap::new(),
        tiers: TopologyTier::ALL
            .into_iter()
            .map(|tier| TierScene {
                tier,
                label_area: Rect::default(),
                node_area: Rect::default(),
                range_area: None,
                visible_ids: Vec::new(),
                overflow: OverflowRange {
                    start: 0,
                    end: 0,
                    total: tier_ids(descriptors, tier).len(),
                },
            })
            .collect(),
        connectors: Vec::new(),
        fabric_buses: Vec::new(),
        dividers: Vec::new(),
        visible_ids: Vec::new(),
        navigation_order: semantic_order(descriptors),
    }
}

/// Builds render-ready topology geometry without reading or mutating application state.
pub(crate) fn layout_scene(
    area: Rect,
    mode: LayoutMode,
    descriptors: &[ResourceDescriptor],
    selected: Option<&ResourceId>,
    overflow_position: usize,
) -> TopologyScene {
    let mut scene = empty_scene(descriptors);
    // Rect's right/bottom are saturating. Rebuild from those effective bounds so
    // a raw overflowing origin/extent can never wrap subsequent coordinate math.
    let area = Rect::new(
        area.x,
        area.y,
        area.right().saturating_sub(area.x),
        area.bottom().saturating_sub(area.y),
    );
    if mode == LayoutMode::Minimum || area.width == 0 || area.height == 0 {
        return scene;
    }

    let selected_tier = selected.and_then(|id| {
        descriptors
            .iter()
            .any(|descriptor| descriptor.id == *id)
            .then(|| {
                TopologyTier::ALL
                    .into_iter()
                    .find(|tier| tier.kind() == id.kind)
            })
            .flatten()
    });
    let node_height = match mode {
        LayoutMode::Wide => 3_u16,
        LayoutMode::Compact => 1_u16,
        LayoutMode::Minimum => unreachable!(),
    };
    let full_height = 6_u32 + 3 * u32::from(node_height);
    let has_spine = area.width >= 3;
    let minimum_width = if mode == LayoutMode::Wide { 12 } else { 8 };
    let has_readable_content = has_spine && area.width - 2 >= minimum_width;

    // Reduced scenes spend every available cell on one truthful tier. In
    // particular a selected node wins over labels and decorative structure.
    if u32::from(area.height) < full_height || !has_readable_content {
        let tier = selected_tier
            .or_else(|| {
                TopologyTier::ALL.into_iter().find(|tier| {
                    descriptors.iter().any(|item| item.kind == tier.kind())
                })
            })
            .unwrap_or(TopologyTier::Routers);
        let mut ids = tier_ids(descriptors, tier);
        ids.sort();
        let chosen =
            selected.and_then(|id| ids.binary_search(id).ok()).or_else(|| {
                (!ids.is_empty()).then(|| overflow_position.min(ids.len() - 1))
            });
        let visible_ids =
            chosen.map(|index| vec![ids[index].clone()]).unwrap_or_default();
        place_nodes(area, &visible_ids, &mut scene.node_rects);
        scene.visible_ids.clone_from(&visible_ids);
        let metadata =
            scene.tiers.iter_mut().find(|item| item.tier == tier).unwrap();
        metadata.node_area = area;
        metadata.visible_ids = visible_ids;
        metadata.overflow = OverflowRange {
            start: chosen.unwrap_or(0),
            end: chosen.map_or(0, |index| index + 1),
            total: ids.len(),
        };
        return scene;
    }

    let spine_x = area.x;
    let content = Rect::new(area.x + 2, area.y, area.width - 2, area.height);
    let mut y = area.y;
    let mut lower_rails = Vec::new();
    for tier in TopologyTier::ALL {
        let label_row = Rect::new(content.x, y, content.width, 1);
        y += 1;
        let node_area = Rect::new(content.x, y, content.width, node_height);
        y += node_height;

        let mut ids = tier_ids(descriptors, tier);
        ids.sort();
        let capacity =
            usize::from(exact_capacity(content.width, minimum_width));
        let visible_count = ids.len().min(capacity);
        let max_start = ids.len().saturating_sub(visible_count);
        let start = selected
            .and_then(|id| ids.binary_search(id).ok())
            .map(|index| index.saturating_sub(visible_count / 2).min(max_start))
            .unwrap_or_else(|| overflow_position.min(max_start));
        let end = start + visible_count;
        let visible_ids = ids[start..end].to_vec();

        let (label_area, range_area) =
            split_label_row(label_row, ids.len() > visible_count);
        place_nodes(node_area, &visible_ids, &mut scene.node_rects);
        scene.visible_ids.extend(visible_ids.iter().cloned());

        if tier == TopologyTier::Routers {
            let bus_y = y;
            scene.fabric_buses.push(FabricBus {
                area: Rect::new(spine_x, bus_y, area.width, 1),
                symbol: "─",
            });
            for id in &visible_ids {
                let rect = scene.node_rects[id];
                scene.connectors.push(ConnectorCell {
                    x: rect.x + rect.width / 2,
                    y: bus_y,
                    symbol: "┴",
                    tier,
                    fabric_junction: true,
                });
            }
            y += 1;
        } else if !visible_ids.is_empty() {
            lower_rails.push((tier, y, visible_ids.clone()));
        }
        if tier != TopologyTier::Routers {
            y += 1;
        }

        let tier_scene = scene
            .tiers
            .iter_mut()
            .find(|metadata| metadata.tier == tier)
            .expect("all tiers have metadata");
        *tier_scene = TierScene {
            tier,
            label_area,
            node_area,
            range_area,
            visible_ids,
            overflow: OverflowRange { start, end, total: ids.len() },
        };
    }

    let bus_y = scene.fabric_buses[0].area.y;
    let last_rail = lower_rails.last().map_or(bus_y, |(_, rail_y, _)| *rail_y);
    if let Some((tier, _, _)) = lower_rails.first() {
        scene.connectors.push(ConnectorCell {
            x: spine_x,
            y: bus_y,
            symbol: "┬",
            tier: *tier,
            fabric_junction: true,
        });
    }
    for connector_y in bus_y + 1..=last_rail {
        let at_rail =
            lower_rails.iter().find(|(_, rail_y, _)| *rail_y == connector_y);
        let tier = at_rail.map_or(TopologyTier::Sleds, |(tier, _, _)| *tier);
        let is_last_rail = connector_y == last_rail;
        scene.connectors.push(ConnectorCell {
            x: spine_x,
            y: connector_y,
            symbol: if at_rail.is_some() {
                if is_last_rail { "└" } else { "├" }
            } else {
                "│"
            },
            tier,
            fabric_junction: false,
        });
        if let Some((tier, _, ids)) = at_rail {
            let centers: Vec<u16> = ids
                .iter()
                .map(|id| {
                    let rect = scene.node_rects[id];
                    rect.x + rect.width / 2
                })
                .collect();
            let rightmost = centers.iter().copied().max().unwrap_or(spine_x);
            for x in spine_x + 1..=rightmost {
                scene.connectors.push(ConnectorCell {
                    x,
                    y: connector_y,
                    symbol: if centers.contains(&x) { "┴" } else { "─" },
                    tier: *tier,
                    fabric_junction: false,
                });
            }
        }
    }
    scene
}

fn tier_ids(
    descriptors: &[ResourceDescriptor],
    tier: TopologyTier,
) -> Vec<ResourceId> {
    descriptors
        .iter()
        .filter(|descriptor| descriptor.kind == tier.kind())
        .map(|descriptor| descriptor.id.clone())
        .collect()
}

fn exact_capacity(width: u16, minimum_width: u16) -> u16 {
    if width < minimum_width {
        0
    } else {
        ((u32::from(width) + 1) / (u32::from(minimum_width) + 1)) as u16
    }
}

fn split_label_row(row: Rect, overflowing: bool) -> (Rect, Option<Rect>) {
    if !overflowing || row.width < 2 || row.height == 0 {
        return (row, None);
    }
    let range_width = row.width.min(12).min(row.width - 1);
    (
        Rect::new(row.x, row.y, row.width - range_width, 1),
        Some(Rect::new(row.right() - range_width, row.y, range_width, 1)),
    )
}

fn place_nodes(
    area: Rect,
    ids: &[ResourceId],
    output: &mut BTreeMap<ResourceId, Rect>,
) {
    if ids.is_empty() || area.area() == 0 {
        return;
    }
    let count = ids.len() as u16;
    let gaps = count.saturating_sub(1);
    let gap =
        u16::from(area.width >= count.saturating_mul(2).saturating_sub(1));
    let usable = area.width.saturating_sub(gaps * gap);
    let base = usable / count;
    let extra = usable % count;
    let mut x = area.x;
    for (index, id) in ids.iter().enumerate() {
        let width = base + u16::from((index as u16) < extra);
        if width > 0 {
            output.insert(id.clone(), Rect::new(x, area.y, width, area.height));
        }
        x = x.saturating_add(width + gap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tui::reconcile::ObservedDeploymentState,
        tui::{
            event::AppEvent,
            telemetry::{
                HealthDiagnostic, NtpDiagnostic, RackId, ServiceState,
            },
        },
    };
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use std::time::{Duration, Instant};

    fn descriptor(kind: ResourceKind, name: &str) -> ResourceDescriptor {
        let rack = (kind != ResourceKind::Router).then_some(RackId(0));
        ResourceDescriptor {
            id: rack.map_or_else(
                || ResourceId::fleet(kind, name),
                |rack| ResourceId::rack(rack, kind, name),
            ),
            rack,
            kind,
            name: name.into(),
            host: None,
        }
    }

    fn resources() -> Vec<ResourceDescriptor> {
        [
            (ResourceKind::Sled, "sled界"),
            (ResourceKind::Router, "routere\u{301}"),
            (ResourceKind::SwitchZone, "switch"),
        ]
        .into_iter()
        .flat_map(|(kind, prefix)| {
            (0..12)
                .rev()
                .map(move |i| descriptor(kind, &format!("{prefix}-{i:02}")))
        })
        .collect()
    }

    fn cells(rect: Rect) -> impl Iterator<Item = (u16, u16)> {
        (rect.y..rect.bottom())
            .flat_map(move |y| (rect.x..rect.right()).map(move |x| (x, y)))
    }

    #[test]
    fn every_selected_health_state_keeps_interaction_and_semantic_styles_local()
    {
        let cases = [
            (HealthState::Healthy, "●", super::super::colors::TUI_GREEN),
            (HealthState::Checking, "◌", super::super::colors::TUI_YELLOW),
            (HealthState::Degraded, "!", super::super::colors::OX_RED),
            (HealthState::Failed, "!", super::super::colors::OX_RED),
            (HealthState::Stale, "◐", super::super::colors::TUI_GREY),
            (HealthState::Unknown, "?", super::super::colors::TUI_GREY_DARK),
            (
                HealthState::Unavailable,
                "×",
                super::super::colors::TUI_GREY_DARK,
            ),
            (HealthState::Stopped, "■", super::super::colors::TUI_GREY_DARK),
        ];
        for (wanted, glyph, color) in cases {
            let selected = descriptor(ResourceKind::Sled, "selected");
            let mut app = App::new(vec![selected.clone()], 4, 4);
            app.session.selected_resource = Some(selected.id.clone());
            app.deployment.observed = if wanted == HealthState::Stopped {
                ObservedDeploymentState::Stopped
            } else {
                ObservedDeploymentState::Running
            };
            let now = Instant::now();
            app.update(AppEvent::Tick { now });
            let diagnostic = match wanted {
                HealthState::Healthy | HealthState::Stale => {
                    Some(HealthDiagnostic {
                        sled_agent: Some(ServiceState::Online),
                        ntp: NtpDiagnostic {
                            synchronized: Some(true),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                }
                HealthState::Degraded => Some(HealthDiagnostic {
                    failed_services: vec!["nexus".into()],
                    ..Default::default()
                }),
                HealthState::Failed => Some(HealthDiagnostic {
                    sled_agent: Some(ServiceState::Offline),
                    ..Default::default()
                }),
                HealthState::Unknown => Some(HealthDiagnostic::default()),
                _ => None,
            };
            if let Some(diagnostic) = diagnostic {
                app.update(AppEvent::Health {
                    id: selected.id.clone(),
                    at: if wanted == HealthState::Stale {
                        now - Duration::from_secs(61)
                    } else {
                        now
                    },
                    diagnostic,
                });
            }
            if wanted == HealthState::Unavailable {
                app.observability.health.remove(&selected.id);
            }
            assert_eq!(resource_health_state(&app, &selected.id), wanted);
            let scene = layout_scene(
                Rect::new(0, 0, 40, 15),
                LayoutMode::Wide,
                std::slice::from_ref(&selected),
                Some(&selected.id),
                0,
            );
            let rect = scene.node_rects[&selected.id];
            let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
            terminal.draw(|frame| draw(frame, &scene, &app)).unwrap();
            let buffer = terminal.backend().buffer();
            let selected_cells = cells(rect)
                .filter_map(|position| {
                    let cell = &buffer[position];
                    (cell.fg == super::super::colors::TUI_PURPLE)
                        .then_some(cell)
                })
                .collect::<Vec<_>>();
            assert!(selected_cells.iter().any(|cell| cell.symbol() == "▶"));
            assert!(selected_cells.iter().any(|cell| cell.symbol() == "S"));
            assert!(selected_cells.iter().all(|cell| {
                cell.modifier.contains(Modifier::BOLD)
                    && cell.modifier.contains(Modifier::REVERSED)
            }));
            assert!(cells(rect).any(|position| {
                buffer[position].symbol() == glyph
                    && buffer[position].fg == color
            }));
            assert_eq!(buffer[(rect.x, rect.bottom() - 1)].fg, color);
            assert_ne!(color, Color::Reset);
        }
    }

    #[test]
    fn every_artifact_is_bounded_and_disjoint_except_explicit_bus_junctions() {
        for (area, mode) in [
            (Rect::new(3, 2, 160, 20), LayoutMode::Wide),
            (Rect::new(3, 2, 60, 10), LayoutMode::Compact),
            (Rect::new(3, 2, 48, 6), LayoutMode::Compact),
            (Rect::new(9, 11, 7, 3), LayoutMode::Compact),
            (Rect::new(65_500, 65_520, 35, 15), LayoutMode::Compact),
        ] {
            let scene = layout_scene(area, mode, &resources(), None, 4);
            assert!(scene.node_rects.values().all(|rect| rect.area() > 0));
            assert_geometry(&scene, area);
            assert_connector_continuity(&scene);
            assert!(scene.fabric_buses.len() <= 1);
        }
    }

    fn assert_geometry(scene: &TopologyScene, area: Rect) {
        let mut occupied: BTreeMap<(u16, u16), &'static str> = BTreeMap::new();
        let mut claim = |rect: Rect, kind: &'static str| {
            assert!(rect.x >= area.x && rect.y >= area.y);
            assert!(
                rect.right() <= area.right() && rect.bottom() <= area.bottom()
            );
            for cell in cells(rect) {
                assert!(
                    occupied.insert(cell, kind).is_none(),
                    "{kind} collision {cell:?}"
                );
            }
        };
        for tier in &scene.tiers {
            if tier.label_area.area() > 0 {
                claim(tier.label_area, "label");
            }
            if let Some(rect) = tier.range_area {
                claim(rect, "range");
            }
        }
        for rect in scene.node_rects.values() {
            claim(*rect, "node");
        }
        for rect in &scene.dividers {
            claim(*rect, "divider");
        }
        for cell in scene.connectors.iter().filter(|cell| !cell.fabric_junction)
        {
            claim(Rect::new(cell.x, cell.y, 1, 1), "connector");
        }
        for bus in &scene.fabric_buses {
            claim(bus.area, "bus");
        }
        let mut junctions = BTreeMap::new();
        for cell in scene.connectors.iter().filter(|cell| cell.fabric_junction)
        {
            assert_eq!(occupied.get(&(cell.x, cell.y)), Some(&"bus"));
            assert!(junctions.insert((cell.x, cell.y), cell.tier).is_none());
        }
    }

    fn assert_connector_continuity(scene: &TopologyScene) {
        let Some(bus) = scene.fabric_buses.first() else {
            assert!(scene.connectors.is_empty());
            return;
        };
        let router = &scene.tiers[0];
        for id in &router.visible_ids {
            let node = scene.node_rects[id];
            let center = node.x + node.width / 2;
            assert_eq!(node.bottom(), bus.area.y);
            assert!(scene.connectors.iter().any(|cell| {
                cell.x == center
                    && cell.y == bus.area.y
                    && cell.tier == TopologyTier::Routers
                    && cell.fabric_junction
            }));
        }

        let populated_lower: Vec<&TierScene> = scene
            .tiers
            .iter()
            .skip(1)
            .filter(|tier| !tier.visible_ids.is_empty())
            .collect();
        if populated_lower.is_empty() {
            assert!(!scene.connectors.iter().any(|cell| cell.x == bus.area.x
                && cell.tier != TopologyTier::Routers));
            return;
        }

        assert!(scene.connectors.iter().any(|cell| {
            cell.x == bus.area.x && cell.y == bus.area.y && cell.fabric_junction
        }));
        let last_rail_y =
            populated_lower.last().expect("nonempty").node_area.bottom();
        for y in bus.area.y + 1..=last_rail_y {
            assert!(
                scene
                    .connectors
                    .iter()
                    .any(|cell| cell.x == bus.area.x && cell.y == y)
            );
        }
        for tier in populated_lower {
            let rail_y = tier.node_area.bottom();
            assert!(bus.area.y < tier.label_area.y);
            for id in &tier.visible_ids {
                let node = scene.node_rects[id];
                let center = node.x + node.width / 2;
                assert_eq!(node.bottom(), rail_y);
                assert!(scene.connectors.iter().any(|cell| {
                    cell.x == center
                        && cell.y == rail_y
                        && cell.tier == tier.tier
                        && cell.symbol == "┴"
                        && !cell.fabric_junction
                }));
                for x in bus.area.x..=center {
                    assert!(
                        scene
                            .connectors
                            .iter()
                            .any(|cell| cell.x == x && cell.y == rail_y)
                    );
                }
            }
        }
    }

    #[test]
    fn maximal_dimensions_do_not_overflow_geometry_arithmetic() {
        for area in
            [Rect::new(0, 0, u16::MAX, 20), Rect::new(0, 0, 40, u16::MAX)]
        {
            let scene = layout_scene(
                area,
                LayoutMode::Wide,
                &resources(),
                None,
                usize::MAX,
            );
            assert!(scene.fabric_buses.len() <= 1);
            assert_geometry(&scene, area);
            assert_connector_continuity(&scene);
        }
    }

    #[test]
    fn overflowing_raw_rect_extents_are_normalized_before_layout() {
        let area = Rect::new(u16::MAX - 5, u16::MAX - 5, 20, 20);
        let descriptors = resources();
        let selected = descriptors
            .iter()
            .find(|item| item.kind == ResourceKind::Sled)
            .unwrap()
            .id
            .clone();
        let scene = layout_scene(
            area,
            LayoutMode::Compact,
            &descriptors,
            Some(&selected),
            0,
        );
        assert!(scene.visible_ids.contains(&selected));
        assert_geometry(&scene, area);

        let effectively_empty = layout_scene(
            Rect::new(u16::MAX, 3, 1, 10),
            LayoutMode::Compact,
            &descriptors,
            Some(&selected),
            0,
        );
        assert!(effectively_empty.visible_ids.is_empty());
        assert!(effectively_empty.node_rects.is_empty());
    }

    #[test]
    fn near_maximum_origin_still_allocates_the_complete_scene() {
        let area = Rect::new(65_500, 65_520, 35, 15);
        let scene =
            layout_scene(area, LayoutMode::Compact, &resources(), None, 0);
        assert_eq!(scene.fabric_buses.len(), 1);
        assert!(scene.dividers.is_empty());
        assert!(scene.tiers.iter().all(|tier| tier.label_area.area() > 0));
        assert!(scene.node_rects.len() >= 3);
        assert!(scene.connectors.iter().any(|cell| !cell.fabric_junction));
        assert_eq!(
            scene.connectors.iter().filter(|cell| cell.fabric_junction).count(),
            scene.tiers[0].visible_ids.len()
                + usize::from(
                    scene
                        .tiers
                        .iter()
                        .skip(1)
                        .any(|tier| !tier.visible_ids.is_empty())
                )
        );
        assert_geometry(&scene, area);
        assert_connector_continuity(&scene);
    }

    #[test]
    fn empty_and_partial_tiers_keep_explicit_metadata() {
        let cases = [
            Vec::new(),
            vec![descriptor(ResourceKind::Router, "router")],
            vec![descriptor(ResourceKind::SwitchZone, "switch")],
            vec![descriptor(ResourceKind::Sled, "sled")],
            vec![
                descriptor(ResourceKind::Router, "router"),
                descriptor(ResourceKind::Sled, "sled"),
            ],
            vec![
                descriptor(ResourceKind::SwitchZone, "switch"),
                descriptor(ResourceKind::Sled, "sled"),
            ],
        ];
        for descriptors in cases {
            let scene = layout_scene(
                Rect::new(2, 3, 80, 20),
                LayoutMode::Wide,
                &descriptors,
                None,
                0,
            );
            assert_eq!(scene.tiers.len(), 3);
            for tier in TopologyTier::ALL {
                let metadata =
                    scene.tiers.iter().find(|item| item.tier == tier).unwrap();
                let expected_ids = tier_ids(&descriptors, tier);
                assert_eq!(metadata.overflow.total, expected_ids.len());
                assert_eq!(metadata.overflow.start, 0);
                assert_eq!(metadata.overflow.end, expected_ids.len());
                assert_eq!(metadata.visible_ids, expected_ids);
                for id in expected_ids {
                    assert!(scene.node_rects.contains_key(&id));
                }
            }
            assert_eq!(scene.fabric_buses.len(), 1);
            assert_geometry(&scene, Rect::new(2, 3, 80, 20));
            assert_connector_continuity(&scene);
        }
    }

    #[test]
    fn nonexistent_selection_does_not_displace_semantic_tier_priority() {
        let descriptors = resources();
        let missing =
            ResourceId::rack(RackId(0), ResourceKind::Sled, "missing");
        let scene = layout_scene(
            Rect::new(0, 0, 48, 3),
            LayoutMode::Compact,
            &descriptors,
            Some(&missing),
            0,
        );
        assert!(!scene.tiers[0].visible_ids.is_empty());
        assert!(scene.tiers[2].visible_ids.is_empty());
    }

    #[test]
    fn selected_tier_is_prioritized_and_zero_area_is_truthful() {
        let descriptors = resources();
        let selected = descriptors
            .iter()
            .find(|d| d.kind == ResourceKind::Sled)
            .unwrap()
            .id
            .clone();
        for area in [Rect::new(0, 0, 48, 6), Rect::new(4, 7, 2, 1)] {
            let scene = layout_scene(
                area,
                LayoutMode::Compact,
                &descriptors,
                Some(&selected),
                usize::MAX,
            );
            assert!(scene.visible_ids.contains(&selected));
            assert!(scene.node_rects.contains_key(&selected));
        }
        for area in [Rect::new(0, 0, 0, 8), Rect::new(0, 0, 8, 0)] {
            let scene = layout_scene(
                area,
                LayoutMode::Compact,
                &descriptors,
                Some(&selected),
                0,
            );
            assert!(
                scene.visible_ids.is_empty() && scene.node_rects.is_empty()
            );
        }
    }

    #[test]
    fn minimum_is_empty_but_retains_safe_metadata() {
        let scene = layout_scene(
            Rect::new(5, 8, 160, 20),
            LayoutMode::Minimum,
            &resources(),
            None,
            0,
        );
        assert!(scene.visible_ids.is_empty());
        assert!(
            scene.node_rects.is_empty()
                && scene.connectors.is_empty()
                && scene.fabric_buses.is_empty()
        );
        assert_eq!(scene.tiers.len(), 3);
        assert!(scene.tiers.iter().all(|tier| tier.overflow.end == 0));
    }

    #[test]
    fn reduced_empty_scene_has_no_selection_or_overflow() {
        let scene = layout_scene(
            Rect::new(0, 0, 48, 3),
            LayoutMode::Compact,
            &[],
            None,
            7,
        );

        assert!(scene.visible_ids.is_empty());
        assert!(scene.node_rects.is_empty());
        assert!(scene.tiers.iter().all(|tier| {
            tier.overflow.start == 0
                && tier.overflow.end == 0
                && tier.overflow.total == 0
        }));
    }

    #[test]
    fn wide_and_compact_are_distinct_and_exact_fit_is_used() {
        let descriptors = resources();
        let wide = layout_scene(
            Rect::new(0, 0, 27, 20), // spine + gap + (12 + gap + 12)
            LayoutMode::Wide,
            &descriptors,
            None,
            0,
        );
        let compact = layout_scene(
            Rect::new(0, 0, 19, 20), // spine + gap + (8 + gap + 8)
            LayoutMode::Compact,
            &descriptors,
            None,
            0,
        );
        assert_eq!(wide.tiers[0].visible_ids.len(), 2); // 12 + one gap + 12
        assert_eq!(compact.tiers[0].visible_ids.len(), 2);
        assert!(
            wide.tiers[0].node_area.height > compact.tiers[0].node_area.height
        );
    }

    #[test]
    fn capacity_respects_minimum_width_and_gap_boundaries() {
        for minimum_width in [8, 12] {
            assert_eq!(exact_capacity(minimum_width - 1, minimum_width), 0);
            assert_eq!(exact_capacity(minimum_width, minimum_width), 1);
            assert_eq!(exact_capacity(2 * minimum_width, minimum_width), 1);
            assert_eq!(exact_capacity(2 * minimum_width + 1, minimum_width), 2);
        }

        let descriptors = resources();
        let selected = descriptors
            .iter()
            .find(|item| item.kind == ResourceKind::SwitchZone)
            .unwrap()
            .id
            .clone();
        let scene = layout_scene(
            Rect::new(0, 0, 9, 20), // 7 content columns: below compact minimum.
            LayoutMode::Compact,
            &descriptors,
            Some(&selected),
            0,
        );
        assert_eq!(scene.visible_ids, vec![selected.clone()]);
        assert_eq!(scene.node_rects[&selected], Rect::new(0, 0, 9, 20));
        assert!(scene.fabric_buses.is_empty() && scene.connectors.is_empty());
    }

    #[test]
    fn overflow_ranges_are_truthful_and_semantic_order_is_stable() {
        let descriptors = resources();
        let scene = layout_scene(
            Rect::new(7, 9, 60, 20),
            LayoutMode::Wide,
            &descriptors,
            None,
            99,
        );
        for tier in &scene.tiers {
            assert_eq!(
                tier.overflow.end - tier.overflow.start,
                tier.visible_ids.len()
            );
            assert!(
                tier.overflow.start <= tier.overflow.end
                    && tier.overflow.end <= tier.overflow.total
            );
        }
        let mut expected = Vec::new();
        for kind in
            [ResourceKind::Router, ResourceKind::SwitchZone, ResourceKind::Sled]
        {
            let mut ids: Vec<_> = descriptors
                .iter()
                .filter(|descriptor| descriptor.kind == kind)
                .map(|descriptor| descriptor.id.clone())
                .collect();
            ids.sort();
            expected.extend(ids);
        }
        assert_eq!(scene.navigation_order, expected);
        // Unicode names deliberately participate only as stable IDs; Task 3 fits display text.
    }
}
