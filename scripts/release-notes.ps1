# Writes the release notes for a tag: the section CHANGELOG.md carries for that
# version, how to install, the commits since the previous tag, and the checksum
# of the zip. Run by the release workflow after scripts\build.ps1 release;
# nothing in it needs GitHub. A version without its section is refused: a release
# must have something to say.
#
#   .\scripts\release-notes.ps1 -Tag v0.1.0    writes dist\notes.md and dist\SHA256SUMS.txt
param(
    [Parameter(Mandatory)] [string] $Tag
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$dist = Join-Path $root 'dist'

if ($Tag -notmatch '^v(\d+\.\d+\.\d+)$') { throw "tag `"$Tag`" is not vX.Y.Z" }
$version = $Matches[1]

# The notes proper: the section for this version, heading excluded, up to the
# next one, without the link definitions Keep a Changelog keeps at the bottom.
$changelog = Get-Content (Join-Path $root 'CHANGELOG.md') -Raw
$pattern = "(?ms)^## \[$([regex]::Escape($version))\] - \d{4}-\d{2}-\d{2}[^\r\n]*\r?\n(.*?)(?=^## |\z)"
$section = [regex]::Match($changelog, $pattern)
if (-not $section.Success) { throw "CHANGELOG.md has no dated section for $version" }
$notes = (($section.Groups[1].Value -replace '\r\n', "`n") -split "`n" |
          Where-Object { $_ -notmatch '^\[[^\]]+\]: ' }) -join "`n"
$notes = $notes.Trim()

$zipName = "ecm-$version-windows-x64.zip"
$zip = Join-Path $dist $zipName
if (-not (Test-Path $zip)) { throw "$zip is missing; run scripts\build.ps1 release first" }

# Checksums, in the shape sha256sum reads back: with LF line ends, since
# Set-Content would end lines with CRLF on Windows, and sha256sum -c would then
# look for a file whose name ends with a carriage return.
$sums = @('{0}  {1}' -f (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower(), $zipName)
[IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), (($sums -join "`n") + "`n"), [Text.Encoding]::ASCII)

# The previous tag by version order, not by date.
$tags = @(& git -C $root tag --sort=-v:refname | Where-Object { $_ -ne $Tag -and $_ -match '^v\d+\.\d+\.\d+$' })
$previous = if ($tags.Count) { $tags[0] } else { $null }
$commits = if ($previous) {
    $log = @(& git -C $root log --format='- %s' "$previous..$Tag" --no-merges)
    "## For the curious`n`nThe commits since $previous, newest first:`n`n" + ($log -join "`n")
} else {
    "## For the curious`n`nThe first release: every commit is in it."
}

$lines = @(
    'Audits and repairs a mailbox after a botched migration: an Exchange one over EWS, or an Outlook.com one through Microsoft Graph, from a PST file or an Outlook.com mailbox. Windows x64.',
    '',
    '## Install',
    '',
    "Unzip **$zipName** anywhere and run ``ecm.exe``. The README inside says how to store the Exchange password in the Credential Manager and how to register the Microsoft Graph application Outlook.com needs.",
    '',
    '## What changed',
    '',
    $notes,
    '',
    $commits,
    '',
    '## Verifying',
    '',
    'GitHub attests that this repository''s release workflow built the zip from this tag: `gh attestation verify <file> --repo geeooff/exchange-catastrophic-migration`. Its SHA-256:',
    '',
    '```'
) + $sums + @(
    '```'
)
Set-Content -Path (Join-Path $dist 'notes.md') -Value $lines -Encoding utf8
Get-Content (Join-Path $dist 'notes.md')
