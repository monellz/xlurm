use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Nvidia,
    Ascend,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Nvidia => "nvidia",
            Self::Ascend => "ascend",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub kind: Kind,
    pub id: u32,
    pub name: String,
    /// NVIDIA UUID or Ascend logical ID; never a physical Ascend card ID.
    pub visible: String,
    pub npu: Option<u32>,
    pub chip: Option<u32>,
}

impl Device {
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind, self.visible)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub name: String,
    pub count: usize,
    pub kind: Option<Kind>,
    pub time_limit: Option<u64>,
    /// Batch scripts are snapshotted at submission, not read when dequeued.
    pub script: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum State {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl State {
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub owner: Owner,
    pub spec: Submission,
    pub state: State,
    pub devices: Vec<Device>,
    pub submitted_at: u64,
    pub started_at: Option<u64>,
    pub result: Option<Outcome>,
}

/// Queue metadata deliberately excludes commands, scripts and environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: u64,
    pub owner: Owner,
    pub name: String,
    pub state: State,
    pub count: usize,
    pub devices: Vec<Device>,
    pub submitted_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
}

impl From<&Job> for JobSummary {
    fn from(job: &Job) -> Self {
        Self {
            id: job.id,
            owner: job.owner.clone(),
            name: job.spec.name.clone(),
            state: job.state,
            count: job.spec.count,
            devices: job.devices.clone(),
            submitted_at: job.submitted_at,
            started_at: job.started_at,
            finished_at: job.result.as_ref().map(|result| result.finished_at),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub state: State,
    pub exit_code: i32,
    pub finished_at: u64,
    pub error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub next_id: u64,
    pub jobs: Vec<Job>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Submit(Submission),
    Queue,
    Get(u64),
    Cancel(u64),
    Log { id: u64, offset: u64 },
    Info,
    Stop,
    Restart,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceView {
    pub device: Device,
    pub job: Option<u64>,
    /// idle, busy (outside xlurm), unknown (probe failed), or allocated.
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Job(Box<Job>),
    Queue(Vec<JobSummary>),
    Log { bytes: Vec<u8>, offset: u64 },
    Info(Vec<DeviceView>),
    Ok,
    Error(String),
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
