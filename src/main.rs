//! Entry point: setup, the terminal event loop, teardown.

use std::collections::VecDeque;
use std::error::Error as _;
use std::io::{self, stdout, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{Event, EventStream};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use url::Url;

use railway_tui::api::Api;
use railway_tui::app::{App, AppEvent, Command, DataEvent, Secret, StartPrefs};
use railway_tui::auth::{token_source_from_env, AuthError, TokenSource};
use railway_tui::budget::{RateBudget, SharedBudget};
use railway_tui::config::{config_path, load_config, Config, ConfigError};
use railway_tui::pending::{load_pending, pending_path, Pending};
use railway_tui::remember::{load_remembered, remembered_path};
use railway_tui::ui;
use railway_tui::worker::{run_worker, WorkerConfig, HTTP_ENDPOINT, WS_ENDPOINT};

const TICK: Duration = Duration::from_millis(250);
/// How long background work (a state-file write, a token refresh) may delay exit.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Why the app stopped with an error; printed with its causes on exit.
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("railway-tui: cannot determine the home directory")]
    NoHome,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("railway-tui: cannot start the async runtime")]
    Runtime(#[source] io::Error),
    #[error("railway-tui: cannot set up the HTTP client")]
    Http(#[from] reqwest::Error),
    #[error("railway-tui: cannot set up the terminal")]
    Terminal(#[source] io::Error),
    #[error("railway-tui: drawing failed")]
    Draw(#[source] io::Error),
    #[error("railway-tui: terminal input failed")]
    Input(#[source] io::Error),
    #[error("railway-tui: the background worker stopped unexpectedly")]
    WorkerGone,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", with_causes(&e));
            ExitCode::FAILURE
        }
    }
}

/// `e` followed by each cause, `: `-separated. A cause whose text the message
/// already shows (many errors here include their source's text) is skipped.
fn with_causes(e: &Error) -> String {
    let mut message = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        let text = c.to_string();
        if !message.contains(&text) {
            message = format!("{message}: {text}");
        }
        cause = c.source();
    }
    message
}

fn run() -> Result<(), Error> {
    let home = dirs::home_dir().ok_or(Error::NoHome)?;
    let xdg = |name| std::env::var_os(name).map(PathBuf::from);
    let config = load_config(&config_path(xdg("XDG_CONFIG_HOME").as_deref(), &home))?;
    // Fail on a missing login before the terminal is touched.
    let tokens = token_source_from_env()?;
    tokens.token()?;
    let state_path = remembered_path(xdg("XDG_STATE_HOME").as_deref(), &home);
    let pending_file = pending_path(xdg("XDG_STATE_HOME").as_deref(), &home);
    let prefs = start_prefs(&config, &state_path, &pending_file);

    let runtime = tokio::runtime::Runtime::new().map_err(Error::Runtime)?;
    let result = runtime.block_on(tui(config, tokens, state_path, pending_file, prefs));
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    result
}

fn start_prefs(config: &Config, state_path: &Path, pending_file: &Path) -> StartPrefs {
    let (pending, pending_error) = match load_pending(pending_file) {
        Ok(pending) => (pending, None),
        Err(e) => (Pending::default(), Some(e.to_string())),
    };
    StartPrefs {
        workspace: config.default_workspace.clone(),
        project: config.default_project.clone(),
        environment: config.default_environment.clone(),
        theme: config.theme.clone(),
        remembered: load_remembered(state_path),
        pending,
        pending_error,
    }
}

async fn tui(
    config: Config,
    tokens: Arc<dyn TokenSource>,
    state_path: PathBuf,
    pending_file: PathBuf,
    prefs: StartPrefs,
) -> Result<(), Error> {
    let http = Url::parse(HTTP_ENDPOINT).expect("valid endpoint");
    let ws = Url::parse(WS_ENDPOINT).expect("valid endpoint");
    let budget = SharedBudget::new(RateBudget::new(config.rate_limit_per_hour));
    let api = Arc::new(Api::new(http, tokens.clone(), budget)?);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(1024);
    let worker_config = WorkerConfig {
        api,
        ws,
        tokens,
        remembered_path: state_path,
        pending_path: pending_file,
        configured_interval: config.refresh_interval_secs,
        gh: PathBuf::from(railway_tui::github::GH),
    };
    let mut worker = tokio::spawn(run_worker(worker_config, cmd_rx, event_tx));

    let result = match enter_terminal() {
        Ok(mut terminal) => event_loop(&mut terminal, prefs, cmd_tx, event_rx).await,
        Err(e) => {
            drop(cmd_tx);
            Err(Error::Terminal(e))
        }
    };
    restore_terminal();
    // The command sender is gone, so the worker handles what is already queued
    // (a save sent just before quit included) and returns; abort it if it overruns.
    if tokio::time::timeout(SHUTDOWN_GRACE, &mut worker)
        .await
        .is_err()
    {
        worker.abort();
    }
    result
}

/// Raw mode and the alternate screen, with a panic hook that undoes both.
fn enter_terminal() -> std::io::Result<Tui> {
    install_panic_hook();
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout()))
}

