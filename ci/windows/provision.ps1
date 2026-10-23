# Turn the Windows CI box (already carrying the VS Build Tools and rustup that the
# sibling repos' provision scripts install) into one that can also build the frontend
# (bun) and the installer (the .NET SDK, and WiX as a dotnet tool). The C libraries
# zdesk links come as prebuilt archives, so nothing here compiles C.
#
# Runs *on the VM*, elevated, deployed and started as a SYSTEM scheduled task by
# ../devtools/ci/windows/remote.ps1 (`remote.ps1 provision`), so a dropped connection
# cannot kill an installer half way. Every step is guarded, so re-running it after
# adding a step is a no-op for everything already installed. Progress goes to
# C:\ci-workspaces\provision\zdesk-provision.log; the last line is DONE-OK or
# DONE-FAIL.
#
# What is *not* installed here, and why:
#   - msbuild, cl, nmake, link: on PATH only inside a VS developer shell; ci.ps1 enters
#     one (Enter-VsDevShell) rather than polluting the machine PATH with a toolset
#     version.
#
# PowerShell 7 runs this — remote.ps1 registers the task with pwsh.exe, the same one
# sshd runs as its subsystem. Never Windows PowerShell 5.1: it reads a BOM-less UTF-8
# file as ANSI (an em dash then ends a string early) and turns a native tool's stderr
# — rustup's "info:" lines — into a terminating error.
#Requires -Version 7
$ErrorActionPreference = 'Stop'
# Invoke-WebRequest is many times slower with the progress bar on.
$ProgressPreference = 'SilentlyContinue'

$Root = 'C:\ci-workspaces\provision'
# remote.ps1 tails C:\provision\<repo>-provision.log for DONE-OK / DONE-FAIL.
$LogFile = 'C:\provision\zdesk-provision.log'
New-Item -ItemType Directory -Force -Path $Root, 'C:\provision' | Out-Null

function Log($m) { "[{0:HH:mm:ss}] {1}" -f (Get-Date), $m | Tee-Object -FilePath $LogFile -Append }

function Add-MachinePath($dir) {
    $p = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if (($p -split ';') -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', "$p;$dir", 'Machine')
        Log "added $dir to machine PATH"
    }
    if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }
}

function Set-MachineEnv($name, $value) {
    if ([Environment]::GetEnvironmentVariable($name, 'Machine') -ne $value) {
        [Environment]::SetEnvironmentVariable($name, $value, 'Machine')
        Log "set machine $name=$value"
    }
}

