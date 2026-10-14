// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Terminal plumbing: the raw-mode and alternate-screen lifecycle on
//! `/dev/tty`, key mapping, the input and tick tasks, and OSC 52 clipboard
//! writes.

use std::{
    io::{self, Write},
    panic::{self, PanicHookInfo},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::Duration,
};

use base64::Engine;

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::tui::{
    event::{Action, AppEvent},
    logging::DurableLog,
};

pub type TuiTerminal = Terminal<CrosstermBackend<io::BufWriter<PanicGate>>>;

pub const DEFAULT_EVENT_CAPACITY: usize = 64;
const INPUT_POLL: Duration = Duration::from_millis(50);
const SEND_RETRY: Duration = Duration::from_millis(5);

pub fn event_channel() -> (mpsc::Sender<AppEvent>, mpsc::Receiver<AppEvent>) {
    event_channel_with_capacity(DEFAULT_EVENT_CAPACITY)
}

pub fn event_channel_with_capacity(
    capacity: usize,
) -> (mpsc::Sender<AppEvent>, mpsc::Receiver<AppEvent>) {
    assert!(capacity > 0, "event channel capacity must be nonzero");
    mpsc::channel(capacity)
}

pub fn key_action(key: KeyEvent) -> Option<Action> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    Some(match key.code {
        KeyCode::Char('v') => Action::ToggleView,
        KeyCode::Tab => Action::NextSection,
        KeyCode::BackTab => Action::PreviousSection,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Enter => Action::Activate,
        KeyCode::Char(' ') => Action::ToggleSection,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char('s') => Action::CopyExternalMonitoringYaml,
        KeyCode::Char('a') => Action::CopyExternalMonitoringGuide,
        KeyCode::Up => Action::Scroll { delta: -1, page: false },
        KeyCode::Down => Action::Scroll { delta: 1, page: false },
        KeyCode::PageUp => Action::Scroll { delta: -1, page: true },
        KeyCode::PageDown => Action::Scroll { delta: 1, page: true },
        KeyCode::Char('f') => Action::CycleLogFilter,
        KeyCode::Esc => Action::Close,
        KeyCode::Char('y') => Action::CopyReattachCommand,
        KeyCode::Char('n') => Action::Reject,
        KeyCode::Char('l') => Action::RequestLaunch,
        KeyCode::Char('r') => Action::RequestRoute,
        KeyCode::Char('d') => Action::RequestDetach,
        KeyCode::Char('q') => Action::RequestQuit,
        KeyCode::Char('x') => Action::RequestCancelAndDestroy,
        _ => return None,
    })
}

pub fn osc52(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\u{1b}]52;c;{encoded}\u{7}")
}

pub fn write_osc52(mut writer: impl io::Write, text: &str) -> io::Result<()> {
    writer.write_all(osc52(text).as_bytes())?;
    writer.flush()
}

fn handoff(
    tx: &mpsc::Sender<AppEvent>,
    cancel: &CancellationToken,
    mut event: AppEvent,
) -> bool {
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(SEND_RETRY);
            }
        }
    }
}

trait EventSource {
    fn next(&mut self, wait: Duration) -> io::Result<Option<Event>>;
}
struct CrosstermEvents;
impl EventSource for CrosstermEvents {
    fn next(&mut self, wait: Duration) -> io::Result<Option<Event>> {
        if event::poll(wait)? { event::read().map(Some) } else { Ok(None) }
    }
}
fn input_loop(
    mut source: impl EventSource,
    tx: mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) -> io::Result<()> {
    while !cancel.is_cancelled() {
        if tx.is_closed() {
            break;
        }
        let message = match source.next(INPUT_POLL)? {
            Some(Event::Key(key)) => key_action(key).map(AppEvent::Action),
            Some(Event::Resize(width, height)) => {
                Some(AppEvent::Resize { width, height })
            }
            _ => None,
        };
        if message.is_some_and(|message| !handoff(&tx, &cancel, message)) {
            break;
        }
    }
    Ok(())
}
pub fn spawn_input(
    tx: mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
) -> JoinHandle<io::Result<()>> {
    tokio::task::spawn_blocking(move || input_loop(CrosstermEvents, tx, cancel))
}

