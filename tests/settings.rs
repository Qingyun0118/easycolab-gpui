use easycolab_gpui::{models::ServerStatus, settings::Settings};
use std::fs;

#[test]
fn settings_persist_secrets_privately_and_redact_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config/settings.json");
    let settings = Settings {
        management_password: "management-private".into(),
        hf_token: "hf_private".into(),
        service_url: "https://service.example".into(),
        ..Settings::default()
    };
    settings.save_to(&path).unwrap();
    let restored = Settings::load_from(&path).unwrap();
    assert_eq!(restored.management_password, settings.management_password);
    assert_eq!(restored.service_url, settings.service_url);
    assert_eq!(
        restored.redact("management-private / hf_private"),
        "*** / ***"
    );
    // Replacing an existing settings file must also work on Windows.
    let updated = Settings {
        idle_timeout_minutes: 45,
        ..restored
    };
    updated.save_to(&path).unwrap();
    assert_eq!(Settings::load_from(&path).unwrap().idle_timeout_minutes, 45);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn malformed_settings_are_reported_and_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.json");
    fs::write(&path, "broken JSON").unwrap();
    assert!(Settings::load_from(&path).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "broken JSON");
}

#[test]
fn omitted_metrics_are_unknown_and_extra_server_fields_are_accepted() {
    let status: ServerStatus = serde_json::from_str(
        r#"{"ok":true,"future_field":1,"comfyui":{"is_running":true,"future_field":2}}"#,
    )
    .unwrap();
    assert!(status.comfyui.is_running);
    assert_eq!(status.vram_used_gb, None);
    assert_eq!(status.session_cu_used, None);
}