# Everything downloaded here is then run or installed, so nothing is trusted until it is
# checked: a release asset against the SHA-256 GitHub's release API reports for that exact
# asset (`gh api repos/<owner>/<repo>/releases/tags/<tag> --jq '.assets[] | .name, .digest'`),
# and a script that has no fixed digest — dotnet-install.ps1 is republished in place —
# against the Authenticode signer on the bytes that arrived. The download lands in
# `<out>.part` and takes its final name only after the check, so an interrupted run leaves
# nothing a rerun would mistake for a finished file. Every URL names a version; `latest`
# would make the pinned digest wrong at the next upstream release.
function Get-File($url, $out, $sha256, $signer) {
    if (Test-Path $out) { return }
    if (-not $sha256 -and -not $signer) { throw "Get-File ${url}: no digest or signer to check against" }
    # `name.part.ext`, not `name.ext.part`: Get-AuthenticodeSignature picks its verifier by the
    # extension and reports UnknownError for one it does not know.
    $part = [IO.Path]::ChangeExtension($out, 'part' + [IO.Path]::GetExtension($out))
    # A large GitHub release asset drops the connection now and then on this VM;
    # partial files are removed so a retry starts clean.
    for ($attempt = 1; $attempt -le 4; $attempt++) {
        Log "downloading $url (attempt $attempt)"
        Remove-Item $part -Force -ErrorAction SilentlyContinue
        try {
            Invoke-WebRequest $url -OutFile $part -UseBasicParsing
            break
        } catch {
            Log "download failed: $($_.Exception.Message)"
            Remove-Item $part -Force -ErrorAction SilentlyContinue
            if ($attempt -eq 4) { throw }
            Start-Sleep -Seconds (15 * $attempt)
        }
    }
    if ($sha256) {
        # `-ne` compares strings case-insensitively; Get-FileHash prints upper case.
        $actual = (Get-FileHash $part -Algorithm SHA256).Hash
        if ($actual -ne $sha256) {
            Remove-Item $part -Force
            throw "${url}: sha256 is $actual, expected $sha256"
        }
    }
    if ($signer) {
        $sig = Get-AuthenticodeSignature $part
        if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch "(^|, )CN=$([regex]::Escape($signer))(,|$)") {
            Remove-Item $part -Force
            throw "${url}: signature $($sig.Status) by '$($sig.SignerCertificate.Subject)', expected a valid one by CN=$signer"
        }
    }
    Move-Item $part $out
}

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

    # --- what the sibling repos' provisioning must already have left ---------
    foreach ($must in @('C:\BuildTools\VC\Auxiliary\Build\vcvars64.bat', 'C:\rust\cargo\bin\cargo.exe')) {
        if (-not (Test-Path $must)) { throw "$must is missing — run a Rust repo's provision (e.g. wrustic) first" }
    }

    # --- bun, for the frontend ------------------------------------------------
    if (-not (Test-Path 'C:\tools\bun\bun.exe')) {
        $zip = "$Root\bun-v1.4.0-windows-x64.zip"
        Get-File 'https://github.com/oven-sh/bun/releases/download/bun-v1.4.0/bun-windows-x64.zip' $zip `
            -Sha256 'e6f093d39da486b20262ca8cdd5ed6a9e8bc9c2f275b78e6d3a0c5b28cc95901'
        New-Item -ItemType Directory -Force -Path 'C:\tools\bun' | Out-Null
        Expand-Archive -Path $zip -DestinationPath "$Root\bun-unpack" -Force
        $bunExe = Get-ChildItem "$Root\bun-unpack" -Recurse -Filter bun.exe | Select-Object -First 1
        if (-not $bunExe) { throw 'bun zip held no bun.exe' }
        Copy-Item $bunExe.FullName 'C:\tools\bun\bun.exe' -Force
        Remove-Item -Recurse -Force "$Root\bun-unpack"
    } else { Log 'bun already present' }
    Add-MachinePath 'C:\tools\bun'
    Log "bun $(& 'C:\tools\bun\bun.exe' --version)"

    # --- .NET SDK + WiX: the MSI toolset is a dotnet tool ----------------------------
    # The SDK lands in C:\dotnet through Microsoft's dotnet-install script, which works
    # from a SYSTEM task with no UI; WiX beside it under C:\tools\wix via --tool-path,
    # so nothing depends on a per-user tools directory.
    if (-not (Test-Path 'C:\dotnet\dotnet.exe')) {
        Get-File 'https://dot.net/v1/dotnet-install.ps1' "$Root\dotnet-install.ps1" -Signer 'Microsoft Corporation'
        & "$Root\dotnet-install.ps1" -Channel 10.0 -InstallDir 'C:\dotnet' -NoPath
    } else { Log 'dotnet already present' }
    Add-MachinePath 'C:\dotnet'
    Set-MachineEnv 'DOTNET_CLI_TELEMETRY_OPTOUT' '1'
    $env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
    $env:DOTNET_NOLOGO = '1'
    Log "dotnet SDK $(& 'C:\dotnet\dotnet.exe' --version)"
    if (-not (Test-Path 'C:\tools\wix\wix.exe')) {
        & 'C:\dotnet\dotnet.exe' tool install wix --version 5.0.2 --tool-path 'C:\tools\wix' 2>&1 | ForEach-Object { Log "  $_" }
        if ($LASTEXITCODE -ne 0) { throw "dotnet tool install wix failed (exit $LASTEXITCODE)" }
    } else { Log 'wix already present' }
    Add-MachinePath 'C:\tools\wix'
    Log "wix $(& 'C:\tools\wix\wix.exe' --version)"

    # --- rust: the components ci.ps1 uses ---------------------------------------
    $env:RUSTUP_HOME = 'C:\rust\rustup'
    $env:CARGO_HOME = 'C:\rust\cargo'
    & 'C:\rust\cargo\bin\rustup.exe' component add clippy rustfmt 2>&1 | ForEach-Object { Log "  $_" }
    # The frontend's WebAssembly module (frontend\wasm\egfx) is built by the nightly
    # toolchain its directory pins, which rustup installs on the module's first build.
    Log "rust: $(& 'C:\rust\cargo\bin\rustc.exe' --version)"

    # --- pagefile: 2 GB of RAM is not enough for a thin-LTO release link -------
    # System-managed sizing on this box stopped at 1.5 GB; a fixed 8 GB file keeps
    # the linker and nmake from being killed rather than merely slow. Takes effect at
    # the next reboot, which `remote.ps1 provision` does not do — reboot by hand.
    $pf = Get-CimInstance Win32_PageFileSetting -ErrorAction SilentlyContinue | Where-Object { $_.Name -ieq 'C:\pagefile.sys' }
    if (-not $pf -or $pf.MaximumSize -lt 8192) {
        $cs = Get-CimInstance Win32_ComputerSystem
        if ($cs.AutomaticManagedPagefile) {
            Set-CimInstance -InputObject $cs -Property @{ AutomaticManagedPagefile = $false }
        }
        if ($pf) {
            Set-CimInstance -InputObject $pf -Property @{ InitialSize = [uint32]8192; MaximumSize = [uint32]8192 }
        } else {
            # The CIM properties are UInt32; a bare PowerShell integer is Int32 and is refused.
            New-CimInstance -ClassName Win32_PageFileSetting -Property @{ Name = 'C:\pagefile.sys'; InitialSize = [uint32]8192; MaximumSize = [uint32]8192 } | Out-Null
        }
        Log 'pagefile set to a fixed 8192 MB (takes effect after a reboot)'
    } else { Log "pagefile already $($pf.MaximumSize) MB" }

    Log 'DONE-OK'
} catch {
    Log "DONE-FAIL $($_.Exception.Message)"
    exit 1
}
