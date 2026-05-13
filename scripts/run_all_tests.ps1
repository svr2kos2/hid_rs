<#
.SYNOPSIS
    Master test orchestrator for hid_rs — runs tests across all target platforms.

.DESCRIPTION
    Delegates to the individual platform scripts:
        test_windows.ps1   — native Windows (cargo test)
        test_wsl.ps1       — Linux via WSL 2
        test_web.ps1       — WASM / WebHID (wasm-pack test)
        test_android.ps1   — Android via ADB

    By default, only the Windows tests run (safe for a clean checkout).
    Pass the relevant switches to enable other platforms.

.PARAMETER Windows
    Run Windows tests (on by default unless another platform is explicitly chosen).

.PARAMETER Linux
    Run Linux tests via WSL 2.

.PARAMETER Web
    Run WASM / WebHID tests via wasm-pack.

.PARAMETER Android
    Run tests on an Android device via ADB.

.PARAMETER All
    Run all platform tests.

.PARAMETER Integration
    Enable hardware-dependent tests on every selected platform.

.PARAMETER WebBrowser
    Open a visible Chrome window for WebHID tests (default: headless).
    Ignored unless -Web or -All is also passed.

.PARAMETER AndroidTarget
    Rust target triple for Android (default: aarch64-linux-android).

.PARAMETER AndroidSerial
    ADB device serial to target a specific Android device.

.PARAMETER WslDistro
    WSL distribution name (default: system default).

.EXAMPLE
    # Quick smoke-test on Windows only:
    .\run_all_tests.ps1

    # All platforms, no hardware:
    .\run_all_tests.ps1 -All

    # All platforms with hardware:
    .\run_all_tests.ps1 -All -Integration

    # Windows + Linux, hardware enabled:
    .\run_all_tests.ps1 -Windows -Linux -Integration

    # Android only, specific device:
    .\run_all_tests.ps1 -Android -AndroidSerial emulator-5554 -Integration
#>
param(
    [switch]$Windows,
    [switch]$Linux,
    [switch]$Web,
    [switch]$Android,
    [switch]$All,
    [switch]$Integration,
    [switch]$WebBrowser,
    [string]$AndroidTarget = "aarch64-linux-android",
    [string]$AndroidSerial = "",
    [string]$WslDistro     = ""
)

# Default: run Windows tests when no platform flag is supplied.
if (-not ($Windows -or $Linux -or $Web -or $Android -or $All)) {
    $Windows = $true
}
if ($All) { $Windows = $Linux = $Web = $Android = $true }

$ErrorActionPreference = "Stop"
$ScriptDir = $PSScriptRoot
$results   = [ordered]@{}

function Invoke-PlatformTests([string]$Name, [string]$Script, [hashtable]$Params) {
    $bar = "─" * 50
    Write-Host "`n$bar" -ForegroundColor Blue
    Write-Host " Platform: $Name" -ForegroundColor Blue
    Write-Host "$bar" -ForegroundColor Blue
    try {
        & (Join-Path $ScriptDir $Script) @Params
        $results[$Name] = "PASS"
    } catch {
        Write-Host "  ERROR: $_" -ForegroundColor Red
        $results[$Name] = "FAIL"
    }
}

# ── Windows ───────────────────────────────────────────────────────────────────
if ($Windows) {
    $p = @{}
    if ($Integration) { $p.Integration = $true }
    Invoke-PlatformTests "Windows" "test_windows.ps1" $p
}

# ── Linux (WSL) ───────────────────────────────────────────────────────────────
if ($Linux) {
    $p = @{}
    if ($Integration)  { $p.Integration = $true }
    if ($WslDistro)    { $p.WslDistro   = $WslDistro }
    Invoke-PlatformTests "Linux (WSL)" "test_wsl.ps1" $p
}

# ── Web (WASM) ────────────────────────────────────────────────────────────────
if ($Web) {
    $p = @{}
    if ($WebBrowser) { $p.Browser  = $true } else { $p.Headless = $true }
    Invoke-PlatformTests "Web (WASM)" "test_web.ps1" $p
}

# ── Android (ADB) ─────────────────────────────────────────────────────────────
if ($Android) {
    $p = @{ Target = $AndroidTarget }
    if ($Integration)   { $p.Integration   = $true }
    if ($AndroidSerial) { $p.DeviceSerial  = $AndroidSerial }
    Invoke-PlatformTests "Android" "test_android.ps1" $p
}

# ── Summary ───────────────────────────────────────────────────────────────────
$bar = "═" * 50
Write-Host "`n$bar" -ForegroundColor Blue
Write-Host " Test Summary" -ForegroundColor Blue
Write-Host "$bar" -ForegroundColor Blue

$allPassed = $true
foreach ($kv in $results.GetEnumerator()) {
    $color = if ($kv.Value -eq "PASS") { "Green" } else { "Red" }
    $mark  = if ($kv.Value -eq "PASS") { "✓" } else { "✗" }
    Write-Host ("  $mark  {0,-20} {1}" -f $kv.Key, $kv.Value) -ForegroundColor $color
    if ($kv.Value -ne "PASS") { $allPassed = $false }
}

Write-Host $bar -ForegroundColor Blue

if (-not $allPassed) {
    Write-Host "`nOne or more platforms FAILED." -ForegroundColor Red
    exit 1
}
Write-Host "`nAll platforms PASSED." -ForegroundColor Green
