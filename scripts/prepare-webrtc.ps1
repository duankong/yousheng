# Returns only the directory expected by LK_CUSTOM_WEBRTC on the success stream.
# -Archive accepts an existing official zip. -PackageNotices also collects license
# texts from the locally installed npm packages and Cargo's locked Windows graph.
param([string]$Archive, [switch]$PackageNotices)
$ErrorActionPreference = 'Stop'
$tag = 'webrtc-a545de4'
$digest = '862403fcb248c7d8803487e4c22b8821c6130e89100cb9c69be95ef45ff46146'
$asset = 'webrtc-win-x64-release.zip'
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$cache = Join-Path $root 'src-tauri/target/webrtc-cache'
$resources = Join-Path $root 'src-tauri/resources'
$versionDir = Join-Path $cache $tag
$native = Join-Path $versionDir 'win-x64-release'
$marker = Join-Path $versionDir 'verified.sha256'
$license = Join-Path $native 'LICENSE.md'
$null = [IO.Directory]::CreateDirectory($versionDir)
$null = [IO.Directory]::CreateDirectory($resources)

# Concurrent desktop builds may request the same archive and resource files.
$lockPath = Join-Path $cache 'prepare.lock'
$deadline = [DateTime]::UtcNow.AddMinutes(10)
while ($true) {
    try {
        $lock = [IO.File]::Open($lockPath, 'OpenOrCreate', 'ReadWrite', 'None')
        break
    } catch [IO.IOException] {
        if ([DateTime]::UtcNow -ge $deadline) { throw }
        Start-Sleep -Milliseconds 400
    }
}

