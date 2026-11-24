<#
.SYNOPSIS
    Local release path: build, sign the payload, stage, forge, sign the installer.

.DESCRIPTION
    The same order as release-windows.yml, with sign.ps1 in place of the
    signing action. Output: target\windows\dist\telepresence-<version>-windows-x64-setup.exe

.PARAMETER Version
    Installer version. Defaults to the workspace version in Cargo.toml.

.PARAMETER CredsFile
    Optional text file with "Tenant ID:", "Client ID:" and "Secret:" lines,
    loaded into this process's environment when AZURE_* are not already set.

.PARAMETER NoSign
    Unsigned development build. The installer is forged with --dev, which the
    stub displays, so it cannot pass for a release.
#>
param(
    [string]$Version,
    [string]$CredsFile,
    [switch]$NoSign,
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

# Keep in step with FORGE_VERSION in .github/workflows/release-windows.yml.
$ForgeVersion = 'v0.4.1'

$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Work = Join-Path $Repo 'target\windows'

if (-not $Version) {
    $m = Select-String -Path (Join-Path $Repo 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    $Version = $m.Matches[0].Groups[1].Value
}
$Version = $Version.TrimStart('v')

if (-not $NoSign -and $CredsFile -and -not $env:AZURE_CLIENT_SECRET) {
    $map = @{ 'Tenant ID' = 'AZURE_TENANT_ID'; 'Client ID' = 'AZURE_CLIENT_ID'; 'Secret' = 'AZURE_CLIENT_SECRET' }
    foreach ($line in Get-Content $CredsFile) {
        if ($line -match '^(Tenant ID|Client ID|Secret):\s*(\S+)\s*$') {
            [Environment]::SetEnvironmentVariable($map[$Matches[1]], $Matches[2], 'Process')
        }
    }
}

$stage = Join-Path $PSScriptRoot 'stage.ps1'
$sign = Join-Path $PSScriptRoot 'sign.ps1'

if (-not $SkipBuild) { & $stage -Phase build }

# Payload binaries we build are signed before staging: Forge extracts payload
# members verbatim, so signing the installer does nothing for them.
if (-not $NoSign) {
    & $sign "$Work\own\telepresence.exe" "$Work\own\tp-setup-helper.exe" `
        "$Work\own\VirtualCamera\tp-vcam.dll" "$Work\own\VirtualCamera\tp-vcam-register.exe"
}

& $stage -Phase assemble

# Forge release assets. lwstub.exe is signed upstream, which is what makes the
# embedded uninstaller signed.
$tools = Join-Path $Work "forge-$ForgeVersion"
if (-not (Test-Path "$tools\lwforge.exe")) {
    gh release download $ForgeVersion --repo Locke-Werks/Forge --pattern '*.exe' --dir $tools --clobber
}
foreach ($n in 'lwforge.exe', 'lwstub.exe') {
    $s = Get-AuthenticodeSignature "$tools\$n"
    if ($s.Status -ne 'Valid') { throw "$n is $($s.Status)" }
}

# Versioned copy of the config. icon is resolved relative to the config file,
# so it is rewritten to an absolute path for the copy.
$config = Join-Path $Work 'installer.toml'
$icon = (Resolve-Path (Join-Path $Repo 'assets\telepresence.ico')).Path -replace '\\', '/'
(Get-Content (Join-Path $PSScriptRoot 'installer.toml') -Raw) `
    -replace '(?m)^version\s*=\s*"0\.0\.0"', "version      = `"$Version`"" `
    -replace '(?m)^icon\s*=\s*".*"', "icon         = `"$icon`"" |
    Set-Content -Encoding utf8NoBOM $config

$dist = Join-Path $Work 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
$out = Join-Path $dist "telepresence-$Version-windows-x64-setup.exe"
$forgeArgs = @('build', '--config', $config, '--payload', "$Work\stage", '--stub', "$tools\lwstub.exe", '--out', $out)
if ($NoSign) { $forgeArgs += '--dev' }
& "$tools\lwforge.exe" @forgeArgs

if (-not $NoSign) {
    & $sign $out
    # Signing appends the certificate table after the container; this proves
    # the installer can still read its own payload.
    & "$tools\lwforge.exe" inspect $out | Out-Null
}
Write-Host "Installer: $out"
