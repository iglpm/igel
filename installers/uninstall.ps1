<#
.SYNOPSIS
    Uninstalls Igel on Windows / PowerShell.

.DESCRIPTION
    Removes the `igl` binary from ~/.igel/bin (or a custom directory). Pass
    -Purge to also remove Igel's cache and configuration directory (~/.igel).
    Also cleans up the User PATH environment variable if necessary.

.EXAMPLE
    irm https://runepm.com/uninstall.ps1 | iex

.EXAMPLE
    & ([scriptblock]::Create((irm https://runepm.com/uninstall.ps1))) -Purge -Yes
#>

[CmdletBinding()]
param(
    [Alias("InstallDir", "To")]
    [string]$From = $(if ($env:IGEL_INSTALL_DIR) { $env:IGEL_INSTALL_DIR } else {
        if ($env:USERPROFILE) {
            Join-Path $env:USERPROFILE ".igel\bin"
        } else {
            Join-Path $HOME ".igel\bin"
        }
    }),

    [Alias("All")]
    [switch]$Purge,

    [Alias("y", "Force")]
    [switch]$Yes,

    [Alias("h")]
    [switch]$Help
)

$ErrorActionPreference = "Stop"

function Say {
    param([string]$Message)
    Write-Host "`e[1;35m▸`e[0m $Message"
}

function SaySuccess {
    param([string]$Message)
    Write-Host "`e[1;32m✓`e[0m $Message"
}

function Die {
    param([string]$Message)
    Write-Host "`e[1;31m✗`e[0m $Message" -ForegroundColor Red
    exit 1
}

if ($Help) {
    Get-Help $MyInvocation.MyCommand.Path
    exit 0
}

$igelHome = if ($env:IGEL_HOME) {
    $env:IGEL_HOME
} else {
    if ($env:USERPROFILE) {
        Join-Path $env:USERPROFILE ".igel"
    } else {
        Join-Path $HOME ".igel"
    }
}

if (-not $Yes -and [Environment]::UserInteractive) {
    $title = "Uninstall Igel"
    $prompt = "Are you sure you want to uninstall Igel from '$From'?"
    Write-Host -NoNewline "$prompt [y/N] "
    $response = [Console]::ReadLine()
    if ($response -notmatch '^(y|yes)$') {
        Say "uninstallation cancelled"
        exit 0
    }
}

Say "uninstalling Igel..."

# Look for igl.exe or igl
$candidates = @(
    Join-Path $From "igl.exe",
    Join-Path $From "igl"
)

$removedBin = $false
foreach ($bin in $candidates) {
    if (Test-Path $bin) {
        try {
            Remove-Item -Path $bin -Force
            SaySuccess "removed $bin"
            $removedBin = $true
        } catch {
            Die "failed to remove $bin: $($_.Exception.Message)"
        }
    }
}

if (-not $removedBin) {
    Say "binary not found in $From"
}

# Remove directory if empty and within ~/.igel
if (Test-Path $From) {
    $items = Get-ChildItem -Path $From -Force -ErrorAction SilentlyContinue
    if (-not $items -and $From.StartsWith($igelHome)) {
        Remove-Item -Path $From -Force -ErrorAction SilentlyContinue
    }
}

# Purge cache and configuration if requested
if ($Purge) {
    if (Test-Path $igelHome) {
        try {
            Remove-Item -Path $igelHome -Recurse -Force
            SaySuccess "removed $igelHome (cache and configuration)"
        } catch {
            Die "failed to remove $igelHome: $($_.Exception.Message)"
        }
    }
} else {
    if (Test-Path $igelHome) {
        Say "configuration and cache preserved at $igelHome (pass -Purge to remove)"
    }
}

# Clean User PATH if on Windows
if ($PSVersionTable.PSEdition -ne "Core" -or $IsWindows) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)
    if ($userPath) {
        $pathEntries = ($userPath -split ';') | Where-Object { $_ -ne "" }
        $normalizedFrom = if (Test-Path $From) { (Resolve-Path $From).Path.TrimEnd('\') } else { $From.TrimEnd('\') }

        $newEntries = @()
        $foundInPath = $false
        foreach ($entry in $pathEntries) {
            $normalizedEntry = $entry.TrimEnd('\')
            if ($normalizedEntry -eq $normalizedFrom) {
                $foundInPath = $true
            } else {
                $newEntries += $entry
            }
        }

        if ($foundInPath) {
            $newUserPath = $newEntries -join ';'
            [Environment]::SetEnvironmentVariable("Path", $newUserPath, [EnvironmentVariableTarget]::User)
            SaySuccess "removed $From from User PATH"
        }
    }
}

SaySuccess "Igel uninstalled successfully"
