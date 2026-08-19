# setup-win.ps1 -- install the native dependencies RDPiO needs to build and run
# on Windows.
#
# RDPiO is a from-scratch RDP client; its protocol stack, codecs and WebRTC
# redirector are pure Rust, so the native footprint is small. This script
# provisions the pieces that are *not* shipped by the Windows SDK / MSVC
# toolchain:
#
#   * ffmpeg      -- optional; used by the Linux port for H.264 decode, and by
#                   developer tooling for capturing/re-encoding test fixtures.
#                   Harmless to have on Windows, never required at runtime.
#   * opus        -- optional; the WebRTC redirector (rdp-webrtc) negotiates
#                   Opus audio. The in-tree engine is pure Rust; libopus is only
#                   needed if you build a native add-in DLL that links it.
#   * NVENC SDK   -- optional; the NVIDIA Video Codec SDK headers are only needed
#                   if you compile the (experimental) NVENC capture path.
#                   The *runtime* side (driver-level NVENC) is checked via
#                   nvidia-smi and only warned about, never installed.
#
# Idempotent: every step first checks whether the dependency is already
# present and exits 0 when it is. Re-running this script is safe.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts/setup-win.ps1
#
# Exit codes:
#   0  all dependencies present or installed successfully (or optional deps
#      deliberately skipped)
#   1  a required dependency could not be installed

[CmdletBinding()]
param(
    # Install optional tooling (ffmpeg, opus, NVENC SDK headers) too.
    # Default: skip optional installs -- the core build needs none of them.
    [switch]$IncludeOptional,

    # Install with elevated rights when a package manager needs it.
    [switch]$Elevated
)

# NOTE: we deliberately do NOT set $ErrorActionPreference = 'Stop' here. In
# Windows PowerShell 5.1, 'Stop' makes *stderr output from native commands*
# (ffmpeg, nvidia-smi, winget, choco, …) a terminating error, and Windows 11
# ships app-execution alias stubs in %LOCALAPPDATA%\Microsoft\WindowsApps that
# look like real tools but fail on invocation. Every place that needs a hard
# failure checks $LASTEXITCODE explicitly instead.
$ErrorActionPreference = 'Continue'

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Ok {
    param([string]$Message)
    Write-Host "    OK: $Message" -ForegroundColor Green
}

function Write-Skip {
    param([string]$Message)
    Write-Host "    skip: $Message" -ForegroundColor Yellow
}

function Write-Fail {
    param([string]$Message)
    Write-Host "    FAIL: $Message" -ForegroundColor Red
}

# ---------------------------------------------------------------------------
# 0. Elevate once up front if requested (winget/choco installs of machine-wide
#    tools may need it). Detach into a fresh elevated process to keep the rest
#    of the script's output in the current window.
# ---------------------------------------------------------------------------
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
$isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

if ($Elevated -and -not $isAdmin) {
    Write-Step "Re-launching elevated..."
    $scriptPath = $MyInvocation.MyCommand.Path
    if (-not $scriptPath) {
        $scriptPath = Join-Path $PSScriptRoot 'setup-win.ps1'
    }
    $argsForChild = @('-File', $scriptPath, '-IncludeOptional:' + $IncludeOptional.IsPresent)
    Start-Process -FilePath 'powershell.exe' -ArgumentList $argsForChild -Verb RunAs -Wait
    exit $LASTEXITCODE
}

# ---------------------------------------------------------------------------
# Helper: does a command exist on PATH?
# ---------------------------------------------------------------------------
function Test-Command {
    param([string]$Name)
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    if (-not $cmd) { return $false }
    # Windows 11 app-execution aliases (e.g. the Microsoft Store ffmpeg stub in
    # %LOCALAPPDATA%\Microsoft\WindowsApps) resolve via Get-Command but are not
    # real tools — invoking them opens the Store or fails. Treat them as absent.
    if ($cmd.Source -and $cmd.Source -like "$env:LOCALAPPDATA\Microsoft\WindowsApps\*") {
        return $false
    }
    return $true
}

# ---------------------------------------------------------------------------
# 1. ffmpeg (optional)
# ---------------------------------------------------------------------------
Write-Step "Checking ffmpeg..."
if (Test-Command 'ffmpeg') {
    $ver = try { (ffmpeg -version 2>$null | Select-Object -First 1) } catch { $null }
    if ($ver) { Write-Ok "ffmpeg already installed ($ver)" }
    else { Write-Ok "ffmpeg already installed" }
}
elseif ($IncludeOptional) {
    if (Test-Command 'winget') {
        Write-Step "Installing ffmpeg via winget..."
        winget install --id Gyan.FFmpeg --accept-source-agreements --accept-package-agreements --silent | Out-Null
        if ($LASTEXITCODE -eq 0) { Write-Ok "ffmpeg installed via winget" }
        else {
            Write-Fail "winget install of ffmpeg returned $LASTEXITCODE"
            exit 1
        }
    }
    elseif (Test-Command 'choco') {
        Write-Step "Installing ffmpeg via chocolatey..."
        choco install ffmpeg -y --no-progress | Out-Null
        if ($LASTEXITCODE -eq 0) { Write-Ok "ffmpeg installed via chocolatey" }
        else {
            Write-Fail "choco install of ffmpeg returned $LASTEXITCODE"
            exit 1
        }
    }
    else {
        Write-Skip "no winget or choco available; install ffmpeg manually from https://ffmpeg.org/download.html"
    }
}
else {
    Write-Skip "ffmpeg not present and optional installs disabled (pass -IncludeOptional to install)"
}

