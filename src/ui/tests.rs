use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;

use super::draw;
use crate::theme::{Theme, THEMES};

/// The default theme, which the fixtures render with.
const T: &Theme = &THEMES[0];
use crate::app::{
    App, BranchChoices, BranchPicker, BranchSwitch, Confirm, LatestConfirm, LogsView, Modal,
    StartPrefs, View,
};
use crate::logs::buffer::{LogBuffer, LOG_CAP};
use crate::logs::follow::StreamState;
use crate::model::{
    DeploymentStatus, DeploymentSummary, Instance, InstanceStatus, LogKind, LogLine,
    ServiceInstance,
};

fn render_sized(app: &App, now: Instant, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| draw(f, app, now)).unwrap();
    terminal.backend().buffer().clone()
}

fn render(app: &App, now: Instant) -> Buffer {
    render_sized(app, now, 100, 30)
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol())
        .collect::<String>()
}

fn text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn find(buf: &Buffer, symbol: &str) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf[(x, y)].symbol() == symbol {
                out.push((x, y));
            }
        }
    }
    out
}

fn deploy(id: &str, status: DeploymentStatus, instances: &[InstanceStatus]) -> DeploymentSummary {
    DeploymentSummary {
        id: id.into(),
        status,
        created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        commit_sha: Some("a1b2c3d4e5f6".into()),
        commit_message: Some("fix auth header".into()),
        repo: Some("acme/api".into()),
        branch: Some("main".into()),
        image: None,
        instances: instances
            .iter()
            .enumerate()
            .map(|(i, s)| Instance {
                id: format!("{id}-i{i}"),
                status: s.clone(),
            })
            .collect(),
    }
}

fn service(
    name: &str,
    latest: DeploymentSummary,
    active: Vec<DeploymentSummary>,
) -> ServiceInstance {
    ServiceInstance {
        service_id: format!("{name}-id"),
        service_name: name.into(),
        num_replicas: 2,
        latest: Some(latest),
        active,
    }
}

fn services_app(services: Vec<ServiceInstance>) -> App {
    let (mut app, _) = App::new(StartPrefs::default());
    app.stack = vec![View::Projects, View::Services];
    app.environment = Some(crate::model::Environment {
        id: "e-prod".into(),
        name: "production".into(),
    });
    app.services.value = Some(services);
    app.services.fetched_at = Some(Instant::now());
    app
}

fn line(secs: i64, msg: &str) -> LogLine {
    LogLine {
        timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 10, 20, 0).unwrap()
            + chrono::Duration::seconds(secs),
        message: msg.into(),
        attributes: vec![],
    }
}

fn logs_app(lines: Vec<LogLine>, state: StreamState) -> App {
    let (mut app, _) = App::new(StartPrefs::default());
    let mut buffer = LogBuffer::new(LOG_CAP);
    buffer.extend(lines);
    app.stack = vec![
        View::Projects,
        View::Services,
        View::Logs(LogsView {
            session: 1,
            deployment_id: "dep-1".into(),
            service_name: "api".into(),
            kind: LogKind::Runtime,
            replica: None,
            buffer,
            follow: true,
            scroll: 0,
            filter: None,
            state,
        }),
    ];
    app
}

fn logs_mut(app: &mut App) -> &mut LogsView {
    match app.stack.last_mut() {
        Some(View::Logs(l)) => l,
        _ => panic!("not a log view"),
    }
}

#[test]
fn services_row_shows_coloured_boxes() {
    let d = deploy(
        "d1",
        DeploymentStatus::Success,
        &[InstanceStatus::Running, InstanceStatus::Crashed],
    );
    let app = services_app(vec![service("api", d.clone(), vec![d])]);
    let buf = render_sized(&app, Instant::now(), 120, 30);
    let cells = find(&buf, "■");
    assert_eq!(cells.len(), 2, "{}", text(&buf));
    assert_eq!(buf[cells[0]].fg, T.ok);
    assert_eq!(buf[cells[1]].fg, T.bad);
    // Boxes sit in the INSTANCES column, on the service's row.
    let header_y = (0..30)
        .find(|y| row_text(&buf, *y).contains("INSTANCES"))
        .unwrap();
    let col = row_text(&buf, header_y).find("INSTANCES").unwrap() as u16;
    assert_eq!(cells[0].1, header_y + 1);
    assert_eq!(cells[0].0, col);
    assert_eq!(cells[1].0, col + 2);
    let row = row_text(&buf, header_y + 1);
    assert!(row.contains("1/2") && row.contains("a1b2c3d") && row.contains("fix auth header"));
}

#[test]
fn failed_but_serving_has_striped_marker() {
    let running = deploy("old", DeploymentStatus::Success, &[InstanceStatus::Running]);
    let failed = deploy("new", DeploymentStatus::Failed, &[]);
    let app = services_app(vec![service("api", failed, vec![running])]);
    let buf = render(&app, Instant::now());
    let cells = find(&buf, "▚");
    assert_eq!(cells.len(), 2, "{}", text(&buf));
    for c in cells {
        assert_eq!(buf[c].fg, T.bad);
        assert_eq!(buf[c].bg, T.ok);
    }
}

#[test]
fn row_colours_follow_health() {
    let ok = deploy(
        "a",
        DeploymentStatus::Success,
        &[InstanceStatus::Running, InstanceStatus::Running],
    );
    let building = deploy("b", DeploymentStatus::Building, &[]);
    let removed = deploy("c", DeploymentStatus::Removed, &[]);
    let mut app = services_app(vec![
        service("healthy", ok.clone(), vec![ok]),
        service("building", building, vec![]),
        service("removed", removed, vec![]),
    ]);
    app.selection.service = Some("removed-id".into());
    let buf = render(&app, Instant::now());
    // The name stays in plain text; only the STATUS label carries the health colour.
    let fg_of = |name: &str| {
        let y = (0..30)
            .find(|y| row_text(&buf, *y).trim_start().starts_with(name))
            .unwrap();
        let status_x = row_text(&buf, 2).find("STATUS").unwrap() as u16;
        (buf[(0, y)].fg, buf[(status_x, y)].fg)
    };
    assert_eq!(fg_of("healthy"), (T.text, T.ok));
    assert_eq!(fg_of("building"), (T.text, T.busy));
    assert_eq!(fg_of("removed"), (T.text, T.idle));
}

#[test]
fn freshness_shows_seconds_ago() {
    let now = Instant::now();
    let mut app = services_app(vec![]);
    app.services.fetched_at = Some(now - Duration::from_secs(5));
    let buf = render(&app, now);
    assert!(text(&buf).contains("updated 5s ago"), "{}", text(&buf));
}

#[test]
fn error_shows_stale_marker() {
    let mut app = services_app(vec![]);
    app.services.error = Some("boom".into());
    let buf = render(&app, Instant::now());
    let at = find(&buf, "S")
        .into_iter()
        .find(|(x, y)| row_text(&buf, *y)[*x as usize..].starts_with("STALE: boom"))
        .expect("STALE marker");
    assert_eq!(buf[at].fg, T.bad);
}

