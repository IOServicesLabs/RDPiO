#!/usr/bin/env pwsh
#requires -Version 5.1
#requires -RunAsAdministrator
<#
.SYNOPSIS
    Tune the RDP host for maximum graphics/audio performance.

.DESCRIPTION
    Run this on the RDP HOST machine as Administrator.
    It applies Group Policy registry settings that raise the RDP bitrate/quality
    ceiling and ensure audio redirection is enabled.

.PARAMETER WhatIf
    Show what would change without writing to the registry.
#>
# SupportsShouldProcess already supplies -WhatIf and -Confirm as common
# parameters; declaring a -WhatIf switch here as well makes every invocation fail
# with "A parameter with the name 'WhatIf' was defined multiple times".
[CmdletBinding(SupportsShouldProcess = $true)]
param()

$ErrorActionPreference = 'Stop'

$ts = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services'

$settings = @{
    fEnableVirtualizedGraphics = 1          # enable advanced RemoteFX graphics
    VisualExperiencePolicy     = 1          # rich multimedia visual experience
    MaxColorDepth              = 5          # 32-bit color (1=8bpp, 2=15bpp, 3=16bpp, 4=24bpp, 5=32bpp)
    CompressionEnabled         = 0          # 0 = do not use RDP compression
    fDisableAudioCapture       = 0          # allow audio input (microphone) redirection
    fDisableCam                = 0          # allow camera redirection
}

if (-not (Test-Path $ts)) {
    if ($PSCmdlet.ShouldProcess($ts, 'Create Terminal Services policy key')) {
        New-Item -Path $ts -Force | Out-Null
    }
}

foreach ($kv in $settings.GetEnumerator()) {
    $action = "Set $($kv.Key) = $($kv.Value)"
    if ($PSCmdlet.ShouldProcess($ts, $action)) {
        Set-ItemProperty -Path $ts -Name $kv.Key -Value $kv.Value -Type DWord
        Write-Host "$action" -ForegroundColor Green
    } elseif ($WhatIfPreference) {
        Write-Host "Would $action" -ForegroundColor Cyan
    }
}

# Session-host specific visual-quality settings under the same key
$extra = @{
    # 0 = off/no limit; if the key exists with a low value it can cap bitrate.
    # Comment out if your environment relies on an explicit bandwidth policy.
    # MaxBandwidth = 0
}

foreach ($kv in $extra.GetEnumerator()) {
    $action = "Set $($kv.Key) = $($kv.Value)"
    if ($PSCmdlet.ShouldProcess($ts, $action)) {
        Set-ItemProperty -Path $ts -Name $kv.Key -Value $kv.Value -Type DWord
        Write-Host "$action" -ForegroundColor Green
    }
}

Write-Host @"

Registry changes applied. Some policies require one of the following to take effect:
  1. Restart the Remote Desktop Services service (termservice), OR
  2. Run 'gpupdate /force' and reconnect the RDP session, OR
  3. Reboot the host.

If the host has a GPU or iGPU, also enable H.264/AVC hardware encoding via GPO:
  Computer Configuration -> Administrative Templates -> Windows Components ->
  Remote Desktop Services -> Remote Desktop Session Host -> Remote Session Environment ->
  'Configure H.264/AVC hardware encoding for Remote Desktop connections' = Enabled
"@ -ForegroundColor Yellow
