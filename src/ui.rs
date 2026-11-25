use std::{collections::HashMap, path::PathBuf};

use ely_gpui_component::{
    buttons::{Button, ButtonVariant, CopyButton},
    charts::{LineChart, RealtimeChart, Series},
    data_display::{Badge, Tone},
    feedback::{InlineMessage, Toast, ToastViewport, Toaster},
    forms::{Choice, Input, Select, TextInput},
    layout::{AppShell, Card, CardHeader, Page, ScrollArea, Sidebar},
    motion::ProgressBar,
    navigation::NavItem,
    overlays::Dialog,
    primitives::{FocusScope, Icon, IconName, Severity, raise},
    settings::{Appearance, ThemeSelector},
    shell::{ResizeBorder, drag_region},
    terminal::{Launch, Terminal, TerminalEvent},
    theme::{ActiveTheme, ControlSize, IconSize, Mode, TextSize},
    typography::{Caption, Ellipsis, Paragraph, SelectableText},
};
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Decorations, Entity, FocusHandle,
    Focusable, FontFallbacks, FontWeight, InteractiveElement, IntoElement, MouseButton,
    ParentElement, PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, img, prelude::FluentBuilder as _, px,
};

use crate::{
    app::{
        AppState, Confirmation, DrawerTab, LifecycleStage, LocalTunnelState, appearance, is_image,
    },
    models::*,
};

#[derive(Clone)]
pub struct Fields(HashMap<&'static str, Entity<TextInput>>);

impl Fields {
    pub fn get(&self, id: &str) -> &Entity<TextInput> {
        self.0.get(id).expect("registered input")
    }
    fn text(&self, id: &str, cx: &App) -> String {
        self.get(id).read(cx).text().to_owned()
    }
    pub fn apply(&self, state: &mut AppState, cx: &mut Context<AppState>) -> Result<(), String> {
        let idle = self.text("idle", cx).trim().parse::<u32>().map_err(|_| {
            state
                .text(
                    "闲置时间必须是整数分钟",
                    "Idle timeout must be a whole number of minutes",
                )
                .to_string()
        })?;
        let service_url = self
            .text("service-url", cx)
            .trim()
            .trim_end_matches('/')
            .to_owned();
        if service_url != state.settings.service_url {
            state.service_generation += 1;
            state.settings.service_url = service_url;
        }
        state.settings.management_password = self.text("password", cx);
        state.settings.hf_token = self.text("hf-token", cx);
        state.settings.idle_timeout_minutes = idle;
        let directory = PathBuf::from(self.text("output-dir", cx));
        if directory != state.settings.output_dir {
            state.settings.output_dir = directory;
            state.scan_outputs(cx);
        }
        Ok(())
    }
}

pub struct RootView {
    pub state: Entity<AppState>,
    pub fields: Fields,
    pub toaster: Entity<Toaster>,
    focus: FocusHandle,
    font_fallbacks: Option<FontFallbacks>,
    terminal: Option<Entity<Terminal>>,
    terminal_error: Option<String>,
    last_terminal_open: bool,
    _terminal_sub: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = state.read(cx).settings.clone();
        let values = [
            ("service-url", settings.service_url, false),
            ("password", settings.management_password, true),
            ("hf-token", settings.hf_token, true),
            ("auth-code", String::new(), true),
            ("idle", settings.idle_timeout_minutes.to_string(), false),
            (
                "output-dir",
                settings.output_dir.display().to_string(),
                false,
            ),
            ("weight-source", String::new(), false),
            ("weight-filename", String::new(), false),
            ("weight-folder", "checkpoints".into(), false),
            ("project-local-path", String::new(), false),
            ("project-remote-dir", "project".into(), false),
        ];
        let fields = Fields(
            values
                .into_iter()
                .map(|(id, text, masked)| {
                    let field = cx.new(|cx| {
                        let input = TextInput::new(window, cx).label(id);
                        let mut input = if masked { input.masked() } else { input };
                        input.set_text(text, cx);
                        input
                    });
                    (id, field)
                })
                .collect(),
        );
        let focus = cx.focus_handle();
        let installed = cx.text_system().all_font_names();
        let families: Vec<String> = [
            "Noto Sans CJK SC",
            "Source Han Sans SC",
            "Noto Sans SC",
            "WenQuanYi Micro Hei",
            "Microsoft YaHei UI",
            "Microsoft YaHei",
            "PingFang SC",
            "Hiragino Sans GB",
        ]
        .into_iter()
        .filter(|family| installed.iter().any(|installed| installed == family))
        .map(str::to_owned)
        .collect();
        let font_fallbacks = (!families.is_empty()).then(|| FontFallbacks::from_fonts(families));
        window.focus(&focus, cx);
        let close_state = state.downgrade();
        ely_gpui_component::shell::guard_close(window, cx, move |_, cx| {
            let _ = close_state.update(cx, |state, cx| state.request_quit(cx));
            false
        });
        let toaster = cx.new(|_| Toaster::default());
        let subscriptions = vec![
            cx.observe(&state, |_, _, cx| cx.notify()),
            cx.observe(&toaster, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.state.update(cx, |state, cx| {
                    state.system_appearance_changed(Mode::from(window.appearance()), cx)
                });
            }),
        ];
        Self {
            state,
            fields,
            toaster,
            focus,
            font_fallbacks,
            terminal: None,
            terminal_error: None,
            last_terminal_open: false,
            _terminal_sub: None,
            _subscriptions: subscriptions,
        }
    }

    fn ensure_terminal(&mut self, cx: &mut Context<Self>) {
        if self.terminal.is_some() || self.terminal_error.is_some() {
            return;
        }
        let root = crate::platform::runtime_root();
        let venv = root.join(".venv");
        let venv_bin = crate::platform::venv_bin(&root);
        let mut env = Vec::new();
        if venv_bin.is_dir() {
            let mut paths = vec![venv_bin];
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                env.push(("PATH".to_owned(), joined.to_string_lossy().into_owned()));
            }
            env.push((
                "VIRTUAL_ENV".to_owned(),
                venv.to_string_lossy().into_owned(),
            ));
        }
        let launch = Launch {
            program: None,
            cwd: Some(root),
            env,
        };
        let mut spawn_error = None;
        let entity = cx.new(|cx| {
            // GPUI's deterministic scheduler cannot receive events from a
            // real PTY reader thread. UI tests exercise the same terminal view
            // with replay data; normal applications still launch a shell.
            if cx.is_test() {
                return Terminal::replay(b"", 80, 12, cx);
            }
            match Terminal::spawn(launch, cx) {
                Ok(term) => term,
                Err(error) => {
                    spawn_error = Some(format!("{error:#}"));
                    Terminal::replay(b"", 80, 12, cx)
                }
            }
        });
        if let Some(error) = spawn_error {
            self.terminal_error = Some(error);
            return;
        }
        self._terminal_sub = Some(cx.subscribe(&entity, |_, _, event: &TerminalEvent, cx| {
            if matches!(event, TerminalEvent::Title(_) | TerminalEvent::Exited(_)) {
                cx.notify();
            }
        }));
        self.terminal = Some(entity);
    }

    fn restart_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal = None;
        self.terminal_error = None;
        self._terminal_sub = None;
        self.ensure_terminal(cx);
        if let Some(term) = &self.terminal {
            let handle = term.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }
}

pub fn tagged(id: impl Into<SharedString>, child: impl IntoElement) -> gpui::Div {
    let id = id.into();
    div().debug_selector(move || id.to_string()).child(child)
}

fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    state: &Entity<AppState>,
    action: impl Fn(&mut AppState, &mut Context<AppState>) + 'static,
) -> AnyElement {
    let id = id.into();
    let handle = state.clone();
    tagged(
        id.clone(),
        Button::new(id, label)
            .size(ControlSize::Sm)
            .disabled(!enabled)
            .on_click(move |_, _, cx| handle.update(cx, |state, cx| action(state, cx))),
    )
    .into_any_element()
}

fn render_progress_bar(id: &'static str, value: Option<f32>) -> AnyElement {
    let bar = match value {
        Some(v) => ProgressBar::new(id, v.clamp(0.0, 1.0)),
        None => ProgressBar::indeterminate(id),
    };
    tagged(id, bar).into_any_element()
}

fn progress_caption(label: &str, value: Option<f32>) -> String {
    match value {
        Some(v) => format!("{} ({:.0}%)", label, v.clamp(0.0, 1.0) * 100.0),
        None => label.to_owned(),
    }
}

fn request_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    state: &Entity<AppState>,
    fields: &Fields,
    request: Request,
) -> AnyElement {
    let fields = fields.clone();
    button(id, label, enabled, state, move |state, cx| {
        match fields.apply(state, cx) {
            Ok(()) => state.request(request.clone(), cx),
            Err(error) => {
                state.set_error(error);
                cx.notify();
            }
        }
    })
}

fn field(id: &'static str, label: impl Into<SharedString>, fields: &Fields) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(Caption::new(label))
        .child(tagged(format!("field-{id}"), Input::new(fields.get(id))))
        .into_any_element()
}

fn row() -> gpui::Div {
    div().flex().flex_wrap().items_center().gap_2()
}

fn metric(label: &str, value: impl Into<SharedString>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .flex_1()
        .min_w(px(130.))
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(theme.colors.border)
        .child(Caption::new(label.to_owned()))
        .child(
            div()
                .text_size(theme.text_size(TextSize::Lg))
                .child(value.into()),
        )
        .into_any_element()
}

fn number(value: Option<f64>, suffix: &str) -> String {
    value.map_or_else(|| "—".into(), |value| format!("{value:.1}{suffix}"))
}

fn sidebar(state: &AppState, handle: &Entity<AppState>, _cx: &App) -> AnyElement {
    let routes = [
        (
            Route::Dashboard,
            "nav-dashboard",
            state.text("概览", "Overview"),
            IconName::LayoutDashboard,
        ),
        (
            Route::Deploy,
            "nav-deploy",
            state.text("部署中心", "Deploy"),
            IconName::Rocket,
        ),
        (
            Route::Workflows,
            "nav-workflows",
            state.text("工作流", "Workflows"),
            IconName::Layers,
        ),
        (
            Route::Outputs,
            "nav-outputs",
            state.text("输出文件", "Outputs"),
            IconName::Folder,
        ),
        (
            Route::Settings,
            "nav-settings",
            state.text("设置", "Settings"),
            IconName::Settings,
        ),
    ];
    let mut column =
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .child(div().px_2().pt_1().child(Caption::new(
                state.text("ComfyUI 创作工作台", "ComfyUI workspace"),
            )));
    if state.mock {
        column = column.child(tagged(
            "mock-badge",
            Badge::new(state.text("演示数据 · 不计费", "Demo data · no billing"))
                .tone(Tone::Warning),
        ));
    }
    let stage = state.lifecycle_stage();
    column = column
        .children(routes.into_iter().map(|(route, id, title, icon)| {
            let handle = handle.clone();
            let mut item = NavItem::new(id, icon, title)
                .active(state.route == route)
                .on_click(move |_, cx| handle.update(cx, |state, cx| state.navigate(route, cx)));
            match route {
                Route::Deploy => {
                    if !state.has_awake_runtime() {
                        item = item.count(state.text("需GPU", "GPU"));
                    } else if !state.connected {
                        item = item.count(state.text("下一步", "Next"));
                    }
                }
                Route::Workflows if !state.connected => {
                    item = item.count(state.text("待部署", "Lock"));
                }
                _ => {}
            }
            tagged(id, item)
        }))
        .child(div().flex_1())
        .child(Caption::new(if state.account.logged_in {
            state.account.email.clone()
        } else {
            state.text("尚未登录", "Not signed in").into()
        }))
        .child(
            Badge::new(match stage {
                LifecycleStage::ServiceReady => state.text("ComfyUI 在线", "Connected"),
                LifecycleStage::GpuReady => state.text("GPU 已就绪", "GPU ready"),
                LifecycleStage::NeedGpu if state.has_unverified_cloud_sessions() => {
                    state.text("待验证实例", "Unverified")
                }
                LifecycleStage::NeedGpu => state.text("待唤醒 GPU", "No GPU"),
                LifecycleStage::SignedOut => state.text("未登录", "Signed out"),
            })
            .tone(match stage {
                LifecycleStage::ServiceReady => Tone::Success,
                LifecycleStage::GpuReady => Tone::Info,
                LifecycleStage::NeedGpu if state.has_unverified_cloud_sessions() => Tone::Warning,
                LifecycleStage::NeedGpu | LifecycleStage::SignedOut => Tone::Neutral,
            }),
        );
    Sidebar::new("sidebar", false)
        .child(column)
        .into_any_element()
}

