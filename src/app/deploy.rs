//! Deploying one pending group: read Railway, confirm with `y`, then follow
//! the worker's steps. Every id comes from the group, never from the open
//! project or environment, and only what was actually sent is marked applied.
//!
//! Also deploying a service's latest commit (`D` on Services or Deployments),
//! or a branch picked with `d` (switching the service's trigger to it first):
//! confirmed with `y`.

use std::time::Instant;

use super::{
    App, BranchChoices, BranchPicker, BranchReply, BranchSwitch, Command, Confirm, DeployConfirm,
    DeployStep, GroupKey, Kind, LatestConfirm, LatestOutcome, Modal, Phase, Secret, VarsPurpose,
    View,
};
use crate::model::{Variables, WriteFailure};
use crate::pending::{Change, PendingVar};

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
    /// The branch the trigger was switched to, when one was picked with `d`.
    switched_to: Option<String>,
}

impl App {
    /// Whether any deploy (pending group or latest commit) is in flight.
    pub(super) fn deploy_in_flight(&self) -> bool {
        !self.deploys.is_empty() || !self.latest_deploys.is_empty()
    }

    /// `D` on a service or its deployments: ask before deploying its latest
    /// commit, showing what is deployed now and where the new one goes.
    pub(super) fn request_deploy_latest(&mut self, service_id: String) {
        if let Some(target) = self.latest_target(&service_id) {
            self.modal = Some(Modal::Confirm(Confirm::DeployLatest(Box::new(target))));
        }
    }

    /// `d` on a service or its deployments: open the branch picker and load
    /// the branches and the trigger that would be switched.
    pub(super) fn open_branch_picker(&mut self, service_id: String, out: &mut Vec<Command>) {
        let Some(target) = self.latest_target(&service_id) else {
            return;
        };
        let Some(repo) = target.repo.clone() else {
            self.message = Some(format!(
                "cannot pick a branch: no GitHub repo is known for {}",
                target.service_name
            ));
            return;
        };
        out.push(Command::LoadBranches {
            project_id: target.project_id.clone(),
            env_id: target.env_id.clone(),
            service_id: target.service_id.clone(),
            repo,
        });
        self.modal = Some(Modal::BranchPicker(Box::new(BranchPicker {
            target,
            choices: None,
            query: String::new(),
            index: 0,
        })));
    }

    /// The service's triggers and branches arrived: fill the open picker.
    pub(super) fn on_branches(
        &mut self,
        env_id: &str,
        service_id: &str,
        result: Result<BranchReply, String>,
    ) {
        let Some(Modal::BranchPicker(picker)) = &mut self.modal else {
            return;
        };
        if picker.target.env_id != env_id || picker.target.service_id != service_id {
            return;
        }
        let repo = picker.target.repo.as_deref().unwrap_or_default();
        picker.choices = Some(result.and_then(|BranchReply { triggers, names }| {
            let trigger = triggers
                .iter()
                .find(|t| t.repository == repo)
                .or(triggers.first())
                .ok_or("the service has no GitHub deploy trigger in this environment")?;
            let (mut names, unlisted) = match names {
                Ok(names) => (names, None),
                Err(why) => (Vec::new(), Some(why)),
            };
            names.sort();
            Ok(BranchChoices {
                trigger_id: trigger.id.clone(),
                connected: trigger.branch.clone(),
                names,
                unlisted,
            })
        }));
        picker.index = 0;
    }

    /// Enter in the picker: confirm deploying the highlighted branch.
    pub(super) fn pick_branch(&mut self) {
        let Some(Modal::BranchPicker(picker)) = &self.modal else {
            return;
        };
        let Some(Ok(choices)) = &picker.choices else {
            return;
        };
        let name = if choices.unlisted.is_some() {
            Some(picker.query.trim().to_string()).filter(|n| !n.is_empty())
        } else {
            picker.filtered().get(picker.index).map(|n| n.to_string())
        };
        let Some(name) = name else {
            return;
        };
        let switch = (name != choices.connected).then(|| BranchSwitch {
            trigger_id: choices.trigger_id.clone(),
            from: choices.connected.clone(),
            to: name.clone(),
        });
        let service_id = picker.target.service_id.clone();
        self.modal = None;
        // Checked again: a deploy may have started while the picker was open.
        if let Some(target) = self.latest_target(&service_id) {
            self.modal = Some(Modal::Confirm(Confirm::DeployLatest(Box::new(
                LatestConfirm {
                    branch: Some(name),
                    switch,
                    ..target
                },
            ))));
        }
    }

