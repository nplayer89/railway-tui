//! Deploying one pending group: read Railway, confirm with `y`, then follow
//! the worker's steps. Every id comes from the group, never from the open
//! project or environment, and only what was actually sent is marked applied.
//!
//! Also deploying a service's latest commit (`D` on Services or Deployments):
//! one mutation, confirmed with `y`.

use std::time::Instant;

use super::{
    App, Command, Confirm, DeployConfirm, DeployStep, Kind, LatestConfirm, LatestOutcome, Phase,
    VarsPurpose, View,
};
use crate::model::Variables;
use crate::pending::Change;

/// A deploy in flight: exactly what was sent, and the names to report it by.
#[derive(Clone)]
pub(super) struct Sent {
    project_id: String,
    env_name: String,
    service_name: String,
    upserts: Variables,
    deletes: Vec<String>,
}

/// A latest-commit deploy in flight, with the names to report it by.
#[derive(Clone)]
pub(super) struct LatestSent {
    project_id: String,
    env_name: String,
    service_name: String,
}

impl App {
    /// Whether any deploy (pending group or latest commit) is in flight.
    pub(super) fn deploy_in_flight(&self) -> bool {
        !self.deploys.is_empty() || !self.latest_deploys.is_empty()
    }

    /// `D` on a service or its deployments: ask before deploying its latest
    /// commit, showing what is deployed now and where the new one goes.
    pub(super) fn request_deploy_latest(&mut self, service_id: String) {
        let (Some(project), Some(env)) = (self.project.clone(), self.environment.clone()) else {
            self.message = Some("cannot deploy: no environment is open".into());
            return;
        };
        let Some(svc) = self.service(&service_id) else {
            self.message = Some("cannot deploy: the service is not loaded yet".into());
            return;
        };
        let key = (env.id.clone(), service_id.clone());
        if self.deploys.contains_key(&key) || self.latest_deploys.contains_key(&key) {
            self.message = Some(format!("already deploying {}", svc.service_name));
            return;
        }
        let source = svc.latest.as_ref().or(svc.active.first());
        let repo = source.and_then(|d| d.repo.clone());
        if repo.is_none() && source.is_some_and(|d| d.image.is_some()) {
            self.message = Some(format!(
                "cannot deploy {}: it runs an image, not a repo",
                svc.service_name
            ));
            return;
        }
        let written = self
            .pending
            .service(&env.id, &service_id)
            .map_or(0, |sp| sp.vars.values().filter(|v| v.applied).count());
        self.confirm = Some(Confirm::DeployLatest(Box::new(LatestConfirm {
            project_id: project.id,
            env_id: env.id,
            env_name: env.name,
            service_id,
            service_name: svc.service_name.clone(),
            repo,
            branch: source.and_then(|d| d.branch.clone()),
            current: svc.active.first().or(svc.latest.as_ref()).cloned(),
            written,
        })));
    }

    /// `y` on the latest-commit confirm: send the deploy.
    pub(super) fn start_deploy_latest(&mut self, c: LatestConfirm, out: &mut Vec<Command>) {
        let key = (c.env_id, c.service_id);
        if self.deploys.contains_key(&key) || self.latest_deploys.contains_key(&key) {
            self.message = Some(format!("already deploying {}", c.service_name));
            return;
        }
        self.message = Some(format!(
            "deploying the latest commit of {} to {}…",
            c.service_name, c.env_name
        ));
        out.push(Command::DeployLatest {
            env_id: key.0.clone(),
            service_id: key.1.clone(),
        });
        self.latest_deploys.insert(
            key,
            LatestSent {
                project_id: c.project_id,
                env_name: c.env_name,
                service_name: c.service_name,
            },
        );
    }

    pub(super) fn on_deploy_latest(
        &mut self,
        env_id: String,
        service_id: String,
        outcome: LatestOutcome,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        let key = (env_id, service_id);
        let Some(sent) = self.latest_deploys.remove(&key) else {
            return;
        };
        match outcome {
            LatestOutcome::Started => {
                // Written variables went out with this deploy.
                if self.pending.service(&key.0, &key.1).is_some_and(|sp| {
                    sp.vars.values().any(|v| v.applied) || sp.written_not_deployed
                }) {
                    self.finish_deploy(&key);
                    out.push(Command::SavePending(self.pending.clone()));
                }
                self.message = Some(format!(
                    "deploying the latest commit of {} to {}: Railway accepted it",
                    sent.service_name, sent.env_name
                ));
                self.reload_after_deploy(&key, &sent.project_id, now, out);
                if self.project_id() == Some(sent.project_id.as_str())
                    && self.env_id() == Some(key.0.as_str())
                    && self.view_service_id(Kind::Deployments) == Some(key.1.as_str())
                {
                    self.load_deployments(key.1.clone(), now, out);
                }
            }
            LatestOutcome::Refused(error) => {
                self.message = Some(format!("deploy of {} failed: {error}", sent.service_name));
            }
            LatestOutcome::Unknown(_) => {
                self.message = Some(format!(
                    "deploy of {} outcome unknown — check the deployments view",
                    sent.service_name
                ));
            }
        }
    }

