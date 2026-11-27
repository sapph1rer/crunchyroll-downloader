# Build and publish a GitHub Release the app can update itself from.
#
#   1. set the new version in Cargo.toml (version = "0.3.0")
#   2. powershell -ExecutionPolicy Bypass -File scripts\make-release.ps1 -Notes "what changed"
#        -NoPublish   only build dist\ (no GitHub)
#        -Draft       publish as a draft release
#
# Result: dist\crunchyroll-downloader-windows-x64.zip (the three .exe files) + .sha256.
# The app only installs a zip whose checksum file is published beside it.
param(
    [string]$Notes = "",
    [switch]$NoPublish,
    [switch]$Draft
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$Version = (Select-String -Path "Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$helperVersion = (Select-String -Path "login-helper/Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
if ($helperVersion -ne $Version) { throw "login-helper/Cargo.toml has version $helperVersion, Cargo.toml has $Version - make them match" }

Write-Host "== building $Version"
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$name = "crunchyroll-downloader-windows-x64"
$dist = "dist/$name"
Remove-Item -Recurse -Force "dist" -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item "target/release/crunchyroll-downloader-gui.exe", "target/release/crunchyroll-downloader.exe", "target/release/cr-login-helper.exe" $dist

$zip = "dist/$name.zip"
Compress-Archive -Path "$dist/*" -DestinationPath $zip
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
Set-Content -NoNewline -Encoding ascii "$zip.sha256" "$hash  $name.zip"
Write-Host "== $zip  sha256 $hash"

if ($NoPublish) { Write-Host "built only (-NoPublish)"; exit 0 }
if (-not $Notes) { $Notes = "v$Version" }
$ghArgs = @("release", "create", "v$Version", $zip, "$zip.sha256", "--title", "v$Version", "--notes", $Notes)
if ($Draft) { $ghArgs += "--draft" }
gh @ghArgs
if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }
Write-Host "== published v$Version - running copies offer it in Settings > Updates"