    /// The deploy of the service's connected branch as it stands, or `None`
    /// (with a message) when it cannot be deployed now.
    fn latest_target(&mut self, service_id: &str) -> Option<LatestConfirm> {
        let (Some(project), Some(env)) = (self.project.clone(), self.environment.clone()) else {
            self.message = Some("cannot deploy: no environment is open".into());
            return None;
        };
        let Some(svc) = self.service(service_id) else {
            self.message = Some("cannot deploy: the service is not loaded yet".into());
            return None;
        };
        if self.is_deploying(&env.id, service_id) || self.is_deploying_latest(&env.id, service_id) {
            self.message = Some(format!("already deploying {}", svc.service_name));
            return None;
        }
        let source = svc.latest.as_ref().or(svc.active.first());
        let repo = source.and_then(|d| d.repo.clone());
        if repo.is_none() && source.is_some_and(|d| d.image.is_some()) {
            self.message = Some(format!(
                "cannot deploy {}: it runs an image, not a repo",
                svc.service_name
            ));
            return None;
        }
        let written = self
            .pending
            .service(&env.id, service_id)
            .map_or(0, |sp| sp.vars.values().filter(|v| v.applied).count());
        Some(LatestConfirm {
            project_id: project.id,
            env_id: env.id,
            env_name: env.name,
            service_id: service_id.to_string(),
            service_name: svc.service_name.clone(),
            repo,
            branch: source.and_then(|d| d.branch.clone()),
            current: svc.active.first().or(svc.latest.as_ref()).cloned(),
            written,
            switch: None,
        })
    }

    /// `y` on the latest-commit confirm: send the deploy.
    pub(super) fn start_deploy_latest(&mut self, c: LatestConfirm, out: &mut Vec<Command>) {
        let key = GroupKey::new(c.env_id, c.service_id);
        if self.deploys.contains_key(&key) || self.latest_deploys.contains_key(&key) {
            self.message = Some(format!("already deploying {}", c.service_name));
            return;
        }
        self.message = Some(match &c.switch {
            Some(sw) => format!(
                "switching {} to {} and deploying to {}…",
                c.service_name, sw.to, c.env_name
            ),
            None => format!(
                "deploying the latest commit of {} to {}…",
                c.service_name, c.env_name
            ),
        });
        self.latest_deploys.insert(
            key.clone(),
            LatestSent {
                project_id: c.project_id,
                env_name: c.env_name,
                service_name: c.service_name,
                switched_to: c.switch.as_ref().map(|sw| sw.to.clone()),
            },
        );
        out.push(Command::DeployLatest {
            env_id: key.env_id,
            service_id: key.service_id,
            switch: c.switch,
        });
    }