    /// `D` on a group in the review: read the service's variables first, so
    /// the confirm can list what changed on Railway since the edits.
    pub(super) fn request_deploy(&mut self, env_id: String, service_id: String) -> Vec<Command> {
        if self.environments.value.is_none() {
            self.message = Some("cannot deploy: environments are not loaded yet".into());
            return Vec::new();
        }
        let Some(sp) = self.pending.service(&env_id, &service_id) else {
            return Vec::new();
        };
        if self
            .deploys
            .contains_key(&(env_id.clone(), service_id.clone()))
        {
            self.message = Some(format!("already deploying {}", sp.service_name));
            return Vec::new();
        }
        self.message = Some(format!("checking {} on Railway…", sp.service_name));
        vec![Command::LoadVariables {
            project_id: sp.project_id.clone(),
            env_id,
            service_id,
            purpose: VarsPurpose::Deploy,
        }]
    }

    /// Whether a deploy of this group is in flight (including its reconcile read).
    pub(super) fn is_deploying(&self, env_id: &str, service_id: &str) -> bool {
        self.deploys
            .contains_key(&(env_id.to_string(), service_id.to_string()))
    }

    /// Railway's variables for a deploy check arrived: open the confirm.
    pub(super) fn on_deploy_check(
        &mut self,
        env_id: String,
        service_id: String,
        result: Result<Variables, String>,
    ) {
        let Some(sp) = self.pending.service(&env_id, &service_id) else {
            return;
        };
        let current = match result {
            Ok(current) => current,
            Err(e) => {
                self.message = Some(format!(
                    "cannot deploy {}: could not read its variables: {e}",
                    sp.service_name
                ));
                return;
            }
        };
        if self
            .deploys
            .contains_key(&(env_id.clone(), service_id.clone()))
        {
            self.message = Some(format!("already deploying {}", sp.service_name));
            return;
        }
        // The confirm takes the next key, so it may only appear where Neil is
        // waiting for it: on the review, with nothing being typed or picked.
        let busy = self.confirm.is_some()
            || self.var_input.is_some()
            || self.filter_input.is_some()
            || self.command_line.is_some()
            || self.env_picker.is_some()
            || self.theme_picker.is_some()
            || self.help
            || self.kind() != Kind::Pending;
        if busy {
            self.message = Some(format!(
                "deploy check of {} done; press D again to deploy",
                sp.service_name
            ));
            return;
        }
        let drift = sp
            .vars
            .iter()
            .filter(|(_, v)| !v.applied)
            .filter(|(name, v)| match &v.base {
                Some(base) => current.get(*name) != Some(base),
                None => current.contains_key(*name),
            })
            .map(|(name, _)| name.clone())
            .collect();
        self.confirm = Some(Confirm::Deploy(DeployConfirm {
            env_id,
            service_id,
            drift,
        }));
    }

    /// `y` on the deploy confirm: send the group's unapplied changes.
    pub(super) fn start_deploy(&mut self, d: DeployConfirm, out: &mut Vec<Command>) {
        let key = (d.env_id, d.service_id);
        let Some(sp) = self.pending.service(&key.0, &key.1) else {
            self.message = Some("nothing to deploy".into());
            return;
        };
        if self.deploys.contains_key(&key) {
            self.message = Some(format!("already deploying {}", sp.service_name));
            return;
        }
        let mut upserts = Variables::new();
        let mut deletes = Vec::new();
        for (name, var) in sp.vars.iter().filter(|(_, v)| !v.applied) {
            match &var.change {
                Change::Set { value } => {
                    upserts.insert(name.clone(), value.clone());
                }
                Change::Delete => deletes.push(name.clone()),
            }
        }
        let redeploy_only = upserts.is_empty() && deletes.is_empty() && sp.written_not_deployed;
        let sent = Sent {
            project_id: sp.project_id.clone(),
            env_name: sp.env_name.clone(),
            service_name: sp.service_name.clone(),
            upserts: upserts.clone(),
            deletes: deletes.clone(),
        };
        self.message = Some(format!(
            "deploying {} to {}…",
            sent.service_name, sent.env_name
        ));
        out.push(Command::DeployService {
            project_id: sent.project_id.clone(),
            env_id: key.0.clone(),
            service_id: key.1.clone(),
            upserts,
            deletes,
            redeploy_only,
        });
        self.deploys.insert(key, sent);
    }

