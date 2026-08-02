#!/usr/bin/env pwsh
#requires -Version 5.1
<#
.SYNOPSIS
    Launch RDPiO with performance-tuned presets.

.DESCRIPTION
    Run this on the CLIENT machine (the one with the GPU doing the decoding).
    It picks sensible flags for either gaming/low-latency or office/clarity use.

.PARAMETER Mode
    gaming  = AVC420-only codec preset, render-scale + FSR upscale (FSR runs on
              any GPU — VSR needs an NVIDIA RTX / Intel Arc driver and silently
              degrades elsewhere). Run rdpio.exe directly with --low-latency
              if you also want tearing presents (lowest lag, visible shear).
    office  = clarity-first defaults: vsync, 1:1 rendering, bicubic.

.PARAMETER RdpHost
    RDP host IP or name. (Not named -Host: $Host is a read-only PowerShell
    automatic variable, so binding a -Host parameter fails.)

.PARAMETER User
    Windows username.

.PARAMETER Password
    Windows password.

.PARAMETER RenderScale
    Optional override (0.4 .. 1.0). Lower = less host encode work.

.EXAMPLE
    .\run-rdpio.ps1 -Mode gaming -RdpHost 192.168.1.50 -User .\alice -Password 'hunter2'
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('gaming', 'office')]
    [string] $Mode,

    [Parameter(Mandatory)]
    [Alias('ComputerName', 'Server')]
    [string] $RdpHost,

    [Parameter(Mandatory)]
    [string] $User,

    [Parameter(Mandatory)]
    [string] $Password,

    [ValidateRange(0.4, 1.0)]
    [double] $RenderScale,

    [switch] $Log
)

$ErrorActionPreference = 'Stop'

$exe = Join-Path $PSScriptRoot 'target\release\rdpio.exe'
if (-not (Test-Path $exe)) {
    throw "RDPiO release binary not found at: $exe`nRun 'cargo build --release' first."
}

$common = @(
    '-h', $RdpHost
    '-u', $User
    '-p', $Password
    '--insecure'        # self-signed / untrusted certs
)

if ($Log) {
    $ts = Get-Date -Format 'yyyyMMdd_HHmmss'
    $logFile = Join-Path $PSScriptRoot "rdpio_$Mode`_$ts.log"
    $common += '--log-file', $logFile
    Write-Host "Logging to: $logFile" -ForegroundColor Cyan
}

switch ($Mode) {
    'gaming' {
        # FSR, not VSR: VSR is an NVIDIA RTX / Intel Arc driver feature — on
        # anything else (e.g. Intel UHD iGPUs) it can't engage and the scaled
        # frame goes through a lesser fallback. FSR's EASU+RCAS shader runs on
        # any GPU and reconstructs game imagery best.
        # No --udp: the UDP side-band is experimental and corrupts the stream
        # under packet loss (artifacts); re-add once the transport is fixed.
        # No --low-latency: tearing presents are an opt-in trade-off.
        $flags = @(
            '--gaming'
            '--upscale', 'fsr'
        )
        if ($PSBoundParameters.ContainsKey('RenderScale')) {
            $flags += '--render-scale', $RenderScale
        } else {
            $flags += '--render-scale', '0.7'
        }
        Write-Host 'Launching RDPiO in GAMING mode (AVC420, render-scale, FSR)...' -ForegroundColor Green
    }
    'office' {
        # --office already selects vsync, 1:1 rendering and bicubic; only a
        # user-chosen render-scale needs forwarding. (No --force-avc444: the
        # GPU decode path cannot use the extra chroma stream yet, so it would
        # double the host's encode work for an identical picture.)
        $flags = @('--office')
        if ($PSBoundParameters.ContainsKey('RenderScale')) {
            $flags += '--render-scale', $RenderScale
        }
        Write-Host 'Launching RDPiO in OFFICE mode (clarity-first: vsync, 1:1, bicubic)...' -ForegroundColor Green
    }
}

$argList = $common + $flags
Write-Host "Command: $exe $argList" -ForegroundColor DarkGray
& $exe @argList
