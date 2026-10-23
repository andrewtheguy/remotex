# Build the Windows native package for x86-64: dist\zdesk-windows-x86_64.msi.
#
# The same tree the tarball carries (packaging/build-tarball.sh), installed by Windows
# Installer by default under %ProgramFiles%\zdesk with bin on the machine PATH — and
# nothing else,
# like the .deb, .rpm and .pkg: no service, no config. The gateway finds its config at
# %ProgramData%\zdesk\zdesk.toml (`installed_layout_for_exe` in src/config.rs); the web
# client is compiled into the exe:
#
#   C:\Program Files\zdesk\
#   ├── VERSION
#   ├── bin\zdesk.exe
#   └── share\doc\zdesk\
#       ├── zdesk.example.toml
#       └── LICENSE
#
# Runs on Windows under PowerShell 7 with cargo, the MSVC toolchain and WiX 5 on PATH
# (`dotnet tool install --global wix --version 5.0.2`; the UI extension the wizard pages
# come from is fetched below); the C libraries arrive as
# prebuilt static archives from their `-prebuilt` crates. `cargo build` compiles the frontend
# from Cargo's OUT_DIR into the exe; release CI points ZDESK_PREBUILT_FRONTEND at its shared
# platform-independent bundle. packaging/verify-windows-msi.ps1 then installs the result, runs
# it and removes it.
#Requires -Version 7
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
    throw 'wix is not on PATH: dotnet tool install --global wix --version 5.0.2'
}
# The stock dialog set lives in an extension pinned to the same WiX version. Adding it again
# is a no-op, so this needs no state on the machine beyond wix itself.
$uiExt = 'WixToolset.UI.wixext'
& wix extension add -g "$uiExt/5.0.2"
if ($LASTEXITCODE -ne 0) { throw "wix extension add $uiExt failed (exit $LASTEXITCODE)" }

# The version from cargo's own parse of the manifest rather than a regex over it — the same
# `[package]` version the tarball script reads with tomllib, without needing a Python.
$metadata = & cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed (exit $LASTEXITCODE)" }
$version = ($metadata.packages | Where-Object { $_.name -eq 'zdesk' }).version
if ($version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$') {
    throw "invalid version in Cargo.toml: '$version'"
}
# Windows Installer versions are three numbers (major and minor under 256, build under 65536)
# and nothing else; a pre-release suffix is dropped from the MSI's ProductVersion and kept in
# the VERSION file and `--version`.
$msiVersion = $version -replace '[-+].*$', ''
$parts = $msiVersion.Split('.') | ForEach-Object { [int]$_ }
if ($parts[0] -gt 255 -or $parts[1] -gt 255 -or $parts[2] -gt 65535) {
    throw "$msiVersion does not fit an MSI ProductVersion (255.255.65535)"
}
if ($msiVersion -ne $version) { Write-Host ">> MSI ProductVersion $msiVersion for $version" }

Write-Host '>> building release binary'
& cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
# cargo honours CARGO_TARGET_DIR, and so must this.
$targetDir = ($metadata.target_directory)
$exe = Join-Path $targetDir 'release\zdesk.exe'
if (-not (Test-Path $exe)) { throw "no release binary at $exe" }
$reported = (& $exe --version) -join ' '
if ($LASTEXITCODE -ne 0 -or $reported -notmatch [regex]::Escape($version)) {
    throw "the built gateway reports '$reported', not $version"
}

$stage = Join-Path ([System.IO.Path]::GetTempPath()) "zdesk-msi-$PID"
try {
    Write-Host ">> assembling zdesk-$version"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force -Path "$stage\bin", "$stage\share\doc\zdesk" | Out-Null
    Copy-Item $exe "$stage\bin\zdesk.exe"
    Copy-Item 'zdesk.example.toml', 'LICENSE' "$stage\share\doc\zdesk\"
    # Bare LF and no BOM, like the tarball's VERSION.
    [System.IO.File]::WriteAllText("$stage\VERSION", "$version`n")

    New-Item -ItemType Directory -Force -Path dist | Out-Null
    # Unversioned, like zdesk-linux-amd64.deb: the version is inside, and the release
    # page's `latest/download` URL stays stable.
    $msi = Join-Path (Resolve-Path dist).Path 'zdesk-windows-x86_64.msi'
    if (Test-Path $msi) { Remove-Item -Force $msi }
    Write-Host '>> building the MSI'
    & wix build -arch x64 -ext $uiExt -d "Version=$msiVersion" -d "Stage=$stage" -o $msi packaging\windows\zdesk.wxs
    if ($LASTEXITCODE -ne 0) { throw "wix build failed (exit $LASTEXITCODE)" }
    if (-not (Test-Path $msi)) { throw "wix build wrote no $msi" }
    Write-Host ">> wrote dist\zdesk-windows-x86_64.msi ($([math]::Round((Get-Item $msi).Length / 1MB, 1)) MB)"
} finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}