#[test]
fn loading_and_live_off_and_rate_limit_are_shown() {
    let now = Instant::now();
    let mut app = services_app(vec![]);
    app.services.fetched_at = None;
    app.services.loading = true;
    app.live_updates = false;
    app.blocked_until = Some(now + Duration::from_secs(7));
    let t = text(&render(&app, now));
    assert!(t.contains("loading…"), "{t}");
    assert!(t.contains("live updates off"), "{t}");
    assert!(t.contains("rate limited, retrying in 7s"), "{t}");
    app.blocked_until = Some(now - Duration::from_secs(1));
    assert!(!text(&render(&app, now)).contains("rate limited"));
}

#[test]
fn reconnecting_banner_visible() {
    let app = logs_app(
        vec![line(0, "hello")],
        StreamState::Reconnecting {
            attempt: 3,
            retry_in: Duration::from_secs(4),
        },
    );
    let buf = render(&app, Instant::now());
    let y = (0..30)
        .find(|y| row_text(&buf, *y).contains("RECONNECTING"))
        .expect("banner");
    assert!(row_text(&buf, y).contains("RECONNECTING (attempt 3, retry in 4s)"));
    assert_eq!(buf[(0, y)].bg, T.busy);
    assert_eq!(buf[(99, y)].bg, T.busy);
}

#[test]
fn disconnected_banner_is_red_across_the_width() {
    let app = logs_app(vec![], StreamState::Failed("nope".into()));
    let buf = render(&app, Instant::now());
    let y = (0..30)
        .find(|y| row_text(&buf, *y).contains("DISCONNECTED: nope"))
        .unwrap();
    assert_eq!(buf[(0, y)].bg, T.bad);
    assert_eq!(buf[(99, y)].bg, T.bad);
}

#[test]
fn live_badge_and_follow_state() {
    let mut app = logs_app(vec![line(0, "hi")], StreamState::Live);
    let buf = render(&app, Instant::now());
    let t = text(&buf);
    assert!(
        t.contains("● LIVE") && t.contains("FOLLOW") && t.contains("RUNTIME"),
        "{t}"
    );
    let live = find(&buf, "●")[0];
    assert_eq!(buf[live].fg, T.ok);
    assert!(t.contains("10:20:00 hi"), "{t}");
    logs_mut(&mut app).follow = false;
    assert!(text(&render(&app, Instant::now())).contains("PAUSED"));
}

#[test]
fn logs_show_only_filtered_lines() {
    let mut app = logs_app(
        vec![
            line(0, "alpha one"),
            line(1, "beta two"),
            line(2, "alpha three"),
        ],
        StreamState::Live,
    );
    logs_mut(&mut app).filter = Some("alpha".into());
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("alpha one") && t.contains("alpha three"), "{t}");
    assert!(!t.contains("beta two"), "{t}");
    assert!(t.contains("filter: alpha"));
}

#[test]
fn log_scroll_is_clamped_and_empty_buffer_renders() {
    let lines: Vec<LogLine> = (0..50).map(|i| line(i, &format!("msg {i}"))).collect();
    let mut app = logs_app(lines, StreamState::Live);
    // Following shows the newest line.
    assert!(text(&render(&app, Instant::now())).contains("msg 49"));
    // Scrolled up by 10 hides the newest and shows msg 39 at the bottom.
    logs_mut(&mut app).scroll = 10;
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("msg 39") && !t.contains("msg 40"), "{t}");
    // Out of range scroll, then filtered down to one line: no panic, line shown.
    {
        let l = logs_mut(&mut app);
        l.scroll = 10_000;
        l.filter = Some("msg 7".into());
    }
    assert!(text(&render(&app, Instant::now())).contains("msg 7"));
    // Filter matching nothing, huge scroll.
    logs_mut(&mut app).filter = Some("zzz".into());
    render(&app, Instant::now());
    let empty = logs_app(vec![], StreamState::Connecting);
    assert!(text(&render(&empty, Instant::now())).contains("CONNECTING…"));
}

#[test]
fn control_characters_in_log_text_are_stripped() {
    let app = logs_app(vec![line(0, "red\u{1b}[31m text\tx")], StreamState::Live);
    let buf = render(&app, Instant::now());
    assert!(text(&buf).contains("red[31m text x"));
    assert!(buf.content().iter().all(|c| !c.symbol().contains('\u{1b}')));
}

#[test]
fn projects_are_grouped_under_workspace_headings() {
    use crate::model::{ProjectRef, Workspace};
    let (mut app, _) = App::new(StartPrefs::default());
    let ws = |id: &str, projects: &[&str]| Workspace {
        id: id.into(),
        name: format!("{id}-team"),
        projects: projects
            .iter()
            .map(|p| ProjectRef {
                id: format!("{p}-id"),
                name: (*p).into(),
            })
            .collect(),
    };
    app.workspaces.value = Some(vec![ws("w1", &["alpha", "beta"]), ws("w2", &["gamma"])]);
    let buf = render(&app, Instant::now());
    let y = |needle: &str| {
        (0..30)
            .find(|y| row_text(&buf, *y).contains(needle))
            .unwrap()
    };
    assert!(y("w1-team") < y("alpha") && y("alpha") < y("beta") && y("beta") < y("w2-team"));
    assert!(y("w2-team") < y("gamma"));
    assert!(row_text(&buf, y("alpha")).starts_with("  alpha"));
}

#[test]
fn deployments_show_tags() {
    let latest = deploy("new", DeploymentStatus::Failed, &[]);
    let active = deploy("old", DeploymentStatus::Success, &[InstanceStatus::Running]);
    let (mut app, _) = App::new(StartPrefs::default());
    app.stack = vec![
        View::Projects,
        View::Services,
        View::Deployments {
            service_id: "api-id".into(),
        },
    ];
    app.services.value = Some(vec![service("api", latest.clone(), vec![active.clone()])]);
    app.deployments.value = Some(vec![latest, active]);
    let t = text(&render(&app, Instant::now()));
    assert!(
        t.contains("LATEST") && t.contains("ACTIVE") && t.contains("TAG"),
        "{t}"
    );
}

#[test]
fn overlays_message_and_command_line() {
    let mut app = services_app(vec![]);
    app.message = Some("unknown command: x".into());
    assert!(text(&render(&app, Instant::now())).contains("unknown command: x"));
    app.message = None;
    app.help = true;
    assert!(text(&render(&app, Instant::now())).contains("Help"));
    app.help = false;
    app.modal = Some(Modal::Command("dep".into()));
    let buf = render(&app, Instant::now());
    assert!(row_text(&buf, 29).starts_with(":dep"));
}

#[test]
fn tiny_terminals_do_not_panic() {
    let mut app = services_app(vec![service(
        "api",
        deploy("d", DeploymentStatus::Success, &[InstanceStatus::Running]),
        vec![],
    )]);
    app.help = true;
    for (w, h) in [(20, 5), (1, 1), (0, 0), (5, 2), (30, 3)] {
        render_sized(&app, Instant::now(), w, h);
    }
    let mut logs = logs_app(vec![line(0, "x")], StreamState::Live);
    logs_mut(&mut logs).scroll = 99;
    for (w, h) in [(20, 5), (1, 1), (0, 0), (10, 3)] {
        render_sized(&logs, Instant::now(), w, h);
    }
}

