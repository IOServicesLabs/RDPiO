#!/usr/bin/env pwsh
#requires -Version 5.1
<#
.SYNOPSIS
    Launch RDPiO with performance-tuned presets.

.DESCRIPTION
    Run this on the CLIENT machine (the one with the GPU doing the decoding).
    It picks sensible flags for either gaming/low-latency or office/clarity use.

.PARAMETER Mode
    gaming  = low latency, AVC420-only, render-scale + RTX VSR upscale
    office  = clarity-first, full AVC444, bicubic upscale

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
        $flags = @(
            '--gaming'
            '--low-latency'
            '--udp'
            '--upscale', 'vsr'
        )
        if ($PSBoundParameters.ContainsKey('RenderScale')) {
            $flags += '--render-scale', $RenderScale
        } else {
            $flags += '--render-scale', '0.7'
        }
        Write-Host 'Launching RDPiO in GAMING mode (latency-first, AVC420, render-scale, VSR)...' -ForegroundColor Green
    }
    'office' {
        $flags = @(
            '--office'
            '--force-avc444'
            '--upscale', 'bicubic'
        )
        if ($PSBoundParameters.ContainsKey('RenderScale')) {
            $flags += '--render-scale', $RenderScale
        }
        Write-Host 'Launching RDPiO in OFFICE mode (clarity-first, AVC444, bicubic)...' -ForegroundColor Green
    }
}

$argList = $common + $flags
Write-Host "Command: $exe $argList" -ForegroundColor DarkGray
& $exe @argList
