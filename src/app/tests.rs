use super::*;
use crate::model::{InstanceStatus, LogLine};
use chrono::{TimeZone, Utc};
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

/// The open modal's parts, for asserting on.
fn confirm(app: &App) -> Option<&Confirm> {
    match &app.modal {
        Some(Modal::Confirm(c)) => Some(c),
        _ => None,
    }
}

fn var_input(app: &App) -> Option<&VarInput> {
    match &app.modal {
        Some(Modal::VarInput(input)) => Some(input),
        _ => None,
    }
}

fn filter_input(app: &App) -> Option<&str> {
    match &app.modal {
        Some(Modal::Filter(text)) => Some(text),
        _ => None,
    }
}

fn env_picker(app: &App) -> Option<usize> {
    match app.modal {
        Some(Modal::EnvPicker(index)) => Some(index),
        _ => None,
    }
}

fn branch_picker(app: &App) -> Option<&BranchPicker> {
    match &app.modal {
        Some(Modal::BranchPicker(picker)) => Some(picker),
        _ => None,
    }
}

fn theme_picker(app: &App) -> Option<usize> {
    match app.modal {
        Some(Modal::ThemePicker { restore }) => Some(restore),
        _ => None,
    }
}

fn key(c: char) -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn code(c: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(c, KeyModifiers::NONE))
}

fn ctrl(c: char) -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn typed(app: &mut App, text: &str, now: Instant) {
    for c in text.chars() {
        app.update(key(c), now);
    }
}

fn deploy(id: &str, status: DeploymentStatus) -> DeploymentSummary {
    DeploymentSummary {
        id: id.into(),
        status,
        created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        commit_sha: None,
        commit_message: None,
        repo: None,
        branch: None,
        image: None,
        instances: vec![Instance {
            id: format!("{id}-i0"),
            status: InstanceStatus::Running,
        }],
    }
}

fn service(id: &str, latest: DeploymentSummary, active: Vec<DeploymentSummary>) -> ServiceInstance {
    ServiceInstance {
        service_id: id.into(),
        service_name: format!("{id}-name"),
        num_replicas: 1,
        latest: Some(latest),
        active,
    }
}

/// svc-a: latest d2 FAILED while d1 still runs. svc-b: latest d3 SUCCESS.
fn services() -> Vec<ServiceInstance> {
    let failed = DeploymentSummary {
        instances: vec![],
        ..deploy("d2", DeploymentStatus::Failed)
    };
    vec![
        service(
            "svc-a",
            failed,
            vec![deploy("d1", DeploymentStatus::Success)],
        ),
        service("svc-b", deploy("d3", DeploymentStatus::Success), vec![]),
    ]
}

#[test]
fn service_focus_restarts_when_the_cursor_moves_and_clears_off_services() {
    let t0 = Instant::now();
    let mut app = app_in_services(t0);
    let t1 = t0 + Duration::from_secs(5);
    app.update(key('j'), t1);
    assert_eq!(app.service_focus, Some(("svc-b".into(), t1)));
    app.update(AppEvent::Tick(t1), t1 + Duration::from_secs(3));
    assert_eq!(app.service_focus, Some(("svc-b".into(), t1)));
    app.update(code(KeyCode::Esc), t1);
    assert_eq!(app.service_focus, None);
}

fn workspaces() -> Vec<Workspace> {
    let ws = |id: &str, name: &str, pid: &str, pname: &str| Workspace {
        id: id.into(),
        name: name.into(),
        projects: vec![ProjectRef {
            id: pid.into(),
            name: pname.into(),
        }],
    };
    vec![
        ws("w1", "Personal", "p1", "Alpha"),
        ws("w2", "Team", "p2", "Beta"),
    ]
}

fn envs() -> Vec<Environment> {
    let env = |id: &str, name: &str| Environment {
        id: id.into(),
        name: name.into(),
    };
    vec![env("e-stg", "staging"), env("e-prod", "production")]
}

fn line(sec: u32, msg: &str) -> LogLine {
    LogLine {
        timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, sec).unwrap(),
        message: msg.into(),
        attributes: vec![],
    }
}

fn data(app: &mut App, d: DataEvent, now: Instant) -> Vec<Command> {
    app.update(AppEvent::Data(d), now)
}

fn envs_event(project_id: &str) -> DataEvent {
    DataEvent::Environments {
        project_id: project_id.into(),
        result: Ok(envs()),
    }
}

fn services_event(env_id: &str, result: Result<Vec<ServiceInstance>, String>) -> DataEvent {
    DataEvent::Services {
        project_id: "p2".into(),
        env_id: env_id.into(),
        result,
    }
}

fn log_event(session: u64, event: FollowEvent) -> DataEvent {
    DataEvent::Log { session, event }
}

fn load_services(env_id: &str) -> Command {
    Command::LoadServices {
        project_id: "p2".into(),
        env_id: env_id.into(),
    }
}

fn prefs_for(project: &str) -> StartPrefs {
    StartPrefs {
        project: Some(project.into()),
        ..StartPrefs::default()
    }
}

/// Project p2 / production, services loaded, Services view on top.
fn app_in_services(now: Instant) -> App {
    let (mut app, _) = App::new(prefs_for("beta"));
    data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
    data(&mut app, envs_event("p2"), now);
    data(&mut app, services_event("e-prod", Ok(services())), now);
    app
}

fn logs_view(app: &App) -> &LogsView {
    match app.stack.last() {
        Some(View::Logs(l)) => l,
        other => panic!("expected logs view, got {other:?}"),
    }
}

#[test]
fn new_loads_workspaces_with_default_backstop() {
    let (app, cmds) = App::new(StartPrefs::default());
    assert_eq!(cmds, vec![Command::LoadWorkspaces]);
    assert_eq!(app.backstop, Duration::from_secs(360));
    assert!(matches!(app.stack.as_slice(), [View::Projects]));
}

#[test]
fn failed_first_workspace_load_is_retried_on_the_backstop() {
    let t0 = Instant::now();
    let (mut app, _) = App::new(StartPrefs::default());
    assert!(app.workspaces.loading);
    data(&mut app, DataEvent::Workspaces(Err("offline".into())), t0);
    assert_eq!(app.workspaces.error.as_deref(), Some("offline"));
    let early = t0 + Duration::from_secs(359);
    assert!(app.update(AppEvent::Tick(early), early).is_empty());
    let due = t0 + Duration::from_secs(360);
    assert_eq!(
        app.update(AppEvent::Tick(due), due),
        vec![Command::LoadWorkspaces]
    );
}

#[test]
fn start_resolves_configured_project_by_name_and_loads_envs() {
    let now = Instant::now();
    let mut prefs = prefs_for("BETA");
    prefs.remembered.last_project_id = Some("p1".into());
    let (mut app, _) = App::new(prefs);
    let cmds = data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
    assert_eq!(
        cmds,
        vec![Command::LoadEnvironments {
            project_id: "p2".into()
        }]
    );
    assert_eq!(app.project.as_ref().map(|p| p.id.as_str()), Some("p2"));
    assert_eq!(app.workspace_id.as_deref(), Some("w2"));
    assert!(matches!(
        app.stack.as_slice(),
        [View::Projects, View::Services]
    ));
}

#[test]
fn start_falls_back_to_remembered_project() {
    let now = Instant::now();
    let mut prefs = prefs_for("no-such-project");
    prefs.remembered.last_project_id = Some("p1".into());
    let (mut app, _) = App::new(prefs);
    let cmds = data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
    assert_eq!(
        cmds,
        vec![Command::LoadEnvironments {
            project_id: "p1".into()
        }]
    );
}

#[test]
fn start_without_prefs_shows_projects() {
    let now = Instant::now();
    let prefs = StartPrefs {
        workspace: Some("team".into()),
        ..StartPrefs::default()
    };
    let (mut app, _) = App::new(prefs);
    let cmds = data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
    assert!(cmds.is_empty());
    assert!(matches!(app.stack.as_slice(), [View::Projects]));
    let ids: Vec<_> = app
        .project_rows()
        .iter()
        .map(|(_, p)| p.id.clone())
        .collect();
    assert_eq!(ids, ["p2", "p1"], "preferred workspace is listed first");
    assert_eq!(app.selected_index(), Some(0));

    let cmds = app.update(code(KeyCode::Enter), now);
    assert_eq!(
        cmds,
        vec![Command::LoadEnvironments {
            project_id: "p2".into()
        }]
    );
    assert!(matches!(
        app.stack.as_slice(),
        [View::Projects, View::Services]
    ));
}

#[test]
fn env_resolution_prefers_production() {
    let now = Instant::now();
    let (mut app, _) = App::new(prefs_for("p2"));
    data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
    let cmds = data(&mut app, envs_event("p2"), now);
    assert_eq!(
        cmds,
        vec![
            load_services("e-prod"),
            Command::SaveRemembered(Remembered {
                last_project_id: Some("p2".into()),
                last_environment_id: Some("e-prod".into()),
                theme: None,
            }),
        ]
    );
    assert_eq!(
        app.environment.as_ref().map(|e| e.id.as_str()),
        Some("e-prod")
    );
}

#[test]
fn env_resolution_prefers_configured_then_remembered() {
    let now = Instant::now();
    let resolve = |prefs: StartPrefs| {
        let (mut app, _) = App::new(prefs);
        data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
        data(&mut app, envs_event("p2"), now);
        app.environment.unwrap().id
    };
    let mut prefs = prefs_for("p2");
    prefs.environment = Some("STAGING".into());
    prefs.remembered.last_environment_id = Some("e-prod".into());
    assert_eq!(resolve(prefs.clone()), "e-stg");
    prefs.environment = None;
    prefs.remembered.last_environment_id = Some("e-stg".into());
    assert_eq!(resolve(prefs), "e-stg");
}

#[test]
fn services_arrival_watches_latest_and_leaving_stops() {
    let now = Instant::now();
    let (mut app, _) = App::new(prefs_for("p2"));
    data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
    data(&mut app, envs_event("p2"), now);
    let watch = Command::WatchDeployments {
        ids: vec!["d2".into(), "d3".into()],
    };
    let cmds = data(&mut app, services_event("e-prod", Ok(services())), now);
    assert_eq!(cmds, vec![watch.clone()]);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert_eq!(
        cmds,
        vec![
            Command::WatchDeployments { ids: vec![] },
            Command::LoadDeployments {
                project_id: "p2".into(),
                env_id: "e-prod".into(),
                service_id: "svc-a".into()
            },
        ]
    );
    assert_eq!(app.update(code(KeyCode::Esc), now), vec![watch]);
}