    pub(super) fn on_deploy_latest(
        &mut self,
        env_id: String,
        service_id: String,
        outcome: LatestOutcome,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        let key = GroupKey::new(env_id, service_id);
        let Some(sent) = self.latest_deploys.remove(&key) else {
            return;
        };
        let name = &sent.service_name;
        // Said first whenever the switch went through, so it is never missed.
        let switched = sent
            .switched_to
            .as_ref()
            .map_or(String::new(), |b| format!("switched {name} to {b}; "));
        match outcome {
            LatestOutcome::Started => {
                // Written variables went out with this deploy.
                if self
                    .pending
                    .service(&key.env_id, &key.service_id)
                    .is_some_and(|sp| {
                        sp.vars.values().any(|v| v.applied) || sp.written_not_deployed
                    })
                {
                    self.finish_deploy(&key);
                    out.push(self.save_pending());
                }
                self.message = Some(format!(
                    "{switched}deploying the latest commit of {name} to {}: Railway accepted it",
                    sent.env_name
                ));
                self.reload_after_deploy(&key, &sent.project_id, now, out);
                if self.project_id() == Some(sent.project_id.as_str())
                    && self.env_id() == Some(key.env_id.as_str())
                    && self.view_service_id(Kind::Deployments) == Some(key.service_id.as_str())
                {
                    self.load_deployments(key.service_id.clone(), now, out);
                }
            }
            LatestOutcome::Failed {
                error,
                failure: WriteFailure::Refused,
            } => {
                self.message = Some(format!("{switched}deploy of {name} failed: {error}"));
            }
            LatestOutcome::Failed {
                failure: WriteFailure::Unknown,
                ..
            } => {
                self.message = Some(format!(
                    "{switched}deploy of {name} outcome unknown — check the deployments view"
                ));
            }
            LatestOutcome::SwitchFailed { error, failure } => {
                let branch = sent.switched_to.as_deref().unwrap_or_default();
                self.message = Some(match failure {
                    WriteFailure::Unknown => format!(
                        "switching {name} to {branch}: outcome unknown — check its branch on Railway; nothing was deployed"
                    ),
                    WriteFailure::Refused => format!(
                        "could not switch {name} to {branch}: {error}; nothing was deployed"
                    ),
                });
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
        if self.is_deploying(&env_id, &service_id) {
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
        self.deploys.keys().any(|k| k.is(env_id, service_id))
    }

    /// Whether a latest-commit deploy of this service is waiting for Railway's answer.
    fn is_deploying_latest(&self, env_id: &str, service_id: &str) -> bool {
        self.latest_deploys.keys().any(|k| k.is(env_id, service_id))
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
        if self.is_deploying(&env_id, &service_id) {
            self.message = Some(format!("already deploying {}", sp.service_name));
            return;
        }
        // The confirm takes the next key, so it may only appear where Neil is
        // waiting for it: on the review, with nothing being typed or picked.
        let busy = self.modal.is_some() || self.help || self.kind() != Kind::Pending;
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
            .filter(|(_, v)| !v.applied && !v.base_sealed)
            .filter(|(name, v)| match &v.base {
                Some(base) => current.get(*name) != Some(base),
                None => current.contains_key(*name),
            })
            .map(|(name, _)| name.clone())
            .collect();
        self.modal = Some(Modal::Confirm(Confirm::Deploy(DeployConfirm {
            env_id,
            service_id,
            drift,
        })));
    }

    /// `y` on the deploy confirm: send the group's unapplied changes.
    pub(super) fn start_deploy(&mut self, d: DeployConfirm, out: &mut Vec<Command>) {
        let key = GroupKey::new(d.env_id, d.service_id);
        let Some(sp) = self.pending.service(&key.env_id, &key.service_id) else {
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
            env_id: key.env_id.clone(),
            service_id: key.service_id.clone(),
            upserts: Secret::new(upserts),
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
        let key = GroupKey::new(env_id, service_id);
        let Some(sent) = self.deploys.get(&key).cloned() else {
            return;
        };
        match step {
            DeployStep::Upserted => {
                self.mark_applied(&key, |name, var| {
                    sent.upserts
                        .get(name)
                        .is_some_and(|v| var.change == Change::Set { value: v.clone() })
                });
                out.push(self.save_pending());
                self.message = Some(format!(
                    "deploying {} to {}: variables written…",
                    sent.service_name, sent.env_name
                ));
            }
            DeployStep::Deleted(deleted) => {
                self.mark_applied(&key, |name, var| {
                    name == deleted
                        && sent.deletes.contains(&deleted)
                        && var.change == Change::Delete
                });
                out.push(self.save_pending());
                self.message = Some(format!(
                    "deploying {} to {}: {deleted} deleted…",
                    sent.service_name, sent.env_name
                ));
            }
            DeployStep::Redeployed => {
                self.deploys.remove(&key);
                self.finish_deploy(&key);
                out.push(self.save_pending());
                self.message = Some(format!(
                    "deployed {} to {}",
                    sent.service_name, sent.env_name
                ));
                self.reload_after_deploy(&key, &sent.project_id, now, out);
            }
            DeployStep::Failed {
                during: Phase::Redeploy,
                failure: WriteFailure::Unknown,
                ..
            } => {
                self.deploys.remove(&key);
                self.message = Some("redeploy outcome unknown — check the deployments view".into());
            }
            DeployStep::Failed {
                during,
                failure: WriteFailure::Unknown,
                ..
            } => {
                // Kept in flight until Railway says what landed.
                self.message = Some(format!(
                    "outcome of {during} unknown: checking {} on Railway…",
                    sent.service_name
                ));
                out.push(Command::LoadVariables {
                    project_id: sent.project_id,
                    env_id: key.env_id,
                    service_id: key.service_id,
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
        let key = GroupKey::new(env_id, service_id);
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
        self.mark_applied(&key, |name, var| match &var.change {
            Change::Set { value } => current.get(name) == Some(value),
            // A sealed variable the map omits is absent before and after its
            // delete: absence proves nothing, so it stays unapplied for `D`.
            Change::Delete => !var.base_sealed && !current.contains_key(name),
        });
        out.push(self.save_pending());
        if let Some(sp) = self.pending.service(&key.env_id, &key.service_id) {
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
    fn mark_applied(&mut self, key: &GroupKey, matches: impl Fn(&str, &PendingVar) -> bool) {
        let Some(sp) = self
            .pending_mut()
            .envs
            .get_mut(&key.env_id)
            .and_then(|s| s.get_mut(&key.service_id))
        else {
            return;
        };
        for (name, var) in sp.vars.iter_mut().filter(|(_, v)| !v.applied) {
            if matches(name, var) {
                var.applied = true;
            }
        }
        if sp.vars.values().all(|v| v.applied) {
            sp.written_not_deployed = true;
        }
    }

    /// The redeploy succeeded: drop the applied entries. An entry edited while
    /// the deploy ran was not sent, so it stays pending.
    fn finish_deploy(&mut self, key: &GroupKey) {
        let applied: Vec<String> = self
            .pending
            .service(&key.env_id, &key.service_id)
            .map(|sp| {
                sp.vars
                    .iter()
                    .filter(|(_, v)| v.applied)
                    .map(|(n, _)| n.clone())
                    .collect()
            })
            .unwrap_or_default();
        for name in applied {
            self.pending_mut().undo(&key.env_id, &key.service_id, &name);
        }
        if let Some(sp) = self
            .pending_mut()
            .envs
            .get_mut(&key.env_id)
            .and_then(|s| s.get_mut(&key.service_id))
        {
            sp.written_not_deployed = false;
        }
    }

    /// Reloads Services and the service's variables when they show what was deployed.
    fn reload_after_deploy(
        &mut self,
        key: &GroupKey,
        project_id: &str,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        if self.project_id() != Some(project_id) || self.env_id() != Some(key.env_id.as_str()) {
            return;
        }
        if self.stack.iter().any(|v| matches!(v, View::Services)) {
            self.load_services(now, out);
        }
        if self.view_service_id(Kind::Variables) == Some(key.service_id.as_str()) {
            self.load_variables(key.service_id.clone(), now, out);
        }
    }
}
