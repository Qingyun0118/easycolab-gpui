use std::{
    collections::HashSet,
    io::Read,
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ely_gpui_component::{
    Assets,
    i18n::I18n,
    primitives::Severity,
    settings::Appearance,
    theme::{ActiveTheme, Mode, Theme},
};
use futures::future::{Either, select};
use gpui::{
    App, AppContext, Bounds, Context, Entity, Task, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowDecorations, WindowOptions, px, size,
};

use crate::{
    backend::{self, Backend, BridgeEvent},
    models::*,
    settings::Settings,
    ui::RootView,
};

const TELEMETRY_SEED_LEN: usize = 14;
const TELEMETRY_MAX_SAMPLES: usize = 48;

#[derive(Clone, Debug, Default)]
pub struct TelemetryHistory {
    pub vram_pct: Vec<f64>,
    pub ram_pct: Vec<f64>,
    pub disk_pct: Vec<f64>,
    pub pushed: u64,
}

impl TelemetryHistory {
    fn push_metric(
        series: &mut Vec<f64>,
        used: Option<f64>,
        free: Option<f64>,
        total: Option<f64>,
    ) -> bool {
        let Some(total) = total.filter(|t| *t > 0.0 && t.is_finite()) else {
            return false;
        };
        let used = used
            .or_else(|| free.map(|f| (total - f).max(0.0)))
            .unwrap_or(0.0);
        let pct = ((used / total) * 100.0).clamp(0.0, 100.0);
        if series.is_empty() {
            series.resize(TELEMETRY_SEED_LEN, pct);
        } else {
            series.push(pct);
            if series.len() > TELEMETRY_MAX_SAMPLES {
                series.drain(..series.len() - TELEMETRY_MAX_SAMPLES);
            }
        }
        true
    }

    pub fn push(&mut self, server: &ServerStatus) {
        let mut updated = false;
        updated |= Self::push_metric(
            &mut self.vram_pct,
            server.vram_used_gb,
            server.vram_free_gb,
            server.vram_total_gb,
        );
        updated |= Self::push_metric(
            &mut self.ram_pct,
            server.ram_used_gb,
            server.ram_free_gb,
            server.ram_total_gb,
        );
        updated |= Self::push_metric(
            &mut self.disk_pct,
            server.disk_used_gb,
            server.disk_free_gb,
            server.disk_total_gb,
        );
        if updated {
            self.pushed = self.pushed.saturating_add(1);
        }
    }

    pub fn clear(&mut self) {
        self.vram_pct.clear();
        self.ram_pct.clear();
        self.disk_pct.clear();
        self.pushed = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmation {
    Wake,
    ReleaseAll,
    ReleaseCurrent,
    Quit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DrawerTab {
    #[default]
    Logs,
    Terminal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskProgress {
    pub key: &'static str,
    pub value: Option<f32>,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleStage {
    SignedOut,
    NeedGpu,
    GpuReady,
    ServiceReady,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastNotice {
    pub severity: Severity,
    pub title: String,
    pub body: Option<String>,
    pub stay: Option<Duration>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LocalTunnelState {
    #[default]
    Off,
    Starting {
        port: u16,
    },
    Ready {
        port: u16,
        url: String,
    },
}

struct LocalTunnelHandle {
    child: Arc<Mutex<Option<Child>>>,
}

impl LocalTunnelHandle {
    fn kill(&self) {
        if let Ok(mut guard) = self.child.lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for LocalTunnelHandle {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct AppState {
    pub settings: Settings,
    pub route: Route,
    pub mock: bool,
    pub account: AccountStatus,
    pub active_session: Option<Session>,
    pub runtime_metrics: Option<ServerStatus>,
    pub server: Option<ServerStatus>,
    pub connected: bool,
    pub managed: bool,
    pub local_tunnel: LocalTunnelState,
    pub logs: Vec<String>,
    pub logs_open: bool,
    pub drawer_tab: DrawerTab,
    pub active_task: Option<TaskProgress>,
    pub error: Option<String>,
    pub connection_error: Option<String>,
    pub notice: Option<String>,
    pub pending_toasts: Vec<ToastNotice>,
    pub auth_url: Option<String>,
    pub auth_cancel: Option<Arc<AtomicBool>>,
    pub show_manual_auth: bool,
    pub upload_target: UploadTarget,
    pub last_uploaded_remote: Option<String>,
    pub telemetry: TelemetryHistory,
    pub local_outputs: Vec<PathBuf>,
    pub preview: Option<PathBuf>,
    pub confirmation: Option<Confirmation>,
    pub busy: HashSet<&'static str>,
    pub pending_fields: Vec<(&'static str, String)>,
    pub backend: Arc<dyn Backend>,
    pub system_mode: Mode,
    pub poll_service: bool,
    pub service_generation: u64,
    last_console: Vec<String>,
    last_logged_connection_error: Option<String>,
    last_logged_metrics_error: Option<String>,
    pending_manual_auth: bool,
    quitting_after_release: bool,
    local_tunnel_generation: u64,
    local_tunnel_handle: Option<LocalTunnelHandle>,
    local_tunnel_task: Option<Task<()>>,
    save: Option<Task<()>>,
    poller: Option<Task<()>>,
}

impl AppState {
    pub fn new(
        settings: Settings,
        backend: Arc<dyn Backend>,
        mock: bool,
        error: Option<String>,
        cx: &mut App,
    ) -> Entity<Self> {
        let system_mode = Mode::from(cx.window_appearance());
        cx.set_global(I18n::new(&settings.language));
        let mode = appearance(&settings.appearance).mode(system_mode);
        Theme::set_mode_now(mode, cx);
        let managed = settings.bridge_managed;
        let poll_service = !settings.service_url.is_empty();
        let initial_toasts = error
            .as_ref()
            .map(|msg| {
                let safe = settings.redact(msg);
                vec![ToastNotice {
                    severity: Severity::Danger,
                    title: if settings.language.starts_with("zh") {
                        "启动提示".into()
                    } else {
                        "Startup notice".into()
                    },
                    body: Some(safe),
                    stay: Some(Duration::from_secs(8)),
                }]
            })
            .unwrap_or_default();
        cx.new(|_| Self {
            settings,
            route: Route::Dashboard,
            mock,
            account: AccountStatus::default(),
            active_session: None,
            runtime_metrics: None,
            server: None,
            connected: false,
            managed,
            local_tunnel: LocalTunnelState::Off,
            logs: vec![],
            logs_open: false,
            drawer_tab: DrawerTab::Logs,
            active_task: None,
            error,
            connection_error: None,
            notice: None,
            pending_toasts: initial_toasts,
            auth_url: None,
            auth_cancel: None,
            show_manual_auth: false,
            upload_target: UploadTarget::Runtime,
            last_uploaded_remote: None,
            telemetry: TelemetryHistory::default(),
            local_outputs: vec![],
            preview: None,
            confirmation: None,
            busy: HashSet::new(),
            pending_fields: vec![],
            backend,
            system_mode,
            poll_service,
            service_generation: 0,
            last_console: vec![],
            last_logged_connection_error: None,
            last_logged_metrics_error: None,
            pending_manual_auth: false,
            quitting_after_release: false,
            local_tunnel_generation: 0,
            local_tunnel_handle: None,
            local_tunnel_task: None,
            save: None,
            poller: None,
        })
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.scan_outputs(cx);
        self.request(Request::AuthStatus, cx);
        if !self.settings.service_url.is_empty() {
            self.request(Request::Status, cx);
        }
        self.poller = Some(cx.spawn(async move |this, cx| {
            let mut seconds = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                seconds += 3;
                if this
                    .update(cx, |state, cx| {
                        if state.poll_service {
                            state.request(Request::Status, cx);
                        } else if state.active_session.is_some() && seconds.is_multiple_of(6) {
                            state.request(Request::RuntimeMetrics, cx);
                        }
                        if seconds.is_multiple_of(30)
                            && (state.connected || state.active_session.is_some())
                        {
                            state.request(Request::Keepalive, cx);
                        }
                        if seconds.is_multiple_of(60) && state.account.logged_in {
                            state.request(Request::AuthStatus, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    pub fn text(&self, zh: &'static str, en: &'static str) -> &'static str {
        if self.settings.language.starts_with("zh") {
            zh
        } else {
            en
        }
    }

    pub fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        self.route = route;
        cx.notify();
    }

    pub fn set_language(&mut self, language: &str, cx: &mut Context<Self>) {
        self.settings.language = language.to_owned();
        I18n::set_locale(language, cx);
        self.save_settings(cx);
        cx.notify();
    }

    pub fn set_appearance(&mut self, value: &str, cx: &mut Context<Self>) {
        self.settings.appearance = value.to_owned();
        Theme::set_mode(appearance(value).mode(self.system_mode), cx);
        self.save_settings(cx);
        cx.notify();
    }

    pub fn system_appearance_changed(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.system_mode = mode;
        if self.settings.appearance == "system" {
            Theme::set_mode(mode, cx);
        }
    }

    pub fn toggle_combined_telemetry(&mut self, cx: &mut Context<Self>) {
        self.settings.combined_telemetry_chart = !self.settings.combined_telemetry_chart;
        self.save_settings(cx);
        cx.notify();
    }

    pub fn open_drawer_tab(&mut self, tab: DrawerTab, cx: &mut Context<Self>) {
        if self.logs_open && self.drawer_tab == tab {
            self.logs_open = false;
        } else {
            self.drawer_tab = tab;
            self.logs_open = true;
        }
        cx.notify();
    }

    pub fn stop_local_tunnel(&mut self, notify: bool, cx: &mut Context<Self>) {
        let was_active = !matches!(self.local_tunnel, LocalTunnelState::Off);
        self.local_tunnel_generation += 1;
        self.local_tunnel_task = None;
        if let Some(handle) = self.local_tunnel_handle.take() {
            handle.kill();
        }
        self.local_tunnel = LocalTunnelState::Off;
        if was_active && notify {
            let msg = self.text("已关闭本机直连转发", "Local port forwarding stopped");
            self.log(msg.into());
            self.set_info(msg);
        }
        cx.notify();
    }

    pub fn toggle_local_tunnel(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.local_tunnel, LocalTunnelState::Off) {
            self.stop_local_tunnel(true, cx);
            return;
        }
        let running = self.server.as_ref().is_some_and(|s| s.comfyui.is_running);
        if !self.connected || !running {
            let msg = self.text(
                "请先启动 ComfyUI 后再开启本机直连转发",
                "Start ComfyUI before enabling local port forwarding",
            );
            self.set_error(msg);
            cx.notify();
            return;
        }
        if self.mock {
            let url = "http://127.0.0.1:8188".to_owned();
            self.local_tunnel = LocalTunnelState::Ready {
                port: 8188,
                url: url.clone(),
            };
            let msg = if self.settings.language.starts_with("zh") {
                format!("本机直连转发已就绪（演示）：{url}")
            } else {
                format!("Local port forward ready (demo): {url}")
            };
            self.log(msg.clone());
            self.set_notice(msg);
            cx.notify();
            return;
        }

        let remote_port = self
            .server
            .as_ref()
            .map(|s| s.comfyui.local_port)
            .filter(|&p| p > 0)
            .unwrap_or(28188);
        let session_name = self.active_session_name().to_owned();
        let local_port = pick_local_forward_port(8188);
        self.local_tunnel_generation += 1;
        let generation = self.local_tunnel_generation;
        self.local_tunnel = LocalTunnelState::Starting { port: local_port };
        let start_msg = if self.settings.language.starts_with("zh") {
            format!(
                "正在建立本机直连转发 127.0.0.1:{local_port} → 云端 127.0.0.1:{remote_port} (会话: {session_name})…"
            )
        } else {
            format!(
                "Starting local forward 127.0.0.1:{local_port} → remote 127.0.0.1:{remote_port} (session: {session_name})…"
            )
        };
        self.log(start_msg);
        cx.notify();

        let shared_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        self.local_tunnel_handle = Some(LocalTunnelHandle {
            child: shared_child.clone(),
        });

        let watcher_child = shared_child.clone();
        self.local_tunnel_task = Some(cx.spawn(async move |this, cx| {
            let spawn_result = cx
                .background_spawn(async move {
                    spawn_colab_ssh_forward(&session_name, local_port, remote_port, &watcher_child)
                })
                .await;

            let ready_stderr = match spawn_result {
                Ok(stderr_buf) => {
                    let url = format!("http://127.0.0.1:{local_port}");
                    let still_current = this
                        .update(cx, |state, cx| {
                            if state.local_tunnel_generation != generation {
                                return false;
                            }
                            state.local_tunnel = LocalTunnelState::Ready {
                                port: local_port,
                                url: url.clone(),
                            };
                            let msg = if state.settings.language.starts_with("zh") {
                                format!("本机直连转发已就绪：{url}（零隧道延迟，无需输密码）")
                            } else {
                                format!("Local forward ready: {url} (direct, no tunnel latency)")
                            };
                            state.log(msg.clone());
                            state.set_notice(msg);
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !still_current {
                        return;
                    }
                    stderr_buf
                }
                Err(error) => {
                    let _ = this.update(cx, |state, cx| {
                        if state.local_tunnel_generation != generation {
                            return;
                        }
                        state.local_tunnel_handle = None;
                        state.local_tunnel = LocalTunnelState::Off;
                        let msg = format!(
                            "{}: {error}",
                            state.text("本机直连转发启动失败", "Local port forward failed")
                        );
                        state.log(msg.clone());
                        state.set_error(msg);
                        cx.notify();
                    });
                    return;
                }
            };

            // Monitor the SSH process until stopped or unexpected exit.
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let exited = {
                    let Ok(mut guard) = shared_child.lock() else {
                        break;
                    };
                    match guard.as_mut() {
                        Some(child) => match child.try_wait() {
                            Ok(Some(status)) => {
                                guard.take();
                                Some(format!("{status}"))
                            }
                            Ok(None) => None,
                            Err(err) => {
                                guard.take();
                                Some(err.to_string())
                            }
                        },
                        None => break,
                    }
                };
                if let Some(reason) = exited {
                    let tail = ready_stderr
                        .lock()
                        .ok()
                        .map(|s| s.trim().to_owned())
                        .unwrap_or_default();
                    let _ = this.update(cx, |state, cx| {
                        if state.local_tunnel_generation != generation {
                            return;
                        }
                        state.local_tunnel_handle = None;
                        state.local_tunnel = LocalTunnelState::Off;
                        let detail = if tail.is_empty() {
                            reason
                        } else {
                            let last = tail.lines().last().unwrap_or(&tail);
                            format!("{reason} ({last})")
                        };
                        let msg = format!(
                            "{}: {detail}",
                            state.text("本机直连转发已断开", "Local port forward disconnected")
                        );
                        state.log(msg.clone());
                        state.notify_warning(
                            state.text("本机直连转发已断开", "Local port forward disconnected"),
                            Some(detail),
                        );
                        cx.notify();
                    });
                    break;
                }
            }
        }));
    }

    pub fn push_toast(
        &mut self,
        severity: Severity,
        title: impl Into<String>,
        body: Option<String>,
        stay: Option<Duration>,
    ) {
        let title = self.settings.redact(&title.into());
        let body = body.map(|b| self.settings.redact(&b));
        self.pending_toasts.push(ToastNotice {
            severity,
            title,
            body,
            stay,
        });
    }

    pub fn set_notice(&mut self, message: impl Into<String>) {
        let clean = self.settings.redact(&message.into());
        self.notice = Some(clean.clone());
        self.push_toast(Severity::Success, clean, None, Some(Duration::from_secs(4)));
    }

    pub fn set_info(&mut self, message: impl Into<String>) {
        let clean = self.settings.redact(&message.into());
        self.notice = Some(clean.clone());
        self.push_toast(Severity::Info, clean, None, Some(Duration::from_secs(4)));
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        let clean = self.settings.redact(&message.into());
        self.error = Some(clean.clone());
        self.push_toast(
            Severity::Danger,
            self.text("操作提示", "Action failed"),
            Some(clean),
            Some(Duration::from_secs(8)),
        );
    }

    pub fn notify_warning(&mut self, title: impl Into<String>, body: Option<String>) {
        self.push_toast(Severity::Warning, title, body, Some(Duration::from_secs(6)));
    }

    pub fn cancel_auth(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.auth_cancel.take() {
            cancel.store(true, Ordering::SeqCst);
        }
        self.auth_url = None;
        self.pending_manual_auth = false;
        let msg = self.text("正在取消浏览器授权…", "Cancelling browser sign-in…");
        self.log(msg.into());
        self.set_info(msg);
        cx.notify();
    }

    pub fn switch_to_manual_auth(&mut self, cx: &mut Context<Self>) {
        self.show_manual_auth = true;
        self.auth_url = None;
        if let Some(cancel) = self.auth_cancel.take() {
            cancel.store(true, Ordering::SeqCst);
            self.pending_manual_auth = true;
            cx.notify();
        } else if !self.busy.contains("auth") {
            self.request(Request::AuthUrl, cx);
        } else {
            cx.notify();
        }
    }

    pub fn save_settings(&mut self, cx: &mut Context<Self>) {
        self.save_settings_inner(true, cx);
    }

    pub fn save_settings_silent(&mut self, cx: &mut Context<Self>) {
        self.save_settings_inner(false, cx);
    }

    fn save_settings_inner(&mut self, notify: bool, cx: &mut Context<Self>) {
        if cx.is_test() {
            if notify {
                let msg = self.text("设置已保存", "Settings saved");
                self.set_notice(msg);
            }
            return;
        }
        let settings = self.settings.clone();
        let mock = self.mock;
        let previous = self.save.take();
        self.save = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = cx
                .background_spawn(async move {
                    if mock {
                        settings.save_to(&crate::settings::config_dir().join("demo/settings.json"))
                    } else {
                        settings.save()
                    }
                })
                .await;
            let _ = this.update(cx, |state, cx| {
                match result {
                    Ok(()) => {
                        if notify {
                            let msg = state.text("设置已保存", "Settings saved");
                            state.set_notice(msg);
                        }
                    }
                    Err(error) => {
                        let msg = format!(
                            "{}: {error:#}",
                            state.text("保存失败", "Could not save settings")
                        );
                        state.log(msg.clone());
                        state.set_error(msg);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// 仅当已在本会话成功唤醒/验证过 GPU 实例，或已连接远程 ComfyUI 服务时返回 true。
    pub fn has_awake_runtime(&self) -> bool {
        self.active_session.is_some() || self.connected
    }

    /// 优先返回已连接 ComfyUI 的完整状态；仅唤醒 GPU 未部署 ComfyUI 时返回内核直采的显存/内存/磁盘遥测。
    pub fn telemetry_status(&self) -> Option<&ServerStatus> {
        self.server.as_ref().or(self.runtime_metrics.as_ref())
    }

    /// 账号下存在尚未在本应用验证/接管的云端实例。
    pub fn has_unverified_cloud_sessions(&self) -> bool {
        !self.has_awake_runtime() && !self.account.sessions.is_empty()
    }

    pub fn lifecycle_stage(&self) -> LifecycleStage {
        if self.connected {
            LifecycleStage::ServiceReady
        } else if self.active_session.is_some() {
            LifecycleStage::GpuReady
        } else if self.account.logged_in {
            LifecycleStage::NeedGpu
        } else {
            LifecycleStage::SignedOut
        }
    }

    pub fn active_session_name(&self) -> &str {
        self.active_session
            .as_ref()
            .or_else(|| self.account.sessions.first())
            .map(|s| s.name.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("easycolab")
    }

    pub fn ensure_management_password(&mut self) -> Result<(), String> {
        if self.settings.management_password.is_empty() {
            let mut bytes = [0u8; 32];
            if let Err(error) = getrandom::fill(&mut bytes) {
                return Err(format!("Could not generate password: {error}"));
            }
            self.settings.management_password =
                bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            self.pending_fields
                .push(("password", self.settings.management_password.clone()));
        }
        Ok(())
    }

    pub fn ask_wake(&mut self, cx: &mut Context<Self>) {
        if !self.account.logged_in {
            let msg = self.text(
                "请先完成第 1 步：绑定 Google 账号",
                "Complete Step 1 first: sign in with Google",
            );
            self.set_error(msg);
            cx.notify();
            return;
        }
        self.confirmation = Some(Confirmation::Wake);
        self.request(Request::AuthStatus, cx);
        cx.notify();
    }

    pub fn start_deploy(&mut self, cx: &mut Context<Self>) {
        if !self.has_awake_runtime() {
            let msg = self.text(
                "请先唤醒并验证 GPU 实例后再部署 ComfyUI",
                "Wake and verify a GPU runtime before deploying ComfyUI",
            );
            self.set_error(msg);
            cx.notify();
            return;
        }
        if let Err(error) = self.ensure_management_password() {
            self.set_error(error);
            cx.notify();
            return;
        }
        self.save_settings_silent(cx);
        self.request(Request::Deploy, cx);
    }

    pub fn ask_release(&mut self, cx: &mut Context<Self>) {
        if self.managed || !self.connected {
            self.ask_release_all(cx);
        } else {
            self.confirmation = Some(Confirmation::ReleaseCurrent);
        }
        cx.notify();
    }

    pub fn ask_release_all(&mut self, cx: &mut Context<Self>) {
        self.confirmation = Some(Confirmation::ReleaseAll);
        self.request(Request::AuthStatus, cx);
        cx.notify();
    }

    pub fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.connected
            || self.active_session.is_some()
            || !self.account.sessions.is_empty()
            || !self.busy.is_empty()
        {
            self.confirmation = Some(Confirmation::Quit);
            cx.notify();
        } else {
            cx.quit();
        }
    }

    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        match self.confirmation.take() {
            Some(Confirmation::Wake) => self.request(Request::Wake, cx),
            Some(Confirmation::ReleaseAll) => self.request(Request::ReleaseAll, cx),
            Some(Confirmation::ReleaseCurrent) => {
                self.request(Request::Action(ComfyAction::Disconnect), cx)
            }
            Some(Confirmation::Quit) => {
                self.quitting_after_release = true;
                if self.managed || self.settings.service_url.is_empty() {
                    self.request(Request::ReleaseAll, cx);
                } else {
                    self.request(Request::Action(ComfyAction::Disconnect), cx);
                }
            }
            None => {}
        }
        cx.notify();
    }

    pub fn log(&mut self, line: String) {
        let redacted = self.settings.redact(&line);
        let trimmed = redacted.trim();
        if trimmed.is_empty() {
            return;
        }
        let formatted = if trimmed.starts_with('[') {
            redacted
        } else {
            format!("[{}] {}", utc_hms(), redacted)
        };
        self.logs.push(formatted);
        if self.logs.len() > 1000 {
            self.logs.drain(..self.logs.len() - 1000);
        }
    }

    fn request_start_banner(&self, request: &Request) -> Option<(Option<f32>, String, bool)> {
        let zh = self.settings.language.starts_with("zh");
        match request {
            Request::AuthStatus
            | Request::Keepalive
            | Request::Status
            | Request::RuntimeMetrics => None,
            Request::AuthLogin => Some((
                None,
                if zh {
                    "启动 Google OAuth 浏览器授权…".into()
                } else {
                    "Starting Google OAuth browser sign-in…".into()
                },
                false,
            )),
            Request::AuthUrl => Some((
                None,
                if zh {
                    "生成手动 Google 授权链接…".into()
                } else {
                    "Generating manual Google OAuth URL…".into()
                },
                false,
            )),
            Request::AuthCode(_) => Some((
                None,
                if zh {
                    "正在提交 Google 授权码…".into()
                } else {
                    "Submitting Google authorization code…".into()
                },
                false,
            )),
            Request::Logout => Some((
                None,
                if zh {
                    "正在退出 Google 账号并清理本地凭据…".into()
                } else {
                    "Signing out of Google account…".into()
                },
                false,
            )),
            Request::Wake => Some((
                Some(0.05),
                if zh {
                    format!("开始唤醒 {} GPU 实例…", self.settings.gpu)
                } else {
                    format!("Waking {} GPU runtime…", self.settings.gpu)
                },
                true,
            )),
            Request::Deploy => Some((
                Some(0.05),
                if zh {
                    "开始在已唤醒 GPU 上部署 ComfyUI…".into()
                } else {
                    "Deploying ComfyUI on active GPU runtime…".into()
                },
                true,
            )),
            Request::UploadProject {
                local_path,
                remote_dir,
                target,
            } => Some((
                Some(0.05),
                {
                    let base = target.base_path();
                    let rel = remote_dir.trim().trim_matches('/');
                    let rel = if rel.is_empty() { "project" } else { rel };
                    if zh {
                        format!("开始上传项目 {} → {base}/{rel}", local_path.display())
                    } else {
                        format!("Uploading {} → {base}/{rel}", local_path.display())
                    }
                },
                true,
            )),
            Request::ReleaseAll => Some((
                None,
                if zh {
                    "正在释放全部 Colab 运行时实例…".into()
                } else {
                    "Releasing all Colab runtimes…".into()
                },
                true,
            )),
            Request::Action(action) => match action {
                ComfyAction::Start { force_restart } => Some((
                    Some(0.05),
                    if zh {
                        if *force_restart {
                            "请求强制重启 ComfyUI…".into()
                        } else {
                            "请求启动 ComfyUI…".into()
                        }
                    } else if *force_restart {
                        "Restarting ComfyUI…".into()
                    } else {
                        "Starting ComfyUI…".into()
                    },
                    true,
                )),
                ComfyAction::Stop => Some((
                    None,
                    if zh {
                        "请求停止 ComfyUI 并同步输出…".into()
                    } else {
                        "Stopping ComfyUI and syncing outputs…".into()
                    },
                    false,
                )),
                ComfyAction::FreeVram => Some((
                    None,
                    if zh {
                        "请求释放显存缓存…".into()
                    } else {
                        "Freeing GPU VRAM cache…".into()
                    },
                    false,
                )),
                ComfyAction::SyncDrive => Some((
                    Some(0.05),
                    if zh {
                        "开始同步 Google Drive…".into()
                    } else {
                        "Syncing Google Drive…".into()
                    },
                    true,
                )),
                ComfyAction::DownloadWeight {
                    source, subfolder, ..
                } => Some((
                    Some(0.05),
                    if zh {
                        format!("开始下载权重 [{subfolder}]: {source}")
                    } else {
                        format!("Downloading model [{subfolder}]: {source}")
                    },
                    true,
                )),
                ComfyAction::InstallPack { pack_id } => Some((
                    Some(0.05),
                    if zh {
                        format!("开始安装工作流包: {pack_id}")
                    } else {
                        format!("Installing workflow pack: {pack_id}")
                    },
                    true,
                )),
                ComfyAction::SetIdle => Some((
                    None,
                    if zh {
                        format!(
                            "更新闲置自动释放为 {} 分钟",
                            self.settings.idle_timeout_minutes
                        )
                    } else {
                        format!(
                            "Updating idle timeout to {} min",
                            self.settings.idle_timeout_minutes
                        )
                    },
                    false,
                )),
                ComfyAction::Disconnect => Some((
                    None,
                    if zh {
                        "正在断开并释放当前服务实例…".into()
                    } else {
                        "Disconnecting current runtime…".into()
                    },
                    false,
                )),
            },
            Request::Download(output) => Some((
                None,
                if zh {
                    format!("开始下载输出文件: {}", output.filename)
                } else {
                    format!("Downloading output: {}", output.filename)
                },
                false,
            )),
        }
    }

    pub fn scan_outputs(&mut self, cx: &mut Context<Self>) {
        let directory = self.settings.output_dir.clone();
        cx.spawn(async move |this, cx| {
            let entries = cx
                .background_spawn(async move {
                    let mut paths: Vec<_> = std::fs::read_dir(&directory)
                        .into_iter()
                        .flatten()
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.is_file()
                                && path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                                    matches!(
                                        e.to_ascii_lowercase().as_str(),
                                        "png"
                                            | "jpg"
                                            | "jpeg"
                                            | "webp"
                                            | "gif"
                                            | "mp4"
                                            | "webm"
                                            | "mov"
                                            | "mkv"
                                            | "avi"
                                    )
                                })
                        })
                        .collect();
                    paths.sort();
                    (directory, paths)
                })
                .await;
            let _ = this.update(cx, |state, cx| {
                if state.settings.output_dir == entries.0 {
                    state.local_outputs = entries.1;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn open_output_dir(&mut self, cx: &mut Context<Self>) {
        let directory = self.settings.output_dir.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    std::fs::create_dir_all(&directory)?;
                    let canonical = directory.canonicalize()?;
                    url::Url::from_directory_path(canonical)
                        .map_err(|()| anyhow::anyhow!("Could not open output directory"))
                })
                .await;
            let _ = this.update(cx, |state, cx| match result {
                Ok(url) => cx.open_url(url.as_str()),
                Err(error) => {
                    let msg = format!("{error:#}");
                    state.log(format!(
                        "{}: {msg}",
                        state.text("打开目录失败", "Could not open folder")
                    ));
                    state.set_error(msg);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn request(&mut self, request: Request, cx: &mut Context<Self>) {
        if matches!(request, Request::UploadProject { .. }) && !self.has_awake_runtime() {
            let msg = self.text(
                "请先唤醒并验证 GPU 实例后再上传项目",
                "Wake and verify a GPU runtime before uploading a project",
            );
            self.set_error(msg);
            cx.notify();
            return;
        }
        let key = request.key();
        if self.busy.contains(key) {
            return;
        }
        if (key == "status" || key == "runtime-metrics")
            && (self.busy.contains("wake")
                || self.busy.contains("deploy")
                || self.busy.contains("upload")
                || self.busy.contains("release"))
        {
            return;
        }
        if !matches!(
            request,
            Request::Status | Request::RuntimeMetrics | Request::Keepalive | Request::AuthStatus
        ) {
            self.error = None;
            self.notice = None;
        }
        if matches!(request, Request::Wake | Request::Deploy) {
            self.managed = true;
            self.settings.bridge_managed = true;
            self.save_settings_silent(cx);
        }
        if matches!(
            request,
            Request::Wake
                | Request::Deploy
                | Request::ReleaseAll
                | Request::Action(ComfyAction::Disconnect)
        ) {
            self.service_generation += 1;
        }
        let generation = self.service_generation;
        self.busy.insert(key);
        if let Some((initial_progress, start_label, auto_open_logs)) =
            self.request_start_banner(&request)
        {
            self.log(start_label.clone());
            self.active_task = Some(TaskProgress {
                key,
                value: initial_progress,
                label: start_label,
            });
            if auto_open_logs && !(self.logs_open && self.drawer_tab == DrawerTab::Terminal) {
                self.logs_open = true;
                self.drawer_tab = DrawerTab::Logs;
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        if matches!(request, Request::AuthLogin) {
            self.auth_cancel = Some(cancel.clone());
            self.show_manual_auth = false;
        }
        let backend = self.backend.clone();
        let settings = self.settings.clone();
        let update = request.clone();
        let (sender, receiver) = mpsc::channel();
        let cancel_for_worker = cancel.clone();
        cx.spawn(async move |this, cx| {
            let task = cx.background_spawn(async move {
                backend.run_with_events(&settings, request, &cancel_for_worker, &mut |event| {
                    let _ = sender.send(event);
                })
            });
            futures::pin_mut!(task);
            let drain = |cx: &mut gpui::AsyncApp| {
                let events: Vec<_> = receiver.try_iter().collect();
                if !events.is_empty() {
                    let _ = this.update(cx, |state, cx| {
                        for event in events {
                            match event {
                                BridgeEvent::Log(line) => state.log(line),
                                BridgeEvent::AuthUrl(url) => {
                                    if !cancel.load(Ordering::Relaxed) {
                                        cx.open_url(&url);
                                        state.auth_url = Some(url);
                                    }
                                }
                                BridgeEvent::Progress { value, label } => {
                                    state.active_task = Some(TaskProgress { key, value, label });
                                }
                            }
                        }
                        cx.notify();
                    });
                }
            };
            let result = loop {
                match select(
                    task.as_mut(),
                    cx.background_executor().timer(Duration::from_millis(100)),
                )
                .await
                {
                    Either::Left((result, _)) => break result,
                    Either::Right(((), _)) => drain(cx),
                }
            };
            drain(cx);
            let _ = this.update(cx, |state, cx| {
                state.busy.remove(key);
                if state
                    .active_task
                    .as_ref()
                    .is_some_and(|task| task.key == key)
                {
                    state.active_task = None;
                }
                if matches!(update, Request::AuthLogin) {
                    state.auth_cancel = None;
                }
                if generation != state.service_generation
                    && matches!(
                        update,
                        Request::Wake
                            | Request::Deploy
                            | Request::Status
                            | Request::RuntimeMetrics
                            | Request::Keepalive
                            | Request::ReleaseAll
                            | Request::Action(_)
                    )
                {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(response) => {
                        state.pending_manual_auth = false;
                        state.apply_current_response(update, response, generation, cx);
                    }
                    Err(error) => {
                        if matches!(update, Request::AuthLogin) && cancel.load(Ordering::Relaxed) {
                            state.auth_url = None;
                            state.log(
                                state
                                    .text("已取消 Google 登录", "Google sign-in cancelled")
                                    .into(),
                            );
                            if std::mem::take(&mut state.pending_manual_auth) {
                                state.request(Request::AuthUrl, cx);
                            } else {
                                let msg =
                                    state.text("已取消 Google 登录", "Google sign-in cancelled");
                                state.set_info(msg);
                            }
                            cx.notify();
                            return;
                        }
                        let message = state.settings.redact(&format!("{error:#}"));
                        if matches!(update, Request::Status) {
                            if state.last_logged_connection_error.as_deref() != Some(&message) {
                                state.log(format!(
                                    "{}: {message}",
                                    state.text("连接异常", "Connection error")
                                ));
                                state.notify_warning(
                                    state.text(
                                        "服务失联，正在自动重试",
                                        "Connection lost; retrying",
                                    ),
                                    Some(message.clone()),
                                );
                                state.last_logged_connection_error = Some(message.clone());
                            }
                            state.connected = false;
                            state.connection_error = Some(message);
                        } else if matches!(update, Request::RuntimeMetrics) {
                            if state.last_logged_metrics_error.as_deref() != Some(&message) {
                                state.log(format!(
                                    "{}: {message}",
                                    state.text("内核监控采集异常", "Kernel telemetry error")
                                ));
                                state.last_logged_metrics_error = Some(message);
                            }
                        } else {
                            state.log(format!(
                                "{}: {message}",
                                state.text("操作失败", "Action failed")
                            ));
                            state.set_error(message);
                        }
                        if matches!(update, Request::Wake) {
                            state.request(Request::AuthStatus, cx);
                        }
                        state.quitting_after_release = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn apply_current_response(
        &mut self,
        request: Request,
        response: Response,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if generation != self.service_generation
            && matches!(
                request,
                Request::Wake
                    | Request::Deploy
                    | Request::Status
                    | Request::RuntimeMetrics
                    | Request::Keepalive
                    | Request::ReleaseAll
                    | Request::Action(_)
            )
        {
            return;
        }
        self.apply_response(request, response, cx);
        cx.notify();
    }

    fn apply_response(&mut self, request: Request, response: Response, cx: &mut Context<Self>) {
        match response {
            Response::Account(account) => {
                let prev_email = self.account.email.clone();
                let was_logged_in = self.account.logged_in;
                self.account = account;
                if matches!(
                    request,
                    Request::Logout | Request::AuthCode(_) | Request::AuthLogin
                ) {
                    self.auth_url = None;
                    self.show_manual_auth = false;
                    self.auth_cancel = None;
                }
                if !self.account.logged_in {
                    self.active_session = None;
                    self.runtime_metrics = None;
                    if self.managed && !self.connected {
                        self.managed = false;
                        self.settings.bridge_managed = false;
                        self.save_settings_silent(cx);
                    }
                } else if self.account.sessions.is_empty()
                    && !self.connected
                    && !self.busy.contains("wake")
                    && !self.busy.contains("deploy")
                {
                    if self.active_session.take().is_some() {
                        self.runtime_metrics = None;
                        self.telemetry.clear();
                        let msg = self.text(
                            "云端 GPU 实例已结束或被释放，请重新唤醒",
                            "Cloud GPU runtime is no longer active; wake a new one",
                        );
                        self.log(msg.into());
                        self.notify_warning(msg, None);
                    }
                    if self.managed && self.settings.service_url.is_empty() {
                        self.managed = false;
                        self.settings.bridge_managed = false;
                        self.save_settings_silent(cx);
                    }
                } else if let Some(active) = &self.active_session
                    && !self.connected
                    && !self.busy.contains("wake")
                    && !self
                        .account
                        .sessions
                        .iter()
                        .any(|s| s.endpoint == active.endpoint)
                {
                    self.active_session = None;
                    self.runtime_metrics = None;
                    self.telemetry.clear();
                    let msg = self.text(
                        "原 GPU 实例已失效，请重新唤醒或接管当前实例",
                        "Previous GPU runtime expired; wake or verify an active runtime",
                    );
                    self.log(msg.into());
                    self.notify_warning(msg, None);
                }
                if matches!(request, Request::AuthLogin | Request::AuthCode(_))
                    && self.account.logged_in
                {
                    let email_changed =
                        was_logged_in && !prev_email.is_empty() && self.account.email != prev_email;
                    if email_changed {
                        self.stop_local_tunnel(false, cx);
                        self.active_session = None;
                        self.runtime_metrics = None;
                        self.last_uploaded_remote = None;
                        if self.managed {
                            self.connected = false;
                            self.managed = false;
                            self.server = None;
                            self.poll_service = false;
                            self.telemetry.clear();
                            self.settings.bridge_managed = false;
                            self.settings.service_url.clear();
                            self.pending_fields.push(("service-url", String::new()));
                            self.save_settings_silent(cx);
                        }
                    }
                    let label = if self.account.email.is_empty() {
                        self.text("Google 账号已绑定", "Google account signed in")
                            .to_owned()
                    } else {
                        format!(
                            "{}: {}",
                            self.text("Google 账号已绑定", "Google account signed in"),
                            self.account.email
                        )
                    };
                    self.log(label.clone());
                    self.set_notice(label);
                }
            }
            Response::AuthUrl(url) => {
                cx.open_url(&url);
                self.auth_url = Some(url);
                self.show_manual_auth = true;
            }
            Response::Woken(session) => {
                self.managed = true;
                self.settings.bridge_managed = true;
                self.active_session = Some(session.clone());
                self.runtime_metrics = None;
                self.last_logged_metrics_error = None;
                if !self.connected {
                    self.telemetry.clear();
                }
                if !self
                    .account
                    .sessions
                    .iter()
                    .any(|s| s.endpoint == session.endpoint)
                {
                    self.account.sessions.push(session.clone());
                }
                self.route = Route::Deploy;
                let note = format!(
                    "{}: {} ({})",
                    self.text("GPU 实例已唤醒就绪", "GPU runtime ready"),
                    session.accelerator,
                    session.name
                );
                self.log(note.clone());
                self.set_notice(note);
                self.save_settings_silent(cx);
                self.request(Request::AuthStatus, cx);
                self.request(Request::RuntimeMetrics, cx);
            }
            Response::Deployed(url) => {
                self.log(format!(
                    "{}: {url}",
                    self.text("ComfyUI 部署完成，公网地址", "ComfyUI deployed at")
                ));
                self.set_notice(self.text(
                    "ComfyUI 部署完成，已连接服务",
                    "ComfyUI deployed and connected",
                ));
                self.settings.service_url = url.clone();
                self.pending_fields.push(("service-url", url));
                self.managed = true;
                self.settings.bridge_managed = true;
                self.poll_service = true;
                self.connected = true;
                self.telemetry.clear();
                self.save_settings_silent(cx);
                self.request(Request::Status, cx);
                self.request(Request::AuthStatus, cx);
            }
            Response::Uploaded { remote_path, files } => {
                self.last_uploaded_remote = Some(remote_path.clone());
                let note = if self.settings.language.starts_with("zh") {
                    format!("已上传 {files} 个文件至 {remote_path}，可在底部终端运行")
                } else {
                    format!("Uploaded {files} file(s) to {remote_path}; run it in Terminal below")
                };
                self.log(note.clone());
                self.set_notice(note);
            }
            Response::RuntimeMetrics(metrics) => {
                if self.active_session.is_some() && !self.connected {
                    self.last_logged_metrics_error = None;
                    self.telemetry.push(&metrics);
                    self.runtime_metrics = Some(*metrics);
                }
            }
            Response::Status(status) => {
                let recovered = self.last_logged_connection_error.is_some();
                if !self.connected || recovered {
                    let msg = if status.gpu_name.is_empty() {
                        self.text("已连接远程服务", "Connected to runtime")
                            .to_owned()
                    } else {
                        format!(
                            "{} ({})",
                            self.text("已连接远程服务", "Connected to runtime"),
                            status.gpu_name
                        )
                    };
                    self.log(msg.clone());
                    if recovered {
                        self.set_notice(msg);
                    }
                }
                self.last_logged_connection_error = None;
                let previous = &self.last_console;
                let current = &status.console_lines;
                let overlap = (0..=previous.len().min(current.len()))
                    .rev()
                    .find(|&count| previous[previous.len() - count..] == current[..count])
                    .unwrap_or(0);
                for line in current.iter().skip(overlap) {
                    self.log(line.clone());
                }
                self.last_console = current.clone();
                self.telemetry.push(&status);
                if !status.comfyui.is_running
                    && !status.comfyui.is_starting
                    && !matches!(self.local_tunnel, LocalTunnelState::Off)
                {
                    self.stop_local_tunnel(false, cx);
                }
                self.server = Some(*status);
                self.connected = true;
                self.connection_error = None;
            }
            Response::Downloaded(path) => {
                if is_image(&path) {
                    self.preview = Some(path.clone());
                }
                if !self.local_outputs.contains(&path) {
                    self.local_outputs.push(path.clone());
                }
                let note = format!("{}: {}", self.text("已下载", "Downloaded"), path.display());
                self.log(note.clone());
                self.set_notice(note);
            }
            Response::Done(message) => {
                if !matches!(request, Request::Keepalive | Request::RuntimeMetrics) {
                    let clean = self.settings.redact(&message);
                    self.log(clean.clone());
                    self.set_notice(clean);
                }
                match request {
                    Request::ReleaseAll | Request::Action(ComfyAction::Disconnect) => {
                        self.stop_local_tunnel(false, cx);
                        self.active_session = None;
                        self.runtime_metrics = None;
                        if self.managed
                            || matches!(request, Request::Action(ComfyAction::Disconnect))
                        {
                            self.connected = false;
                            self.managed = false;
                            self.server = None;
                            self.poll_service = false;
                            self.telemetry.clear();
                            self.settings.bridge_managed = false;
                            self.settings.service_url.clear();
                            self.pending_fields.push(("service-url", String::new()));
                            self.save_settings_silent(cx);
                        }
                        if matches!(request, Request::ReleaseAll) {
                            self.account.sessions.clear();
                            self.last_uploaded_remote = None;
                        }
                        if self.quitting_after_release {
                            cx.quit();
                        }
                    }
                    Request::Logout => {
                        self.stop_local_tunnel(false, cx);
                        self.account = AccountStatus::default();
                        self.active_session = None;
                        self.runtime_metrics = None;
                        self.last_uploaded_remote = None;
                        self.auth_url = None;
                        self.show_manual_auth = false;
                        self.auth_cancel = None;
                        if self.managed {
                            self.connected = false;
                            self.managed = false;
                            self.server = None;
                            self.poll_service = false;
                            self.telemetry.clear();
                            self.settings.bridge_managed = false;
                            self.settings.service_url.clear();
                            self.pending_fields.push(("service-url", String::new()));
                            self.save_settings_silent(cx);
                        }
                    }
                    Request::Keepalive => {}
                    Request::Action(_) => self.request(Request::Status, cx),
                    _ => {}
                }
            }
        }
    }
}

fn pick_local_forward_port(preferred: u16) -> u16 {
    for port in preferred..=preferred.saturating_add(12) {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    preferred
}

fn ensure_ed25519_ssh_key() -> Result<(), String> {
    let home = dirs::home_dir()
        .ok_or_else(|| "Could not determine user home directory for ~/.ssh".to_owned())?;
    let ssh_dir = home.join(".ssh");
    let has_supported = ["id_ed25519.pub", "id_ecdsa.pub"]
        .iter()
        .any(|name| ssh_dir.join(name).is_file());
    if has_supported {
        return Ok(());
    }
    std::fs::create_dir_all(&ssh_dir).map_err(|e| format!("create ~/.ssh: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o700));
    }
    let key_path = ssh_dir.join("id_ed25519");
    let mut command = Command::new(crate::platform::openssh_program("ssh-keygen"));
    command
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(&key_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let status = command
        .status()
        .map_err(|e| format!("ssh-keygen not available: {e}"))?;
    if !status.success() {
        return Err(format!("ssh-keygen failed with {status}"));
    }
    Ok(())
}

fn quote_proxy_arg(value: &str) -> String {
    if cfg!(windows) {
        return quote_windows_arg(value);
    }
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b':'))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

// Win32 OpenSSH starts ProxyCommand with CreateProcess, without a shell.
// Match the CRT quoting used by Python's subprocess.list2cmdline.
fn quote_windows_arg(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        if character == '"' {
            result.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            result.extend(std::iter::repeat_n('\\', slashes));
        }
        result.push(character);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

fn ssh_proxy_command(python: &std::path::Path, session_name: &str) -> Result<String, String> {
    // OpenSSH parses ProxyCommand as a single config line before launching it.
    if session_name.contains(['\r', '\n', '\0']) {
        return Err("Invalid Colab session name".to_owned());
    }
    let python = python.to_string_lossy();
    if python.contains(['\r', '\n', '\0']) {
        return Err("Invalid Python executable path".to_owned());
    }
    let args = [
        python.as_ref(),
        "-m",
        "colab_cli.cli",
        "ssh",
        "--proxy-mode",
        "-s",
        session_name,
    ];
    Ok(args
        .iter()
        .map(|arg| quote_proxy_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
        // OpenSSH expands % tokens even inside quoted arguments.
        .replace('%', "%%"))
}

fn spawn_colab_ssh_forward(
    session_name: &str,
    local_port: u16,
    remote_port: u16,
    slot: &Arc<Mutex<Option<Child>>>,
) -> Result<Arc<Mutex<String>>, String> {
    ensure_ed25519_ssh_key()?;
    let python = crate::platform::python(&crate::platform::runtime_root());
    if !python.is_file() {
        return Err(format!(
            "Project Python environment is missing. Run {} first",
            crate::platform::python_setup_command()
        ));
    }
    let proxy_cmd = ssh_proxy_command(&python, session_name)?;
    let mut cmd = Command::new(crate::platform::openssh_program("ssh"));
    cmd.arg("-N")
        .arg("-L")
        .arg(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"))
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=20")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg(if cfg!(windows) {
            "UserKnownHostsFile=NUL"
        } else {
            "UserKnownHostsFile=/dev/null"
        })
        .arg("-o")
        .arg(format!("ProxyCommand={proxy_cmd}"))
        .arg("root@colab-runtime")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start ssh ({e}); ensure OpenSSH client is installed"))?;

    let stderr_buf = Arc::new(Mutex::new(String::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let capture = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 2048];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut buf) = capture.lock() {
                    buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if buf.len() > 8192 {
                        let mut keep_from = buf.len() - 4096;
                        while !buf.is_char_boundary(keep_from) {
                            keep_from += 1;
                        }
                        buf.drain(..keep_from);
                    }
                }
            }
        });
    }

    if let Ok(mut guard) = slot.lock() {
        *guard = Some(child);
    }

    let addr: SocketAddr = ([127, 0, 0, 1], local_port).into();
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        if let Ok(mut guard) = slot.lock() {
            match guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => {
                        guard.take();
                        let detail = stderr_buf
                            .lock()
                            .ok()
                            .map(|s| s.trim().to_owned())
                            .unwrap_or_default();
                        return Err(if detail.is_empty() {
                            format!("ssh exited early ({status})")
                        } else {
                            format!("ssh exited ({status}): {detail}")
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        guard.take();
                        return Err(format!("ssh status error: {e}"));
                    }
                },
                None => return Err("cancelled".to_owned()),
            }
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(stderr_buf);
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    if let Ok(mut guard) = slot.lock()
        && let Some(mut child) = guard.take()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
    let detail = stderr_buf
        .lock()
        .ok()
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    Err(if detail.is_empty() {
        "Timed out waiting for 127.0.0.1 port forward to open".to_owned()
    } else {
        format!("Timed out waiting for port forward: {detail}")
    })
}

fn utc_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub fn is_image(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "gif"
        )
    })
}

pub fn appearance(value: &str) -> Appearance {
    match value {
        "light" => Appearance::Light,
        "dark" => Appearance::Dark,
        _ => Appearance::System,
    }
}

pub fn run(mock: bool) {
    let loaded = if mock {
        Settings::load_from(&crate::settings::config_dir().join("demo/settings.json"))
    } else {
        Settings::load()
    };
    let (mut settings, error) = match loaded {
        Ok(settings) => (settings, None),
        Err(error) => (
            Settings::default(),
            Some(format!("无法加载设置 / Could not load settings: {error:#}")),
        ),
    };
    // Demo runtimes are in memory and do not survive closing the application.
    if mock {
        settings.service_url.clear();
        settings.bridge_managed = false;
    }
    if mock && settings.output_dir == Settings::default().output_dir {
        settings.output_dir = settings.output_dir.join("Demo");
    }
    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx| {
            if let Err(error) = ely_gpui_component::init(cx) {
                log::error!("{error:#}");
                cx.quit();
                return;
            }
            let state = AppState::new(settings, backend::create(mock), mock, error, cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("EasyColab GPUI".into()),
                    appears_transparent: true,
                    traffic_light_position: cfg!(target_os = "macos")
                        .then(|| cx.theme().traffic_light_origin()),
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1220.), px(820.)),
                    cx,
                ))),
                window_min_size: Some(size(px(940.), px(640.))),
                window_decorations: cfg!(any(target_os = "linux", target_os = "freebsd"))
                    .then_some(WindowDecorations::Client),
                window_background: if cfg!(any(target_os = "linux", target_os = "freebsd")) {
                    WindowBackgroundAppearance::Transparent
                } else {
                    WindowBackgroundAppearance::Opaque
                },
                app_id: Some("easycolab-gpui".into()),
                ..Default::default()
            };
            let state_for_window = state.clone();
            if let Err(error) = cx.open_window(options, move |window, cx| {
                cx.new(|cx| RootView::new(state_for_window, window, cx))
            }) {
                log::error!("Could not open window: {error:#}");
                cx.quit();
                return;
            }
            state.update(cx, |state, cx| state.start(cx));
            cx.activate(true);
        });
}

#[cfg(test)]
mod ssh_command_tests {
    use super::*;

    #[test]
    fn windows_proxy_arguments_preserve_spaces_unicode_quotes_and_backslashes() {
        assert_eq!(
            quote_windows_arg(r"C:\Program Files\中文\python.exe"),
            r#""C:\Program Files\中文\python.exe""#
        );
        assert_eq!(quote_windows_arg("session \"one\""), r#""session \"one\"""#);
        assert_eq!(quote_windows_arg("trailing\\"), "\"trailing\\\\\"");
        assert_eq!(quote_windows_arg(""), "\"\"");
    }

    #[test]
    fn proxy_command_preserves_literal_openssh_tokens_and_rejects_config_lines() {
        let command =
            ssh_proxy_command(std::path::Path::new("/bundle/python"), "session%h").unwrap();
        assert!(command.contains("session%%h"));
        assert!(command.contains("colab_cli.cli"));
        assert!(
            ssh_proxy_command(std::path::Path::new("/bundle/python"), "session\nHost evil")
                .is_err()
        );
    }
}