try {
    if ($Archive) {
        $source = (Resolve-Path -LiteralPath $Archive).Path
        if ((Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ne $digest) {
            throw "WebRTC archive digest mismatch for $tag."
        }
    }
    $complete = [IO.File]::Exists($marker) -and [IO.File]::ReadAllText($marker).Trim() -eq $digest -and
        [IO.File]::Exists($license) -and [IO.File]::Exists((Join-Path $native 'lib/webrtc.lib')) -and
        [IO.File]::Exists((Join-Path $native 'include/api/peer_connection_interface.h'))
    if (!$complete) {
        $cachedZip = Join-Path $cache "$tag-$asset"
        if (!$Archive) {
            if ([IO.File]::Exists($cachedZip) -and (Get-FileHash -LiteralPath $cachedZip -Algorithm SHA256).Hash -eq $digest) {
                $source = $cachedZip
            }
            if (!$source) {
                $partial = "$cachedZip.partial"
                Write-Host "Downloading LiveKit $tag Windows x64 WebRTC."
                Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/livekit/rust-sdks/releases/download/$tag/$asset" -OutFile $partial
                if ((Get-FileHash -LiteralPath $partial -Algorithm SHA256).Hash -ne $digest) {
                    throw "WebRTC download digest mismatch for $tag."
                }
                if ([IO.File]::Exists($cachedZip)) { [IO.File]::Replace($partial, $cachedZip, $null) }
                else { [IO.File]::Move($partial, $cachedZip) }
                $source = $cachedZip
            }
        }
        Add-Type -AssemblyName System.IO.Compression
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $file = [IO.File]::Open($source, 'Open', 'Read', 'Read')
        try {
            if ((Get-FileHash -InputStream $file -Algorithm SHA256).Hash -ne $digest) {
                throw "WebRTC archive digest mismatch for $tag."
            }
            $file.Position = 0
            $zip = [IO.Compression.ZipArchive]::new($file, [IO.Compression.ZipArchiveMode]::Read, $true)
            try {
                Write-Host "Extracting verified LiveKit $tag Windows x64 WebRTC."
                $prefix = $native + [IO.Path]::DirectorySeparatorChar
                foreach ($entry in $zip.Entries) {
                    if (!$entry.Name) { continue }
                    $path = [IO.Path]::GetFullPath((Join-Path $versionDir $entry.FullName))
                    if (!$path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
                        throw "Unexpected archive path $($entry.FullName)."
                    }
                    $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path))
                    [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $path, $true)
                }
            } finally { $zip.Dispose() }
        } finally { $file.Dispose() }
        foreach ($name in @('lib/webrtc.lib', 'include/api/peer_connection_interface.h', 'LICENSE.md')) {
            if (![IO.File]::Exists((Join-Path $native $name))) { throw "WebRTC release is missing $name." }
        }
        [IO.File]::WriteAllText($marker, $digest)
    }
    [IO.File]::Copy($license, (Join-Path $resources 'WebRTC-NOTICES.md'), $true)

    if ($PackageNotices) {
        # Node is already required by the desktop build. Only local package files
        # are read; Cargo refuses network access and changes to the lock file.
        @'
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const root = process.argv[2];
const ui = path.join(root, 'ui');
const output = path.join(root, 'src-tauri/resources/ThirdParty-NOTICES.txt');
const crates = JSON.parse(execFileSync('cargo', [
  'metadata', '--manifest-path', path.join(root, 'src-tauri/Cargo.toml'),
  '--format-version', '1', '--locked', '--offline', '--features', 'custom-protocol',
  '--filter-platform', 'x86_64-pc-windows-msvc'
], { encoding: 'utf8', maxBuffer: 128 * 1024 * 1024 }));
const ids = new Set(crates.resolve.nodes.map(n => n.id));
const packages = crates.packages.filter(p => ids.has(p.id) && p.source).map(p => ({
  label: 'Cargo ' + p.name + ' ' + p.version,
  license: p.license, extra: p.license_file,
  dir: path.dirname(p.manifest_path),
  source: p.repository || 'https://crates.io/crates/' + p.name + '/' + p.version
}));
const lock = JSON.parse(fs.readFileSync(path.join(ui, 'package-lock.json'), 'utf8'));
for (const [location, entry] of Object.entries(lock.packages)) {
  if (!location || entry.dev) continue;
  const dir = path.join(ui, location);
  if (entry.optional && !fs.existsSync(dir)) continue;
  const manifest = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
  if (manifest.version !== entry.version) throw Error('Run npm ci for ' + location);
  packages.push({
    label: 'npm ' + manifest.name + ' ' + manifest.version,
    license: manifest.license || entry.license, dir,
    source: 'https://www.npmjs.com/package/' + manifest.name + '/v/' + manifest.version
  });
}
const texts = new Map();
const heading = [
  'Third-party package notices',
  'Locked Windows Cargo dependency graph and installed npm production dependencies.',
  'Identical license texts are reproduced once and referenced by number.',
  ''
];
for (const pkg of packages.sort((a, b) => a.label.localeCompare(b.label, 'en'))) {
  const files = new Set();
  for (const ent of fs.readdirSync(pkg.dir, { withFileTypes: true })) {
    const file = path.join(pkg.dir, ent.name);
    if (ent.isFile() && /(^|[-._])(licen[sc]e|copying|notice|copyright|unlicense)([._-]|$)/i.test(ent.name)) files.add(file);
    if (ent.isDirectory() && /^(licen[sc]es?|notices)$/i.test(ent.name)) {
      for (const item of fs.readdirSync(file, { withFileTypes: true })) {
        if (item.isFile()) files.add(path.join(file, item.name));
      }
    }
  }
  if (pkg.extra) files.add(path.resolve(pkg.dir, pkg.extra));
  const refs = [];
  for (const file of [...files].sort()) {
    const raw = fs.readFileSync(file);
    const hash = crypto.createHash('sha256').update(raw).digest('hex');
    if (!texts.has(hash)) texts.set(hash, { id: texts.size + 1, raw });
    refs.push(path.basename(file) + ' [' + texts.get(hash).id + ']');
  }
  const expression = typeof pkg.license === 'string' ? pkg.license : pkg.license?.type;
  heading.push(pkg.label, '  Source: ' + pkg.source);
  if (expression) heading.push('  License: ' + expression);
  if (refs.length) heading.push('  Files: ' + refs.join(', '));
  heading.push('');
}
const parts = [Buffer.from(heading.join('\n'))];
for (const { id, raw } of texts.values()) {
  parts.push(Buffer.from('\n===== License text [' + id + '] =====\n\n'), raw, Buffer.from('\n'));
}
fs.writeFileSync(output + '.tmp', Buffer.concat(parts));
fs.renameSync(output + '.tmp', output);
'@ | & node - $root
        if ($LASTEXITCODE -ne 0) { throw 'Unable to generate local Rust and npm package notices.' }
    }
} finally { $lock.Dispose() }
Write-Output $native
