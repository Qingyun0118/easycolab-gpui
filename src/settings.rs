use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub service_url: String,
    pub management_password: String,
    pub hf_token: String,
    pub gpu: String,
    pub idle_timeout_minutes: u32,
    pub output_dir: PathBuf,
    pub appearance: String,
    pub language: String,
    pub bridge_managed: bool,
    pub combined_telemetry_chart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            service_url: String::new(),
            management_password: String::new(),
            hf_token: String::new(),
            gpu: "G4".into(),
            idle_timeout_minutes: 30,
            output_dir: dirs::video_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(std::env::temp_dir)
                .join("EasyColab-GPUI"),
            appearance: "system".into(),
            language: "zh-CN".into(),
            bridge_managed: false,
            combined_telemetry_chart: false,
        }
    }
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("EASYCOLAB_GPUI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("easycolab-gpui")
        })
}

impl Settings {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_dir().join("settings.json"))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Could not read settings JSON"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("Could not read settings"),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_dir().join("settings.json"))
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("Settings path has no parent")?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let temporary = path.with_extension("json.tmp");
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        use std::io::Write;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    pub fn redact(&self, message: &str) -> String {
        let mut result = message.to_owned();
        for secret in [&self.management_password, &self.hf_token] {
            if !secret.is_empty() {
                result = result.replace(secret, "***");
            }
        }
        result
    }
}
