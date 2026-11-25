use std::sync::{Arc, Mutex};

use easycolab_gpui::{
    app::{AppState, Confirmation},
    backend::Backend,
    models::*,
    settings::Settings,
    ui::RootView,
};
use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, px, size};

#[derive(Default)]
struct FakeBackend {
    calls: Mutex<Vec<String>>,
    fail_wake: std::sync::atomic::AtomicBool,
}

impl Backend for FakeBackend {
    fn run(
        &self,
        _: &Settings,
        request: Request,
        log: &mut dyn FnMut(String),
    ) -> anyhow::Result<Response> {
        let name = match &request {
            Request::Action(ComfyAction::Disconnect) => "disconnect",
            other => other.key(),
        };
        self.calls.lock().unwrap().push(name.into());
        log(format!("Completed {name}"));
        if matches!(request, Request::Wake)
            && self.fail_wake.load(std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("Deployment failed after runtime allocation");
        }
        Ok(match request {
            Request::AuthStatus => Response::Account(account()),
            Request::AuthUrl => Response::AuthUrl("https://oauth.invalid/manual".into()),
            Request::AuthLogin => Response::Account(AccountStatus {
                logged_in: true,
                email: "switched@example.invalid".into(),
                ..Default::default()
            }),
            Request::AuthCode(_) => Response::Account(AccountStatus {
                logged_in: true,
                email: "code@example.invalid".into(),
                ..Default::default()
            }),
            Request::Wake => Response::Woken(Session {
                name: "easycolab".into(),
                endpoint: "runtime-1".into(),
                accelerator: "T4".into(),
                variant: "GPU".into(),
                shape: "HIGH_RAM".into(),
            }),
            Request::Deploy => Response::Deployed("https://demo.invalid".into()),
            Request::UploadProject {
                remote_dir, target, ..
            } => Response::Uploaded {
                remote_path: format!("{}/{remote_dir}", target.base_path()),
                files: 2,
            },
            Request::RuntimeMetrics => Response::RuntimeMetrics(Box::new(ServerStatus {
                gpu_name: "NVIDIA T4".into(),
                vram_total_gb: Some(16.0),
                vram_used_gb: Some(2.5),
                vram_free_gb: Some(13.5),
                ram_total_gb: Some(32.0),
                ram_used_gb: Some(6.0),
                ram_free_gb: Some(26.0),
                disk_total_gb: Some(100.0),
                disk_used_gb: Some(22.0),
                disk_free_gb: Some(78.0),
                shm_total_gb: Some(16.0),
                shm_used_gb: Some(0.5),
                shm_free_gb: Some(15.5),
                ..Default::default()
            })),
            Request::Status => Response::Status(Box::new(ServerStatus {
                gpu_name: "NVIDIA T4".into(),
                vram_total_gb: Some(16.0),
                vram_used_gb: Some(4.0),
                vram_free_gb: Some(12.0),
                ram_total_gb: Some(32.0),
                ram_used_gb: Some(10.0),
                ram_free_gb: Some(22.0),
                disk_total_gb: Some(100.0),
                disk_used_gb: Some(25.0),
                disk_free_gb: Some(75.0),
                shm_total_gb: Some(16.0),
                shm_used_gb: Some(1.0),
                shm_free_gb: Some(15.0),
                drive_total_gb: Some(500.0),
                drive_used_gb: Some(50.0),
                drive_free_gb: Some(450.0),
                ..Default::default()
            })),
            _ => Response::Done("Done".into()),
        })
    }
}

