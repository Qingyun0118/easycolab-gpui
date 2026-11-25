#!/usr/bin/env bash
set -euo pipefail
EASYCOLAB_PROJECT_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
source "$EASYCOLAB_PROJECT_ROOT/scripts/env.sh"
mkdir -p "$EASYCOLAB_PROJECT_ROOT/.tooling"
if [[ ! -x "$CARGO_HOME/bin/rustc" ]]; then
    curl --proto '=https' --tlsv1.2 -fsSL --retry 2 \
        https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init \
        -o "$EASYCOLAB_PROJECT_ROOT/.tooling/rustup-init"
    chmod +x "$EASYCOLAB_PROJECT_ROOT/.tooling/rustup-init"
    "$EASYCOLAB_PROJECT_ROOT/.tooling/rustup-init" -y --no-modify-path --profile minimal \
        --default-toolchain 1.98.0 --component rustfmt --component clippy
fi
if [[ "$(uname -s)" == Linux ]] && ! pkg-config --exists xkbcommon-x11 openssl; then
    if ! command -v apt-get >/dev/null 2>&1; then
        printf '%s\n' 'Install the development packages for fontconfig, xkbcommon-x11 and OpenSSL using your distribution package manager.' >&2
        exit 1
    fi
    mkdir -p "$EASYCOLAB_PROJECT_ROOT/.tooling/debs" "$EASYCOLAB_PROJECT_ROOT/.tooling/sysroot"
    (
        cd "$EASYCOLAB_PROJECT_ROOT/.tooling/debs"
        apt-get download libxkbcommon-x11-dev libxcb-xkb-dev libssl-dev
        for package in ./*.deb; do
            dpkg-deb -x "$package" "$EASYCOLAB_PROJECT_ROOT/.tooling/sysroot"
        done
    )
    python3 - "$EASYCOLAB_PROJECT_ROOT/.tooling/sysroot" <<'PY'
from pathlib import Path
import sys
root = Path(sys.argv[1])
lib = root / 'usr/lib/x86_64-linux-gnu'
for pc in (lib / 'pkgconfig').glob('*.pc'):
    text = pc.read_text().replace('prefix=/usr', f'prefix={root}/usr')
    pc.write_text(text)
for name, target in {
    'libxkbcommon-x11.so': 'libxkbcommon-x11.so.0',
    'libxcb-xkb.so': 'libxcb-xkb.so.1',
    'libssl.so': 'libssl.so.3',
    'libcrypto.so': 'libcrypto.so.3',
}.items():
    destination = lib / name
    runtime = Path('/usr/lib/x86_64-linux-gnu') / target
    if destination.is_symlink() and not destination.exists() and runtime.exists():
        destination.unlink()
        destination.symlink_to(runtime)
PY
    source "$EASYCOLAB_PROJECT_ROOT/scripts/env.sh"
fi
pkg-config --exists fontconfig xkbcommon-x11 openssl
rustc --version
printf '%s\n' 'Build environment is ready. Run ./run.sh --mock to preview without a Google account.'