fn lifecycle_stepper(state: &AppState) -> AnyElement {
    let stage = state.lifecycle_stage();
    let waking = state.busy.contains("wake");
    let deploying = state.busy.contains("deploy");

    let step_badge = |label: &'static str, done: bool, active: bool| {
        Badge::new(label).tone(if done {
            Tone::Success
        } else if active {
            Tone::Info
        } else {
            Tone::Neutral
        })
    };

    let s1_done = state.account.logged_in || state.connected;
    let s2_done = state.has_awake_runtime();
    let s3_done = state.connected || state.last_uploaded_remote.is_some();
    let s4_done = state.connected && state.server.as_ref().is_some_and(|s| s.comfyui.is_running);

    let next_hint = match stage {
        LifecycleStage::SignedOut => state.text(
            "当前阶段：第 1 步 — 请先绑定 Google 账号（或手动连接已有的 HTTPS 服务）。",
            "Step 1 — Sign in with Google (or connect an existing HTTPS service).",
        ),
        LifecycleStage::NeedGpu if waking => state.text(
            "当前阶段：第 2 步 — 正在唤醒并校验 Colab GPU 实例，完成后将自动解锁部署中心。",
            "Step 2 — Waking and verifying your Colab GPU runtime…",
        ),
        LifecycleStage::NeedGpu if state.has_unverified_cloud_sessions() => state.text(
            "当前阶段：第 2 步 — 检测到账号已有云端实例，请先点击「验证并接管」或释放后重新唤醒。",
            "Step 2 — Existing cloud runtime detected; verify & attach or release before continuing.",
        ),
        LifecycleStage::NeedGpu => state.text(
            "当前阶段：第 2 步 — 选择显卡型号并点击「唤醒 GPU 实例」，验证可用后进入下一步。",
            "Step 2 — Pick a GPU and wake a runtime before deploying or uploading.",
        ),
        LifecycleStage::GpuReady if deploying => state.text(
            "当前阶段：第 3 步 — 正在已验证的 GPU 实例上部署 ComfyUI 服务端…",
            "Step 3 — Deploying ComfyUI on your verified GPU runtime…",
        ),
        LifecycleStage::GpuReady => state.text(
            "当前阶段：第 3 步 — GPU 实例已就绪！前往「部署中心」部署 ComfyUI，或上传自定义项目并在终端运行。",
            "Step 3 — GPU ready! Open Deploy to install ComfyUI or upload your own project.",
        ),
        LifecycleStage::ServiceReady => state.text(
            "当前阶段：第 4 步 — ComfyUI 服务已在线，可在「工作流」安装模型并在「输出文件」下载成片。",
            "Step 4 — ComfyUI is online. Install workflows or download outputs.",
        ),
    };

    tagged(
        "lifecycle-stepper",
        Card::new()
            .header(CardHeader::new(
                state.text("使用流程", "Workflow lifecycle"),
            ))
            .child(
                row()
                    .child(step_badge(
                        state.text("1. 绑定账号", "1. Sign in"),
                        s1_done,
                        stage == LifecycleStage::SignedOut,
                    ))
                    .child(Caption::new("→"))
                    .child(step_badge(
                        state.text("2. 唤醒 GPU", "2. Wake GPU"),
                        s2_done,
                        stage == LifecycleStage::NeedGpu,
                    ))
                    .child(Caption::new("→"))
                    .child(step_badge(
                        state.text("3. 部署 / 上传", "3. Deploy / Upload"),
                        s3_done,
                        stage == LifecycleStage::GpuReady,
                    ))
                    .child(Caption::new("→"))
                    .child(step_badge(
                        state.text("4. 创作与输出", "4. Create & Output"),
                        s4_done,
                        stage == LifecycleStage::ServiceReady && !s4_done,
                    )),
            )
            .child(Caption::new(next_hint)),
    )
    .into_any_element()
}

