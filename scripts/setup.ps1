$ErrorActionPreference = 'Stop'
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    throw 'Install Rust with the x86_64-pc-windows-msvc toolchain from https://rustup.rs/ and Visual Studio 2022 Build Tools (Desktop development with C++, including the Windows SDK), then reopen PowerShell.'
}
& rustup toolchain install 1.98.0 --profile minimal --component rustfmt --component clippy
if ($LASTEXITCODE -ne 0) { throw 'Could not install the Rust 1.98.0 toolchain.' }
& rustup target add --toolchain 1.98.0 x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw 'Could not install the Windows MSVC target.' }
Write-Host 'Build environment is ready. Run .\run.ps1 --mock to preview without a Google account.'
