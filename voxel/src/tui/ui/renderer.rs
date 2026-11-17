// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The render loop. Chooses a layout mode from the terminal size, limits
//! redraws to a frame budget, and moves events and effects between the
//! [`App`] and the runtime.

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
    Cancelled,
    EventChannelClosed,
    EffectChannelClosed { unsent: Vec<Effect> },
    RenderFailed { message: String },
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
        return Ok(LoopExit::RenderFailed { message: error.to_string() });
    }
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(LoopExit::Cancelled),
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
            return Ok(LoopExit::RenderFailed { message: error.to_string() });
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
    use crate::tui::Action;
    use crate::tui::event::OperationRequestId;
    use crate::tui::operation::OperationKind;
    use crate::tui::reconcile::ObservedDeploymentState;
    use ratatui::backend::TestBackend;

    /// An action the operator confirmed is handed back, not dropped, when the
    /// runtime can no longer receive it.
    #[tokio::test]
    async fn closed_effect_receiver_returns_exact_unsent_effect() {
        let mut app = App::new(vec![], 2, 2);
        app.deployment.observed = ObservedDeploymentState::Stopped;
        let (tx, mut rx) = mpsc::channel(3);
        // Open the launch dialog, select Launch above the default, confirm.
        for action in [
            Action::RequestLaunch,
            Action::Scroll { delta: -1, page: false },
            Action::Activate,
        ] {
            tx.send(action.into()).await.unwrap();
        }
        drop(tx);
        let (effects_tx, mut effects_rx) = mpsc::unbounded_channel();
        effects_rx.close();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

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

        assert_eq!(
            exit,
            LoopExit::EffectChannelClosed {
                unsent: vec![Effect::Start {
                    request_id: OperationRequestId::FIRST,
                    kind: OperationKind::Launch,
                }],
            }
        );
    }
}
