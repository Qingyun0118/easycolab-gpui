//! Blocking operations, called only from the application's background workers.
//! The Python snapshot owns Colab OAuth/session compatibility; Rust owns the UI
//! and authenticated service requests. The mock never contacts a real account.
use crate::{models::*, settings::Settings};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

#[derive(Clone, Debug, PartialEq)]
pub enum BridgeEvent {
    Log(String),
    AuthUrl(String),
    Progress { value: Option<f32>, label: String },
}

pub trait Backend: Send + Sync {
    fn run(
        &self,
        settings: &Settings,
        request: Request,
        log: &mut dyn FnMut(String),
    ) -> Result<Response>;

    fn run_with_events(
        &self,
        settings: &Settings,
        request: Request,
        cancel: &AtomicBool,
        on_event: &mut dyn FnMut(BridgeEvent),
    ) -> Result<Response> {
        let _ = cancel;
        self.run(settings, request, &mut |line| {
            on_event(BridgeEvent::Log(line))
        })
    }
}

pub fn create(mock: bool) -> Arc<dyn Backend> {
    if mock {
        Arc::new(MockBackend::default())
    } else {
        Arc::new(RealBackend::default())
    }
}

pub struct RealBackend {
    python: PathBuf,
    bridge: PathBuf,
    app: PathBuf,
    agent: ureq::Agent,
    // colab-cli caches credentials and sessions in a shared local store.
    bridge_lock: Mutex<()>,
}

impl Default for RealBackend {
    fn default() -> Self {
        Self::from_root(&crate::platform::runtime_root())
    }
}

impl RealBackend {
    pub fn from_root(root: &Path) -> Self {
        let python = crate::platform::python(root);
        Self::with_bridge(
            python,
            root.join("python/colab_bridge.py"),
            root.join("python/app.py"),
        )
    }

