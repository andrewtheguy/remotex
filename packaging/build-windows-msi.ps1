# Build the Windows native package for x86-64: dist\remotex-windows-x86_64.msi.
#
# The same tree the tarball carries (packaging/build-tarball.sh), installed by Windows
# Installer under %ProgramFiles%\remotex with bin on the machine PATH — and nothing else,
# like the .deb, .rpm and .pkg: no service, no config. The gateway finds its config at
# %ProgramData%\remotex\remotex.toml and the web client at ..\share\remotex\web beside the
# exe (`installed_layout_for_exe` in src/config.rs):
#
#   C:\Program Files\remotex\
#   ├── VERSION
#   ├── bin\remotex.exe
#   ├── share\doc\remotex\remotex.example.toml
#   └── share\remotex\web\
#
# Runs on Windows under PowerShell 7 with cargo, the MSVC toolchain and WiX 5 on PATH
# (`dotnet tool install --global wix --version 5.0.2`); the three C libraries arrive as
# prebuilt static archives from their `-prebuilt` crates. SKIP_FRONTEND_BUILD=1 reuses an
# existing frontend\dist, as the tarball script does. packaging/verify-windows-msi.ps1 then
# installs the result, runs it and removes it.
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
    throw 'wix is not on PATH: dotnet tool install --global wix --version 5.0.2'
}

# The version from cargo's own parse of the manifest rather than a regex over it — the same
# `[workspace.package]` value the tarball script reads with tomllib, without needing a Python.
$metadata = & cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed (exit $LASTEXITCODE)" }
$version = ($metadata.packages | Where-Object { $_.name -eq 'remotex' }).version
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

if ($env:SKIP_FRONTEND_BUILD -eq '1') {
    if (-not (Test-Path 'frontend\dist\index.html')) { throw 'SKIP_FRONTEND_BUILD=1 but frontend\dist is missing' }
    Write-Host '>> using prebuilt frontend\dist'
} else {
    Write-Host '>> building frontend'
    Push-Location frontend
    try {
        & bun run build
        if ($LASTEXITCODE -ne 0) { throw "bun run build failed (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
}

Write-Host '>> building release binary'
& cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
# cargo honours CARGO_TARGET_DIR, and so must this.
$targetDir = ($metadata.target_directory)
$exe = Join-Path $targetDir 'release\remotex.exe'
if (-not (Test-Path $exe)) { throw "no release binary at $exe" }
$reported = (& $exe --version) -join ' '
if ($LASTEXITCODE -ne 0 -or $reported -notmatch [regex]::Escape($version)) {
    throw "the built gateway reports '$reported', not $version"
}

$stage = Join-Path ([System.IO.Path]::GetTempPath()) "remotex-msi-$PID"
try {
    Write-Host ">> assembling remotex-$version"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force -Path "$stage\bin", "$stage\share\doc\remotex", "$stage\share\remotex" | Out-Null
    Copy-Item $exe "$stage\bin\remotex.exe"
    Copy-Item 'remotex.example.toml' "$stage\share\doc\remotex\remotex.example.toml"
    Copy-Item -Recurse 'frontend\dist' "$stage\share\remotex\web"
    # Bare LF and no BOM, like the tarball's VERSION.
    [System.IO.File]::WriteAllText("$stage\VERSION", "$version`n")

    New-Item -ItemType Directory -Force -Path dist | Out-Null
    # Unversioned, like remotex-linux-amd64.deb: the version is inside, and the release
    # page's `latest/download` URL stays stable.
    $msi = Join-Path (Resolve-Path dist).Path 'remotex-windows-x86_64.msi'
    if (Test-Path $msi) { Remove-Item -Force $msi }
    Write-Host '>> building the MSI'
    & wix build -arch x64 -d "Version=$msiVersion" -d "Stage=$stage" -o $msi packaging\windows\remotex.wxs
    if ($LASTEXITCODE -ne 0) { throw "wix build failed (exit $LASTEXITCODE)" }
    if (-not (Test-Path $msi)) { throw "wix build wrote no $msi" }
    Write-Host ">> wrote dist\remotex-windows-x86_64.msi ($([math]::Round((Get-Item $msi).Length / 1MB, 1)) MB)"
} finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}