/// Restores the terminal when the UI thread panics. Panics on other threads
/// are background tasks the worker survives (a panicking load is reported as
/// an error), so the screen must stay as it is.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restore_terminal();
        }
        previous(info);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, Show);
}

async fn event_loop(
    terminal: &mut Tui,
    prefs: StartPrefs,
    commands: mpsc::Sender<Command>,
    mut data: mpsc::Receiver<AppEvent>,
) -> Result<(), Error> {
    let (mut app, initial) = App::new(prefs);
    if !dispatch(&commands, terminal, &mut app, initial).await? {
        return Ok(());
    }
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal
            .draw(|frame| ui::draw(frame, &app, Instant::now()))
            .map_err(Error::Draw)?;
        let event = tokio::select! {
            ev = input.next() => match ev {
                Some(Ok(Event::Key(key))) => AppEvent::Key(key),
                // A resize or other terminal event only needs a redraw.
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(Error::Input(e)),
                None => return Ok(()),
            },
            _ = tick.tick() => AppEvent::Tick(Instant::now()),
            ev = data.recv() => ev.ok_or(Error::WorkerGone)?,
        };
        let produced = app.update(event, Instant::now());
        if !dispatch(&commands, terminal, &mut app, produced).await? {
            return Ok(());
        }
        // Apply whatever else has already arrived before drawing again.
        while let Ok(event) = data.try_recv() {
            let produced = app.update(event, Instant::now());
            if !dispatch(&commands, terminal, &mut app, produced).await? {
                return Ok(());
            }
        }
    }
}

/// Sends commands to the worker; `Ok(false)` means the app asked to quit.
/// `Copy` also stays here. `EditExternal` never reaches the worker: the editor runs here and its result is fed
/// back to the app, whose follow-up commands are handled in turn.
async fn dispatch(
    worker: &mpsc::Sender<Command>,
    terminal: &mut Tui,
    app: &mut App,
    commands: Vec<Command>,
) -> Result<bool, Error> {
    let mut queue = VecDeque::from(commands);
    while let Some(command) = queue.pop_front() {
        match command {
            Command::Quit => return Ok(false),
            Command::EditExternal { name, initial } => {
                let result =
                    tokio::task::block_in_place(|| edit_external(terminal, initial.expose()))
                        .map(Secret::new);
                let event = AppEvent::Data(DataEvent::Edited { name, result });
                queue.extend(app.update(event, Instant::now()));
            }
            Command::Copy { text } => {
                let mut out = stdout();
                out.write_all(railway_tui::clipboard::osc52(text.expose()).as_bytes())
                    .and_then(|()| out.flush())
                    .map_err(Error::Terminal)?;
            }
            other => worker.send(other).await.map_err(|_| Error::WorkerGone)?,
        }
    }
    Ok(true)
}

/// Suspends the TUI, edits `initial` in `$VISUAL`, else `$EDITOR`, else `vi`, and resumes.
/// The value only ever touches a 0600 temp file, which is deleted on return. Error text
/// never includes the value.
fn edit_external(terminal: &mut Tui, initial: &str) -> Result<String, String> {
    restore_terminal();
    let result = run_editor(initial);
    let resumed = enable_raw_mode()
        .and_then(|()| execute!(stdout(), EnterAlternateScreen))
        .and_then(|()| terminal.clear());
    match (result, resumed) {
        (_, Err(e)) => Err(format!("could not return to the terminal: {e}")),
        (result, Ok(())) => result,
    }
}

fn run_editor(initial: &str) -> Result<String, String> {
    let mut file = tempfile::Builder::new()
        .prefix("railway-tui-")
        .tempfile()
        .map_err(|e| format!("could not create a temporary file: {e}"))?;
    file.write_all(initial.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| format!("could not write the temporary file: {e}"))?;
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    // The variable may carry arguments, e.g. "code --wait".
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or("vi");
    let status = std::process::Command::new(program)
        .args(words)
        .arg(file.path())
        .status()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if !status.success() {
        return Err(format!("{program} exited with {status}; no change made"));
    }
    let mut text = std::fs::read_to_string(file.path())
        .map_err(|e| format!("could not read the edited file: {e}"))?;
    if text.ends_with('\n') {
        text.pop();
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_message_adds_each_cause_once() {
        let terminal = Error::Terminal(io::Error::other("not a tty"));
        assert_eq!(
            with_causes(&terminal),
            "railway-tui: cannot set up the terminal: not a tty"
        );
        // The config error's text already includes the parser's.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("railway-tui.conf");
        std::fs::write(&path, "rate_limit = 5\n").unwrap();
        let config = Error::from(load_config(&path).unwrap_err());
        assert_eq!(with_causes(&config), config.to_string());
    }
}
