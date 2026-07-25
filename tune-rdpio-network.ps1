#!/usr/bin/env pwsh
#requires -Version 5.1
#requires -RunAsAdministrator
<#
.SYNOPSIS
    Tune Windows network stack for high-throughput, low-latency RDP.

.DESCRIPTION
    Run this on BOTH the RDP CLIENT and the RDP HOST as Administrator.
    It applies TCP/IP and NIC settings that help saturate a 100 Gbps/internal
    link and reduce RDP stutter. Most settings are safe defaults for a dedicated
    lab/network; review before deploying on shared production hosts.

.PARAMETER WhatIf
    Show what would change without applying anything.

.PARAMETER EnableJumboFrames
    Attempt to set all Ethernet NICs to MTU 9000. Only effective if the switch
    and the remote endpoint also support jumbo frames.

.PARAMETER LatencyProfile
    gaming  = favor lowest latency (disable some coalescing/interrupt moderation)
    throughput = favor raw throughput (default)
    wifi    = host/client on Wi-Fi: as 'gaming', plus disable wireless-adapter
              power saving (the biggest source of Wi-Fi jitter) and never apply
              jumbo frames (they force fragmentation on a lossy link).
#>
# SupportsShouldProcess already supplies -WhatIf and -Confirm as common
# parameters; declaring a -WhatIf switch here as well makes every invocation fail
# with "A parameter with the name 'WhatIf' was defined multiple times".
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [switch] $EnableJumboFrames,
    [ValidateSet('gaming', 'throughput', 'wifi')]
    [string] $LatencyProfile = 'throughput'
)

# Jumbo frames are a wired-LAN win only; on Wi-Fi they force IP fragmentation and
# one lost fragment drops the whole packet. Refuse the combination.
if ($EnableJumboFrames -and $LatencyProfile -eq 'wifi') {
    Write-Warning "Ignoring -EnableJumboFrames: jumbo frames hurt on Wi-Fi (fragmentation on a lossy link)."
    $EnableJumboFrames = $false
}

$ErrorActionPreference = 'Stop'

function Set-DWordValue {
    param(
        [string] $Path,
        [string] $Name,
        [object] $Value,
        [Microsoft.Win32.RegistryValueKind] $Type = 'DWord'
    )
    if (-not (Test-Path $Path)) {
        if ($PSCmdlet.ShouldProcess($Path, 'Create registry key')) {
            New-Item -Path $Path -Force | Out-Null
        }
    }
    $action = "Set $Path\$Name = $Value"
    if ($PSCmdlet.ShouldProcess($action, 'Apply registry value')) {
        Set-ItemProperty -Path $Path -Name $Name -Value $Value -Type $Type -Force
        Write-Host $action -ForegroundColor Green
    }
}

Write-Host "Applying network tuning (profile: $LatencyProfile)..." -ForegroundColor Cyan

# ---------------------------------------------------------------------------
# TCP / IP stack tuning
# ---------------------------------------------------------------------------
$tcpParams = 'HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters'

# Congestion provider: CTCP historically best for high-BDP Windows links.
# Windows Server 2019+/11 also support Cubic. Pick CTCP as a safe high-BDP default.
Set-DWordValue -Path $tcpParams -Name 'CongestionProvider' -Value 1  # 1 = CTCP

# Keep TCP auto-tuning enabled for high-throughput links.
# 0=disabled, 1=normal(restricted), 2=experimental(least restricted), 3=normal
Set-DWordValue -Path $tcpParams -Name 'TcpAutotuning' -Value 2

# Increase ephemeral port range and reduce TIME_WAIT reuse delay.
Set-DWordValue -Path $tcpParams -Name 'MaxUserPort' -Value 65534
Set-DWordValue -Path $tcpParams -Name 'TcpTimedWaitDelay' -Value 30

# Disable network throttling that Windows applies to multimedia apps.
# This stops the OS from limiting RDP/remote-audio network scheduling.
$multimedia = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile'
Set-DWordValue -Path $multimedia -Name 'NetworkThrottlingIndex' -Value 0xffffffff
Set-DWordValue -Path $multimedia -Name 'SystemResponsiveness' -Value 0

# ---------------------------------------------------------------------------
# NIC offloads and RSS
# ---------------------------------------------------------------------------
$nics = Get-NetAdapter -Physical | Where-Object { $_.Status -eq 'Up' -and $_.InterfaceDescription -notmatch 'Loopback|Pseudo' }

