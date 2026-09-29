<#
.SYNOPSIS
    Uninstalls Rune on Windows / PowerShell.

.DESCRIPTION
    Removes the `rune` binary from ~/.rune/bin (or a custom directory). Pass
    -Purge to also remove Rune's cache and configuration directory (~/.rune).
    Also cleans up the User PATH environment variable if necessary.

.EXAMPLE
    irm https://raw.githubusercontent.com/runepm/rune/main/installers/uninstall.ps1 | iex

.EXAMPLE
    & ([scriptblock]::Create((irm https://raw.githubusercontent.com/runepm/rune/main/installers/uninstall.ps1))) -Purge -Yes
#>

[CmdletBinding()]
param(
    [Alias("InstallDir", "To")]
    [string]$From = $(if ($env:RUNE_INSTALL_DIR) { $env:RUNE_INSTALL_DIR } else {
        if ($env:USERPROFILE) {
            Join-Path $env:USERPROFILE ".rune\bin"
        } else {
            Join-Path $HOME ".rune\bin"
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

$runeHome = if ($env:RUNE_HOME) {
    $env:RUNE_HOME
} else {
    if ($env:USERPROFILE) {
        Join-Path $env:USERPROFILE ".rune"
    } else {
        Join-Path $HOME ".rune"
    }
}

if (-not $Yes -and [Environment]::UserInteractive) {
    $title = "Uninstall Rune"
    $prompt = "Are you sure you want to uninstall Rune from '$From'?"
    Write-Host -NoNewline "$prompt [y/N] "
    $response = [Console]::ReadLine()
    if ($response -notmatch '^(y|yes)$') {
        Say "uninstallation cancelled"
        exit 0
    }
}

Say "uninstalling Rune..."

# Look for rune.exe or rune
$candidates = @(
    Join-Path $From "rune.exe",
    Join-Path $From "rune"
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

# Remove directory if empty and within ~/.rune
if (Test-Path $From) {
    $items = Get-ChildItem -Path $From -Force -ErrorAction SilentlyContinue
    if (-not $items -and $From.StartsWith($runeHome)) {
        Remove-Item -Path $From -Force -ErrorAction SilentlyContinue
    }
}

# Purge cache and configuration if requested
if ($Purge) {
    if (Test-Path $runeHome) {
        try {
            Remove-Item -Path $runeHome -Recurse -Force
            SaySuccess "removed $runeHome (cache and configuration)"
        } catch {
            Die "failed to remove $runeHome: $($_.Exception.Message)"
        }
    }
} else {
    if (Test-Path $runeHome) {
        Say "configuration and cache preserved at $runeHome (pass -Purge to remove)"
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

SaySuccess "Rune uninstalled successfully"
