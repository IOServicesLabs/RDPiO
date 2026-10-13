#!/usr/bin/env pwsh
#requires -Version 5.1
<#
.SYNOPSIS
    Tune the RDP host for maximum graphics/audio performance (and gaming).

.DESCRIPTION
    Run this on the RDP HOST machine. With no switches it only reports the
    current state (no admin needed). -Apply writes the settings (as
    Administrator); the values it replaces are saved first, and -Revert puts
    them back.

    Always applied:
      Rich RemoteFX graphics, 32-bit colour, no RDP bulk compression, mic and
      camera redirection allowed.

    -Gaming adds what a host with a GPU needs for fast, low-latency motion:
      - the session renders on the hardware GPU (bEnumerateHWBeforeSW)
      - H.264 is encoded on the GPU (AVCHardwareEncodePreferred)
      - the whole screen is one H.264 stream (AVC444ModePreferred), so the
        hardware encoder handles every pixel
      - UDP transport allowed alongside TCP (SelectTransport = 0)
      - the frame-rate cap lifted to -Fps (DWMFRAMEINTERVAL), and above 60 the
        session's virtual display refresh raised too (DisplayRefreshRate)

    60 fps is the documented RDP ceiling. 90/120/144 are an experiment: run the
    client and read `host_fps` in its `metrics:` log line to see what the host
    actually sends.

.PARAMETER Apply
    Write the settings (requires Administrator).

.PARAMETER Gaming
    Include the GPU/gaming settings listed above.

.PARAMETER Fps
    Frame-rate cap for -Gaming: 30, 60 (default), 90, 120 or 144.

.PARAMETER Revert
    Restore every value this script changed to what it was before the first
    -Apply (requires Administrator).

.EXAMPLE
    .\tune-rdpio-host.ps1                          # report only
    .\tune-rdpio-host.ps1 -Apply -Gaming           # 60 fps, GPU encode
    .\tune-rdpio-host.ps1 -Apply -Gaming -Fps 120  # the 120 fps experiment
    .\tune-rdpio-host.ps1 -Revert
#>
# SupportsShouldProcess already supplies -WhatIf and -Confirm as common
# parameters; declaring a -WhatIf switch here as well makes every invocation fail
# with "A parameter with the name 'WhatIf' was defined multiple times".
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [switch]$Apply,
    [switch]$Gaming,
    [ValidateSet(30, 60, 90, 120, 144)]
    [int]$Fps = 60,
    [switch]$Revert
)

$ErrorActionPreference = 'Stop'

$policy = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services'
$winstations = 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations'
$backupFile = Join-Path $env:ProgramData 'rdpio\host-tuning-backup.json'

# DWMFRAMEINTERVAL is the minimum time between captured frames, in ms. 15 is
# Microsoft's documented value for 60 fps; the others follow 1000/fps, rounded
# down so the cap sits just above the target.
$frameInterval = @{ 30 = 33; 60 = 15; 90 = 11; 120 = 8; 144 = 6 }

# Each entry: registry key, value name, target (DWORD), what it does.
$wanted = [System.Collections.Generic.List[object]]::new()
function Want($key, $name, $value, $what) {
    $wanted.Add([pscustomobject]@{ Key = $key; Name = $name; Value = $value; What = $what })
}

Want $policy 'fEnableVirtualizedGraphics' 1 'advanced RemoteFX graphics'
Want $policy 'VisualExperiencePolicy'     1 'rich multimedia visual experience'
Want $policy 'MaxColorDepth'              5 '32-bit colour'
Want $policy 'CompressionEnabled'         0 'no RDP bulk compression'
Want $policy 'fDisableAudioCapture'       0 'microphone redirection allowed'
Want $policy 'fDisableCam'                0 'camera redirection allowed'
if ($Gaming) {
    Want $policy 'bEnumerateHWBeforeSW'       1 'sessions render on the hardware GPU'
    Want $policy 'AVCHardwareEncodePreferred' 1 'H.264 encoded on the GPU'
    Want $policy 'AVC444ModePreferred'        1 'whole screen as one H.264 stream'
    Want $policy 'SelectTransport'            0 'UDP and TCP transports'
    Want $winstations 'DWMFRAMEINTERVAL' $frameInterval[$Fps] "frame-rate cap ~$Fps fps"
    if ($Fps -gt 60) {
        Want $winstations 'DisplayRefreshRate' $Fps "session display at $Fps Hz (experimental)"
    }
}