#[test]
fn l_on_failed_service_opens_build_logs_of_latest() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    let cmds = app.update(key('l'), now);
    assert!(cmds.contains(&Command::OpenLogs {
        session: 1,
        env_id: "e-prod".into(),
        deployment_id: "d2".into(),
        kind: LogKind::Build,
    }));
    let l = logs_view(&app);
    assert_eq!(l.deployment_id, "d2");
    assert_eq!(l.service_name, "svc-a-name");
    assert!(l.follow);
    assert_eq!(l.state, StreamState::Connecting);
}

#[test]
fn deployments_view_shows_failed_latest_then_active_and_enter_opens_row() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(code(KeyCode::Enter), now);
    let history = vec![deploy("d0", DeploymentStatus::Removed)];
    data(
        &mut app,
        DataEvent::Deployments {
            service_id: "svc-a".into(),
            env_id: "e-prod".into(),
            result: Ok(history),
        },
        now,
    );
    let ids: Vec<_> = app
        .deployment_table()
        .iter()
        .map(|r| r.deployment.id.clone())
        .collect();
    assert_eq!(ids, ["d2", "d1", "d0"]);
    app.update(key('j'), now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert_eq!(
        cmds,
        vec![Command::OpenLogs {
            session: 1,
            env_id: "e-prod".into(),
            deployment_id: "d1".into(),
            kind: LogKind::Runtime,
        }]
    );
}

#[test]
fn instances_enter_opens_runtime_logs_for_replica() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('i'), now);
    assert!(matches!(app.stack.last(), Some(View::Instances { .. })));
    let cmds = app.update(code(KeyCode::Enter), now);
    assert!(cmds.contains(&Command::OpenLogs {
        session: 1,
        env_id: "e-prod".into(),
        deployment_id: "d1".into(),
        kind: LogKind::Runtime,
    }));
    assert_eq!(logs_view(&app).replica.as_deref(), Some("d1-i0"));
}

#[test]
fn esc_from_logs_closes_session() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('l'), now);
    let cmds = app.update(code(KeyCode::Esc), now);
    assert_eq!(cmds[0], Command::CloseLogs { session: 1 });
    assert!(matches!(app.stack.last(), Some(View::Services)));
}

#[test]
fn b_toggles_kind_with_a_new_session() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('l'), now);
    let cmds = app.update(key('b'), now);
    assert_eq!(
        cmds,
        vec![
            Command::CloseLogs { session: 1 },
            Command::OpenLogs {
                session: 2,
                env_id: "e-prod".into(),
                deployment_id: "d2".into(),
                kind: LogKind::Runtime,
            },
        ]
    );
    assert_eq!(logs_view(&app).kind, LogKind::Runtime);
}

#[test]
fn ctrl_r_in_logs_reopens_the_same_stream_as_a_new_session() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('l'), now);
    data(
        &mut app,
        log_event(
            1,
            FollowEvent::State(StreamState::Failed("Railway rejected".into())),
        ),
        now,
    );
    let cmds = app.update(ctrl('r'), now);
    assert_eq!(
        cmds,
        vec![
            Command::CloseLogs { session: 1 },
            Command::OpenLogs {
                session: 2,
                env_id: "e-prod".into(),
                deployment_id: "d2".into(),
                kind: LogKind::Build,
            },
        ]
    );
    let l = logs_view(&app);
    assert_eq!((l.session, l.kind), (2, LogKind::Build));
    assert_eq!(l.state, StreamState::Connecting);
    // Events for the old session no longer land.
    data(
        &mut app,
        log_event(1, FollowEvent::Lines(vec![line(1, "old")])),
        now,
    );
    assert!(logs_view(&app).buffer.is_empty());
}

#[test]
fn late_services_for_old_env_ignored() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('e'), now);
    assert_eq!(
        env_picker(&app),
        Some(1),
        "picker starts on the current env"
    );
    // Modal: k moves the picker, it is not a table move.
    app.update(key('k'), now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert!(cmds.contains(&load_services("e-stg")));
    assert!(cmds.contains(&Command::SaveRemembered(Remembered {
        last_project_id: Some("p2".into()),
        last_environment_id: Some("e-stg".into()),
        theme: None,
    })));
    assert_eq!(env_picker(&app), None);
    assert!(app.services.value.is_none());
    assert!(app.services.loading);

    let cmds = data(&mut app, services_event("e-prod", Ok(services())), now);
    assert!(cmds.is_empty());
    assert!(app.services.value.is_none());
    assert!(app.services.loading);
    assert!(app.services.fetched_at.is_none());
}

#[test]
fn late_deployments_and_environments_for_old_context_ignored() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(code(KeyCode::Enter), now); // Deployments for svc-a
    let other = vec![deploy("x", DeploymentStatus::Success)];
    let cmds = data(
        &mut app,
        DataEvent::Deployments {
            service_id: "svc-b".into(),
            env_id: "e-prod".into(),
            result: Ok(other),
        },
        now,
    );
    assert!(cmds.is_empty());
    assert!(app.deployments.value.is_none());
    let cmds = data(
        &mut app,
        DataEvent::Environments {
            project_id: "p1".into(),
            result: Ok(vec![]),
        },
        now,
    );
    assert!(cmds.is_empty());
    assert_eq!(app.environments.value.as_ref().map(Vec::len), Some(2));
}

#[test]
fn log_events_for_closed_session_ignored() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('l'), now);
    app.update(key('b'), now); // closes session 1, opens session 2
    assert_eq!(logs_view(&app).session, 2);
    data(
        &mut app,
        log_event(1, FollowEvent::Lines(vec![line(1, "old")])),
        now,
    );
    data(
        &mut app,
        log_event(1, FollowEvent::State(StreamState::Live)),
        now,
    );
    assert!(logs_view(&app).buffer.is_empty());
    assert_eq!(logs_view(&app).state, StreamState::Connecting);

    data(
        &mut app,
        log_event(2, FollowEvent::Lines(vec![line(1, "new")])),
        now,
    );
    data(
        &mut app,
        log_event(2, FollowEvent::State(StreamState::Live)),
        now,
    );
    assert_eq!(logs_view(&app).buffer.len(), 1);
    assert_eq!(logs_view(&app).state, StreamState::Live);

    // Once the view is gone, its session's events are dropped too.
    app.update(code(KeyCode::Esc), now);
    data(
        &mut app,
        log_event(2, FollowEvent::Lines(vec![line(2, "x")])),
        now,
    );
    assert!(matches!(app.stack.last(), Some(View::Services)));
}

#[test]
fn scroll_up_pauses_follow_and_shift_g_resumes() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('l'), now);
    let lines = (0..10).map(|i| line(i, &format!("l{i}"))).collect();
    data(&mut app, log_event(1, FollowEvent::Lines(lines)), now);
    app.update(key('k'), now);
    assert!(!logs_view(&app).follow);
    assert_eq!(logs_view(&app).scroll, 1);
    app.update(code(KeyCode::Up), now);
    assert_eq!(logs_view(&app).scroll, 2);
    // Lines arriving while paused keep the view anchored.
    let late = vec![line(30, "late")];
    data(&mut app, log_event(1, FollowEvent::Lines(late)), now);
    assert_eq!(logs_view(&app).scroll, 3);

    let shift_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
    app.update(AppEvent::Key(shift_g), now);
    assert!(logs_view(&app).follow);
    assert_eq!(logs_view(&app).scroll, 0);

    app.update(code(KeyCode::PageUp), now);
    assert!(!logs_view(&app).follow);
    assert_eq!(logs_view(&app).scroll, 10, "page up clamps to the top line");
    app.update(key('f'), now);
    assert!(logs_view(&app).follow);
}

#[test]
fn logs_filter_is_modal_and_esc_clears_then_goes_back() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('l'), now);
    app.update(key('/'), now);
    assert!(
        app.update(key('q'), now).is_empty(),
        "q is filter text, not quit"
    );
    app.update(code(KeyCode::Enter), now);
    assert_eq!(filter_input(&app), None);
    assert_eq!(logs_view(&app).filter.as_deref(), Some("q"));
    assert!(app.update(code(KeyCode::Esc), now).is_empty());
    assert_eq!(logs_view(&app).filter, None);
    let cmds = app.update(code(KeyCode::Esc), now);
    assert_eq!(cmds[0], Command::CloseLogs { session: 1 });
}

#[test]
fn table_filter_narrows_rows_and_esc_clears_it_first() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    typed(&mut app, "/SVC-B", now);
    app.update(code(KeyCode::Enter), now);
    let ids: Vec<_> = app
        .service_rows()
        .iter()
        .map(|s| s.service_id.clone())
        .collect();
    assert_eq!(ids, ["svc-b"]);
    assert_eq!(app.selected_index(), Some(0));
    app.update(code(KeyCode::Esc), now);
    assert_eq!(app.table_filter, None);
    assert!(matches!(app.stack.last(), Some(View::Services)));
}

#[test]
fn theme_picker_previews_and_esc_restores() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('t'), now);
    assert_eq!(theme_picker(&app), Some(0));
    app.update(key('j'), now);
    app.update(key('j'), now);
    assert_eq!(app.theme, 2, "moving previews the theme");
    let cmds = app.update(code(KeyCode::Esc), now);
    assert!(cmds.is_empty());
    assert_eq!((app.theme, theme_picker(&app)), (0, None));
    assert!(matches!(app.stack.last(), Some(View::Services)));
}

#[test]
fn theme_picker_enter_saves_choice_with_remembered_ids() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    typed(&mut app, ":theme", now);
    app.update(code(KeyCode::Enter), now);
    assert!(theme_picker(&app).is_some());
    app.update(code(KeyCode::Down), now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert_eq!(theme_picker(&app), None);
    let Some(Command::SaveRemembered(saved)) = cmds.first() else {
        panic!("expected a save, got {cmds:?}");
    };
    assert_eq!(saved.theme.as_deref(), Some(THEMES[1].id));
    assert_eq!(saved.last_environment_id.as_deref(), Some("e-prod"));
    // Keeping the same theme again writes nothing.
    app.update(key('t'), now);
    assert!(app.update(code(KeyCode::Enter), now).is_empty());
}