fn assert_readable(buf: &Buffer, y: u16) {
    for x in 0..buf.area.width {
        let c = &buf[(x, y)];
        // Terminal-default on terminal-default (unselected cells) is readable by definition.
        if c.symbol().trim().is_empty() || c.bg == Color::Reset {
            continue;
        }
        assert_ne!(c.fg, c.bg, "unreadable cell {:?} at ({x},{y})", c.symbol());
    }
}

#[test]
fn selected_grey_rows_stay_readable() {
    let removed = deploy("c", DeploymentStatus::Removed, &[InstanceStatus::Stopped]);
    let app = services_app(vec![service("removed", removed, vec![])]);
    let buf = render(&app, Instant::now());
    let y = (0..30)
        .find(|y| row_text(&buf, *y).starts_with("removed"))
        .unwrap();
    assert_readable(&buf, y);
    assert!(!find(&buf, "■").is_empty());
    // Same for a grey instance row.
    let d = deploy("d", DeploymentStatus::Success, &[InstanceStatus::Stopped]);
    let mut app = services_app(vec![service("api", d.clone(), vec![d])]);
    app.stack.push(View::Instances {
        service_id: "api-id".into(),
    });
    let buf = render(&app, Instant::now());
    let y = (0..30)
        .find(|y| row_text(&buf, *y).contains("STOPPED"))
        .unwrap();
    assert_readable(&buf, y);
}

#[test]
fn instances_view_lists_columns() {
    let d = deploy(
        "d1",
        DeploymentStatus::Success,
        &[InstanceStatus::Running, InstanceStatus::Crashed],
    );
    let mut app = services_app(vec![service("api", d.clone(), vec![d])]);
    app.stack.push(View::Instances {
        service_id: "api-id".into(),
    });
    let buf = render(&app, Instant::now());
    let t = text(&buf);
    for want in [
        "ID",
        "STATUS",
        "DEPLOYMENT",
        "d1-i0",
        "d1-i1",
        "RUNNING",
        "CRASHED",
    ] {
        assert!(t.contains(want), "missing {want}\n{t}");
    }
    let y = (0..30)
        .find(|y| row_text(&buf, *y).contains("CRASHED"))
        .unwrap();
    assert!(row_text(&buf, y).contains("d1"));
    assert_eq!(
        buf[(row_text(&buf, y).find("CRASHED").unwrap() as u16, y)].fg,
        T.bad
    );
}

fn envs(n: usize) -> Vec<crate::model::Environment> {
    (0..n)
        .map(|i| crate::model::Environment {
            id: format!("e{i}"),
            name: format!("env-{i}"),
        })
        .collect()
}

#[test]
fn env_picker_lists_and_highlights_selected() {
    let mut app = services_app(vec![]);
    app.environments.value = Some(envs(3));
    app.modal = Some(Modal::EnvPicker(1));
    let buf = render(&app, Instant::now());
    let t = text(&buf);
    assert!(
        t.contains("env-0") && t.contains("env-1") && t.contains("env-2"),
        "{t}"
    );
    let at = |name: &str| {
        let y = (0..30).find(|y| row_text(&buf, *y).contains(name)).unwrap();
        (row_text(&buf, y).find(name).unwrap() as u16, y)
    };
    assert_ne!(buf[at("env-1")].bg, buf[at("env-0")].bg);
    assert_readable(&buf, at("env-1").1);
}

#[test]
fn env_picker_with_more_environments_than_fit_does_not_panic() {
    let mut app = services_app(vec![]);
    app.environments.value = Some(envs(80));
    app.modal = Some(Modal::EnvPicker(79));
    render(&app, Instant::now());
    render_sized(&app, Instant::now(), 20, 5);
}

#[test]
fn projects_heading_row_is_styled_and_projects_indented() {
    use crate::model::{ProjectRef, Workspace};
    let (mut app, _) = App::new(StartPrefs::default());
    app.workspaces.value = Some(vec![Workspace {
        id: "w1".into(),
        name: "team".into(),
        projects: vec![ProjectRef {
            id: "p".into(),
            name: "alpha".into(),
        }],
    }]);
    let buf = render(&app, Instant::now());
    let h = (0..30)
        .find(|y| row_text(&buf, *y).starts_with("team"))
        .unwrap();
    assert_eq!(buf[(0, h)].fg, T.accent);
    assert!(row_text(&buf, h + 1).starts_with("  alpha"));
}

#[test]
fn freshness_variants_are_never_silent() {
    let now = Instant::now();
    let ago = Some(now - Duration::from_secs(9));
    let mut app = services_app(vec![]);
    // Reload in flight over existing data keeps the age.
    app.services.fetched_at = ago;
    app.services.loading = true;
    assert!(text(&render(&app, now)).contains("updated 9s ago · refreshing…"));
    // Error over old data says how old.
    app.services.loading = false;
    app.services.error = Some("boom".into());
    assert!(text(&render(&app, now)).contains("STALE: boom (last ok 9s ago)"));
    // Error with nothing ever loaded.
    app.services.fetched_at = None;
    assert!(text(&render(&app, now)).contains("STALE: boom"));
    // Never loaded, idle.
    app.services.error = None;
    assert!(text(&render(&app, now)).contains("not loaded"));
}

#[test]
fn server_labels_are_cleaned() {
    let mut d = deploy("d", DeploymentStatus::Unknown("WEI\u{1b}RD".into()), &[]);
    d.instances = vec![Instance {
        id: "i".into(),
        status: InstanceStatus::Unknown("BA\u{1b}D".into()),
    }];
    let mut app = services_app(vec![service("api", d.clone(), vec![d])]);
    let buf = render(&app, Instant::now());
    assert!(buf.content().iter().all(|c| !c.symbol().contains('\u{1b}')));
    assert!(text(&buf).contains("UNKNOWN(WEIRD)"));
    app.stack.push(View::Instances {
        service_id: "api-id".into(),
    });
    let buf = render(&app, Instant::now());
    assert!(buf.content().iter().all(|c| !c.symbol().contains('\u{1b}')));
    assert!(text(&buf).contains("UNKNOWN(BAD)"), "{}", text(&buf));
}

/// Services (or Instances) before any environment is chosen.
fn no_env_app(instances_view: bool) -> App {
    let (mut app, _) = App::new(StartPrefs::default());
    app.stack = vec![View::Projects, View::Services];
    if instances_view {
        app.stack.push(View::Instances {
            service_id: "api-id".into(),
        });
    }
    app
}

#[test]
fn failed_environments_load_is_shown_not_hidden() {
    let now = Instant::now();
    for instances_view in [false, true] {
        let mut app = no_env_app(instances_view);
        app.environments.error = Some("Railway API returned HTTP 500".into());
        let t = text(&render(&app, now));
        assert!(t.contains("STALE: Railway API returned HTTP 500"), "{t}");
        assert!(!t.contains("not loaded"), "{t}");
    }
}