fn account() -> AccountStatus {
    AccountStatus {
        logged_in: true,
        email: "demo@example.invalid".into(),
        sessions: vec![Session {
            name: "existing-runtime".into(),
            endpoint: "runtime-1".into(),
            accelerator: "T4".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

struct TestApp<'a> {
    cx: &'a mut VisualTestContext,
    state: Entity<AppState>,
    root: Entity<RootView>,
    backend: Arc<FakeBackend>,
}

impl<'a> TestApp<'a> {
    fn new(cx: &'a mut TestAppContext) -> Self {
        cx.update(ely_gpui_component::init_for_tests);
        let backend = Arc::new(FakeBackend::default());
        let state =
            cx.update(|cx| AppState::new(Settings::default(), backend.clone(), true, None, cx));
        state.update(cx, |state, _| {
            state.account = account();
        });
        let mounted = state.clone();
        let (root, cx) = cx.add_window_view(move |window, cx| RootView::new(mounted, window, cx));
        cx.simulate_resize(size(px(1220.), px(1800.)));
        cx.update(|window, _| window.activate_window());
        let mut app = Self {
            cx,
            state,
            root,
            backend,
        };
        app.settle();
        app
    }

    fn settle(&mut self) {
        self.cx.run_until_parked();
        self.cx.update(|window, _| window.refresh());
        self.cx.run_until_parked();
    }
    fn click(&mut self, selector: &'static str) {
        let bounds = self
            .cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        self.cx.simulate_click(bounds.center(), Modifiers::none());
        self.settle();
    }
    fn calls(&self, name: &str) -> usize {
        self.backend
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == name)
            .count()
    }
}

#[gpui::test]
fn navigation_keeps_text_input_entities_and_demo_badge(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    assert!(app.cx.debug_bounds("mock-badge").is_some());
    app.click("nav-settings");
    assert_eq!(
        app.state.read_with(app.cx, |state, _| state.route),
        Route::Settings
    );
    app.click("field-service-url");
    app.cx.simulate_input("https://my-runtime.example");
    app.settle();
    app.click("nav-workflows");
    assert!(app.cx.debug_bounds("workflows-gate-card").is_some());
    assert!(app.cx.debug_bounds("pack-wan22_i2v_flf2v").is_none());
    app.click("nav-deploy");
    assert_eq!(
        app.state.read_with(app.cx, |state, _| state.route),
        Route::Deploy
    );
    assert!(app.cx.debug_bounds("deploy-gate-card").is_some());
    app.click("nav-outputs");
    assert!(app.cx.debug_bounds("outputs-gate-card").is_some());
    app.click("nav-settings");
    let input = app
        .root
        .read_with(app.cx, |root, _| root.fields.get("service-url").clone());
    assert_eq!(
        input.read_with(app.cx, |input, _| input.text().to_owned()),
        "https://my-runtime.example"
    );
    app.click("segment theme.dark");
    assert_eq!(
        app.state
            .read_with(app.cx, |state, _| state.settings.appearance.clone()),
        "dark"
    );
    let shown_toasts = app
        .root
        .read_with(app.cx, |root, cx| root.toaster.read(cx).shown());
    assert!(
        shown_toasts
            .iter()
            .any(|(title, _)| title.contains("设置已保存") || title.contains("Settings saved")),
        "expected non-blocking toast on settings change: {shown_toasts:?}"
    );
}

#[gpui::test]
fn wake_confirms_existing_runtimes_before_deployment(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    assert!(app.cx.debug_bounds("lifecycle-stepper").is_some());
    assert!(app.cx.debug_bounds("awake-runtime-card").is_none());
    assert!(app.cx.debug_bounds("unverified-runtime-card").is_some());
    assert!(app.cx.debug_bounds("comfy-start").is_none());
    app.click("wake");
    assert_eq!(app.calls("wake"), 0);
    assert_eq!(
        app.state.read_with(app.cx, |state, _| state.confirmation),
        Some(Confirmation::Wake)
    );
    assert!(app.cx.debug_bounds("confirm-runtime-runtime-1").is_some());
    app.click("confirm-action");
    assert_eq!(app.calls("wake"), 1);
    assert_eq!(app.calls("deploy"), 0);
    assert_eq!(
        app.state.read_with(app.cx, |state, _| state.route),
        Route::Deploy
    );
    assert!(app.cx.debug_bounds("deploy-comfy-card").is_some());
    assert!(app.cx.debug_bounds("deploy-runtime-metrics").is_some());
    assert!(app.cx.debug_bounds("upload-project-card").is_some());
    assert!(app.cx.debug_bounds("terminal-guide-card").is_some());

    // Without deploying ComfyUI, Overview already renders live kernel VRAM/RAM/Disk charts
    app.click("nav-dashboard");
    assert!(app.cx.debug_bounds("awake-runtime-card").is_some());
    assert!(app.cx.debug_bounds("telemetry-split-charts").is_some());
    assert!(app.cx.debug_bounds("refresh-runtime-metrics").is_some());
    assert!(app.cx.debug_bounds("comfy-start").is_none());

    // Workflows stay gated until ComfyUI is actually deployed
    app.click("nav-workflows");
    assert!(app.cx.debug_bounds("workflows-gate-card").is_some());
    app.click("workflows-go-prereq");
    assert_eq!(
        app.state.read_with(app.cx, |state, _| state.route),
        Route::Deploy
    );

    app.click("deploy-comfyui");
    assert_eq!(app.calls("deploy"), 1);
    assert_eq!(
        app.state
            .read_with(app.cx, |state, _| state.settings.management_password.len()),
        64
    );
    assert!(app.state.read_with(app.cx, |state, _| state.connected));

    // Workflows unlock once ComfyUI is deployed & connected
    app.click("nav-workflows");
    assert!(app.cx.debug_bounds("pack-wan22_i2v_flf2v").is_some());

    // Upload custom project (both Runtime and Drive targets) and jump to terminal from Deploy page
    app.click("nav-deploy");
    assert!(app.cx.debug_bounds("upload-target").is_some());
    app.click("field-project-local-path");
    app.cx.simulate_input("/tmp/my-project");
    app.settle();
    app.click("upload-project");
    assert_eq!(app.calls("upload"), 1);
    assert_eq!(
        app.state
            .read_with(app.cx, |state, _| state.last_uploaded_remote.clone()),
        Some("/content/project".into())
    );
    app.state.update(app.cx, |state, cx| {
        state.upload_target = UploadTarget::Drive;
        cx.notify();
    });
    app.settle();
    assert!(app.cx.debug_bounds("drive-mount-status").is_some());
    app.click("upload-project");
    assert_eq!(app.calls("upload"), 2);
    assert_eq!(
        app.state
            .read_with(app.cx, |state, _| state.last_uploaded_remote.clone()),
        Some("/content/drive/MyDrive/project".into())
    );
    app.click("open-deploy-terminal");
    assert!(app.state.read_with(app.cx, |state, _| {
        state.logs_open && state.drawer_tab == easycolab_gpui::app::DrawerTab::Terminal
    }));

    app.click("collapse-drawer");
    app.click("nav-workflows");
    // Local port forwarding toggle works cleanly once ComfyUI is running
    app.state.update(app.cx, |state, cx| {
        if let Some(server) = &mut state.server {
            server.comfyui.is_running = true;
            server.comfyui.public_url = "https://demo-comfy.invalid".into();
        }
        cx.notify();
    });
    app.settle();
    assert!(app.cx.debug_bounds("local-tunnel-row").is_some());
    app.click("toggle-local-tunnel");
    assert!(app.cx.debug_bounds("open-local-comfy").is_some());
    assert!(matches!(
        app.state
            .read_with(app.cx, |state, _| state.local_tunnel.clone()),
        easycolab_gpui::app::LocalTunnelState::Ready { port: 8188, .. }
    ));
    app.click("toggle-local-tunnel");
    assert_eq!(
        app.state
            .read_with(app.cx, |state, _| state.local_tunnel.clone()),
        easycolab_gpui::app::LocalTunnelState::Off
    );

    app.click("nav-dashboard");
    assert!(app.cx.debug_bounds("comfy-start").is_some());
}

#[gpui::test]
fn manual_runtime_release_requires_confirmation_and_uses_service_disconnect(
    cx: &mut TestAppContext,
) {
    let mut app = TestApp::new(cx);
    app.state.update(app.cx, |state, cx| {
        state.connected = true;
        state.managed = false;
        state.settings.service_url = "https://manual.example".into();
        cx.notify();
    });
    app.settle();
    app.click("disconnect");
    assert_eq!(app.calls("disconnect"), 0);
    assert_eq!(app.calls("release"), 0);
    app.click("confirm-action");
    assert_eq!(app.calls("disconnect"), 1);
    assert_eq!(app.calls("release"), 0);
    assert!(!app.state.read_with(app.cx, |state, _| state.connected));
}

#[gpui::test]
fn failed_deployment_keeps_account_runtime_release_available(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    app.backend
        .fail_wake
        .store(true, std::sync::atomic::Ordering::SeqCst);
    app.click("wake");
    app.click("confirm-action");
    assert!(app.state.read_with(app.cx, |state, _| state.managed
        && state.settings.bridge_managed));
    assert!(
        app.state
            .read_with(app.cx, |state, _| state.error.is_some())
    );
    app.click("release-account-runtimes");
    assert!(app.cx.debug_bounds("confirm-runtime-runtime-1").is_some());
    app.click("confirm-action");
    assert_eq!(app.calls("release"), 1);
}

#[gpui::test]
fn output_selection_renders_native_image_preview(cx: &mut TestAppContext) {
    let folder = tempfile::tempdir().unwrap();
    let image = folder.path().join("preview.png");
    std::fs::write(&image, include_bytes!("../assets/demo-preview.png")).unwrap();
    let mut app = TestApp::new(cx);
    app.state.update(app.cx, |state, cx| {
        state.route = Route::Outputs;
        state.local_outputs.push(image);
        cx.notify();
    });
    app.settle();
    app.click("preview-0");
    assert!(app.cx.debug_bounds("image-preview").is_some());
    let frame = app.cx.debug_bounds("image-preview").unwrap();
    let image = app.cx.debug_bounds("preview-image").unwrap();
    assert!(image.size.height <= frame.size.height + px(1.));
    assert!(
        app.state
            .read_with(app.cx, |state, _| state.preview.is_some())
    );
}

#[gpui::test]
fn status_from_before_release_cannot_reconnect_the_ui(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    app.state.update(app.cx, |state, _| {
        state.managed = true;
        state.connected = true;
    });
    let generation = app
        .state
        .read_with(app.cx, |state, _| state.service_generation);
    let stale = app
        .backend
        .run(&Settings::default(), Request::Status, &mut |_| {})
        .unwrap();
    app.state
        .update(app.cx, |state, cx| state.request(Request::ReleaseAll, cx));
    app.settle();
    app.state.update(app.cx, |state, cx| {
        state.apply_current_response(Request::Status, stale, generation, cx)
    });
    assert!(!app.state.read_with(app.cx, |state, _| state.connected));
    assert!(
        app.state
            .read_with(app.cx, |state, _| state.server.is_none())
    );
}

#[gpui::test]
fn telemetry_renders_split_and_overlay_modes_with_storage_footer(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);
    app.state.update(app.cx, |state, cx| {
        state.connected = true;
        state.settings.service_url = "https://runtime.example".into();
        state.request(Request::Status, cx);
    });
    app.settle();
    assert!(app.cx.debug_bounds("telemetry-split-charts").is_some());
    assert!(app.cx.debug_bounds("telemetry-vram-chart").is_some());
    assert!(app.cx.debug_bounds("telemetry-ram-chart").is_some());
    assert!(app.cx.debug_bounds("telemetry-disk-chart").is_some());
    assert!(app.cx.debug_bounds("telemetry-storage-footer").is_some());

    app.click("telemetry-mode-toggle");
    assert!(
        app.state
            .read_with(app.cx, |state, _| state.settings.combined_telemetry_chart)
    );
    assert!(app.cx.debug_bounds("telemetry-combined-chart").is_some());

    app.click("telemetry-mode-toggle");
    assert!(
        !app.state
            .read_with(app.cx, |state, _| state.settings.combined_telemetry_chart)
    );
    assert!(app.cx.debug_bounds("telemetry-split-charts").is_some());
}

#[gpui::test]
fn account_switching_clears_managed_connection_and_manual_code_fallback_works(
    cx: &mut TestAppContext,
) {
    let mut app = TestApp::new(cx);
    app.state.update(app.cx, |state, cx| {
        state.managed = true;
        state.connected = true;
        state.settings.service_url = "https://old-account.example".into();
        state.request(Request::Status, cx);
    });
    app.settle();
    app.click("switch-account");
    assert_eq!(
        app.state
            .read_with(app.cx, |state, _| state.account.email.clone()),
        "switched@example.invalid"
    );
    assert!(!app.state.read_with(app.cx, |state, _| state.connected));
    assert!(
        app.state
            .read_with(app.cx, |state, _| state.server.is_none())
    );

    // Logout and test manual auth-code fallback flow
    app.click("logout");
    assert!(
        !app.state
            .read_with(app.cx, |state, _| state.account.logged_in)
    );
    app.click("manual-auth-entry");
    assert!(
        app.state
            .read_with(app.cx, |state, _| state.show_manual_auth)
    );
    app.click("field-auth-code");
    app.cx.simulate_input("manual-code-42");
    app.settle();
    app.click("submit-auth");
    assert_eq!(
        app.state
            .read_with(app.cx, |state, _| state.account.email.clone()),
        "code@example.invalid"
    );
}

#[gpui::test]
fn titlebar_places_controls_top_left_and_drawer_switches_logs_and_terminal(
    cx: &mut TestAppContext,
) {
    let mut app = TestApp::new(cx);
    let titlebar = app.cx.debug_bounds("title-bar").expect("title-bar drawn");
    let controls = app
        .cx
        .debug_bounds("title-bar-controls")
        .expect("top-left window controls drawn");
    assert!(controls.origin.x < titlebar.center().x);
    assert!(controls.origin.y <= titlebar.bottom());
    assert!(app.cx.debug_bounds("gpu-spec-detail").is_some());

    app.click("toggle-logs");
    assert!(app.state.read_with(app.cx, |state, _| {
        state.logs_open && state.drawer_tab == easycolab_gpui::app::DrawerTab::Logs
    }));
    app.click("toggle-terminal");
    assert!(app.state.read_with(app.cx, |state, _| {
        state.logs_open && state.drawer_tab == easycolab_gpui::app::DrawerTab::Terminal
    }));
    assert!(
        app.cx.debug_bounds("embedded-terminal").is_some()
            || app.cx.debug_bounds("terminal-error").is_some()
    );
    app.click("collapse-drawer");
    assert!(!app.state.read_with(app.cx, |state, _| state.logs_open));
}

#[gpui::test]
fn activity_log_records_timestamps_key_actions_and_renders_progress_bars(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);

    // Waking GPU auto-opens the activity log drawer and logs timestamped start + completion
    app.click("wake");
    app.click("confirm-action");
    assert!(app.state.read_with(app.cx, |state, _| {
        state.logs_open && state.drawer_tab == easycolab_gpui::app::DrawerTab::Logs
    }));
    let logs = app.state.read_with(app.cx, |state, _| state.logs.clone());
    assert!(
        logs.iter().all(|line| {
            line.len() > 10
                && line.starts_with('[')
                && line.as_bytes()[3] == b':'
                && line.as_bytes()[6] == b':'
                && line.as_bytes()[9] == b']'
        }),
        "expected [HH:MM:SS] timestamps on all log lines: {logs:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.contains("开始唤醒") || l.contains("Waking")),
        "missing wake start log: {logs:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.contains("GPU 实例已唤醒") || l.contains("GPU runtime awake")),
        "missing wake completion log: {logs:?}"
    );
    assert!(app.cx.debug_bounds("log-text").is_some());
    app.click("copy-logs");
    let copied = app
        .cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default();
    assert!(
        copied.contains("GPU 实例已唤醒") || copied.contains("GPU runtime awake"),
        "expected clipboard to contain activity logs, got: {copied:?}"
    );

    // Active progress bars render in Deploy cards and in the bottom drawer strip
    app.state.update(app.cx, |state, cx| {
        state.route = Route::Deploy;
        state.busy.insert("deploy");
        state.busy.insert("upload");
        state.active_task = Some(easycolab_gpui::app::TaskProgress {
            key: "deploy",
            value: Some(0.42),
            label: "配置 Google Drive 持久化挂载...".into(),
        });
        cx.notify();
    });
    app.settle();
    assert!(app.cx.debug_bounds("deploy-progress").is_some());
    assert!(app.cx.debug_bounds("upload-progress").is_some());
    assert!(app.cx.debug_bounds("task-progress-strip").is_some());
    assert!(app.cx.debug_bounds("drawer-task-progress").is_some());
    assert!(app.cx.debug_bounds("drawer-streaming-badge").is_some());
}

