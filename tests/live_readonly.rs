//! Read-only checks against the REAL Railway API, using the `railway` CLI login.
//!
//! Ignored by default and inert unless `RAILWAY_TUI_LIVE=1`:
//!
//! ```sh
//! RAILWAY_TUI_LIVE=1 cargo test --test live_readonly -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Queries and subscriptions only; nothing here writes to Railway or to `~/.railway`.
//! Output never includes tokens, and log messages are truncated.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use railway_tui::api::subscribe::{connect_authed, deployment_logs_stream};
use railway_tui::api::Api;
use railway_tui::auth::{token_source_from_env, TokenSource};
use railway_tui::budget::{parse_hourly_limit, RateBudget};
use railway_tui::model::{DeploymentStatus, DeploymentSummary, LogLine};
use railway_tui::worker::{HTTP_ENDPOINT, WS_ENDPOINT};
use url::Url;

fn live() -> bool {
    let on = std::env::var("RAILWAY_TUI_LIVE").as_deref() == Ok("1");
    if !on {
        eprintln!("skipped: set RAILWAY_TUI_LIVE=1 to run live read-only checks");
    }
    on
}

fn tokens() -> Arc<dyn TokenSource> {
    token_source_from_env().expect("railway CLI login (run `railway login`)")
}

fn api(tokens: Arc<dyn TokenSource>) -> Api {
    Api::new(
        Url::parse(HTTP_ENDPOINT).unwrap(),
        tokens,
        Arc::new(Mutex::new(RateBudget::new(None))),
    )
}

fn short(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

fn ts(l: &LogLine) -> String {
    l.timestamp.to_rfc3339()
}

struct Target {
    project_id: String,
    env_id: String,
    service_id: String,
    /// The service's currently running deployment.
    active: DeploymentSummary,
    /// The service's newest deployment (may equal `active`).
    latest: Option<DeploymentSummary>,
}

/// First service (in workspace/project order) that has a running deployment.
/// Costs 1 + 2 per project scanned.
async fn find_target(api: &Api) -> Target {
    let workspaces = api.workspaces().await.expect("workspaces");
    for project in workspaces.iter().flat_map(|w| &w.projects) {
        let envs = api.environments(&project.id).await.expect("environments");
        let Some(env) = envs
            .iter()
            .find(|e| e.name == "production")
            .or(envs.first())
        else {
            continue;
        };
        let services = api
            .service_instances(&project.id, &env.id)
            .await
            .expect("service instances");
        if let Some(s) = services.into_iter().find(|s| !s.active.is_empty()) {
            println!(
                "target: project …{} env …{} service …{}",
                tail(&project.id),
                tail(&env.id),
                tail(&s.service_id)
            );
            return Target {
                project_id: project.id.clone(),
                env_id: env.id.clone(),
                service_id: s.service_id,
                active: s.active[0].clone(),
                latest: s.latest,
            };
        }
    }
    panic!("no service with a running deployment found");
}

fn tail(id: &str) -> &str {
    &id[id.len().saturating_sub(6)..]
}

fn remaining(api: &Api) -> Option<u32> {
    api.budget().lock().unwrap().remaining()
}

/// 2 requests: one raw query to see every header, one through `Api`.
#[tokio::test]
#[ignore]
async fn live_rate_limit_headers() {
    if !live() {
        return;
    }
    let tokens = tokens();
    let token = {
        let t = tokens.clone();
        tokio::task::spawn_blocking(move || t.token())
            .await
            .unwrap()
            .expect("token")
    };
    let resp = reqwest::Client::new()
        .post(HTTP_ENDPOINT)
        .bearer_auth(token)
        .json(&serde_json::json!({ "query": "query { me { id } }" }))
        .send()
        .await
        .expect("request");
    println!("status: {}", resp.status());
    for (name, value) in resp.headers() {
        let n = name.as_str();
        if n.contains("ratelimit") || n.contains("rate-limit") || n == "retry-after" {
            println!("header {n}: {}", value.to_str().unwrap_or("<non-ascii>"));
        }
    }
    let hourly = parse_hourly_limit(resp.headers());
    println!("parse_hourly_limit(raw headers) = {hourly:?}");

    let api = api(tokens);
    api.workspaces().await.expect("workspaces");
    let budget = api.budget();
    let b = budget.lock().unwrap();
    println!(
        "budget after workspaces(): hourly_limit={:?} remaining={:?}",
        b.hourly_limit(),
        b.remaining()
    );
    assert!(hourly.is_some(), "no hourly limit found in the headers");
}

/// Discovery + 1 request, plus one websocket held open for 60s.
#[tokio::test]
#[ignore]
async fn live_subscription_metering() {
    if !live() {
        return;
    }
    let tokens = tokens();
    let api = api(tokens.clone());
    let target = find_target(&api).await;
    let before = remaining(&api);

    let client = connect_authed(&Url::parse(WS_ENDPOINT).unwrap(), &tokens)
        .await
        .expect("websocket");
    let mut stream = deployment_logs_stream(&client, &target.active.id);
    let (mut batches, mut lines) = (0usize, 0usize);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut ended = None;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => break,
            Ok(None) => {
                ended = Some("stream ended".to_string());
                break;
            }
            Ok(Some(Err(e))) => {
                ended = Some(format!("stream error: {e}"));
                break;
            }
            Ok(Some(Ok(batch))) => {
                batches += 1;
                lines += batch.len();
            }
        }
    }
    drop(stream);
    drop(client);

    // One HTTP request between the two readings.
    api.environments(&target.project_id)
        .await
        .expect("environments");
    let after = remaining(&api);
    println!("subscription: {batches} batches, {lines} lines in 60s; early end: {ended:?}");
    println!("X-RateLimit-Remaining before={before:?} after={after:?}");
    if let (Some(b), Some(a)) = (before, after) {
        let used = i64::from(b) - i64::from(a);
        println!(
            "remaining dropped by {used}; minus 1 HTTP request = {} attributable to the subscription",
            used - 1
        );
    }
}