#[test]
fn project_without_environments_says_so() {
    let now = Instant::now();
    for instances_view in [false, true] {
        let mut app = no_env_app(instances_view);
        app.environments.value = Some(Vec::new());
        app.environments.fetched_at = Some(now - Duration::from_secs(3));
        let t = text(&render(&app, now));
        assert!(t.contains("no environments"), "{t}");
        assert!(t.contains("updated 3s ago"), "{t}");
        assert!(
            !t.contains("no services") && !t.contains("no instances"),
            "{t}"
        );
    }
}

#[test]
fn environments_loading_shows_loading() {
    let mut app = no_env_app(false);
    app.environments.loading = true;
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("loading…"), "{t}");
    assert!(!t.contains("no services"), "{t}");
}

#[test]
fn theme_paints_background_and_status_colours() {
    let d = deploy("d1", DeploymentStatus::Success, &[InstanceStatus::Running]);
    let mut app = services_app(vec![service("api", d.clone(), vec![d])]);
    app.theme = crate::theme::find("catppuccin-latte").unwrap();
    let latte = app.theme();
    let buf = render(&app, Instant::now());
    assert_eq!(buf[(99, 29)].bg, latte.bg);
    assert_eq!(buf[find(&buf, "■")[0]].fg, latte.ok);
}

#[test]
fn theme_picker_lists_every_theme() {
    let mut app = services_app(vec![]);
    app.modal = Some(Modal::ThemePicker { restore: 0 });
    let t = text(&render_sized(&app, Instant::now(), 100, 40));
    for theme in THEMES {
        assert!(t.contains(theme.name), "missing {}\n{t}", theme.name);
    }
}

#[test]
fn selected_rows_are_readable_in_every_theme() {
    let removed = deploy("c", DeploymentStatus::Removed, &[InstanceStatus::Stopped]);
    let mut app = services_app(vec![service("removed", removed, vec![])]);
    for i in 0..THEMES.len() {
        app.theme = i;
        let buf = render(&app, Instant::now());
        let y = (0..30)
            .find(|y| row_text(&buf, *y).starts_with("removed"))
            .unwrap();
        assert_readable(&buf, y);
    }
}

fn vars_app(env: &str, vars: &[(&str, &str)]) -> App {
    vars_app_sealed(env, vars, &[])
}

fn vars_app_sealed(env: &str, vars: &[(&str, &str)], sealed: &[&str]) -> App {
    let mut app = services_app(vec![service(
        "api",
        deploy("d", DeploymentStatus::Success, &[InstanceStatus::Running]),
        vec![],
    )]);
    app.services.value.as_mut().unwrap()[0].service_id = "svc-1".into();
    app.project = Some(crate::model::ProjectRef {
        id: "p1".into(),
        name: "proj".into(),
    });
    app.environment = Some(crate::model::Environment {
        id: "e1".into(),
        name: env.into(),
    });
    app.stack = vec![
        View::Projects,
        View::Services,
        View::Variables(crate::app::VarsView {
            service_id: "svc-1".into(),
            revealed: Default::default(),
            reveal_all: false,
            sealed: sealed.iter().map(|s| s.to_string()).collect(),
            anchor: None,
        }),
    ];
    app.variables.value = Some(
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    );
    app.variables.fetched_at = Some(Instant::now());
    app
}

fn press(app: &mut App, c: char) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    app.update(
        crate::app::AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
        Instant::now(),
    );
}

fn add_pending(app: &mut App, name: &str, change: crate::pending::Change, base: Option<&str>) {
    let sp = std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .entry("e1".into())
        .or_default()
        .entry("svc-1".into())
        .or_default();
    sp.project_id = "p1".into();
    sp.vars.insert(
        name.into(),
        crate::pending::PendingVar {
            change,
            base: base.map(Into::into),
            base_sealed: false,
            applied: false,
        },
    );
}

#[test]
fn values_hidden_until_revealed() {
    let mut app = vars_app("staging", &[("KEY", "s3cret")]);
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("••••••"), "{t}");
    assert!(!t.contains("s3cret"), "{t}");
    assert!(t.contains("api · variables · ENV staging"), "{t}");
    press(&mut app, 'r');
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("s3cret"), "{t}");
}

#[test]
fn sealed_shows_opaque_even_when_revealed() {
    let mut app = vars_app_sealed("staging", &[("TOKEN", "")], &["TOKEN", "GONE"]);
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("<Opaque>"), "{t}");
    assert!(!t.contains("<Empty>") && !t.contains("••••••"), "{t}");
    press(&mut app, 'R');
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("<Opaque>"), "{t}");
    assert!(!t.contains("<Empty>"), "{t}");
}

#[test]
fn empty_value_shows_empty_even_while_hidden() {
    let app = vars_app("staging", &[("BLANK", ""), ("KEY", "s3cret")]);
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("<Empty>"), "{t}");
    assert!(t.contains("••••••"), "{t}");
}

#[test]
fn pending_set_to_empty_shows_empty() {
    let mut app = vars_app("staging", &[("KEY", "s3cret")]);
    add_pending(
        &mut app,
        "KEY",
        crate::pending::Change::Set { value: "".into() },
        Some("s3cret"),
    );
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("<Empty>"), "{t}");
    assert!(!t.contains("••••••"), "{t}");
}

#[test]
fn revealed_value_strips_escapes() {
    let mut app = vars_app("staging", &[("KEY", "a\x1b[31mb")]);
    press(&mut app, 'R');
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("a[31mb"), "{t}");
    assert!(!t.contains('\x1b'));
}

#[test]
fn multiline_value_shows_first_line_and_ellipsis() {
    let mut app = vars_app("staging", &[("KEY", "line1\nline2")]);
    press(&mut app, 'R');
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("line1…"), "{t}");
    assert!(!t.contains("line2"), "{t}");
}

#[test]
fn production_env_title_is_bad_colour() {
    let app = vars_app("production", &[("KEY", "v")]);
    let buf = render(&app, Instant::now());
    let y = (0..30)
        .find(|y| row_text(&buf, *y).contains("· variables ·"))
        .unwrap();
    let x = row_text(&buf, y).find("production").unwrap() as u16;
    assert_eq!(buf[(x, y)].fg, T.bad);
    let app = vars_app("staging", &[("KEY", "v")]);
    let buf = render(&app, Instant::now());
    let x = row_text(&buf, y).find("staging").unwrap() as u16;
    assert_eq!(buf[(x, y)].fg, T.accent);
}

#[test]
fn is_production_matches_prod_names() {
    assert!(super::is_production("production"));
    assert!(super::is_production("Prod-eu"));
    assert!(!super::is_production("staging"));
}

