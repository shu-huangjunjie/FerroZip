# Builds the release programs and the installer:
#   dist\FerroZip-Setup-<version>.exe
# Run from anywhere:  powershell -ExecutionPolicy Bypass -File package.ps1

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

cargo build --release --locked -p fz-gui -p fz-cli
if ($LASTEXITCODE -ne 0) { throw 'building FerroZip.exe / fz.exe failed' }
# The installer embeds the two executables built above.
cargo build --release --locked -p fz-setup
if ($LASTEXITCODE -ne 0) { throw 'building FerroZip-Setup.exe failed' }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
New-Item -ItemType Directory -Force dist | Out-Null
$out = "dist\FerroZip-Setup-$version.exe"
Copy-Item target\release\FerroZip-Setup.exe $out -Force
"{0}  ({1:N1} MB)" -f (Resolve-Path $out), ((Get-Item $out).Length / 1MB)
