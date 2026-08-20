use std::{io, thread, time::Duration};

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

use crate::tui::event::{Action, AppEvent, View};

pub type TuiTerminal = Terminal<CrosstermBackend<io::BufWriter<std::fs::File>>>;

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
        KeyCode::Char('1') => Action::SwitchView(View::Deployment),
        KeyCode::Char('2') => Action::SwitchView(View::Monitor),
        KeyCode::Tab => Action::NextItem,
        KeyCode::BackTab => Action::PreviousItem,
        KeyCode::Left => Action::PreviousRack,
        KeyCode::Right => Action::NextRack,
        KeyCode::Enter => Action::Activate,
        KeyCode::Char(' ') => Action::ToggleSection,
        KeyCode::Char('?') | KeyCode::F(1) => Action::ToggleHelp,
        KeyCode::Char('s') => Action::CopyExternalMonitoringSelected,
        KeyCode::Char('a') => Action::CopyExternalMonitoringAll,
        KeyCode::Char('u') => Action::CopyExternalMonitoringGuide,
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
        KeyCode::Char('c') => Action::RequestCancelAndLeave,
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
    backend_writer: Option<io::BufWriter<std::fs::File>>,
    terminal: Option<TuiTerminal>,
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
}
impl TerminalSession {
    pub fn enter(writer: std::fs::File) -> io::Result<Self> {
        let fallback = io::BufWriter::new(writer.try_clone()?);
        let mut ops = RealOps {
            fallback,
            backend_writer: Some(io::BufWriter::new(writer)),
            terminal: None,
        };
        let lifecycle = Lifecycle::setup(&mut ops)?;
        Ok(Self {
            terminal: ops.terminal,
            fallback: Some(ops.fallback),
            lifecycle,
        })
    }
    pub fn terminal_mut(&mut self) -> &mut TuiTerminal {
        self.terminal.as_mut().expect("active terminal")
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
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex, mpsc as std_mpsc},
    };
    #[test]
    fn maps_all_documented_keys() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let cases = [
            (KeyCode::Char('1'), Action::SwitchView(View::Deployment)),
            (KeyCode::Char('2'), Action::SwitchView(View::Monitor)),
            (KeyCode::Tab, Action::NextItem),
            (KeyCode::BackTab, Action::PreviousItem),
            (KeyCode::Left, Action::PreviousRack),
            (KeyCode::Right, Action::NextRack),
            (KeyCode::Enter, Action::Activate),
            (KeyCode::Char(' '), Action::ToggleSection),
            (KeyCode::Char('?'), Action::ToggleHelp),
            (KeyCode::F(1), Action::ToggleHelp),
            (KeyCode::Char('s'), Action::CopyExternalMonitoringSelected),
            (KeyCode::Char('a'), Action::CopyExternalMonitoringAll),
            (KeyCode::Char('u'), Action::CopyExternalMonitoringGuide),
            (KeyCode::Esc, Action::Close),
            (KeyCode::Char('l'), Action::RequestLaunch),
            (KeyCode::Char('r'), Action::RequestRoute),
            (KeyCode::Char('d'), Action::RequestDetach),
            (KeyCode::Char('q'), Action::RequestQuit),
            (KeyCode::Char('c'), Action::RequestCancelAndLeave),
            (KeyCode::Char('x'), Action::RequestCancelAndDestroy),
            (KeyCode::Char('y'), Action::CopyReattachCommand),
            (KeyCode::Char('n'), Action::Reject),
        ];
        for (c, a) in cases {
            assert_eq!(key_action(k(c)), Some(a));
        }
        assert_eq!(key_action(k(KeyCode::Char('3'))), None);
        assert_eq!(key_action(k(KeyCode::Char('4'))), None);
        assert_eq!(key_action(k(KeyCode::Char('o'))), None);
        assert_eq!(
            key_action(KeyEvent {
                kind: KeyEventKind::Release,
                ..k(KeyCode::Char('q'))
            }),
            None
        );
        assert_eq!(key_action(k(KeyCode::F(12))), None);
    }

    #[test]
    fn osc52_encodes_exact_text_without_a_newline() {
        assert_eq!(
            osc52("voxel tui resume"),
            "\u{1b}]52;c;dm94ZWwgdHVpIHJlc3VtZQ==\u{7}"
        );
    }

    #[test]
    fn maps_log_navigation_and_filter_keys() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(
            key_action(key(KeyCode::Up)),
            Some(Action::Scroll { delta: -1, page: false })
        );
        assert_eq!(
            key_action(key(KeyCode::Down)),
            Some(Action::Scroll { delta: 1, page: false })
        );
        assert_eq!(
            key_action(key(KeyCode::PageUp)),
            Some(Action::Scroll { delta: -1, page: true })
        );
        assert_eq!(
            key_action(key(KeyCode::PageDown)),
            Some(Action::Scroll { delta: 1, page: true })
        );
        assert_eq!(
            key_action(key(KeyCode::Char('f'))),
            Some(Action::CycleLogFilter)
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
    fn restore_returns_first_error_and_attempts_every_cleanup_step() {
        let mut f = Fake::default();
        let mut s = Lifecycle::setup(&mut f).unwrap();
        f.calls.clear();
        f.fail = vec!["show", "leave", "disable"];
        assert_eq!(s.restore(&mut f).unwrap_err().to_string(), "show");
        assert_eq!(f.calls, vec!["show", "leave", "disable"]);
    }
    #[test]
    fn restore_retries_only_cleanup_steps_that_failed() {
        let mut f = Fake::default();
        let mut s = Lifecycle::setup(&mut f).unwrap();
        f.calls.clear();
        f.fail = vec!["show", "disable"];
        assert_eq!(s.restore(&mut f).unwrap_err().to_string(), "show");
        assert_eq!(f.calls, vec!["show", "leave", "disable"]);

        f.calls.clear();
        f.fail.clear();
        assert!(s.restore(&mut f).is_ok());
        assert_eq!(f.calls, vec!["show", "disable"]);

        f.calls.clear();
        assert!(s.restore(&mut f).is_ok());
        assert!(f.calls.is_empty());
    }
    #[test]
    fn restore_is_idempotent() {
        let mut f = Fake::default();
        let mut s = Lifecycle::setup(&mut f).unwrap();
        f.calls.clear();
        assert!(s.restore(&mut f).is_ok());
        assert_eq!(f.calls, vec!["show", "leave", "disable"]);
        f.calls.clear();
        assert!(s.restore(&mut f).is_ok());
        assert!(f.calls.is_empty());
    }
    struct Source(Arc<Mutex<VecDeque<Event>>>);
    impl EventSource for Source {
        fn next(&mut self, _: Duration) -> io::Result<Option<Event>> {
            Ok(self.0.lock().unwrap().pop_front())
        }
    }
    #[tokio::test]
    async fn fake_source_events_arrive_in_source_order() {
        let (tx, mut rx) = event_channel_with_capacity(3);
        let events = Arc::new(Mutex::new(VecDeque::from([
            Event::Resize(2, 3),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        ])));
        let cancel = CancellationToken::new();
        let (done_tx, done_rx) = std_mpsc::channel();
        let h = std::thread::spawn(move || {
            let result = input_loop(Source(events), tx, cancel);
            let _ = done_tx.send(());
            result
        });
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .expect("first source event timed out"),
            Some(AppEvent::Resize { width: 2, height: 3 })
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .expect("second source event timed out"),
            Some(AppEvent::Action(Action::RequestQuit))
        ));
        drop(rx);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("input loop did not stop after receiver closed");
        assert!(h.join().unwrap().is_ok());
    }
    #[test]
    fn input_full_channel_cancellation_joins_promptly() {
        let (tx, mut rx) = event_channel_with_capacity(1);
        tx.try_send(AppEvent::Resize { width: 1, height: 1 }).unwrap();
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let events = Arc::new(Mutex::new(VecDeque::from([
            Event::Resize(2, 2),
            Event::Resize(3, 3),
        ])));
        let (done_tx, done_rx) = std_mpsc::channel();
        let h = std::thread::spawn(move || {
            let result = input_loop(Source(events), tx, c);
            let _ = done_tx.send(());
            result
        });
        std::thread::sleep(Duration::from_millis(15));
        cancel.cancel();
        let started = std::time::Instant::now();
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("input loop did not stop after cancellation");
        assert!(h.join().unwrap().is_ok());
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(matches!(rx.try_recv(), Ok(AppEvent::Resize { width: 1, .. })));
    }
    #[test]
    fn dropped_receiver_stops_input_loop_promptly() {
        let (tx, rx) = event_channel_with_capacity(1);
        drop(rx);
        let events =
            Arc::new(Mutex::new(VecDeque::from([Event::Resize(4, 5)])));
        let started = std::time::Instant::now();
        assert!(
            input_loop(Source(events), tx, CancellationToken::new()).is_ok()
        );
        assert!(started.elapsed() < Duration::from_millis(100));
    }
    #[tokio::test]
    async fn tick_with_full_channel_stops_on_cancel_and_zero_is_rejected() {
        let (tx, _rx) = event_channel_with_capacity(1);
        tx.try_send(AppEvent::Resize { width: 1, height: 1 }).unwrap();
        let cancel = CancellationToken::new();
        let h =
            spawn_ticks(tx, cancel.clone(), Duration::from_millis(1)).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(100), h)
            .await
            .unwrap()
            .unwrap();
        let (tx, _) = event_channel();
        assert_eq!(
            spawn_ticks(tx, CancellationToken::new(), Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