#[test]
fn markers_and_strikethrough() {
    use crate::pending::Change;
    let mut app = vars_app("staging", &[("CHG", "old"), ("DEL", "gone"), ("SAME", "x")]);
    add_pending(&mut app, "ADD", Change::Set { value: "n".into() }, None);
    add_pending(
        &mut app,
        "CHG",
        Change::Set {
            value: "new".into(),
        },
        Some("old"),
    );
    add_pending(&mut app, "DEL", Change::Delete, Some("gone"));
    if let Some(View::Variables(v)) = app.stack.last_mut() {
        v.reveal_all = true;
    }
    let buf = render(&app, Instant::now());
    let find_row = |name: &str| {
        (0..30)
            .find(|y| row_text(&buf, *y).contains(name))
            .unwrap_or_else(|| panic!("no row {name}\n{}", text(&buf)))
    };
    let marker = |name: &str| row_text(&buf, find_row(name)).chars().next().unwrap();
    assert_eq!(marker("ADD"), '+');
    assert_eq!(marker("CHG"), '~');
    assert_eq!(marker("DEL"), '-');
    assert_eq!(marker("SAME"), ' ');
    let chg = row_text(&buf, find_row("CHG"));
    assert!(chg.contains("new") && !chg.contains("old"), "{chg}");
    let y = find_row("DEL");
    let x = row_text(&buf, y).find("DEL").unwrap() as u16;
    assert!(buf[(x, y)]
        .modifier
        .contains(ratatui::style::Modifier::CROSSED_OUT));
    let y = find_row("SAME");
    let x = row_text(&buf, y).find("SAME").unwrap() as u16;
    assert!(!buf[(x, y)]
        .modifier
        .contains(ratatui::style::Modifier::CROSSED_OUT));
}

/// A Variables view with an added and a changed pending value.
fn pending_values_app() -> App {
    use crate::pending::Change;
    let mut app = vars_app("staging", &[("CHG", "oldsecret")]);
    let set = |value: &str| Change::Set {
        value: value.into(),
    };
    add_pending(&mut app, "ADD", set("addsecret"), None);
    add_pending(&mut app, "CHG", set("chgsecret"), Some("oldsecret"));
    let sp = std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .get_mut("e1")
        .unwrap()
        .get_mut("svc-1")
        .unwrap();
    sp.env_name = "staging".into();
    sp.service_name = "api".into();
    app
}

#[test]
fn pending_new_values_hidden_until_revealed() {
    let mut app = pending_values_app();
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("ADD") && t.contains("CHG"), "{t}");
    for secret in ["addsecret", "chgsecret", "oldsecret"] {
        assert!(!t.contains(secret), "{t}");
    }
    app.environments.value = Some(vec![crate::model::Environment {
        id: "e1".into(),
        name: "staging".into(),
    }]);
    app.stack.push(View::Pending(Default::default()));
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("+ ADD") && t.contains("~ CHG"), "{t}");
    for secret in ["addsecret", "chgsecret", "oldsecret"] {
        assert!(!t.contains(secret), "{t}");
    }
}

#[test]
fn r_on_a_pending_row_reveals_only_that_row() {
    let mut app = pending_values_app();
    app.selection.variable = Some("ADD".into());
    press(&mut app, 'r');
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("addsecret"), "{t}");
    assert!(!t.contains("chgsecret") && !t.contains("oldsecret"), "{t}");
    app.selection.variable = Some("CHG".into());
    press(&mut app, 'r');
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("addsecret") && t.contains("chgsecret"), "{t}");
    assert!(!t.contains("oldsecret"), "{t}");
}

#[test]
fn header_shows_pending_count() {
    use crate::pending::Change;
    let mut app = vars_app("staging", &[("A", "1")]);
    assert!(!text(&render(&app, Instant::now())).contains("pending"));
    add_pending(&mut app, "A", Change::Delete, Some("1"));
    add_pending(&mut app, "B", Change::Set { value: "2".into() }, None);
    let buf = render(&app, Instant::now());
    let line = row_text(&buf, 1);
    assert!(line.contains("2 pending"), "{line}");
    let x = line.find("2 pending").unwrap() as u16;
    assert_eq!(buf[(x, 1)].fg, T.busy);
}

#[test]
fn footer_shows_variable_input() {
    let mut app = vars_app("staging", &[("A", "1")]);
    app.modal = Some(Modal::VarInput(crate::app::VarInput {
        name: None,
        text: crate::app::Secret::new("NEW".into()),
    }));
    assert!(row_text(&render(&app, Instant::now()), 29).contains("name: NEW▏"));
    app.modal = Some(Modal::VarInput(crate::app::VarInput {
        name: Some("A".into()),
        text: crate::app::Secret::new("val".into()),
    }));
    assert!(row_text(&render(&app, Instant::now()), 29).contains("A = val▏"));
}

fn review_app() -> App {
    let mut app = vars_app("production", &[]);
    app.environments.value = Some(vec![crate::model::Environment {
        id: "e1".into(),
        name: "production".into(),
    }]);
    let set = |value: &str| crate::pending::Change::Set {
        value: value.into(),
    };
    add_pending(&mut app, "NEW", set("newsecret"), None);
    add_pending(&mut app, "CHG", set("after"), Some("before"));
    add_pending(
        &mut app,
        "GONE",
        crate::pending::Change::Delete,
        Some("oldsecret"),
    );
    let sp = std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .get_mut("e1")
        .unwrap()
        .get_mut("svc-1")
        .unwrap();
    sp.env_name = "production".into();
    sp.service_name = "api".into();
    sp.vars.get_mut("CHG").unwrap().applied = true;
    app.stack.push(View::Pending(Default::default()));
    app
}

#[test]
fn review_values_hidden_until_revealed() {
    let mut app = review_app();
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("ENV production"), "{t}");
    assert!(t.contains("partly written"), "{t}");
    assert!(t.contains("+ NEW"), "{t}");
    assert!(t.contains("~ CHG"), "{t}");
    assert!(t.contains("- GONE"), "{t}");
    assert!(t.contains('✓'), "{t}");
    for secret in ["newsecret", "after", "before", "oldsecret"] {
        assert!(!t.contains(secret), "{t}");
    }
    press(&mut app, 'r');
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("before → after"), "{t}");
    assert!(t.contains("newsecret") && t.contains("oldsecret"), "{t}");
}

#[test]
fn variables_hint_fits_80_columns_and_keeps_help() {
    let app = vars_app("staging", &[]);
    let hint = super::hints(&app);
    assert!(hint.chars().count() <= 80, "{hint}");
    assert!(hint.ends_with("<?> help"), "{hint}");
}

#[test]
fn review_shows_an_unread_sealed_base_as_opaque() {
    let mut app = review_app();
    add_pending(&mut app, "SEALED", crate::pending::Change::Delete, Some(""));
    std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .get_mut("e1")
        .unwrap()
        .get_mut("svc-1")
        .unwrap()
        .vars
        .get_mut("SEALED")
        .unwrap()
        .base_sealed = true;
    for reveal in [false, true] {
        if reveal {
            press(&mut app, 'r');
        }
        let t = text(&render(&app, Instant::now()));
        let line = t.lines().find(|l| l.contains("- SEALED")).expect(&t);
        assert!(
            line.contains("<Opaque>") && !line.contains("<Empty>"),
            "{t}"
        );
    }
}

#[test]
fn review_orphan_label_and_discard_prompt() {
    let mut app = review_app();
    app.environments.value = Some(vec![]);
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("(environment no longer exists)"), "{t}");
    press(&mut app, 'x');
    let t = text(&render(&app, Instant::now()));
    assert!(
        t.contains("Discard pending changes for api in production? (y/n)"),
        "{t}"
    );
}

