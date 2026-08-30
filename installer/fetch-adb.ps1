# Pre-build helper: download Google's Android platform-tools and
# extract just the three files Penflow needs into `installer/adb/`.
# The Tauri MSI bundle includes that folder as a resource so the
# installed Penflow has its own private adb.exe and never has to
# rely on whatever the user has on PATH (scoop shims, partial Android
# Studio installs, etc — see crates/penflow-transport/src/adb.rs's
# `resolve_through_shim` for why touching the user's PATH adb is a
# minefield in `windows_subsystem="windows"` parents).
#
# We download the LATEST platform-tools rather than pinning a version
# because Google rotates the URL on every release and old versions
# stop being hosted. ADB protocol is backward-compatible at the
# host-tablet wire level for any reasonable pair of versions, so
# tracking latest is fine for our purposes.
#
# Run from the repo root:
#   pwsh -File installer\fetch-adb.ps1
# Or:
#   powershell -ExecutionPolicy Bypass -File installer\fetch-adb.ps1

$ErrorActionPreference = "Stop"

$Asset = "platform-tools-latest-windows.zip"
$Url   = "https://dl.google.com/android/repository/$Asset"

$RepoRoot = Resolve-Path "$PSScriptRoot\.."
$Cache    = Join-Path $RepoRoot "installer\.cache"
$Out      = Join-Path $RepoRoot "installer\adb"
$ZipPath  = Join-Path $Cache    $Asset

New-Item -ItemType Directory -Force -Path $Cache | Out-Null
New-Item -ItemType Directory -Force -Path $Out   | Out-Null

if (-not (Test-Path $ZipPath)) {
    Write-Host "[fetch-adb] downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $ZipPath -UseBasicParsing
} else {
    Write-Host "[fetch-adb] $ZipPath already cached"
}

Write-Host "[fetch-adb] extracting to $Out"
Remove-Item -Recurse -Force "$Out\*" -ErrorAction SilentlyContinue

# Extract to a temp dir then cherry-pick. The full platform-tools zip
# is ~16 MB and ships fastboot, mke2fs, etc that we don't need; we
# only want adb.exe + the two adb DLLs (~9 MB total).
$Temp = Join-Path $Cache "adb-extract"
Remove-Item -Recurse -Force $Temp -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Temp | Out-Null

Expand-Archive -Path $ZipPath -DestinationPath $Temp -Force

# Layout in platform-tools-latest-windows.zip:
#   platform-tools/adb.exe
#   platform-tools/AdbWinApi.dll
#   platform-tools/AdbWinUsbApi.dll
#   platform-tools/<misc tools>
$Src = Join-Path $Temp "platform-tools"
if (-not (Test-Path $Src)) {
    throw "[fetch-adb] platform-tools dir missing in extracted zip — Google changed layout?"
}

$want = @("adb.exe", "AdbWinApi.dll", "AdbWinUsbApi.dll")
foreach ($name in $want) {
    $file = Join-Path $Src $name
    if (-not (Test-Path $file)) {
        throw "[fetch-adb] required file '$name' missing in platform-tools — Google changed layout?"
    }
    Copy-Item $file -Destination $Out -Force
}

Write-Host "[fetch-adb] staged:"
Get-ChildItem $Out | Format-Table Name, Length -AutoSize
