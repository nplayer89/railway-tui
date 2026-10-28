use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;

use super::{draw, theme};
use crate::app::{App, LogsView, StartPrefs, View};
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
    let buf = render(&app, Instant::now());
    let cells = find(&buf, "■");
    assert_eq!(cells.len(), 2, "{}", text(&buf));
    assert_eq!(buf[cells[0]].fg, theme::OK);
    assert_eq!(buf[cells[1]].fg, theme::BAD);
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
        assert_eq!(buf[c].fg, theme::BAD);
        assert_eq!(buf[c].bg, theme::OK);
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
    assert_eq!(fg_of("healthy"), (theme::TEXT, theme::OK));
    assert_eq!(fg_of("building"), (theme::TEXT, theme::BUSY));
    assert_eq!(fg_of("removed"), (theme::TEXT, theme::IDLE));
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
    assert_eq!(buf[at].fg, theme::BAD);
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
    assert_eq!(buf[(0, y)].bg, theme::BUSY);
    assert_eq!(buf[(99, y)].bg, theme::BUSY);
}

#[test]
fn disconnected_banner_is_red_across_the_width() {
    let app = logs_app(vec![], StreamState::Failed("nope".into()));
    let buf = render(&app, Instant::now());
    let y = (0..30)
        .find(|y| row_text(&buf, *y).contains("DISCONNECTED: nope"))
        .unwrap();
    assert_eq!(buf[(0, y)].bg, theme::BAD);
    assert_eq!(buf[(99, y)].bg, theme::BAD);
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
    assert_eq!(buf[live].fg, theme::OK);
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
    app.command_line = Some("dep".into());
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
        theme::BAD
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
    app.env_picker = Some(1);
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
    app.env_picker = Some(79);
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
    assert_eq!(buf[(0, h)].fg, theme::ACCENT);
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