#[test]
fn written_not_deployed_only_when_every_entry_is_written() {
    let mut app = review_app();
    let sp = std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .get_mut("e1")
        .unwrap()
        .get_mut("svc-1")
        .unwrap();
    // Written earlier, then more edits: some entries are not on Railway yet.
    sp.written_not_deployed = true;
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("partly written"), "{t}");
    assert!(!t.contains("written, not deployed"), "{t}");
    // An edit re-based on a written value: nothing applied, but the flag is set.
    let sp = std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .get_mut("e1")
        .unwrap()
        .get_mut("svc-1")
        .unwrap();
    sp.vars.values_mut().for_each(|v| v.applied = false);
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("partly written"), "{t}");
    let sp = std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .get_mut("e1")
        .unwrap()
        .get_mut("svc-1")
        .unwrap();
    sp.vars.values_mut().for_each(|v| v.applied = true);
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("written, not deployed"), "{t}");
    assert!(!t.contains("partly written"), "{t}");
    let sp = std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .get_mut("e1")
        .unwrap()
        .get_mut("svc-1")
        .unwrap();
    sp.vars.values_mut().for_each(|v| v.applied = false);
    sp.written_not_deployed = false;
    let t = text(&render(&app, Instant::now()));
    assert!(!t.contains("written"), "{t}");
}

#[test]
fn review_labels_a_group_whose_service_is_gone() {
    let mut app = review_app();
    app.services.value.as_mut().unwrap()[0].service_id = "svc-other".into();
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("(service no longer exists)"), "{t}");
    assert!(!t.contains("(environment no longer exists)"), "{t}");
}

fn deploy_confirm(app: &mut App, drift: &[&str]) {
    app.modal = Some(Modal::Confirm(crate::app::Confirm::Deploy(
        crate::app::DeployConfirm {
            env_id: "e1".into(),
            service_id: "svc-1".into(),
            drift: drift.iter().map(|d| d.to_string()).collect(),
        },
    )));
}

/// The column of `needle` in `row`, counted in cells.
fn col(row: &str, needle: &str) -> u16 {
    row[..row.find(needle).unwrap()].chars().count() as u16
}

#[test]
fn production_confirm_shows_env_in_bad_colour() {
    let mut app = review_app();
    deploy_confirm(&mut app, &["NEW"]);
    let buf = render(&app, Instant::now());
    let t = text(&buf);
    assert!(
        t.contains("Deploy? y to confirm, any other key cancels"),
        "{t}"
    );
    assert!(t.contains("service: api"), "{t}");
    // Unapplied changes only (the review behind shows CHG), values hidden.
    let top = (0..30)
        .find(|y| row_text(&buf, *y).contains("┌ Deploy"))
        .unwrap_or_else(|| panic!("no overlay\n{t}"));
    let overlay: String = (top..30).map(|y| row_text(&buf, y)).collect();
    assert!(
        overlay.contains("+ NEW") && overlay.contains("- GONE"),
        "{t}"
    );
    assert!(!overlay.contains("CHG"), "{t}");
    for secret in ["newsecret", "oldsecret"] {
        assert!(!t.contains(secret), "{t}");
    }
    let drift = (0..30)
        .map(|y| row_text(&buf, y))
        .find(|r| r.contains("changed on Railway since your edit"))
        .unwrap_or_else(|| panic!("no drift line\n{t}"));
    assert!(drift.contains("NEW"), "{drift}");
    let y = (0..30)
        .find(|y| row_text(&buf, *y).contains("Deploy to production"))
        .unwrap_or_else(|| panic!("no title\n{t}"));
    let x = col(&row_text(&buf, y), "production");
    assert_eq!(buf[(x, y)].fg, T.bad);

    let mut app = review_app();
    std::sync::Arc::make_mut(&mut app.pending)
        .envs
        .get_mut("e1")
        .unwrap()
        .get_mut("svc-1")
        .unwrap()
        .env_name = "staging".into();
    deploy_confirm(&mut app, &[]);
    let buf = render(&app, Instant::now());
    let t = text(&buf);
    assert!(!t.contains("changed on Railway"), "{t}");
    let y = (0..30)
        .find(|y| row_text(&buf, *y).contains("Deploy to staging"))
        .unwrap_or_else(|| panic!("no title\n{t}"));
    let x = col(&row_text(&buf, y), "staging");
    assert_eq!(buf[(x, y)].fg, T.accent);
}

#[test]
fn long_deploy_confirm_keeps_the_prompt() {
    let mut app = review_app();
    for i in 0..60 {
        add_pending(
            &mut app,
            &format!("V{i:02}"),
            crate::pending::Change::Delete,
            Some("x"),
        );
    }
    deploy_confirm(&mut app, &[]);
    let t = text(&render(&app, Instant::now()));
    assert!(
        t.contains("Deploy? y to confirm, any other key cancels"),
        "{t}"
    );
    assert!(t.contains("more changes"), "{t}");
}

#[test]
fn help_overlay_shows_every_line_in_full() {
    let mut app = services_app(vec![]);
    app.help = true;
    let out = text(&render_sized(&app, Instant::now(), 100, 40));
    for line in super::HELP_LINES {
        assert!(
            out.contains(line.trim_end()),
            "help line truncated: {line:?}"
        );
    }
}

#[test]
fn elapsed_is_compact() {
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let at = |s| super::tables::elapsed(t0, t0 + chrono::Duration::seconds(s));
    assert_eq!(at(-3), "0s");
    assert_eq!(at(42), "42s");
    assert_eq!(at(187), "3m07s");
    assert_eq!(at(3840), "1h04m");
}

fn building_since(id: &str, secs: i64) -> DeploymentSummary {
    DeploymentSummary {
        created_at: Utc::now() - chrono::Duration::seconds(secs),
        ..deploy(id, DeploymentStatus::Building, &[])
    }
}

#[test]
fn building_shows_elapsed_in_services_and_deployments() {
    let app = services_app(vec![
        service("api", building_since("b", 125), vec![]),
        service("web", deploy("s", DeploymentStatus::Success, &[]), vec![]),
    ]);
    let buf = render(&app, Instant::now());
    let rows: Vec<String> = (0..30).map(|y| row_text(&buf, y)).collect();
    let api = rows.iter().find(|r| r.contains("api")).unwrap();
    assert!(api.contains("BUILDING 2m0"), "{api}");
    let web = rows.iter().find(|r| r.contains("web")).unwrap();
    assert!(!web.contains("m0"), "{web}");

    let mut app = app;
    app.stack.push(View::Deployments {
        service_id: "api-id".into(),
    });
    app.deployments.value = Some(vec![building_since("b", 125)]);
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("BUILDING 2m0"), "{t}");
}

#[test]
fn logs_show_build_elapsed_while_building() {
    let mut app = logs_app(vec![], StreamState::Live);
    app.services.value = Some(vec![service("api", building_since("dep-1", 30), vec![])]);
    let buf = render(&app, Instant::now());
    let (y, row) = (0..30)
        .map(|y| (y, row_text(&buf, y)))
        .find(|(_, r)| r.contains("● LIVE"))
        .unwrap();
    let byte = row.find("BUILDING 3").unwrap_or_else(|| panic!("{row}"));
    let x = row[..byte].chars().count() as u16;
    assert_eq!(buf[(x, y)].fg, T.busy);

    app.services.value = Some(vec![service(
        "api",
        deploy("dep-1", DeploymentStatus::Deploying, &[]),
        vec![],
    )]);
    assert!(!text(&render(&app, Instant::now())).contains("BUILDING"));
}