fn dashboard(state: &AppState, handle: &Entity<AppState>, fields: &Fields, cx: &App) -> Page {
    let enabled = !state.busy.contains("wake") && !state.busy.contains("release");
    let account = &state.account;
    let mut page = Page::new(state.text("概览", "Overview"))
        .subtitle(state.text(
            "按「绑定账号 → 唤醒 GPU → 部署 ComfyUI 或上传项目 → 创作管理」顺序使用，每一步均在前置条件就绪后解锁。",
            "Follow Sign in → Wake GPU → Deploy ComfyUI / Upload → Create. Each step unlocks once verified.",
        ))
        .child(lifecycle_stepper(state));
    page = page.child(
        row()
            .child(metric(
                state.text("计算单元余额", "Compute units"),
                number(account.paid_balance, " CU"),
                cx,
            ))
            .child(metric(
                state.text("消耗速率", "Burn rate"),
                number(
                    account
                        .burn_rate_hourly
                        .or_else(|| state.server.as_ref().and_then(|s| s.cu_burn_rate_per_hour)),
                    " CU/h",
                ),
                cx,
            ))
            .child(metric(
                "GPU",
                state
                    .telemetry_status()
                    .map(|s| s.gpu_name.clone())
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        state
                            .active_session
                            .as_ref()
                            .map(|s| s.accelerator.clone())
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or_else(|| "—".into()),
                cx,
            )),
    );
    let mut account_card = Card::new()
        .header(
            CardHeader::new(state.text("Google 账号", "Google account")).action(
                Badge::new(if account.logged_in {
                    state.text("已绑定", "Signed in")
                } else if state.busy.contains("auth") {
                    state.text("授权中", "Authorizing")
                } else {
                    state.text("未绑定", "Signed out")
                })
                .tone(if account.logged_in {
                    Tone::Success
                } else if state.busy.contains("auth") {
                    Tone::Info
                } else {
                    Tone::Neutral
                }),
            ),
        )
        .child(Paragraph::new(if account.logged_in {
            account.email.clone()
        } else {
            state
                .text(
                    "登录后可唤醒 Colab GPU。也可以连接已有的 HTTPS 服务。",
                    "Sign in to wake a Colab GPU, or connect an existing HTTPS service.",
                )
                .into()
        }));
    let auth_busy = state.busy.contains("auth");
    if account.logged_in {
        account_card = account_card.child(
            row()
                .child(request_button(
                    "refresh-account",
                    state.text("刷新余额", "Refresh balance"),
                    !state.busy.contains("auth-status"),
                    handle,
                    fields,
                    Request::AuthStatus,
                ))
                .child(request_button(
                    "switch-account",
                    state.text("换号", "Switch account"),
                    !auth_busy,
                    handle,
                    fields,
                    Request::AuthLogin,
                ))
                .child(request_button(
                    "logout",
                    state.text("退出登录", "Sign out"),
                    !auth_busy,
                    handle,
                    fields,
                    Request::Logout,
                )),
        );
        if !account.sessions.is_empty() {
            account_card = account_card.child(button(
                "release-account-runtimes",
                state.text("释放账号下全部实例…", "Release all account runtimes…"),
                enabled,
                handle,
                |state, cx| state.ask_release_all(cx),
            ));
        }
    } else {
        account_card = account_card.child(
            row()
                .child(request_button(
                    "login",
                    if auth_busy && !state.show_manual_auth {
                        state.text("等待浏览器授权…", "Waiting for browser…")
                    } else {
                        state.text("在浏览器中登录", "Sign in with browser")
                    },
                    !auth_busy,
                    handle,
                    fields,
                    Request::AuthLogin,
                ))
                .child(button(
                    "manual-auth-entry",
                    state.text("使用授权码登录", "Use auth code"),
                    !auth_busy,
                    handle,
                    |state, cx| state.switch_to_manual_auth(cx),
                )),
        );
    }
    if auth_busy && !state.show_manual_auth {
        let mut waiting_row = row().child(button(
            "cancel-auth",
            state.text("取消授权", "Cancel"),
            true,
            handle,
            |state, cx| state.cancel_auth(cx),
        ));
        if let Some(url) = &state.auth_url {
            waiting_row = waiting_row.child(CopyButton::new("copy-auth-url", url.clone()));
        }
        waiting_row = waiting_row.child(button(
            "switch-manual-auth",
            state.text("无法回跳？改用授权码", "Can't redirect? Use code"),
            true,
            handle,
            |state, cx| state.switch_to_manual_auth(cx),
        ));
        account_card = account_card
            .child(Caption::new(if account.logged_in {
                state.text(
                    "请在浏览器中选择新账号完成授权；取消将保留当前账号。",
                    "Authorize the new account in your browser; cancelling keeps your current account.",
                )
            } else {
                state.text(
                    "已在浏览器打开 Google 授权页，完成授权后将自动回跳登录。",
                    "Browser opened for Google sign-in; authorization completes automatically on redirect.",
                )
            }))
            .child(waiting_row);
    }
    if state.show_manual_auth {
        if let Some(url) = &state.auth_url {
            account_card = account_card
                .child(CopyButton::new("copy-auth-url", url.clone()))
                .child(Caption::new(state.text(
                    "完成授权后，粘贴授权码。",
                    "Paste the authorization code after signing in.",
                )));
        }
        account_card = account_card.child(field(
            "auth-code",
            state.text("授权码", "Authorization code"),
            fields,
        ));
        let input = fields.get("auth-code").clone();
        account_card = account_card.child(
            row()
                .child(button(
                    "submit-auth",
                    state.text("完成登录", "Complete sign in"),
                    !auth_busy,
                    handle,
                    move |state, cx| {
                        let code = input.read(cx).text().trim().to_owned();
                        if code.is_empty() {
                            let msg = state.text("请先填写授权码", "Enter an authorization code");
                            state.set_error(msg);
                            cx.notify();
                            return;
                        }
                        input.update(cx, |input, cx| input.set_text(String::new(), cx));
                        state.request(Request::AuthCode(code), cx);
                    },
                ))
                .child(button(
                    "hide-manual-auth",
                    state.text("收起", "Hide"),
                    true,
                    handle,
                    |state, cx| {
                        state.show_manual_auth = false;
                        state.auth_url = None;
                        cx.notify();
                    },
                )),
        );
    }
    struct GpuSpec {
        id: &'static str,
        label_zh: &'static str,
        label_en: &'static str,
        note_zh: &'static str,
        note_en: &'static str,
        detail_zh: &'static str,
        detail_en: &'static str,
    }

    const GPU_SPECS: [GpuSpec; 5] = [
        GpuSpec {
            id: "T4",
            label_zh: "T4 · 16GB 显存",
            label_en: "T4 · 16GB VRAM",
            note_zh: "High-RAM ~51GB · ~1.17 CU/h",
            note_en: "High-RAM ~51GB · ~1.17 CU/h",
            detail_zh: "NVIDIA T4 · 16 GB 显存 · 约 51 GB High-RAM 内存 · 约 1.17 CU/小时",
            detail_en: "NVIDIA T4 · 16 GB VRAM · ~51 GB High-RAM · ~1.17 CU/h",
        },
        GpuSpec {
            id: "L4",
            label_zh: "L4 · 24GB 显存",
            label_en: "L4 · 24GB VRAM",
            note_zh: "53GB 内存 · ~1.71 CU/h",
            note_en: "53GB RAM · ~1.71 CU/h",
            detail_zh: "NVIDIA L4 · 24 GB 显存 · 约 53 GB 内存 · 约 1.71 CU/小时",
            detail_en: "NVIDIA L4 · 24 GB VRAM · ~53 GB RAM · ~1.71 CU/h",
        },
        GpuSpec {
            id: "A100",
            label_zh: "A100 · 40/80GB 显存",
            label_en: "A100 · 40/80GB VRAM",
            note_zh: "High-RAM ~83.5GB · ~5.37 CU/h",
            note_en: "High-RAM ~83.5GB · ~5.37 CU/h",
            detail_zh: "NVIDIA A100 · 40/80 GB 显存 · 约 83.5 GB High-RAM 内存 · 约 5.37 CU/小时",
            detail_en: "NVIDIA A100 · 40/80 GB VRAM · ~83.5 GB High-RAM · ~5.37 CU/h",
        },
        GpuSpec {
            id: "H100",
            label_zh: "H100 · 80GB 显存",
            label_en: "H100 · 80GB VRAM",
            note_zh: "167GB 内存 · ~11.78 CU/h",
            note_en: "167GB RAM · ~11.78 CU/h",
            detail_zh: "NVIDIA H100 · 80 GB 显存 · 约 167 GB 内存 · 约 11.78 CU/小时",
            detail_en: "NVIDIA H100 · 80 GB VRAM · ~167 GB RAM · ~11.78 CU/h",
        },
        GpuSpec {
            id: "G4",
            label_zh: "G4 · PRO 6000 (96GB 显存)",
            label_en: "G4 · PRO 6000 (96GB VRAM)",
            note_zh: "176GB 内存 · ~10.05 CU/h",
            note_en: "176GB RAM · ~10.05 CU/h",
            detail_zh: "NVIDIA RTX PRO 6000 Blackwell · 96 GB 显存 · 176 GB 内存 · 约 10.05 CU/小时（推荐）",
            detail_en: "NVIDIA RTX PRO 6000 Blackwell · 96 GB VRAM · 176 GB RAM · ~10.05 CU/h (Recommended)",
        },
    ];

    fn gpu_spec(id: &str) -> &'static GpuSpec {
        GPU_SPECS
            .iter()
            .find(|spec| spec.id == id)
            .unwrap_or(&GPU_SPECS[4])
    }
    page = page.child(account_card);
    let fields_for_wake = fields.clone();
    let h = handle.clone();
    let selected_gpu = gpu_spec(&state.settings.gpu);
    let gpu = Select::new(
        "gpu-select",
        GPU_SPECS.iter().map(|spec| {
            Choice::new(spec.id, state.text(spec.label_zh, spec.label_en))
                .note(state.text(spec.note_zh, spec.note_en))
        }),
    )
    .label(state.text("GPU 型号", "GPU type"))
    .selected(state.settings.gpu.clone())
    .on_change(move |value, _, cx| {
        h.update(cx, |state, cx| {
            state.settings.gpu = value.to_string();
            state.save_settings(cx);
            cx.notify();
        })
    });
    let wake_busy = state.busy.contains("wake");
    let mut runtime_card = Card::new()
        .header(CardHeader::new(state.text("Colab 运行时", "Colab runtime")))
        .child(gpu)
        .child(tagged(
            "gpu-spec-detail",
            Caption::new(state.text(selected_gpu.detail_zh, selected_gpu.detail_en)),
        ))
        .child(
            row()
                .child(button(
                    "wake",
                    if wake_busy {
                        state.text("正在唤醒 GPU…", "Waking GPU…")
                    } else if state.has_unverified_cloud_sessions() {
                        state.text("验证并接管 GPU 实例", "Verify & attach GPU")
                    } else if state.has_awake_runtime() {
                        state.text("重新唤醒 / 切换 GPU", "Switch / re-wake GPU")
                    } else {
                        state.text("唤醒 GPU 实例", "Wake GPU runtime")
                    },
                    enabled && account.logged_in,
                    handle,
                    move |state, cx| match fields_for_wake.apply(state, cx) {
                        Ok(()) => state.ask_wake(cx),
                        Err(error) => {
                            state.set_error(error);
                            cx.notify();
                        }
                    },
                ))
                .child(button(
                    "release-all",
                    if state.managed || !state.connected {
                        state.text("释放全部 Colab 实例", "Release all Colab runtimes")
                    } else {
                        state.text("释放当前服务实例", "Release current runtime")
                    },
                    enabled && (state.has_awake_runtime() || !account.sessions.is_empty()),
                    handle,
                    |state, cx| state.ask_release(cx),
                )),
        );
    if wake_busy {
        let task = state.active_task.as_ref().filter(|t| t.key == "wake");
        let label = task
            .map(|t| t.label.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| state.text("正在唤醒 GPU 实例…", "Waking GPU runtime…"));
        runtime_card = runtime_card
            .child(render_progress_bar(
                "wake-progress",
                task.and_then(|t| t.value),
            ))
            .child(Caption::new(progress_caption(
                label,
                task.and_then(|t| t.value),
            )));
    }
    page = page.child(runtime_card.child(Caption::new(if !account.logged_in {
        state.text(
            "请先完成第 1 步（绑定 Google 账号），然后选择显卡型号并唤醒 GPU 实例。",
            "Complete Step 1 (sign in with Google) first, then pick a GPU and wake a runtime.",
        )
    } else {
        state.text(
            "唤醒仅分配并验证 GPU 实例，不会自动部署 ComfyUI。实例存在期间持续计费；关闭窗口不会结束计费。",
            "Waking allocates and verifies the GPU runtime without installing ComfyUI. Billing continues while a runtime exists.",
        )
    })));
    if let Some(session) = &state.active_session
        && !state.connected
    {
        let active_card = Card::new()
            .header(
                CardHeader::new(state.text("已唤醒实例", "Active GPU runtime")).action(
                    Badge::new(state.text("已验证就绪", "Verified ready")).tone(Tone::Success),
                ),
            )
            .child(Paragraph::new(state.text(
                "GPU 实例已唤醒并通过连通性校验，尚未部署 ComfyUI。现在可以进入「部署中心」部署 ComfyUI，或上传自己的项目在终端运行。",
                "GPU runtime is awake and verified without ComfyUI deployed yet. Open Deploy to install ComfyUI or upload your own project.",
            )))
            .child(Caption::new(format!(
                "• {} · {} · {} ({})",
                session.name, session.accelerator, session.shape, session.endpoint
            )))
            .child(button(
                "go-to-deploy",
                state.text("进入第 3 步：部署中心", "Step 3: Open Deploy"),
                true,
                handle,
                |state, cx| state.navigate(Route::Deploy, cx),
            ));
        page = page.child(tagged("awake-runtime-card", active_card));
    } else if state.has_unverified_cloud_sessions() {
        let mut unverified_card = Card::new()
            .header(
                CardHeader::new(state.text(
                    "检测到账号下已有云端实例",
                    "Existing cloud runtime detected",
                ))
                .action(Badge::new(state.text("未验证接管", "Unverified")).tone(Tone::Warning)),
            )
            .child(Paragraph::new(state.text(
                "Google Colab 返回了你账号下之前存在的实例，尚未在本会话验证内核是否可用。请先点击「验证并接管实例」，或先释放旧实例后再重新唤醒。",
                "Your account reports an existing Colab assignment that has not been verified in this session yet. Verify & attach it or release it before continuing.",
            )));
        for session in &state.account.sessions {
            unverified_card = unverified_card.child(Caption::new(format!(
                "• {} · {} · {} ({})",
                session.name, session.accelerator, session.shape, session.endpoint
            )));
        }
        let verify_fields = fields.clone();
        unverified_card = unverified_card.child(
            row()
                .child(button(
                    "verify-existing-runtime",
                    state.text("验证并接管实例", "Verify & attach"),
                    enabled && account.logged_in,
                    handle,
                    move |state, cx| match verify_fields.apply(state, cx) {
                        Ok(()) => state.ask_wake(cx),
                        Err(error) => {
                            state.set_error(error);
                            cx.notify();
                        }
                    },
                ))
                .child(button(
                    "release-unverified-runtimes",
                    state.text("释放旧实例", "Release old runtime"),
                    enabled,
                    handle,
                    |state, cx| state.ask_release_all(cx),
                )),
        );
        page = page.child(tagged("unverified-runtime-card", unverified_card));
    }
    let manual_fields = fields.clone();
    page = page.child(
        Card::new()
            .header(CardHeader::new(
                state.text("连接已有服务", "Connect an existing service"),
            ))
            .child(field("service-url", "HTTPS URL", fields))
            .child(
                row()
                    .child(button(
                        "connect",
                        state.text("连接", "Connect"),
                        enabled && !state.busy.contains("status"),
                        handle,
                        move |state, cx| match manual_fields.apply(state, cx) {
                            Ok(()) => {
                                state.service_generation += 1;
                                state.connected = false;
                                state.managed = false;
                                state.telemetry.clear();
                                state.settings.bridge_managed = false;
                                state.poll_service = true;
                                state.log(format!(
                                    "{}: {}",
                                    state
                                        .text("手动连接已有服务", "Connecting to existing service"),
                                    state.settings.service_url
                                ));
                                state.request(Request::Status, cx);
                                state.save_settings(cx);
                            }
                            Err(error) => {
                                state.set_error(error);
                                cx.notify();
                            }
                        },
                    ))
                    .child(button(
                        "disconnect",
                        state.text("释放当前服务实例", "Release current runtime"),
                        state.connected && !state.managed && enabled,
                        handle,
                        |state, cx| state.ask_release(cx),
                    )),
            ),
    );
    if let Some(telemetry_server) = state.telemetry_status() {
        page = page.child(resources(telemetry_server, state, handle, cx));
    }
    if state.connected || state.busy.contains("deploy") {
        page = page.child(comfy_card(state, handle, fields, cx));
    }
    page
}

fn pick_local_path(
    state: &mut AppState,
    directories: bool,
    prompt: &'static str,
    cx: &mut Context<AppState>,
) {
    let receiver = cx.prompt_for_paths(PathPromptOptions {
        files: !directories,
        directories,
        multiple: false,
        prompt: Some(prompt.into()),
    });
    let _ = state;
    cx.spawn(async move |this, cx| {
        if let Ok(Ok(Some(paths))) = receiver.await
            && let Some(path) = paths.into_iter().next()
        {
            let _ = this.update(cx, |state, cx| {
                state
                    .pending_fields
                    .push(("project-local-path", path.display().to_string()));
                cx.notify();
            });
        }
    })
    .detach();
}

