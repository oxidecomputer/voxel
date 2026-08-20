# Voxel TUI operator guide

`voxel tui` is the private, persistent control and observability interface for
a Voxel deployment. The TUI supervises the public `voxel launch`, `voxel
route`, and `voxel destroy` commands as opaque child processes. Its displayed
phase transitions are advisory hints parsed from output, not lifecycle APIs or
proof of deployment state. It tracks deployment state through live probes
implemented by collector and telemetry interfaces.

## Usage

Build Voxel and prepare images as described in the [project
quickstart](../README.md#quickstart), then start the interface:

```console
$ cargo build -p voxel --bin voxel
$ pfexec ./target/debug/voxel tui
```

Logs are written to `<resolved workdir>/voxel-tui.log` and displayed in a
bounded on-screen history. The TUI can launch and destroy Voxel deployments,
and its session can attach to or detach from new or existing deployments. Use
`voxel tui resume` to resume the most recent detached session, or add `--choose`
to select one.

## Views and actions

The TUI has two alternate views, Deployment and Monitoring. Each view has a
number of foldable sections stacked within it. `Up` and `Down` move within
nested content and hand focus to the adjacent section at an effective edge;
section traversal wraps. `Tab` and `Shift-Tab` always move section focus, and
`Space` folds or expands the focused section.

| Key | Action |
|---|---|
| `1` / `2` | Switch between views |
| `Tab` / `Shift-Tab` | Next / previous top-level section |
| `Space` | Fold / expand the focused section |
| `Left` / `Right` | Previous / next rack in Rack Summary |
| `Up` / `Down` | Navigate nested content, handing off at an edge |
| `PageUp` / `PageDown` | Page nested content without changing section focus |
| `Enter` | Open the selected topology resource detail |
| `?` / `F1` | Open or close Help |
| `Esc` | Dismiss the topmost confirmation, Help, detail, or selection |
| `f` | Cycle the level filter while Deployment Logs is focused |
| `l` / `r` | Request launch / route |
| `c` / `x` | Cancel and leave resources / cancel and destroy |
| `d` | Detach and leave deployment resources in place |
| `y` | Copy the full fallback command from the detach dialog |
| `n` | Reject a confirmation |
| `q` | Quit, destroying resources first unless observed stopped |

### Deployment

The Deployment view tracks the launch and teardown of a Voxel deployment, and
contains **Overall Progress**, **Phases**, **Status**, **Current Phase**, and
**Logs** in separate sections. Live probes are used to track observed deployment
and route state.

Logs are displayed oldest to newest, with new entries edging older ones out of
the bounded view. Press `f` to filter them by level: All, Info, Warning, or
Error. The complete history remains in the durable log file.

### Monitoring

The Monitoring view exposes the state of a Voxel deployment, and contains
**Rack Summary**, **Topology**, and **Top Zones by Traffic** in separate
sections.
Rack Summary reports RSS readiness, aggregate RX/TX, health counts, and history.
Topology presents fleet routers, the shared switch-fabric bus, rack-local switch
zones, and sleds in the order of router → switch zone → sled.

The resource inspector sits beside the topology on wide layouts and below it on
compact layouts. Health summarizes sled-agent, maintenance services, zones, and
NTP probes for sleds, and traffic collection for routers and switch zones. The
inspector also shows the last successful probe, latest collection error,
RX/TX/total and packet rates, a 60-second sparkline, and zones. Missing samples
say collecting or unavailable rather than reporting zero. For the purposes of
a basic labelled implementation, traffic is considered normal at or below 100
KB/s, elevated above 100 KB/s through 5 MB/s, and high above 5 MB/s. **Top Zones
by Traffic** is a separate rack-wide ranking.

### Display

Wide terminals place topology and inspector side by side. Compact windows will
stack them and page resources rather than hiding labels, and prioritize focussed
sections while keeping folded sections minimized. Below `48x16`, the TUI
displays its minimum-size requirement instead of clipping an unsafe layout.

## Testing

```sh
cargo test -p voxel tui::app::tests -- --nocapture
cargo test -p voxel tui::terminal::tests -- --nocapture
cargo test -p voxel tui::ui::confirm_dialog::tests -- --nocapture
cargo test -p voxel tui::ui::widgets::tests -- --nocapture
cargo test -p voxel tui::effects::tests -- --nocapture
```