# ---------------------------------------------------------------------------
# 2. opus (optional; libopus only needed for native add-in builds)
# ---------------------------------------------------------------------------
Write-Step "Checking opus..."
$opusPresent = (Test-Command 'opusenc') -or (Test-Command 'opusinfo')
if ($opusPresent) {
    Write-Ok "opus tooling already installed"
}
elseif ($IncludeOptional) {
    if (Test-Command 'winget') {
        Write-Step "Installing opus-tools via winget..."
        winget install --id Xiph.Org.OpusTools --accept-source-agreements --accept-package-agreements --silent | Out-Null
        if ($LASTEXITCODE -eq 0) { Write-Ok "opus-tools installed via winget" }
        else {
            Write-Skip "winget install of opus-tools returned $LASTEXITCODE (optional; continuing)"
        }
    }
    elseif (Test-Command 'choco') {
        Write-Step "Installing opus-tools via chocolatey..."
        choco install opus-tools -y --no-progress | Out-Null
        if ($LASTEXITCODE -eq 0) { Write-Ok "opus-tools installed via chocolatey" }
        else {
            Write-Skip "choco install of opus-tools returned $LASTEXITCODE (optional; continuing)"
        }
    }
    else {
        Write-Skip "no winget or choco available; install opus-tools manually"
    }
}
else {
    Write-Skip "opus not present and optional installs disabled (pass -IncludeOptional to install)"
}

# ---------------------------------------------------------------------------
# 3. NVENC (runtime check only -- the SDK headers are optional)
# ---------------------------------------------------------------------------
Write-Step "Checking NVENC runtime (NVIDIA driver)..."
$nvidiaSmi = Get-Command 'nvidia-smi' -ErrorAction SilentlyContinue
if ($nvidiaSmi) {
    $gpuLine = (nvidia-smi --query-gpu=name --format=csv,noheader 2>$null | Select-Object -First 1)
    if ($gpuLine) {
        Write-Ok "NVIDIA driver present ($gpuLine) -- NVENC runtime available"
    }
    else {
        Write-Skip "nvidia-smi found but returned no GPU; NVENC runtime may be unavailable"
    }
}
else {
    Write-Skip "nvidia-smi not on PATH; NVENC runtime not verified (only needed for the experimental NVENC capture path)"
}

if ($IncludeOptional) {
    # NVENC SDK headers (nvEncodeAPI.h) live in the NVIDIA Video Codec SDK.
    # There is no winget/choco package for the SDK itself, so we only check
    # whether the header is already on disk and tell the user where to get it.
    $sdkHeader = Get-ChildItem -Path 'C:\Program Files\NVIDIA Corporation\NVIDIA Video Codec SDK*' `
        -Filter 'nvEncodeAPI.h' -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($sdkHeader) {
        Write-Ok "NVENC SDK header found at $($sdkHeader.FullName)"
    }
    else {
        Write-Skip "NVENC SDK headers not found; download the Video Codec SDK from https://developer.nvidia.com/nvidia-video-codec-sdk if you build the NVENC capture path"
    }
}
else {
    Write-Skip "NVENC SDK headers skipped (pass -IncludeOptional to check/install)"
}

# ---------------------------------------------------------------------------
# 4. MSVC toolchain sanity (required for the native build)
# ---------------------------------------------------------------------------
Write-Step "Checking MSVC build toolchain..."
$hasLink = Test-Command 'link'
$hasCl = Test-Command 'cl'
if ($hasLink -and $hasCl) {
    Write-Ok "MSVC toolchain (cl/link) on PATH"
}
elseif (Test-Command 'cargo') {
    # cargo is enough if the VS Build Tools are installed and discoverable via
    # vswhere; the developer usually runs from a "Developer PowerShell" prompt.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($vs) {
            Write-Ok "Visual Studio Build Tools found at $vs (use a Developer PowerShell to build)"
        }
        else {
            Write-Skip "cargo found but MSVC Build Tools component not detected; install 'Desktop development with C++' from Visual Studio Installer"
        }
    }
    else {
        Write-Skip "cargo found; MSVC toolchain presence not verified (install VS Build Tools if builds fail with linker errors)"
    }
}
else {
    Write-Skip "no cargo/cl/link on PATH; install Rust (https://rustup.rs) and Visual Studio Build Tools with the C++ workload"
}

Write-Step "setup-win.ps1 finished."
Write-Host "    Next: cargo build --release -p rdp-client" -ForegroundColor Green
exit 0
