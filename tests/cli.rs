use std::process::Command;

#[test]
fn conflicting_backend_flags_are_rejected_before_opening_a_window() {
    for arguments in [["--mock", "--real"], ["--real", "--mock"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_easycolab-gpui"))
            .args(arguments)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("not both"));
    }
}

#[test]
fn help_and_unknown_arguments_work_without_a_display_or_account() {
    let help = Command::new(env!("CARGO_BIN_EXE_easycolab-gpui"))
        .arg("--help")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--mock"));
    let unknown = Command::new(env!("CARGO_BIN_EXE_easycolab-gpui"))
        .arg("--mok")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(2));
}
