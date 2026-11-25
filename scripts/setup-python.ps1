$ErrorActionPreference = 'Stop'
$ProjectRoot = Split-Path -Parent $PSScriptRoot
$VenvPython = Join-Path $ProjectRoot '.venv\Scripts\python.exe'

if (-not (Test-Path $VenvPython)) {
    $PythonArguments = @()
    if ($env:EASYCOLAB_PYTHON) {
        $Python = $env:EASYCOLAB_PYTHON
    } elseif (Get-Command py -ErrorAction SilentlyContinue) {
        $Python = 'py'
        $PythonArguments = @('-3.12')
    } elseif (Get-Command python -ErrorAction SilentlyContinue) {
        $Python = 'python'
    } else {
        throw 'Install Python 3.12 (64-bit) from python.org, or set EASYCOLAB_PYTHON to its python.exe path.'
    }
    & $Python @PythonArguments -c "import sys, struct; assert sys.version_info >= (3, 12), 'Python 3.12 or later is required'; assert struct.calcsize('P') == 8, '64-bit Python is required'"
    if ($LASTEXITCODE -ne 0) { throw 'Python is unavailable. Install Python 3.12 (64-bit), or set EASYCOLAB_PYTHON.' }
    & $Python @PythonArguments -m venv (Join-Path $ProjectRoot '.venv')
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the project Python environment.' }
}
& $VenvPython -c "import sys, struct; assert sys.version_info >= (3, 12), 'Delete .venv and rerun setup with Python 3.12 or later'; assert struct.calcsize('P') == 8, '64-bit Python is required'"
if ($LASTEXITCODE -ne 0) { throw 'The existing .venv is incompatible. Delete it and rerun this setup script with Python 3.12 (64-bit).' }
& $VenvPython -m pip install --disable-pip-version-check -r (Join-Path $ProjectRoot 'python\requirements.txt')
if ($LASTEXITCODE -ne 0) { throw 'Could not install Python bridge dependencies.' }
Write-Host 'Local Python bridge environment is ready. No Google account was accessed.'
