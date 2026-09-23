# set_shortcuts.ps1 - point aura's Desktop / Start Menu / Startup shortcuts at the
# deployed build under C:\workshop\aura-<version>.
#
# Why this exists: aura only self-registers startup + creates shortcuts when it detects
# a Squirrel install (an Update.exe sibling, see src/installer/windows_squirrel.rs
# ensure_startup_registered). Running from C:\workshop\aura-<ver> is not a Squirrel
# install, so the app reports SkippedNotInstalled and creates nothing. This script
# fills that gap and is re-run on every deploy so the shortcuts follow the newest build.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\set_shortcuts.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\set_shortcuts.ps1 -Target C:\path\aura.exe
#   ... -NoStartup     create Desktop + Start Menu only (skip login autostart)
#
# NOTE: keep this file ASCII-only on purpose. On a Chinese-locale system, Windows
# PowerShell 5.1 reads BOM-less UTF-8 as the ANSI codepage (GBK) and any non-ASCII byte
# can be misparsed, breaking the script parse. Same rule as flowtary's deploy_workshop.ps1.
param(
    [string]$Target,
    [string]$Root,
    [switch]$NoStartup
)

$ErrorActionPreference = 'Stop'

if (-not $Root) { $Root = Split-Path -Parent $PSScriptRoot }   # .. from tools/ = project root

if (-not $Target) {
    $cargoToml = Join-Path $Root 'Cargo.toml'
    if (-not (Test-Path $cargoToml)) { Write-Error "Cargo.toml not found: $cargoToml"; exit 1 }

    $text = [IO.File]::ReadAllText($cargoToml, [Text.Encoding]::UTF8)

    # Only look inside the [package] section: dependency entries also carry `version =`.
    $idx = $text.IndexOf('[package]')
    if ($idx -lt 0) { Write-Error "[package] section not found in $cargoToml"; exit 1 }
    $m = [regex]::Match($text.Substring($idx), '(?m)^\s*version\s*=\s*"([^"]+)"')
    if (-not $m.Success) { Write-Error "version not found in [package] section of $cargoToml"; exit 1 }

    $version = $m.Groups[1].Value
    $Target = Join-Path 'C:\workshop' "aura-$version\aura.exe"
}

if (-not (Test-Path -LiteralPath $Target)) {
    Write-Error "target exe does not exist: $Target (run 'just deploy-workshop' first)"
    exit 1
}

$workingDir = Split-Path -Parent $Target
$linkPaths = @(
    (Join-Path ([Environment]::GetFolderPath('Desktop')) 'aura.lnk'),
    (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\aura.lnk')
)
if (-not $NoStartup) {
    $linkPaths += (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Startup\aura.lnk')
}

$shell = New-Object -ComObject WScript.Shell

foreach ($linkPath in $linkPaths) {
    $linkDir = Split-Path -Parent $linkPath
    if (-not (Test-Path -LiteralPath $linkDir)) {
        New-Item -ItemType Directory -Force -Path $linkDir | Out-Null
    }

    $shortcut = $shell.CreateShortcut($linkPath)
    $shortcut.TargetPath = $Target
    $shortcut.WorkingDirectory = $workingDir
    $shortcut.IconLocation = "$Target,0"
    $shortcut.Description = 'aura wallpaper manager'
    $shortcut.Save()

    Write-Output "[shortcut] $linkPath"
    Write-Output "           -> $Target"
}

exit 0
