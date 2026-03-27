[CmdletBinding()]
param(
    [string]$Version = $env:NODUS_VERSION,
    [string]$InstallDir = $env:NODUS_INSTALL_DIR,
    [switch]$Verify,
    [switch]$Uninstall,
    [Alias("h")]
    [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoSlug = "nodus-rs/nodus"
$BinaryName = "nodus"
$ExecutableName = "nodus.exe"
$InstallMarkerName = "$BinaryName.install.json"
$script:TempDir = $null

function Show-Usage {
    @"
Install nodus from GitHub release assets.

Usage:
  ./install.ps1 [-Version <tag>] [-InstallDir <path>] [-Verify]
  ./install.ps1 -Uninstall [-InstallDir <path>]

Options:
  -Version <tag>      Install a specific release tag, for example v0.1.0.
  -InstallDir <path>  Install the binary into this directory.
  -Verify             Verify the downloaded archive with a release checksum when available.
  -Uninstall          Remove the installed binary from the install directory.
  -Help               Show this help text.

Environment:
  NODUS_VERSION       Same as -Version.
  NODUS_INSTALL_DIR   Same as -InstallDir.
"@
}

function Log([string]$Message) {
    Write-Host $Message
}

function Fail([string]$Message) {
    throw "error: $Message"
}

function Normalize-Version([string]$Value) {
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $Value
    }
    if ($Value.StartsWith("v")) {
        return $Value
    }
    return "v$Value"
}

function Show-PowerShell-Hint {
    $psVersion = $PSVersionTable.PSVersion
    $isLegacyDesktop = ($PSVersionTable.PSEdition -eq "Desktop") -or ($psVersion.Major -lt 7)
    if ($isLegacyDesktop) {
        Log "Hint: You are running Windows PowerShell $psVersion."
        Log "Hint: For best compatibility, install PowerShell 7+ and run:"
        Log '  pwsh -NoProfile -Command "irm https://nodus.elata.ai/install.ps1 | iex"'
    }
}

function Resolve-Architecture {
    $runtimeInfoType = [System.Runtime.InteropServices.RuntimeInformation]
    $bindingFlags = [System.Reflection.BindingFlags]::Public -bor [System.Reflection.BindingFlags]::Static
    $archProperty = $runtimeInfoType.GetProperty("OSArchitecture", $bindingFlags)
    if ($null -ne $archProperty) {
        $archValue = $archProperty.GetValue($null)
        if ($null -ne $archValue) {
            return $archValue.ToString().ToUpperInvariant()
        }
    }

    if ([Environment]::Is64BitOperatingSystem) {
        return "X64"
    }

    return "X86"
}

function Resolve-Target {
    $arch = Resolve-Architecture
    switch ($arch) {
        "X64" { return "x86_64-pc-windows-msvc" }
        "Arm64" { return "aarch64-pc-windows-msvc" }
        default { Fail "unsupported Windows architecture: $arch" }
    }
}

function Resolve-InstallDir([string]$RequestedInstallDir) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedInstallDir)) {
        return $RequestedInstallDir
    }

    $baseDir = $env:LOCALAPPDATA
    if ([string]::IsNullOrWhiteSpace($baseDir)) {
        $baseDir = $env:APPDATA
    }
    if ([string]::IsNullOrWhiteSpace($baseDir)) {
        Fail "failed to determine the default local application data path on Windows"
    }

    return Join-Path $baseDir "Programs\nodus\bin"
}

function Resolve-Version([string]$RequestedVersion) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedVersion)) {
        return $RequestedVersion
    }

    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$RepoSlug/releases/latest"
    if ([string]::IsNullOrWhiteSpace($release.tag_name)) {
        Fail "could not determine the latest release tag"
    }

    return $release.tag_name
}

function Download-File([string]$Url, [string]$OutputPath) {
    Invoke-WebRequest -Uri $Url -OutFile $OutputPath
}