#[test]
fn start_theme_prefers_remembered_then_config() {
    let mut prefs = StartPrefs {
        theme: Some("Nord".into()),
        ..StartPrefs::default()
    };
    let nord = theme::find("nord").unwrap();
    assert_eq!(App::new(prefs.clone()).0.theme, nord);
    prefs.remembered.theme = Some("dracula".into());
    assert_eq!(
        App::new(prefs.clone()).0.theme,
        theme::find("dracula").unwrap()
    );
    // A stale remembered id falls back to the config.
    prefs.remembered.theme = Some("gone".into());
    assert_eq!(App::new(prefs).0.theme, nord);
    let (app, _) = App::new(StartPrefs {
        theme: Some("nope".into()),
        ..StartPrefs::default()
    });
    assert_eq!(app.theme, 0);
    assert_eq!(
        app.message.as_deref(),
        Some("unknown theme in config: nope")
    );
}

#[test]
fn services_are_listed_alphabetically_ignoring_case() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    let named = |id: &str, name: &str| ServiceInstance {
        service_name: name.into(),
        ..service(id, deploy("d", DeploymentStatus::Success), vec![])
    };
    let list = vec![named("1", "worker"), named("2", "Api"), named("3", "db")];
    data(&mut app, services_event("e-prod", Ok(list)), now);
    let names: Vec<_> = app
        .service_rows()
        .iter()
        .map(|s| s.service_name.clone())
        .collect();
    assert_eq!(names, ["Api", "db", "worker"]);
}

#[test]
fn table_filter_narrows_rows_while_typing() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    typed(&mut app, "/b", now);
    assert_eq!(filter_input(&app), Some("b"));
    let ids: Vec<_> = app
        .service_rows()
        .iter()
        .map(|s| s.service_id.clone())
        .collect();
    assert_eq!(ids, ["svc-b"]);
    assert_eq!(app.selected_index(), Some(0));
    // Esc while typing abandons the search and the full list returns.
    app.update(code(KeyCode::Esc), now);
    assert_eq!(app.service_rows().len(), 2);
    assert!(matches!(app.stack.last(), Some(View::Services)));
}

#[test]
fn backstop_tick_emits_reload_after_interval_only() {
    let t0 = Instant::now();
    let secs = Duration::from_secs;
    let mut app = app_in_services(t0);
    let budget = |blocked_until| DataEvent::Budget {
        interval: secs(60),
        blocked_until,
    };
    data(&mut app, budget(None), t0);
    assert_eq!(app.backstop, secs(60));

    let early = t0 + secs(59);
    assert!(app.update(AppEvent::Tick(early), early).is_empty());
    let t1 = t0 + secs(60);
    assert_eq!(
        app.update(AppEvent::Tick(t1), t1),
        vec![load_services("e-prod")]
    );
    assert!(
        app.update(AppEvent::Tick(t1), t1).is_empty(),
        "no duplicate while loading"
    );

    // While rate limited nothing goes out; afterwards the reload does.
    data(&mut app, services_event("e-prod", Ok(services())), t1);
    let t2 = t1 + secs(60);
    data(&mut app, budget(Some(t2 + secs(10))), t1);
    assert_eq!(app.blocked_until, Some(t2 + secs(10)));
    assert!(app.update(AppEvent::Tick(t2), t2).is_empty());
    let t3 = t2 + secs(10);
    assert_eq!(
        app.update(AppEvent::Tick(t3), t3),
        vec![load_services("e-prod")]
    );
}

#[test]
fn r_reloads_before_the_backstop_except_where_it_reveals() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    let early = now + Duration::from_secs(1);
    assert_eq!(app.update(key('r'), early), vec![load_services("e-prod")]);
    assert!(app.services.loading);

    let mut app = app_in_variables(now);
    assert!(
        app.update(key('r'), now).is_empty(),
        "r reveals in Variables"
    );
}

#[test]
fn load_error_keeps_old_value_and_sets_error() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    let later = now + Duration::from_secs(5);
    assert_eq!(app.update(ctrl('r'), later), vec![load_services("e-prod")]);
    assert!(app.services.loading);
    data(
        &mut app,
        services_event("e-prod", Err("boom".into())),
        later,
    );
    assert_eq!(app.services.value.as_ref().map(Vec::len), Some(2));
    assert_eq!(app.services.error.as_deref(), Some("boom"));
    assert_eq!(app.services.fetched_at, Some(now));
    assert!(!app.services.loading);
}

#[test]
fn terminal_status_event_triggers_service_reload() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    let status_of = |app: &App| {
        let svc = &app.services.value.as_ref().unwrap()[1];
        svc.latest.as_ref().unwrap().status.clone()
    };
    let changed = |status| DataEvent::StatusChanged {
        deployment_id: "d3".into(),
        status,
    };
    assert!(data(&mut app, changed(DeploymentStatus::Deploying), now).is_empty());
    assert_eq!(status_of(&app), DeploymentStatus::Deploying);
    let cmds = data(&mut app, changed(DeploymentStatus::Crashed), now);
    assert_eq!(cmds, vec![load_services("e-prod")]);
    assert_eq!(status_of(&app), DeploymentStatus::Crashed);
}

#[test]
fn colon_q_quits() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key(':'), now);
    assert!(
        app.update(key('q'), now).is_empty(),
        "q is command text, not quit"
    );
    assert_eq!(app.update(code(KeyCode::Enter), now), vec![Command::Quit]);
    assert_eq!(app.update(key('q'), now), vec![Command::Quit]);
}

#[test]
fn command_line_jumps_between_views() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    typed(&mut app, ":logs", now);
    app.update(code(KeyCode::Enter), now);
    assert_eq!(logs_view(&app).session, 1);

    typed(&mut app, ":projects", now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert_eq!(cmds, vec![Command::CloseLogs { session: 1 }]);
    assert!(matches!(app.stack.as_slice(), [View::Projects]));

    typed(&mut app, ":deploys", now);
    app.update(code(KeyCode::Enter), now);
    assert!(matches!(
        app.stack.as_slice(),
        [View::Projects, View::Services, View::Deployments { .. }]
    ));

    typed(&mut app, ":nope", now);
    assert!(app.update(code(KeyCode::Enter), now).is_empty());
    assert!(app.message.is_some());
}

#[test]
fn help_toggles_and_ctrl_c_always_quits() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    app.update(key('?'), now);
    assert!(app.help);
    app.update(key('?'), now);
    assert!(!app.help);
    app.update(key('/'), now);
    assert_eq!(app.update(ctrl('c'), now), vec![Command::Quit]);
}

// ---- variables ----

fn vars(pairs: &[(&str, &str)]) -> Variables {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn vars_event(service_id: &str, result: Result<Variables, String>) -> DataEvent {
    DataEvent::Variables {
        env_id: "e-prod".into(),
        service_id: service_id.into(),
        purpose: VarsPurpose::View,
        result: result.map(Secret::new),
    }
}

fn names(app: &App) -> Vec<String> {
    app.variable_rows()
        .iter()
        .map(|r| r.name.to_string())
        .collect()
}

/// Variables view of svc-a, loaded with a few variables.
fn app_in_variables(now: Instant) -> App {
    let mut app = app_in_services(now);
    app.update(key('v'), now);
    let loaded = vars(&[
        ("DB_URL", "postgres://x"),
        ("PORT", "has db inside"),
        ("REDIS_DB", "1"),
    ]);
    data(&mut app, vars_event("svc-a", Ok(loaded)), now);
    app
}

#[test]
fn v_opens_variables_and_loads() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    let cmds = app.update(key('v'), now);
    // Leaving Services stops its deployment watch first.
    assert_eq!(
        cmds,
        vec![
            Command::WatchDeployments { ids: Vec::new() },
            Command::LoadVariables {
                project_id: "p2".into(),
                env_id: "e-prod".into(),
                service_id: "svc-a".into(),
                purpose: VarsPurpose::View,
            },
        ]
    );
    assert!(matches!(app.stack.last(), Some(View::Variables(_))));
    assert!(app.variables.loading);
    let cmds = app.update(code(KeyCode::Esc), now);
    assert!(matches!(app.stack.as_slice(), [.., View::Services]));
    assert_eq!(
        cmds,
        vec![Command::WatchDeployments {
            ids: vec!["d2".into(), "d3".into()]
        }]
    );
    typed(&mut app, ":vars", now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert!(matches!(
        cmds.last(),
        Some(Command::LoadVariables { service_id, .. }) if service_id == "svc-a"
    ));
}

#[test]
fn ctrl_r_reloads_variables_and_stale_replies_are_ignored() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    let cmds = app.update(ctrl('r'), now);
    assert!(matches!(cmds.as_slice(), [Command::LoadVariables { .. }]));
    data(
        &mut app,
        vars_event("svc-b", Ok(vars(&[("OTHER", "1")]))),
        now,
    );
    assert_eq!(names(&app), ["DB_URL", "PORT", "REDIS_DB"]);
    data(&mut app, vars_event("svc-a", Err("boom".into())), now);
    assert_eq!(app.variables.error.as_deref(), Some("boom"));
    assert_eq!(names(&app).len(), 3, "a failed reload keeps the old rows");
}

#[test]
fn variable_rows_merge_railway_and_pending_sorted() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    let target = crate::pending::Target {
        project_id: "p2",
        env_id: "e-prod",
        env_name: "production",
        service_id: "svc-a",
        service_name: "svc-a-name",
    };
    app.pending_mut()
        .set(&target, "ALPHA_NEW", "v".into(), None);
    app.pending_mut()
        .delete(&target, "PORT", Some("has db inside"));
    app.pending_mut()
        .set(&target, "REDIS_DB", "2".into(), Some("1"));
    let rows = app.variable_rows();
    let got: Vec<_> = rows
        .iter()
        .map(|r| (r.name, r.current, r.pending.is_some()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("ALPHA_NEW", None, true),
            ("DB_URL", Some("postgres://x"), false),
            ("PORT", Some("has db inside"), true),
            ("REDIS_DB", Some("1"), true),
        ]
    );
}

#[test]
fn slash_filters_variables_live_by_name_only() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    app.update(key('/'), now);
    typed(&mut app, "db", now);
    assert_eq!(names(&app), ["DB_URL", "REDIS_DB"]);
    assert!(!names(&app).contains(&"PORT".to_string()));
    app.update(code(KeyCode::Esc), now);
    assert_eq!(names(&app), ["DB_URL", "PORT", "REDIS_DB"]);
}

#[test]
fn search_survives_reload() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    app.update(key('/'), now);
    typed(&mut app, "db", now);
    app.update(code(KeyCode::Enter), now);
    let reloaded = vars(&[("DB_URL", "y"), ("PORT", "db"), ("REDIS_DB", "z")]);
    data(&mut app, vars_event("svc-a", Ok(reloaded)), now);
    assert_eq!(names(&app), ["DB_URL", "REDIS_DB"]);
}