#[test]
fn marquee_rests_scrolls_and_rests() {
    let m = |ms| super::tables::marquee("abcdefgh", 5, Duration::from_millis(ms));
    assert_eq!(
        super::tables::marquee("abc", 5, Duration::from_secs(9)),
        "abc"
    );
    assert_eq!(m(0), "abcde");
    assert_eq!(m(1_499), "abcde");
    assert_eq!(m(1_750), "bcdef");
    assert_eq!(m(2_250), "defgh");
    assert_eq!(m(3_500), "defgh");
    // 3 scroll steps + 2 × 6 pause steps, then it starts over.
    assert_eq!(m(3_750), "abcde");
}

#[test]
fn focused_long_service_name_scrolls() {
    let long = "a-very-long-service-name-that-overflows";
    let mut app = services_app(vec![
        service(long, deploy("d", DeploymentStatus::Success, &[]), vec![]),
        service(
            "zz-other-long-service-name-that-overflows",
            deploy("e", DeploymentStatus::Success, &[]),
            vec![],
        ),
    ]);
    let since = Instant::now();
    app.service_focus = Some((format!("{long}-id"), since));
    let rows = |at| {
        let buf = render_sized(&app, at, 120, 30);
        (0..30).map(|y| row_text(&buf, y)).collect::<Vec<_>>()
    };
    let start = rows(since);
    assert!(
        start
            .iter()
            .any(|r| r.starts_with("a-very-long-service-name-that- ")),
        "{start:?}"
    );
    let later = rows(since + Duration::from_millis(1_500 + 250 * 3));
    assert!(
        later
            .iter()
            .any(|r| r.starts_with("ery-long-service-name-that-ove")),
        "{later:?}"
    );
    // Unfocused rows stay put.
    assert!(
        later
            .iter()
            .any(|r| r.starts_with("zz-other-long-service-name-tha")),
        "{later:?}"
    );
}

#[test]
fn latest_commit_confirm_shows_target_source_and_what_runs_now() {
    let mut app = services_app(vec![service(
        "api",
        deploy("d1", DeploymentStatus::Success, &[InstanceStatus::Running]),
        vec![],
    )]);
    app.modal = Some(Modal::Confirm(Confirm::DeployLatest(Box::new(
        LatestConfirm {
            project_id: "p".into(),
            env_id: "e-prod".into(),
            env_name: "production".into(),
            service_id: "api-id".into(),
            service_name: "api".into(),
            repo: Some("acme/api".into()),
            branch: Some("main".into()),
            current: Some(deploy("d1", DeploymentStatus::Success, &[])),
            written: 2,
            switch: None,
        },
    ))));
    let screen = text(&render(&app, Instant::now()));
    for want in [
        "Deploy latest commit to production",
        "service: api",
        "repo:    acme/api",
        "branch:  main",
        "now:     a1b2c3d  fix auth header",
        "deploy:  latest commit on main",
        "! 2 written variable changes will ship with this deploy",
        "Deploy? y to confirm, any other key cancels",
    ] {
        assert!(screen.contains(want), "missing {want:?} in\n{screen}");
    }
}

fn latest_confirm(switch: Option<BranchSwitch>) -> LatestConfirm {
    LatestConfirm {
        project_id: "p".into(),
        env_id: "e-prod".into(),
        env_name: "production".into(),
        service_id: "api-id".into(),
        service_name: "api".into(),
        repo: Some("acme/api".into()),
        branch: Some("feat/x".into()),
        current: Some(deploy("d1", DeploymentStatus::Success, &[])),
        written: 0,
        switch,
    }
}

#[test]
fn branch_confirm_shows_the_switch_and_that_it_stays() {
    let mut app = services_app(vec![]);
    app.modal = Some(Modal::Confirm(Confirm::DeployLatest(Box::new(
        latest_confirm(Some(BranchSwitch {
            trigger_id: "t1".into(),
            from: "main".into(),
            to: "feat/x".into(),
        })),
    ))));
    let screen = text(&render(&app, Instant::now()));
    for want in [
        "Deploy branch feat/x to production",
        "branch:  main → feat/x",
        "the service stays on feat/x afterwards",
        "deploy:  latest commit on feat/x",
    ] {
        assert!(screen.contains(want), "missing {want:?} in\n{screen}");
    }
}

#[test]
fn branch_picker_shows_loading_then_the_filtered_list() {
    let mut app = services_app(vec![]);
    let mut picker = BranchPicker {
        target: latest_confirm(None),
        choices: None,
        query: "fe".into(),
        index: 1,
    };
    app.modal = Some(Modal::BranchPicker(Box::new(picker.clone())));
    let screen = text(&render(&app, Instant::now()));
    assert!(screen.contains("Deploy branch: api"), "{screen}");
    assert!(screen.contains("> fe▏"), "{screen}");
    assert!(screen.contains("loading branches…"), "{screen}");

    picker.choices = Some(Ok(BranchChoices {
        trigger_id: "t1".into(),
        connected: "feat/a".into(),
        names: vec!["feat/a".into(), "feat/b".into(), "main".into()],
        unlisted: None,
    }));
    app.modal = Some(Modal::BranchPicker(Box::new(picker)));
    let buf = render(&app, Instant::now());
    let screen = text(&buf);
    assert!(screen.contains("feat/a  (connected)"), "{screen}");
    assert!(!screen.contains(" main"), "filtered out: {screen}");
    let row = (0..buf.area.height)
        .find(|&y| row_text(&buf, y).contains("feat/b"))
        .unwrap();
    let x = row_text(&buf, row).find("feat/b").unwrap() as u16;
    assert_eq!(
        buf[(x, row)].bg,
        app.theme().select_bg,
        "feat/b is highlighted"
    );
}

#[test]
fn unlisted_branches_explain_why_wrapped_and_ask_for_a_name() {
    let mut app = services_app(vec![]);
    let why = "gh: HTTP 404: Not Found; Railway: Railway API error: Not Authorized (this is long)";
    app.modal = Some(Modal::BranchPicker(Box::new(BranchPicker {
        target: latest_confirm(None),
        choices: Some(Ok(BranchChoices {
            trigger_id: "t1".into(),
            connected: "main".into(),
            names: vec![],
            unlisted: Some(why.into()),
        })),
        query: "feat/x".into(),
        index: 0,
    })));
    let screen = text(&render(&app, Instant::now()));
    for want in [
        "│ could not list branches: gh: HTTP 404: Not Found;        │",
        "│ Railway: Railway API error: Not Authorized (this is      │",
        "│ long)",
        "type the exact branch name, enter to pick",
        "connected: main",
        "> feat/x▏",
    ] {
        assert!(screen.contains(want), "missing {want:?} in\n{screen}");
    }
}

fn key(app: &mut App, code: crossterm::event::KeyCode) {
    use crossterm::event::{KeyEvent, KeyModifiers};
    app.update(
        crate::app::AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        Instant::now(),
    );
}

fn status_row(buf: &Buffer) -> String {
    row_text(buf, buf.area.height - 2)
}