fn deploy_page(state: &AppState, handle: &Entity<AppState>, fields: &Fields, _cx: &App) -> Page {
    let mut page = Page::new(state.text("部署中心", "Deploy")).subtitle(state.text(
        "在已验证可用的 Colab GPU 实例上按需部署 ComfyUI，或上传本地项目到云端并在终端运行。",
        "Deploy ComfyUI on your verified Colab GPU, or upload a local project and run it in Terminal.",
    ));
    if !state.has_awake_runtime() {
        let (title, badge, body, btn_label) = if !state.account.logged_in {
            (
                state.text("第 1 步尚未完成：请先绑定 Google 账号", "Step 1 required: Sign in with Google"),
                state.text("未登录", "Signed out"),
                state.text(
                    "必须先在「概览」页面绑定 Google 账号并唤醒 GPU 实例，验证实例可用后才会解锁部署 ComfyUI 与项目上传功能。",
                    "Sign in with Google and wake a GPU runtime on Overview first. Deploy and project upload unlock once the GPU is verified.",
                ),
                state.text("前往概览绑定账号", "Go to Overview"),
            )
        } else if state.has_unverified_cloud_sessions() {
            (
                state.text("第 2 步尚未完成：需先验证并接管 GPU 实例", "Step 2 required: Verify GPU runtime"),
                state.text("未验证", "Unverified"),
                state.text(
                    "检测到你的账号下存在历史云端实例，但尚未完成连通性与内核验证。请先返回「概览」点击「验证并接管实例」或重新唤醒 GPU。",
                    "An existing cloud assignment was detected on your account, but it has not been verified yet. Return to Overview to verify & attach or wake a fresh GPU.",
                ),
                state.text("前往概览验证 GPU", "Go to Overview"),
            )
        } else {
            (
                state.text("第 2 步尚未完成：请先唤醒 GPU 实例", "Step 2 required: Wake a GPU runtime"),
                state.text("未唤醒", "No GPU"),
                state.text(
                    "当前尚未唤醒可用的 GPU 实例。请先在「概览」页面选择显卡型号并唤醒 GPU，等待实例校验就绪后即可部署 ComfyUI 或上传项目。",
                    "No verified GPU runtime is active yet. Return to Overview, select a GPU, and wake it before deploying ComfyUI or uploading a project.",
                ),
                state.text("前往概览唤醒 GPU", "Go to Overview"),
            )
        };
        return page.child(tagged(
            "deploy-gate-card",
            Card::new()
                .header(CardHeader::new(title).action(Badge::new(badge).tone(Tone::Warning)))
                .child(Paragraph::new(body))
                .child(button(
                    "deploy-go-overview",
                    btn_label,
                    true,
                    handle,
                    |state, cx| state.navigate(Route::Dashboard, cx),
                )),
        ));
    }

    if let Some(session) = &state.active_session {
        let mut runtime_info = Card::new()
            .header(
                CardHeader::new(state.text("当前已验证 GPU 实例", "Verified GPU runtime")).action(
                    Badge::new(format!("{} · {}", session.accelerator, session.name))
                        .tone(Tone::Success),
                ),
            )
            .child(Caption::new(format!(
                "{}: {} · {}",
                state.text("会话节点", "Runtime endpoint"),
                session.endpoint,
                session.shape
            )));
        if let Some(metrics) = state.telemetry_status() {
            let mut summary = row();
            if let Some(v) = metric_stats(
                metrics.vram_used_gb,
                metrics.vram_free_gb,
                metrics.vram_total_gb,
            ) {
                summary = summary.child(
                    Badge::new(format!(
                        "{} {:.1}/{:.1}G ({:.0}%)",
                        state.text("显存", "VRAM"),
                        v.used,
                        v.total,
                        v.pct
                    ))
                    .tone(threshold_tone(v.free, 8.0, 3.0, Tone::Info)),
                );
            }
            if let Some(r) = metric_stats(
                metrics.ram_used_gb,
                metrics.ram_free_gb,
                metrics.ram_total_gb,
            ) {
                summary = summary.child(
                    Badge::new(format!(
                        "{} {:.1}/{:.1}G ({:.0}%)",
                        state.text("内存", "RAM"),
                        r.used,
                        r.total,
                        r.pct
                    ))
                    .tone(threshold_tone(r.free, 10.0, 5.0, Tone::Success)),
                );
            }
            if let Some(d) = metric_stats(
                metrics.disk_used_gb,
                metrics.disk_free_gb,
                metrics.disk_total_gb,
            ) {
                summary = summary.child(
                    Badge::new(format!(
                        "{} {:.1}/{:.1}G ({:.0}%)",
                        state.text("磁盘", "Disk"),
                        d.used,
                        d.total,
                        d.pct
                    ))
                    .tone(threshold_tone(d.free, 20.0, 10.0, Tone::Info)),
                );
            }
            runtime_info = runtime_info.child(tagged("deploy-runtime-metrics", summary));
        }
        page = page.child(runtime_info);
    }

    let deploy_busy = state.busy.contains("deploy");
    let upload_busy = state.busy.contains("upload");
    let wake_or_release = state.busy.contains("wake") || state.busy.contains("release");
    let deploy_fields = fields.clone();
    let mut comfy_card = Card::new()
        .header(
            CardHeader::new(state.text("部署 ComfyUI", "Deploy ComfyUI")).action(
                Badge::new(if state.connected {
                    state.text("已部署", "Deployed")
                } else if deploy_busy {
                    state.text("部署中", "Deploying")
                } else {
                    state.text("未部署", "Not deployed")
                })
                .tone(if state.connected {
                    Tone::Success
                } else if deploy_busy {
                    Tone::Info
                } else {
                    Tone::Neutral
                }),
            ),
        )
        .child(Paragraph::new(state.text(
            "安装并启动 ComfyUI 服务端与公网隧道。部署完成后即可使用节点画布、工作流安装与输出下载。",
            "Install and start the ComfyUI server and tunnel. Once ready, use the canvas, workflows, and outputs.",
        )))
        .child(field(
            "password",
            state.text("管理密码（可选，留空自动生成）", "Management password (auto-generated if empty)"),
            fields,
        ))
        .child(
            row()
                .child(button(
                    "deploy-comfyui",
                    if deploy_busy {
                        state.text("正在部署 ComfyUI…", "Deploying ComfyUI…")
                    } else if state.connected {
                        state.text("重新部署 ComfyUI", "Redeploy ComfyUI")
                    } else {
                        state.text("部署 ComfyUI", "Deploy ComfyUI")
                    },
                    !deploy_busy && !wake_or_release,
                    handle,
                    move |state, cx| match deploy_fields.apply(state, cx) {
                        Ok(()) => state.start_deploy(cx),
                        Err(error) => {
                            state.error = Some(error);
                            cx.notify();
                        }
                    },
                ))
                .child(button(
                    "deploy-open-workflows",
                    state.text("前往工作流", "Open Workflows"),
                    state.connected,
                    handle,
                    |state, cx| state.navigate(Route::Workflows, cx),
                )),
        );
    if deploy_busy {
        let task = state.active_task.as_ref().filter(|t| t.key == "deploy");
        let label = task
            .map(|t| t.label.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| state.text("正在部署 ComfyUI…", "Deploying ComfyUI…"));
        comfy_card = comfy_card
            .child(render_progress_bar(
                "deploy-progress",
                task.and_then(|t| t.value),
            ))
            .child(Caption::new(progress_caption(
                label,
                task.and_then(|t| t.value),
            )));
    }
    page = page.child(tagged("deploy-comfy-card", comfy_card));

    let upload_fields = fields.clone();
    let dir_prompt = state.text("选择要上传的项目目录", "Choose project folder to upload");
    let file_prompt = state.text("选择要上传的文件", "Choose file to upload");
    let target_handle = handle.clone();
    let upload_target = state.upload_target;
    let upload_target_select = Select::new(
        "upload-target-select",
        [
            Choice::new(
                "runtime",
                state.text("实例临时目录 (/content/)", "Runtime disk (/content/)"),
            )
            .note(state.text(
                "读写最快 · 实例释放后清空",
                "Fastest I/O · cleared when runtime stops",
            )),
            Choice::new(
                "drive",
                state.text(
                    "Google Drive 云盘 (/content/drive/MyDrive/)",
                    "Google Drive (/content/drive/MyDrive/)",
                ),
            )
            .note(state.text(
                "持久化保存 · 上传前自动校验真实挂载",
                "Persistent · verifies real mount before upload",
            )),
        ],
    )
    .label(state.text("上传目标位置", "Upload destination"))
    .selected(upload_target.as_arg())
    .on_change(move |value, _, cx| {
        target_handle.update(cx, |state, cx| {
            state.upload_target = if value.as_ref() == "drive" {
                UploadTarget::Drive
            } else {
                UploadTarget::Runtime
            };
            cx.notify();
        });
    });
    let drive_stats = state
        .telemetry_status()
        .and_then(|s| metric_stats(s.drive_used_gb, s.drive_free_gb, s.drive_total_gb));
    let mut upload_card = Card::new()
        .header(
            CardHeader::new(state.text("上传本地项目到云端", "Upload local project")).action(
                Badge::new(if upload_busy {
                    state.text("上传中", "Uploading")
                } else if upload_target == UploadTarget::Drive {
                    state.text("Google Drive", "Google Drive")
                } else {
                    state.text("自定义项目", "Custom project")
                })
                .tone(if upload_busy {
                    Tone::Info
                } else if upload_target == UploadTarget::Drive {
                    if drive_stats.is_some() {
                        Tone::Success
                    } else {
                        Tone::Warning
                    }
                } else {
                    Tone::Neutral
                }),
            ),
        )
        .child(Paragraph::new(state.text(
            "将本地文件或项目目录打包上传至云端（自动跳过 .git、.venv、node_modules、target、__pycache__）。可选择写入实例临时磁盘或已挂载的 Google Drive 云盘。",
            "Upload a local file or folder to the cloud (skips .git, .venv, node_modules, target, __pycache__). Choose ephemeral runtime storage or mounted Google Drive.",
        )))
        .child(tagged("upload-target", upload_target_select));
    if upload_target == UploadTarget::Drive {
        upload_card = upload_card.child(tagged(
            "drive-mount-status",
            match drive_stats {
                Some(drive) => Badge::new(if state.settings.language.starts_with("zh") {
                    format!(
                        "Drive 已真实挂载 · 可用 {:.1} / {:.0} GB",
                        drive.free, drive.total
                    )
                } else {
                    format!(
                        "Drive mounted · {:.1} / {:.0} GB free",
                        drive.free, drive.total
                    )
                })
                .tone(Tone::Success),
                None => Badge::new(state.text(
                    "尚未检测到 Drive 挂载 · 上传前将自动校验，未挂载会中止以防误写入临时磁盘",
                    "Drive mount not detected yet · verified before upload; aborts if unmounted",
                ))
                .tone(Tone::Warning),
            },
        ));
    }
    upload_card = upload_card
        .child(field(
            "project-local-path",
            state.text("本地文件或项目目录", "Local file or folder"),
            fields,
        ))
        .child(
            row()
                .child(button(
                    "pick-project-dir",
                    state.text("选择目录…", "Choose folder…"),
                    !upload_busy,
                    handle,
                    move |state, cx| pick_local_path(state, true, dir_prompt, cx),
                ))
                .child(button(
                    "pick-project-file",
                    state.text("选择文件…", "Choose file…"),
                    !upload_busy,
                    handle,
                    move |state, cx| pick_local_path(state, false, file_prompt, cx),
                )),
        )
        .child(field(
            "project-remote-dir",
            if upload_target == UploadTarget::Drive {
                state.text(
                    "云盘目标子目录（/content/drive/MyDrive/ 下）",
                    "Drive subfolder (under /content/drive/MyDrive/)",
                )
            } else {
                state.text(
                    "云端目标目录（/content/ 下）",
                    "Remote folder (under /content/)",
                )
            },
            fields,
        ))
        .child(button(
            "upload-project",
            if upload_busy {
                state.text("正在上传…", "Uploading…")
            } else if upload_target == UploadTarget::Drive {
                state.text("校验挂载并上传到 Drive", "Verify mount & upload to Drive")
            } else {
                state.text("上传到云端", "Upload to cloud")
            },
            !upload_busy && !wake_or_release,
            handle,
            move |state, cx| {
                let local_raw = upload_fields
                    .text("project-local-path", cx)
                    .trim()
                    .to_owned();
                if local_raw.is_empty() {
                    let msg = state.text("请先选择或填写本地路径", "Select or enter a local path");
                    state.set_error(msg);
                    cx.notify();
                    return;
                }
                let remote_dir = upload_fields
                    .text("project-remote-dir", cx)
                    .trim()
                    .to_owned();
                let target = state.upload_target;
                state.request(
                    Request::UploadProject {
                        local_path: PathBuf::from(local_raw),
                        remote_dir: if remote_dir.is_empty() {
                            "project".into()
                        } else {
                            remote_dir
                        },
                        target,
                    },
                    cx,
                );
            },
        ));
    if upload_busy {
        let task = state.active_task.as_ref().filter(|t| t.key == "upload");
        let label = task
            .map(|t| t.label.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| state.text("正在上传项目至云端…", "Uploading project…"));
        upload_card = upload_card
            .child(render_progress_bar(
                "upload-progress",
                task.and_then(|t| t.value),
            ))
            .child(Caption::new(progress_caption(
                label,
                task.and_then(|t| t.value),
            )));
    }
    if let Some(remote_path) = &state.last_uploaded_remote {
        upload_card = upload_card.child(Caption::new(format!(
            "{}: {remote_path}",
            state.text("最近上传目录", "Last uploaded to")
        )));
    }
    page = page.child(tagged("upload-project-card", upload_card));

    let session_name = state.active_session_name().to_owned();
    let remote_hint = state
        .last_uploaded_remote
        .clone()
        .unwrap_or_else(|| "/content/project".into());
    let ssh_cmd = format!("colab ssh -s {session_name}");
    let exec_cmd = format!("colab ssh -s {session_name} -- ls -la {remote_hint}");
    let terminal_card = Card::new()
        .header(CardHeader::new(state.text(
            "在终端运行自定义项目",
            "Run your project in Terminal",
        )))
        .child(Paragraph::new(state.text(
            "项目上传后，可打开底部终端直接使用 colab 命令连接实例、安装依赖或启动脚本。",
            "After uploading, open the bottom Terminal and use colab commands to connect, install dependencies, or launch scripts.",
        )))
        .child(Caption::new(format!(
            "{}: {session_name}  ·  {ssh_cmd}",
            state.text("当前会话", "Session")
        )))
        .child(
            row()
                .child(button(
                    "open-deploy-terminal",
                    state.text("打开底部终端", "Open Terminal"),
                    true,
                    handle,
                    |state, cx| {
                        state.drawer_tab = DrawerTab::Terminal;
                        state.logs_open = true;
                        cx.notify();
                    },
                ))
                .child(CopyButton::new("copy-colab-ssh", ssh_cmd))
                .child(CopyButton::new("copy-colab-exec", exec_cmd)),
        )
        .child(Caption::new(state.text(
            "两个复制按钮依次复制：进入云端交互终端 (colab ssh -s)、查看云端项目目录 (colab ssh -s … -- ls -la)。",
            "The copy buttons copy an interactive shell command (colab ssh -s) and a remote directory listing command.",
        )));
    page.child(tagged("terminal-guide-card", terminal_card))
}

