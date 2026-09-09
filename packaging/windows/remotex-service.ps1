<#
.SYNOPSIS
    Template: run the installed gateway as a Windows service supervised by NSSM.

.DESCRIPTION
    The MSI installs a program, not a service, and owns no live config. This
    script is the operator's starting point for the other half, kept outside
    every package manifest so that upgrading or removing remotex never
    registers, changes or deletes a service. Copy it, adapt the defaults below
    to the deployment, and keep the copy with the deployment's own automation.

    remotex.exe is a console program: it has no service control dispatcher, so
    the SCM cannot start it directly (it would fail with error 1053). NSSM
    (https://nssm.cc, public domain) is the service binary instead, and
    supervises `remotex serve` as its child. That indirection is what buys the
    stop path: NSSM's first stop method generates a console Ctrl+C, which is
    the signal `serve` already waits on, so a service stop is the same orderly
    exit as Ctrl+C in a terminal rather than a killed process.

    NSSM is not shipped or downloaded here. Install it first — `winget install
    NSSM.NSSM`, `choco install nssm`, `scoop install nssm`, or the win64
    nssm.exe from the zip at https://nssm.cc/download — and either put it on
    PATH or name it with -Nssm.

.PARAMETER Action
    install   register the service, then start it unless -NoStart is given
    uninstall stop and deregister it; the config and logs are left alone
    status    report what the SCM and NSSM hold for it

.EXAMPLE
    pwsh -File remotex-service.ps1 install
    pwsh -File remotex-service.ps1 status
    pwsh -File remotex-service.ps1 uninstall
#>
[CmdletBinding()]
param(
    [ValidateSet('install', 'uninstall', 'status')]
    [string] $Action = 'install',

    [string] $ServiceName = 'remotex',
    [string] $DisplayName = 'remotex gateway',

    # The tree the MSI owns: <root>\bin\remotex.exe beside <root>\share.
    [string] $InstallRoot = (Join-Path $env:ProgramFiles 'remotex'),

    # The live config. `serve` finds this one by itself from an installed
    # layout; it is passed explicitly anyway so that `status` and the service's
    # own command line say which file the gateway is reading.
    [string] $Config = (Join-Path $env:ProgramData 'remotex\remotex.toml'),

    # Overrides [server].listen. Leave empty to let the config decide.
    [string] $Listen,

    # The account the gateway runs as. It holds the target credentials and
    # listens on the network, so the default is the lowest-privilege built-in
    # that can still open a socket rather than LocalSystem. A domain or local
    # account needs -ServiceAccountPassword and the "log on as a service"
    # right, which NSSM grants when it sets the account.
    [string] $ServiceAccount = 'NT AUTHORITY\NetworkService',
    [string] $ServiceAccountPassword,

    # What `nssm set <service> Start` accepts, which is not what NSSM 2.24's
    # README lists: the binary rejects the README's SERVICE_DELAYED_START and
    # names these four instead.
    [ValidateSet('SERVICE_AUTO_START', 'SERVICE_DELAYED_AUTO_START', 'SERVICE_DEMAND_START', 'SERVICE_DISABLED')]
    [string] $StartMode = 'SERVICE_DELAYED_AUTO_START',

    # Where the gateway's stdout and stderr are rotated. env_logger writes to
    # stderr; both streams are captured so a panic is not lost.
    [string] $LogDirectory = (Join-Path $env:ProgramData 'remotex\logs'),
    [int] $LogRotateBytes = 10MB,

    [string] $Nssm,
    [switch] $NoStart
)

$ErrorActionPreference = 'Stop'

$exe = Join-Path $InstallRoot 'bin\remotex.exe'

function Invoke-Nssm {
    # NSSM reports failure through its exit code and writes the reason to
    # stderr, which PowerShell would otherwise raise as an error record. It
    # writes UTF-16LE, which arrives here as every character followed by a NUL;
    # dropping the NULs is what makes the message readable.
    $output = (& $script:NssmExe @args 2>&1 | Out-String) -replace "`0", ''
    if ($LASTEXITCODE -ne 0) { throw "nssm $($args -join ' ') exited $LASTEXITCODE`n$output" }
    $output.Trim()
}

function Resolve-AccountSid([string] $Account) {
    try {
        (New-Object System.Security.Principal.NTAccount($Account)).Translate(
            [System.Security.Principal.SecurityIdentifier]).Value
    } catch {
        throw "cannot resolve the account '$Account': $($_.Exception.Message)"
    }
}

function Grant-Access([string] $Path, [string] $Sid, [string] $Rights) {
    # By SID, not by name: the config's own icacls line in docs/install.md
    # breaks inheritance, and the built-in accounts are localized.
    $result = & icacls $Path '/grant' "*${Sid}:${Rights}" 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "icacls on ${Path} exited ${LASTEXITCODE}`n$result" }
}

# --- preconditions --------------------------------------------------------
$identity = [System.Security.Principal.WindowsPrincipal][System.Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $identity.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'this needs an elevated PowerShell — the SCM does not take service changes from a filtered token'
}

$script:NssmExe = if ($Nssm) { $Nssm } else { (Get-Command nssm -ErrorAction SilentlyContinue).Source }
if (-not $script:NssmExe -or -not (Test-Path $script:NssmExe)) {
    throw 'nssm.exe not found — install NSSM (winget install NSSM.NSSM) or pass -Nssm <path>'
}
$script:NssmExe = (Resolve-Path $script:NssmExe).Path