foreach ($nic in $nics) {
    Write-Host "Tuning NIC: $($nic.Name) ($($nic.InterfaceDescription))" -ForegroundColor Cyan

    # Enable Receive-Side Scaling so multiple cores process traffic.
    if ($PSCmdlet.ShouldProcess("$($nic.Name) RSS", 'Enable')) {
        Enable-NetAdapterRss -Name $nic.Name -NoRestart -ErrorAction SilentlyContinue
        Write-Host "  Enabled RSS" -ForegroundColor Green
    }

    # Large Send Offload and Checksum offloads reduce CPU on high-BDP links.
    $offloads = @('LsoV2IPv4', 'LsoV2IPv6', 'IPChecksumOffloadIPv4', 'TCPChecksumOffloadIPv4', 'UDPChecksumOffloadIPv4')
    foreach ($offload in $offloads) {
        if ($PSCmdlet.ShouldProcess("$($nic.Name) $offload", 'Set to Enabled')) {
            Set-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword $offload -RegistryValue 1 -NoRestart -ErrorAction SilentlyContinue
            Write-Host "  Enabled $offload" -ForegroundColor Green
        }
    }

    # Receive Segment Coalescing: good for throughput, can add small latency.
    if ($LatencyProfile -eq 'throughput') {
        if ($PSCmdlet.ShouldProcess("$($nic.Name) RSC", 'Enable')) {
            Enable-NetAdapterRsc -Name $nic.Name -ErrorAction SilentlyContinue
            Write-Host "  Enabled RSC" -ForegroundColor Green
        }
    } else {
        if ($PSCmdlet.ShouldProcess("$($nic.Name) RSC", 'Disable')) {
            Disable-NetAdapterRsc -Name $nic.Name -ErrorAction SilentlyContinue
            Write-Host "  Disabled RSC (gaming/low-latency)" -ForegroundColor Green
        }
    }

    # Jumbo frames: usually the biggest single win on a 100 Gbps lab link,
    # but every hop must support MTU 9000.
    if ($EnableJumboFrames) {
        if ($PSCmdlet.ShouldProcess("$($nic.Name) MTU", 'Set to 9000')) {
            Set-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword '*JumboPacket' -RegistryValue 9000 -NoRestart -ErrorAction SilentlyContinue
            Write-Host "  Set MTU/JumboPacket to 9000" -ForegroundColor Green
        }
    }
}

# ---------------------------------------------------------------------------
# Wi-Fi: disable wireless-adapter power saving
# ---------------------------------------------------------------------------
# The single biggest source of Wi-Fi jitter is the radio dropping into power
# save between beacons, which adds periodic tens-of-ms latency spikes. Force the
# wireless adapter to Maximum Performance on both AC and battery.
if ($LatencyProfile -eq 'wifi') {
    $wifiSub = '19cbb8fa-5279-450e-9fac-8a3d5fedd0c1'   # Wireless Adapter Settings
    $wifiSetting = '12bbebe6-58d6-4636-95bb-3217ef867c1a' # Power Saving Mode
    if ($PSCmdlet.ShouldProcess('Wireless adapter', 'Set Power Saving Mode = Maximum Performance')) {
        powercfg /setacvalue SCHEME_CURRENT $wifiSub $wifiSetting 0
        powercfg /setdcvalue SCHEME_CURRENT $wifiSub $wifiSetting 0
        powercfg /S SCHEME_CURRENT
        Write-Host "Wireless adapter power saving disabled (Maximum Performance)" -ForegroundColor Green
    }
    # Best-effort: some drivers also expose a per-adapter power-save knob. Try the
    # common keywords; missing ones are silently skipped.
    foreach ($nic in $nics) {
        foreach ($kw in @('*PowerSaveMode', 'PowerSavingMode', 'ulPowerSaveLevel')) {
            Set-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword $kw -RegistryValue 0 -NoRestart -ErrorAction SilentlyContinue
        }
    }
}

# ---------------------------------------------------------------------------
# QoS / RDP DSCP marking
# ---------------------------------------------------------------------------
# Mark RDPiO traffic with DSCP AF41 so routers prioritize it AND — crucially on
# Wi-Fi — the AP's WMM scheduler maps it to the Video access category for better
# airtime. Targets rdpio.exe (not mstsc.exe); add a second policy for mstsc if you
# also use the built-in client.
$qosKey = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\QoS\rdpio'
if (-not (Test-Path $qosKey)) {
    if ($PSCmdlet.ShouldProcess($qosKey, 'Create QoS policy key')) {
        New-Item -Path $qosKey -Force | Out-Null
    }
}
Set-DWordValue -Path $qosKey -Name 'Application Name' -Value 'rdpio.exe' -Type String
Set-DWordValue -Path $qosKey -Name 'DSCP Value' -Value '34' -Type String  # AF41 → WMM Video
Set-DWordValue -Path $qosKey -Name 'Throttle Rate' -Value '-1' -Type String
Set-DWordValue -Path $qosKey -Name 'Version' -Value '1.0' -Type String
Set-DWordValue -Path $qosKey -Name 'Protocol' -Value '*' -Type String
Set-DWordValue -Path $qosKey -Name 'Local Port' -Value '*' -Type String
Set-DWordValue -Path $qosKey -Name 'Remote Port' -Value '*' -Type String

Write-Host @"

Network tuning applied. A reboot is recommended so all NIC offload/RSS/MTU
changes take effect cleanly. After reboot, verify with:

  Get-NetTCPSetting | Select SettingName, CongestionProvider, AutoTuningLevelLocal
  Get-NetAdapterAdvancedProperty -DisplayName 'Jumbo*','Recv Segment Coalescing*'
  Get-NetAdapterRss

If jumbo frames are enabled, confirm end-to-end MTU with:
  ping -f -l 8972 <rdp-host-ip>
(8972 = 9000 - 28 bytes IP/ICMP headers)
"@ -ForegroundColor Yellow