function Get-Checksum([string]$Path) {
    return (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Verify-Checksum([string]$ArchivePath, [string]$ChecksumPath, [string]$AssetName) {
    $line = Get-Content -Path $ChecksumPath -TotalCount 1
    if ($line -notmatch '^([0-9A-Fa-f]+)') {
        Fail "checksum file did not contain a hash"
    }

    $expected = $Matches[1].ToLowerInvariant()
    $actual = Get-Checksum $ArchivePath
    if ($actual -ne $expected) {
        Fail "checksum verification failed for $AssetName"
    }
}

function Install-Binary([string]$ExtractedRoot, [string]$VersionedExtractedDir, [string]$DestinationDir) {
    $candidates = @(
        (Join-Path $VersionedExtractedDir $ExecutableName),
        (Join-Path $ExtractedRoot $ExecutableName)
    )

    $sourceBinary = $null
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $sourceBinary = $candidate
            break
        }
    }

    if ($null -eq $sourceBinary) {
        Fail "archive did not contain $ExecutableName"
    }

    New-Item -ItemType Directory -Force -Path $DestinationDir | Out-Null
    Copy-Item -LiteralPath $sourceBinary -Destination (Join-Path $DestinationDir $ExecutableName) -Force
}

function Install-MarkerPath([string]$DestinationDir) {
    return Join-Path $DestinationDir $InstallMarkerName
}

function Write-InstallMarker([string]$DestinationDir) {
    $marker = @{
        install_method = "github_release"
        repo_slug = $RepoSlug
        binary_name = $BinaryName
        binary_path = (Join-Path $DestinationDir $ExecutableName)
    }

    $marker | ConvertTo-Json | Set-Content -Path (Install-MarkerPath $DestinationDir) -Encoding Ascii
}

function Warn-If-NotOnPath([string]$DestinationDir) {
    $normalizedInstallDir = $DestinationDir.TrimEnd('\')
    $onPath = ($env:Path -split ';' | ForEach-Object { $_.Trim().TrimEnd('\') }) -contains $normalizedInstallDir
    if (-not $onPath) {
        Log "Installed to $(Join-Path $DestinationDir $ExecutableName)"
        Log "Add $DestinationDir to your PATH to run $BinaryName directly."
    }
}

function Remove-Install([string]$DestinationDir) {
    $installedBinary = Join-Path $DestinationDir $ExecutableName
    $markerPath = Install-MarkerPath $DestinationDir

    if (-not (Test-Path -LiteralPath $installedBinary) -and -not (Test-Path -LiteralPath $markerPath)) {
        Log "$BinaryName is not installed in $DestinationDir"
        return
    }

    if (Test-Path -LiteralPath $installedBinary) {
        Remove-Item -LiteralPath $installedBinary -Force
    }
    if (Test-Path -LiteralPath $markerPath) {
        Remove-Item -LiteralPath $markerPath -Force
    }

    Log "Removed $installedBinary"
}

try {
    if ($Help) {
        Show-Usage
        exit 0
    }

    Show-PowerShell-Hint
    $Version = Normalize-Version $Version
    $InstallDir = Resolve-InstallDir $InstallDir
    if ($Uninstall) {
        Remove-Install $InstallDir
        exit 0
    }

    $Target = Resolve-Target
    $Version = Resolve-Version $Version
    $AssetName = "$BinaryName-$Version-$Target.zip"
    $ChecksumName = "$AssetName.sha256"
    $AssetUrl = "https://github.com/$RepoSlug/releases/download/$Version/$AssetName"
    $ChecksumUrl = "https://github.com/$RepoSlug/releases/download/$Version/$ChecksumName"

    $script:TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Force -Path $script:TempDir | Out-Null

    $archivePath = Join-Path $script:TempDir $AssetName
    $checksumPath = Join-Path $script:TempDir $ChecksumName
    $extractedRoot = Join-Path $script:TempDir "extract"
    $extractedDir = Join-Path $extractedRoot "$BinaryName-$Version-$Target"

    Log "Downloading $AssetName"
    Download-File $AssetUrl $archivePath

    if ($Verify) {
        try {
            Download-File $ChecksumUrl $checksumPath
        }
        catch {
            Log "Checksum unavailable for $Version; continuing without verification."
        }
        if (Test-Path -LiteralPath $checksumPath) {
            Log "Verifying download"
            Verify-Checksum $archivePath $checksumPath $AssetName
        }
    }

    New-Item -ItemType Directory -Force -Path $extractedRoot | Out-Null
    Expand-Archive -Path $archivePath -DestinationPath $extractedRoot -Force
    Install-Binary $extractedRoot $extractedDir $InstallDir
    Write-InstallMarker $InstallDir
    Warn-If-NotOnPath $InstallDir
    Log "Installed $BinaryName $Version"
    Log "Run '$BinaryName --help' to get started."
}
finally {
    if ($null -ne $script:TempDir -and (Test-Path -LiteralPath $script:TempDir)) {
        Remove-Item -LiteralPath $script:TempDir -Recurse -Force
    }
}