function Get-Current($key, $name) {
    $item = Get-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    return $item.$name
}

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    return ([Security.Principal.WindowsPrincipal]$id).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Show-Environment {
    Write-Host "`nHost" -ForegroundColor Cyan
    $os = Get-CimInstance Win32_OperatingSystem
    Write-Host "  $($os.Caption) (build $($os.BuildNumber))"
    $gpus = Get-CimInstance Win32_VideoController |
        Where-Object { $_.Name -notmatch 'Remote Display|Basic Display|Virtual' }
    if ($gpus) {
        foreach ($g in $gpus) { Write-Host "  GPU: $($g.Name)" }
    } else {
        Write-Host '  GPU: none found - the host will encode on the CPU (expect ~30-60 fps at best)' -ForegroundColor Yellow
    }
    $deny = Get-Current 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server' 'fDenyTSConnections'
    if ($deny -eq 0) { Write-Host '  Remote Desktop: enabled' }
    else { Write-Host '  Remote Desktop: DISABLED (Settings > System > Remote Desktop)' -ForegroundColor Yellow }

    $udpRule = Get-NetFirewallRule -Name 'RemoteDesktop-UserMode-In-UDP' -ErrorAction SilentlyContinue
    if ($udpRule -and $udpRule.Enabled -eq 'True') { Write-Host '  Firewall: RDP UDP 3389 allowed' }
    else { Write-Host '  Firewall: RDP UDP 3389 blocked - UDP transport cannot connect' -ForegroundColor Yellow }

    $up = Get-NetAdapter -Physical -ErrorAction SilentlyContinue | Where-Object Status -eq 'Up'
    foreach ($a in $up) {
        $line = "  Network: $($a.Name) ($($a.InterfaceDescription), $($a.LinkSpeed))"
        if ($a.PhysicalMediaType -match '802\.11|Wireless' -or $a.InterfaceDescription -match 'Wi-?Fi|Wireless') {
            Write-Host "$line - Wi-Fi adds jitter and loss; use Ethernet on the host for gaming" -ForegroundColor Yellow
        } else {
            Write-Host $line
        }
    }

    $plan = (powercfg /getactivescheme) -replace '^.*\((.*)\).*$', '$1'
    Write-Host "  Power plan: $plan"
}

function Show-Settings {
    Write-Host "`nSettings" -ForegroundColor Cyan
    $pending = 0
    foreach ($w in $wanted) {
        $cur = Get-Current $w.Key $w.Name
        $shown = if ($null -eq $cur) { 'not set' } else { "$cur" }
        if ($cur -eq $w.Value) {
            Write-Host ("  [ok]   {0,-28} = {1,-4} {2}" -f $w.Name, $w.Value, $w.What) -ForegroundColor Green
        } else {
            $pending++
            Write-Host ("  [todo] {0,-28} = {1,-4} {2} (now: {3})" -f $w.Name, $w.Value, $w.What, $shown) -ForegroundColor Yellow
        }
    }
    return $pending
}

