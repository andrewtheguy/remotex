# The checks AGENTS.md asks for after a Rust change, run natively on the CI box by
# ci/windows/remote.ps1 (`remote.ps1 ci`): the frontend, clippy and the tests.
#
# `remote.ps1 ci -Package` adds what the Windows row of .github/workflows/release.yml does: the
# release installer, then installing it. That is a full release build and an MSI round trip, which
# a change to the code alone does not need on every rerun; ask for it when packaging changes or
# before a release. When this file and the workflow disagree, the workflow is right and this is
# stale.
#
# PowerShell 7. Installs nothing; the machine is provisioned by ci/windows/provision.ps1.
#Requires -Version 7
[CmdletBinding()]
param([switch] $Package)
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

function Invoke-Step([string] $Name, [scriptblock] $Body) {
    Write-Host ''
    Write-Host "== $Name =="
    $clock = [Diagnostics.Stopwatch]::StartNew()
    & $Body
    Write-Host ("   {0} took {1:n0}s" -f $Name, $clock.Elapsed.TotalSeconds)
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
foreach ($dir in 'LIBOPUS_PREBUILT_DIR', 'LIBFLAC_PREBUILT_DIR') {
    $v = [Environment]::GetEnvironmentVariable($dir)
    if ($v) { Write-Host "   $dir=$v" }
}

# Build the platform-independent frontend once, then have each Cargo invocation stage that
# bundle in its own OUT_DIR. The workspace arrives without node_modules. The bundle holds
# a WebAssembly module its build compiles from the gateway's graphics crate, which takes the
# target provision.ps1 adds.
Invoke-Step 'frontend' {
    Push-Location frontend
    try {
        & bun install --frozen-lockfile
        if ($LASTEXITCODE -eq 0) { & bun run build }
    } finally { Pop-Location }
}
$env:REMOTEX_PREBUILT_FRONTEND = 'frontend\dist'
Invoke-Step 'clippy' { & cargo clippy --all-targets -- -D warnings }
Invoke-Step 'cargo test' { & cargo test }
if (-not $Package) {
    Write-Host ''
    Write-Host '== release installer: not asked for (remote.ps1 ci -Package) =='
    exit 0
}
Invoke-Step 'release installer' {
    & pwsh -NoProfile -File packaging\build-windows-msi.ps1
}
Invoke-Step 'the installer installs' {
    # sshd gives an administrator's session its full token, so msiexec runs unprompted here.
    & pwsh -NoProfile -File packaging\verify-windows-msi.ps1
}
