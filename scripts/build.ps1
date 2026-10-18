# Test and build exchange-catastrophic-migration.
#
# The checklist a developer runs before committing, and the one CI runs: one
# script, so the two cannot drift apart.
#
#   .\scripts\build.ps1            # test (the default)
#   .\scripts\build.ps1 build      # test, then build release
#   .\scripts\build.ps1 release    # from a clean tree: test, build, zip into dist\

param(
    [ValidateSet('test', 'build', 'release')]
    [string] $Task = 'test'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $root

# --- plumbing ---------------------------------------------------------------

$script:step = 0

function Step([string] $title) {
    $script:step++
    Write-Host ""
    Write-Host "[$script:step] $title" -ForegroundColor Cyan
}

function Fail([string] $message) {
    Write-Host ""
    Write-Host "FAILED: $message" -ForegroundColor Red
    exit 1
}

# Runs a native command and stops the script if it returns non-zero.
# PowerShell does not do this on its own.
function Run([string] $command, [string[]] $commandArgs) {
    & $command @commandArgs
    if ($LASTEXITCODE -ne 0) { Fail "$command $($commandArgs -join ' ') exited $LASTEXITCODE" }
}

# --- the checks -------------------------------------------------------------

function Invoke-Tests {
    Step "Formatting"
    Run 'cargo' @('fmt', '--all', '--check')
    Write-Host "    clean"

    Step "Clippy, warnings are errors"
    Run 'cargo' @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')

    # Doc comments are code too: a link to a private item or a stray [bracket]
    # is a warning rustdoc would print to whoever reads the API.
    Step "Doc comments build without warnings"
    $env:RUSTDOCFLAGS = '-D warnings'
    try { Run 'cargo' @('doc', '--workspace', '--no-deps', '--locked', '--quiet') } finally { Remove-Item Env:\RUSTDOCFLAGS -ErrorAction SilentlyContinue }

    Step "Tests"
    Run 'cargo' @('test', '--workspace', '--locked')

    Step "Documentation links resolve"
    # Sections too: a link to a renamed heading lands at the top of the page,
    # which looks fine and is not. GitHub's anchors: lower case, punctuation
    # dropped, spaces to hyphens, a repeated heading numbered.
    $anchorsOf = @{}
    function Get-Anchors([string]$path) {
        if (-not $anchorsOf.ContainsKey($path)) {
            $seen = @{}
            $anchors = @{}
            $inCode = $false
            foreach ($line in Get-Content $path) {
                if ($line -match '^```') { $inCode = -not $inCode; continue }
                if ($inCode -or $line -notmatch '^#{1,6}\s+(.*)$') { continue }
                $text = $Matches[1] -replace '\[([^\]]*)\]\([^)]*\)', '$1' -replace '[`*]', ''
                $slug = ($text.ToLowerInvariant() -replace '[^\w\- ]', '') -replace ' ', '-'
                $n = [int]$seen[$slug]
                $seen[$slug] = $n + 1
                $anchors[$(if ($n) { "$slug-$n" } else { $slug })] = $true
            }
            $anchorsOf[$path] = $anchors
        }
        $anchorsOf[$path]
    }
    $broken = @()
    # *.local.md files are never published, so they are not checked.
    $pages = @('README.md', 'CONTRIBUTING.md', 'AGENTS.md', 'CLAUDE.md', 'CHANGELOG.md' | ForEach-Object { Join-Path $root $_ }) +
             (Get-ChildItem (Join-Path $root '.claude\skills') -Recurse -Filter 'SKILL.md' -ErrorAction SilentlyContinue |
                  ForEach-Object { $_.FullName }) +
             (Get-ChildItem (Join-Path $root 'docs') -Recurse -Filter '*.md' |
                  ForEach-Object { $_.FullName })
    foreach ($page in $pages) {
        $dir = Split-Path -Parent $page
        foreach ($match in [regex]::Matches((Get-Content $page -Raw), '\]\(([^)]+)\)')) {
            $link = $match.Groups[1].Value
            if ($link -match '^https?:') { continue }
            $target, $anchor = $link -split '#', 2
            $path = if ($target) { Join-Path $dir $target } else { $page }
            if (-not (Test-Path $path)) {
                $broken += "$($page.Substring($root.Length + 1)) -> $link"
                continue
            }
            if (-not $anchor) { continue }
            if (Test-Path $path -PathType Container) { $path = Join-Path $path 'README.md' }
            if ($path -like '*.md' -and (Test-Path $path) -and -not (Get-Anchors $path).ContainsKey($anchor)) {
                $broken += "$($page.Substring($root.Length + 1)) -> $link (no such section)"
            }
        }
    }
    if ($broken) {
        $broken | ForEach-Object { Write-Host "    broken: $_" -ForegroundColor Red }
        Fail "$($broken.Count) broken documentation link(s)"
    }
    Write-Host "    $($pages.Count) pages, every link resolves"

    # Mailbox data in a public repository cannot be taken back. .gitignore
    # blocks the usual extensions; this catches a file added with --force or
    # under another name.
    Step "No mailbox data is tracked"
    $tracked = & git ls-files
    if ($LASTEXITCODE -ne 0) { Fail "git ls-files exited $LASTEXITCODE" }
    # Test fixtures are the one exception, and each must be listed by name in its
    # folder's README, which says where it comes from.
    function Test-Fixture([string] $file) {
        if ($file -notmatch '^crates/[^/]+/tests/fixtures/([^/]+)$') { return $false }
        $readme = Join-Path $root ($file -replace '[^/]+$', 'README.md')
        (Test-Path $readme) -and (Select-String -Path $readme -Pattern "``$($Matches[1])``" -SimpleMatch -Quiet)
    }
    $data = @($tracked | Where-Object { ($_ -match '\.(pst|ost|msg|eml|ics|vcf|sqlite3?|db)$' -and -not (Test-Fixture $_)) -or $_ -match '^(private|reports|backups)/' -or $_ -match '\.local(\.[^/]+)?$' })
    if ($data) {
        $data | ForEach-Object { Write-Host "    tracked: $_" -ForegroundColor Red }
        Fail "mailbox data or private notes are tracked by git"
    }
    Write-Host "    $(@($tracked).Count) tracked files, none of them mailbox data"
}

function Invoke-Build {
    Step "Release build"
    Run 'cargo' @('build', '--workspace', '--release', '--locked')
}

# A release ships what a commit holds, so that commit must hold everything.
function Assert-CleanTree {
    Step "The working tree is clean"
    $changes = & git -C $root status --porcelain
    if ($LASTEXITCODE -ne 0) { Fail "git status exited $LASTEXITCODE" }
    if ($changes) {
        $changes | ForEach-Object { Write-Host "    $_" -ForegroundColor Red }
        Fail "commit or stash first -- a release must name a commit that contains what it ships"
    }
    Write-Host "    clean"
}

# The workspace version, from Cargo.toml.
function Get-Version {
    $match = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"'
    if (-not $match) { Fail "no version in Cargo.toml" }
    $match.Matches[0].Groups[1].Value
}

# dist\ecm-<version>-windows-x64.zip: the binary, with the README, the change
# log and the licence beside it.
function Invoke-Release {
    $version = Get-Version
    Step "CHANGELOG.md has a dated section for $version"
    $heading = "^## \[$([regex]::Escape($version))\] - \d{4}-\d{2}-\d{2}"
    if (-not (Select-String -Path (Join-Path $root 'CHANGELOG.md') -Pattern $heading -Quiet)) {
        Fail "CHANGELOG.md has no dated section for $version -- a release commit carries ``## [$version] - YYYY-MM-DD``"
    }
    Write-Host "    found"

    Step "The binary reports $version"
    $reported = & (Join-Path $root 'target\release\ecm.exe') --version
    if ($reported -ne "ecm $version") { Fail "ecm --version says `"$reported`", not `"ecm $version`"" }
    Write-Host "    $reported"

    Step "Zip"
    $name = "ecm-$version-windows-x64"
    $dist = Join-Path $root 'dist'
    $stage = Join-Path $dist $name
    $zip = Join-Path $dist "$name.zip"
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    foreach ($file in 'target\release\ecm.exe', 'README.md', 'CHANGELOG.md', 'LICENSE') {
        Copy-Item (Join-Path $root $file) $stage
    }
    if (Test-Path $zip) { Remove-Item $zip }
    Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
    Write-Host "    dist\$name.zip  ($([math]::Round((Get-Item $zip).Length / 1KB)) KB)" -ForegroundColor Green
}

# --- go ---------------------------------------------------------------------

if ($Task -eq 'release') { Assert-CleanTree }
Invoke-Tests
if ($Task -in @('build', 'release')) { Invoke-Build }
if ($Task -eq 'release') { Invoke-Release }

Write-Host ""
Write-Host "$Task OK" -ForegroundColor Green
