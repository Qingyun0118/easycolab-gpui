$ErrorActionPreference = 'Stop'
$AppArguments = @($args)
$ProjectRoot = $PSScriptRoot
$env:PYTHONUTF8 = '1'
$env:PYTHONIOENCODING = 'utf-8'

if (($AppArguments -notcontains '--mock') -and
    ($AppArguments -notcontains '--help') -and ($AppArguments -notcontains '-h') -and
    -not (Test-Path (Join-Path $ProjectRoot '.venv\Scripts\python.exe'))) {
    & (Join-Path $ProjectRoot 'scripts\setup-python.ps1')
}

Push-Location $ProjectRoot
try {
    $Binary = Join-Path $ProjectRoot 'easycolab-gpui.exe'
    if (Test-Path $Binary) {
        & $Binary @AppArguments
    } else {
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
            throw 'Install Rust and the Visual Studio C++ build tools, then run scripts\setup.ps1. Release ZIP users should keep easycolab-gpui.exe beside this launcher.'
        }
        & cargo +1.98.0 run --locked -- @AppArguments
    }
    $AppExitCode = $LASTEXITCODE
} finally {
    Pop-Location
}
exit $AppExitCode