#[test]
fn leaving_variables_rehides_values() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    if let Some(View::Variables(v)) = app.stack.last_mut() {
        v.reveal_all = true;
        v.revealed.insert("PORT".into());
    }
    app.update(code(KeyCode::Esc), now);
    assert!(!app.stack.iter().any(|v| matches!(v, View::Variables(_))));
    app.update(key('v'), now);
    match app.stack.last() {
        Some(View::Variables(v)) => assert!(!v.reveal_all && v.revealed.is_empty()),
        other => panic!("expected variables, got {other:?}"),
    }
}

#[test]
fn pending_error_is_shown_at_start() {
    let (app, _) = App::new(StartPrefs {
        pending_error: Some("pending.toml was unreadable".into()),
        ..StartPrefs::default()
    });
    assert_eq!(app.message.as_deref(), Some("pending.toml was unreadable"));
}

#[test]
fn save_failed_is_shown_and_debug_hides_values() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    data(
        &mut app,
        DataEvent::SaveFailed("could not save".into()),
        now,
    );
    assert_eq!(app.message.as_deref(), Some("could not save"));
    let event = vars_event("svc-a", Ok(vars(&[("K", "s3cret-value")])));
    assert!(!format!("{event:?}").contains("s3cret-value"));
}

// ---- editing ----

fn pending_of(app: &App, name: &str) -> Option<PendingVar> {
    app.pending
        .service("e-prod", "svc-a")
        .and_then(|s| s.vars.get(name))
        .cloned()
}

fn select_var(app: &mut App, name: &str, now: Instant) {
    for _ in 0..10 {
        if app.selection.variable.as_deref() == Some(name) {
            return;
        }
        app.update(key('j'), now);
        if app.selection.variable.is_none() {
            app.selection.variable = app.variable_rows().first().map(|r| r.name.into());
        }
    }
    panic!("variable not found");
}

#[test]
fn edit_commits_set_and_saves() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    select_var(&mut app, "REDIS_DB", now);
    assert!(app.update(key('e'), now).is_empty());
    let input = var_input(&app).unwrap();
    assert_eq!(input.name.as_deref(), Some("REDIS_DB"));
    assert_eq!(*input.text.expose(), "1");
    app.update(key('0'), now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert!(var_input(&app).is_none());
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    let p = pending_of(&app, "REDIS_DB").unwrap();
    assert_eq!(p.change, crate::pending::Change::Set { value: "10".into() });
    assert_eq!(p.base.as_deref(), Some("1"));
}

#[test]
fn esc_cancels_inline_edit() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    select_var(&mut app, "REDIS_DB", now);
    app.update(key('e'), now);
    app.update(key('x'), now);
    assert!(app.update(code(KeyCode::Esc), now).is_empty());
    assert!(var_input(&app).is_none());
    assert!(app.pending.envs.is_empty());
}

#[test]
fn add_validates_name() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    app.update(key('a'), now);
    assert_eq!(var_input(&app).unwrap().name, None);
    typed(&mut app, "1BAD", now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert!(cmds.is_empty());
    assert!(app.message.is_some());
    assert_eq!(var_input(&app).unwrap().name, None);
    for _ in 0..4 {
        app.update(code(KeyCode::Backspace), now);
    }
    typed(&mut app, "GOOD_1", now);
    app.update(code(KeyCode::Enter), now);
    let input = var_input(&app).unwrap();
    assert_eq!(input.name.as_deref(), Some("GOOD_1"));
    assert_eq!(*input.text.expose(), "");
    typed(&mut app, "v", now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    assert_eq!(pending_of(&app, "GOOD_1").unwrap().base, None);
}

#[test]
fn add_existing_name_edits_it() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    app.update(key('a'), now);
    typed(&mut app, "REDIS_DB", now);
    app.update(code(KeyCode::Enter), now);
    let input = var_input(&app).unwrap();
    assert_eq!(input.name.as_deref(), Some("REDIS_DB"));
    assert_eq!(*input.text.expose(), "1");
}

#[test]
fn d_toggles_delete_and_u_undoes() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    select_var(&mut app, "REDIS_DB", now);
    let cmds = app.update(key('d'), now);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    assert_eq!(
        pending_of(&app, "REDIS_DB").unwrap().change,
        crate::pending::Change::Delete
    );
    let cmds = app.update(key('d'), now);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    assert!(pending_of(&app, "REDIS_DB").is_none());
    app.update(key('d'), now);
    let cmds = app.update(key('u'), now);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    assert!(app.pending.envs.is_empty());
    // Nothing to undo: no save.
    assert!(app.update(key('u'), now).is_empty());
}

#[test]
fn editing_a_deleted_variable_starts_from_railway_value() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    select_var(&mut app, "REDIS_DB", now);
    app.update(key('d'), now);
    app.update(key('e'), now);
    assert_eq!(var_input(&app).unwrap().text.expose(), "1");
}

#[test]
fn multiline_value_refuses_inline_edit() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    data(
        &mut app,
        vars_event("svc-a", Ok(vars(&[("KEY", "a\nb")]))),
        now,
    );
    app.selection.variable = Some("KEY".into());
    assert!(app.update(key('e'), now).is_empty());
    assert!(var_input(&app).is_none());
    assert_eq!(
        app.message.as_deref(),
        Some("multi-line value: press E to edit in $EDITOR")
    );
}

#[test]
#[allow(non_snake_case)]
fn E_emits_edit_external_with_current_value() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    select_var(&mut app, "REDIS_DB", now);
    let cmds = app.update(key('E'), now);
    assert_eq!(
        cmds,
        vec![Command::EditExternal {
            name: "REDIS_DB".into(),
            initial: Secret::new("1".into())
        }]
    );
    assert!(!format!("{cmds:?}").contains('1'));
}

#[test]
fn edited_event_sets_pending() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    let cmds = data(
        &mut app,
        DataEvent::Edited {
            name: "REDIS_DB".into(),
            result: Ok(Secret::new("7".into())),
        },
        now,
    );
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    let p = pending_of(&app, "REDIS_DB").unwrap();
    assert_eq!(p.change, crate::pending::Change::Set { value: "7".into() });
    assert_eq!(p.base.as_deref(), Some("1"));
    let cmds = data(
        &mut app,
        DataEvent::Edited {
            name: "REDIS_DB".into(),
            result: Err("editor exited with status 1".into()),
        },
        now,
    );
    assert!(cmds.is_empty());
    assert_eq!(app.message.as_deref(), Some("editor exited with status 1"));
    let event = DataEvent::Edited {
        name: "N".into(),
        result: Ok(Secret::new("s3cret-value".into())),
    };
    assert!(!format!("{event:?}").contains("s3cret-value"));
}

#[test]
#[allow(non_snake_case)]
fn r_and_R_toggle_reveal() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    select_var(&mut app, "PORT", now);
    let revealed = |app: &App| match app.stack.last() {
        Some(View::Variables(v)) => (v.revealed.clone(), v.reveal_all),
        _ => panic!(),
    };
    app.update(key('r'), now);
    assert_eq!(revealed(&app).0.iter().collect::<Vec<_>>(), ["PORT"]);
    app.update(key('r'), now);
    assert!(revealed(&app).0.is_empty());
    app.update(key('R'), now);
    assert!(revealed(&app).1);
    app.update(key('R'), now);
    assert!(!revealed(&app).1);
}

/// REDIS_DB set 1 → 10 and written to Railway (applied), the view reloaded with it.
fn app_with_applied_edit(now: Instant) -> App {
    let mut app = app_in_variables(now);
    select_var(&mut app, "REDIS_DB", now);
    app.update(key('e'), now);
    app.update(key('0'), now);
    app.update(code(KeyCode::Enter), now);
    app.pending_mut()
        .envs
        .get_mut("e-prod")
        .unwrap()
        .get_mut("svc-a")
        .unwrap()
        .vars
        .get_mut("REDIS_DB")
        .unwrap()
        .applied = true;
    let loaded = vars(&[
        ("DB_URL", "postgres://x"),
        ("PORT", "has db inside"),
        ("REDIS_DB", "10"),
    ]);
    data(&mut app, vars_event("svc-a", Ok(loaded)), now);
    app
}

#[test]
fn u_refused_on_an_applied_entry() {
    let now = Instant::now();
    let mut app = app_with_applied_edit(now);
    let before = app.pending.clone();
    assert!(app.update(key('u'), now).is_empty());
    assert_eq!(
        app.message.as_deref(),
        Some("already written to Railway — edit it to revert")
    );
    assert_eq!(app.pending, before);
}

#[test]
fn setting_an_applied_entry_back_sends_the_revert() {
    let now = Instant::now();
    let mut app = app_with_applied_edit(now);
    app.update(key('e'), now);
    assert_eq!(var_input(&app).unwrap().text.expose(), "10");
    app.update(code(KeyCode::Backspace), now);
    let cmds = app.update(code(KeyCode::Enter), now);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    let p = pending_of(&app, "REDIS_DB").expect("the revert stays pending");
    assert_eq!(p.change, crate::pending::Change::Set { value: "1".into() });
    assert_eq!(p.base.as_deref(), Some("10"));
    assert!(!p.applied);
}

#[test]
fn d_on_an_applied_addition_deletes_it_and_d_on_an_applied_delete_is_refused() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    app.update(key('a'), now);
    typed(&mut app, "NEW", now);
    app.update(code(KeyCode::Enter), now);
    typed(&mut app, "n", now);
    app.update(code(KeyCode::Enter), now);
    let sp = app
        .pending_mut()
        .envs
        .get_mut("e-prod")
        .unwrap()
        .get_mut("svc-a")
        .unwrap();
    sp.vars.get_mut("NEW").unwrap().applied = true;
    sp.written_not_deployed = true;
    select_var(&mut app, "NEW", now);
    let cmds = app.update(key('d'), now);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    let p = pending_of(&app, "NEW").expect("the delete stays pending");
    assert_eq!(p.change, crate::pending::Change::Delete);
    assert_eq!(p.base.as_deref(), Some("n"));
    assert!(!p.applied);

    // Now the delete is written too: `d` again would forget it.
    app.pending_mut()
        .envs
        .get_mut("e-prod")
        .unwrap()
        .get_mut("svc-a")
        .unwrap()
        .vars
        .get_mut("NEW")
        .unwrap()
        .applied = true;
    let before = app.pending.clone();
    assert!(app.update(key('d'), now).is_empty());
    assert_eq!(
        app.message.as_deref(),
        Some("already written to Railway — edit it to revert")
    );
    assert_eq!(app.pending, before);
}

