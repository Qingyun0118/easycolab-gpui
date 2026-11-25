//! Contract tests use only a local Python stub and loopback HTTP server.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use easycolab_gpui::{
    backend::{Backend, BridgeEvent, MockBackend, RealBackend},
    models::*,
    settings::Settings,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use tempfile::TempDir;

fn settings(directory: &TempDir) -> Settings {
    Settings {
        management_password: "management-password-test-12345".into(),
        hf_token: "hf_test_secret_token".into(),
        output_dir: directory.path().join("outputs"),
        ..Default::default()
    }
}

fn stub_bridge(directory: &TempDir, script: &str) -> RealBackend {
    let bridge = directory.path().join("bridge with spaces.py");
    let app = directory.path().join("app.py");
    fs::write(&bridge, script).unwrap();
    fs::write(&app, "# test cloud app").unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let python = std::env::var_os("EASYCOLAB_TEST_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let bundled = if cfg!(windows) {
                root.join(".venv/Scripts/python.exe")
            } else {
                root.join(".venv/bin/python")
            };
            if bundled.is_file() {
                bundled
            } else {
                let executable = if cfg!(windows) {
                    "python.exe"
                } else {
                    "python3"
                };
                let output = std::process::Command::new(executable)
                    .args(["-c", "import sys; print(sys.executable)"])
                    .output()
                    .expect("Install Python or set EASYCOLAB_TEST_PYTHON for bridge tests");
                assert!(
                    output.status.success(),
                    "Could not locate a test Python interpreter"
                );
                PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
            }
        });
    RealBackend::with_bridge(python, bridge, app)
}

fn request(
    backend: &dyn Backend,
    settings: &Settings,
    request: Request,
) -> anyhow::Result<Response> {
    backend.run(settings, request, &mut |_| {})
}

/// One HTTP exchange, capturing the complete request without third-party servers.
fn server(
    status: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (String, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let body = body.to_vec();
    let header_text: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let status = status.to_owned();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 4096];
        let mut wanted = None;
        loop {
            let n = stream.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..n]);
            if wanted.is_none()
                && let Some(index) = received.windows(4).position(|w| w == b"\r\n\r\n")
            {
                let header = String::from_utf8_lossy(&received[..index]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|n| n.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                wanted = Some(index + 4 + length);
            }
            if wanted.is_some_and(|wanted| received.len() >= wanted) {
                break;
            }
        }
        let _ = tx.send(received);
        let length = if header_text.to_ascii_lowercase().contains("content-length:") {
            String::new()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        let response =
            format!("HTTP/1.1 {status}\r\n{length}{header_text}Connection: close\r\n\r\n");
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
    });
    (url, rx, handle)
}

fn http_backend() -> RealBackend {
    RealBackend::with_bridge(PathBuf::new(), PathBuf::new(), PathBuf::new())
}

#[test]
fn bridge_json_failure_is_an_error_even_with_exit_zero_and_secrets_are_redacted() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let backend = stub_bridge(
        &directory,
        "import json,sys\nprint('management-password-test-12345 hf_test_secret_token code-secret',file=sys.stderr)\nprint(json.dumps({'ok':False,'error':'failed management-password-test-12345 hf_test_secret_token code-secret'}))\n",
    );
    let mut logs = Vec::new();
    let error = backend
        .run(
            &settings,
            Request::AuthCode("code-secret".into()),
            &mut |line| logs.push(line),
        )
        .unwrap_err()
        .to_string();
    for secret in [
        &settings.management_password,
        &settings.hf_token,
        &"code-secret".to_owned(),
    ] {
        assert!(!error.contains(secret));
        assert!(!logs.join("\n").contains(secret));
    }
    assert!(error.contains("failed"));
}

