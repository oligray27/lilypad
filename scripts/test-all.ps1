# Every automated LilyPad check, Windows and Linux, in one run, with a pass/fail summary.
#
#   ./scripts/test-all.ps1                  # Windows checks, then Linux on the default host
#   ./scripts/test-all.ps1 -SkipLinux       # Windows checks only
#   ./scripts/test-all.ps1 -SkipWindows     # Linux checks only
#   ./scripts/test-all.ps1 -LinuxHost me@box
#
# Linux runs on a host with the rootless build container described in docs/linux-baseline.md:
# the source is copied into an isolated workspace there and nothing touches the user's real
# LilyPad profile. The desktop smoke test on the real session bus is skipped while a LilyPad is
# running there (it would refuse anyway); the others run on a private bus.
#
# What this does NOT cover -- real games, Proton, notifications, the tray, packages, Windows
# desktop behaviour -- is in docs/release-test-plan.md.
param(
    [string]$LinuxHost = 'ogray@bazzite',
    [string]$RemoteRoot = '/var/home/ogray/.local/share/lilypad-parity',
    [switch]$SkipLinux,
    [switch]$SkipWindows
)
$ErrorActionPreference = 'Continue'
Set-Location (Join-Path $PSScriptRoot '..')
$results = [System.Collections.Generic.List[object]]::new()

function Step([string]$name, [scriptblock]$body) {
    Write-Host "`n=== $name" -ForegroundColor Cyan
    $started = Get-Date
    & $body
    $ok = $LASTEXITCODE -eq 0
    $results.Add([pscustomobject]@{ Check = $name; Result = $(if ($ok) { 'PASS' } else { 'FAIL' }); Seconds = [int]((Get-Date) - $started).TotalSeconds })
}

function Skip([string]$name, [string]$why) {
    Write-Host "`n=== $name (skipped: $why)" -ForegroundColor Yellow
    $results.Add([pscustomobject]@{ Check = $name; Result = "SKIP ($why)"; Seconds = 0 })
}

# --- Windows -----------------------------------------------------------------------------
if ($SkipWindows) { Skip 'Windows' 'requested' } else {
Step 'Windows: core + Tauri unit/integration tests' { cargo test -p lilypad-core -p lilypad --locked }
Step 'Windows: real-process end-to-end tests' {
    cargo test -p lilypad-core --locked --test e2e_process -- --ignored --test-threads=1
}
Step 'Windows: Tauri app builds' { cargo build -p lilypad --locked }
Step 'Windows: GTK type-check (only Linux-only notify.rs errors allowed)' {
    $out = & ./scripts/check-gtk-on-windows.ps1 2>&1 | Out-String
    $unexpected = $out -split "`n" | Where-Object { $_ -match '^error\[' } |
        Where-Object { $_ -notmatch 'get_server_information|show_async' }
    $unexpected | ForEach-Object { Write-Host $_ }
    $global:LASTEXITCODE = [int]([bool]$unexpected)
}
}

# --- Linux -------------------------------------------------------------------------------
$reachable = $false
if (-not $SkipLinux) {
    ssh -o BatchMode=yes -o ConnectTimeout=8 $LinuxHost 'true' 2>$null
    $reachable = $LASTEXITCODE -eq 0
}
if ($SkipLinux) {
    Skip 'Linux' 'requested'
} elseif (-not $reachable) {
    Skip 'Linux' "cannot reach $LinuxHost"
} else {
    Step 'Linux: copy source to the isolated workspace' {
        tar -czf target/linux-parity-source.tar.gz --exclude=target --exclude=node_modules --exclude=.git --exclude=*.log `
            Cargo.toml Cargo.lock crates src-tauri src index.html scripts package.json package-lock.json .github docs LINUX_PARITY_PLAN.md decky
        if ($LASTEXITCODE -eq 0) { scp -q target/linux-parity-source.tar.gz "${LinuxHost}:$RemoteRoot/source.tar.gz" }
        if ($LASTEXITCODE -eq 0) {
            # Tracked dirs are cleared first so files deleted locally do not linger remotely.
            ssh $LinuxHost "set -e; S=$RemoteRoot/source; rm -rf `$S/crates `$S/src-tauri/src `$S/scripts; tar -xzf $RemoteRoot/source.tar.gz -C `$S"
        }
    }
    Step 'Linux: tests (core, e2e, GTK, no-notification-daemon) + release build' {
        ssh $LinuxHost "podman start lilypad-parity-build >/dev/null && podman exec -e CARGO_BUILD_JOBS=4 lilypad-parity-build bash scripts/validate-linux.sh 2>&1 | grep -E '^test result|FAILED|panicked|^error|Finished' ; exit `${PIPESTATUS[0]}"
    }
    $desktop = "cd $RemoteRoot/source && export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=wayland"
    $running = ssh $LinuxHost 'pgrep -x lilypad-gtk >/dev/null && echo yes'
    if ($running -eq 'yes') {
        Skip 'Linux desktop: startup + second instance (real session, tray present)' 'your LilyPad is running'
    } else {
        Step 'Linux desktop: startup + second instance (real session, tray present)' {
            ssh $LinuxHost "$desktop DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus && bash scripts/smoke-linux-desktop.sh ../target/release/lilypad-gtk | grep -E 'passed'; exit `${PIPESTATUS[0]}"
        }
    }
    Step 'Linux desktop: upgrade from v0.5.5 data (private bus)' {
        ssh $LinuxHost "$desktop && LILYPAD_SMOKE_SEED=crates/lilypad-core/tests/fixtures/linux-legacy dbus-run-session -- bash scripts/smoke-linux-desktop.sh ../target/release/lilypad-gtk 2>/dev/null | grep -E 'passed|migrated'; exit `${PIPESTATUS[0]}"
    }
    Step 'Linux desktop: no tray host, no notification daemon (private bus)' {
        ssh $LinuxHost "$desktop && dbus-run-session -- bash scripts/smoke-linux-desktop.sh ../target/release/lilypad-gtk 2>/dev/null | grep -E 'passed|tray failed'; exit `${PIPESTATUS[0]}"
    }
    ssh $LinuxHost 'podman stop --time 1 lilypad-parity-build >/dev/null 2>&1'
}

Write-Host "`n=== Summary" -ForegroundColor Cyan
foreach ($r in $results) {
    $colour = switch -Wildcard ($r.Result) { 'PASS' { 'Green' } 'FAIL' { 'Red' } default { 'Yellow' } }
    Write-Host ("{0,-6} {1,5}s  {2}" -f $r.Result.Split(' ')[0], $r.Seconds, "$($r.Check) $(if ($r.Result -like 'SKIP*') { $r.Result.Substring(4) })") -ForegroundColor $colour
}
if ($results | Where-Object Result -eq 'FAIL') { exit 1 }