struct MetricStats {
    used: f64,
    free: f64,
    total: f64,
    pct: f64,
}

fn metric_stats(used: Option<f64>, free: Option<f64>, total: Option<f64>) -> Option<MetricStats> {
    let total = total.filter(|t| *t > 0.0 && t.is_finite())?;
    let used = used
        .or_else(|| free.map(|f| (total - f).max(0.0)))
        .unwrap_or(0.0);
    let free = free.unwrap_or_else(|| (total - used).max(0.0));
    let pct = ((used / total) * 100.0).clamp(0.0, 100.0);
    Some(MetricStats {
        used,
        free,
        total,
        pct,
    })
}

fn threshold_tone(free: f64, warn_below: f64, crit_below: f64, normal: Tone) -> Tone {
    if free < crit_below {
        Tone::Danger
    } else if free < warn_below {
        Tone::Warning
    } else {
        normal
    }
}

fn series_or_seed(history: &[f64], fallback_pct: f64, len: usize) -> Vec<f64> {
    if history.is_empty() {
        vec![fallback_pct; len.max(14)]
    } else if history.len() < len {
        let mut out = vec![history[0]; len - history.len()];
        out.extend_from_slice(history);
        out
    } else {
        history[history.len() - len..].to_vec()
    }
}

fn telemetry_metric_card(
    id: &'static str,
    title: &str,
    stats: &MetricStats,
    history: &[f64],
    tone: Tone,
    state: &AppState,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let values = if history.is_empty() {
        vec![stats.pct; 14]
    } else {
        history.to_vec()
    };
    let subtitle = if state.settings.language.starts_with("zh") {
        format!(
            "{:.1} / {:.1} GB · 余 {:.1} GB",
            stats.used, stats.total, stats.free
        )
    } else {
        format!(
            "{:.1} / {:.1} GB · {:.1} GB free",
            stats.used, stats.total, stats.free
        )
    };
    div()
        .flex_1()
        .min_w(px(230.))
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.colors.border)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title.to_owned()),
                )
                .child(Badge::new(format!("{:.0}%", stats.pct)).tone(tone)),
        )
        .child(Caption::new(subtitle))
        .child(tagged(
            id,
            RealtimeChart::new(id, values, state.telemetry.pushed).h(px(96.)),
        ))
        .into_any_element()
}

fn resources(server: &ServerStatus, state: &AppState, handle: &Entity<AppState>, cx: &App) -> Card {
    let used = |a, b| format!("{} / {}", number(a, ""), number(b, " GB"));
    let vram = metric_stats(
        server.vram_used_gb,
        server.vram_free_gb,
        server.vram_total_gb,
    );
    let ram = metric_stats(server.ram_used_gb, server.ram_free_gb, server.ram_total_gb);
    let disk = metric_stats(
        server.disk_used_gb,
        server.disk_free_gb,
        server.disk_total_gb,
    );
    let has_telemetry = vram.is_some() || ram.is_some() || disk.is_some();

    let mut header = CardHeader::new(if state.connected {
        state.text("资源与队列", "Resources and queue")
    } else {
        state.text(
            "GPU 实例资源监控（内核直连）",
            "GPU runtime telemetry (kernel)",
        )
    });
    if has_telemetry {
        let mut actions = row();
        if !state.connected {
            actions = actions.child(button(
                "refresh-runtime-metrics",
                state.text("刷新监控", "Refresh"),
                !state.busy.contains("runtime-metrics")
                    && !state.busy.contains("wake")
                    && !state.busy.contains("deploy")
                    && !state.busy.contains("upload"),
                handle,
                |state, cx| state.request(Request::RuntimeMetrics, cx),
            ));
        }
        actions = actions.child(button(
            "telemetry-mode-toggle",
            if state.settings.combined_telemetry_chart {
                state.text("分项图表", "Split")
            } else {
                state.text("同屏对比", "Overlay")
            },
            true,
            handle,
            |state, cx| state.toggle_combined_telemetry(cx),
        ));
        header = header.action(actions);
    }
    let mut card = Card::new().header(header);

    if has_telemetry {
        if state.settings.combined_telemetry_chart {
            let count = state
                .telemetry
                .vram_pct
                .len()
                .max(state.telemetry.ram_pct.len())
                .max(state.telemetry.disk_pct.len())
                .max(14);
            let labels: Vec<String> = (0..count)
                .map(|ix| {
                    let ago = (count - 1 - ix) * 3;
                    if ago == 0 {
                        state.text("现在", "now").to_owned()
                    } else {
                        format!("-{ago}s")
                    }
                })
                .collect();
            let mut chart = LineChart::new("telemetry-combined", labels)
                .smooth()
                .format(|v| format!("{v:.0}%"));
            let mut summary = row();
            if let Some(stats) = &vram {
                summary = summary.child(
                    Badge::new(format!(
                        "{} {:.1}/{:.1}G ({:.0}%)",
                        state.text("显存", "VRAM"),
                        stats.used,
                        stats.total,
                        stats.pct
                    ))
                    .tone(threshold_tone(stats.free, 8.0, 3.0, Tone::Info)),
                );
                chart = chart.series(Series::new(
                    state.text("显存 VRAM", "VRAM"),
                    series_or_seed(&state.telemetry.vram_pct, stats.pct, count),
                ));
            }
            if let Some(stats) = &ram {
                summary = summary.child(
                    Badge::new(format!(
                        "{} {:.1}/{:.1}G ({:.0}%)",
                        state.text("内存", "RAM"),
                        stats.used,
                        stats.total,
                        stats.pct
                    ))
                    .tone(threshold_tone(
                        stats.free,
                        10.0,
                        5.0,
                        Tone::Success,
                    )),
                );
                chart = chart.series(Series::new(
                    state.text("内存 RAM", "RAM"),
                    series_or_seed(&state.telemetry.ram_pct, stats.pct, count),
                ));
            }
            if let Some(stats) = &disk {
                summary = summary.child(
                    Badge::new(format!(
                        "{} {:.1}/{:.1}G ({:.0}%)",
                        state.text("磁盘", "Disk"),
                        stats.used,
                        stats.total,
                        stats.pct
                    ))
                    .tone(threshold_tone(stats.free, 20.0, 10.0, Tone::Info)),
                );
                chart = chart.series(Series::new(
                    state.text("磁盘 Disk", "Disk"),
                    series_or_seed(&state.telemetry.disk_pct, stats.pct, count),
                ));
            }
            card = card
                .child(summary)
                .child(tagged("telemetry-combined-chart", chart.h(px(210.))));
        } else {
            let mut charts_row = div().flex().flex_wrap().gap_3();
            if let Some(stats) = &vram {
                charts_row = charts_row.child(telemetry_metric_card(
                    "telemetry-vram-chart",
                    state.text("显存 VRAM", "VRAM"),
                    stats,
                    &state.telemetry.vram_pct,
                    threshold_tone(stats.free, 8.0, 3.0, Tone::Info),
                    state,
                    cx,
                ));
            }
            if let Some(stats) = &ram {
                charts_row = charts_row.child(telemetry_metric_card(
                    "telemetry-ram-chart",
                    state.text("内存 RAM", "System RAM"),
                    stats,
                    &state.telemetry.ram_pct,
                    threshold_tone(stats.free, 10.0, 5.0, Tone::Success),
                    state,
                    cx,
                ));
            }
            if let Some(stats) = &disk {
                charts_row = charts_row.child(telemetry_metric_card(
                    "telemetry-disk-chart",
                    state.text("实例磁盘 Disk", "Runtime Disk"),
                    stats,
                    &state.telemetry.disk_pct,
                    threshold_tone(stats.free, 20.0, 10.0, Tone::Info),
                    state,
                    cx,
                ));
            }
            card = card.child(tagged("telemetry-split-charts", charts_row));
        }

        let shm = metric_stats(server.shm_used_gb, server.shm_free_gb, server.shm_total_gb)
            .filter(|s| s.total >= 1.0);
        let drive = metric_stats(
            server.drive_used_gb,
            server.drive_free_gb,
            server.drive_total_gb,
        );
        if shm.is_some() || drive.is_some() {
            let mut footer = row();
            if let Some(shm) = shm {
                footer = footer.child(Caption::new(if state.settings.language.starts_with("zh") {
                    format!(
                        "内存盘 (/dev/shm) 可用 {:.1} / {:.0} GB",
                        shm.free, shm.total
                    )
                } else {
                    format!("/dev/shm Free {:.1} / {:.0} GB", shm.free, shm.total)
                }));
            }
            if let Some(drive) = drive {
                footer = footer.child(Caption::new(if state.settings.language.starts_with("zh") {
                    format!("云盘可用 {:.1} GB", drive.free)
                } else {
                    format!("Drive Free {:.1} GB", drive.free)
                }));
            }
            card = card.child(tagged("telemetry-storage-footer", footer));
        }
    }

    if state.connected {
        card.child(
            row()
                .child(metric(
                    "Drive",
                    used(server.drive_used_gb, server.drive_total_gb),
                    cx,
                ))
                .child(metric(
                    state.text("正在运行 / 等待", "Running / pending"),
                    format!(
                        "{} / {}",
                        server.comfyui.queue_running, server.comfyui.queue_pending
                    ),
                    cx,
                ))
                .child(metric(
                    state.text("闲置剩余时间", "Idle time remaining"),
                    if server.idle_remaining_seconds < 0 {
                        state.text("未启用", "Disabled").to_owned()
                    } else {
                        format!("{} min", server.idle_remaining_seconds / 60)
                    },
                    cx,
                )),
        )
    } else {
        card.child(Caption::new(state.text(
            "直接通过已唤醒 Colab 内核实时采集 nvidia-smi / 内存 / 磁盘数据，无需部署 ComfyUI 即可持续监控自定义项目资源占用。",
            "Sampled directly from the awake Colab kernel (nvidia-smi / RAM / disk) so you can monitor custom projects without deploying ComfyUI.",
        )))
    }
}

