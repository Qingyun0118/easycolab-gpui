#!/usr/bin/env bash
# Source this file before invoking cargo directly.
EASYCOLAB_PROJECT_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_HOME="$EASYCOLAB_PROJECT_ROOT/.tooling/cargo"
export RUSTUP_HOME="$EASYCOLAB_PROJECT_ROOT/.tooling/rustup"
export PATH="$CARGO_HOME/bin:$PATH"
if [[ -d "$EASYCOLAB_PROJECT_ROOT/.tooling/sysroot/usr" ]]; then
    EASYCOLAB_NATIVE_ROOT="$EASYCOLAB_PROJECT_ROOT/.tooling/sysroot/usr"
    export PKG_CONFIG_PATH="$EASYCOLAB_NATIVE_ROOT/lib/x86_64-linux-gnu/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    export LIBRARY_PATH="$EASYCOLAB_NATIVE_ROOT/lib/x86_64-linux-gnu${LIBRARY_PATH:+:$LIBRARY_PATH}"
    if [[ -f "$EASYCOLAB_NATIVE_ROOT/include/openssl/ssl.h" ]]; then
        export OPENSSL_DIR="$EASYCOLAB_NATIVE_ROOT"
        export OPENSSL_LIB_DIR="$EASYCOLAB_NATIVE_ROOT/lib/x86_64-linux-gnu"
        export OPENSSL_INCLUDE_DIR="$EASYCOLAB_NATIVE_ROOT/include"
    fi
fi
