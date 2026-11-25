#!/usr/bin/env bash
set -euo pipefail
EASYCOLAB_PROJECT_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source "$EASYCOLAB_PROJECT_ROOT/scripts/env.sh"
if [[ ! -x "$EASYCOLAB_PROJECT_ROOT/easycolab-gpui" ]] && ! command -v cargo >/dev/null 2>&1; then
    "$EASYCOLAB_PROJECT_ROOT/scripts/setup.sh"
    source "$EASYCOLAB_PROJECT_ROOT/scripts/env.sh"
fi
EASYCOLAB_MOCK=false
for argument in "$@"; do
    if [[ "$argument" == --mock || "$argument" == --help || "$argument" == -h ]]; then EASYCOLAB_MOCK=true; fi
done
if [[ "$EASYCOLAB_MOCK" == false ]] && [[ ! -x "$EASYCOLAB_PROJECT_ROOT/.venv/bin/python" ]]; then
    "$EASYCOLAB_PROJECT_ROOT/scripts/setup-python.sh"
fi
cd "$EASYCOLAB_PROJECT_ROOT"
if [[ -x "$EASYCOLAB_PROJECT_ROOT/easycolab-gpui" ]]; then
    exec "$EASYCOLAB_PROJECT_ROOT/easycolab-gpui" "$@"
fi
exec cargo run --locked -- "$@"
