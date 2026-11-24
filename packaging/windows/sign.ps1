<#
.SYNOPSIS
    Signs files locally with Azure Artifact Signing (formerly Trusted Signing).

.DESCRIPTION
    Local path only. CI signs with azure/trusted-signing-action, which brings
    its own client tools and metadata; see release-windows.yml.

    Needs AZURE_TENANT_ID, AZURE_CLIENT_ID and AZURE_CLIENT_SECRET in the
    environment (a client-secret service principal, nothing federated) and
    the Artifact Signing client tools
    (winget install -e --id Microsoft.Azure.ArtifactSigningClientTools).
#>
param(
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]]$Files
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

foreach ($v in 'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET') {
    if (-not [Environment]::GetEnvironmentVariable($v)) { throw "$v is not set" }
}

# The client tools moved when Trusted Signing was renamed; probe both.
$Dlib = @(
    "$env:LOCALAPPDATA\Microsoft\ArtifactSigningClientTools\Azure.CodeSigning.Dlib.dll",
    "$env:LOCALAPPDATA\Microsoft\MicrosoftArtifactSigningClientTools\Azure.CodeSigning.Dlib.dll"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Dlib) { throw 'Azure.CodeSigning.Dlib.dll not found; install Microsoft.Azure.ArtifactSigningClientTools' }

# Newest SDK first. 10.0.20348 is documented as unsupported for this dlib.
$SignTool = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^10\.' -and $_.Name -notmatch '^10\.0\.20348' } |
    Sort-Object { [version]$_.Name } -Descending |
    ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' } |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $SignTool) { throw 'signtool.exe not found' }

# Generated per run rather than committed. ExcludeCredentials leaves
# EnvironmentCredential as the only path; without it the chain wanders into
# credential types that were never provisioned and fails pointing nowhere.
$Metadata = Join-Path ([IO.Path]::GetTempPath()) "tp-sign-metadata-$PID.json"
@{
    Endpoint               = 'https://eus.codesigning.azure.net'
    CodeSigningAccountName = 'specterpoint'
    CertificateProfileName = 'specterpoint'
    ExcludeCredentials     = @(
        'ManagedIdentityCredential', 'WorkloadIdentityCredential', 'SharedTokenCacheCredential',
        'VisualStudioCredential', 'VisualStudioCodeCredential', 'AzureCliCredential',
        'AzurePowerShellCredential', 'AzureDeveloperCliCredential', 'InteractiveBrowserCredential'
    )
} | ConvertTo-Json | Set-Content -Encoding utf8 $Metadata

try {
    # @() so a single file stays an array; splatting a bare string passes
    # signtool one argument per character.
    $paths = @($Files | ForEach-Object { (Resolve-Path $_).Path })
    # Certificates last three days; the RFC3161 timestamp is what keeps the
    # signature valid after that.
    & $SignTool sign /fd SHA256 /tr http://timestamp.acs.microsoft.com /td SHA256 `
        /dlib $Dlib /dmdf $Metadata @paths
    if ($LASTEXITCODE -ne 0) { throw "signtool sign exited $LASTEXITCODE" }
}
finally {
    Remove-Item $Metadata -ErrorAction SilentlyContinue
}

# signtool exiting zero is not the same as every file being signed.
foreach ($p in $paths) {
    $sig = Get-AuthenticodeSignature $p
    if ($sig.Status -ne 'Valid') { throw "$p is $($sig.Status) after signing" }
    Write-Host "signed: $p"
}
