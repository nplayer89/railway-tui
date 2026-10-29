//! Service health derivation and deployment-list ordering.

use crate::model::{DeploymentStatus, DeploymentSummary, InstanceStatus, LogKind, ServiceInstance};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Degraded,
    Failing,
    FailedButServing,
    Progressing,
    Inactive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowStatus {
    pub health: Health,
    pub label: String,
    pub boxes: Vec<InstanceStatus>,
    pub ready: u32,
    pub desired: u32,
}

pub fn derive_row(svc: &ServiceInstance) -> RowStatus {
    let source: Vec<&DeploymentSummary> = if svc.active.is_empty() {
        svc.latest.iter().collect()
    } else {
        svc.active.iter().collect()
    };
    let boxes: Vec<InstanceStatus> = source
        .iter()
        .flat_map(|d| d.instances.iter().map(|i| i.status.clone()))
        .collect();
    let ready = boxes
        .iter()
        .filter(|s| **s == InstanceStatus::Running)
        .count() as u32;
    let desired = svc.num_replicas.max(1).max(ready);

    let (health, label) = match &svc.latest {
        None => (Health::Inactive, "NO DEPLOYS".to_string()),
        Some(latest) => {
            use DeploymentStatus as D;
            match &latest.status {
                D::Failed | D::Crashed => {
                    let prev_running = svc.active.iter().any(|a| {
                        a.id != latest.id
                            && a.instances
                                .iter()
                                .any(|i| i.status == InstanceStatus::Running)
                    });
                    if prev_running {
                        (
                            Health::FailedButServing,
                            format!("{} (prev running)", latest.status.label()),
                        )
                    } else {
                        (Health::Failing, latest.status.label())
                    }
                }
                D::Building
                | D::Deploying
                | D::Queued
                | D::Waiting
                | D::Initializing
                | D::NeedsApproval => (Health::Progressing, latest.status.label()),
                D::Removed | D::Removing | D::Skipped | D::Sleeping | D::Unknown(_) => {
                    (Health::Inactive, latest.status.label())
                }
                D::Success if ready < desired => (Health::Degraded, "DEGRADED".to_string()),
                D::Success => (Health::Healthy, "SUCCESS".to_string()),
            }
        }
    };

    RowStatus {
        health,
        label,
        boxes,
        ready,
        desired,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxColour {
    Green,
    Red,
    Yellow,
    Grey,
}

pub fn box_colour(s: &InstanceStatus) -> BoxColour {
    use InstanceStatus as I;
    match s {
        I::Running => BoxColour::Green,
        I::Crashed | I::Exited => BoxColour::Red,
        I::Created | I::Initializing | I::Restarting => BoxColour::Yellow,
        _ => BoxColour::Grey,
    }
}

/// Railway uses CRASHED for runtime failures and FAILED for build or
/// healthcheck failures, so Failed and Building default to the build log.
pub fn default_log_kind(d: &DeploymentSummary) -> LogKind {
    match d.status {
        DeploymentStatus::Failed | DeploymentStatus::Building => LogKind::Build,
        _ => LogKind::Runtime,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTag {
    Latest,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRow {
    pub deployment: DeploymentSummary,
    pub tag: Option<RowTag>,
}

/// Latest first (if not also active), then active, then the rest of history
/// in its original order, with no duplicate ids.
pub fn deployment_rows(svc: &ServiceInstance, history: &[DeploymentSummary]) -> Vec<DeploymentRow> {
    let mut rows: Vec<DeploymentRow> = Vec::new();
    let mut push = |d: &DeploymentSummary, tag: Option<RowTag>| {
        if !rows.iter().any(|r| r.deployment.id == d.id) {
            rows.push(DeploymentRow {
                deployment: d.clone(),
                tag,
            });
        }
    };
    if let Some(latest) = &svc.latest {
        if !svc.active.iter().any(|a| a.id == latest.id) {
            push(latest, Some(RowTag::Latest));
        }
    }
    for a in &svc.active {
        push(a, Some(RowTag::Active));
    }
    for h in history {
        push(h, None);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use chrono::{TimeZone, Utc};

    fn deploy(id: &str, status: DeploymentStatus, inst: &[InstanceStatus]) -> DeploymentSummary {
        DeploymentSummary {
            id: id.into(),
            status,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            commit_sha: None,
            commit_message: None,
            repo: None,
            branch: None,
            image: None,
            instances: inst
                .iter()
                .enumerate()
                .map(|(i, s)| Instance {
                    id: format!("{id}-i{i}"),
                    status: s.clone(),
                })
                .collect(),
        }
    }

    fn svc(
        n: u32,
        latest: Option<DeploymentSummary>,
        active: Vec<DeploymentSummary>,
    ) -> ServiceInstance {
        ServiceInstance {
            service_id: "s".into(),
            service_name: "svc".into(),
            num_replicas: n,
            latest,
            active,
        }
    }

    use DeploymentStatus as D;
    use InstanceStatus as I;

    #[test]
    fn latest_failed_prev_running_is_striped_case() {
        let d1 = deploy("d1", D::Success, &[I::Running, I::Running]);
        let d2 = deploy("d2", D::Failed, &[]);
        let r = derive_row(&svc(2, Some(d2), vec![d1]));
        assert_eq!(r.health, Health::FailedButServing);
        assert_eq!(r.label, "FAILED (prev running)");
        assert_eq!(r.boxes, vec![I::Running, I::Running]);
        assert_eq!(r.ready, 2);
        assert_eq!(r.desired, 2);
    }

    #[test]
    fn latest_failed_nothing_serving_is_failing() {
        let d2 = deploy("d2", D::Failed, &[]);
        let r = derive_row(&svc(1, Some(d2), vec![]));
        assert_eq!(r.health, Health::Failing);
        assert_eq!(r.label, "FAILED");
    }

    #[test]
    fn success_with_crashed_replica_is_degraded() {
        let d = deploy("d1", D::Success, &[I::Running, I::Crashed]);
        let r = derive_row(&svc(2, Some(d.clone()), vec![d]));
        assert_eq!(r.health, Health::Degraded);
        assert_eq!(r.label, "DEGRADED");
        assert_eq!(r.ready, 1);
        assert_eq!(r.desired, 2);
    }

    #[test]
    fn building_while_old_runs_is_progressing_and_boxes_show_running() {
        let d1 = deploy("d1", D::Success, &[I::Running]);
        let d2 = deploy("d2", D::Building, &[]);
        let r = derive_row(&svc(1, Some(d2), vec![d1]));
        assert_eq!(r.health, Health::Progressing);
        assert_eq!(r.boxes, vec![I::Running]);
    }

    #[test]
    fn no_deployments_is_inactive() {
        let r = derive_row(&svc(0, None, vec![]));
        assert_eq!(r.health, Health::Inactive);
        assert_eq!(r.label, "NO DEPLOYS");
        assert_eq!(r.desired, 1);
    }

    #[test]
    fn unknown_status_is_inactive_and_labelled() {
        let s = D::from_api("HIBERNATING");
        assert_eq!(s, D::Unknown("HIBERNATING".into()));
        let d = deploy("d1", s, &[]);
        let r = derive_row(&svc(1, Some(d), vec![]));
        assert_eq!(r.health, Health::Inactive);
        assert_eq!(r.label, "UNKNOWN(HIBERNATING)");
        assert_eq!(box_colour(&I::from_api("WEIRD")), BoxColour::Grey);
    }

    #[test]
    fn deployment_rows_pin_latest_then_active_then_history() {
        let d1 = deploy("d1", D::Success, &[]);
        let d2 = deploy("d2", D::Success, &[I::Running]);
        let d3 = deploy("d3", D::Failed, &[]);
        let s = svc(1, Some(d3.clone()), vec![d2.clone()]);
        let rows = deployment_rows(&s, &[d3, d2, d1]);
        let ids: Vec<_> = rows.iter().map(|r| r.deployment.id.as_str()).collect();
        let tags: Vec<_> = rows.iter().map(|r| r.tag).collect();
        assert_eq!(ids, ["d3", "d2", "d1"]);
        assert_eq!(tags, [Some(RowTag::Latest), Some(RowTag::Active), None]);
    }

    #[test]
    fn default_log_kind_rules() {
        let k = |s| default_log_kind(&deploy("d", s, &[]));
        assert_eq!(k(D::Failed), LogKind::Build);
        assert_eq!(k(D::Building), LogKind::Build);
        assert_eq!(k(D::Crashed), LogKind::Runtime);
        assert_eq!(k(D::Success), LogKind::Runtime);
    }
}