#[test]
fn bridge_takes_literal_arguments_and_parses_last_json_result() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let code = "code with spaces; $(not-a-shell) ' quote";
    let source = format!(
        "import json,sys\nassert sys.argv[1]=='auth_code'\nassert sys.argv[2]=={}\nprint('non-json startup notice')\nprint(json.dumps({{'ok':True,'logged_in':True,'email':'unit@example.test','paid_balance':42.5}}))\n",
        serde_json::to_string(code).unwrap()
    );
    let backend = stub_bridge(&directory, &source);
    let Response::Account(account) =
        request(&backend, &settings, Request::AuthCode(code.into())).unwrap()
    else {
        panic!("Expected account");
    };
    assert!(account.logged_in);
    assert_eq!(account.email, "unit@example.test");
    assert_eq!(account.paid_balance, Some(42.5));
}

#[test]
fn wake_allocates_session_without_deploying_and_deploy_passes_password_and_idle() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    settings.idle_timeout_minutes = 45;
    let wake_backend = stub_bridge(
        &directory,
        "import json,os,sys\nassert sys.argv[1:4]==['wake','--gpu','G4']\nassert 'EASYCOLAB_ADMIN_PASSWORD' not in os.environ\nprint('WAKE_LOG: GPU ready',flush=True)\nprint('WAKE_SESSION:'+json.dumps({'name':'easycolab','endpoint':'ep-1','accelerator':'G4','variant':'GPU','shape':'HIGH_RAM'}),flush=True)\n",
    );
    let mut wake_logs = Vec::new();
    let Response::Woken(session) = wake_backend
        .run(&settings, Request::Wake, &mut |line| wake_logs.push(line))
        .unwrap()
    else {
        panic!("Expected woken session");
    };
    assert_eq!(session.accelerator, "G4");
    assert_eq!(wake_logs, ["GPU ready"]);

    let deploy_backend = stub_bridge(
        &directory,
        "import os,sys\nassert os.environ['EASYCOLAB_IDLE_TIMEOUT_MINUTES']=='45'\nassert sys.argv[1:3]==['deploy','--app-py']\nprint('WAKE_LOG: deploy '+os.environ['EASYCOLAB_ADMIN_PASSWORD'],flush=True)\nprint('WAKE_URL:https://example.gradio.live',flush=True)\n",
    );
    let mut deploy_logs = Vec::new();
    let Response::Deployed(url) = deploy_backend
        .run(&settings, Request::Deploy, &mut |line| {
            deploy_logs.push(line)
        })
        .unwrap()
    else {
        panic!("Expected deployment");
    };
    assert_eq!(url, "https://example.gradio.live");
    assert_eq!(deploy_logs, ["deploy ***"]);
}

#[test]
fn upload_project_validates_local_path_and_passes_remote_subdir() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let project_dir = directory.path().join("my_proj");
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(project_dir.join("main.py"), "print('hi')").unwrap();
    let backend = stub_bridge(
        &directory,
        "import json,sys\nassert sys.argv[1]=='upload'\nassert sys.argv[2]=='--local'\nassert sys.argv[4]=='--remote'\nassert sys.argv[5]=='custom_app'\nassert sys.argv[6]=='--target'\ntarget=sys.argv[7]\nbase='/content/drive/MyDrive' if target=='drive' else '/content'\nprint(json.dumps({'ok':True,'remote_path':f'{base}/custom_app','files':1}))\n",
    );
    let Response::Uploaded { remote_path, files } = request(
        &backend,
        &settings,
        Request::UploadProject {
            local_path: project_dir.clone(),
            remote_dir: "/content/custom_app/".into(),
            target: UploadTarget::Runtime,
        },
    )
    .unwrap() else {
        panic!("Expected upload response");
    };
    assert_eq!(remote_path, "/content/custom_app");
    assert_eq!(files, 1);

    let Response::Uploaded { remote_path, files } = request(
        &backend,
        &settings,
        Request::UploadProject {
            local_path: project_dir,
            remote_dir: "/content/drive/MyDrive/custom_app/".into(),
            target: UploadTarget::Drive,
        },
    )
    .unwrap() else {
        panic!("Expected drive upload response");
    };
    assert_eq!(remote_path, "/content/drive/MyDrive/custom_app");
    assert_eq!(files, 1);

    assert!(
        request(
            &backend,
            &settings,
            Request::UploadProject {
                local_path: directory.path().join("missing"),
                remote_dir: "custom_app".into(),
                target: UploadTarget::Runtime,
            },
        )
        .is_err()
    );
}

