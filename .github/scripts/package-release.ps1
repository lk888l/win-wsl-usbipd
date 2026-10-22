param([string]$Tag)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Run from the repository root, on Windows x64, after cargo build --release --locked.
$metadataJson = cargo metadata --no-deps --format-version 1 --locked
if ($LASTEXITCODE -ne 0) { throw 'Unable to read Cargo metadata' }
$metadata = $metadataJson | ConvertFrom-Json
$package = $metadata.packages | Where-Object { $_.name -eq 'win-wsl-usbipd' }
$version = $package.version
if (-not $Tag) { $Tag = "v$version" }
if ($Tag -cne "v$version" -or $Tag -notmatch '^v\d+\.\d+\.\d+$') {
    throw "Release tag '$Tag' must match stable package version 'v$version'"
}

$binary = Join-Path $metadata.target_directory 'release/wsl-usb.exe'
$reportedVersion = & $binary --version
if ($LASTEXITCODE -ne 0 -or $reportedVersion -cne "wsl-usb $version") {
    throw "Unexpected executable version: $reportedVersion"
}
& $binary --help | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Executable help smoke test failed' }

# Catch accidental packaging of an ARM64/32-bit executable under an x64 name.
$bytes = [IO.File]::ReadAllBytes($binary)
$peOffset = [BitConverter]::ToInt32($bytes, 0x3c)
if ([BitConverter]::ToUInt16($bytes, $peOffset + 4) -ne 0x8664) {
    throw 'The release executable must target Windows x86_64'
}

$dist = Join-Path $metadata.target_directory 'dist'
New-Item -ItemType Directory -Path $dist -Force | Out-Null
$archiveName = "wsl-usb-$Tag-windows-x86_64.zip"
$archive = Join-Path $dist $archiveName
Compress-Archive -LiteralPath $binary, 'README.md', 'LICENSE', 'CHANGELOG.md' -DestinationPath $archive -Force

# Check the archive itself, including executable bytes, before producing its checksum.
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead($archive)
try {
    $expected = @('CHANGELOG.md', 'LICENSE', 'README.md', 'wsl-usb.exe')
    $actual = @($zip.Entries.FullName | Sort-Object)
    if (Compare-Object $expected $actual) { throw 'Unexpected release archive contents' }
    $entryStream = $zip.GetEntry('wsl-usb.exe').Open()
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $entryHash = [BitConverter]::ToString($sha256.ComputeHash($entryStream)).Replace('-', '')
        if ($entryHash -ne (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash) {
            throw 'Archived executable differs from the verified build'
        }
    } finally {
        $entryStream.Dispose()
        $sha256.Dispose()
    }
} finally {
    $zip.Dispose()
}

$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), "$hash  $archiveName`n")

$changelog = Get-Content -LiteralPath 'CHANGELOG.md' -Raw -Encoding UTF8
$heading = '(?m)^## v' + [regex]::Escape($version) + '(?=\s|$)[^\r\n]*\r?\n'
$section = [regex]::Split($changelog, $heading)
if ($section.Count -ne 2) { throw "Missing or duplicate changelog section for $version" }
$notes = ([regex]::Split($section[1], '(?m)^## '))[0].Trim()
if (-not $notes) { throw "Empty changelog section for $version" }
[IO.File]::WriteAllText((Join-Path $dist 'release-notes.md'), "$notes`n")
Write-Output "Verified $archiveName ($hash)"