    /// Explicit paths also let tests use a tiny local bridge, without Google.
    pub fn with_bridge(python: PathBuf, bridge: PathBuf, app: PathBuf) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(180)))
            .http_status_as_error(false)
            // A credential-bearing request must never follow a redirect.
            .max_redirects(0)
            .user_agent(concat!("easycolab-gpui/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            python,
            bridge,
            app,
            agent: ureq::Agent::new_with_config(config),
            bridge_lock: Mutex::new(()),
        }
    }

    fn bridge(
        &self,
        settings: &Settings,
        args: &[String],
        cancel: &AtomicBool,
        on_event: &mut dyn FnMut(BridgeEvent),
    ) -> Result<Vec<String>> {
        let keepalive = args.first().is_some_and(|a| a == "keepalive");
        let metrics = args.first().is_some_and(|a| a == "metrics");
        let _guard = if keepalive || metrics {
            match self.bridge_lock.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Ok(vec![
                        json!({"ok":true,"deferred":true,"message":"Another bridge operation is running; poll deferred"})
                            .to_string(),
                    ]);
                }
                Err(_) => bail!("Colab bridge lock is unavailable"),
            }
        } else {
            self.bridge_lock
                .lock()
                .map_err(|_| anyhow!("Colab bridge lock is unavailable"))?
        };
        ensure!(!cancel.load(Ordering::Relaxed), "Operation cancelled");
        ensure!(
            self.python.is_file(),
            "Project Python environment is missing. Run {} first ({})",
            crate::platform::python_setup_command(),
            self.python.display()
        );
        ensure!(self.bridge.is_file(), "Bundled Colab bridge is missing");
        let waking = args.first().is_some_and(|a| a == "wake");
        let deploying = args.first().is_some_and(|a| a == "deploy");
        let uploading = args.first().is_some_and(|a| a == "upload");
        let logging_in = args.first().is_some_and(|a| a == "auth_login");
        if waking {
            ensure!(
                ["G4", "H100", "A100", "L4", "T4"].contains(&settings.gpu.as_str()),
                "Unsupported GPU selection"
            );
        }
        if deploying {
            ensure!(self.app.is_file(), "Bundled cloud app.py is missing");
            ensure!(
                settings.management_password.len() >= 24
                    && settings.management_password.len() <= 256
                    && settings
                        .management_password
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "Management password must contain 24–256 URL-safe ASCII characters"
            );
        }
        let mut command = Command::new(&self.python);
        command
            .arg("-u")
            .arg(&self.bridge)
            .args(args)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if deploying {
            command
                .env("EASYCOLAB_ADMIN_PASSWORD", &settings.management_password)
                .env(
                    "EASYCOLAB_IDLE_TIMEOUT_MINUTES",
                    settings.idle_timeout_minutes.to_string(),
                );
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command
            .spawn()
            .context("Could not start the project Colab bridge")?;
        let (tx, rx) = mpsc::channel();
        for (reader, is_stdout) in [
            (
                child
                    .stdout
                    .take()
                    .map(|r| Box::new(r) as Box<dyn std::io::Read + Send>),
                true,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|r| Box::new(r) as Box<dyn std::io::Read + Send>),
                false,
            ),
        ] {
            let tx = tx.clone();
            if let Some(reader) = reader {
                std::thread::spawn(move || {
                    for line in BufReader::new(reader).lines() {
                        match line {
                            Ok(line) => {
                                if tx.send((is_stdout, line)).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        }
        drop(tx);
        let timeout = if waking || deploying {
            Duration::from_secs(1200)
        } else if uploading {
            Duration::from_secs(600)
        } else if logging_in {
            Duration::from_secs(320)
        } else if metrics {
            Duration::from_secs(40)
        } else {
            Duration::from_secs(180)
        };
        let started = Instant::now();
        let mut output = Vec::new();
        let mut stderr = Vec::new();
        let auth_code = (args.first().is_some_and(|a| a == "auth_code"))
            .then(|| args.get(1))
            .flatten();
        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Operation cancelled");
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((is_stdout, line)) => {
                    if is_stdout
                        && logging_in
                        && let Some(url) = line.strip_prefix("AUTH_URL:")
                    {
                        let url = url.trim();
                        if let Ok(parsed) = Url::parse(url)
                            && parsed.scheme() == "https"
                        {
                            on_event(BridgeEvent::AuthUrl(url.to_owned()));
                        }
                        continue;
                    }
                    let safe = redact(settings, &line, auth_code.map(String::as_str));
                    if is_stdout {
                        if waking || deploying || uploading {
                            if let Some(payload) = safe.strip_prefix("WAKE_PROGRESS:") {
                                if let Some((raw_value, label)) = payload.split_once('|') {
                                    let value = if raw_value.trim() == "-" {
                                        None
                                    } else {
                                        raw_value
                                            .trim()
                                            .parse::<f32>()
                                            .ok()
                                            .filter(|v| v.is_finite())
                                            .map(|v| v.clamp(0.0, 1.0))
                                    };
                                    on_event(BridgeEvent::Progress {
                                        value,
                                        label: label.trim().to_owned(),
                                    });
                                }
                                continue;
                            }
                            if let Some(message) = safe.strip_prefix("WAKE_LOG:") {
                                on_event(BridgeEvent::Log(message.trim().into()));
                            } else {
                                let trimmed = safe.trim();
                                if !trimmed.is_empty()
                                    && !safe.starts_with("WAKE_URL:")
                                    && !safe.starts_with("WAKE_SESSION:")
                                    && !(trimmed.starts_with('{') && trimmed.ends_with('}'))
                                {
                                    on_event(BridgeEvent::Log(safe));
                                }
                            }
                        } else {
                            let trimmed = safe.trim();
                            if !trimmed.is_empty()
                                && !(trimmed.starts_with('{') && trimmed.ends_with('}'))
                            {
                                on_event(BridgeEvent::Log(safe));
                            }
                        }
                        output.push(line);
                    } else {
                        if !safe.trim().is_empty() {
                            on_event(BridgeEvent::Log(safe.clone()));
                        }
                        stderr.push(safe);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if started.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Colab bridge timed out; inspect the activity log before retrying");
            }
        }
        // Pipes normally close at exit, but a bridge may close them first.
        let status = loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Operation cancelled");
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Colab bridge timed out");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        ensure!(
            status.success(),
            "Colab bridge exited with {}: {}",
            status,
            stderr.join("\n")
        );
        Ok(output)
    }

    fn bridge_json(
        &self,
        settings: &Settings,
        args: &[String],
        cancel: &AtomicBool,
        on_event: &mut dyn FnMut(BridgeEvent),
    ) -> Result<Value> {
        let output = self.bridge(settings, args, cancel, on_event)?;
        let mut value = output
            .iter()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
            .context("Colab bridge returned no JSON result")?;
        check_business_result(&value)?;
        redact_value(settings, &mut value);
        Ok(value)
    }

    fn service_url(&self, settings: &Settings, suffix: &str) -> Result<Url> {
        let mut url =
            Url::parse(settings.service_url.trim()).context("Enter a valid service URL")?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(name)) => name == "localhost",
            None => false,
        };
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "Service URL must use HTTPS (HTTP is allowed only for localhost)"
        );
        ensure!(
            url.host_str().is_some() && url.username().is_empty() && url.password().is_none(),
            "Service URL must not contain embedded credentials"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "Service URL must not contain a query or fragment"
        );
        ensure!(
            !settings.management_password.is_empty(),
            "Enter the management password"
        );
        let prefix = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{prefix}{suffix}"));
        Ok(url)
    }

    fn authorization(settings: &Settings) -> String {
        format!(
            "Basic {}",
            STANDARD.encode(format!("easycolab:{}", settings.management_password))
        )
    }

    fn http(&self, settings: &Settings, path: &str, body: Option<Value>) -> Result<Value> {
        let url = self.service_url(settings, path)?;
        let authorization = Self::authorization(settings);
        let mut response = if let Some(body) = body {
            self.agent
                .post(url.as_str())
                .header("Authorization", &authorization)
                .header("Content-Type", "application/json")
                .send(serde_json::to_vec(&body)?)?
        } else {
            self.agent
                .get(url.as_str())
                .header("Authorization", &authorization)
                .call()?
        };
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_to_vec()?;
        ensure!(
            (200..300).contains(&status),
            "Service HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        let mut value: Value =
            serde_json::from_slice(&bytes).context("Service returned invalid JSON")?;
        check_business_result(&value)?;
        redact_value(settings, &mut value);
        Ok(value)
    }

    fn download(&self, settings: &Settings, output: &RemoteOutput) -> Result<PathBuf> {
        let components = relative_components(&output.rel_path)?;
        validate_filename(&output.filename)?;
        let mut url = self.service_url(settings, "/minimax_api/comfyui/output_media")?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("Invalid service URL"))?;
            for component in components {
                path.push(component);
            }
        }
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Authorization", Self::authorization(settings))
            .call()?;
        let status = response.status().as_u16();
        ensure!(
            (200..300).contains(&status),
            "Output download failed: HTTP {status}"
        );
        save_output(
            settings,
            &output.filename,
            &mut response.body_mut().as_reader(),
        )
    }

    fn run_inner(
        &self,
        settings: &Settings,
        request: Request,
        cancel: &AtomicBool,
        on_event: &mut dyn FnMut(BridgeEvent),
    ) -> Result<Response> {
        match request {
            Request::AuthStatus | Request::AuthLogin | Request::AuthCode(_) | Request::Logout => {
                let args = match request {
                    Request::AuthLogin => vec!["auth_login".into()],
                    Request::AuthCode(code) => vec!["auth_code".into(), code],
                    Request::Logout => vec!["auth_logout".into()],
                    _ => vec!["auth_status".into()],
                };
                let value = self.bridge_json(settings, &args, cancel, on_event)?;
                Ok(Response::Account(serde_json::from_value(value)?))
            }
            Request::AuthUrl => {
                let value = self.bridge_json(settings, &["auth_url".into()], cancel, on_event)?;
                Ok(Response::AuthUrl(
                    value["auth_url"]
                        .as_str()
                        .context("Bridge returned no authorization URL")?
                        .into(),
                ))
            }
            Request::Wake => {
                let output = self.bridge(
                    settings,
                    &["wake".into(), "--gpu".into(), settings.gpu.clone()],
                    cancel,
                    on_event,
                )?;
                if let Some(error) = output
                    .iter()
                    .rev()
                    .find_map(|line| line.strip_prefix("WAKE_ERROR:"))
                {
                    bail!("{}", error.trim());
                }
                let raw_session = output
                    .iter()
                    .rev()
                    .find_map(|line| line.strip_prefix("WAKE_SESSION:"))
                    .context(
                        "Wake finished without runtime session info; inspect the activity log",
                    )?
                    .trim();
                let session: Session = serde_json::from_str(raw_session)
                    .context("Bridge returned invalid runtime session JSON")?;
                Ok(Response::Woken(session))
            }
            Request::Deploy => {
                let output = self.bridge(
                    settings,
                    &[
                        "deploy".into(),
                        "--app-py".into(),
                        self.app.to_string_lossy().into(),
                    ],
                    cancel,
                    on_event,
                )?;
                if let Some(error) = output
                    .iter()
                    .rev()
                    .find_map(|line| line.strip_prefix("WAKE_ERROR:"))
                {
                    bail!("{}", error.trim());
                }
                let url = output
                    .iter()
                    .rev()
                    .find_map(|line| line.strip_prefix("WAKE_URL:"))
                    .context("Deployment finished without a service URL; inspect the activity log")?
                    .trim();
                let parsed = Url::parse(url).context("Bridge returned an invalid service URL")?;
                ensure!(
                    parsed.scheme() == "https"
                        && parsed.host_str().is_some()
                        && parsed.username().is_empty()
                        && parsed.password().is_none(),
                    "Bridge returned an unsafe service URL"
                );
                Ok(Response::Deployed(url.into()))
            }
            Request::UploadProject {
                local_path,
                remote_dir,
                target,
            } => {
                ensure!(
                    !local_path.as_os_str().is_empty() && local_path.exists(),
                    "Select an existing local file or folder to upload"
                );
                let rel = normalize_upload_subdir(&remote_dir, target);
                let _ = relative_components(rel)?;
                let value = self.bridge_json(
                    settings,
                    &[
                        "upload".into(),
                        "--local".into(),
                        local_path.to_string_lossy().into(),
                        "--remote".into(),
                        rel.into(),
                        "--target".into(),
                        target.as_arg().into(),
                    ],
                    cancel,
                    on_event,
                )?;
                let remote_path = value["remote_path"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{}/{rel}", target.base_path()));
                let files = value["files"].as_u64().unwrap_or(0) as u32;
                Ok(Response::Uploaded { remote_path, files })
            }
            Request::RuntimeMetrics => {
                let value = self.bridge_json(settings, &["metrics".into()], cancel, on_event)?;
                if value["deferred"].as_bool() == Some(true) {
                    return Ok(Response::Done("Runtime telemetry deferred".into()));
                }
                Ok(Response::RuntimeMetrics(Box::new(serde_json::from_value(
                    value,
                )?)))
            }
            Request::Keepalive => {
                let value = self.bridge_json(settings, &["keepalive".into()], cancel, on_event)?;
                ensure!(
                    value["logged_in"].as_bool() != Some(false),
                    "Google account is signed out; bind the account again to refresh Colab activity"
                );
                if let Some(warning) = value["warning"].as_str() {
                    on_event(BridgeEvent::Log(warning.into()));
                }
                Ok(Response::Done(
                    value["message"]
                        .as_str()
                        .unwrap_or("Colab heartbeat refreshed")
                        .into(),
                ))
            }
            Request::ReleaseAll => {
                if !settings.service_url.trim().is_empty() {
                    // Releasing is explicit: attempt a final sync, report any
                    // failure, then still honor the request to stop billing.
                    let _ = self
                        .http(settings, "/minimax_api/comfyui/sync_drive", Some(json!({})))
                        .map_err(|error| {
                            on_event(BridgeEvent::Log(
                                settings.redact(&format!("Drive sync before release: {error:#}")),
                            ))
                        });
                }
                let value = self.bridge_json(settings, &["stop_all".into()], cancel, on_event)?;
                if let Some(remaining) = value["active_assignments"].as_u64() {
                    ensure!(
                        remaining == 0,
                        "Release is incomplete: {remaining} Colab runtime(s) remain active. Refresh account status before retrying."
                    );
                }
                Ok(Response::Done(
                    value["message"]
                        .as_str()
                        .unwrap_or("All Colab runtimes released")
                        .into(),
                ))
            }
            Request::Status => Ok(Response::Status(Box::new(serde_json::from_value(
                self.http(settings, "/minimax_api/status", None)?,
            )?))),
            Request::Action(action) => {
                let (path, body) = match action {
                    ComfyAction::Start { force_restart } => (
                        "/minimax_api/comfyui/start",
                        json!({"unload_existing":false,"force_restart":force_restart}),
                    ),
                    ComfyAction::Stop => ("/minimax_api/comfyui/stop", json!({})),
                    ComfyAction::FreeVram => ("/minimax_api/comfyui/free_vram", json!({})),
                    ComfyAction::SyncDrive => ("/minimax_api/comfyui/sync_drive", json!({})),
                    ComfyAction::DownloadWeight {
                        source,
                        filename,
                        subfolder,
                    } => {
                        ensure!(
                            !source.trim().is_empty(),
                            "Enter a model URL or Hugging Face repository"
                        );
                        (
                            "/minimax_api/comfyui/download_weight",
                            json!({"url_or_repo":source.trim(),"filename":filename.trim(),"subfolder":subfolder,"hf_token":settings.hf_token}),
                        )
                    }
                    ComfyAction::InstallPack { pack_id } => (
                        "/minimax_api/comfyui/install_pack",
                        json!({"pack_id":pack_id,"auto_start":true,"hf_token":settings.hf_token}),
                    ),
                    ComfyAction::SetIdle => (
                        "/minimax_api/config_idle",
                        json!({"idle_timeout_minutes":settings.idle_timeout_minutes}),
                    ),
                    ComfyAction::Disconnect => {
                        let result =
                            self.http(settings, "/minimax_api/disconnect", Some(json!({})))?;
                        let message = result["message"]
                            .as_str()
                            .filter(|message| !message.trim().is_empty())
                            .unwrap_or("已请求释放运行时 · Runtime release requested");
                        // This legacy endpoint returns ok:true even when its
                        // release operation reports failure in the message.
                        let lower = message.to_ascii_lowercase();
                        ensure!(
                            !message.contains("释放运行时失败")
                                && ![
                                    "failed to release",
                                    "release failed",
                                    "releasing runtime failed",
                                    "failed to disconnect",
                                    "disconnect failed"
                                ]
                                .iter()
                                .any(|failure| lower.contains(failure)),
                            "{message}"
                        );
                        return Ok(Response::Done(message.into()));
                    }
                };
                let status = serde_json::from_value(self.http(settings, path, Some(body))?)?;
                Ok(Response::Status(Box::new(status)))
            }
            Request::Download(output) => {
                Ok(Response::Downloaded(self.download(settings, &output)?))
            }
        }
    }
}

