<#
.SYNOPSIS
  Builds rdpio Display (the Hyper virtual display driver) into .\out:
  rdpio-display.dll, rdpio-display.inf (stamped), rdpio-display.cat.

.PARAMETER TestSign
  Also sign the DLL and catalog with a self-signed "rdpio Test Signing"
  certificate (made once in CurrentUser\My) and export it as out\rdpio-test.cer.
  Windows loads a test-signed driver only with test signing on
  (`bcdedit /set testsigning on` and a reboot) and the certificate trusted
  (rdpio --hyper-display-install does that part).

.PARAMETER Version
  The driver version stamped into the INF (default 1.0.0.0).

  Needs Visual Studio 2022 (C++), and the Windows Driver Kit 10.0.26100.
#>
param(
    [switch]$TestSign,
    [string]$Version = "1.0.0.0"
)
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$out = Join-Path $here "out"
New-Item -ItemType Directory -Force $out | Out-Null

$kits = "${env:ProgramFiles(x86)}\Windows Kits\10"
$sdk = (Get-ChildItem "$kits\Include" -Directory | Where-Object Name -like "10.*" | Sort-Object Name -Descending | Select-Object -First 1).Name
$umdf = "2.25"
$iddcx = "1.6"
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "Visual Studio with the C++ tools isn't installed" }
$vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"

$inc = @(
    "$kits\Include\$sdk\um", "$kits\Include\$sdk\shared", "$kits\Include\$sdk\ucrt",
    "$kits\Include\wdf\umdf\$umdf", "$kits\Include\$sdk\um\iddcx\$iddcx"
) | ForEach-Object { "/I`"$_`"" }
$libs = @(
    "`"$kits\Lib\wdf\umdf\x64\$umdf\WdfDriverStubUm.lib`"",
    "`"$kits\Lib\$sdk\um\x64\iddcx\$iddcx\iddcxstub.lib`"",
    "d3d11.lib", "dxgi.lib", "avrt.lib", "advapi32.lib", "kernel32.lib", "ntdll.lib"
)
$defs = "/DUNICODE /D_UNICODE /D_WINDLL /D_WIN64 /D_AMD64_ /DAMD64 /DUMDF_DRIVER /D_ATL_NO_WIN_SUPPORT " +
        "/DUMDF_VERSION_MAJOR=2 /DUMDF_VERSION_MINOR=25 /DIDDCX_VERSION_MAJOR=1 /DIDDCX_VERSION_MINOR=6 /DIDDCX_MINIMUM_VERSION_REQUIRED=4"
$cl = "cl /nologo /c /std:c++17 /O2 /MT /EHsc /W4 /WX- /GS /guard:cf /Zi $defs $($inc -join ' ') /Fo`"$out\\`" /Fd`"$out\rdpio-display.pdb`" `"$here\src\Driver.cpp`""
$link = "link /nologo /DLL /OUT:`"$out\rdpio-display.dll`" /PDB:`"$out\rdpio-display.pdb`" /DEBUG /OPT:REF /OPT:ICF /NXCOMPAT /DYNAMICBASE /GUARD:CF /SUBSYSTEM:WINDOWS `"$out\Driver.obj`" $($libs -join ' ')"
cmd /c "`"$vcvars`" >nul && $cl && $link"
if ($LASTEXITCODE) { throw "compile/link failed" }

$bin = "$kits\bin\$sdk"
Copy-Item "$here\rdpio-display.inf" "$out\rdpio-display.inf" -Force
& "$bin\x64\stampinf.exe" -f "$out\rdpio-display.inf" -d * -a amd64 -v $Version -u "2.25.0" | Out-Null
if ($LASTEXITCODE) { throw "stampinf failed" }
Remove-Item "$out\rdpio-display.cat" -ErrorAction SilentlyContinue
& "$bin\x86\Inf2Cat.exe" /driver:"$out" /os:10_CO_X64 /uselocaltime | Out-Null
if ($LASTEXITCODE) { throw "Inf2Cat failed" }

if ($TestSign) {
    $cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq "CN=rdpio Test Signing" -and $_.NotAfter -gt (Get-Date) } | Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=rdpio Test Signing" -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddYears(5)
    }
    Export-Certificate -Cert $cert -FilePath "$out\rdpio-test.cer" | Out-Null
    foreach ($f in "rdpio-display.dll", "rdpio-display.cat") {
        & "$bin\x64\signtool.exe" sign /q /fd sha256 /sha1 $cert.Thumbprint /s My "$out\$f"
        if ($LASTEXITCODE) { throw "signing $f failed" }
    }
}
Get-ChildItem $out -Include *.dll, *.inf, *.cat, *.cer -Recurse | Select-Object Name, Length
