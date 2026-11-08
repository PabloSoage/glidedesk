# Builds the Windows desktop client (or, with --host, the host) with FFmpeg linked in.
#
# FFmpeg comes from vcpkg (vcpkg.json, triplet x64-windows-static-md): the vcpkg that ships
# with Visual Studio / Build Tools (component "vcpkg package manager"), or any vcpkg on PATH
# or in VCPKG_ROOT. The first run builds FFmpeg (about 15 minutes); later runs reuse it.
#
#   scripts\build-windows.ps1            # release build: target\release\glidedesk.exe
#   scripts\build-windows.ps1 --debug    # (any other arguments go to cargo)
#   scripts\build-windows.ps1 --host     # target\release\glidedesk-host.exe

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot
$triplet = 'x64-windows-static-md'

function Find-Vcpkg {
    if ($env:VCPKG_ROOT -and (Test-Path "$env:VCPKG_ROOT\vcpkg.exe")) { return "$env:VCPKG_ROOT\vcpkg.exe" }
    $onPath = Get-Command vcpkg -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        foreach ($vs in & $vswhere -all -products * -property installationPath) {
            if (Test-Path "$vs\VC\vcpkg\vcpkg.exe") { return "$vs\VC\vcpkg\vcpkg.exe" }
        }
    }
    throw 'vcpkg not found: add the "vcpkg package manager" component to Visual Studio, or set VCPKG_ROOT'
}

$vcpkg = Find-Vcpkg
# The host (--host) needs FFmpeg's encoders too: the manifest's "host" feature, installed apart.
$hostBuild = $args -contains '--host'
$rest = $args | Where-Object { $_ -ne '--host' }
$installRoot = if ($hostBuild) { "$repo\vcpkg_installed_host" } else { "$repo\vcpkg_installed" }
[string[]]$features = if ($hostBuild) { '--x-feature=host' } else { @() }
& $vcpkg install --triplet $triplet --x-manifest-root=$repo --x-install-root=$installRoot @features
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$env:FFMPEG_DIR = "$installRoot\$triplet"
$cargoArgs = @(if ($rest -contains '--debug') { $rest | Where-Object { $_ -ne '--debug' } } else { @('--release') + $rest })
if ($hostBuild) {
    # No GNOME and no web front end on Windows yet.
    cargo build -p glidedesk-host --no-default-features @cargoArgs
} else {
    cargo build -p glidedesk-desktop @cargoArgs
}
exit $LASTEXITCODE