impl Backend for RealBackend {
    fn run(
        &self,
        settings: &Settings,
        request: Request,
        log: &mut dyn FnMut(String),
    ) -> Result<Response> {
        let cancel = AtomicBool::new(false);
        self.run_with_events(settings, request, &cancel, &mut |event| {
            if let BridgeEvent::Log(line) = event {
                log(line);
            }
        })
    }

    fn run_with_events(
        &self,
        settings: &Settings,
        request: Request,
        cancel: &AtomicBool,
        on_event: &mut dyn FnMut(BridgeEvent),
    ) -> Result<Response> {
        let code = if let Request::AuthCode(code) = &request {
            Some(code.clone())
        } else {
            None
        };
        self.run_inner(settings, request, cancel, &mut |event| match event {
            BridgeEvent::Log(line) => {
                on_event(BridgeEvent::Log(redact(settings, &line, code.as_deref())))
            }
            BridgeEvent::AuthUrl(url) => on_event(BridgeEvent::AuthUrl(url)),
            BridgeEvent::Progress { value, label } => on_event(BridgeEvent::Progress {
                value,
                label: redact(settings, &label, code.as_deref()),
            }),
        })
        .map_err(|error| anyhow!(redact(settings, &format!("{error:#}"), code.as_deref())))
    }
}