#[test]
fn variable_edits_refused_while_the_service_deploys() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    select_var(&mut app, "REDIS_DB", now);
    app.update(key('d'), now);
    app.update(key('p'), now);
    app.update(key('D'), now);
    data(
        &mut app,
        deploy_vars_event("e-prod", "svc-a", vars(&[("REDIS_DB", "1")])),
        now,
    );
    assert!(app
        .update(key('y'), now)
        .iter()
        .any(|c| matches!(c, Command::DeployService { .. })));
    app.update(code(KeyCode::Esc), now);
    assert!(matches!(app.stack.last(), Some(View::Variables(_))));
    let before = app.pending.clone();
    for k in [
        KeyCode::Char('u'),
        KeyCode::Char('d'),
        KeyCode::Char('e'),
        KeyCode::Enter,
        KeyCode::Char('E'),
        KeyCode::Char('a'),
    ] {
        assert!(app.update(code(k), now).is_empty(), "{k:?}");
        assert!(var_input(&app).is_none(), "{k:?}");
        assert!(
            app.message.as_deref().unwrap().contains("is deploying"),
            "{k:?}: {:?}",
            app.message
        );
        assert_eq!(app.pending, before, "{k:?}");
    }
    let edited = DataEvent::Edited {
        name: "REDIS_DB".into(),
        result: Ok(Secret::new("7".into())),
    };
    assert!(data(&mut app, edited, now).is_empty());
    assert_eq!(app.pending, before);
    assert!(app.message.as_deref().unwrap().contains("is deploying"));
    // Revealing is still fine.
    app.update(key('r'), now);
    assert!(!app.message.as_deref().unwrap_or("").contains("deploying"));
}

#[test]
fn ctrl_c_quits_while_editing() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    app.update(key('a'), now);
    assert_eq!(app.update(ctrl('c'), now), vec![Command::Quit]);
}

// ---- review screen ----

fn add_pending(app: &mut App, env: (&str, &str), svc: (&str, &str), name: &str, v: &str) {
    let sp = app
        .pending_mut()
        .envs
        .entry(env.0.into())
        .or_default()
        .entry(svc.0.into())
        .or_default();
    sp.project_id = "p2".into();
    sp.env_name = env.1.into();
    sp.service_name = svc.1.into();
    sp.vars.insert(
        name.into(),
        crate::pending::PendingVar {
            change: crate::pending::Change::Set { value: v.into() },
            base: None,
            applied: false,
        },
    );
}

/// Three groups in two environments plus another project's, then the review open.
/// Production's services include s1 and s2.
fn app_in_review(now: Instant) -> App {
    let mut app = app_in_services(now);
    let mut loaded = services();
    for id in ["s1", "s2"] {
        loaded.push(service(id, deploy("d9", DeploymentStatus::Success), vec![]));
    }
    data(&mut app, services_event("e-prod", Ok(loaded)), now);
    add_pending(&mut app, ("e-stg", "staging"), ("s1", "web"), "A", "1");
    add_pending(
        &mut app,
        ("e-prod", "production"),
        ("s2", "worker"),
        "B",
        "2",
    );
    add_pending(&mut app, ("e-prod", "production"), ("s1", "api"), "C", "3");
    add_pending(&mut app, ("e-x", "other"), ("s1", "api"), "D", "4");
    app.pending_mut()
        .envs
        .get_mut("e-x")
        .unwrap()
        .get_mut("s1")
        .unwrap()
        .project_id = "p9".into();
    app.update(key('p'), now);
    assert!(matches!(app.stack.last(), Some(View::Pending(_))));
    app
}

#[test]
fn review_lists_all_envs_current_first() {
    let now = Instant::now();
    let app = app_in_review(now);
    assert_eq!(app.environment.as_ref().unwrap().id, "e-prod");
    let ids: Vec<(String, String)> = app
        .pending_groups()
        .into_iter()
        .map(|g| (g.key.env_id, g.key.service_id))
        .collect();
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        ids,
        [
            pair("e-prod", "s1"),
            pair("e-prod", "s2"),
            pair("e-stg", "s1")
        ]
    );
}

#[test]
fn same_named_environments_do_not_interleave() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    app.pending_mut().envs.remove("e-stg");
    add_pending(&mut app, ("e-b", "dup"), ("s1", "alpha"), "A", "1");
    add_pending(&mut app, ("e-a", "dup"), ("s1", "beta"), "A", "1");
    add_pending(&mut app, ("e-a", "dup"), ("s2", "zeta"), "A", "1");
    let envs: Vec<String> = app
        .pending_groups()
        .into_iter()
        .skip(2)
        .map(|g| g.key.env_id)
        .collect();
    assert_eq!(envs, ["e-a", "e-a", "e-b"]);
}

#[test]
fn review_selection_moves_between_groups() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    assert_eq!(app.selected_index(), Some(0));
    app.update(key('j'), now);
    app.update(key('j'), now);
    app.update(key('j'), now);
    assert_eq!(app.selected_index(), Some(2));
}

#[test]
fn orphaned_group_is_labelled_and_cannot_deploy() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    add_pending(&mut app, ("e-gone", "preview"), ("s1", "web"), "Z", "9");
    let groups = app.pending_groups();
    assert_eq!(groups.iter().filter(|g| g.orphaned.is_some()).count(), 1);
    // Current env first, then "preview" before "staging".
    assert_eq!(groups[2].orphaned, Some(Orphan::Environment));
    assert_eq!(groups[2].key.env_id, "e-gone");
    for _ in 0..2 {
        app.update(key('j'), now);
    }
    app.update(key('D'), now);
    assert!(app.message.is_some());
    assert!(confirm(&app).is_none());
}

#[test]
fn group_of_a_service_gone_from_the_current_env_is_orphaned() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    add_pending(
        &mut app,
        ("e-prod", "production"),
        ("s-gone", "old"),
        "Z",
        "9",
    );
    // Another environment's services are not loaded: not judged.
    add_pending(&mut app, ("e-stg", "staging"), ("s-other", "x"), "Z", "9");
    let groups = app.pending_groups();
    let orphans: Vec<(&str, Option<Orphan>)> = groups
        .iter()
        .filter(|g| g.orphaned.is_some())
        .map(|g| (g.key.service_id.as_str(), g.orphaned))
        .collect();
    assert_eq!(orphans, [("s-gone", Some(Orphan::Service))]);
    // Groups sort by service name: "old" is after "api" and before "worker".
    assert_eq!(groups[1].key.service_id, "s-gone");
    app.update(key('j'), now);
    assert!(app.update(key('D'), now).is_empty());
    assert_eq!(
        app.message.as_deref(),
        Some("cannot deploy: the service no longer exists")
    );
    // Not judged before the services list has loaded.
    app.services = Loaded::default();
    assert!(app.pending_groups().iter().all(|g| g.orphaned.is_none()));
}

#[test]
fn discard_requires_y() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    app.update(key('x'), now);
    assert_eq!(
        confirm(&app),
        Some(&Confirm::Discard {
            env_id: "e-prod".into(),
            service_id: "s1".into()
        })
    );
    assert!(app.update(key('n'), now).is_empty());
    assert!(confirm(&app).is_none());
    assert_eq!(app.pending_groups().len(), 3);

    app.update(key('x'), now);
    let cmds = app.update(key('y'), now);
    assert!(confirm(&app).is_none());
    assert_eq!(app.pending_groups().len(), 2);
    assert!(
        matches!(cmds.as_slice(), [Command::SavePending(p)] if p.service("e-prod", "s1").is_none())
    );
}

#[test]
fn pending_command_and_p_from_variables_open_review() {
    let now = Instant::now();
    let mut app = app_in_variables(now);
    app.update(key('p'), now);
    assert!(matches!(app.stack.last(), Some(View::Pending(_))));
    app.update(code(KeyCode::Esc), now);
    assert!(matches!(app.stack.last(), Some(View::Variables(_))));
    let mut app = app_in_services(now);
    app.update(key(':'), now);
    typed(&mut app, "pending", now);
    app.update(code(KeyCode::Enter), now);
    assert!(matches!(app.stack.last(), Some(View::Pending(_))));
}

#[test]
#[allow(non_snake_case)]
fn review_r_and_R_toggle_reveal_all() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    let all = |app: &App| match app.stack.last() {
        Some(View::Pending(v)) => v.reveal_all,
        _ => panic!(),
    };
    app.update(key('r'), now);
    assert!(all(&app));
    app.update(key('R'), now);
    assert!(!all(&app));
}

// ---- deploy ----

fn deploy_vars_event(env_id: &str, service_id: &str, current: Variables) -> DataEvent {
    DataEvent::Variables {
        env_id: env_id.into(),
        service_id: service_id.into(),
        purpose: VarsPurpose::Deploy,
        result: Ok(Secret::new(current)),
    }
}

fn step(env_id: &str, service_id: &str, step: DeployStep) -> DataEvent {
    DataEvent::Deploy {
        env_id: env_id.into(),
        service_id: service_id.into(),
        step,
    }
}

fn sp_mut<'a>(app: &'a mut App, env: &str, svc: &str) -> &'a mut crate::pending::ServicePending {
    app.pending_mut()
        .envs
        .get_mut(env)
        .unwrap()
        .get_mut(svc)
        .unwrap()
}

/// The review open on (e-prod, s1 "api"): `D`, the check's reply, then `y`.
fn deploy_first_group(app: &mut App, current: Variables, now: Instant) -> Vec<Command> {
    let cmds = app.update(key('D'), now);
    assert_eq!(
        cmds,
        vec![Command::LoadVariables {
            project_id: "p2".into(),
            env_id: "e-prod".into(),
            service_id: "s1".into(),
            purpose: VarsPurpose::Deploy,
        }]
    );
    assert!(confirm(app).is_none(), "no confirm before Railway is read");
    assert!(data(app, deploy_vars_event("e-prod", "s1", current), now).is_empty());
    assert!(matches!(confirm(app), Some(Confirm::Deploy(_))));
    app.update(key('y'), now)
}

fn deploy_cmd(cmds: &[Command]) -> (Variables, Vec<String>, bool) {
    match cmds {
        [Command::DeployService {
            upserts,
            deletes,
            redeploy_only,
            ..
        }] => (upserts.expose().clone(), deletes.clone(), *redeploy_only),
        other => panic!("expected one DeployService, got {other:?}"),
    }
}

