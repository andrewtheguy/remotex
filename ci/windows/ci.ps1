# What the Windows row of .github/workflows/release.yml does, run natively on the CI box by
# ci/windows/remote.ps1 (`remote.ps1 ci`), with the checks AGENTS.md asks for after a Rust
# change in front of it: the frontend, clippy, the tests, then the release installer. When this file
# and the workflow disagree, the workflow is right and this is stale.
#
# PowerShell 7. Installs nothing; the machine is provisioned by ci/windows/provision.ps1.
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

function Invoke-Step([string] $Name, [scriptblock] $Body) {
    Write-Host ''
    Write-Host "== $Name =="
    & $Body
    if ($LASTEXITCODE -ne 0) {
        Write-Host ''
        Write-Host "FAILED: $Name (exit $LASTEXITCODE)"
        exit $LASTEXITCODE
    }
}

Write-Host '== toolchain =='
& rustc --version
& cargo --version
& cargo clippy --version
& bun --version
& wix --version
if ($env:CARGO_TARGET_DIR) { Write-Host "   CARGO_TARGET_DIR=$env:CARGO_TARGET_DIR" }
foreach ($dir in 'FREERDP_PREBUILT_DIR', 'LIBVPX_PREBUILT_DIR', 'LIBOPUS_PREBUILT_DIR') {
    $v = [Environment]::GetEnvironmentVariable($dir)
    if ($v) { Write-Host "   $dir=$v" }
}

# The frontend first, on its own: build.rs runs `bun run build` too, but only from an installed
# node_modules, and the workspace arrives without one.
Invoke-Step 'frontend' {
    Push-Location frontend
    try {
        & bun install --frozen-lockfile
        if ($LASTEXITCODE -eq 0) { & bun run build }
    } finally { Pop-Location }
}
Invoke-Step 'clippy' { & cargo clippy --all-targets -- -D warnings }
Invoke-Step 'cargo test' { & cargo test }
Invoke-Step 'release installer' {
    $env:SKIP_FRONTEND_BUILD = '1'
    & pwsh -NoProfile -File packaging\build-windows-msi.ps1
}
Invoke-Step 'the installer installs' {
    # sshd gives an administrator's session its full token, so msiexec runs unprompted here.
    & pwsh -NoProfile -File packaging\verify-windows-msi.ps1
}