fn last_row(buf: &Buffer) -> String {
    row_text(buf, buf.area.height - 1)
}

#[test]
fn statusline_shows_mode_label_env_project_and_position() {
    let app = vars_app("production", &[("A", "1"), ("B", "2")]);
    let buf = render_sized(&app, Instant::now(), 120, 20);
    let row = status_row(&buf);
    for want in [
        "NORMAL",
        "api › variables",
        "production",
        "proj",
        "1/2",
        "\u{e0bc}",
    ] {
        assert!(row.contains(want), "missing {want:?} in {row:?}");
    }
    let y = buf.area.height - 2;
    let x = row_text(&buf, y).chars().position(|c| c == 'N').unwrap() as u16;
    assert_eq!(buf[(x, y)].bg, app.theme().accent);
    let x = row_text(&buf, y).find("production").unwrap();
    let x = row_text(&buf, y)[..x].chars().count() as u16;
    assert_eq!(buf[(x, y)].fg, app.theme().bad);
}

#[test]
fn visual_mode_shows_in_the_statusline_with_a_count() {
    let mut app = vars_app("staging", &[("A", "1"), ("B", "2")]);
    let buf = render_sized(&app, Instant::now(), 120, 20);
    assert!(!status_row(&buf).contains("VISUAL"));
    press(&mut app, 'v');
    press(&mut app, 'j');
    let buf = render_sized(&app, Instant::now(), 120, 20);
    let row = status_row(&buf);
    assert!(
        row.contains("VISUAL") && row.contains("2 selected"),
        "{row}"
    );
    let y = buf.area.height - 2;
    let x = row.chars().position(|c| c == 'V').unwrap() as u16;
    assert_eq!(buf[(x, y)].bg, app.theme().hint);
    assert!(!text(&buf).contains("-- VISUAL --"));
    key(&mut app, crossterm::event::KeyCode::Esc);
    let t = text(&render(&app, Instant::now()));
    assert!(t.contains("api · variables"), "{t}");
}

#[test]
fn command_mode_marks_the_bar_and_types_on_the_last_line() {
    let mut app = vars_app("staging", &[("A", "1")]);
    for c in ":dep".chars() {
        press(&mut app, c);
    }
    let buf = render_sized(&app, Instant::now(), 120, 20);
    let row = status_row(&buf);
    assert!(row.contains("COMMAND"), "{row}");
    let x = row.chars().position(|c| c == 'C').unwrap() as u16;
    assert_eq!(buf[(x, buf.area.height - 2)].bg, app.theme().busy);
    assert!(last_row(&buf).starts_with(":dep"), "{}", last_row(&buf));
}

#[test]
fn insert_and_filter_modes_are_marked() {
    let mut app = vars_app("staging", &[("A", "1")]);
    press(&mut app, 'a');
    let buf = render_sized(&app, Instant::now(), 120, 20);
    let row = status_row(&buf);
    assert!(row.contains("INSERT"), "{row}");
    let x = row.chars().position(|c| c == 'I').unwrap() as u16;
    assert_eq!(buf[(x, buf.area.height - 2)].bg, app.theme().ok);
    key(&mut app, crossterm::event::KeyCode::Esc);
    press(&mut app, '/');
    let buf = render_sized(&app, Instant::now(), 120, 20);
    assert!(status_row(&buf).contains("FILTER"));
    assert!(last_row(&buf).starts_with('/'), "{}", last_row(&buf));
}

#[test]
fn message_wins_over_filter_on_the_last_line() {
    let mut app = vars_app("staging", &[("A", "1"), ("B", "2")]);
    press(&mut app, '/');
    press(&mut app, 'A');
    key(&mut app, crossterm::event::KeyCode::Enter);
    app.message = Some("hello".into());
    let last = last_row(&render_sized(&app, Instant::now(), 120, 20));
    assert!(
        last.contains("hello") && !last.contains("filter: A"),
        "{last}"
    );
    app.message = None;
    let last = last_row(&render_sized(&app, Instant::now(), 120, 20));
    assert!(last.contains("filter: A"), "{last}");
}

#[test]
fn narrow_statusline_drops_pieces_and_keeps_the_mode() {
    let app = vars_app("staging", &[("A", "1")]);
    let row = status_row(&render_sized(&app, Instant::now(), 40, 20));
    assert!(row.contains("NORMAL") && !row.contains('\u{f07b}'), "{row}");
    let row = status_row(&render_sized(&app, Instant::now(), 24, 20));
    assert!(row.contains("NORMAL") && row.contains('…'), "{row}");
    for w in 0..12 {
        render_sized(&app, Instant::now(), w, 20);
    }
}

#[test]
fn statusline_drops_count_then_project_then_env_then_position() {
    let mut app = vars_app("staging", &[("A", "1"), ("B", "2")]);
    press(&mut app, 'v');
    press(&mut app, 'j');
    let row = |w| status_row(&render_sized(&app, Instant::now(), w, 20));
    let full = row(120);
    assert!(
        full.contains("2 selected") && full.contains('\u{f07b}'),
        "{full}"
    );
    // Find the widest width at which each piece is still absent or present.
    let w_no_count = (1..120)
        .rev()
        .find(|&w| !row(w).contains("selected"))
        .unwrap();
    let r = row(w_no_count);
    assert!(r.contains('\u{f07b}') && r.contains("staging"), "{r}");
    let w_no_env = (1..120)
        .rev()
        .find(|&w| !row(w).contains("staging"))
        .unwrap();
    let r = row(w_no_env);
    assert!(!r.contains('\u{f07b}') && r.contains("2/2"), "{r}");
}

#[test]
fn logs_statusline_has_no_position_and_names_the_service() {
    let app = logs_app(vec![line(0, "hi")], StreamState::Live);
    let row = status_row(&render_sized(&app, Instant::now(), 120, 20));
    assert!(row.contains("› logs"), "{row}");
    assert!(!row.contains('\u{f03a}'), "{row}");
}

#[test]
fn breadcrumb_pill_is_gone() {
    let app = vars_app("staging", &[("A", "1")]);
    assert!(!text(&render(&app, Instant::now())).contains("projects > "));
}

#[test]
fn visual_range_rows_are_highlighted_without_bold() {
    let mut app = vars_app("staging", &[("AAA", "1"), ("BBB", "2"), ("CCC", "3")]);
    app.selection.variable = Some("AAA".into());
    press(&mut app, 'v');
    press(&mut app, 'j');
    let buf = render(&app, Instant::now());
    let cell = |name: &str| {
        let y = (0..buf.area.height)
            .find(|&y| row_text(&buf, y).contains(name))
            .unwrap();
        let x = row_text(&buf, y).find(name).unwrap() as u16;
        buf[(x, y)].clone()
    };
    let t = app.theme();
    let (anchor, cursor, outside) = (cell("AAA"), cell("BBB"), cell("CCC"));
    assert_eq!(anchor.bg, t.select_bg);
    assert!(!anchor.modifier.contains(ratatui::style::Modifier::BOLD));
    assert_eq!(cursor.bg, t.select_bg);
    assert!(cursor.modifier.contains(ratatui::style::Modifier::BOLD));
    assert_ne!(outside.bg, t.select_bg);
}