#[test]
fn wake_error_marker_is_failure_even_with_exit_zero() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let backend = stub_bridge(&directory, "print('WAKE_ERROR: out of quota')\n");
    assert!(
        request(&backend, &settings, Request::Wake)
            .unwrap_err()
            .to_string()
            .contains("out of quota")
    );
}

#[test]
fn release_does_not_report_success_when_bridge_still_lists_active_runtimes() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let backend = stub_bridge(
        &directory,
        "import json\nprint(json.dumps({'ok':True,'active_assignments':1,'message':'All runtimes released'}))\n",
    );
    let error = request(&backend, &settings, Request::ReleaseAll)
        .unwrap_err()
        .to_string();
    assert!(error.contains("1 Colab runtime"));
    assert!(error.contains("Refresh account status"));
}

#[test]
fn heartbeat_defers_while_deployment_owns_the_bridge_store() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let signal = directory.path().join("deployment-started");
    let source = format!(
        "import json,pathlib,time\npathlib.Path({}).touch()\ntime.sleep(0.5)\nprint('WAKE_SESSION:'+json.dumps({{'name':'easycolab','endpoint':'ep-1','accelerator':'G4','variant':'GPU','shape':'HIGH_RAM'}}))\n",
        serde_json::to_string(&signal.to_string_lossy()).unwrap()
    );
    let backend = std::sync::Arc::new(stub_bridge(&directory, &source));
    let worker = backend.clone();
    let worker_settings = settings.clone();
    let deployment =
        thread::spawn(move || request(worker.as_ref(), &worker_settings, Request::Wake));
    for _ in 0..100 {
        if signal.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(signal.exists());
    let started = std::time::Instant::now();
    let Response::Done(message) = request(backend.as_ref(), &settings, Request::Keepalive).unwrap()
    else {
        panic!();
    };
    assert!(message.contains("deferred"));
    assert!(started.elapsed() < Duration::from_millis(300));
    assert!(matches!(
        deployment.join().unwrap().unwrap(),
        Response::Woken(_)
    ));
}

#[test]
fn missing_python_reports_the_setup_command() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let error = request(
        &RealBackend::from_root(directory.path()),
        &settings,
        Request::AuthStatus,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains(if cfg!(windows) {
        "setup-python.ps1"
    } else {
        "setup-python.sh"
    }));
}

#[test]
fn status_uses_basic_auth_and_deserializes_forward_compatible_fields() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    let body = json!({"ok":true,"gpu_name":"test GPU","vram_total_gb":96.0,"future_field":7,"console_lines":[settings.management_password],"comfyui":{"is_running":true,"public_url":"https://canvas.example.test","queue_pending":2}});
    let (url, rx, handle) = server(
        "200 OK",
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    );
    settings.service_url = url;
    let Response::Status(status) = request(&http_backend(), &settings, Request::Status).unwrap()
    else {
        panic!("Expected status");
    };
    assert_eq!(status.gpu_name, "test GPU");
    assert_eq!(status.vram_total_gb, Some(96.0));
    assert!(status.comfyui.is_running);
    assert_eq!(status.comfyui.queue_pending, 2);
    assert_eq!(status.console_lines, ["***"]);
    let captured = String::from_utf8(rx.recv().unwrap())
        .unwrap()
        .to_ascii_lowercase();
    assert!(captured.starts_with("get /minimax_api/status http/1.1"));
    assert!(captured.contains(&format!(
            "authorization: basic {}",
            STANDARD
                .encode(format!("easycolab:{}", settings.management_password))
                .to_ascii_lowercase()
        )));
    handle.join().unwrap();
}

