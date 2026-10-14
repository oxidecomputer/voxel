// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fixtures shared by the TUI's test modules.
//!
//! Everything here is built through the interfaces production code uses, the
//! way voxel-config's tests build configs through `VoxelConfig::from_toml`.
//! Fixtures that need a module's private state stay in that module's tests.

use crossterm::event::KeyCode;
use ratatui::{Frame, Terminal, backend::TestBackend, buffer::Buffer};
use voxel_config::VoxelConfig;

use super::telemetry::{
    BidirectionalRate, RackId, ResourceDescriptor, ResourceId, ResourceKind,
    TrafficSample, TrafficSource,
};

/// Parse a `voxel.toml` snippet, written as a user would write it.
pub(crate) fn config(toml: &str) -> VoxelConfig {
    VoxelConfig::from_toml(toml).expect("test config parses")
}

/// Sled `g{cubby}` in `rack`. Cubbies are numbered across the whole fleet, as
/// `resource_descriptors` numbers them.
pub(crate) fn sled(rack: usize, cubby: u8) -> ResourceDescriptor {
    let name = format!("g{cubby}");
    ResourceDescriptor {
        id: ResourceId::rack(RackId(rack), ResourceKind::Sled, &name),
        rack: Some(RackId(rack)),
        kind: ResourceKind::Sled,
        name,
        host: None,
        slot: Some(cubby),
    }
}

/// The switch zone in `slot`, hosted by scrimlet `host`.
pub(crate) fn switch(rack: usize, slot: u8, host: &str) -> ResourceDescriptor {
    ResourceDescriptor {
        id: ResourceId::rack(RackId(rack), ResourceKind::SwitchZone, host),
        rack: Some(RackId(rack)),
        kind: ResourceKind::SwitchZone,
        name: format!("switch{slot}"),
        host: Some(host.into()),
        slot: Some(slot),
    }
}

/// A fleet-scoped router VM.
pub(crate) fn router(name: &str) -> ResourceDescriptor {
    ResourceDescriptor {
        id: ResourceId::fleet(ResourceKind::Router, name),
        rack: None,
        kind: ResourceKind::Router,
        name: name.into(),
        host: None,
        slot: None,
    }
}

pub(crate) fn rate(rx_bytes_sec: f64, tx_bytes_sec: f64) -> BidirectionalRate {
    BidirectionalRate { rx_bytes_sec, tx_bytes_sec, ..Default::default() }
}

/// A traffic sample whose only nonzero figure is its received bytes.
pub(crate) fn sample(
    rx_bytes_sec: f64,
    source: TrafficSource,
) -> TrafficSample {
    TrafficSample {
        total: rate(rx_bytes_sec, 0.0),
        source,
        ..Default::default()
    }
}

/// The keys a footer or help label names, such as `"↑/↓"` or
/// `"PgUp / PgDn"`. An unknown label panics, so a new label cannot slip past
/// the checks that every advertised key is bound.
pub(crate) fn key_codes(label: &str) -> Vec<KeyCode> {
    label
        .split('/')
        .map(str::trim)
        .flat_map(|token| match token {
            // Arrows are written alone or run together, as in "↑↓←→".
            _ if !token.is_empty()
                && token.chars().all(|c| "↑↓←→".contains(c)) =>
            {
                token
                    .chars()
                    .map(|arrow| match arrow {
                        '↑' => KeyCode::Up,
                        '↓' => KeyCode::Down,
                        '←' => KeyCode::Left,
                        _ => KeyCode::Right,
                    })
                    .collect()
            }
            "Tab" => vec![KeyCode::Tab],
            "S-Tab" | "Shift-Tab" | "⇧" => vec![KeyCode::BackTab],
            "Enter" => vec![KeyCode::Enter],
            "Esc" => vec![KeyCode::Esc],
            "Space" => vec![KeyCode::Char(' ')],
            "Pg" => vec![KeyCode::PageUp, KeyCode::PageDown],
            "PgUp" => vec![KeyCode::PageUp],
            "PgDn" => vec![KeyCode::PageDown],
            _ => match token.chars().collect::<Vec<_>>()[..] {
                [key] => vec![KeyCode::Char(key)],
                _ => panic!("unknown key {token:?} in label {label:?}"),
            },
        })
        .collect()
}

/// Draw one frame into an in-memory terminal of the given size.
pub(crate) fn render(
    width: u16,
    height: u16,
    draw: impl FnOnce(&mut Frame<'_>),
) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(draw).unwrap();
    terminal.backend().buffer().clone()
}

/// The buffer's text, one line per row with trailing blanks trimmed. Rows stay
/// separate so a search cannot match text that only appears by running one
/// row into the next.
pub(crate) fn text(buffer: &Buffer) -> String {
    let area = buffer.area;
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
