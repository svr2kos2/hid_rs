<#
.SYNOPSIS
    Cross-compile hid_rs tests for Android and execute them on a connected
    device via ADB.

.DESCRIPTION
    Workflow:
      1. Cross-compile the test binary with `cargo test --no-run`.
      2. Push the binary to /data/local/tmp/ on the device.
      3. Execute it via `adb shell`.

    Prerequisites:
      • Android NDK installed and ANDROID_NDK_HOME set (or cargo-ndk installed).
      • Rust target added: rustup target add <TARGET>.
      • A linker configured in .cargo/config.toml for the chosen target
        (see docs/android_setup.md or use `cargo ndk`).
      • ADB in PATH with a device connected and USB debugging enabled.

.PARAMETER Target
    Rust target triple for the Android ABI.
    Common values:
        aarch64-linux-android     (64-bit ARM, most modern devices)
        armv7-linux-androideabi   (32-bit ARM)
        x86_64-linux-android      (x86_64 emulator)
        i686-linux-android        (x86 emulator)

.PARAMETER DeviceSerial
    ADB serial of a specific device (output of `adb devices`).
    Leave empty to use the only connected device.

.PARAMETER Integration
    Include hardware-dependent tests (--features integration-tests).

.PARAMETER UseCargoNdk
    Use `cargo ndk` instead of plain `cargo` for cross-compilation.
    Requires: cargo install cargo-ndk

.EXAMPLE
    .\test_android.ps1
    .\test_android.ps1 -Integration
    .\test_android.ps1 -Target x86_64-linux-android -DeviceSerial emulator-5554
    .\test_android.ps1 -UseCargoNdk -Integration
#>
param(
    [string]$Target      = "aarch64-linux-android",
    [string]$DeviceSerial = "",
    [switch]$Integration,
    [switch]$UseCargoNdk,
    # Minimum Android API level used when looking up the NDK clang wrapper.
    [int]$MinApi = 21
)

$ErrorActionPreference = "Stop"

function Write-Step($msg) { Write-Host "`n>>> $msg" -ForegroundColor Cyan }
function Invoke-Adb {
    if ($DeviceSerial) {
        adb -s $DeviceSerial @args
    } else {
        adb @args
    }
}

Write-Host "=== Android tests (ADB) ===" -ForegroundColor Cyan
Write-Host "    Target: $Target" -ForegroundColor DarkGray

# ── Check ADB ─────────────────────────────────────────────────────────────────
Write-Step "Checking ADB"
if (-not (Get-Command adb -ErrorAction SilentlyContinue)) {
    throw "adb not found in PATH.  Install Android SDK Platform Tools."
}

$deviceLines = adb devices | Select-Object -Skip 1 |
               Where-Object { $_ -match "\bdevice$" }
if (-not $deviceLines) {
    throw "No ADB device/emulator found.  Connect a device and enable USB debugging."
}

# If multiple devices are connected and no serial was specified, auto-pick the
# first one and tell the user. They can override with -DeviceSerial.
if (-not $DeviceSerial) {
    $firstSerial = ($deviceLines | Select-Object -First 1) -split '\s+' | Select-Object -First 1
    if (($deviceLines | Measure-Object).Count -gt 1) {
        Write-Host "    Multiple devices found — auto-selecting: $firstSerial" -ForegroundColor Yellow
        Write-Host "    Pass -DeviceSerial to target a specific device." -ForegroundColor DarkGray
        $deviceLines | ForEach-Object { Write-Host "      $_" -ForegroundColor DarkGray }
    }
    $DeviceSerial = $firstSerial
}
Write-Host "    Using device: $DeviceSerial" -ForegroundColor Green

# ── Ensure Rust target is installed ───────────────────────────────────────────
Write-Step "Ensuring Rust target is installed"
rustup target add $Target
if ($LASTEXITCODE -ne 0) { throw "rustup target add failed" }

$projectRoot = $PSScriptRoot | Split-Path -Parent
Push-Location $projectRoot