#[test]
fn every_action_maps_to_the_existing_http_contract() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    let cases = [
        (
            ComfyAction::Start {
                force_restart: true,
            },
            "/minimax_api/comfyui/start",
            json!({"unload_existing":false,"force_restart":true}),
        ),
        (ComfyAction::Stop, "/minimax_api/comfyui/stop", json!({})),
        (
            ComfyAction::FreeVram,
            "/minimax_api/comfyui/free_vram",
            json!({}),
        ),
        (
            ComfyAction::SyncDrive,
            "/minimax_api/comfyui/sync_drive",
            json!({}),
        ),
        (
            ComfyAction::DownloadWeight {
                source: "owner/model".into(),
                filename: "weights.bin".into(),
                subfolder: "loras".into(),
            },
            "/minimax_api/comfyui/download_weight",
            json!({"url_or_repo":"owner/model","filename":"weights.bin","subfolder":"loras","hf_token":settings.hf_token}),
        ),
        (
            ComfyAction::InstallPack {
                pack_id: "wan22_i2v_flf2v".into(),
            },
            "/minimax_api/comfyui/install_pack",
            json!({"pack_id":"wan22_i2v_flf2v","auto_start":true,"hf_token":settings.hf_token}),
        ),
        (
            ComfyAction::SetIdle,
            "/minimax_api/config_idle",
            json!({"idle_timeout_minutes":30}),
        ),
        (
            ComfyAction::Disconnect,
            "/minimax_api/disconnect",
            json!({}),
        ),
    ];
    for (action, path, expected) in cases {
        let (url, rx, handle) = server("200 OK", &[], b"{\"ok\":true,\"comfyui\":{}}");
        settings.service_url = url;
        request(&http_backend(), &settings, Request::Action(action)).unwrap();
        let captured = String::from_utf8(rx.recv().unwrap()).unwrap();
        assert!(
            captured.starts_with(&format!("POST {path} HTTP/1.1")),
            "{captured}"
        );
        let body = captured.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(serde_json::from_str::<Value>(body).unwrap(), expected);
        handle.join().unwrap();
    }
}

#[test]
fn http_business_failure_and_auth_failure_are_reported_without_secrets() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    for (status, body, expected) in [
        (
            "200 OK",
            format!(
                "{{\"ok\":true,\"action_ok\":false,\"action_message\":\"failed {}\"}}",
                settings.hf_token
            ),
            "failed",
        ),
        (
            "401 Unauthorized",
            format!("wrong password {}", settings.management_password),
            "401",
        ),
    ] {
        let (url, _, handle) = server(status, &[], body.as_bytes());
        settings.service_url = url;
        let error = request(
            &http_backend(),
            &settings,
            Request::Action(ComfyAction::FreeVram),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(expected));
        assert!(!error.contains(&settings.management_password));
        assert!(!error.contains(&settings.hf_token));
        handle.join().unwrap();
    }
}

#[test]
fn disconnect_preserves_server_message_and_rejects_legacy_release_failures() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    for message in [
        "⚠️ 释放运行时失败: quota backend unavailable",
        "Failed to release runtime: backend unavailable",
    ] {
        let body = json!({"ok":true,"message":message}).to_string();
        let (url, _, handle) = server("200 OK", &[], body.as_bytes());
        settings.service_url = url;
        let error = request(
            &http_backend(),
            &settings,
            Request::Action(ComfyAction::Disconnect),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(message));
        handle.join().unwrap();
    }
    let message = "已向 Colab 发送释放运行时指令";
    let body = json!({"ok":true,"message":message}).to_string();
    let (url, _, handle) = server("200 OK", &[], body.as_bytes());
    settings.service_url = url;
    let Response::Done(actual) = request(
        &http_backend(),
        &settings,
        Request::Action(ComfyAction::Disconnect),
    )
    .unwrap() else {
        panic!("Expected request acknowledgement");
    };
    assert_eq!(actual, message);
    handle.join().unwrap();
}

