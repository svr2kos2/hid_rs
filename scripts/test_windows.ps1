<#
.SYNOPSIS
    Run hid_rs tests on Windows.

.PARAMETER Integration
    Also run hardware-dependent tests (requires a HID device connected).

.EXAMPLE
    # Unit tests only (no hardware):
    .\test_windows.ps1

    # With hardware:
    .\test_windows.ps1 -Integration
#>
param(
    [switch]$Integration
)

$ErrorActionPreference = "Stop"
$projectRoot = $PSScriptRoot | Split-Path -Parent

function Write-Step($msg) { Write-Host "`n>>> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "    OK: $msg" -ForegroundColor Green }
function Write-Fail($msg) { Write-Host "    FAIL: $msg" -ForegroundColor Red }

Push-Location $projectRoot
try {
    # ── Unit tests ────────────────────────────────────────────────────────────
    Write-Step "Unit tests (no hardware required)"
    cargo test --test unit_tests
    if ($LASTEXITCODE -ne 0) { throw "unit_tests failed (exit $LASTEXITCODE)" }
    Write-Ok "unit_tests"

    # ── Non-hardware integration tests ────────────────────────────────────────
    Write-Step "Integration tests — platform sanity (no hardware required)"
    cargo test --test integration_tests
    if ($LASTEXITCODE -ne 0) { throw "integration_tests (sanity) failed (exit $LASTEXITCODE)" }
    Write-Ok "integration_tests (sanity)"

    # ── Hardware integration tests ────────────────────────────────────────────
    if ($Integration) {
        Write-Step "Integration tests — hardware (device must be connected)"
        cargo test --test integration_tests --features integration-tests
        if ($LASTEXITCODE -ne 0) { throw "integration_tests (hardware) failed (exit $LASTEXITCODE)" }
        Write-Ok "integration_tests (hardware)"
    } else {
        Write-Host "`n    Skipping hardware tests. Pass -Integration to enable." -ForegroundColor DarkGray
    }

    Write-Host "`n=== Windows tests PASSED ===" -ForegroundColor Green
} finally {
    Pop-Location
}