function Save-Backup {
    # Only the first -Apply is recorded, so -Revert always returns to the
    # machine's state from before this script ever ran.
    $existing = @{}
    if (Test-Path $backupFile) {
        foreach ($p in (Get-Content $backupFile -Raw | ConvertFrom-Json)) {
            $existing["$($p.Key)|$($p.Name)"] = $p
        }
    }
    foreach ($w in $wanted) {
        $id = "$($w.Key)|$($w.Name)"
        if (-not $existing.ContainsKey($id)) {
            $existing[$id] = [pscustomobject]@{ Key = $w.Key; Name = $w.Name; Value = (Get-Current $w.Key $w.Name) }
        }
    }
    New-Item -ItemType Directory -Force -Path (Split-Path $backupFile) | Out-Null
    @($existing.Values) | ConvertTo-Json | Set-Content -Path $backupFile -Encoding UTF8
}

function Restart-Hint {
    Write-Host @"

The new settings apply to NEW sessions. Sign out of Windows (or reboot the host)
before connecting again - reconnecting to a session that is still signed in keeps
its old graphics settings.
"@ -ForegroundColor Yellow
}

if ($Revert) {
    if (-not (Test-Admin)) { throw 'Run -Revert from an elevated (Administrator) PowerShell.' }
    if (-not (Test-Path $backupFile)) { throw "No backup at $backupFile - nothing to revert." }
    foreach ($p in (Get-Content $backupFile -Raw | ConvertFrom-Json)) {
        if ($null -eq $p.Value) {
            if ($PSCmdlet.ShouldProcess("$($p.Key)\$($p.Name)", 'Remove')) {
                Remove-ItemProperty -Path $p.Key -Name $p.Name -ErrorAction SilentlyContinue
                Write-Host "Removed $($p.Name)" -ForegroundColor Green
            }
        } elseif ($PSCmdlet.ShouldProcess("$($p.Key)\$($p.Name)", "Set $($p.Value)")) {
            Set-ItemProperty -Path $p.Key -Name $p.Name -Value ([int]$p.Value) -Type DWord
            Write-Host "Restored $($p.Name) = $($p.Value)" -ForegroundColor Green
        }
    }
    Remove-Item $backupFile
    Restart-Hint
    return
}

Show-Environment
$pending = Show-Settings

if (-not $Apply) {
    if ($pending -gt 0) {
        $how = if ($Gaming) { "-Apply -Gaming -Fps $Fps" } else { '-Apply  (add -Gaming for the GPU/frame-rate settings)' }
        Write-Host "`n$pending setting(s) to change. Run from an elevated PowerShell:`n  .\tune-rdpio-host.ps1 $how" -ForegroundColor Cyan
    } else {
        Write-Host "`nEverything is already set." -ForegroundColor Green
    }
    return
}

if (-not (Test-Admin)) { throw 'Run -Apply from an elevated (Administrator) PowerShell.' }
if (-not $WhatIfPreference) { Save-Backup }

foreach ($w in $wanted) {
    if ((Get-Current $w.Key $w.Name) -eq $w.Value) { continue }
    if (-not (Test-Path $w.Key)) { New-Item -Path $w.Key -Force | Out-Null }
    if ($PSCmdlet.ShouldProcess("$($w.Key)\$($w.Name)", "Set $($w.Value)")) {
        Set-ItemProperty -Path $w.Key -Name $w.Name -Value $w.Value -Type DWord
        Write-Host "Set $($w.Name) = $($w.Value)  ($($w.What))" -ForegroundColor Green
    }
}

if ($Gaming) {
    $udpRule = Get-NetFirewallRule -Name 'RemoteDesktop-UserMode-In-UDP' -ErrorAction SilentlyContinue
    if ($udpRule -and $udpRule.Enabled -ne 'True' -and $PSCmdlet.ShouldProcess('RDP UDP firewall rule', 'Enable')) {
        Enable-NetFirewallRule -Name 'RemoteDesktop-UserMode-In-UDP'
        Write-Host 'Enabled the RDP UDP firewall rule' -ForegroundColor Green
    }
}

if (-not $WhatIfPreference) { Write-Host "`nPrevious values saved to $backupFile (undo with -Revert)." }
Restart-Hint