fn check_business_result(value: &Value) -> Result<()> {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        bail!(
            "{}",
            value["error"]
                .as_str()
                .or_else(|| value["message"].as_str())
                .unwrap_or("Operation failed")
        );
    }
    if value.get("action_ok").and_then(Value::as_bool) == Some(false) {
        bail!(
            "{}",
            value["action_message"]
                .as_str()
                .unwrap_or("Requested action failed")
        );
    }
    Ok(())
}

fn redact(settings: &Settings, message: &str, auth_code: Option<&str>) -> String {
    let mut message = settings.redact(message);
    if let Some(code) = auth_code.filter(|s| !s.is_empty()) {
        message = message.replace(code, "***");
    }
    let encoded = STANDARD.encode(format!("easycolab:{}", settings.management_password));
    if !settings.management_password.is_empty() {
        message = message.replace(&encoded, "***");
    }
    message
}

fn redact_value(settings: &Settings, value: &mut Value) {
    match value {
        Value::String(text) => *text = redact(settings, text, None),
        Value::Array(values) => {
            for value in values {
                redact_value(settings, value);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_value(settings, value);
            }
        }
        _ => {}
    }
}

fn relative_components(path: &str) -> Result<Vec<&str>> {
    ensure!(
        !path.is_empty() && !path.contains(['\\', ':']) && !path.chars().any(char::is_control),
        "Output path must be relative and inside the output directory"
    );
    let parts: Vec<_> = path.split('/').collect();
    ensure!(
        parts
            .iter()
            .all(|part| !part.is_empty() && *part != "." && *part != ".."),
        "Output path must not contain absolute paths or traversal"
    );
    Ok(parts)
}

