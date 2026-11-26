param(
    [Parameter(Mandatory)][string]$Revision,
    [Parameter(Mandatory)][string]$Installer,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [string]$Notes = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$commit = git -C $root rev-parse --verify --end-of-options "${Revision}^{commit}"
if ($LASTEXITCODE) { throw "Cannot resolve release revision: $Revision" }
$config = git -C $root show "${commit}:src-tauri/tauri.conf.json"
if ($LASTEXITCODE) { throw "Cannot read Tauri configuration at $commit" }
$version = ($config -join "`n" | ConvertFrom-Json).version
if ($version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
    throw "Invalid release version at ${commit}: $version"
}
if ($Revision -match '^v\d' -and $Revision -cne "v$version") {
    throw "Tag $Revision does not match version $version"
}

$bundle = Get-Item -LiteralPath $Installer
if ($bundle.Name -notlike '*-setup.exe' -or $bundle.VersionInfo.ProductVersion -ne $version) {
    throw "Expected the Windows NSIS installer for version $version"
}
$signatureFile = Get-Item -LiteralPath "$($bundle.FullName).sig"
if ($signatureFile.LastWriteTimeUtc -lt $bundle.LastWriteTimeUtc) {
    throw 'The signature predates this installer. Rebuild with -SignedRelease.'
}
$signature = [IO.File]::ReadAllText($signatureFile.FullName).Trim()
if (!$signature) { throw 'Empty updater signature' }

$destination = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory).TrimEnd('\', '/')
if (Test-Path -LiteralPath $destination) { throw "Choose a new output directory: $destination" }
$parent = [IO.Path]::GetDirectoryName($destination)
$staging = Join-Path $parent ('.yousheng-release-' + [Guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($staging)
try {
    $installerName = 'yousheng-windows-x64-setup.exe'
    $releasedExe = Join-Path $staging $installerName
    Copy-Item -LiteralPath $bundle.FullName -Destination $releasedExe
    Compress-Archive -LiteralPath $releasedExe -DestinationPath (Join-Path $staging "yousheng-$version-windows-x64.zip")
    $sourceZip = Join-Path $staging "yousheng-$version-source.zip"
    git -C $root archive --format=zip "--prefix=yousheng-$version/" "--output=$sourceZip" $commit
    if ($LASTEXITCODE) { throw "Failed to archive source at $commit" }

    # Older clients display notes verbatim. Keep section breaks, bullets and filenames readable.
    $plainNotes = $Notes -replace '(?m)^ {0,3}#{1,6}[ \t]+', ''
    $plainNotes = $plainNotes -replace '(?m)^([ \t]*)[-+*][ \t]+', ('$1' + [char]0x2022 + ' ')
    $plainNotes = $plainNotes -replace '(?<!`)`([^`\r\n]+)`(?!`)', '$1'
    $manifest = [ordered]@{
        version = $version
        notes = $plainNotes
        notesMarkdown = $Notes
        pub_date = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
        platforms = @{
            'windows-x86_64' = @{
                url = "https://github.com/duankong/yousheng/releases/download/v$version/$installerName"
                signature = $signature
            }
        }
    } | ConvertTo-Json -Depth 4
    [IO.File]::WriteAllText((Join-Path $staging 'latest.json'), $manifest, [Text.UTF8Encoding]::new($false))
    [IO.Directory]::Move($staging, $destination)
} finally {
    if ([IO.Directory]::Exists($staging)) {
        foreach ($file in [IO.Directory]::GetFiles($staging)) { [IO.File]::Delete($file) }
        [IO.Directory]::Delete($staging)
    }
}
Get-ChildItem -LiteralPath $destination -File | Get-FileHash -Algorithm SHA256
