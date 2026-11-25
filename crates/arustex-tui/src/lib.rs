//! Terminal client for the read-only ARUSTEX IPC contract.

#![forbid(unsafe_code)]

mod app;
mod views;

use std::{
    future::{Future, pending},
    io::{self, IsTerminal},
};

use arustex_core::{Query, Reply};
use crossterm::{
    cursor::{Hide, Show},
    event::{Event, EventStream, KeyEventKind},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
    },
};
use futures_util::StreamExt;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};
use tokio::signal::unix::{SignalKind, signal};

use app::{Action, Browser};

/// Runs the terminal client. The callback opens a local IPC connection for each
/// request and must be cancellation-safe when its future is dropped.
pub async fn run<F, Fut>(mut query: F) -> io::Result<()>
where
    F: FnMut(Query) -> Fut,
    Fut: Future<Output = Result<Reply, String>>,
{
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let _terminal_mode = TerminalMode::enter()?;
    let (width, height) = size()?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fixed(viewport(width, height)),
        },
    )?;
    let mut events = EventStream::new();

    let mut browser = Browser::default();
    let mut request = Some(Box::pin(query(browser.load())));

    loop {
        terminal.draw(|frame| views::render(frame, &browser))?;
        tokio::select! {
            event = events.next() => match event.transpose()? {
                Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    match browser.key(key) {
                        Action::Quit => break,
                        Action::Load => request = Some(Box::pin(query(browser.load()))),
                        Action::None => {}
                    }
                }
                Some(Event::Resize(width, height)) => terminal.resize(viewport(width, height))?,
                Some(_) => {}
                None => break,
            },
            result = async {
                match request.as_mut() {
                    Some(future) => future.await,
                    None => pending().await,
                }
            } => {
                request = None;
                browser.complete(result);
            },
            _ = interrupt.recv() => break,
            _ = terminate.recv() => break,
        }
    }

    Ok(())
}

fn viewport(width: u16, height: u16) -> Rect {
    // The terminal controls its reported dimensions. Cap the backing buffers.
    Rect::new(0, 0, width.min(300), height.min(120))
}

struct TerminalMode;

impl TerminalMode {
    fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "run the TUI in an interactive terminal",
            ));
        }
        enable_raw_mode()?;
        let mode = Self;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(mode)
    }
}

impl Drop for TerminalMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}
