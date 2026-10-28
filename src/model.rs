//! Domain model shared by the API layer and the UI.

use chrono::{DateTime, Utc};

/// A service's variables, name to (unrendered) value.
pub type Variables = std::collections::BTreeMap<String, String>;

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

    /// The API spelling; unknown values show as `UNKNOWN(<raw>)`.
    pub fn label(&self) -> String {
        match self {
            Self::Building => "BUILDING".into(),
            Self::Crashed => "CRASHED".into(),
            Self::Deploying => "DEPLOYING".into(),
            Self::Failed => "FAILED".into(),
            Self::Initializing => "INITIALIZING".into(),
            Self::NeedsApproval => "NEEDS_APPROVAL".into(),
            Self::Queued => "QUEUED".into(),
            Self::Removed => "REMOVED".into(),
            Self::Removing => "REMOVING".into(),
            Self::Skipped => "SKIPPED".into(),
            Self::Sleeping => "SLEEPING".into(),
            Self::Success => "SUCCESS".into(),
            Self::Waiting => "WAITING".into(),
            Self::Unknown(raw) => format!("UNKNOWN({raw})"),
        }
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
