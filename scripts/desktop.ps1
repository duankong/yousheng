param([ValidateSet('dev','build','test')][string]$Mode='dev',[switch]$Integration,[switch]$SignedRelease)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$previousTargetFlags = $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS
if ($SignedRelease -and ($Mode -ne 'build' -or !$env:TAURI_SIGNING_PRIVATE_KEY)) {
    throw 'Use -SignedRelease with build and set TAURI_SIGNING_PRIVATE_KEY for the official release.'
}
Push-Location $root
try {
    # The prebuilt WebRTC library links the static MSVC runtime.
    $localCargoConfig = Get-Item -LiteralPath '.cargo/config.toml', '.cargo/config' -ErrorAction SilentlyContinue
    $configuredCrt = $localCargoConfig | Select-String -Pattern '[+-]crt-static' -Quiet
    if (!$configuredCrt -and $previousTargetFlags -notmatch '[+-]crt-static') {
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = "$previousTargetFlags -C target-feature=+crt-static".Trim()
    }
    if (!(Get-Command cl.exe -ErrorAction SilentlyContinue)) {
        $locator = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
        if (!(Test-Path $locator)) { throw 'Install Visual Studio C++ Build Tools with a Windows SDK.' }
        $vs = & $locator -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (!$vs) { throw 'Windows C++ Build Tools installation is incomplete.' }
        & (Join-Path $vs 'Common7/Tools/Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
    }
    & (Join-Path $PSScriptRoot 'prepare-livekit.ps1')
    if (!(Test-Path 'ui/node_modules/.bin/tauri.cmd')) {
        npm.cmd --prefix ui ci
        if ($LASTEXITCODE) { exit $LASTEXITCODE }
    }
    if ($Mode -eq 'build') {
        cargo fetch --locked --manifest-path src-tauri/Cargo.toml --target x86_64-pc-windows-msvc
        if ($LASTEXITCODE) { exit $LASTEXITCODE }
        $env:LK_CUSTOM_WEBRTC = & (Join-Path $PSScriptRoot 'prepare-webrtc.ps1') -PackageNotices
    } else {
        $env:LK_CUSTOM_WEBRTC = & (Join-Path $PSScriptRoot 'prepare-webrtc.ps1')
    }
    if ($Mode -eq 'test') {
        npm.cmd --prefix ui test
        if ($LASTEXITCODE) { exit $LASTEXITCODE }
        npm.cmd --prefix ui run build
        if ($LASTEXITCODE) { exit $LASTEXITCODE }
        cargo test --locked --target x86_64-pc-windows-msvc --manifest-path src-tauri/Cargo.toml
        if ($LASTEXITCODE) { exit $LASTEXITCODE }
        if ($Integration) {
            $env:DUIPIN_TEST_SFU = (Resolve-Path 'src-tauri/resources/livekit-server.exe').Path
            cargo test --locked --target x86_64-pc-windows-msvc --manifest-path src-tauri/Cargo.toml -- --ignored --test-threads=1
            if ($LASTEXITCODE) { exit $LASTEXITCODE }
            $env:DUIPIN_TEST_LOCAL_RELAY = '1'
            cargo test --locked --target x86_64-pc-windows-msvc --manifest-path src-tauri/Cargo.toml scenario_tests -- --ignored --test-threads=1
        }
    } else {
        $options = @('--target', 'x86_64-pc-windows-msvc')
        if ($Mode -eq 'build' -and !$SignedRelease) {
            $options += @('--config', (Join-Path $PSScriptRoot 'tauri.local.conf.json'))
        }
        & 'ui/node_modules/.bin/tauri.cmd' $Mode @options
    }
    exit $LASTEXITCODE
} finally {
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = $previousTargetFlags
    Pop-Location
}
