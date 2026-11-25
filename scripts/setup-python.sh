#!/usr/bin/env bash
set -euo pipefail
EASYCOLAB_PROJECT_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ ! -x "$EASYCOLAB_PROJECT_ROOT/.venv/bin/python" ]]; then
    if command -v uv >/dev/null 2>&1; then
        uv venv "$EASYCOLAB_PROJECT_ROOT/.venv" --python 3.12
    else
        EASYCOLAB_SYSTEM_PYTHON="${EASYCOLAB_PYTHON:-python3}"
        "$EASYCOLAB_SYSTEM_PYTHON" -c 'import sys; assert sys.version_info >= (3, 12), "Install Python 3.12 or later, or use uv"'
        "$EASYCOLAB_SYSTEM_PYTHON" -m venv "$EASYCOLAB_PROJECT_ROOT/.venv"
    fi
fi
"$EASYCOLAB_PROJECT_ROOT/.venv/bin/python" -c 'import sys; assert sys.version_info >= (3, 12), "Delete .venv and rerun setup with Python 3.12 or later"'
if command -v uv >/dev/null 2>&1; then
    uv pip install --python "$EASYCOLAB_PROJECT_ROOT/.venv/bin/python" -r "$EASYCOLAB_PROJECT_ROOT/python/requirements.txt"
else
    "$EASYCOLAB_PROJECT_ROOT/.venv/bin/python" -m pip install -r "$EASYCOLAB_PROJECT_ROOT/python/requirements.txt"
fi
printf '%s\n' 'Local Python bridge environment is ready. No Google account was accessed.'