fn delete_var(base: &str) -> PendingVar {
    PendingVar {
        change: crate::pending::Change::Delete,
        base: Some(base.into()),
        applied: false,
    }
}

#[test]
fn deploy_targets_group_not_current_env() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    // Switch to staging and queue an edit there.
    app.update(key('e'), now);
    app.update(key('k'), now);
    app.update(code(KeyCode::Enter), now);
    assert_eq!(app.env_id(), Some("e-stg"));
    data(&mut app, services_event("e-stg", Ok(services())), now);
    app.update(key('v'), now);
    app.update(key('a'), now);
    typed(&mut app, "NEW", now);
    app.update(code(KeyCode::Enter), now);
    typed(&mut app, "v", now);
    app.update(code(KeyCode::Enter), now);
    assert!(app.pending.service("e-stg", "svc-a").is_some());
    // Back to production, then deploy the staging group from the review.
    app.update(code(KeyCode::Esc), now);
    app.update(key('e'), now);
    app.update(key('j'), now);
    app.update(code(KeyCode::Enter), now);
    assert_eq!(app.env_id(), Some("e-prod"));
    app.update(key('p'), now);
    let cmds = app.update(key('D'), now);
    assert_eq!(
        cmds,
        vec![Command::LoadVariables {
            project_id: "p2".into(),
            env_id: "e-stg".into(),
            service_id: "svc-a".into(),
            purpose: VarsPurpose::Deploy,
        }]
    );
    data(
        &mut app,
        deploy_vars_event("e-stg", "svc-a", vars(&[])),
        now,
    );
    let cmds = app.update(key('y'), now);
    assert_eq!(
        cmds,
        vec![Command::DeployService {
            project_id: "p2".into(),
            env_id: "e-stg".into(),
            service_id: "svc-a".into(),
            upserts: Secret::new(vars(&[("NEW", "v")])),
            deletes: vec![],
            redeploy_only: false,
        }]
    );
}

#[test]
fn drift_listed_in_confirm() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    let set = |v: &str, base: &str, applied| PendingVar {
        change: crate::pending::Change::Set { value: v.into() },
        base: Some(base.into()),
        applied,
    };
    let sp = sp_mut(&mut app, "e-prod", "s1");
    sp.vars.insert("X".into(), set("new", "old", false));
    sp.vars.insert("Y".into(), delete_var("keep"));
    sp.vars.insert("Z".into(), set("new", "same", false));
    sp.vars.insert("W".into(), set("new", "b", true));
    app.update(key('D'), now);
    // C was added and now exists; X changed; Y is gone; Z is as edited; W is already applied.
    let current = vars(&[("C", "theirs"), ("X", "other"), ("Z", "same"), ("W", "new")]);
    data(&mut app, deploy_vars_event("e-prod", "s1", current), now);
    assert_eq!(
        confirm(&app),
        Some(&Confirm::Deploy(DeployConfirm {
            env_id: "e-prod".into(),
            service_id: "s1".into(),
            drift: vec!["C".into(), "X".into(), "Y".into()],
        }))
    );
}

#[test]
fn only_y_deploys() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    for cancel in [key('n'), key('Y'), code(KeyCode::Enter), code(KeyCode::Esc)] {
        app.update(key('D'), now);
        data(&mut app, deploy_vars_event("e-prod", "s1", vars(&[])), now);
        assert!(confirm(&app).is_some());
        assert!(app.update(cancel, now).is_empty());
        assert!(confirm(&app).is_none());
    }
    assert!(matches!(app.stack.last(), Some(View::Pending(_))));
    let cmds = deploy_first_group(&mut app, vars(&[]), now);
    assert_eq!(deploy_cmd(&cmds), (vars(&[("C", "3")]), vec![], false));
}

#[test]
fn d_refused_without_environments_or_while_deploying() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    let envs = app.environments.value.take();
    assert!(app.update(key('D'), now).is_empty());
    assert!(app.message.as_deref().unwrap().contains("not loaded"));
    app.environments.value = envs;
    deploy_first_group(&mut app, vars(&[]), now);
    assert!(app.update(key('D'), now).is_empty());
    assert!(app
        .message
        .as_deref()
        .unwrap()
        .contains("already deploying"));
    assert!(confirm(&app).is_none());
}

#[test]
fn steps_mark_applied_then_redeployed_discards_and_reloads() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    add_pending(&mut app, ("e-prod", "production"), ("s1", "api"), "N", "n");
    sp_mut(&mut app, "e-prod", "s1")
        .vars
        .insert("OLD".into(), delete_var("x"));
    let cmds = deploy_first_group(&mut app, vars(&[("OLD", "x")]), now);
    assert_eq!(
        deploy_cmd(&cmds),
        (vars(&[("C", "3"), ("N", "n")]), vec!["OLD".into()], false)
    );
    let cmds = data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
    let sp = app.pending.service("e-prod", "s1").unwrap();
    assert!(sp.vars["C"].applied && sp.vars["N"].applied && !sp.vars["OLD"].applied);
    assert!(!sp.written_not_deployed);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(p)] if p == &app.pending));
    let deleted = DeployStep::Deleted("OLD".into());
    let cmds = data(&mut app, step("e-prod", "s1", deleted), now);
    let sp = app.pending.service("e-prod", "s1").unwrap();
    assert!(sp.vars.values().all(|v| v.applied) && sp.written_not_deployed);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(p)] if p == &app.pending));
    let cmds = data(&mut app, step("e-prod", "s1", DeployStep::Redeployed), now);
    assert!(app.pending.service("e-prod", "s1").is_none());
    assert_eq!(app.message.as_deref(), Some("deployed api to production"));
    assert!(matches!(cmds.first(), Some(Command::SavePending(p)) if p == &app.pending));
    assert!(cmds.contains(&load_services("e-prod")), "{cmds:?}");
}

#[test]
fn edit_during_deploy_is_not_marked_applied() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    deploy_first_group(&mut app, vars(&[]), now);
    // C is edited again while its upsert is on the way.
    sp_mut(&mut app, "e-prod", "s1")
        .vars
        .get_mut("C")
        .unwrap()
        .change = crate::pending::Change::Set { value: "4".into() };
    data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
    assert!(!app.pending.service("e-prod", "s1").unwrap().vars["C"].applied);
    data(&mut app, step("e-prod", "s1", DeployStep::Redeployed), now);
    let sp = app.pending.service("e-prod", "s1").unwrap();
    assert_eq!(sp.vars.len(), 1, "the unsent edit is kept");
    assert!(!sp.written_not_deployed);
}

#[test]
fn redeploy_failure_retry_is_redeploy_only() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    deploy_first_group(&mut app, vars(&[]), now);
    data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
    let failed = DeployStep::Failed {
        during: Phase::Redeploy,
        error: "Railway API returned HTTP 400".into(),
        failure: WriteFailure::Refused,
    };
    assert!(data(&mut app, step("e-prod", "s1", failed), now).is_empty());
    assert_eq!(
        app.message.as_deref(),
        Some("deploy failed during redeploy: Railway API returned HTTP 400")
    );
    let sp = app.pending.service("e-prod", "s1").unwrap();
    assert!(sp.vars["C"].applied && sp.written_not_deployed);
    // C now matches Railway, but it is applied: no drift.
    let cmds = deploy_first_group(&mut app, vars(&[("C", "3")]), now);
    assert_eq!(deploy_cmd(&cmds), (vars(&[]), vec![], true));
}

#[test]
fn failed_write_keeps_entries_and_retry_sends_only_unapplied() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    sp_mut(&mut app, "e-prod", "s1")
        .vars
        .insert("OLD".into(), delete_var("x"));
    deploy_first_group(&mut app, vars(&[("OLD", "x")]), now);
    data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
    let failed = DeployStep::Failed {
        during: Phase::Delete("OLD".into()),
        error: "Railway API returned HTTP 400".into(),
        failure: WriteFailure::Refused,
    };
    data(&mut app, step("e-prod", "s1", failed), now);
    assert_eq!(
        app.message.as_deref(),
        Some("deploy failed during delete OLD: Railway API returned HTTP 400")
    );
    let sp = app.pending.service("e-prod", "s1").unwrap();
    assert!(sp.vars["C"].applied && !sp.vars["OLD"].applied && !sp.written_not_deployed);
    let cmds = deploy_first_group(&mut app, vars(&[("OLD", "x"), ("C", "3")]), now);
    assert_eq!(deploy_cmd(&cmds), (vars(&[]), vec!["OLD".into()], false));
}

#[test]
fn unknown_write_reconciles_and_unknown_redeploy_is_reported() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    add_pending(&mut app, ("e-prod", "production"), ("s1", "api"), "N", "n");
    deploy_first_group(&mut app, vars(&[]), now);
    let unknown = DeployStep::Failed {
        during: Phase::Upsert,
        error: "timed out".into(),
        failure: WriteFailure::Unknown,
    };
    let cmds = data(&mut app, step("e-prod", "s1", unknown), now);
    assert_eq!(
        cmds,
        vec![Command::LoadVariables {
            project_id: "p2".into(),
            env_id: "e-prod".into(),
            service_id: "s1".into(),
            purpose: VarsPurpose::Reconcile,
        }]
    );
    // Still in flight until Railway has been read.
    assert!(app.update(key('D'), now).is_empty());
    let reply = DataEvent::Variables {
        env_id: "e-prod".into(),
        service_id: "s1".into(),
        purpose: VarsPurpose::Reconcile,
        result: Ok(Secret::new(vars(&[("C", "3"), ("N", "other")]))),
    };
    let cmds = data(&mut app, reply, now);
    let sp = app.pending.service("e-prod", "s1").unwrap();
    assert!(sp.vars["C"].applied && !sp.vars["N"].applied);
    assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
    // Retry: only N; then the redeploy's outcome is unknown.
    let cmds = deploy_first_group(&mut app, vars(&[("C", "3")]), now);
    assert_eq!(deploy_cmd(&cmds), (vars(&[("N", "n")]), vec![], false));
    data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
    let unknown = DeployStep::Failed {
        during: Phase::Redeploy,
        error: "timed out".into(),
        failure: WriteFailure::Unknown,
    };
    assert!(data(&mut app, step("e-prod", "s1", unknown), now).is_empty());
    assert_eq!(
        app.message.as_deref(),
        Some("redeploy outcome unknown — check the deployments view")
    );
    let sp = app.pending.service("e-prod", "s1").unwrap();
    assert!(sp.written_not_deployed);
}

