//! Paths shared by source builds and relocatable release bundles.
use std::path::{Path, PathBuf};

pub(crate) fn runtime_root() -> PathBuf {
    std::env::current_exe()
        .map(|executable| root_for_executable(&executable))
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default())
}

fn root_for_executable(executable: &Path) -> PathBuf {
    let directory = executable.parent().unwrap_or_else(|| Path::new("."));
    // Bundles carry python/ beside the executable. Cargo's debug/release and
    // integration-test binaries live two or three levels below the checkout.
    directory
        .ancestors()
        .take(4)
        .find(|root| root.join("python/colab_bridge.py").is_file())
        .unwrap_or(directory)
        .to_path_buf()
}

pub(crate) fn venv_bin(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) {
        ".venv/Scripts"
    } else {
        ".venv/bin"
    })
}

pub(crate) fn python(root: &Path) -> PathBuf {
    venv_bin(root).join(if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    })
}

pub(crate) fn python_setup_command() -> &'static str {
    if cfg!(windows) {
        "powershell -ExecutionPolicy Bypass -File scripts/setup-python.ps1"
    } else {
        "scripts/setup-python.sh"
    }
}

/// Prefer Windows' OpenSSH implementation, whose ProxyCommand uses Windows
/// argument quoting. A Git/MSYS ssh on PATH expects different quoting.
pub(crate) fn openssh_program(name: &str) -> PathBuf {
    #[cfg(windows)]
    if let Some(windows) = std::env::var_os("SystemRoot") {
        let executable = PathBuf::from(windows)
            .join("System32/OpenSSH")
            .join(format!("{name}.exe"));
        if executable.is_file() {
            return executable;
        }
    }
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn relocated_bundle_and_cargo_builds_use_their_own_resources() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("Windows bundle 中文 with spaces");
        fs::create_dir_all(root.join("python")).unwrap();
        fs::write(root.join("python/colab_bridge.py"), "# bundled bridge").unwrap();
        for relative in [
            "easycolab-gpui.exe",
            "target/debug/easycolab-gpui",
            "target/release/easycolab-gpui",
            "target/debug/deps/backend-test",
        ] {
            assert_eq!(root_for_executable(&root.join(relative)), root);
        }
    }

    #[test]
    fn missing_bundle_reports_paths_beside_the_executable() {
        let temporary = TempDir::new().unwrap();
        assert_eq!(
            root_for_executable(&temporary.path().join("easycolab-gpui.exe")),
            temporary.path()
        );
    }
}
