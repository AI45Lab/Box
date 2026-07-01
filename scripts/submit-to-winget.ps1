# Submit a3s-box to Windows Package Manager (winget)
#
# Usage:
#   .\scripts\submit-to-winget.ps1 -Version "0.8.0"
#
# Prerequisites:
#   1. Install wingetcreate: https://aka.ms/wingetcreate
#   2. Fork https://github.com/microsoft/winget-pkgs
#   3. Set GITHUB_TOKEN environment variable

param(
    [Parameter(Mandatory=$true)]
    [string]$Version,

    [Parameter(Mandatory=$false)]
    [string]$GitHubToken = $env:GITHUB_TOKEN
)

$ErrorActionPreference = "Stop"

# Validate inputs
if (-not $GitHubToken) {
    Write-Error "GITHUB_TOKEN environment variable not set. Please set it or pass -GitHubToken parameter."
    exit 1
}

$Tag = "v$Version"
$AssetUrl = "https://github.com/AI45Lab/Box/releases/download/$Tag/a3s-box-$Tag-windows-x86_64.zip"

Write-Host "=== Submitting a3s-box $Version to winget ===" -ForegroundColor Cyan
Write-Host ""

# Check if wingetcreate is installed
$wingetcreate = Get-Command wingetcreate -ErrorAction SilentlyContinue
if (-not $wingetcreate) {
    Write-Host "Installing wingetcreate..." -ForegroundColor Yellow
    Invoke-WebRequest -Uri "https://aka.ms/wingetcreate/latest" -OutFile "$env:TEMP\wingetcreate.exe"
    $wingetcreate = "$env:TEMP\wingetcreate.exe"
} else {
    $wingetcreate = $wingetcreate.Source
}

Write-Host "Using wingetcreate: $wingetcreate" -ForegroundColor Green
Write-Host ""

# Download release asset to compute SHA256
Write-Host "Downloading release asset..." -ForegroundColor Yellow
$AssetPath = "$env:TEMP\a3s-box-$Tag-windows-x86_64.zip"
Invoke-WebRequest -Uri $AssetUrl -OutFile $AssetPath

# Compute SHA256
Write-Host "Computing SHA256..." -ForegroundColor Yellow
$Hash = Get-FileHash -Path $AssetPath -Algorithm SHA256
$SHA256 = $Hash.Hash
Write-Host "SHA256: $SHA256" -ForegroundColor Green
Write-Host ""

# Update manifest files
Write-Host "Updating manifest files..." -ForegroundColor Yellow
$ManifestDir = Join-Path $PSScriptRoot "..\..\.winget"

# Update version
(Get-Content "$ManifestDir\A3SLab.Box.yaml") -replace 'PackageVersion: .*', "PackageVersion: $Version" | Set-Content "$ManifestDir\A3SLab.Box.yaml"
(Get-Content "$ManifestDir\A3SLab.Box.installer.yaml") -replace 'PackageVersion: .*', "PackageVersion: $Version" | Set-Content "$ManifestDir\A3SLab.Box.installer.yaml"
(Get-Content "$ManifestDir\A3SLab.Box.locale.en-US.yaml") -replace 'PackageVersion: .*', "PackageVersion: $Version" | Set-Content "$ManifestDir\A3SLab.Box.locale.en-US.yaml"

# Update installer URL and SHA256
(Get-Content "$ManifestDir\A3SLab.Box.installer.yaml") -replace 'InstallerUrl: .*', "InstallerUrl: $AssetUrl" | Set-Content "$ManifestDir\A3SLab.Box.installer.yaml"
(Get-Content "$ManifestDir\A3SLab.Box.installer.yaml") -replace 'InstallerSha256: .*', "InstallerSha256: $SHA256" | Set-Content "$ManifestDir\A3SLab.Box.installer.yaml"

# Update nested installer path
(Get-Content "$ManifestDir\A3SLab.Box.installer.yaml") -replace 'RelativeFilePath: a3s-box-v[0-9.]+-windows-x86_64', "RelativeFilePath: a3s-box-$Tag-windows-x86_64" | Set-Content "$ManifestDir\A3SLab.Box.installer.yaml"

Write-Host "Manifest files updated." -ForegroundColor Green
Write-Host ""

# Validate manifests
Write-Host "Validating manifests..." -ForegroundColor Yellow
& $wingetcreate validate $ManifestDir

if ($LASTEXITCODE -ne 0) {
    Write-Error "Manifest validation failed!"
    exit 1
}

Write-Host "Manifests validated successfully." -ForegroundColor Green
Write-Host ""

# Submit to winget-pkgs
Write-Host "Submitting to winget-pkgs..." -ForegroundColor Yellow
Write-Host "This will create a PR to microsoft/winget-pkgs" -ForegroundColor Cyan
Write-Host ""

& $wingetcreate submit `
    --token $GitHubToken `
    $ManifestDir

if ($LASTEXITCODE -eq 0) {
    Write-Host ""
    Write-Host "=== Successfully submitted to winget! ===" -ForegroundColor Green
    Write-Host ""
    Write-Host "A PR has been created at: https://github.com/microsoft/winget-pkgs/pulls" -ForegroundColor Cyan
    Write-Host "Please monitor the PR for any feedback from winget maintainers." -ForegroundColor Yellow
} else {
    Write-Host ""
    Write-Host "=== Submission failed ===" -ForegroundColor Red
    Write-Host ""
    Write-Host "Manual submission steps:" -ForegroundColor Yellow
    Write-Host "1. Fork https://github.com/microsoft/winget-pkgs" -ForegroundColor White
    Write-Host "2. Create directory: manifests/a/A3SLab/Box/$Version/" -ForegroundColor White
    Write-Host "3. Copy manifest files from .winget/ to that directory" -ForegroundColor White
    Write-Host "4. Create PR to microsoft/winget-pkgs" -ForegroundColor White
    Write-Host ""
    Write-Host "Or try using wingetcreate update:" -ForegroundColor Yellow
    Write-Host "wingetcreate update A3SLab.Box -v $Version -u $AssetUrl -t YOUR_GITHUB_TOKEN" -ForegroundColor White
}