pub fn spawn_ticks(
    tx: mpsc::Sender<AppEvent>,
    cancel: CancellationToken,
    period: Duration,
) -> io::Result<JoinHandle<()>> {
    if period.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tick period must be nonzero",
        ));
    }
    Ok(tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let send = tx.send(AppEvent::Tick { now: std::time::Instant::now() });
                    tokio::select! { _ = cancel.cancelled() => break, result = send => if result.is_err() { break } }
                }
            }
        }
    }))
}

trait LifecycleOps {
    fn raw(&mut self) -> io::Result<()>;
    fn enter(&mut self) -> io::Result<()>;
    fn hide(&mut self) -> io::Result<()>;
    fn initialize(&mut self) -> io::Result<()>;
    fn show(&mut self) -> io::Result<()>;
    fn leave(&mut self) -> io::Result<()>;
    fn disable(&mut self) -> io::Result<()>;
}
#[derive(Default)]
struct Lifecycle {
    raw: bool,
    alternate: bool,
    hidden: bool,
}
impl Lifecycle {
    fn setup(ops: &mut impl LifecycleOps) -> io::Result<Self> {
        let mut state = Self::default();
        ops.raw()?;
        state.raw = true;
        if let Err(e) = ops.enter() {
            let _ = state.restore(ops);
            return Err(e);
        }
        state.alternate = true;
        if let Err(e) = ops.hide() {
            let _ = state.restore(ops);
            return Err(e);
        }
        state.hidden = true;
        if let Err(e) = ops.initialize() {
            let _ = state.restore(ops);
            return Err(e);
        }
        Ok(state)
    }
    fn restore(&mut self, ops: &mut impl LifecycleOps) -> io::Result<()> {
        let mut first = None;
        if self.hidden {
            match ops.show() {
                Ok(()) => self.hidden = false,
                Err(e) => first = Some(e),
            }
        }
        if self.alternate {
            match ops.leave() {
                Ok(()) => self.alternate = false,
                Err(e) => {
                    if first.is_none() {
                        first = Some(e);
                    }
                }
            }
        }
        if self.raw {
            match ops.disable() {
                Ok(()) => self.raw = false,
                Err(e) => {
                    if first.is_none() {
                        first = Some(e);
                    }
                }
            }
        }
        first.map_or(Ok(()), Err)
    }
}
struct RealOps {
    fallback: io::BufWriter<std::fs::File>,
    backend_writer: Option<io::BufWriter<PanicGate>>,
    terminal: Option<TuiTerminal>,
}

/// The renderer's writer, which discards output once the panic hook has
/// handed the terminal back. A frame drawn between a panic and the render loop
/// observing shutdown would otherwise repaint over the panic message.
pub struct PanicGate {
    tty: std::fs::File,
    released: Released,
}
impl Write for PanicGate {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Held across the write so the hook cannot restore the terminal
        // between this check and the bytes reaching it.
        let released = released(&self.released);
        if *released { Ok(buf.len()) } else { self.tty.write(buf) }
    }
    fn flush(&mut self) -> io::Result<()> {
        let released = released(&self.released);
        if *released { Ok(()) } else { self.tty.flush() }
    }
}

/// Whether the panic hook has taken the terminal back from the renderer.
type Released = Arc<Mutex<bool>>;

fn released(state: &Released) -> MutexGuard<'_, bool> {
    // Nothing panics while holding the lock, but a panic hook must not
    // panic over a poisoned one either.
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

