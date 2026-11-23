$ErrorActionPreference = 'Stop'
$version = '1.13.11'
$sha256 = '5d071ea43339f0140d24a66f0b2ff8a7bf6c4d0dceb19b30abca835670213667'
$dest = Join-Path $PSScriptRoot '../src-tauri/resources'
$null = New-Item -ItemType Directory -Force -Path $dest
$archive = Join-Path $dest "livekit_$version.zip"
if (!(Test-Path -LiteralPath $archive) -or (Get-FileHash -LiteralPath $archive).Hash -ne $sha256) {
    Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/livekit/livekit/releases/download/v$version/livekit_${version}_windows_amd64.zip" -OutFile $archive
}
if ((Get-FileHash -LiteralPath $archive).Hash -ne $sha256) { throw 'LiveKit release digest mismatch' }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead((Resolve-Path $archive))
try {
    $out = @{ 'livekit-server.exe' = 'livekit-server.exe'; 'LICENSE' = 'livekit-LICENSE.txt' }
    foreach ($name in $out.Keys) {
        $entry = $zip.Entries | Where-Object { $_.Name -eq $name } | Select-Object -First 1
        if (!$entry) { throw "Missing $name in official release" }
        $target = Join-Path $dest $out[$name]
        if (Test-Path -LiteralPath $target) {
            $stream = $entry.Open()
            try { $expected = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash }
            finally { $stream.Dispose() }
            if ((Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -eq $expected) { continue }
        }
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry,$target,$true)
    }
} finally { $zip.Dispose() }
Write-Output "LiveKit $version ready"