#[gpui::test]
fn strict_lifecycle_prevents_false_ready_state_and_shows_non_blocking_toasts(
    cx: &mut TestAppContext,
) {
    let mut app = TestApp::new(cx);
    // Larger top-left window controls (16px diameter)
    let close = app
        .cx
        .debug_bounds("window-close")
        .expect("window close button drawn");
    assert!(close.size.width >= px(16.) && close.size.height >= px(16.));

    // Even if bridge_managed was persisted true from an older run with no active sessions,
    // the app must NOT claim a GPU runtime is awake/ready!
    app.state.update(app.cx, |state, cx| {
        state.managed = true;
        state.settings.bridge_managed = true;
        state.account.sessions.clear();
        state.active_session = None;
        state.connected = false;
        cx.notify();
    });
    app.settle();
    assert!(
        !app.state
            .read_with(app.cx, |state, _| state.has_awake_runtime())
    );
    assert!(app.cx.debug_bounds("awake-runtime-card").is_none());
    assert!(app.cx.debug_bounds("unverified-runtime-card").is_none());

    // Attempting to deploy without a verified GPU runtime is rejected with a non-blocking toast
    app.state.update(app.cx, |state, cx| state.start_deploy(cx));
    app.settle();
    assert_eq!(app.calls("deploy"), 0);
    assert!(app.cx.debug_bounds("error-message").is_none());
    assert!(app.cx.debug_bounds("notice-message").is_none());
    let toasts = app
        .root
        .read_with(app.cx, |root, cx| root.toaster.read(cx).shown());
    assert!(
        toasts.iter().any(
            |(_, severity)| *severity == Some(ely_gpui_component::primitives::Severity::Danger)
        ),
        "expected non-blocking danger toast: {toasts:?}"
    );
}
