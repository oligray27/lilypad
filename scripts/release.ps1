# Usage: .\scripts\release.ps1
# Bumps the patch version, builds for production, commits, tags, pushes, and creates a GitHub release.
# Requires: node, cargo/tauri, git, gh (GitHub CLI)

$ErrorActionPreference = 'Stop'

$Root = Split-Path $PSScriptRoot -Parent
Set-Location $Root

# --- Read current version ---
$TauriConf = Get-Content 'src-tauri/tauri.conf.json' -Raw | ConvertFrom-Json
$Current = $TauriConf.version

# --- Bump patch ---
$Parts = $Current.Split('.')
$Parts[2] = [string]([int]$Parts[2] + 1)
$New = $Parts -join '.'

Write-Host "Releasing $Current -> $New"

# --- Update tauri.conf.json ---
$TauriConf.version = $New
$TauriConf | ConvertTo-Json -Depth 10 | Set-Content 'src-tauri/tauri.conf.json' -NoNewline
Add-Content 'src-tauri/tauri.conf.json' ''  # ensure trailing newline

# --- Update package.json ---
$Pkg = Get-Content 'package.json' -Raw | ConvertFrom-Json
$Pkg.version = $New
$Pkg | ConvertTo-Json -Depth 10 | Set-Content 'package.json' -NoNewline
Add-Content 'package.json' ''

# --- Update Cargo.toml (first version = "..." in [package]) ---
$Cargo = Get-Content 'src-tauri/Cargo.toml' -Raw
$Cargo = $Cargo -replace '^version = "[^"]+"', "version = `"$New`""
Set-Content 'src-tauri/Cargo.toml' $Cargo -NoNewline

# --- Build ---
Write-Host 'Building...'
npm run build

# --- Commit version bump ---
git add package.json src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "chore: release v$New"

# --- Tag ---
git tag "v$New"

# --- Push ---
git push origin main
git push origin "v$New"

# --- Collect installer artifacts (latest only) ---
$Assets = @()
$NsisDir = 'src-tauri/target/release/bundle/nsis'
if (Test-Path $NsisDir) {
    $f = Get-ChildItem $NsisDir -Filter '*.exe' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($f) { $Assets += $f.FullName }
}

# --- Create GitHub release ---
if ($Assets.Count -eq 0) {
    Write-Host 'Warning: no installer artifacts found, creating release without assets'
    gh release create "v$New" --title "LilyPad v$New" --generate-notes
} else {
    Write-Host "Creating release with: $($Assets -join ', ')"
    gh release create "v$New" @Assets --title "LilyPad v$New" --generate-notes
}

Write-Host "Done -- v$New released."