#[test]
fn credential_bearing_requests_never_follow_redirects() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let location = format!("http://{}/stolen", target.local_addr().unwrap());
    let (url, _, handle) = server("302 Found", &[("Location", &location)], b"");
    settings.service_url = url;
    assert!(
        request(&http_backend(), &settings, Request::Status)
            .unwrap_err()
            .to_string()
            .contains("302")
    );
    handle.join().unwrap();
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn refuses_non_https_remote_service_and_embedded_credentials() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    for url in [
        "http://example.test",
        "https://user:password@example.test",
        "https://example.test/#fragment",
    ] {
        settings.service_url = url.into();
        assert!(request(&http_backend(), &settings, Request::Status).is_err());
    }
}

#[test]
fn output_download_encodes_relative_path_and_saves_complete_file_inside_output_dir() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    let (url, rx, handle) = server(
        "200 OK",
        &[("Content-Type", "image/png")],
        b"complete-output",
    );
    settings.service_url = url;
    let output = RemoteOutput {
        rel_path: "folder/hello #1.png".into(),
        filename: "hello.png".into(),
        ..Default::default()
    };
    let Response::Downloaded(path) =
        request(&http_backend(), &settings, Request::Download(output)).unwrap()
    else {
        panic!("Expected file");
    };
    assert_eq!(fs::read(&path).unwrap(), b"complete-output");
    assert_eq!(
        path.parent().unwrap(),
        settings.output_dir.canonicalize().unwrap()
    );
    assert_eq!(path.file_name().unwrap(), "comfy_hello.png");
    let captured = String::from_utf8(rx.recv().unwrap()).unwrap();
    assert!(
        captured
            .starts_with("GET /minimax_api/comfyui/output_media/folder/hello%20%231.png HTTP/1.1")
    );
    assert_eq!(fs::read_dir(&settings.output_dir).unwrap().count(), 1);
    handle.join().unwrap();
}

#[test]
fn output_download_rejects_traversal_and_absolute_paths_before_network() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    for path in [
        "../outside.png",
        "/absolute.png",
        "nested/../outside.png",
        "a\\..\\outside",
        "C:/outside.png",
        "https://bad.test/out.png",
        "./file.png",
        "a//b",
    ] {
        let output = RemoteOutput {
            rel_path: path.into(),
            filename: "file.png".into(),
            ..Default::default()
        };
        assert!(
            request(&http_backend(), &settings, Request::Download(output)).is_err(),
            "accepted {path}"
        );
    }
    let output = RemoteOutput {
        rel_path: "normal.png".into(),
        filename: "../outside.png".into(),
        ..Default::default()
    };
    assert!(request(&http_backend(), &settings, Request::Download(output)).is_err());
    assert!(!settings.output_dir.exists());
}

#[test]
fn incomplete_download_cleans_temporary_file_and_preserves_existing_output() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    fs::create_dir_all(&settings.output_dir).unwrap();
    let destination = settings.output_dir.join("comfy_file.png");
    fs::write(&destination, b"old output").unwrap();
    let (url, _, handle) = server("200 OK", &[("Content-Length", "1000")], b"partial");
    settings.service_url = url;
    let output = RemoteOutput {
        rel_path: "file.png".into(),
        filename: "file.png".into(),
        ..Default::default()
    };
    assert!(request(&http_backend(), &settings, Request::Download(output)).is_err());
    assert_eq!(fs::read(&destination).unwrap(), b"old output");
    assert_eq!(fs::read_dir(&settings.output_dir).unwrap().count(), 1);
    handle.join().unwrap();
}