    pub(super) fn on_deploy_step(
        &mut self,
        env_id: String,
        service_id: String,
        step: DeployStep,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        let key = (env_id, service_id);
        let Some(sent) = self.deploys.get(&key).cloned() else {
            return;
        };
        match step {
            DeployStep::Upserted => {
                self.mark_applied(&key, |name, change| {
                    sent.upserts
                        .get(name)
                        .is_some_and(|v| *change == Change::Set { value: v.clone() })
                });
                out.push(Command::SavePending(self.pending.clone()));
                self.message = Some(format!(
                    "deploying {} to {}: variables written…",
                    sent.service_name, sent.env_name
                ));
            }
            DeployStep::Deleted(deleted) => {
                self.mark_applied(&key, |name, change| {
                    name == deleted && sent.deletes.contains(&deleted) && *change == Change::Delete
                });
                out.push(Command::SavePending(self.pending.clone()));
                self.message = Some(format!(
                    "deploying {} to {}: {deleted} deleted…",
                    sent.service_name, sent.env_name
                ));
            }
            DeployStep::Redeployed => {
                self.deploys.remove(&key);
                self.finish_deploy(&key);
                out.push(Command::SavePending(self.pending.clone()));
                self.message = Some(format!(
                    "deployed {} to {}",
                    sent.service_name, sent.env_name
                ));
                self.reload_after_deploy(&key, &sent.project_id, now, out);
            }
            DeployStep::Failed {
                during: Phase::Redeploy,
                unknown: true,
                ..
            } => {
                self.deploys.remove(&key);
                self.message = Some("redeploy outcome unknown — check the deployments view".into());
            }
            DeployStep::Failed {
                during,
                unknown: true,
                ..
            } => {
                // Kept in flight until Railway says what landed.
                self.message = Some(format!(
                    "outcome of {during} unknown: checking {} on Railway…",
                    sent.service_name
                ));
                out.push(Command::LoadVariables {
                    project_id: sent.project_id,
                    env_id: key.0,
                    service_id: key.1,
                    purpose: VarsPurpose::Reconcile,
                });
            }
            DeployStep::Failed { during, error, .. } => {
                self.deploys.remove(&key);
                self.message = Some(format!("deploy failed during {during}: {error}"));
            }
        }
    }

    /// After a write with an unknown outcome: mark each entry applied that
    /// Railway now matches (the new value is there, or a deleted variable is gone).
    pub(super) fn on_reconcile(
        &mut self,
        env_id: String,
        service_id: String,
        result: Result<Variables, String>,
        out: &mut Vec<Command>,
    ) {
        let key = (env_id, service_id);
        let Some(sent) = self.deploys.remove(&key) else {
            return;
        };
        let current = match result {
            Ok(current) => current,
            Err(e) => {
                self.message = Some(format!(
                    "deploy of {} interrupted and Railway could not be checked: {e}",
                    sent.service_name
                ));
                return;
            }
        };
        self.mark_applied(&key, |name, change| match change {
            Change::Set { value } => current.get(name) == Some(value),
            Change::Delete => !current.contains_key(name),
        });
        out.push(Command::SavePending(self.pending.clone()));
        if let Some(sp) = self.pending.service(&key.0, &key.1) {
            let applied = sp.vars.values().filter(|v| v.applied).count();
            self.message = Some(format!(
                "deploy of {} interrupted: {applied} of {} changes are on Railway; D retries",
                sent.service_name,
                sp.vars.len()
            ));
        }
    }

    /// Marks unapplied entries for which `matches(name, change)` holds; once
    /// every entry is applied the group is written but not deployed.
    fn mark_applied(&mut self, key: &(String, String), matches: impl Fn(&str, &Change) -> bool) {
        let Some(sp) = self
            .pending
            .envs
            .get_mut(&key.0)
            .and_then(|s| s.get_mut(&key.1))
        else {
            return;
        };
        for (name, var) in sp.vars.iter_mut().filter(|(_, v)| !v.applied) {
            if matches(name, &var.change) {
                var.applied = true;
            }
        }
        if sp.vars.values().all(|v| v.applied) {
            sp.written_not_deployed = true;
        }
    }

    /// The redeploy succeeded: drop the applied entries. An entry edited while
    /// the deploy ran was not sent, so it stays pending.
    fn finish_deploy(&mut self, key: &(String, String)) {
        let applied: Vec<String> = self
            .pending
            .service(&key.0, &key.1)
            .map(|sp| {
                sp.vars
                    .iter()
                    .filter(|(_, v)| v.applied)
                    .map(|(n, _)| n.clone())
                    .collect()
            })
            .unwrap_or_default();
        for name in applied {
            self.pending.undo(&key.0, &key.1, &name);
        }
        if let Some(sp) = self
            .pending
            .envs
            .get_mut(&key.0)
            .and_then(|s| s.get_mut(&key.1))
        {
            sp.written_not_deployed = false;
        }
    }

    /// Reloads Services and the service's variables when they show what was deployed.
    fn reload_after_deploy(
        &mut self,
        key: &(String, String),
        project_id: &str,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        if self.project_id() != Some(project_id) || self.env_id() != Some(key.0.as_str()) {
            return;
        }
        if self.stack.iter().any(|v| matches!(v, View::Services)) {
            self.load_services(now, out);
        }
        if self.view_service_id(Kind::Variables) == Some(key.1.as_str()) {
            self.load_variables(key.1.clone(), now, out);
        }
    }
}
