<#
.SYNOPSIS
    Installs the latest (or a given) release of Igel on Windows / PowerShell.

.DESCRIPTION
    Downloads the release archive for this machine from GitHub, verifies it
    against its published SHA-256 checksum, extracts the binary, and installs
    it to ~/.igel/bin (or a custom directory).

.EXAMPLE
    irm https://runepm.com/install.ps1 | iex

.EXAMPLE
    & ([scriptblock]::Create((irm https://runepm.com/install.ps1))) -Version 2027.0 -To "C:\Tools\Igel"
#>

[CmdletBinding()]
param(
    [Alias("v")]
    [string]$Version = $(if ($env:IGEL_VERSION) { $env:IGEL_VERSION } else { "latest" }),

    [Alias("InstallDir")]
    [string]$To = $(if ($env:IGEL_INSTALL_DIR) { $env:IGEL_INSTALL_DIR } else {
        if ($env:USERPROFILE) {
            Join-Path $env:USERPROFILE ".igel\bin"
        } else {
            Join-Path $HOME ".igel\bin"
        }
    }),

    [switch]$NoPathUpdate,

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

# Force TLS 1.2 and TLS 1.3
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
if ([Net.SecurityProtocolType].GetMember("Tls13").Count -gt 0) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls13
}

$Repo = "runepm/igel"

# Detect OS
if ($PSVersionTable.PSEdition -eq "Core" -and (-not $IsWindows)) {
    if ($IsLinux) {
        $os = "unknown-linux-gnu"
        $binExt = ""
    } elseif ($IsMacOS) {
        $os = "apple-darwin"
        $binExt = ""
    } else {
        Die "unsupported operating system"
    }
} else {
    $os = "pc-windows-msvc"
    $binExt = ".exe"
}

# Detect Architecture
$rawArch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) {
    $rawArch = $env:PROCESSOR_ARCHITEW6432
}
switch -Regex ($rawArch) {
    "^(AMD64|x86_64|x64)$" { $arch = "x86_64" }
    "^(ARM64|aarch64)$"    { $arch = "aarch64" }
    default {
        # Fallback check using RuntimeInformation if available
        $runtimeArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
        switch -Regex ($runtimeArch) {
            "^(X64|x86_64)$"  { $arch = "x86_64" }
            "^(Arm64|ARM64)$" { $arch = "aarch64" }
            default { Die "no prebuilt Igel for architecture: $rawArch" }
        }
    }
}

# `2027.0`, `v2027.0` and `v2027.0.0` all name release tag `v2027.0.0`.
if ($Version -ne "latest") {
    $v = $Version.TrimStart('v')
    if ($v -notmatch '^\d+(\.\d+){0,2}$') {
        Die "not a release version: $Version (e.g. 2027.0 or 2027.0.1)"
    }
    $parts = @($v.Split('.'))
    while ($parts.Count -lt 3) { $parts += "0" }
    $Version = "v" + ($parts -join '.')
}

$target = "$arch-$os"
$archive = "igel-$target.tar.gz"

if ($env:IGEL_DOWNLOAD_BASE) {
    $base = $env:IGEL_DOWNLOAD_BASE.TrimEnd('/')
} elseif ($Version -eq "latest") {
    $base = "https://github.com/$Repo/releases/latest/download"
} else {
    $base = "https://github.com/$Repo/releases/download/$Version"
}

# Create a temporary directory
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("igel-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    Say "downloading Igel ($Version, $target)"

    $archiveUrl = "$base/$archive"
    $sha256Url = "$base/$archive.sha256"
    $archiveDest = Join-Path $tmp $archive
    $sha256Dest = Join-Path $tmp "$archive.sha256"

    # Download archive
    try {
        Invoke-WebRequest -Uri $archiveUrl -OutFile $archiveDest -UseBasicParsing
    } catch {
        Die "download failed: $archiveUrl`n$($_.Exception.Message)"
    }

    # Download checksum
    try {
        Invoke-WebRequest -Uri $sha256Url -OutFile $sha256Dest -UseBasicParsing
    } catch {
        Die "checksum download failed: $sha256Url`n$($_.Exception.Message)"
    }

    # Verify checksum
    $expectedHash = (Get-Content -Path $sha256Dest -Raw).Trim().Split(" `t`r`n")[0].ToLower()
    $actualHash = (Get-FileHash -Path $archiveDest -Algorithm SHA256).Hash.ToLower()

    if ($expectedHash -ne $actualHash) {
        Die "checksum mismatch: expected $expectedHash, got $actualHash"
    }

    # Extract archive
    $tarCmd = Get-Command "tar" -ErrorAction SilentlyContinue
    if ($tarCmd) {
        & tar -xzf "$archiveDest" -C "$tmp"
        if ($LASTEXITCODE -ne 0) {
            Die "tar extraction failed"
        }
    } else {
        Die "tar utility was not found. Windows 10+ includes tar natively. Please install tar or extract manually."
    }

    # Ensure destination directory exists
    if (-not (Test-Path $To)) {
        New-Item -ItemType Directory -Path $To -Force | Out-Null
    }

    $binName = "igl$binExt"
    $srcBin = Join-Path $tmp (Join-Path "igel-$target" $binName)
    if (-not (Test-Path $srcBin)) {
        # Search recursively in extracted directory if layout is flat
        $found = Get-ChildItem -Path $tmp -Filter $binName -Recurse | Select-Object -First 1
        if ($found) {
            $srcBin = $found.FullName
        } else {
            Die "could not find $binName in extracted archive"
        }
    }

    $targetBin = Join-Path $To $binName
    Copy-Item -Path $srcBin -Destination $targetBin -Force

    # Show version of installed binary
    $versionOutput = & $targetBin --version 2>&1
    if ($LASTEXITCODE -eq 0 -and $versionOutput) {
        SaySuccess "installed $versionOutput to $targetBin"
    } else {
        SaySuccess "installed Igel to $targetBin"
    }

    # Check and update PATH
    if ($os -eq "pc-windows-msvc" -and (-not $NoPathUpdate)) {
        $userPath = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)
        $pathEntries = ($userPath -split ';') | Where-Object { $_ -ne "" }

        $normalizedTo = (Resolve-Path $To).Path.TrimEnd('\')
        $alreadyInPath = $false
        foreach ($entry in $pathEntries) {
            if ($entry.TrimEnd('\') -eq $normalizedTo) {
                $alreadyInPath = $true
                break
            }
        }

        if (-not $alreadyInPath) {
            Say "adding $To to User PATH"
            $newUserPath = if ([string]::IsNullOrEmpty($userPath)) { $To } else { "$userPath;$To" }
            [Environment]::SetEnvironmentVariable("Path", $newUserPath, [EnvironmentVariableTarget]::User)
            $env:Path = "$env:Path;$To"
            SaySuccess "added $To to PATH (restart your terminal for changes to apply everywhere)"
        }
    } else {
        # Check if $To is in current $env:PATH
        $envPathEntries = ($env:Path -split [System.IO.Path]::PathSeparator)
        $alreadyInPath = $false
        foreach ($entry in $envPathEntries) {
            if ($entry.TrimEnd('/\') -eq $To.TrimEnd('/\')) {
                $alreadyInPath = $true
                break
            }
        }
        if (-not $alreadyInPath) {
            Say "add it to your PATH, e.g.:"
            Write-Host "    `$env:Path += `";$To`""
        }
    }
}
finally {
    if (Test-Path $tmp) {
        Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