fn comfy_card(state: &AppState, handle: &Entity<AppState>, fields: &Fields, _cx: &App) -> Card {
    let comfy = state.server.as_ref().map(|s| &s.comfyui);
    let running = comfy.is_some_and(|c| c.is_running);
    let busy = comfy.is_some_and(ComfyStatus::busy) || state.busy.contains("comfy-lifecycle");
    let ready =
        state.connected && !busy && !state.busy.contains("release") && !state.busy.contains("wake");
    let status = comfy
        .map(|c| c.status_message.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| state.text("尚未启动", "Not started").into());
    let mut card = Card::new()
        .header(
            CardHeader::new("ComfyUI").action(
                Badge::new(if running {
                    state.text("运行中", "Running")
                } else if busy {
                    state.text("处理中", "Working")
                } else {
                    state.text("已停止", "Stopped")
                })
                .tone(if running { Tone::Success } else { Tone::Info }),
            ),
        )
        .child(Paragraph::new(status))
        .child(
            row()
                .child(request_button(
                    "comfy-start",
                    state.text("启动", "Start"),
                    ready && !running,
                    handle,
                    fields,
                    Request::Action(ComfyAction::Start {
                        force_restart: false,
                    }),
                ))
                .child(request_button(
                    "comfy-stop",
                    state.text("停止", "Stop"),
                    ready && running,
                    handle,
                    fields,
                    Request::Action(ComfyAction::Stop),
                ))
                .child(request_button(
                    "comfy-restart",
                    state.text("重启", "Restart"),
                    ready,
                    handle,
                    fields,
                    Request::Action(ComfyAction::Start {
                        force_restart: true,
                    }),
                ))
                .child(request_button(
                    "free-vram",
                    state.text("释放显存", "Free VRAM"),
                    ready,
                    handle,
                    fields,
                    Request::Action(ComfyAction::FreeVram),
                )),
        );
    if let Some(comfy) = comfy {
        if busy {
            let progress = comfy.progress.clamp(0., 1.) as f32;
            card = card
                .child(tagged(
                    "comfy-progress",
                    ProgressBar::new("comfy-progress", progress),
                ))
                .child(Caption::new(format!("{:.0}%", progress * 100.)));
        }
        if !comfy.public_url.is_empty() {
            let url = comfy.public_url.clone();
            let open_url = url.clone();
            card = card
                .child(
                    row()
                        .child(button(
                            "open-comfy",
                            state.text("打开 ComfyUI（公网）", "Open ComfyUI (public)"),
                            running,
                            handle,
                            move |_, cx| cx.open_url(&open_url),
                        ))
                        .child(CopyButton::new("copy-comfy-url", url))
                        .child(CopyButton::new(
                            "copy-password",
                            state.settings.management_password.clone(),
                        )),
                )
                .child(Caption::new(state.text(
                    "公网访问用户名: easycolab；复制按钮依次复制公网地址、管理密码。",
                    "Public Basic Auth username: easycolab. Copy buttons copy URL and management password.",
                )));
        }
        if running || !matches!(state.local_tunnel, LocalTunnelState::Off) {
            let can_tunnel = running && (state.managed || state.account.logged_in || state.mock);
            let mut tunnel_row = row();
            match &state.local_tunnel {
                LocalTunnelState::Off => {
                    tunnel_row = tunnel_row.child(button(
                        "toggle-local-tunnel",
                        state.text(
                            "开启本机直连 (127.0.0.1:8188)",
                            "Local forward (127.0.0.1:8188)",
                        ),
                        can_tunnel,
                        handle,
                        |state, cx| state.toggle_local_tunnel(cx),
                    ));
                }
                LocalTunnelState::Starting { port } => {
                    tunnel_row = tunnel_row
                        .child(button(
                            "toggle-local-tunnel",
                            state.text("取消本机直连", "Cancel forward"),
                            true,
                            handle,
                            |state, cx| state.toggle_local_tunnel(cx),
                        ))
                        .child(
                            Badge::new(if state.settings.language.starts_with("zh") {
                                format!("正在建立 127.0.0.1:{port} 转发…")
                            } else {
                                format!("Forwarding 127.0.0.1:{port}…")
                            })
                            .tone(Tone::Info),
                        );
                }
                LocalTunnelState::Ready { port, url } => {
                    let open_local = url.clone();
                    let copy_local = url.clone();
                    tunnel_row = tunnel_row
                        .child(button(
                            "open-local-comfy",
                            if state.settings.language.starts_with("zh") {
                                format!("打开本机直连 (127.0.0.1:{port})")
                            } else {
                                format!("Open local (127.0.0.1:{port})")
                            },
                            true,
                            handle,
                            move |_, cx| cx.open_url(&open_local),
                        ))
                        .child(CopyButton::new("copy-local-comfy-url", copy_local))
                        .child(button(
                            "toggle-local-tunnel",
                            state.text("关闭直连", "Stop forward"),
                            true,
                            handle,
                            |state, cx| state.toggle_local_tunnel(cx),
                        ))
                        .child(
                            Badge::new(state.text("直连加速 · 零隧道延迟", "Direct · low latency"))
                                .tone(Tone::Success),
                        );
                }
            }
            card = card
                .child(tagged("local-tunnel-row", tunnel_row))
                .child(Caption::new(state.text(
                    "单机自用推荐开启本机直连：经 Colab SSH 通道直连 127.0.0.1，不走 trycloudflare 且无需输入密码。",
                    "Direct SSH port forward to 127.0.0.1 bypasses trycloudflare with no Basic Auth prompt.",
                )));
        }
    }
    card
}

fn workflows(state: &AppState, handle: &Entity<AppState>, fields: &Fields, cx: &App) -> Page {
    let mut page = Page::new(state.text("工作流", "Workflows")).subtitle(state.text(
        "安装模型和工作流，在 ComfyUI 中完成创作。",
        "Install models and workflows, then create in ComfyUI.",
    ));
    if !state.connected {
        let (title, badge, body, target_route, btn_label) = match state.lifecycle_stage() {
            LifecycleStage::SignedOut => (
                state.text(
                    "前置步骤未完成：请先绑定账号并唤醒 GPU",
                    "Prerequisites required: Sign in & wake GPU",
                ),
                state.text("未登录", "Signed out"),
                state.text(
                    "工作流包安装、权重下载与 Google Drive 同步依赖已运行的 ComfyUI 服务端。请先完成：1. 绑定 Google 账号 → 2. 唤醒 GPU → 3. 部署 ComfyUI（或在概览手动连接已有服务）。",
                    "Workflow packs, model downloads, and Drive sync require a connected ComfyUI server. Sign in, wake a GPU, and deploy ComfyUI first (or connect an existing HTTPS service).",
                ),
                Route::Dashboard,
                state.text("前往概览", "Go to Overview"),
            ),
            LifecycleStage::NeedGpu => (
                state.text(
                    "第 2 步尚未完成：请先唤醒 GPU 实例",
                    "Step 2 required: Wake a GPU runtime",
                ),
                state.text("未唤醒 GPU", "No GPU"),
                state.text(
                    "请先在「概览」页面唤醒并验证 Colab GPU 实例，然后在「部署中心」部署 ComfyUI，部署完成后将自动解锁工作流与权重管理。",
                    "Wake and verify a Colab GPU runtime on Overview, then deploy ComfyUI in Deploy to unlock workflows and model downloads.",
                ),
                Route::Dashboard,
                state.text("前往概览唤醒 GPU", "Go to Overview"),
            ),
            LifecycleStage::GpuReady | LifecycleStage::ServiceReady => (
                state.text(
                    "第 3 步尚未完成：请先部署 ComfyUI",
                    "Step 3 required: Deploy ComfyUI first",
                ),
                state.text("待部署 ComfyUI", "Not deployed"),
                state.text(
                    "你的 GPU 实例已验证就绪，但尚未部署 ComfyUI 服务端。请先前往「部署中心」点击「部署 ComfyUI」，连接成功后即可在此一键安装预设工作流包与模型权重。",
                    "Your GPU runtime is ready, but ComfyUI is not deployed yet. Open Deploy and start ComfyUI first to install workflow packs and weights here.",
                ),
                Route::Deploy,
                state.text("前往部署中心部署 ComfyUI", "Go to Deploy"),
            ),
        };
        return page.child(tagged(
            "workflows-gate-card",
            Card::new()
                .header(CardHeader::new(title).action(Badge::new(badge).tone(Tone::Warning)))
                .child(Paragraph::new(body))
                .child(button(
                    "workflows-go-prereq",
                    btn_label,
                    true,
                    handle,
                    move |state, cx| state.navigate(target_route, cx),
                )),
        ));
    }
    let comfy = state.server.as_ref().map(|s| &s.comfyui);
    let packs = comfy
        .map(|c| &c.preset_packs)
        .filter(|p| !p.is_empty())
        .cloned()
        .unwrap_or_else(preset_packs);
    let enabled =
        state.connected && !state.busy.contains("install") && !comfy.is_some_and(ComfyStatus::busy);
    page = page.child(comfy_card(state, handle, fields, cx));
    for pack in packs {
        let id = pack.id.clone();
        let label = if pack.ready {
            state.text("已就绪 · 重新安装", "Ready · reinstall")
        } else {
            state.text("安装工作流包", "Install workflow pack")
        };
        let counts = format!(
            "{} / {} {} · {:.1} GB",
            pack.installed_models,
            pack.total_models,
            state.text("模型", "models"),
            pack.approx_size_gb
        );
        page = page.child(
            Card::new()
                .header(
                    CardHeader::new(pack.title).action(
                        Badge::new(if pack.ready {
                            state.text("已就绪", "Ready")
                        } else {
                            state.text("可安装", "Available")
                        })
                        .tone(if pack.ready {
                            Tone::Success
                        } else {
                            Tone::Info
                        }),
                    ),
                )
                .child(Paragraph::new(pack.scenario))
                .child(Caption::new(counts))
                .children(pack.workflows.into_iter().map(Caption::new))
                .child(request_button(
                    format!("pack-{id}"),
                    label,
                    enabled,
                    handle,
                    fields,
                    Request::Action(ComfyAction::InstallPack { pack_id: id }),
                )),
        );
    }
    let download_fields = fields.clone();
    page = page.child(
        Card::new()
            .header(CardHeader::new(
                state.text("下载模型权重", "Download model weights"),
            ))
            .child(field(
                "weight-source",
                state.text(
                    "下载地址 / Hugging Face 来源",
                    "Download URL / Hugging Face source",
                ),
                fields,
            ))
            .child(field(
                "weight-filename",
                state.text("文件名（可选）", "Filename (optional)"),
                fields,
            ))
            .child(field(
                "weight-folder",
                state.text("ComfyUI 模型子目录", "ComfyUI model subfolder"),
                fields,
            ))
            .child(button(
                "download-weight",
                state.text("下载权重", "Download weights"),
                enabled,
                handle,
                move |state, cx| {
                    let source = download_fields.text("weight-source", cx).trim().to_owned();
                    if source.is_empty() {
                        let msg = state.text("请填写模型来源", "Enter a model source");
                        state.set_error(msg);
                        cx.notify();
                        return;
                    }
                    match download_fields.apply(state, cx) {
                        Ok(()) => state.request(
                            Request::Action(ComfyAction::DownloadWeight {
                                source,
                                filename: download_fields.text("weight-filename", cx),
                                subfolder: download_fields.text("weight-folder", cx),
                            }),
                            cx,
                        ),
                        Err(error) => {
                            state.set_error(error);
                            cx.notify();
                        }
                    }
                },
            )),
    );
    page.child(
        Card::new()
            .header(CardHeader::new(
                state.text("Google Drive 持久化", "Google Drive persistence"),
            ))
            .child(Paragraph::new(
                comfy
                    .map(|c| c.persistence_label.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        state
                            .text("连接服务后查看 Drive 状态", "Connect to see Drive status")
                            .into()
                    }),
            ))
            .children(comfy.map(|c| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Caption::new(format!(
                        "{}: {}",
                        state.text("持久化路径", "Persistence path"),
                        c.persist_root
                    )))
                    .child(Caption::new(format!(
                        "{} {} · {} {} · {} {}",
                        c.custom_nodes_count,
                        state.text("节点", "nodes"),
                        c.models_count,
                        state.text("模型", "models"),
                        c.workflows_count,
                        state.text("工作流", "workflows")
                    )))
                    .child(Caption::new(format!(
                        "{}: {}",
                        state.text("上次同步", "Last sync"),
                        if c.last_sync_ts == 0 {
                            state.text("尚未同步", "Not synced yet").to_owned()
                        } else {
                            let elapsed = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs()
                                .saturating_sub(c.last_sync_ts);
                            if elapsed < 60 {
                                state.text("刚刚", "Just now").to_owned()
                            } else {
                                format!("{} {}", elapsed / 60, state.text("分钟前", "minutes ago"))
                            }
                        }
                    )))
            }))
            .child(request_button(
                "sync-drive",
                state.text("同步到 Drive", "Sync to Drive"),
                state.connected
                    && !state.busy.contains("sync-drive")
                    && !comfy.is_some_and(ComfyStatus::busy),
                handle,
                fields,
                Request::Action(ComfyAction::SyncDrive),
            )),
    )
}