#[test]
fn repeated_downloads_keep_both_local_files() {
    let directory = TempDir::new().unwrap();
    let mut settings = settings(&directory);
    let mut paths = Vec::new();
    for body in [b"first output".as_slice(), b"second output".as_slice()] {
        let (url, _, handle) = server("200 OK", &[], body);
        settings.service_url = url;
        let output = RemoteOutput {
            rel_path: "nested/file.png".into(),
            filename: "file.png".into(),
            ..Default::default()
        };
        let Response::Downloaded(path) =
            request(&http_backend(), &settings, Request::Download(output)).unwrap()
        else {
            panic!();
        };
        paths.push(path);
        handle.join().unwrap();
    }
    assert_ne!(paths[0], paths[1]);
    assert_eq!(fs::read(&paths[0]).unwrap(), b"first output");
    assert_eq!(fs::read(&paths[1]).unwrap(), b"second output");
    assert_eq!(paths[1].file_name().unwrap(), "comfy_file (2).png");
}

#[test]
fn mock_exercises_offline_lifecycle_install_and_local_download() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let backend = MockBackend::default();
    let Response::Account(account) = request(&backend, &settings, Request::AuthStatus).unwrap()
    else {
        panic!();
    };
    assert!(account.logged_in);
    assert!(matches!(
        request(&backend, &settings, Request::Wake).unwrap(),
        Response::Woken(_)
    ));
    assert!(matches!(
        request(&backend, &settings, Request::Deploy).unwrap(),
        Response::Deployed(_)
    ));
    let Response::Status(status) = request(
        &backend,
        &settings,
        Request::Action(ComfyAction::Start {
            force_restart: false,
        }),
    )
    .unwrap() else {
        panic!();
    };
    assert!(status.comfyui.is_starting);
    thread::sleep(Duration::from_millis(1900));
    let Response::Status(status) = request(&backend, &settings, Request::Status).unwrap() else {
        panic!();
    };
    assert!(status.comfyui.is_running);
    assert_eq!(status.comfyui.preset_packs.len(), 4);
    assert!(!status.comfyui.recent_outputs.is_empty());
    let output = status.comfyui.recent_outputs[0].clone();
    let Response::Downloaded(path) =
        request(&backend, &settings, Request::Download(output)).unwrap()
    else {
        panic!();
    };
    let decoder = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut pixels).unwrap();
    assert_eq!((info.width, info.height), (960, 540));
    assert!(info.buffer_size() > 0);
    request(
        &backend,
        &settings,
        Request::Action(ComfyAction::InstallPack {
            pack_id: "wan22_i2v_flf2v".into(),
        }),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(1900));
    let Response::Status(status) = request(&backend, &settings, Request::Status).unwrap() else {
        panic!();
    };
    assert!(status.comfyui.preset_packs[0].ready);
    assert!(status.comfyui.models_count > 0);
    request(&backend, &settings, Request::ReleaseAll).unwrap();
    assert!(request(&backend, &settings, Request::Status).is_err());
    request(&backend, &settings, Request::Logout).unwrap();
    assert!(request(&backend, &settings, Request::Wake).is_err());
}

#[test]
fn real_backend_streams_auth_login_url_and_supports_cancellation() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let backend = stub_bridge(
        &directory,
        r#"
import json, sys, time
cmd = sys.argv[1]
if cmd == "auth_login":
    print("AUTH_URL:https://accounts.google.com/o/oauth2/auth?state=abc", flush=True)
    time.sleep(0.05)
    print(json.dumps({"ok": True, "logged_in": True, "email": "real@example.com"}), flush=True)
"#,
    );
    let cancel = AtomicBool::new(false);
    let mut events = Vec::new();
    let res = backend
        .run_with_events(&settings, Request::AuthLogin, &cancel, &mut |ev| {
            events.push(ev)
        })
        .unwrap();
    let Response::Account(account) = res else {
        panic!("expected account");
    };
    assert!(account.logged_in);
    assert_eq!(account.email, "real@example.com");
    assert!(events.iter().any(|ev| matches!(
        ev,
        BridgeEvent::AuthUrl(url) if url == "https://accounts.google.com/o/oauth2/auth?state=abc"
    )));

    // Cancellation interrupts a waiting auth_login child quickly
    let slow_backend = stub_bridge(
        &directory,
        r#"
import sys, time
print("AUTH_URL:https://accounts.google.com/o/oauth2/auth?state=slow", flush=True)
time.sleep(10)
"#,
    );
    let cancel_slow = std::sync::Arc::new(AtomicBool::new(false));
    let cancel_trigger = cancel_slow.clone();
    let err = slow_backend
        .run_with_events(&settings, Request::AuthLogin, &cancel_slow, &mut |ev| {
            if matches!(ev, BridgeEvent::AuthUrl(_)) {
                cancel_trigger.store(true, Ordering::SeqCst);
            }
        })
        .unwrap_err();
    assert!(err.to_string().contains("cancelled"));
}

