//! Entry point: setup, the terminal event loop, teardown.

use std::io::{stdout, Stdout};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
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
use railway_tui::app::{App, AppEvent, Command, StartPrefs};
use railway_tui::auth::{token_source_from_env, TokenSource};
use railway_tui::budget::RateBudget;
use railway_tui::config::{config_path, load_config, Config};
use railway_tui::pending::{load_pending, pending_path, Pending};
use railway_tui::remember::{load_remembered, remembered_path};
use railway_tui::ui;
use railway_tui::worker::{run_worker, HTTP_ENDPOINT, WS_ENDPOINT};

const TICK: Duration = Duration::from_millis(250);
/// How long background work (a state-file write, a token refresh) may delay exit.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const WORKER_GONE: &str = "railway-tui: the background worker stopped unexpectedly";

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let home = dirs::home_dir().ok_or("railway-tui: cannot determine the home directory")?;
    let xdg = |name| std::env::var_os(name).map(PathBuf::from);
    let config = load_config(&config_path(xdg("XDG_CONFIG_HOME").as_deref(), &home))
        .map_err(|e| e.to_string())?;
    // Fail on a missing login before the terminal is touched.
    let tokens = token_source_from_env().map_err(|e| e.to_string())?;
    tokens.token().map_err(|e| e.to_string())?;
    let state_path = remembered_path(xdg("XDG_STATE_HOME").as_deref(), &home);
    let pending_file = pending_path(xdg("XDG_STATE_HOME").as_deref(), &home);
    let prefs = start_prefs(&config, &state_path, &pending_file);

    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let result = runtime.block_on(tui(config, tokens, state_path, pending_file, prefs));
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    result
}

fn start_prefs(config: &Config, state_path: &Path, pending_file: &Path) -> StartPrefs {
    let (pending, pending_error) = match load_pending(pending_file) {
        Ok(pending) => (pending, None),
        Err(e) => (Pending::default(), Some(e)),
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
) -> Result<(), String> {
    let http = Url::parse(HTTP_ENDPOINT).expect("valid endpoint");
    let ws = Url::parse(WS_ENDPOINT).expect("valid endpoint");
    let budget = Arc::new(Mutex::new(RateBudget::new(config.rate_limit_per_hour)));
    let api = Arc::new(Api::new(http, tokens.clone(), budget));
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(1024);
    let worker = tokio::spawn(run_worker(
        api,
        ws,
        tokens,
        state_path,
        pending_file,
        config.refresh_interval_secs,
        cmd_rx,
        event_tx,
    ));

    let result = match enter_terminal() {
        Ok(mut terminal) => event_loop(&mut terminal, prefs, cmd_tx, event_rx).await,
        Err(e) => Err(format!("railway-tui: cannot set up the terminal: {e}")),
    };
    restore_terminal();
    worker.abort();
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
) -> Result<(), String> {
    let (mut app, initial) = App::new(prefs);
    if !dispatch(&commands, initial).await? {
        return Ok(());
    }
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal
            .draw(|frame| ui::draw(frame, &app, Instant::now()))
            .map_err(|e| format!("railway-tui: drawing failed: {e}"))?;
        let event = tokio::select! {
            ev = input.next() => match ev {
                Some(Ok(Event::Key(key))) => AppEvent::Key(key),
                // A resize or other terminal event only needs a redraw.
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(format!("railway-tui: terminal input failed: {e}")),
                None => return Ok(()),
            },
            _ = tick.tick() => AppEvent::Tick(Instant::now()),
            ev = data.recv() => ev.ok_or(WORKER_GONE)?,
        };
        if !dispatch(&commands, app.update(event, Instant::now())).await? {
            return Ok(());
        }
        // Apply whatever else has already arrived before drawing again.
        while let Ok(event) = data.try_recv() {
            if !dispatch(&commands, app.update(event, Instant::now())).await? {
                return Ok(());
            }
        }
    }
}

/// Sends commands to the worker; `Ok(false)` means the app asked to quit.
async fn dispatch(worker: &mpsc::Sender<Command>, commands: Vec<Command>) -> Result<bool, String> {
    for command in commands {
        if command == Command::Quit {
            return Ok(false);
        }
        worker.send(command).await.map_err(|_| WORKER_GONE)?;
    }
    Ok(true)
}