switch ($Action) {

'install' {
    if (-not (Test-Path $exe)) { throw "$exe is missing — install remotex-windows-x86_64.msi first" }
    if (-not (Test-Path $Config)) {
        throw "$Config is missing — create it from share\doc\remotex\remotex.example.toml first (docs/install.md)"
    }
    if (Get-Service $ServiceName -ErrorAction SilentlyContinue) {
        throw "the service '$ServiceName' already exists — run this with 'uninstall' first"
    }

    # A rejected config would otherwise register a service that fails to start
    # with the reason only in a log file nobody is looking at yet.
    & $exe check-config --config $Config
    if ($LASTEXITCODE -ne 0) { throw "$Config was rejected — fix it before registering the service" }

    $sid = Resolve-AccountSid $ServiceAccount

    # The gateway reads one file and writes one directory. Nothing else it
    # touches is outside the read-only tree Program Files already grants.
    Grant-Access $Config $sid 'R'
    New-Item -ItemType Directory -Force -Path $LogDirectory | Out-Null
    Grant-Access $LogDirectory $sid '(OI)(CI)M'
    Write-Host "   $ServiceAccount may read $Config and write $LogDirectory"

    $arguments = @('serve', '--config', "`"$Config`"")
    if ($Listen) { $arguments += @('--listen', $Listen) }

    Invoke-Nssm install $ServiceName $exe ($arguments -join ' ') | Out-Null
    Invoke-Nssm set $ServiceName DisplayName $DisplayName | Out-Null
    Invoke-Nssm set $ServiceName Description "remotex browser gateway reading $Config" | Out-Null
    Invoke-Nssm set $ServiceName AppDirectory $InstallRoot | Out-Null
    Invoke-Nssm set $ServiceName Start $StartMode | Out-Null

    if ($ServiceAccountPassword) {
        Invoke-Nssm set $ServiceName ObjectName $ServiceAccount $ServiceAccountPassword | Out-Null
    } else {
        Invoke-Nssm set $ServiceName ObjectName $ServiceAccount | Out-Null
    }

    # AppNoConsole stays 0 on purpose: the console NSSM gives the child is what
    # carries the Ctrl+C that stops it. AppStopMethodSkip 0 keeps the whole
    # ladder — Ctrl+C, then the window messages, then TerminateProcess — and
    # AppStopMethodConsole is how long the orderly exit gets before the next rung.
    Invoke-Nssm set $ServiceName AppNoConsole 0 | Out-Null
    Invoke-Nssm set $ServiceName AppStopMethodSkip 0 | Out-Null
    Invoke-Nssm set $ServiceName AppStopMethodConsole 10000 | Out-Null

    # A gateway that exits has failed — `serve` returns only on a stop signal or
    # a server error — so every exit that is not a service stop is restarted,
    # after a delay that keeps a misconfigured config out of a tight crash loop.
    # AppThrottle is not that delay and must stay near its 1500 ms default: NSSM
    # holds the service in START_PENDING for it, so raising it adds that long to
    # every `Start-Service` and to every boot.
    Invoke-Nssm set $ServiceName AppExit Default Restart | Out-Null
    Invoke-Nssm set $ServiceName AppRestartDelay 5000 | Out-Null
    Invoke-Nssm set $ServiceName AppThrottle 1500 | Out-Null

    Invoke-Nssm set $ServiceName AppStdout (Join-Path $LogDirectory 'remotex.out.log') | Out-Null
    Invoke-Nssm set $ServiceName AppStderr (Join-Path $LogDirectory 'remotex.err.log') | Out-Null
    Invoke-Nssm set $ServiceName AppStdoutCreationDisposition 4 | Out-Null
    Invoke-Nssm set $ServiceName AppStderrCreationDisposition 4 | Out-Null
    Invoke-Nssm set $ServiceName AppRotateFiles 1 | Out-Null
    Invoke-Nssm set $ServiceName AppRotateOnline 1 | Out-Null
    Invoke-Nssm set $ServiceName AppRotateBytes $LogRotateBytes | Out-Null

    Write-Host "   registered '$ServiceName' as $ServiceAccount ($StartMode)"
    Write-Host "   $exe $($arguments -join ' ')"

    if ($NoStart) {
        Write-Host "   not started (-NoStart); start it with: Start-Service $ServiceName"
    } else {
        Start-Service $ServiceName
        (Get-Service $ServiceName).WaitForStatus('Running', '00:00:30')
        Write-Host "   started; logs under $LogDirectory"
    }
}

'uninstall' {
    if (-not (Get-Service $ServiceName -ErrorAction SilentlyContinue)) {
        Write-Host "   no service '$ServiceName' to remove"
        return
    }
    Stop-Service $ServiceName -Force -ErrorAction SilentlyContinue
    (Get-Service $ServiceName).WaitForStatus('Stopped', '00:00:30')
    Invoke-Nssm remove $ServiceName confirm | Out-Null
    Write-Host "   removed '$ServiceName'; $Config and $LogDirectory were left alone"
}

'status' {
    $service = Get-Service $ServiceName -ErrorAction SilentlyContinue
    if (-not $service) { Write-Host "   no service '$ServiceName'"; return }
    $service | Format-List Name, DisplayName, Status, StartType
    & sc.exe qc $ServiceName
    # `nssm get`, one parameter at a time: 2.24 has no `dump`.
    Write-Host '--- nssm ---'
    foreach ($parameter in 'Application', 'AppParameters', 'AppDirectory', 'ObjectName',
                           'Start', 'AppStdout', 'AppStderr', 'AppStopMethodConsole') {
        "{0,-22} {1}" -f $parameter, (Invoke-Nssm get $ServiceName $parameter)
    }
}

}