#[test]
fn real_backend_parses_wake_progress_events_without_leaking_markers_to_logs() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let backend = stub_bridge(
        &directory,
        r#"
import json, sys
cmd = sys.argv[1]
if cmd == "upload":
    print("WAKE_PROGRESS:0.100|开始打包 2 个本地文件...", flush=True)
    print("WAKE_LOG: 正在上传 2 个文件...", flush=True)
    print("WAKE_PROGRESS:-|等待云端解压响应...", flush=True)
    print("WAKE_PROGRESS:1.000|已上传 2 个文件至 /content/demo", flush=True)
    print(json.dumps({"ok": True, "remote_path": "/content/demo", "files": 2}), flush=True)
"#,
    );
    let cancel = AtomicBool::new(false);
    let mut events = Vec::new();
    let res = backend
        .run_with_events(
            &settings,
            Request::UploadProject {
                local_path: directory.path().to_path_buf(),
                remote_dir: "demo".into(),
                target: UploadTarget::Runtime,
            },
            &cancel,
            &mut |ev| events.push(ev),
        )
        .unwrap();
    let Response::Uploaded { remote_path, files } = res else {
        panic!("expected uploaded response");
    };
    assert_eq!(remote_path, "/content/demo");
    assert_eq!(files, 2);
    assert_eq!(
        events,
        vec![
            BridgeEvent::Progress {
                value: Some(0.1),
                label: "开始打包 2 个本地文件...".into(),
            },
            BridgeEvent::Log("正在上传 2 个文件...".into()),
            BridgeEvent::Progress {
                value: None,
                label: "等待云端解压响应...".into(),
            },
            BridgeEvent::Progress {
                value: Some(1.0),
                label: "已上传 2 个文件至 /content/demo".into(),
            },
        ]
    );
}

#[test]
fn real_backend_fetches_kernel_runtime_metrics_without_comfyui() {
    let directory = TempDir::new().unwrap();
    let settings = settings(&directory);
    let backend = stub_bridge(
        &directory,
        r#"
import json, sys
if sys.argv[1] == "metrics":
    print(json.dumps({
        "ok": True,
        "gpu_name": "NVIDIA RTX PRO 6000",
        "vram_total_gb": 96.0,
        "vram_used_gb": 12.5,
        "vram_free_gb": 83.5,
        "ram_total_gb": 176.0,
        "ram_used_gb": 18.0,
        "ram_free_gb": 158.0,
        "disk_total_gb": 235.0,
        "disk_used_gb": 40.0,
        "disk_free_gb": 195.0,
        "shm_total_gb": 80.0,
        "shm_used_gb": 1.0,
        "shm_free_gb": 79.0
    }), flush=True)
"#,
    );
    let Response::RuntimeMetrics(metrics) =
        request(&backend, &settings, Request::RuntimeMetrics).unwrap()
    else {
        panic!("expected RuntimeMetrics");
    };
    assert_eq!(metrics.gpu_name, "NVIDIA RTX PRO 6000");
    assert_eq!(metrics.vram_total_gb, Some(96.0));
    assert_eq!(metrics.vram_used_gb, Some(12.5));
    assert_eq!(metrics.ram_total_gb, Some(176.0));
    assert_eq!(metrics.disk_free_gb, Some(195.0));
    assert_eq!(metrics.shm_total_gb, Some(80.0));
}