/// Restores the terminal before the previous hook prints. The default hook
/// runs before unwinding reaches [`TerminalSession`]'s `Drop`, and a panic in
/// a spawned task never unwinds the UI at all, so without this the message
/// lands in the alternate screen in raw mode. Cancelling `shutdown` makes the
/// TUI exit through its usual failure path instead of running on with no
/// screen; the durable log keeps the message for detached sessions.
fn install_panic_hook(
    tty: std::fs::File,
    state: Released,
    shutdown: CancellationToken,
    log: DurableLog,
) -> Arc<PanicHook> {
    let previous = Arc::new(panic::take_hook());
    let chained = previous.clone();
    panic::set_hook(Box::new(move |info| {
        {
            let mut released = released(&state);
            if !*released {
                *released = true;
                // Errors are ignored: failing here would lose the panic.
                let _ = execute!(&mut &tty, Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
            }
        }
        shutdown.cancel();
        let _ = log.write_line_within(
            &format!("ERROR TUI {info}"),
            Duration::from_millis(100),
        );
        chained(info);
    }));
    previous
}
impl LifecycleOps for RealOps {
    fn raw(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }
    fn enter(&mut self) -> io::Result<()> {
        execute!(self.fallback, EnterAlternateScreen)
    }
    fn hide(&mut self) -> io::Result<()> {
        execute!(self.fallback, Hide)
    }
    fn initialize(&mut self) -> io::Result<()> {
        let writer = self.backend_writer.take().expect("unused backend writer");
        let mut t = Terminal::new(CrosstermBackend::new(writer))?;
        t.clear()?;
        self.terminal = Some(t);
        Ok(())
    }
    fn show(&mut self) -> io::Result<()> {
        if let Some(t) = &mut self.terminal {
            execute!(t.backend_mut(), Show)
        } else {
            execute!(self.fallback, Show)
        }
    }
    fn leave(&mut self) -> io::Result<()> {
        if let Some(t) = &mut self.terminal {
            execute!(t.backend_mut(), LeaveAlternateScreen)
        } else {
            execute!(self.fallback, LeaveAlternateScreen)
        }
    }
    fn disable(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
}
pub struct TerminalSession {
    terminal: Option<TuiTerminal>,
    fallback: Option<io::BufWriter<std::fs::File>>,
    lifecycle: Lifecycle,
    released: Released,
    /// The hook [`install_panic_hook`] replaced, until it is reinstated.
    previous_hook: Option<Arc<PanicHook>>,
}
impl TerminalSession {
    /// Takes over `writer` until [`restore`](Self::restore). A panic on any
    /// thread in the meantime restores the terminal, cancels `shutdown`, and
    /// records the panic in `log`.
    pub fn enter(
        writer: std::fs::File,
        shutdown: CancellationToken,
        log: DurableLog,
    ) -> io::Result<Self> {
        let fallback = io::BufWriter::new(writer.try_clone()?);
        let hook_tty = writer.try_clone()?;
        let released = Released::default();
        let mut ops = RealOps {
            fallback,
            backend_writer: Some(io::BufWriter::new(PanicGate {
                tty: writer,
                released: released.clone(),
            })),
            terminal: None,
        };
        let lifecycle = Lifecycle::setup(&mut ops)?;
        let previous_hook =
            Some(install_panic_hook(hook_tty, released.clone(), shutdown, log));
        Ok(Self {
            terminal: ops.terminal,
            fallback: Some(ops.fallback),
            lifecycle,
            released,
            previous_hook,
        })
    }
    pub fn terminal_mut(&mut self) -> &mut TuiTerminal {
        self.terminal.as_mut().expect("active terminal")
    }
    /// Whether a panic has taken the terminal back from the renderer.
    pub fn panicked(&self) -> bool {
        *released(&self.released)
    }
    pub fn restore(&mut self) -> io::Result<()> {
        let mut ops = RealOps {
            fallback: self.fallback.take().expect("terminal fallback writer"),
            backend_writer: None,
            terminal: self.terminal.take(),
        };
        let result = self.lifecycle.restore(&mut ops);
        self.terminal = ops.terminal;
        self.fallback = Some(ops.fallback);
        // set_hook panics on a panicking thread, so while unwinding the
        // restoring hook stays installed for the rest of the process.
        if !thread::panicking()
            && let Some(previous) = self.previous_hook.take()
        {
            panic::set_hook(Box::new(move |info| previous(info)));
        }
        result
    }
}
impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn every_documented_key_maps_and_nothing_else_acts() {
        let cases = [
            (KeyCode::Char('v'), Some(Action::ToggleView)),
            (KeyCode::Tab, Some(Action::NextSection)),
            (KeyCode::BackTab, Some(Action::PreviousSection)),
            (KeyCode::Left, Some(Action::Left)),
            (KeyCode::Right, Some(Action::Right)),
            (KeyCode::Enter, Some(Action::Activate)),
            (KeyCode::Char(' '), Some(Action::ToggleSection)),
            (KeyCode::Char('?'), Some(Action::ToggleHelp)),
            (KeyCode::Char('s'), Some(Action::CopyExternalMonitoringYaml)),
            (KeyCode::Char('a'), Some(Action::CopyExternalMonitoringGuide)),
            (KeyCode::Esc, Some(Action::Close)),
            (KeyCode::Char('l'), Some(Action::RequestLaunch)),
            (KeyCode::Char('r'), Some(Action::RequestRoute)),
            (KeyCode::Char('d'), Some(Action::RequestDetach)),
            (KeyCode::Char('q'), Some(Action::RequestQuit)),
            (KeyCode::Char('x'), Some(Action::RequestCancelAndDestroy)),
            (KeyCode::Char('y'), Some(Action::CopyReattachCommand)),
            (KeyCode::Char('n'), Some(Action::Reject)),
            (KeyCode::Up, Some(Action::Scroll { delta: -1, page: false })),
            (KeyCode::Down, Some(Action::Scroll { delta: 1, page: false })),
            (KeyCode::PageUp, Some(Action::Scroll { delta: -1, page: true })),
            (KeyCode::PageDown, Some(Action::Scroll { delta: 1, page: true })),
            (KeyCode::Char('f'), Some(Action::CycleLogFilter)),
            // A retired numeric view key.
            (KeyCode::Char('1'), None),
        ];
        for (code, action) in cases {
            assert_eq!(key_action(key(code)), action, "{code:?}");
        }
        assert_eq!(
            key_action(KeyEvent {
                kind: KeyEventKind::Release,
                ..key(KeyCode::Char('q'))
            }),
            None,
            "release of q"
        );
    }

    #[test]
    fn osc52_encodes_exact_text_without_a_newline() {
        assert_eq!(
            osc52("voxel tui resume"),
            "\u{1b}]52;c;dm94ZWwgdHVpIHJlc3VtZQ==\u{7}"
        );
    }

    #[derive(Default)]
    struct Fake {
        calls: Vec<&'static str>,
        fail: Vec<&'static str>,
    }
    impl Fake {
        fn call(&mut self, n: &'static str) -> io::Result<()> {
            self.calls.push(n);
            if self.fail.contains(&n) {
                Err(io::Error::other(n))
            } else {
                Ok(())
            }
        }
    }
    impl LifecycleOps for Fake {
        fn raw(&mut self) -> io::Result<()> {
            self.call("raw")
        }
        fn enter(&mut self) -> io::Result<()> {
            self.call("enter")
        }
        fn hide(&mut self) -> io::Result<()> {
            self.call("hide")
        }
        fn initialize(&mut self) -> io::Result<()> {
            self.call("init")
        }
        fn show(&mut self) -> io::Result<()> {
            self.call("show")
        }
        fn leave(&mut self) -> io::Result<()> {
            self.call("leave")
        }
        fn disable(&mut self) -> io::Result<()> {
            self.call("disable")
        }
    }
    #[test]
    fn setup_rolls_back_each_successful_stage() {
        for (failure, want) in [
            ("raw", vec!["raw"]),
            ("enter", vec!["raw", "enter", "disable"]),
            ("hide", vec!["raw", "enter", "hide", "leave", "disable"]),
            (
                "init",
                vec![
                    "raw", "enter", "hide", "init", "show", "leave", "disable",
                ],
            ),
        ] {
            let mut f = Fake { fail: vec![failure], ..Default::default() };
            assert!(Lifecycle::setup(&mut f).is_err());
            assert_eq!(f.calls, want);
        }
    }
    #[test]
    fn restore_attempts_every_step_and_retries_only_failures() {
        // The first case returns the first of two errors and skips the step
        // that succeeded on retry; the second continues past a later failure.
        for (label, failing) in [
            ("show and disable fail", vec!["show", "disable"]),
            ("only leave fails", vec!["leave"]),
        ] {
            let mut f = Fake::default();
            let mut s = Lifecycle::setup(&mut f).unwrap();
            f.calls.clear();
            f.fail = failing.clone();
            assert_eq!(
                s.restore(&mut f).unwrap_err().to_string(),
                failing[0],
                "{label}: first error"
            );
            assert_eq!(
                f.calls,
                vec!["show", "leave", "disable"],
                "{label}: every step attempted"
            );

            f.calls.clear();
            f.fail.clear();
            assert!(s.restore(&mut f).is_ok(), "{label}: retry");
            assert_eq!(f.calls, failing, "{label}: only failures retried");

            f.calls.clear();
            assert!(s.restore(&mut f).is_ok(), "{label}: idempotent");
            assert!(f.calls.is_empty(), "{label}: nothing left to restore");
        }
    }

    /// A panic on any thread restores the terminal, and the renderer cannot
    /// draw afterwards: a later frame would land on top of the panic message.
    #[test]
    fn panic_restores_the_terminal_and_drops_later_frames() {
        // A plain file stands in for the TTY so the test can read back exactly
        // which bytes reached it. The pid and timestamp keep concurrent and
        // repeated runs from sharing files.
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let tty_path =
            std::env::temp_dir().join(format!("voxel-tui-panic-tty-{unique}"));
        let log_path =
            std::env::temp_dir().join(format!("voxel-tui-panic-log-{unique}"));
        let tty = std::fs::File::create(&tty_path).unwrap();
        let released = Released::default();
        let mut renderer = PanicGate {
            tty: tty.try_clone().unwrap(),
            released: released.clone(),
        };
        let shutdown = CancellationToken::new();
        let original_hook = install_panic_hook(
            tty,
            released,
            shutdown.clone(),
            DurableLog::open(&log_path).unwrap(),
        );

        renderer.write_all(b"frame before").unwrap();
        let panic_result =
            std::thread::spawn(|| panic!("collector task failed")).join();
        renderer.write_all(b"frame after").unwrap();
        renderer.flush().unwrap();

        // Reinstall the original hook before asserting, so a failing assertion
        // below doesn't run through the hook under test.
        panic::set_hook(Box::new(move |info| original_hook(info)));

        assert!(panic_result.is_err(), "the spawned thread should panic");

        // The TTY holds the frame from before the panic, then the restore
        // sequence, and nothing written after it.
        let mut expected_tty = b"frame before".to_vec();
        execute!(expected_tty, Show, LeaveAlternateScreen).unwrap();
        assert_eq!(std::fs::read(&tty_path).unwrap(), expected_tty);

        assert!(shutdown.is_cancelled(), "the panic should cancel the TUI");

        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.starts_with("ERROR TUI ")
                && log.contains("collector task failed"),
            "the durable log should record the panic, got {log:?}"
        );

        std::fs::remove_file(tty_path).unwrap();
        std::fs::remove_file(log_path).unwrap();
    }
}
