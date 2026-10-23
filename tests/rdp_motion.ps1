# rdp_motion.ps1 start|stop: a full-screen moving picture on a Windows host's desktop, for the RDP probes that need
# one (examples/rdp_link_probe.rs runs it as ZDESK_UAT_MOTION; examples/rdp_damage_probe.rs's first case wants it
# running and its second wants it stopped). `start` draws an animation, every pixel changing each frame at about
# 30 frames a second, in the desktop session of the user the probe logs on as, through a scheduled task, since an
# SSH session has no desktop; `stop` closes it and removes the task. The host is reached with PowerShell over SSH
# by ZDESK_UAT_TARGET's name, the target in tmp/test_uat.toml, so that name must be an SSH host here as well.
param([Parameter(Mandatory)][ValidateSet('start', 'stop')][string]$Action)
$ErrorActionPreference = 'Stop'

$hostName = $env:ZDESK_UAT_TARGET
if (-not $hostName) { throw 'set ZDESK_UAT_TARGET to the Windows host' }

# The animation, run on the host in the user's desktop session: a borderless window over the whole primary screen,
# bands of colour sliding across it and a square sweeping over them, so a codec has motion everywhere and nothing
# still to skip.
$animation = @'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'
$form.StartPosition = 'Manual'
$form.Bounds = $screen
$form.TopMost = $true
$form.Text = 'zdesk motion'
$form.GetType().GetProperty('DoubleBuffered', [Reflection.BindingFlags]'Instance,NonPublic').SetValue($form, $true)
$script:frame = 0
$form.Add_Paint({
    param($sender, $e)
    $g = $e.Graphics
    $w = $sender.ClientSize.Width; $h = $sender.ClientSize.Height
    $bands = 24
    for ($i = 0; $i -lt $bands; $i++) {
        $hue = (($i * 360 / $bands) + $script:frame * 6) % 360
        $c = [System.Drawing.Color]::FromArgb(255, [int](127 + 127 * [Math]::Sin($hue * [Math]::PI / 180)),
            [int](127 + 127 * [Math]::Sin(($hue + 120) * [Math]::PI / 180)), [int](127 + 127 * [Math]::Sin(($hue + 240) * [Math]::PI / 180)))
        $brush = New-Object System.Drawing.SolidBrush $c
        $x = (($i * $w / $bands) + $script:frame * 9) % $w
        $g.FillRectangle($brush, [single]$x, 0, [single]($w / $bands + 1), [single]$h)
        $g.FillRectangle($brush, [single]($x - $w), 0, [single]($w / $bands + 1), [single]$h)
        $brush.Dispose()
    }
    $side = [Math]::Min($w, $h) / 4
    $sx = ($script:frame * 17) % ($w - $side); $sy = ($script:frame * 11) % ($h - $side)
    $g.FillRectangle([System.Drawing.Brushes]::White, [single]$sx, [single]$sy, [single]$side, [single]$side)
})
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 33
$timer.Add_Tick({ $script:frame++; $form.Invalidate() })
$timer.Start()
[System.Windows.Forms.Application]::Run($form)
'@

Invoke-Command -HostName $hostName -SSHTransport -ArgumentList $Action, $animation -ScriptBlock {
    param($Action, $Animation)
    $ErrorActionPreference = 'Stop'
    $task = 'zdesk-motion'
    $dir = 'C:\ci-workspaces\zdesk-motion'
    if ($Action -eq 'start') {
        New-Item -ItemType Directory -Force $dir | Out-Null
        Set-Content -Path "$dir\animation.ps1" -Value $Animation -Encoding utf8
        $run = New-ScheduledTaskAction -Execute 'C:\Program Files\PowerShell\7\pwsh.exe' `
            -Argument "-NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $dir\animation.ps1"
        # Interactive: in the user's own desktop session, which the probe's logon takes over, and not session 0.
        $who = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive
        Register-ScheduledTask -TaskName $task -Action $run -Principal $who -Force | Out-Null
        Start-ScheduledTask -TaskName $task
        Start-Sleep -Seconds 3
        if (-not (Get-CimInstance Win32_Process -Filter "Name = 'pwsh.exe'" | Where-Object CommandLine -like '*zdesk-motion*')) {
            throw 'the animation did not start: is the user logged on to the desktop?'
        }
        'the animation is running'
    } else {
        Get-CimInstance Win32_Process -Filter "Name = 'pwsh.exe'" | Where-Object CommandLine -like '*zdesk-motion*' |
            ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
        Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
        'the animation is stopped'
    }
}
