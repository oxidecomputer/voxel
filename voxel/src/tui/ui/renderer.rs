use crate::tui::{App, AppEvent, Effect};
use ratatui::{Terminal, backend::Backend, layout::Rect};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const MIN_WIDTH: u16 = 48;
pub const MIN_HEIGHT: u16 = 16;
pub const WIDE_WIDTH: u16 = 100;
pub const WIDE_HEIGHT: u16 = 22;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutMode {
    Minimum,
    Compact,
    Wide,
}
pub fn layout_mode(area: Rect) -> LayoutMode {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        LayoutMode::Minimum
    } else if area.width < WIDE_WIDTH || area.height < WIDE_HEIGHT {
        LayoutMode::Compact
    } else {
        LayoutMode::Wide
    }
}

pub fn render_frame(frame: &mut ratatui::Frame<'_>, app: &App) {
    crate::tui::ui::widgets::draw(frame, app);
}

pub struct Renderer {
    minimum_interval: Duration,
    last_draw: Option<Instant>,
}
impl Renderer {
    pub fn new(minimum_interval: Duration) -> Self {
        Self { minimum_interval, last_draw: None }
    }
    pub fn draw<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &App,
        force: bool,
    ) -> io::Result<bool> {
        self.draw_at(terminal, app, force, Instant::now())
    }
    pub fn draw_at<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &App,
        force: bool,
        now: Instant,
    ) -> io::Result<bool> {
        if !force
            && self.last_draw.is_some_and(|last| {
                now.saturating_duration_since(last) < self.minimum_interval
            })
        {
            return Ok(false);
        }
        terminal.draw(|frame| render_frame(frame, app))?;
        self.last_draw = Some(now);
        Ok(true)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum LoopExit {
    ConfirmedExit,
    Cancelled { unsent: Vec<Effect> },
    EventChannelClosed,
    EffectChannelClosed { unsent: Vec<Effect> },
    RenderFailed { kind: io::ErrorKind, message: String, unsent: Vec<Effect> },
}

fn forward_effects(
    sender: &mpsc::UnboundedSender<Effect>,
    effects: Vec<Effect>,
) -> Result<(), Vec<Effect>> {
    let mut effects = effects.into_iter();
    while let Some(current) = effects.next() {
        if let Err(error) = sender.send(current) {
            return Err(std::iter::once(error.0).chain(effects).collect());
        }
    }
    Ok(())
}

pub async fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    clipboard: &mut impl Write,
    renderer: &mut Renderer,
    app: &mut App,
    events: &mut mpsc::Receiver<AppEvent>,
    effects: &mpsc::UnboundedSender<Effect>,
    cancel: CancellationToken,
) -> io::Result<LoopExit> {
    if let Err(error) = renderer.draw(terminal, app, true) {
        return Ok(LoopExit::RenderFailed {
            kind: error.kind(),
            message: error.to_string(),
            unsent: vec![],
        });
    }
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(LoopExit::Cancelled { unsent: vec![] }),
            event = events.recv() => match event {
                Some(event) => event,
                None => return Ok(LoopExit::EventChannelClosed),
            }
        };
        let pending_effects = app.update(event);
        let force_draw = !pending_effects.is_empty();
        let mut forwarded = Vec::with_capacity(pending_effects.len());
        for effect in pending_effects {
            match effect {
                Effect::CopyToClipboard(command) => {
                    crate::tui::terminal::write_osc52(
                        &mut *clipboard,
                        &command,
                    )?;
                }
                effect => forwarded.push(effect),
            }
        }
        if let Err(error) = renderer.draw(terminal, app, force_draw) {
            return Ok(LoopExit::RenderFailed {
                kind: error.kind(),
                message: error.to_string(),
                unsent: forwarded,
            });
        }
        let confirmed_exit =
            forwarded.iter().any(|effect| matches!(effect, Effect::Quit));
        if let Err(unsent) = forward_effects(effects, forwarded) {
            return Ok(LoopExit::EffectChannelClosed { unsent });
        }
        if confirmed_exit {
            return Ok(LoopExit::ConfirmedExit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{Action, AppEvent};
    use ratatui::{
        backend::{ClearType, TestBackend, WindowSize},
        buffer::Cell,
        layout::{Position, Size},
    };

    struct FailAfterDraw {
        inner: TestBackend,
        draws_remaining: usize,
    }

    impl Backend for FailAfterDraw {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            if self.draws_remaining == 0 {
                return Err(io::Error::other("draw failed"));
            }
            self.draws_remaining -= 1;
            self.inner.draw(content)
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> io::Result<()> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> io::Result<()> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> io::Result<Size> {
            self.inner.size()
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    #[test]
    fn deterministic_throttle_boundaries_and_force() {
        let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let app = App::new(vec![], 2, 2);
        let start = Instant::now();
        let mut r = Renderer::new(Duration::from_secs(1));
        assert!(r.draw_at(&mut t, &app, false, start).unwrap());
        assert!(
            !r.draw_at(&mut t, &app, false, start + Duration::from_millis(999))
                .unwrap()
        );
        assert!(
            r.draw_at(&mut t, &app, false, start + Duration::from_secs(1))
                .unwrap()
        );
        assert!(
            r.draw_at(&mut t, &app, true, start + Duration::from_secs(1))
                .unwrap()
        );
        assert!(
            !r.draw_at(&mut t, &app, false, start - Duration::from_secs(1))
                .unwrap()
        );
    }
    #[tokio::test]
    async fn loop_forces_draw_forwards_exact_effect_and_closes() {
        let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut app = App::new(vec![], 2, 2);
        app.deployment.observed =
            crate::tui::reconcile::ObservedDeploymentState::Stopped;
        let (tx, mut rx) = {
            let (a, b) = mpsc::channel(4);
            (a, b)
        };
        tx.send(AppEvent::Action(Action::RequestLaunch)).await.unwrap();
        tx.send(AppEvent::Action(Action::Scroll { delta: -1, page: false }))
            .await
            .unwrap();
        tx.send(AppEvent::Action(Action::Activate)).await.unwrap();
        drop(tx);
        let (etx, mut erx) = mpsc::unbounded_channel();
        let mut r = Renderer::new(Duration::ZERO);
        let mut clipboard = Vec::new();
        let exit = run_loop(
            &mut t,
            &mut clipboard,
            &mut r,
            &mut app,
            &mut rx,
            &etx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(exit, LoopExit::EventChannelClosed);
        assert!(matches!(
            erx.recv().await,
            Some(Effect::Start {
                kind: crate::tui::operation::OperationKind::Launch,
                ..
            })
        ));
        assert!(erx.try_recv().is_err());
        assert!(
            t.backend()
                .buffer()
                .content()
                .iter()
                .any(|c| !c.symbol().is_empty())
        );
    }

    #[tokio::test]
    async fn loop_consumes_clipboard_effect_without_forwarding_it() {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut app = App::new(vec![], 2, 2);
        app.reattach_command = Some("pfexec '/opt/voxel' tui".into());
        let (tx, mut rx) = mpsc::channel(2);
        tx.send(Action::RequestDetach.into()).await.unwrap();
        tx.send(Action::CopyReattachCommand.into()).await.unwrap();
        drop(tx);
        let (effects_tx, mut effects_rx) = mpsc::unbounded_channel();
        let mut clipboard = Vec::new();
        let exit = run_loop(
            &mut terminal,
            &mut clipboard,
            &mut Renderer::new(Duration::ZERO),
            &mut app,
            &mut rx,
            &effects_tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(exit, LoopExit::EventChannelClosed);
        assert_eq!(
            String::from_utf8(clipboard).unwrap(),
            crate::tui::terminal::osc52("pfexec '/opt/voxel' tui")
        );
        assert!(effects_rx.try_recv().is_err());
        assert!(app.clipboard_copied);
    }
    #[tokio::test]
    async fn effect_forwarding_is_unbounded_and_never_blocks_control_delivery()
    {
        let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut app = App::new(vec![], 2, 2);
        app.deployment.observed =
            crate::tui::reconcile::ObservedDeploymentState::Stopped;
        let (tx, mut rx) = mpsc::channel(3);
        tx.send(Action::RequestLaunch.into()).await.unwrap();
        tx.send(Action::Scroll { delta: -1, page: false }.into())
            .await
            .unwrap();
        tx.send(Action::Activate.into()).await.unwrap();
        let (etx, mut erx) = mpsc::unbounded_channel();
        etx.send(Effect::Quit).unwrap();
        let c = CancellationToken::new();
        let cc = c.clone();
        let mut r = Renderer::new(Duration::ZERO);
        let mut clipboard = Vec::new();
        let loop_future = tokio::time::timeout(
            Duration::from_millis(50),
            run_loop(
                &mut t,
                &mut clipboard,
                &mut r,
                &mut app,
                &mut rx,
                &etx,
                cc,
            ),
        );
        let cancel_future = async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            c.cancel();
        };
        let (result, ()) = tokio::join!(loop_future, cancel_future);
        let exit = result.unwrap().unwrap();
        assert!(
            matches!(exit, LoopExit::Cancelled { unsent } if unsent.is_empty())
        );
        assert_eq!(erx.recv().await, Some(Effect::Quit));
        assert!(matches!(erx.recv().await, Some(Effect::Start { .. })));
        assert!(
            app.operation.pending.is_some(),
            "launch and confirm were processed before cancellation"
        );
        let rendered = t.backend().buffer().content().iter().fold(
            String::new(),
            |mut text, cell| {
                text.push_str(cell.symbol());
                text
            },
        );
        assert!(
            rendered.contains("Launch pending"),
            "pending launch must render before effect forwarding"
        );
    }

    #[tokio::test]
    async fn closed_effect_receiver_returns_exact_unsent_effect() {
        let mut t = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut app = App::new(vec![], 2, 2);
        app.deployment.observed =
            crate::tui::reconcile::ObservedDeploymentState::Stopped;
        let (tx, mut rx) = mpsc::channel(3);
        tx.send(Action::RequestLaunch.into()).await.unwrap();
        tx.send(Action::Scroll { delta: -1, page: false }.into())
            .await
            .unwrap();
        tx.send(Action::Activate.into()).await.unwrap();
        let (etx, erx) = mpsc::unbounded_channel();
        drop(erx);
        let mut clipboard = Vec::new();
        let exit = run_loop(
            &mut t,
            &mut clipboard,
            &mut Renderer::new(Duration::ZERO),
            &mut app,
            &mut rx,
            &etx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(exit, LoopExit::EffectChannelClosed { unsent } if matches!(unsent.as_slice(), [Effect::Start { kind: crate::tui::operation::OperationKind::Launch, .. }]))
        );
    }

    #[tokio::test]
    async fn confirmed_quit_is_forwarded_before_loop_exits() {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut app = App::new(vec![], 2, 2);
        app.deployment.observed =
            crate::tui::reconcile::ObservedDeploymentState::Stopped;
        let (tx, mut rx) = mpsc::channel(3);
        tx.send(Action::RequestQuit.into()).await.unwrap();
        tx.send(Action::Scroll { delta: -1, page: false }.into())
            .await
            .unwrap();
        tx.send(Action::Activate.into()).await.unwrap();
        let (effects_tx, mut effects_rx) = mpsc::unbounded_channel();
        let exit = run_loop(
            &mut terminal,
            &mut Vec::new(),
            &mut Renderer::new(Duration::ZERO),
            &mut app,
            &mut rx,
            &effects_tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(exit, LoopExit::ConfirmedExit);
        assert_eq!(effects_rx.recv().await, Some(Effect::Quit));
    }

    #[tokio::test]
    async fn render_failure_returns_reducer_effect_without_forwarding_it() {
        let mut t = Terminal::new(FailAfterDraw {
            inner: TestBackend::new(80, 20),
            draws_remaining: 1,
        })
        .unwrap();
        let mut app = App::new(vec![], 2, 2);
        app.deployment.observed =
            crate::tui::reconcile::ObservedDeploymentState::Stopped;
        app.session.confirmation =
            Some(crate::tui::event::Confirmation::Launch);
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Action::Activate.into()).await.unwrap();
        let (etx, mut erx) = mpsc::unbounded_channel();
        let mut clipboard = Vec::new();

        let exit = run_loop(
            &mut t,
            &mut clipboard,
            &mut Renderer::new(Duration::ZERO),
            &mut app,
            &mut rx,
            &etx,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(matches!(
            exit,
            LoopExit::RenderFailed {
                kind: io::ErrorKind::Other,
                message,
                unsent,
            } if message == "draw failed"
                && matches!(unsent.as_slice(), [Effect::Start {
                    kind: crate::tui::operation::OperationKind::Launch,
                    ..
                }])
        ));
        assert!(erx.try_recv().is_err());
        assert!(app.operation.pending.is_some());
    }

    #[tokio::test]
    async fn closed_forwarding_preserves_unsent_current_and_tail_order() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let effects = vec![
            Effect::Quit,
            Effect::Cancel {
                request_id: crate::tui::event::OperationRequestId::FIRST,
                choice: crate::tui::event::CancelChoice::Leave,
            },
        ];
        let unsent = forward_effects(&tx, effects.clone()).unwrap_err();
        assert_eq!(unsent, effects);
    }
}