/// Discovery + 1 request.
#[tokio::test]
#[ignore]
async fn live_log_attribute_keys() {
    if !live() {
        return;
    }
    let api = api(tokens());
    let target = find_target(&api).await;
    let logs = api
        .deployment_logs(&target.active.id, None, 50)
        .await
        .expect("deployment logs");
    let instance_ids: Vec<&str> = target
        .active
        .instances
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    println!(
        "{} lines; deployment has {} instances",
        logs.len(),
        instance_ids.len()
    );
    // key -> (lines carrying it, lines whose value is one of the instance ids)
    let mut keys: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for l in &logs {
        for (k, v) in &l.attributes {
            let e = keys.entry(k.as_str()).or_default();
            e.0 += 1;
            if instance_ids.contains(&v.as_str()) {
                e.1 += 1;
            }
        }
    }
    for (k, (n, matches)) in &keys {
        println!("attribute key {k:?}: on {n} lines, value == an instance id on {matches}");
    }
    for l in logs.iter().take(3) {
        println!("sample {}: {:?}", ts(l), short(&l.message, 40));
    }
}

/// Discovery + 2 requests.
#[tokio::test]
#[ignore]
async fn live_deployments_order() {
    if !live() {
        return;
    }
    let api = api(tokens());
    let t = find_target(&api).await;
    // `Api::deployments` sorts, so compare which items `first:N` selects.
    let few = api
        .deployments(&t.project_id, &t.env_id, &t.service_id, 3)
        .await
        .expect("deployments 3");
    let many = api
        .deployments(&t.project_id, &t.env_id, &t.service_id, 20)
        .await
        .expect("deployments 20");
    let show = |d: &[DeploymentSummary]| {
        d.iter()
            .map(|d| d.created_at.to_rfc3339())
            .collect::<Vec<_>>()
    };
    println!("first:3  createdAt (sorted desc) = {:?}", show(&few));
    println!("first:20 createdAt (sorted desc) = {:?}", show(&many));
    let newest_three: Vec<&str> = many.iter().take(3).map(|d| d.id.as_str()).collect();
    let picked: Vec<&str> = few.iter().map(|d| d.id.as_str()).collect();
    let verdict = if picked == newest_three {
        "first:N returns the NEWEST N"
    } else {
        "first:N does NOT return the newest N"
    };
    println!("{verdict}");
    if many.len() > 3 {
        assert_eq!(picked, newest_three, "{verdict}");
    }
}

