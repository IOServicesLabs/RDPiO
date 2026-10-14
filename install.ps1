# rdpio installer: the latest release, for this Windows user (no admin).
#
#   irm https://raw.githubusercontent.com/IOServicesLabs/RDPiO/main/install.ps1 | iex
#
# Puts rdpio.exe (and its signed update manifest) in
# %LOCALAPPDATA%\Programs\rdpio, adds a Start menu shortcut and the folder to
# your PATH, and opens the launcher. Run it again to reinstall; after that,
# rdpio keeps itself up to date (Settings -> Updates).
#
# Options, as environment variables set before running it:
#   $env:RDPIO_CHANNEL = 'nightly'   every build of main instead of releases
#   $env:RDPIO_DIR = 'D:\Apps\rdpio' somewhere else
#   $env:RDPIO_PORTABLE = '1'        just the files: no shortcut, PATH or launch

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue' # (Windows PowerShell downloads crawl with the bar)
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repo = 'IOServicesLabs/RDPiO'
$base = if ($env:RDPIO_CHANNEL -eq 'nightly') {
    "https://github.com/$repo/releases/download/nightly"
} else {
    "https://github.com/$repo/releases/latest/download"
}
$dir = if ($env:RDPIO_DIR) { $env:RDPIO_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\rdpio' }
$portable = $env:RDPIO_PORTABLE -eq '1'

if (-not [Environment]::Is64BitOperatingSystem) { throw 'rdpio needs 64-bit Windows.' }

Write-Host "Installing rdpio to $dir"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("rdpio-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $exeNew = Join-Path $tmp 'rdpio.exe'
    $manifestNew = Join-Path $tmp 'rdpio.exe.update'
    Invoke-WebRequest -UseBasicParsing -Uri "$base/rdpio.exe.update" -OutFile $manifestNew
    Invoke-WebRequest -UseBasicParsing -Uri "$base/rdpio.exe" -OutFile $exeNew

    # The download is the file the release describes (rdpio itself checks the
    # release signature on every update after this).
    $manifest = (Get-Content -Raw $manifestNew | ConvertFrom-Json).manifest | ConvertFrom-Json
    $hash = (Get-FileHash -Algorithm SHA256 $exeNew).Hash.ToLowerInvariant()
    if ($hash -ne $manifest.sha256) { throw "The download is damaged (SHA-256 $hash, expected $($manifest.sha256)). Try again." }

    $exe = Join-Path $dir 'rdpio.exe'
    if (Test-Path $exe) {
        # A running rdpio can't be overwritten, only moved aside (it's
        # deleted the next time rdpio starts).
        Move-Item -Force $exe (Join-Path $dir ("rdpio.exe.$PID.old"))
    }
    Move-Item -Force $exeNew $exe
    Move-Item -Force $manifestNew (Join-Path $dir 'rdpio.exe.update')
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

if (-not $portable) {
    # Start menu shortcut.
    $programs = [Environment]::GetFolderPath('Programs')
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut((Join-Path $programs 'rdpio.lnk'))
    $link.TargetPath = $exe
    $link.WorkingDirectory = $dir
    $link.Description = 'rdpio: remote desktop and Hyper streaming'
    $link.Save()

    # `rdpio` in new terminals.
    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @($path -split ';' | Where-Object { $_ })
    if ($parts -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', (($parts + $dir) -join ';'), 'User')
    }
}

Write-Host "rdpio $($manifest.version) installed ($($manifest.build))." -ForegroundColor Green
if (-not $portable) {
    Write-Host 'Opening the launcher. Next time: Start menu -> rdpio, or `rdpio` in a new terminal.'
    Start-Process -FilePath $exe -WorkingDirectory $dir
}
