[CmdletBinding()]
param(
    [switch]$Build,
    [string]$SourceDirectory,
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'zed-tui')
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
$InstallDirectory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDirectory)

if (-not $SourceDirectory) {
    $targetDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
    if (-not [System.IO.Path]::IsPathRooted($targetDirectory)) {
        $targetDirectory = Join-Path $repo $targetDirectory
    }
    $SourceDirectory = Join-Path $targetDirectory 'release'
}

if ($Build) {
    Push-Location $repo
    try {
        & cargo build --locked --release -p zed -p cli
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
}

foreach ($binary in @('zed.exe', 'cli.exe')) {
    if (-not (Test-Path -LiteralPath (Join-Path $SourceDirectory $binary) -PathType Leaf)) {
        throw "Missing $binary in $SourceDirectory. Run script\zed-tui-install.ps1 -Build from a Visual Studio developer PowerShell."
    }
}

$binDirectory = Join-Path $InstallDirectory 'bin'
New-Item -ItemType Directory -Force -Path $binDirectory | Out-Null
Copy-Item -LiteralPath (Join-Path $SourceDirectory 'zed.exe') -Destination (Join-Path $InstallDirectory 'zed.exe') -Force
Copy-Item -LiteralPath (Join-Path $SourceDirectory 'cli.exe') -Destination (Join-Path $binDirectory 'zed.exe') -Force

Get-ChildItem -LiteralPath $SourceDirectory -Filter '*.dll' -File | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination $InstallDirectory -Force
}
$consoleHost = Join-Path $SourceDirectory 'OpenConsole.exe'
if (Test-Path -LiteralPath $consoleHost -PathType Leaf) {
    Copy-Item -LiteralPath $consoleHost -Destination $InstallDirectory -Force
}

$wrapper = Join-Path $binDirectory 'zed-tui.cmd'
[System.IO.File]::WriteAllText($wrapper, "@echo off`r`n`"%~dp0zed.exe`" --tui %*`r`nexit /b %errorlevel%`r`n", [System.Text.Encoding]::ASCII)
Write-Host "Installed $wrapper"
Write-Host "Run: & '$wrapper' ."
Write-Host "Add '$binDirectory' to your user PATH to run zed-tui from any terminal."