/// Discovery + 1 request. Prints only the count, never names or values.
#[tokio::test]
#[ignore]
async fn live_variables_read() {
    if !live() {
        return;
    }
    let api = api(tokens());
    let t = find_target(&api).await;
    let vars = api
        .variables(&t.project_id, &t.env_id, &t.service_id)
        .await
        .expect("variables");
    println!("variables: {} defined", vars.len());
}

/// Discovery + 4 requests.
#[tokio::test]
#[ignore]
async fn live_deployment_logs_window() {
    if !live() {
        return;
    }
    let api = api(tokens());
    let t = find_target(&api).await;
    let dep = &t.active.id;
    let span = |l: &[LogLine]| match (l.first(), l.last()) {
        (Some(a), Some(b)) => format!("{} lines, {} .. {}", l.len(), ts(a), ts(b)),
        _ => "0 lines".into(),
    };
    let ascending = |l: &[LogLine]| l.windows(2).all(|w| w[0].timestamp <= w[1].timestamp);

    let no_start = api
        .deployment_logs(dep, None, 100)
        .await
        .expect("logs without startDate");
    println!(
        "no startDate, limit 100: {} (ascending: {})",
        span(&no_start),
        ascending(&no_start)
    );

    // A limit below the total shows which end of the window the API keeps.
    let half = (no_start.len() / 2).max(1) as i64;
    let which_end = |page: &[LogLine]| {
        let n = page.len();
        let all = &no_start;
        if n == 0 || all.len() <= n {
            "inconclusive"
        } else if page.first().map(|l| l.timestamp) == all.first().map(|l| l.timestamp) {
            "OLDEST"
        } else if page.last().map(|l| l.timestamp) == all.last().map(|l| l.timestamp) {
            "NEWEST"
        } else {
            "neither end"
        }
    };
    let no_start_half = api
        .deployment_logs(dep, None, half)
        .await
        .expect("logs without startDate, small limit");
    println!(
        "no startDate, limit {half}: {} -> {} lines",
        span(&no_start_half),
        which_end(&no_start_half)
    );

    let created = t.active.created_at;
    println!("deployment createdAt: {}", created.to_rfc3339());
    let from_created = api
        .deployment_logs(dep, Some(created), half)
        .await
        .expect("logs from createdAt");
    println!(
        "startDate=createdAt, limit {half}: {} (ascending: {}) -> {} lines after startDate",
        span(&from_created),
        ascending(&from_created),
        which_end(&from_created)
    );

    if no_start.len() >= 10 {
        let tenth = &no_start[9];
        let from_tenth = api
            .deployment_logs(dep, Some(tenth.timestamp), 20)
            .await
            .expect("logs from 10th line");
        println!(
            "startDate=10th line ({}), limit 20: {}",
            ts(tenth),
            span(&from_tenth)
        );
        let inclusive = from_tenth
            .iter()
            .any(|l| l.timestamp == tenth.timestamp && l.message == tenth.message);
        println!("10th line included (startDate inclusive): {inclusive}");
        let overlaps_next = from_tenth.first().map(|l| l.timestamp) <= Some(tenth.timestamp);
        println!("page starts at/before the 10th line: {overlaps_next}");
    }
}

/// Discovery + 2 requests (snapshot + environmentLogs).
#[tokio::test]
#[ignore]
async fn live_build_logs() {
    if !live() {
        return;
    }
    let api = api(tokens());
    let t = find_target(&api).await;
    let dep = match &t.latest {
        Some(d) if d.status == DeploymentStatus::Success => d.clone(),
        _ => t.active.clone(),
    };
    println!(
        "deployment …{} status {} createdAt {}",
        tail(&dep.id),
        dep.status.label(),
        dep.created_at.to_rfc3339()
    );
    let logs = api
        .build_logs(&t.env_id, &dep.id, None, 500)
        .await
        .expect("build logs");
    match (logs.first(), logs.last()) {
        (Some(a), Some(b)) => {
            println!("{} build lines, {} .. {}", logs.len(), ts(a), ts(b));
            println!(
                "first line is {}s after createdAt",
                (a.timestamp - dep.created_at).num_seconds()
            );
            for l in logs.iter().take(2) {
                println!("sample: {:?}", short(&l.message, 40));
            }
        }
        _ => println!("0 build lines"),
    }
}
