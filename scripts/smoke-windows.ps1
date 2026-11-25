param([Parameter(Mandatory = $true)][string]$Binary)
$ErrorActionPreference = 'Stop'
$Temporary = Join-Path ([IO.Path]::GetTempPath()) ('easycolab-smoke-' + [Guid]::NewGuid())
$PreviousConfig = $env:EASYCOLAB_GPUI_CONFIG_DIR
$Process = $null
New-Item -ItemType Directory -Path $Temporary | Out-Null
try {
    $env:EASYCOLAB_GPUI_CONFIG_DIR = Join-Path $Temporary 'config'
    $Process = Start-Process -FilePath $Binary -ArgumentList '--mock' -PassThru `
        -RedirectStandardOutput (Join-Path $Temporary 'stdout.log') `
        -RedirectStandardError (Join-Path $Temporary 'stderr.log')
    for ($Attempt = 0; $Attempt -lt 20; $Attempt++) {
        Start-Sleep -Milliseconds 500
        $Process.Refresh()
        if ($Process.HasExited) {
            Get-Content (Join-Path $Temporary 'stderr.log')
            throw "The packaged Windows application exited during startup ($($Process.ExitCode))."
        }
        if ($Process.MainWindowHandle -ne 0) { break }
    }
    if ($Process.MainWindowHandle -eq 0) {
        Get-Content (Join-Path $Temporary 'stderr.log')
        throw 'The packaged Windows application did not create its demo window.'
    }
    Write-Host 'Relocated Windows release opened its demo window successfully.'
} finally {
    if ($Process -and -not $Process.HasExited) { Stop-Process -Id $Process.Id -Force }
    $env:EASYCOLAB_GPUI_CONFIG_DIR = $PreviousConfig
    Remove-Item $Temporary -Recurse -Force
}