try {
    $featureArgs = if ($Integration) { @("--features", "integration-tests") } else { @() }

    # ── Choose toolchain: cargo-ndk (preferred) or manual NDK linker env var ────
    #
    # cargo-ndk auto-detects the NDK and configures all linker env vars.
    # Without it, we locate the NDK ourselves and set CARGO_TARGET_*_LINKER.

    # Auto-detect cargo-ndk even when -UseCargoNdk was not passed.
    $useNdkTool = $UseCargoNdk
    if (-not $useNdkTool -and (Get-Command cargo-ndk -ErrorAction SilentlyContinue)) {
        Write-Host "    cargo-ndk detected — using it automatically" -ForegroundColor DarkGray
        $useNdkTool = $true
    }

    if ($useNdkTool) {
        # Ensure cargo-ndk is installed.
        if (-not (Get-Command cargo-ndk -ErrorAction SilentlyContinue)) {
            Write-Host "    cargo-ndk not found — installing..." -ForegroundColor Yellow
            cargo install cargo-ndk
            if ($LASTEXITCODE -ne 0) { throw "cargo install cargo-ndk failed" }
        }
    } else {
        # ── Manual NDK linker setup via env vars ──────────────────────────────
        Write-Step "Locating Android NDK for linker configuration"

        $ndkHome = $env:ANDROID_NDK_HOME
        if (-not $ndkHome) { $ndkHome = $env:ANDROID_NDK }
        if (-not $ndkHome) { $ndkHome = $env:NDK_HOME }
        if (-not $ndkHome) {
            foreach ($sdk in @($env:ANDROID_SDK_ROOT, $env:ANDROID_HOME)) {
                if ($sdk) {
                    $ndkDir = Get-ChildItem (Join-Path $sdk "ndk") -Directory -ErrorAction SilentlyContinue |
                              Sort-Object Name -Descending | Select-Object -First 1
                    if ($ndkDir) { $ndkHome = $ndkDir.FullName; break }
                }
            }
        }
        if (-not $ndkHome) {
            throw (
                "Android NDK not found. Options:`n" +
                "  1. Set ANDROID_NDK_HOME to your NDK directory.`n" +
                "  2. Or install cargo-ndk for automatic setup: cargo install cargo-ndk"
            )
        }
        Write-Host "    NDK: $ndkHome" -ForegroundColor DarkGray

        # Map Rust target to the NDK clang wrapper prefix.
        $clangPrefix = switch ($Target) {
            "aarch64-linux-android"   { "aarch64-linux-android" }
            "armv7-linux-androideabi" { "armv7a-linux-androideabi" }
            "x86_64-linux-android"   { "x86_64-linux-android" }
            "i686-linux-android"     { "i686-linux-android" }
            default { throw "Unsupported target triple: $Target" }
        }
        $toolchainBin = Join-Path $ndkHome "toolchains\llvm\prebuilt\windows-x86_64\bin"

        # NDK r23+ ships .cmd wrappers on Windows; older NDKs may ship bare .exe.
        $linker = Join-Path $toolchainBin "${clangPrefix}${MinApi}-clang.cmd"
        if (-not (Test-Path $linker)) {
            $linker = Join-Path $toolchainBin "${clangPrefix}${MinApi}-clang.exe"
        }
        if (-not (Test-Path $linker)) {
            throw (
                "NDK clang wrapper not found:`n  $linker`n" +
                "  Check NDK version and MinApi level (currently $MinApi)."
            )
        }
        Write-Host "    Linker: $linker" -ForegroundColor DarkGray

        # cargo reads CARGO_TARGET_<UPPER_TARGET>_LINKER to find the C linker.
        $linkerEnvKey = "CARGO_TARGET_$($Target.ToUpper() -replace '-','_')_LINKER"
        [System.Environment]::SetEnvironmentVariable($linkerEnvKey, $linker, "Process")
        Write-Host "    $linkerEnvKey = $linker" -ForegroundColor DarkGray
    }

    # ── Cross-compile test binary ──────────────────────────────────────────────
    Write-Step "Cross-compiling test binary (--no-run)"
    if ($useNdkTool) {
        $ndkAbi = switch ($Target) {
            "aarch64-linux-android"   { "arm64-v8a" }
            "armv7-linux-androideabi" { "armeabi-v7a" }
            "x86_64-linux-android"   { "x86_64" }
            "i686-linux-android"     { "x86" }
            default                  { $Target }
        }
        cargo ndk --target $ndkAbi test --no-run @featureArgs
    } else {
        cargo test --target $Target --no-run @featureArgs
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo test --no-run failed" }

    # ── Locate the compiled test binary ───────────────────────────────────────
    Write-Step "Locating test binary"
    $depsDir = Join-Path $projectRoot "target\$Target\debug\deps"

    # The binary has no extension on Linux targets; match hid_rs-<hex>.
    $testBin = Get-ChildItem $depsDir -File |
               Where-Object { $_.Name -match "^hid_rs-[0-9a-f]+" -and $_.Extension -eq "" } |
               Sort-Object LastWriteTime -Descending |
               Select-Object -First 1

    if (-not $testBin) {
        throw "Could not find test binary in $depsDir.  Run with -Verbose for details."
    }
    Write-Host "    Binary: $($testBin.FullName)" -ForegroundColor DarkGray

    # ── Push and execute on device ────────────────────────────────────────────
    $remotePath = "/data/local/tmp/$($testBin.Name)"

    Write-Step "Pushing binary to device"
    Invoke-Adb push $testBin.FullName $remotePath
    Invoke-Adb shell chmod +x $remotePath

    Write-Step "Running tests on device"
    # --test-threads=1 avoids race conditions on a single shared device.
    Invoke-Adb shell "$remotePath --test-threads=1"
    if ($LASTEXITCODE -ne 0) { throw "Tests failed on device (exit $LASTEXITCODE)" }

    # ── Cleanup ───────────────────────────────────────────────────────────────
    Invoke-Adb shell rm -f $remotePath

    Write-Host "`n=== Android tests PASSED ===" -ForegroundColor Green
} finally {
    Pop-Location
}
