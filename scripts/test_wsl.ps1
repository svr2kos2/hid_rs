<#
.SYNOPSIS
    Run hid_rs tests on Linux via WSL 2.

.DESCRIPTION
    Converts the project's Windows path to a WSL mount path and runs
    `cargo test` inside the specified (or default) WSL distribution.

    For hardware integration tests, USB access must first be forwarded to WSL 2
    with usbipd-win:
        usbipd list                        # find bus ID
        usbipd wsl attach --busid <busid>  # attach device to WSL

.PARAMETER Integration
    Also run hardware-dependent tests inside WSL.

.PARAMETER WslDistro
    Name of the WSL distribution to use (default: system default).

.EXAMPLE
    .\test_wsl.ps1
    .\test_wsl.ps1 -Integration -WslDistro Ubuntu-22.04
#>
param(
    [switch]$Integration,
    [string]$WslDistro = ""
)

$ErrorActionPreference = "Stop"

function Convert-ToWslPath([string]$winPath) {
    $drive = $winPath[0].ToString().ToLower()
    $rest  = $winPath.Substring(2).Replace('\', '/')
    return "/mnt/$drive$rest"
}

function Write-Step($msg) { Write-Host "`n>>> $msg" -ForegroundColor Cyan }

$projectRoot = $PSScriptRoot | Split-Path -Parent
$wslPath     = Convert-ToWslPath $projectRoot

Write-Host "=== Linux (WSL) tests ===" -ForegroundColor Cyan
Write-Host "    Project (WSL): $wslPath" -ForegroundColor DarkGray
if ($WslDistro) {
    Write-Host "    Distribution:  $WslDistro" -ForegroundColor DarkGray
}

# Build the shell command sequence to run inside WSL.
$cmds = [System.Collections.Generic.List[string]]::new()
$cmds.Add("set -e")
$cmds.Add("cd '$wslPath'")

# Ensure cargo/rustup are on PATH (common for non-login shells in WSL).
$cmds.Add('export PATH="$HOME/.cargo/bin:$PATH"')

Write-Step "Unit tests"
$cmds.Add("echo '>>> Unit tests'")
$cmds.Add("cargo test --test unit_tests")

Write-Step "Integration tests — platform sanity"
$cmds.Add("echo '>>> Integration tests (sanity)'")
$cmds.Add("cargo test --test integration_tests")

if ($Integration) {
    Write-Step "Integration tests — hardware"
    $cmds.Add("echo '>>> Integration tests (hardware)'")
    $cmds.Add("cargo test --test integration_tests --features integration-tests")
}

# Write the commands to a temporary shell script.
# Running a file (instead of bash -lc "...") avoids command-length limits,
# quoting edge-cases, and a PowerShell buffering issue where WSL output is
# swallowed when the command string is too long.
$tmpWin = Join-Path $env:TEMP "hid_rs_wsl_$([System.Diagnostics.Process]::GetCurrentProcess().Id).sh"
$wslTmp = Convert-ToWslPath $tmpWin

$scriptBody = "#!/bin/bash`n" + ($cmds -join "`n") + "`n"
[System.IO.File]::WriteAllText(
    $tmpWin,
    $scriptBody,
    (New-Object System.Text.UTF8Encoding $false)  # UTF-8, no BOM, LF line endings
)

# WSL_UTF8=1 ensures cargo/rustc output is not mangled by encoding conversion.
$env:WSL_UTF8 = "1"
try {
    if ($WslDistro) {
        wsl.exe -d $WslDistro -- bash "$wslTmp"
    } else {
        wsl.exe -- bash "$wslTmp"
    }
    if ($LASTEXITCODE -ne 0) { throw "WSL tests failed (exit $LASTEXITCODE)" }
} finally {
    Remove-Item $tmpWin -ErrorAction SilentlyContinue
}

Write-Host "`n=== Linux (WSL) tests PASSED ===" -ForegroundColor Green
