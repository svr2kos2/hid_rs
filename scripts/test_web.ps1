<#
.SYNOPSIS
    Run hid_rs WASM / WebHID tests with wasm-pack.

.DESCRIPTION
    Two modes:

    --Headless  (default when no flag given)
        Runs tests in headless Chrome.  Suitable for pure-logic tests.
        WebHID is NOT available in headless Chrome, so WebHID-dependent
        tests are skipped automatically via is_supported() guards.

    --Browser
        Opens a visible Chrome window.  WebHID IS available; tests can
        access HID devices that were previously granted by this Chrome
        profile (via requestDevice in an earlier session).
        Pair your device once manually, then re-run with --Browser for
        automated verification.

.PARAMETER Headless
    Run in headless Chrome (default).

.PARAMETER Browser
    Open Chrome with a visible window (required for WebHID device access).

.PARAMETER Firefox
    Use Firefox instead of Chrome.

.PARAMETER ChromeProfile
    Path to a Chrome user-data-dir that already has HID device grants.
    Passed via CHROME_FLAGS env var so wasm-pack forwards it to the browser.

.EXAMPLE
    .\test_web.ps1                           # headless, Chrome
    .\test_web.ps1 -Browser                  # visible Chrome, WebHID enabled
    .\test_web.ps1 -Browser -Firefox         # visible Firefox
    .\test_web.ps1 -Browser -ChromeProfile "C:\MyProfile"
#>
param(
    [switch]$Headless,
    [switch]$Browser,
    [switch]$Firefox,
    [string]$ChromeProfile = ""
)

$ErrorActionPreference = "Stop"

# Default to headless when neither flag is given.
if (-not $Browser) { $Headless = $true }

function Write-Step($msg) { Write-Host "`n>>> $msg" -ForegroundColor Cyan }

Write-Host "=== Web (WASM/WebHID) tests ===" -ForegroundColor Cyan

# ── Ensure wasm-pack is installed ─────────────────────────────────────────────
Write-Step "Checking wasm-pack"
if (-not (Get-Command wasm-pack -ErrorAction SilentlyContinue)) {
    Write-Host "    wasm-pack not found — installing via cargo install..." -ForegroundColor Yellow
    cargo install wasm-pack
}
Write-Host "    wasm-pack: $(wasm-pack --version)"

$projectRoot = $PSScriptRoot | Split-Path -Parent
Push-Location $projectRoot

try {
    $browserFlag = if ($Firefox) { "--firefox" } else { "--chrome" }

    # ── Optional: forward a Chrome user-data-dir for persisted HID grants ─────
    if ($ChromeProfile -and -not $Firefox) {
        $env:WASM_BINDGEN_TEST_TIMEOUT = "60"  # longer timeout when opening browser
        $env:CHROME_FLAGS = "--user-data-dir=`"$ChromeProfile`""
        Write-Host "    Chrome profile: $ChromeProfile" -ForegroundColor DarkGray
    }

    if ($Headless) {
        Write-Step "Running WASM tests (headless Chrome — no WebHID)"
        Write-Host "    WebHID device tests will be skipped gracefully." -ForegroundColor DarkGray
        wasm-pack test $browserFlag --headless
    } else {
        Write-Step "Running WASM tests (browser with UI — WebHID enabled)"
        Write-Host "    Ensure your HID device is connected." -ForegroundColor Yellow
        Write-Host "    Chrome must have previously granted access to the device." -ForegroundColor Yellow
        Write-Host "    The test server starts at http://localhost:8000 automatically." -ForegroundColor DarkGray
        wasm-pack test $browserFlag
    }

    if ($LASTEXITCODE -ne 0) { throw "wasm-pack test failed (exit $LASTEXITCODE)" }

    Write-Host "`n=== Web tests PASSED ===" -ForegroundColor Green
} finally {
    Pop-Location
    Remove-Item Env:\CHROME_FLAGS -ErrorAction SilentlyContinue
}
