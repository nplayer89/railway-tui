//! Domain model shared by the API layer and the UI.

use std::fmt;

use chrono::{DateTime, Utc};

/// A service's variables, name to (unrendered) value.
pub type Variables = std::collections::BTreeMap<String, String>;

/// Why a write did not succeed: Railway refused it, or it was sent and Railway
/// may or may not have applied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFailure {
    Refused,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentStatus {
    Building,
    Crashed,
    Deploying,
    Failed,
    Initializing,
    NeedsApproval,
    Queued,
    Removed,
    Removing,
    Skipped,
    Sleeping,
    Success,
    Waiting,
    Unknown(String),
}

impl DeploymentStatus {
    /// Parse the API's SCREAMING_SNAKE spelling.
    pub fn from_api(s: &str) -> Self {
        match s {
            "BUILDING" => Self::Building,
            "CRASHED" => Self::Crashed,
            "DEPLOYING" => Self::Deploying,
            "FAILED" => Self::Failed,
            "INITIALIZING" => Self::Initializing,
            "NEEDS_APPROVAL" => Self::NeedsApproval,
            "QUEUED" => Self::Queued,
            "REMOVED" => Self::Removed,
            "REMOVING" => Self::Removing,
            "SKIPPED" => Self::Skipped,
            "SLEEPING" => Self::Sleeping,
            "SUCCESS" => Self::Success,
            "WAITING" => Self::Waiting,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Whether the deployment has stopped changing on its own: it succeeded,
    /// failed, crashed or was removed.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Success | Self::Failed | Self::Crashed | Self::Removed => true,
            Self::Building
            | Self::Deploying
            | Self::Initializing
            | Self::NeedsApproval
            | Self::Queued
            | Self::Removing
            | Self::Skipped
            | Self::Sleeping
            | Self::Waiting
            | Self::Unknown(_) => false,
        }
    }
}

/// The API spelling; unknown values show as `UNKNOWN(<raw>)`.
impl fmt::Display for DeploymentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Building => "BUILDING",
            Self::Crashed => "CRASHED",
            Self::Deploying => "DEPLOYING",
            Self::Failed => "FAILED",
            Self::Initializing => "INITIALIZING",
            Self::NeedsApproval => "NEEDS_APPROVAL",
            Self::Queued => "QUEUED",
            Self::Removed => "REMOVED",
            Self::Removing => "REMOVING",
            Self::Skipped => "SKIPPED",
            Self::Sleeping => "SLEEPING",
            Self::Success => "SUCCESS",
            Self::Waiting => "WAITING",
            Self::Unknown(raw) => return write!(f, "UNKNOWN({raw})"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceStatus {
    Crashed,
    Created,
    Exited,
    Initializing,
    Removed,
    Removing,
    Restarting,
    Running,
    Skipped,
    Stopped,
    Unknown(String),
}

impl InstanceStatus {
    /// Parse the API's SCREAMING_SNAKE spelling.
    pub fn from_api(s: &str) -> Self {
        match s {
            "CRASHED" => Self::Crashed,
            "CREATED" => Self::Created,
            "EXITED" => Self::Exited,
            "INITIALIZING" => Self::Initializing,
            "REMOVED" => Self::Removed,
            "REMOVING" => Self::Removing,
            "RESTARTING" => Self::Restarting,
            "RUNNING" => Self::Running,
            "SKIPPED" => Self::Skipped,
            "STOPPED" => Self::Stopped,
            other => Self::Unknown(other.to_string()),
        }
    }
}

/// The API spelling; unknown values show as `UNKNOWN(<raw>)`.
impl fmt::Display for InstanceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Crashed => "CRASHED",
            Self::Created => "CREATED",
            Self::Exited => "EXITED",
            Self::Initializing => "INITIALIZING",
            Self::Removed => "REMOVED",
            Self::Removing => "REMOVING",
            Self::Restarting => "RESTARTING",
            Self::Running => "RUNNING",
            Self::Skipped => "SKIPPED",
            Self::Stopped => "STOPPED",
            Self::Unknown(raw) => return write!(f, "UNKNOWN({raw})"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub id: String,
    pub status: InstanceStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentSummary {
    pub id: String,
    pub status: DeploymentStatus,
    pub created_at: DateTime<Utc>,
    pub commit_sha: Option<String>,
    pub commit_message: Option<String>,
    /// `owner/name` of the GitHub repo it was built from.
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub image: Option<String>,
    pub instances: Vec<Instance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInstance {
    pub service_id: String,
    pub service_name: String,
    pub num_replicas: u32,
    pub latest: Option<DeploymentSummary>,
    pub active: Vec<DeploymentSummary>,
}

/// The GitHub trigger that decides which branch a service deploys from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub id: String,
    pub branch: String,
    /// `owner/name`.
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub projects: Vec<ProjectRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub timestamp: DateTime<Utc>,
    pub message: String,
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Build,
    Runtime,
}