#[test]
fn deploy_debug_hides_values() {
    let cmd = Command::DeployService {
        project_id: "p".into(),
        env_id: "e".into(),
        service_id: "s".into(),
        upserts: Secret::new(vars(&[("KEY", "s3cret-value")])),
        deletes: vec!["OLD".into()],
        redeploy_only: false,
    };
    let out = format!("{cmd:?}");
    assert!(out.contains("OLD") && out.contains("<redacted>"), "{out}");
    assert!(!out.contains("s3cret-value"), "{out}");
    let ev = deploy_vars_event("e", "s", vars(&[("KEY", "s3cret-value")]));
    assert!(!format!("{ev:?}").contains("s3cret-value"));
}

#[test]
fn debug_never_prints_a_secret() {
    const SECRET: &str = "s3cret-value";
    let value = || Secret::new(SECRET.to_string());
    let input = VarInput {
        name: Some("KEY".into()),
        text: value(),
    };
    let shown = [
        format!("{input:?}"),
        format!("{:?}", Modal::VarInput(input.clone())),
        format!(
            "{:?}",
            Command::EditExternal {
                name: "KEY".into(),
                initial: value(),
            }
        ),
        format!(
            "{:?}",
            Command::DeployService {
                project_id: "p".into(),
                env_id: "e".into(),
                service_id: "s".into(),
                upserts: Secret::new(vars(&[("KEY", SECRET)])),
                deletes: Vec::new(),
                redeploy_only: false,
            }
        ),
        format!(
            "{:?}",
            DataEvent::Edited {
                name: "KEY".into(),
                result: Ok(value()),
            }
        ),
        format!(
            "{:?}",
            DataEvent::Variables {
                env_id: "e".into(),
                service_id: "s".into(),
                purpose: VarsPurpose::View,
                result: Ok(Secret::new(vars(&[("KEY", SECRET)]))),
            }
        ),
    ];
    for out in shown {
        assert!(!out.contains(SECRET), "{out}");
        assert!(out.contains("<redacted>"), "{out}");
    }
}

fn no_deploy(cmds: &[Command]) -> bool {
    !cmds
        .iter()
        .any(|c| matches!(c, Command::DeployService { .. }))
}

#[test]
fn deploy_check_never_pops_over_typing() {
    let now = Instant::now();
    // Typing a variable value in another view.
    let mut app = app_in_review(now);
    app.update(key('D'), now);
    app.update(code(KeyCode::Esc), now);
    app.update(key('v'), now);
    app.update(key('a'), now);
    typed(&mut app, "KE", now);
    data(&mut app, deploy_vars_event("e-prod", "s1", vars(&[])), now);
    assert!(confirm(&app).is_none());
    assert!(app.message.as_deref().unwrap().contains("press D again"));
    assert!(no_deploy(&app.update(key('y'), now)));
    assert_eq!(var_input(&app).unwrap().text.expose(), "KEy");

    // A filter being typed on the review itself, and the command line.
    for start in [key('/'), key(':')] {
        let mut app = app_in_review(now);
        app.update(key('D'), now);
        app.update(start, now);
        data(&mut app, deploy_vars_event("e-prod", "s1", vars(&[])), now);
        assert!(confirm(&app).is_none());
        assert!(no_deploy(&app.update(key('y'), now)));
    }

    // Help open on the review.
    let mut app = app_in_review(now);
    app.update(key('D'), now);
    app.update(key('?'), now);
    data(&mut app, deploy_vars_event("e-prod", "s1", vars(&[])), now);
    assert!(confirm(&app).is_none());
}

#[test]
fn discard_refused_while_deploying() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    deploy_first_group(&mut app, vars(&[]), now);
    assert!(app.update(key('x'), now).is_empty());
    assert!(confirm(&app).is_none());
    assert!(app.message.as_deref().unwrap().contains("deploying"));
    assert!(app.pending.service("e-prod", "s1").is_some());
}

const DEPLOY_IN_PROGRESS: &str =
    "a deploy is in progress — wait for it to finish, or press ctrl-c twice to force quit";

#[test]
fn quit_refused_while_deploying_and_ctrl_c_twice_forces_it() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    deploy_first_group(&mut app, vars(&[]), now);
    assert!(app.update(key('q'), now).is_empty());
    assert_eq!(app.message.as_deref(), Some(DEPLOY_IN_PROGRESS));
    typed(&mut app, ":q", now);
    assert!(app.update(code(KeyCode::Enter), now).is_empty());
    assert_eq!(app.message.as_deref(), Some(DEPLOY_IN_PROGRESS));
    assert!(app.update(ctrl('c'), now).is_empty());
    assert_eq!(app.message.as_deref(), Some(DEPLOY_IN_PROGRESS));
    assert_eq!(app.update(ctrl('c'), now), vec![Command::Quit]);
}

#[test]
fn ctrl_c_arming_ends_with_the_deploy() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    deploy_first_group(&mut app, vars(&[]), now);
    assert!(app.update(ctrl('c'), now).is_empty());
    let failed = DeployStep::Failed {
        during: Phase::Upsert,
        error: "Railway API returned HTTP 400".into(),
        failure: WriteFailure::Refused,
    };
    data(&mut app, step("e-prod", "s1", failed), now);
    // A new deploy is a new in-flight period: one ctrl-c only warns.
    deploy_first_group(&mut app, vars(&[]), now);
    assert!(app.update(ctrl('c'), now).is_empty());
    assert_eq!(app.message.as_deref(), Some(DEPLOY_IN_PROGRESS));
    data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
    data(&mut app, step("e-prod", "s1", DeployStep::Redeployed), now);
    assert_eq!(app.update(key('q'), now), vec![Command::Quit]);
}

#[test]
fn progress_is_shown_step_by_step() {
    let now = Instant::now();
    let mut app = app_in_review(now);
    sp_mut(&mut app, "e-prod", "s1")
        .vars
        .insert("OLD".into(), delete_var("x"));
    deploy_first_group(&mut app, vars(&[("OLD", "x")]), now);
    assert_eq!(app.message.as_deref(), Some("deploying api to production…"));
    data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
    assert_eq!(
        app.message.as_deref(),
        Some("deploying api to production: variables written…")
    );
    let deleted = DeployStep::Deleted("OLD".into());
    data(&mut app, step("e-prod", "s1", deleted), now);
    assert_eq!(
        app.message.as_deref(),
        Some("deploying api to production: OLD deleted…")
    );
}

// ---- deploy latest commit ----

fn git_deploy(id: &str, sha: &str, msg: &str) -> DeploymentSummary {
    DeploymentSummary {
        commit_sha: Some(sha.into()),
        commit_message: Some(msg.into()),
        repo: Some("acme/api".into()),
        branch: Some("main".into()),
        ..deploy(id, DeploymentStatus::Success)
    }
}

/// Services open on production with svc-a built from acme/api@main.
fn app_with_git_service(now: Instant) -> App {
    let mut app = app_in_services(now);
    let svc = service(
        "svc-a",
        git_deploy("d2", "bbbbbbb2", "newer"),
        vec![git_deploy("d1", "aaaaaaa1", "running")],
    );
    data(&mut app, services_event("e-prod", Ok(vec![svc])), now);
    app
}

fn latest_outcome(outcome: LatestOutcome) -> DataEvent {
    DataEvent::DeployedLatest {
        env_id: "e-prod".into(),
        service_id: "svc-a".into(),
        outcome,
    }
}

#[test]
fn shift_d_on_a_service_asks_with_what_will_be_deployed() {
    let now = Instant::now();
    let mut app = app_with_git_service(now);
    assert_eq!(app.update(key('D'), now), vec![]);
    let Some(Confirm::DeployLatest(c)) = confirm(&app) else {
        panic!(
            "expected the latest-commit confirm, got {:?}",
            confirm(&app)
        );
    };
    assert_eq!(
        (&c.env_id[..], &c.env_name[..], &c.service_name[..]),
        ("e-prod", "production", "svc-a-name")
    );
    assert_eq!(c.repo.as_deref(), Some("acme/api"));
    assert_eq!(c.branch.as_deref(), Some("main"));
    // What is running, not the newest attempt.
    assert_eq!(c.current.as_ref().map(|d| d.id.as_str()), Some("d1"));
    assert_eq!(c.written, 0);
}

#[test]
fn y_sends_the_deploy_and_any_other_key_cancels() {
    let now = Instant::now();
    let mut app = app_with_git_service(now);
    app.update(key('D'), now);
    assert_eq!(app.update(key('n'), now), vec![]);
    assert!(confirm(&app).is_none());

    app.update(key('D'), now);
    assert_eq!(
        app.update(key('y'), now),
        vec![Command::DeployLatest {
            env_id: "e-prod".into(),
            service_id: "svc-a".into(),
            switch: None,
        }]
    );
    assert_eq!(
        app.message.as_deref(),
        Some("deploying the latest commit of svc-a-name to production…")
    );
    // A second D while it is in flight is refused, and so is quitting.
    app.update(key('D'), now);
    assert!(confirm(&app).is_none());
    assert_eq!(app.message.as_deref(), Some("already deploying svc-a-name"));
    assert_eq!(app.update(key('q'), now), vec![]);
}

#[test]
fn shift_d_in_deployments_deploys_that_service() {
    let now = Instant::now();
    let mut app = app_with_git_service(now);
    app.update(code(KeyCode::Enter), now);
    assert_eq!(app.kind(), Kind::Deployments);
    app.update(key('D'), now);
    assert!(matches!(
        confirm(&app),
        Some(Confirm::DeployLatest(c)) if c.service_id == "svc-a"
    ));
    let cmds = app.update(key('y'), now);
    assert!(matches!(cmds.as_slice(), [Command::DeployLatest { .. }]));
    // Railway accepted it: the open deployments list reloads.
    let cmds = data(&mut app, latest_outcome(LatestOutcome::Started), now);
    assert!(cmds.iter().any(
        |c| matches!(c, Command::LoadDeployments { service_id, .. } if service_id == "svc-a")
    ));
    assert!(cmds
        .iter()
        .any(|c| matches!(c, Command::LoadServices { .. })));
    assert_eq!(app.update(key('q'), now), vec![Command::Quit]);
}

