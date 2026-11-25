#!/usr/bin/env python3
"""Create a portable release from an explicit, credential-free file list."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parent.parent
TARGETS = ("x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu")
COMMON_FILES = (
    "README.md", "LICENSE", "NOTICE",
    "licenses/EasyColab-Apache-2.0.txt", "licenses/Playground-MIT.txt",
    "python/app.py", "python/colab_bridge.py", "python/runtime_auth.py",
    "python/requirements.txt", "python/SOURCE.json",
    "assets/demo-preview.png", "assets/mock-dashboard.png",
    "assets/mock-preview.png", "assets/mock-workflows.png",
)


def package(binary: Path, target: str, version: str, output: Path, commit: str):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", version):
        raise ValueError("Version must contain only letters, digits, dots, underscores or hyphens")
    if commit != "unknown" and not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise ValueError("Commit must be a full Git SHA")
    if not binary.is_file() or binary.is_symlink():
        raise ValueError("Expected a regular compiled binary")
    output.mkdir(parents=True, exist_ok=True)
    windows = target == "x86_64-pc-windows-msvc"
    name = f"easycolab-gpui-{version}-{target}"
    platform_files = ("run.ps1", "scripts/setup-python.ps1") if windows else (
        "run.sh", "scripts/setup-python.sh", "scripts/env.sh",
    )
    with tempfile.TemporaryDirectory(prefix="easycolab-package-") as temporary:
        bundle = Path(temporary) / name
        bundle.mkdir()
        for relative in COMMON_FILES + platform_files:
            source = ROOT / relative
            if not source.is_file() or source.is_symlink():
                raise ValueError(f"Missing regular release source: {relative}")
            destination = bundle / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
            destination.chmod(0o755 if relative.endswith(".sh") else 0o644)
        executable = bundle / ("easycolab-gpui.exe" if windows else "easycolab-gpui")
        shutil.copyfile(binary, executable)
        executable.chmod(0o755)
        (bundle / "BUILD_INFO.json").write_text(json.dumps({
            "version": version, "target": target, "commit": commit,
        }, indent=2) + "\n", encoding="utf-8")
        # Run outside the checkout, including a path containing spaces. This
        # catches missing runtime DLLs without opening a desktop or an account.
        smoke = Path(temporary) / "relocated smoke test"
        shutil.copytree(bundle, smoke)
        subprocess.run([str(smoke / executable.name), "--help"], cwd=temporary, check=True)
        if windows:
            subprocess.run([
                "powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                str(ROOT / "scripts/smoke-windows.ps1"), "-Binary", str(smoke / executable.name),
            ], cwd=temporary, check=True)
        if windows:
            archive = output / f"{name}.zip"
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as handle:
                for source in sorted(bundle.rglob("*")):
                    if source.is_file():
                        handle.write(source, source.relative_to(bundle.parent))
        else:
            archive = output / f"{name}.tar.gz"
            with tarfile.open(archive, "w:gz") as handle:
                handle.add(bundle, arcname=name)
    with archive.open("rb") as handle:
        digest = hashlib.file_digest(handle, "sha256").hexdigest()
    checksum = archive.with_name(archive.name + ".sha256")
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    print(archive)
    print(checksum)
    return archive


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    parser.add_argument("--commit", default=os.environ.get("GITHUB_SHA", "unknown"))
    options = parser.parse_args()
    package(options.binary.resolve(), options.target, options.version, options.output.resolve(), options.commit)