fn outputs(state: &AppState, handle: &Entity<AppState>, fields: &Fields, cx: &App) -> Page {
    let outputs = state
        .server
        .as_ref()
        .map(|s| s.comfyui.recent_outputs.clone())
        .unwrap_or_default();
    let mut page = Page::new(state.text("输出文件", "Outputs")).subtitle(state.text(
        "手动下载生成结果；图片在应用中预览，视频交给系统播放器。",
        "Download results manually; preview images here and open videos with your system player.",
    ));
    page = page.child(
        row()
            .child(request_button(
                "refresh-outputs",
                state.text("刷新列表", "Refresh outputs"),
                state.connected && !state.busy.contains("status"),
                handle,
                fields,
                Request::Status,
            ))
            .child(button(
                "open-output-directory",
                state.text("打开保存目录", "Open download folder"),
                true,
                handle,
                |state, cx| state.open_output_dir(cx),
            )),
    );
    if !state.connected {
        let (target_route, btn_label) = if state.has_awake_runtime() {
            (
                Route::Deploy,
                state.text("前往部署中心部署 ComfyUI", "Go to Deploy"),
            )
        } else {
            (Route::Dashboard, state.text("前往概览", "Go to Overview"))
        };
        page = page.child(tagged(
            "outputs-gate-card",
            Card::new()
                .header(
                    CardHeader::new(state.text("尚未连接 ComfyUI 服务", "ComfyUI not connected"))
                        .action(Badge::new(state.text("仅本地浏览", "Local only")).tone(Tone::Info)),
                )
                .child(Paragraph::new(state.text(
                    "云端输出列表将在部署并连接 ComfyUI 后自动同步；下方仍可随时预览已下载到本地的成片。",
                    "Remote cloud outputs sync once ComfyUI is deployed and connected. Already downloaded local files remain browsable below.",
                )))
                .child(button(
                    "outputs-go-prereq",
                    btn_label,
                    true,
                    handle,
                    move |state, cx| state.navigate(target_route, cx),
                )),
        ));
    } else if outputs.is_empty() {
        page = page.child(Caption::new(
            state.text("还没有生成结果。", "No generated outputs yet."),
        ));
    }
    for output in outputs {
        page = page.child(
            Card::new()
                .header(CardHeader::new(output.filename.clone()))
                .child(Caption::new(format!(
                    "{} · {:.1} MB",
                    output.media_type, output.size_mb
                )))
                .child(request_button(
                    format!("download-{}", output.rel_path),
                    state.text("下载到本机", "Download"),
                    !state.busy.contains("download-output"),
                    handle,
                    fields,
                    Request::Download(output),
                )),
        );
    }
    if let Some(path) = &state.preview {
        page = page.child(
            Card::new()
                .header(CardHeader::new(state.text("图片预览", "Image preview")))
                .child(tagged(
                    "image-preview",
                    div()
                        .w_full()
                        .h(px(420.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .overflow_hidden()
                        .child(
                            img(path.clone())
                                .h(px(420.))
                                .max_w_full()
                                .min_h_0()
                                .min_w_0()
                                .debug_selector(|| "preview-image".into()),
                        ),
                )),
        );
    }
    if !state.local_outputs.is_empty() {
        let mut card = Card::new().header(CardHeader::new(state.text("已下载", "Downloaded")));
        for (index, path) in state.local_outputs.iter().enumerate() {
            let selected = path.clone();
            let open = path.clone();
            let mut entry = row().child(Caption::new(path.display().to_string()));
            if is_image(path) {
                entry = entry.child(button(
                    format!("preview-{index}"),
                    state.text("预览", "Preview"),
                    true,
                    handle,
                    move |state, cx| {
                        state.preview = Some(selected.clone());
                        cx.notify();
                    },
                ));
            }
            entry = entry.child(button(
                format!("open-output-{index}"),
                state.text("用系统打开", "Open with system"),
                true,
                handle,
                move |state, cx| match url::Url::from_file_path(&open) {
                    Ok(url) => cx.open_url(url.as_str()),
                    Err(()) => {
                        let msg = state.text("无法打开此路径", "Could not open this path");
                        state.set_error(msg);
                        cx.notify();
                    }
                },
            ));
            card = card.child(entry);
        }
        page = page.child(card);
    }
    let _ = cx;
    page
}

fn settings(state: &AppState, handle: &Entity<AppState>, fields: &Fields, _cx: &App) -> Page {
    let language_handle = handle.clone();
    let language = Select::new(
        "language-select",
        [
            Choice::new("zh-CN", "简体中文"),
            Choice::new("en-US", "English"),
        ],
    )
    .selected(state.settings.language.clone())
    .on_change(move |value, _, cx| {
        language_handle.update(cx, |state, cx| state.set_language(value, cx))
    });
    let appearance_handle = handle.clone();
    let theme = ThemeSelector::new(
        "appearance-selector",
        appearance(&state.settings.appearance),
        move |appearance, _, cx| {
            let value = match appearance {
                Appearance::System => "system",
                Appearance::Light => "light",
                Appearance::Dark => "dark",
            };
            appearance_handle.update(cx, |state, cx| state.set_appearance(value, cx));
        },
    );
    let save_fields = fields.clone();
    let idle_fields = fields.clone();
    Page::new(state.text("设置", "Settings")).subtitle(state.text("外观、服务连接与保存位置。", "Appearance, service connection, and downloads."))
        .child(Card::new().header(CardHeader::new(state.text("外观与语言", "Appearance and language"))).child(tagged("language", language)).child(tagged("appearance", theme)))
        .child(Card::new().header(CardHeader::new(state.text("服务配置", "Service configuration")))
            .child(field("service-url", "HTTPS URL", fields))
            .child(field("password", state.text("管理密码", "Management password"), fields))
            .child(field("hf-token", state.text("Hugging Face Token（可选）", "Hugging Face token (optional)"), fields))
            .child(field("idle", state.text("闲置自动停止（分钟，0 为禁用）", "Idle shutdown (minutes; 0 disables)"), fields))
            .child(Caption::new(state.text("按照服务端的活动检测自动释放运行时；关闭窗口后仍由服务端执行。", "The server releases an idle runtime based on its activity detection, even after this window closes.")))
            .child(field("output-dir", state.text("本地输出目录", "Local output folder"), fields))
            .child(row().child(button("save-settings", state.text("保存设置", "Save settings"), true, handle, move |state, cx| {
                match save_fields.apply(state, cx) { Ok(()) => state.save_settings(cx), Err(error) => { state.set_error(error); cx.notify(); } }
            })).child(button("apply-idle", state.text("应用闲置时间到服务", "Apply idle timeout"), state.connected && !state.busy.contains("set-idle"), handle, move |state, cx| {
                match idle_fields.apply(state, cx) { Ok(()) => { state.save_settings_silent(cx); state.request(Request::Action(ComfyAction::SetIdle), cx); }, Err(error) => { state.set_error(error); cx.notify(); } }
            }))))
        .child(Caption::new(format!("EasyColab GPUI {} · GPUI + Ely", env!("CARGO_PKG_VERSION"))))
}

fn confirmation(state: &AppState, handle: &Entity<AppState>, _cx: &App) -> Option<AnyElement> {
    let confirmation = state.confirmation?;
    let close_handle = handle.clone();
    let (title, message, confirm_label) = match confirmation {
        Confirmation::Wake => (state.text("唤醒 Colab GPU 实例？", "Wake a Colab GPU runtime?"), state.text("仅分配或切换到所选 GPU 实例（不会自动部署 ComfyUI）。操作可能释放当前账号的其他实例；停止会丢失未保存的数据。", "Allocates or switches to the selected GPU runtime without deploying ComfyUI. Other runtimes on this account may be released; stopping loses unsaved data."), state.text("唤醒实例", "Wake runtime")),
        Confirmation::ReleaseAll => (state.text("释放全部实例？", "Release all runtimes?"), state.text("将释放下列全部 Colab 实例并结束计费。未保存到 Drive 的数据会丢失。", "Release every Colab runtime below and end billing. Data not saved to Drive will be lost."), state.text("释放全部", "Release all")),
        Confirmation::ReleaseCurrent => (state.text("释放当前服务实例？", "Release current runtime?"), state.text("向当前 HTTPS 服务发送释放请求。当前任务将停止，未保存的数据会丢失。", "Ask the current HTTPS service to release its runtime. Running tasks stop and unsaved data is lost."), state.text("释放当前实例", "Release current runtime")),
        Confirmation::Quit => (state.text("关闭 EasyColab？", "Close EasyColab?"), state.text("保留运行时会继续计费，后台任务可能仍在执行。你也可以释放实例后退出。", "Keeping runtimes running continues billing; background work may still be active. You can release runtimes before quitting."), state.text("释放并退出", "Release and quit")),
    };
    let mut dialog = Dialog::new("confirmation", title, move |_, cx| {
        close_handle.update(cx, |state, cx| {
            state.confirmation = None;
            cx.notify();
        })
    })
    .child(Paragraph::new(message));
    if state.busy.contains("auth-status") {
        dialog = dialog.child(Caption::new(
            state.text("正在核对实例…", "Checking runtimes…"),
        ));
    }
    if state.managed
        || matches!(confirmation, Confirmation::Wake | Confirmation::ReleaseAll)
        || (confirmation == Confirmation::Quit && state.settings.service_url.is_empty())
    {
        for session in &state.account.sessions {
            dialog = dialog.child(tagged(
                format!("confirm-runtime-{}", session.endpoint),
                Caption::new(format!(
                    "• {} · {} · {}",
                    session.name, session.accelerator, session.endpoint
                )),
            ));
        }
        if state.account.sessions.is_empty() {
            dialog = dialog.child(Caption::new(state.text(
                "当前没有列出的 Colab 实例。",
                "No Colab runtimes currently listed.",
            )));
        }
    } else {
        dialog = dialog.child(Caption::new(state.settings.service_url.clone()));
    }
    let cancel_handle = handle.clone();
    let cancel_label = state.text("取消", "Cancel");
    dialog = dialog.action(move |_| {
        button(
            "confirm-cancel",
            cancel_label,
            true,
            &cancel_handle,
            |state, cx| {
                state.confirmation = None;
                cx.notify();
            },
        )
    });
    if confirmation == Confirmation::Quit {
        let keep_handle = handle.clone();
        let label = state.text("保留运行时并退出", "Keep running and quit");
        dialog = dialog
            .action(move |_| button("quit-keep", label, true, &keep_handle, |_, cx| cx.quit()));
    }
    let confirm_handle = handle.clone();
    let enabled = !state.busy.contains("auth-status")
        && !state.busy.contains("wake")
        && !state.busy.contains("release");
    Some(
        dialog
            .action(move |_| {
                button(
                    "confirm-action",
                    confirm_label,
                    enabled,
                    &confirm_handle,
                    |state, cx| state.confirm(cx),
                )
            })
            .into_any_element(),
    )
}

fn window_control_dots(handle: &Entity<AppState>, window: &mut Window, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let colors = &theme.colors;
    let active = window.is_window_active();
    let group = SharedString::from("title-window-controls");
    let dot_size = px(16.);
    let close_handle = handle.clone();
    let buttons = [
        ("window-close", colors.danger, "×", 0u8),
        ("window-minimize", colors.warning, "−", 1u8),
        ("window-maximize", colors.success, "+", 2u8),
    ];
    tagged(
        "title-bar-controls",
        div()
            .id("window-controls")
            .group(group.clone())
            .occlude()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .children(buttons.into_iter().map(|(id, tint, glyph, kind)| {
                let close_handle = close_handle.clone();
                div()
                    .id(id)
                    .debug_selector(move || id.to_owned())
                    .size(dot_size)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .bg(if active { tint } else { colors.border_strong })
                    .border_1()
                    .border_color(colors.shadow.opacity(0.18))
                    .text_size(px(11.))
                    .line_height(dot_size)
                    .font_weight(FontWeight::BOLD)
                    .text_color(colors.shadow.opacity(0.35))
                    .group_hover(group.clone(), |style| {
                        style.text_color(colors.shadow.opacity(0.78))
                    })
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(move |_, window, cx| match kind {
                        0 => close_handle.update(cx, |state, cx| state.request_quit(cx)),
                        1 => window.minimize_window(),
                        _ => window.zoom_window(),
                    })
                    .child(glyph)
            })),
    )
    .into_any_element()
}

fn titlebar(handle: &Entity<AppState>, window: &mut Window, cx: &mut App) -> AnyElement {
    let show_client_controls = !cfg!(target_os = "macos");
    let controls = show_client_controls.then(|| window_control_dots(handle, window, cx));
    let mac_inset = cfg!(target_os = "macos") && !window.is_fullscreen();
    let (titlebar_height, border_color) = {
        let theme = cx.theme();
        (theme.titlebar_height(), theme.colors.border)
    };
    let brand = {
        let theme = cx.theme();
        let colors = &theme.colors;
        let mark = div()
            .debug_selector(|| "title-bar-mark".into())
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .child(
                Icon::new(IconName::Zap)
                    .size(IconSize::Sm)
                    .color(colors.accent),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(theme.text_size(TextSize::Sm))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.fg)
                    .child(Ellipsis::new("EasyColab GPUI")),
            );
        div()
            .debug_selector(|| "title-bar-brand".into())
            .flex()
            .flex_none()
            .items_center()
            .gap_3()
            .h_full()
            .w(theme.sidebar_width(false))
            .border_r_1()
            .border_color(colors.border)
            .map(|brand| match mac_inset {
                true => brand.pl(theme.traffic_light_inset()).pr_2(),
                false => brand.px_3(),
            })
            .children(controls)
            .child(mark)
    };
    drag_region("title-bar", window, cx)
        .debug_selector(|| "title-bar".into())
        .flex()
        .flex_none()
        .w_full()
        .h(titlebar_height)
        .border_b_1()
        .border_color(border_color)
        .child(brand)
        .child(div().flex_1().h_full())
        .into_any_element()
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (updates, toasts) = self.state.update(cx, |state, _| {
            (
                std::mem::take(&mut state.pending_fields),
                std::mem::take(&mut state.pending_toasts),
            )
        });
        for (id, value) in updates {
            self.fields
                .get(id)
                .update(cx, |input, cx| input.set_text(value, cx));
        }
        if !toasts.is_empty() {
            self.toaster.update(cx, |toaster, cx| {
                for item in toasts {
                    let mut toast = Toast::new(item.title)
                        .severity(item.severity)
                        .stay(item.stay);
                    if let Some(body) = item.body {
                        toast = toast.body(body);
                    }
                    toaster.push(toast, cx);
                }
            });
        }
        let (drawer_open, drawer_tab) = {
            let state = self.state.read(cx);
            (state.logs_open, state.drawer_tab)
        };
        let terminal_active = drawer_open && drawer_tab == DrawerTab::Terminal;
        if terminal_active {
            self.ensure_terminal(cx);
            if !self.last_terminal_open
                && let Some(term) = &self.terminal
            {
                let handle = term.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        self.last_terminal_open = terminal_active;

        let title_bar = titlebar(&self.state, window, cx);
        let state = self.state.read(cx);
        let theme = cx.theme();
        let page = match state.route {
            Route::Dashboard => dashboard(state, &self.state, &self.fields, cx),
            Route::Deploy => deploy_page(state, &self.state, &self.fields, cx),
            Route::Workflows => workflows(state, &self.state, &self.fields, cx),
            Route::Outputs => outputs(state, &self.state, &self.fields, cx),
            Route::Settings => settings(state, &self.state, &self.fields, cx),
        };
        let content = div().w_full().flex().flex_col().gap_4().p_6().child(page);
        let log_handle = self.state.clone();
        let term_handle = self.state.clone();
        let copy_logs_handle = self.state.clone();
        let clear_handle = self.state.clone();
        let collapse_handle = self.state.clone();
        let drawer_progress: Option<(Option<f32>, String)> = state
            .active_task
            .as_ref()
            .map(|task| (task.value, task.label.clone()))
            .or_else(|| {
                state.server.as_ref().and_then(|server| {
                    server.comfyui.busy().then(|| {
                        let value = Some((server.comfyui.progress as f32).clamp(0.0, 1.0));
                        let label = if server.comfyui.status_message.is_empty() {
                            state
                                .text("云端任务执行中…", "Cloud task running…")
                                .to_owned()
                        } else {
                            server.comfyui.status_message.clone()
                        };
                        (value, label)
                    })
                })
            });
        let logs_active = drawer_open && drawer_tab == DrawerTab::Logs;
        let mut left_tabs = div()
            .flex()
            .items_center()
            .gap_2()
            .child(tagged(
                "toggle-logs",
                Button::new("toggle-logs", state.text("运行日志", "Activity log"))
                    .variant(if logs_active {
                        ButtonVariant::Secondary
                    } else {
                        ButtonVariant::Ghost
                    })
                    .size(ControlSize::Sm)
                    .on_click(move |_, _, cx| {
                        log_handle
                            .update(cx, |state, cx| state.open_drawer_tab(DrawerTab::Logs, cx))
                    }),
            ))
            .child(tagged(
                "toggle-terminal",
                Button::new(
                    "toggle-terminal",
                    state.text("终端 (colab-cli)", "Terminal (colab-cli)"),
                )
                .variant(if terminal_active {
                    ButtonVariant::Secondary
                } else {
                    ButtonVariant::Ghost
                })
                .size(ControlSize::Sm)
                .on_click(move |_, _, cx| {
                    term_handle.update(cx, |state, cx| {
                        state.open_drawer_tab(DrawerTab::Terminal, cx)
                    })
                }),
            ));
        if let Some((_, ref label)) = drawer_progress {
            left_tabs = left_tabs.child(tagged(
                "drawer-streaming-badge",
                Badge::new(state.text("实时执行中", "Streaming")).tone(Tone::Info),
            ));
            if !logs_active {
                left_tabs = left_tabs.child(Caption::new(label.clone()));
            }
        } else if !state.logs.is_empty() {
            left_tabs = left_tabs.child(
                Badge::new(if state.settings.language.starts_with("zh") {
                    format!("{} 行", state.logs.len())
                } else {
                    format!("{} lines", state.logs.len())
                })
                .tone(Tone::Neutral),
            );
        }
        let mut drawer_header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(left_tabs);
        if drawer_open {
            let mut actions = div().flex().items_center().gap_2();
            if drawer_tab == DrawerTab::Logs {
                let has_logs = !state.logs.is_empty();
                actions = actions
                    .child(tagged(
                        "copy-logs",
                        Button::new("copy-logs", state.text("复制日志", "Copy"))
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .disabled(!has_logs)
                            .on_click(move |_, _, cx| {
                                copy_logs_handle.update(cx, |state, cx| {
                                    if state.logs.is_empty() {
                                        return;
                                    }
                                    let count = state.logs.len();
                                    let joined = state.logs.join("\n");
                                    cx.write_to_clipboard(ClipboardItem::new_string(joined));
                                    let msg = if state.settings.language.starts_with("zh") {
                                        format!("已复制 {count} 行日志到剪贴板")
                                    } else {
                                        format!("Copied {count} log line(s) to clipboard")
                                    };
                                    state.set_notice(msg);
                                    cx.notify();
                                })
                            }),
                    ))
                    .child(tagged(
                        "clear-logs",
                        Button::new("clear-logs", state.text("清空", "Clear"))
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .disabled(!has_logs)
                            .on_click(move |_, _, cx| {
                                clear_handle.update(cx, |state, cx| {
                                    state.logs.clear();
                                    cx.notify();
                                })
                            }),
                    ));
            } else {
                let clear_term = self.terminal.clone();
                let view_for_restart = cx.entity().clone();
                actions = actions
                    .child(tagged(
                        "clear-terminal",
                        Button::new("clear-terminal", state.text("清屏", "Clear"))
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .disabled(clear_term.is_none())
                            .on_click(move |_, _, cx| {
                                if let Some(term) = &clear_term {
                                    term.update(cx, |term, cx| term.clear(cx));
                                }
                            }),
                    ))
                    .child(tagged(
                        "restart-terminal",
                        Button::new("restart-terminal", state.text("重启终端", "Restart"))
                            .variant(ButtonVariant::Ghost)
                            .size(ControlSize::Sm)
                            .on_click(move |_, window, cx| {
                                view_for_restart.update(cx, |view, cx| {
                                    view.restart_terminal(window, cx);
                                })
                            }),
                    ));
            }
            actions = actions.child(tagged(
                "collapse-drawer",
                Button::new("collapse-drawer", state.text("收起", "Hide"))
                    .variant(ButtonVariant::Ghost)
                    .size(ControlSize::Sm)
                    .on_click(move |_, _, cx| {
                        collapse_handle.update(cx, |state, cx| {
                            state.logs_open = false;
                            cx.notify();
                        })
                    }),
            ));
            drawer_header = drawer_header.child(actions);
        }
        let mut console = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(theme.colors.border)
            .child(drawer_header);
        if drawer_open {
            match drawer_tab {
                DrawerTab::Logs => {
                    if let Some((value, label)) = &drawer_progress {
                        console = console.child(tagged(
                            "task-progress-strip",
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .pb_1()
                                .child(Caption::new(progress_caption(label, *value)))
                                .child(render_progress_bar("drawer-task-progress", *value)),
                        ));
                    }
                    let log_body = if state.logs.is_empty() {
                        Caption::new(state.text(
                            "暂无运行日志。执行登录、唤醒、部署或上传操作后将在此实时记录，支持鼠标划选或一键复制。",
                            "No activity yet. Operations log here in real time; drag to select or click Copy.",
                        ))
                        .into_any_element()
                    } else {
                        let visible_text = state
                            .logs
                            .iter()
                            .rev()
                            .take(200)
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n");
                        tagged(
                            "log-text",
                            div()
                                .w_full()
                                .font_family(theme.mono_family.clone())
                                .text_size(theme.text_size(TextSize::Xs))
                                .text_color(theme.colors.fg_muted)
                                .child(SelectableText::new("activity-log-text", visible_text)),
                        )
                        .into_any_element()
                    };
                    console = console.child(
                        ScrollArea::new("log-scroll")
                            .vertical()
                            .h(px(200.))
                            .child(log_body),
                    );
                }
                DrawerTab::Terminal => {
                    if let Some(term) = &self.terminal {
                        console = console.child(tagged(
                            "embedded-terminal",
                            div()
                                .w_full()
                                .h(px(220.))
                                .rounded_md()
                                .overflow_hidden()
                                .border_1()
                                .border_color(theme.colors.border)
                                .child(term.clone()),
                        ));
                    } else if let Some(error) = &self.terminal_error {
                        console = console.child(tagged(
                            "terminal-error",
                            InlineMessage::new(
                                Severity::Danger,
                                format!(
                                    "{}: {}",
                                    state.text("无法启动终端", "Could not start terminal"),
                                    error
                                ),
                            ),
                        ));
                    }
                }
            }
        }
        let client_drawn = matches!(window.window_decorations(), Decorations::Client { .. });
        let mut root = FocusScope::new(&self.focus)
            .root()
            .size_full()
            .flex()
            .flex_col()
            .when(!client_drawn, |root| root.bg(theme.colors.bg))
            .text_color(theme.colors.fg)
            .font_family(theme.font_family.clone())
            .text_size(theme.text_size(TextSize::Base))
            .child(raise("title-strip", title_bar).with_priority(1))
            .child(
                div().flex_1().min_h_0().child(
                    AppShell::new()
                        .sidebar(sidebar(state, &self.state, cx))
                        .child(
                            tagged(
                                "page-area",
                                ScrollArea::new("page-scroll")
                                    .vertical()
                                    .size_full()
                                    .child(content),
                            )
                            .flex_1()
                            .min_w_0()
                            .min_h_0(),
                        ),
                ),
            )
            .child(console)
            .child(ToastViewport::new("app-toasts", &self.toaster))
            .children(confirmation(state, &self.state, cx));
        root.text_style().font_fallbacks = self.font_fallbacks.clone();
        ResizeBorder::new().child(root)
    }
}