#[test]
fn started_deploy_ships_written_variables_and_keeps_unwritten_ones() {
    let now = Instant::now();
    let mut app = app_with_git_service(now);
    let t = crate::pending::Target {
        project_id: "p2",
        env_id: "e-prod",
        env_name: "production",
        service_id: "svc-a",
        service_name: "svc-a-name",
    };
    app.pending_mut().set(&t, "WRITTEN", "1".into(), None);
    app.pending_mut().set(&t, "LATER", "2".into(), None);
    sp_mut(&mut app, "e-prod", "svc-a")
        .vars
        .get_mut("WRITTEN")
        .unwrap()
        .applied = true;
    app.update(key('D'), now);
    assert!(matches!(confirm(&app), Some(Confirm::DeployLatest(c)) if c.written == 1));
    app.update(key('y'), now);
    let cmds = data(&mut app, latest_outcome(LatestOutcome::Started), now);
    assert!(cmds.iter().any(|c| matches!(c, Command::SavePending(_))));
    let sp = app.pending.service("e-prod", "svc-a").unwrap();
    assert_eq!(sp.vars.keys().collect::<Vec<_>>(), ["LATER"]);
    assert!(!sp.written_not_deployed);
}

#[test]
fn refused_and_unknown_deploys_are_reported() {
    let now = Instant::now();
    let mut app = app_with_git_service(now);
    app.update(key('D'), now);
    app.update(key('y'), now);
    data(
        &mut app,
        latest_outcome(LatestOutcome::Failed {
            error: "Not Authorized".into(),
            failure: WriteFailure::Refused,
        }),
        now,
    );
    assert_eq!(
        app.message.as_deref(),
        Some("deploy of svc-a-name failed: Not Authorized")
    );
    app.update(key('D'), now);
    app.update(key('y'), now);
    data(
        &mut app,
        latest_outcome(LatestOutcome::Failed {
            error: "timeout".into(),
            failure: WriteFailure::Unknown,
        }),
        now,
    );
    assert_eq!(
        app.message.as_deref(),
        Some("deploy of svc-a-name outcome unknown — check the deployments view")
    );
}

#[test]
fn an_image_service_has_no_commit_to_deploy() {
    let now = Instant::now();
    let mut app = app_in_services(now);
    let image = DeploymentSummary {
        image: Some("postgres:16".into()),
        ..deploy("d1", DeploymentStatus::Success)
    };
    data(
        &mut app,
        services_event("e-prod", Ok(vec![service("svc-a", image, vec![])])),
        now,
    );
    app.update(key('D'), now);
    assert!(confirm(&app).is_none());
    assert_eq!(
        app.message.as_deref(),
        Some("cannot deploy svc-a-name: it runs an image, not a repo")
    );
}

// ---- deploy a picked branch ----

/// The picker's reply with the branches listed.
fn branches_event(
    service_id: &str,
    result: Result<(Vec<Trigger>, Vec<String>), String>,
) -> DataEvent {
    DataEvent::Branches {
        env_id: "e-prod".into(),
        service_id: service_id.into(),
        result: result.map(|(triggers, names)| BranchReply {
            triggers,
            names: Ok(names),
        }),
    }
}

fn trigger() -> Trigger {
    Trigger {
        id: "t1".into(),
        branch: "main".into(),
        repository: "acme/api".into(),
    }
}

/// The picker open on svc-a with branches loaded (connected: main).
fn app_with_picker(now: Instant) -> App {
    let mut app = app_with_git_service(now);
    let cmds = app.update(key('d'), now);
    assert_eq!(
        cmds,
        vec![Command::LoadBranches {
            project_id: "p2".into(),
            env_id: "e-prod".into(),
            service_id: "svc-a".into(),
            repo: "acme/api".into(),
        }]
    );
    let names = ["release/2", "feat/Login", "main", "feat/logout"];
    data(
        &mut app,
        branches_event(
            "svc-a",
            Ok((
                vec![trigger()],
                names.iter().map(|n| n.to_string()).collect(),
            )),
        ),
        now,
    );
    app
}

fn picked(app: &App) -> Vec<&str> {
    branch_picker(app).unwrap().filtered()
}

#[test]
fn d_lists_branches_connected_first_and_typing_filters() {
    let now = Instant::now();
    let mut app = app_with_picker(now);
    assert_eq!(
        picked(&app),
        ["main", "feat/Login", "feat/logout", "release/2"]
    );
    // Letters go to the query, including j and k.
    typed(&mut app, "LOG", now);
    assert_eq!(branch_picker(&app).unwrap().query, "LOG");
    assert_eq!(picked(&app), ["feat/Login", "feat/logout"]);
    app.update(code(KeyCode::Down), now);
    app.update(code(KeyCode::Down), now);
    assert_eq!(branch_picker(&app).unwrap().index, 1, "stops at the last");
    app.update(code(KeyCode::Backspace), now);
    assert_eq!(branch_picker(&app).unwrap().index, 0, "editing resets");
    typed(&mut app, "zzz", now);
    assert!(picked(&app).is_empty());
    app.update(code(KeyCode::Enter), now);
    assert!(confirm(&app).is_none(), "nothing to pick");
    app.update(code(KeyCode::Esc), now);
    assert!(branch_picker(&app).is_none());
    assert_eq!(app.kind(), Kind::Services, "esc only closes the picker");
}

#[test]
fn picking_another_branch_confirms_the_switch_then_y_sends_it() {
    let now = Instant::now();
    let mut app = app_with_picker(now);
    typed(&mut app, "logout", now);
    app.update(code(KeyCode::Enter), now);
    assert!(branch_picker(&app).is_none());
    let Some(Confirm::DeployLatest(c)) = confirm(&app) else {
        panic!("expected the confirm, got {:?}", confirm(&app));
    };
    let switch = BranchSwitch {
        trigger_id: "t1".into(),
        from: "main".into(),
        to: "feat/logout".into(),
    };
    assert_eq!(c.switch.as_ref(), Some(&switch));
    assert_eq!(c.branch.as_deref(), Some("feat/logout"));
    assert_eq!(
        app.update(key('y'), now),
        vec![Command::DeployLatest {
            env_id: "e-prod".into(),
            service_id: "svc-a".into(),
            switch: Some(switch),
        }]
    );
    assert_eq!(
        app.message.as_deref(),
        Some("switching svc-a-name to feat/logout and deploying to production…")
    );
    data(&mut app, latest_outcome(LatestOutcome::Started), now);
    assert_eq!(
        app.message.as_deref(),
        Some("switched svc-a-name to feat/logout; deploying the latest commit of svc-a-name to production: Railway accepted it")
    );
}

#[test]
fn picking_the_connected_branch_deploys_without_switching() {
    let now = Instant::now();
    let mut app = app_with_picker(now);
    app.update(code(KeyCode::Enter), now);
    let Some(Confirm::DeployLatest(c)) = confirm(&app) else {
        panic!("expected the confirm");
    };
    assert_eq!(c.switch, None);
    assert_eq!(c.branch.as_deref(), Some("main"));
}

#[test]
fn a_failed_switch_says_nothing_was_deployed() {
    let now = Instant::now();
    let mut app = app_with_picker(now);
    typed(&mut app, "release", now);
    app.update(code(KeyCode::Enter), now);
    app.update(key('y'), now);
    let failed = LatestOutcome::SwitchFailed {
        error: "Not Authorized".into(),
        failure: WriteFailure::Refused,
    };
    data(&mut app, latest_outcome(failed), now);
    assert_eq!(
        app.message.as_deref(),
        Some("could not switch svc-a-name to release/2: Not Authorized; nothing was deployed")
    );
    app.update(key('d'), now);
    data(
        &mut app,
        branches_event("svc-a", Ok((vec![trigger()], vec!["release/2".into()]))),
        now,
    );
    app.update(code(KeyCode::Enter), now);
    assert!(confirm(&app).is_some());
}

#[test]
fn picker_errors_stale_replies_and_image_services() {
    let now = Instant::now();
    let mut app = app_with_git_service(now);
    app.update(key('d'), now);
    // A reply for another service is ignored.
    data(
        &mut app,
        branches_event("svc-b", Ok((vec![trigger()], vec![]))),
        now,
    );
    assert_eq!(branch_picker(&app).unwrap().choices, None);
    data(
        &mut app,
        branches_event("svc-a", Ok((vec![], vec!["main".into()]))),
        now,
    );
    assert_eq!(
        branch_picker(&app).unwrap().choices,
        Some(Err(
            "the service has no GitHub deploy trigger in this environment".into()
        ))
    );
    app.update(code(KeyCode::Esc), now);

    let image = DeploymentSummary {
        image: Some("postgres:16".into()),
        ..deploy("d1", DeploymentStatus::Success)
    };
    data(
        &mut app,
        services_event("e-prod", Ok(vec![service("svc-a", image, vec![])])),
        now,
    );
    assert_eq!(app.update(key('d'), now), vec![]);
    assert!(branch_picker(&app).is_none());
}

#[test]
fn d_in_deployments_opens_the_picker_for_that_service() {
    let now = Instant::now();
    let mut app = app_with_git_service(now);
    app.update(code(KeyCode::Enter), now);
    let cmds = app.update(key('d'), now);
    assert!(matches!(
        cmds.as_slice(),
        [Command::LoadBranches { service_id, .. }] if service_id == "svc-a"
    ));
    assert!(branch_picker(&app).is_some());
}

#[test]
fn unlisted_branches_let_the_typed_name_be_picked() {
    let now = Instant::now();
    let mut app = app_with_git_service(now);
    app.update(key('d'), now);
    let unlisted = DataEvent::Branches {
        env_id: "e-prod".into(),
        service_id: "svc-a".into(),
        result: Ok(BranchReply {
            triggers: vec![trigger()],
            names: Err("gh is not installed".into()),
        }),
    };
    data(&mut app, unlisted, now);
    let choices = branch_picker(&app).unwrap().choices.clone();
    assert!(
        matches!(&choices, Some(Ok(c)) if c.unlisted.as_deref() == Some("gh is not installed"))
    );
    app.update(code(KeyCode::Enter), now);
    assert!(confirm(&app).is_none(), "nothing typed yet");
    typed(&mut app, " feat/typed ", now);
    app.update(code(KeyCode::Enter), now);
    let Some(Confirm::DeployLatest(c)) = confirm(&app) else {
        panic!("expected the confirm, got {:?}", confirm(&app));
    };
    assert_eq!(
        c.switch.as_ref().map(|sw| (&sw.from[..], &sw.to[..])),
        Some(("main", "feat/typed"))
    );
}

#[test]
fn same_name_ignores_case_beyond_ascii() {
    assert!(same_name("Production", "production"));
    assert!(same_name("ÉTÉ", "été"));
    assert!(!same_name("prod", "production"));
    assert!(!same_name("production", "prod"));
}
