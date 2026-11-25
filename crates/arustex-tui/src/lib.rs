//! Terminal interface. It talks to the daemon over the local socket only; it never
//! opens the database or any network connection itself.

pub mod app;
pub mod text;
pub mod ui;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arustex_app::client::{Client, ClientEvent};
use arustex_core::CoreResult;
use arustex_core::ipc::{Request, Response};
use crossterm::event::{Event, EventStream};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub use app::{App, View};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct TuiOptions {
    pub socket: PathBuf,
    pub request_timeout: Duration,
}

/// Restores the terminal on drop and on panic.
#[derive(Debug)]
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen, cursor::Hide) {
            restore_terminal();
            return Err(e);
        }
        let terminal =
            Terminal::new(CrosstermBackend::new(stdout)).inspect_err(|_| restore_terminal())?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

enum Msg {
    Connected(Arc<Client>, mpsc::Receiver<ClientEvent>),
    ConnectFailed(String),
    Reply(CoreResult<Response>),
}

/// Runs the interface until the operator quits.
pub async fn run(options: TuiOptions) -> io::Result<()> {
    let theme = ui::Theme::from_env();
    let mut guard = TerminalGuard::new()?;
    let mut app = App::new();
    let mut input = EventStream::new();
    let (msg_tx, mut msg_rx) = mpsc::channel::<Msg>(256);
    let mut client: Option<Arc<Client>> = None;
    let mut events: Option<mpsc::Receiver<ClientEvent>> = None;
    let mut backoff = Duration::from_millis(500);
    let mut ticker = tokio::time::interval(Duration::from_millis(400));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    spawn_connect(options.clone(), msg_tx.clone(), Duration::ZERO);
    let send = |client: &Option<Arc<Client>>, reqs: Vec<Request>, tx: &mpsc::Sender<Msg>| {
        let Some(c) = client else { return };
        for r in reqs {
            let c = c.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(Msg::Reply(c.request(r).await)).await;
            });
        }
    };

    loop {
        guard.terminal.draw(|f| ui::draw(f, &app, theme))?;
        if app.quit {
            break;
        }
        tokio::select! {
            maybe = input.next() => match maybe {
                Some(Ok(Event::Key(key))) => {
                    let reqs = app.on_key(key);
                    if !reqs.is_empty() && client.is_none() {
                        app.error("not connected to the daemon");
                    }
                    send(&client, reqs, &msg_tx);
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => break,
            },
            ev = async {
                match events.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match ev {
                Some(ClientEvent::Event(e)) => {
                    let reqs = app.on_event(e);
                    send(&client, reqs, &msg_tx);
                }
                Some(ClientEvent::Resync { latest_seq }) => {
                    let reqs = app.on_resync(latest_seq);
                    send(&client, reqs, &msg_tx);
                }
                Some(ClientEvent::Disconnected) | None => {
                    client = None;
                    events = None;
                    app.on_disconnected();
                    spawn_connect(options.clone(), msg_tx.clone(), backoff);
                }
            },
            Some(msg) = msg_rx.recv() => match msg {
                Msg::Connected(c, rx) => {
                    backoff = Duration::from_millis(500);
                    let reqs = app.on_connected(c.server.clone());
                    client = Some(c);
                    events = Some(rx);
                    send(&client, reqs, &msg_tx);
                }
                Msg::ConnectFailed(e) => {
                    app.connected = false;
                    app.error(format!("{e}; retrying in {}s", backoff.as_secs().max(1)));
                    spawn_connect(options.clone(), msg_tx.clone(), backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Msg::Reply(Ok(resp)) => {
                    let reqs = app.on_response(resp);
                    send(&client, reqs, &msg_tx);
                }
                Msg::Reply(Err(e)) => app.error(e.to_string()),
            },
            _ = ticker.tick() => {
                if client.is_some() {
                    let reqs = app.take_dirty();
                    send(&client, reqs, &msg_tx);
                }
            }
            _ = term.recv() => break,
            _ = hup.recv() => break,
        }
    }
    drop(guard);
    Ok(())
}

fn spawn_connect(options: TuiOptions, tx: mpsc::Sender<Msg>, delay: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let timeout = options.request_timeout.max(CONNECT_TIMEOUT);
        let msg = match Client::connect(&options.socket, "arustex-tui", timeout, 2048).await {
            Ok((c, rx)) => Msg::Connected(Arc::new(c), rx),
            Err(e) => Msg::ConnectFailed(e.to_string()),
        };
        let _ = tx.send(msg).await;
    });
}

#[cfg(test)]
mod tests;
