use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Route {
    #[default]
    Dashboard,
    Deploy,
    Workflows,
    Outputs,
    Settings,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AccountStatus {
    pub logged_in: bool,
    pub email: String,
    pub paid_balance: Option<f64>,
    pub burn_rate_hourly: Option<f64>,
    pub active_assignments: Option<u32>,
    pub sessions: Vec<Session>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Session {
    pub name: String,
    pub endpoint: String,
    pub accelerator: String,
    pub variant: String,
    pub shape: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerStatus {
    pub gpu_name: String,
    pub cuda_available: bool,
    pub vram_total_gb: Option<f64>,
    pub vram_used_gb: Option<f64>,
    pub vram_free_gb: Option<f64>,
    pub ram_total_gb: Option<f64>,
    pub ram_used_gb: Option<f64>,
    pub ram_free_gb: Option<f64>,
    pub disk_total_gb: Option<f64>,
    pub disk_used_gb: Option<f64>,
    pub disk_free_gb: Option<f64>,
    pub shm_total_gb: Option<f64>,
    pub shm_used_gb: Option<f64>,
    pub shm_free_gb: Option<f64>,
    pub drive_total_gb: Option<f64>,
    pub drive_used_gb: Option<f64>,
    pub drive_free_gb: Option<f64>,
    pub cu_burn_rate_per_hour: Option<f64>,
    pub session_cu_used: Option<f64>,
    pub uptime_seconds: u64,
    pub idle_timeout_minutes: u32,
    pub idle_remaining_seconds: i64,
    pub console_lines: Vec<String>,
    pub comfyui: ComfyStatus,
    pub action_ok: Option<bool>,
    pub action_message: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ComfyStatus {
    pub is_running: bool,
    pub is_starting: bool,
    pub is_syncing_drive: bool,
    pub is_downloading_weight: bool,
    pub installing_pack_id: String,
    pub progress: f64,
    pub status_message: String,
    pub public_url: String,
    pub local_port: u16,
    pub queue_running: u32,
    pub queue_pending: u32,
    pub persistence_mode: String,
    pub persistence_label: String,
    pub drive_persisted: bool,
    pub persist_root: String,
    pub custom_nodes_count: u32,
    pub models_count: u32,
    pub workflows_count: u32,
    pub last_sync_ts: u64,
    pub recent_outputs: Vec<RemoteOutput>,
    pub preset_packs: Vec<WorkflowPack>,
}

impl ComfyStatus {
    pub fn busy(&self) -> bool {
        self.is_starting
            || self.is_syncing_drive
            || self.is_downloading_weight
            || !self.installing_pack_id.is_empty()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RemoteOutput {
    pub rel_path: String,
    pub filename: String,
    pub media_type: String,
    pub size_mb: f64,
    pub mtime: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkflowPack {
    pub id: String,
    pub title: String,
    pub badge: String,
    pub scenario: String,
    pub approx_size_gb: f64,
    pub workflows: Vec<String>,
    pub installed_models: u32,
    pub total_models: u32,
    pub workflows_installed: bool,
    pub ready: bool,
}

pub fn preset_packs() -> Vec<WorkflowPack> {
    [
        (
            "wan22_i2v_flf2v",
            "Wan2.2 14B",
            "图生视频 / 首尾帧 · Image to video",
        ),
        (
            "minimax_h3_i2v_flf2v",
            "MiniMax-H3 Turbo",
            "图生视频 / 首尾帧 · Image to video",
        ),
        (
            "ltx23_flf2v",
            "LTX-2.3 22B",
            "首尾帧运镜 · First/last frame",
        ),
        (
            "hunyuan15_i2v_1080p",
            "HunyuanVideo 1.5",
            "1080p 图生视频 · Image to video",
        ),
    ]
    .into_iter()
    .map(|(id, title, scenario)| WorkflowPack {
        id: id.into(),
        title: title.into(),
        scenario: scenario.into(),
        ..Default::default()
    })
    .collect()
}

#[derive(Clone, Debug)]
pub enum ComfyAction {
    Start {
        force_restart: bool,
    },
    Stop,
    FreeVram,
    SyncDrive,
    DownloadWeight {
        source: String,
        filename: String,
        subfolder: String,
    },
    InstallPack {
        pack_id: String,
    },
    SetIdle,
    Disconnect,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UploadTarget {
    #[default]
    Runtime,
    Drive,
}

impl UploadTarget {
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Drive => "drive",
        }
    }

    pub fn base_path(self) -> &'static str {
        match self {
            Self::Runtime => "/content",
            Self::Drive => "/content/drive/MyDrive",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Request {
    AuthStatus,
    AuthLogin,
    AuthUrl,
    AuthCode(String),
    Logout,
    Wake,
    Deploy,
    UploadProject {
        local_path: PathBuf,
        remote_dir: String,
        target: UploadTarget,
    },
    RuntimeMetrics,
    Keepalive,
    ReleaseAll,
    Status,
    Action(ComfyAction),
    Download(RemoteOutput),
}

impl Request {
    pub fn key(&self) -> &'static str {
        match self {
            Self::AuthStatus => "auth-status",
            Self::AuthLogin | Self::AuthUrl | Self::AuthCode(_) | Self::Logout => "auth",
            Self::Wake => "wake",
            Self::Deploy => "deploy",
            Self::UploadProject { .. } => "upload",
            Self::RuntimeMetrics => "runtime-metrics",
            Self::Keepalive => "keepalive",
            Self::ReleaseAll => "release",
            Self::Status => "status",
            Self::Download(_) => "download-output",
            Self::Action(action) => match action {
                ComfyAction::Start { .. } | ComfyAction::Stop => "comfy-lifecycle",
                ComfyAction::FreeVram => "free-vram",
                ComfyAction::SyncDrive => "sync-drive",
                ComfyAction::DownloadWeight { .. } | ComfyAction::InstallPack { .. } => "install",
                ComfyAction::SetIdle => "set-idle",
                ComfyAction::Disconnect => "release",
            },
        }
    }
}

#[derive(Clone, Debug)]
pub enum Response {
    Account(AccountStatus),
    AuthUrl(String),
    Woken(Session),
    Deployed(String),
    Uploaded { remote_path: String, files: u32 },
    RuntimeMetrics(Box<ServerStatus>),
    Status(Box<ServerStatus>),
    Downloaded(PathBuf),
    Done(String),
}