fn validate_filename(filename: &str) -> Result<()> {
    ensure!(
        !filename.is_empty()
            && filename != "."
            && filename != ".."
            && !filename.contains(['/', '\\', ':'])
            && !filename.chars().any(char::is_control),
        "Invalid output filename"
    );
    Ok(())
}

static NEXT_DOWNLOAD: AtomicU64 = AtomicU64::new(0);
static OUTPUT_RENAME_LOCK: Mutex<()> = Mutex::new(());

fn save_output(
    settings: &Settings,
    filename: &str,
    reader: &mut dyn std::io::Read,
) -> Result<PathBuf> {
    validate_filename(filename)?;
    fs::create_dir_all(&settings.output_dir)
        .context("Could not create the local output directory")?;
    let directory = settings.output_dir.canonicalize()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = directory.join(format!(
        ".easycolab-{nonce}-{}.part",
        NEXT_DOWNLOAD.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let bytes = std::io::copy(reader, &mut file)?;
        ensure!(bytes > 0, "Output download was empty");
        file.flush()?;
        file.sync_all()?;
        drop(file);
        // Repeated runs and different cloud subdirectories may have the same
        // filename. Serialize the local naming decision and keep both outputs.
        let _guard = OUTPUT_RENAME_LOCK
            .lock()
            .map_err(|_| anyhow!("Output naming lock unavailable"))?;
        let mut target = directory.join(format!("comfy_{filename}"));
        let path = Path::new(filename);
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(filename);
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| format!(".{s}"))
            .unwrap_or_default();
        let mut index = 2u64;
        while target.try_exists()? || fs::symlink_metadata(&target).is_ok() {
            target = directory.join(format!("comfy_{stem} ({index}){extension}"));
            index += 1;
        }
        fs::rename(&temporary, &target).context("Could not save the completed output")?;
        Ok(target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

// Shared with the UI preview; generated locally and bundled for offline demos.
const MOCK_PNG: &[u8] = include_bytes!("../assets/demo-preview.png");

pub struct MockBackend {
    state: Mutex<MockState>,
}
struct MockState {
    logged_in: bool,
    woken: bool,
    deployed: bool,
    gpu: String,
    status: ServerStatus,
    created: Instant,
    pending: Option<(Instant, MockJob)>,
}
enum MockJob {
    Start,
    Install(String),
    DownloadWeight,
    Sync,
}

impl Default for MockBackend {
    fn default() -> Self {
        let mut packs = preset_packs();
        for (index, pack) in packs.iter_mut().enumerate() {
            pack.approx_size_gb = [38.5, 44.0, 29.0, 33.0][index];
            pack.total_models = [5, 4, 3, 4][index];
            pack.badge = "ComfyUI".into();
            pack.workflows = vec![format!("{}_workflow.json", pack.id)];
        }
        let status = ServerStatus {
            gpu_name: "NVIDIA RTX PRO 6000 Blackwell (Demo)".into(),
            cuda_available: true,
            vram_total_gb: Some(96.0),
            vram_used_gb: Some(1.6),
            vram_free_gb: Some(94.4),
            ram_total_gb: Some(176.0),
            ram_used_gb: Some(12.4),
            ram_free_gb: Some(163.6),
            disk_total_gb: Some(225.0),
            disk_used_gb: Some(28.0),
            disk_free_gb: Some(197.0),
            shm_total_gb: Some(64.0),
            shm_used_gb: Some(2.4),
            shm_free_gb: Some(61.6),
            drive_total_gb: Some(2048.0),
            drive_used_gb: Some(126.0),
            drive_free_gb: Some(1922.0),
            cu_burn_rate_per_hour: Some(16.2),
            idle_timeout_minutes: 30,
            idle_remaining_seconds: 1800,
            comfyui: ComfyStatus {
                local_port: 28188,
                persistence_mode: "drive_symlink".into(),
                persistence_label: "Google Drive 实时软链直连 · Demo".into(),
                drive_persisted: true,
                persist_root: "/content/drive/MyDrive/EasyColab_ComfyUI".into(),
                preset_packs: packs,
                status_message: "演示环境已准备好 · Demo ready".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        Self {
            state: Mutex::new(MockState {
                logged_in: true,
                woken: false,
                deployed: false,
                gpu: "G4".into(),
                status,
                created: Instant::now(),
                pending: None,
            }),
        }
    }
}

impl MockState {
    fn tick(&mut self) {
        self.status.uptime_seconds = self.created.elapsed().as_secs();
        self.status.session_cu_used = Some(self.status.uptime_seconds as f64 / 3600.0 * 16.2);
        let phase = self.created.elapsed().as_secs_f64().sin();
        let ram_used = 16.0 + phase * 2.0;
        let vram_used = if self.status.comfyui.is_running {
            28.0 + phase * 4.0
        } else {
            1.6
        };
        self.status.ram_used_gb = Some(ram_used);
        self.status.ram_free_gb = self.status.ram_total_gb.map(|t| (t - ram_used).max(0.0));
        self.status.vram_used_gb = Some(vram_used);
        self.status.vram_free_gb = self.status.vram_total_gb.map(|t| (t - vram_used).max(0.0));
        let Some((started, job)) = self.pending.take() else {
            return;
        };
        let progress = (started.elapsed().as_secs_f64() / 1.8).min(1.0);
        self.status.comfyui.progress = progress;
        if progress < 1.0 {
            self.pending = Some((started, job));
            return;
        }
        let c = &mut self.status.comfyui;
        match job {
            MockJob::Start => {
                c.is_starting = false;
                c.is_running = true;
                c.public_url = "https://demo-comfyui.invalid".into();
                c.custom_nodes_count = 3;
                c.status_message = "ComfyUI 已就绪 · Demo canvas ready".into();
            }
            MockJob::Install(id) => {
                if let Some(pack) = c.preset_packs.iter_mut().find(|p| p.id == id) {
                    if !pack.ready {
                        c.models_count += pack.total_models;
                        c.workflows_count += pack.workflows.len() as u32;
                    }
                    pack.installed_models = pack.total_models;
                    pack.workflows_installed = true;
                    pack.ready = true;
                }
                c.is_downloading_weight = false;
                c.installing_pack_id.clear();
                c.is_running = true;
                c.public_url = "https://demo-comfyui.invalid".into();
                c.custom_nodes_count = 5;
                c.status_message = "工作流包已安装 · Workflow pack ready".into();
            }
            MockJob::DownloadWeight => {
                c.is_downloading_weight = false;
                c.models_count += 1;
                c.status_message = "权重已保存至 Drive · Model saved to Drive".into();
            }
            MockJob::Sync => {
                c.is_syncing_drive = false;
                c.last_sync_ts = unix_now();
                c.status_message = "云盘同步完成 · Drive sync complete".into();
            }
        }
        if c.is_running && c.recent_outputs.is_empty() {
            c.recent_outputs.push(RemoteOutput {
                rel_path: "demo/preview.png".into(),
                filename: "demo-preview.png".into(),
                media_type: "image".into(),
                size_mb: 0.01,
                mtime: unix_now() as f64,
            });
        }
        self.status.console_lines.push(c.status_message.clone());
    }
    fn account(&self) -> AccountStatus {
        AccountStatus {
            logged_in: self.logged_in,
            email: if self.logged_in {
                "demo@easycolab.local".into()
            } else {
                String::new()
            },
            paid_balance: self.logged_in.then_some(423.8),
            burn_rate_hourly: Some(if self.woken || self.deployed {
                16.2
            } else {
                0.0
            }),
            active_assignments: Some(u32::from(self.woken || self.deployed)),
            sessions: if self.woken || self.deployed {
                vec![Session {
                    name: "easycolab-demo".into(),
                    accelerator: self.gpu.clone(),
                    endpoint: "demo-runtime".into(),
                    variant: "GPU".into(),
                    shape: "HIGH_RAM".into(),
                }]
            } else {
                vec![]
            },
        }
    }
}

impl Backend for MockBackend {
    fn run(
        &self,
        settings: &Settings,
        request: Request,
        log: &mut dyn FnMut(String),
    ) -> Result<Response> {
        let cancel = AtomicBool::new(false);
        self.run_with_events(settings, request, &cancel, &mut |event| {
            if let BridgeEvent::Log(line) = event {
                log(line);
            }
        })
    }

    fn run_with_events(
        &self,
        settings: &Settings,
        request: Request,
        cancel: &AtomicBool,
        on_event: &mut dyn FnMut(BridgeEvent),
    ) -> Result<Response> {
        if matches!(request, Request::AuthLogin) {
            on_event(BridgeEvent::AuthUrl(
                "https://demo-oauth.invalid/authorize".into(),
            ));
            for _ in 0..16 {
                if cancel.load(Ordering::Relaxed) {
                    bail!("已取消演示登录 · Demo sign-in cancelled");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("Demo state unavailable"))?;
            state.tick();
            state.logged_in = true;
            return Ok(Response::Account(state.account()));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("Demo state unavailable"))?;
        state.tick();
        match request {
            Request::AuthStatus => Ok(Response::Account(state.account())),
            Request::AuthLogin => unreachable!(),
            Request::AuthUrl => Ok(Response::AuthUrl(
                "https://demo-oauth.invalid/authorize".into(),
            )),
            Request::AuthCode(_) => {
                state.logged_in = true;
                Ok(Response::Account(state.account()))
            }
            Request::Logout => {
                state.logged_in = false;
                Ok(Response::Account(state.account()))
            }
            Request::Wake => {
                ensure!(
                    state.logged_in,
                    "请先绑定演示账号 · Sign in to the demo first"
                );
                let target_gpu = settings.gpu.clone();
                drop(state);
                for (pct, label) in [
                    (0.15, "检查演示账号现有 GPU 实例..."),
                    (0.55, "向演示环境申请目标 GPU 实例..."),
                    (0.88, "等待容器与 Jupyter 内核就绪..."),
                ] {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("Operation cancelled");
                    }
                    on_event(BridgeEvent::Progress {
                        value: Some(pct),
                        label: label.into(),
                    });
                    on_event(BridgeEvent::Log(format!("演示：{label}")));
                    std::thread::sleep(Duration::from_millis(45));
                }
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("Demo state unavailable"))?;
                state.woken = true;
                state.gpu = target_gpu;
                on_event(BridgeEvent::Progress {
                    value: Some(1.0),
                    label: format!("GPU 实例 [{}] 已唤醒就绪", state.gpu),
                });
                on_event(BridgeEvent::Log(format!(
                    "演示：已唤醒 {} GPU 实例（未自动部署服务；可前往部署中心选择 ComfyUI 或上传项目）",
                    state.gpu
                )));
                Ok(Response::Woken(Session {
                    name: "easycolab-demo".into(),
                    accelerator: state.gpu.clone(),
                    endpoint: "demo-runtime".into(),
                    variant: "GPU".into(),
                    shape: "HIGH_RAM".into(),
                }))
            }
            Request::Deploy => {
                ensure!(
                    state.logged_in,
                    "请先绑定演示账号 · Sign in to the demo first"
                );
                ensure!(
                    state.woken || state.deployed,
                    "请先唤醒演示 GPU 实例 · Wake a demo GPU runtime first"
                );
                drop(state);
                for (pct, label) in [
                    (0.18, "上传 ComfyUI 服务端脚本 (app.py)..."),
                    (0.45, "配置 Google Drive 挂载与模型引擎..."),
                    (0.75, "启动 ComfyUI 服务端与安全隧道..."),
                    (0.95, "验证公网隧道连通性..."),
                ] {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("Operation cancelled");
                    }
                    on_event(BridgeEvent::Progress {
                        value: Some(pct),
                        label: label.into(),
                    });
                    on_event(BridgeEvent::Log(format!("演示：{label}")));
                    std::thread::sleep(Duration::from_millis(55));
                }
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("Demo state unavailable"))?;
                state.woken = true;
                state.deployed = true;
                state.status.idle_timeout_minutes = settings.idle_timeout_minutes;
                on_event(BridgeEvent::Progress {
                    value: Some(1.0),
                    label: "ComfyUI 服务端与公网隧道已就绪".into(),
                });
                on_event(BridgeEvent::Log(
                    "演示：在已唤醒实例上部署 ComfyUI 服务端 → 准备 Drive（不连接真实 Colab）"
                        .into(),
                ));
                Ok(Response::Deployed("https://demo-easycolab.invalid".into()))
            }
            Request::UploadProject {
                local_path,
                remote_dir,
                target,
            } => {
                ensure!(
                    state.woken || state.deployed,
                    "请先唤醒演示 GPU 实例 · Wake a demo GPU runtime first"
                );
                ensure!(
                    !local_path.as_os_str().is_empty(),
                    "请选择本地文件或项目目录 · Choose a local file or folder"
                );
                if target == UploadTarget::Drive {
                    ensure!(
                        state.status.drive_total_gb.is_some(),
                        "演示 Google Drive 尚未挂载 · Demo Google Drive is not mounted"
                    );
                }
                let rel = normalize_upload_subdir(&remote_dir, target);
                let remote_path = format!("{}/{rel}", target.base_path());
                drop(state);
                let mut steps: Vec<(f32, String)> = Vec::new();
                if target == UploadTarget::Drive {
                    steps.push((0.08, "校验 Google Drive 真实挂载状态...".into()));
                }
                steps.push((0.25, "打包本地项目文件...".into()));
                steps.push((0.65, format!("正在上传至 {remote_path}...")));
                steps.push((0.90, format!("云端解压至 {remote_path}...")));
                for (pct, label) in steps {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("Operation cancelled");
                    }
                    on_event(BridgeEvent::Progress {
                        value: Some(pct),
                        label: label.clone(),
                    });
                    on_event(BridgeEvent::Log(format!("演示：{label}")));
                    std::thread::sleep(Duration::from_millis(45));
                }
                on_event(BridgeEvent::Progress {
                    value: Some(1.0),
                    label: format!("已上传 1 个文件至 {remote_path}"),
                });
                on_event(BridgeEvent::Log(format!(
                    "演示：已上传 {} 至 {remote_path}",
                    local_path.display()
                )));
                Ok(Response::Uploaded {
                    remote_path,
                    files: 1,
                })
            }
            Request::RuntimeMetrics => {
                ensure!(
                    state.woken || state.deployed,
                    "请先唤醒演示 GPU 实例 · Wake a demo GPU runtime first"
                );
                Ok(Response::RuntimeMetrics(Box::new(state.status.clone())))
            }
            Request::Keepalive => Ok(Response::Done("演示保活已刷新 · Demo heartbeat".into())),
            Request::ReleaseAll => {
                state.woken = false;
                state.deployed = false;
                state.pending = None;
                state.status.comfyui.is_running = false;
                state.status.comfyui.is_starting = false;
                state.status.comfyui.is_downloading_weight = false;
                state.status.comfyui.is_syncing_drive = false;
                state.status.comfyui.installing_pack_id.clear();
                state.status.comfyui.public_url.clear();
                Ok(Response::Done(
                    "演示实例已全部释放 · Demo runtimes released".into(),
                ))
            }
            Request::Status => {
                ensure!(
                    state.deployed,
                    "演示服务尚未部署，请点击唤醒 · Wake the demo runtime first"
                );
                Ok(Response::Status(Box::new(state.status.clone())))
            }
            Request::Action(action) => {
                ensure!(
                    state.deployed,
                    "请先唤醒演示实例 · Wake the demo runtime first"
                );
                if matches!(action, ComfyAction::Disconnect) {
                    state.deployed = false;
                    state.pending = None;
                    state.status.comfyui.is_running = false;
                    return Ok(Response::Done(
                        "演示实例已断开 · Demo runtime disconnected".into(),
                    ));
                }
                if !matches!(action, ComfyAction::Stop | ComfyAction::SetIdle) {
                    ensure!(
                        !state.status.comfyui.busy(),
                        "演示操作正在进行 · A demo operation is in progress"
                    );
                }
                let c = &mut state.status.comfyui;
                let job = match action {
                    ComfyAction::Start { force_restart: _ } => {
                        c.is_starting = true;
                        c.is_running = false;
                        c.status_message = "正在部署 ComfyUI · Starting ComfyUI".into();
                        Some(MockJob::Start)
                    }
                    ComfyAction::Stop => {
                        c.is_running = false;
                        c.is_starting = false;
                        c.is_syncing_drive = false;
                        c.is_downloading_weight = false;
                        c.installing_pack_id.clear();
                        c.public_url.clear();
                        c.queue_running = 0;
                        c.queue_pending = 0;
                        c.status_message = "ComfyUI 已停止并同步 · Stopped and synced".into();
                        state.pending = None;
                        None
                    }
                    ComfyAction::FreeVram => {
                        ensure!(c.is_running, "ComfyUI 当前未运行 · ComfyUI is stopped");
                        c.status_message = "已清理显存缓存 · VRAM cache freed".into();
                        None
                    }
                    ComfyAction::SyncDrive => {
                        c.is_syncing_drive = true;
                        c.status_message = "正在同步至 Drive · Syncing Drive".into();
                        Some(MockJob::Sync)
                    }
                    ComfyAction::DownloadWeight { source, .. } => {
                        ensure!(
                            !source.trim().is_empty(),
                            "请输入模型来源 · Enter a model source"
                        );
                        c.is_downloading_weight = true;
                        c.status_message = "8 线程下载权重 · Downloading weights".into();
                        Some(MockJob::DownloadWeight)
                    }
                    ComfyAction::InstallPack { pack_id } => {
                        ensure!(
                            c.preset_packs.iter().any(|p| p.id == pack_id),
                            "Unknown demo workflow pack"
                        );
                        c.is_downloading_weight = true;
                        c.installing_pack_id = pack_id.clone();
                        c.status_message = "正在安装模型与工作流 · Installing workflow pack".into();
                        Some(MockJob::Install(pack_id))
                    }
                    ComfyAction::SetIdle => {
                        state.status.idle_timeout_minutes = settings.idle_timeout_minutes;
                        state.status.idle_remaining_seconds = if settings.idle_timeout_minutes == 0
                        {
                            -1
                        } else {
                            i64::from(settings.idle_timeout_minutes) * 60
                        };
                        None
                    }
                    ComfyAction::Disconnect => unreachable!(),
                };
                if let Some(job) = job {
                    state.status.comfyui.progress = 0.02;
                    state.pending = Some((Instant::now(), job));
                }
                on_event(BridgeEvent::Log(
                    settings.redact(&state.status.comfyui.status_message),
                ));
                Ok(Response::Status(Box::new(state.status.clone())))
            }
            Request::Download(output) => {
                relative_components(&output.rel_path)?;
                let mut png = MOCK_PNG;
                Ok(Response::Downloaded(save_output(
                    settings,
                    &output.filename,
                    &mut png,
                )?))
            }
        }
    }
}

fn normalize_upload_subdir(remote_dir: &str, target: UploadTarget) -> &str {
    let mut trimmed = remote_dir.trim();
    if target == UploadTarget::Drive {
        for prefix in [
            "/content/drive/MyDrive/",
            "content/drive/MyDrive/",
            "drive/MyDrive/",
            "/content/",
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                trimmed = rest;
                break;
            }
        }
    } else if let Some(rest) = trimmed.strip_prefix("/content/") {
        trimmed = rest;
    }
    let trimmed = trimmed.trim_matches('/');
    if trimmed.is_empty() {
        "project"
    } else {
        trimmed
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
