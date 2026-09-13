# Thin wrapper: the logic lives in the sibling devtools repo, parameterized by
# this repo's .devtools.conf.
#Requires -Version 7
param([Parameter(ValueFromRemainingArguments = $true)][string[]] $Rest)
$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..' '..')).Path
$DevTools = if ($env:DEVTOOLS_DIR) { $env:DEVTOOLS_DIR } else { Join-Path (Split-Path -Parent $RepoRoot) 'devtools' }
$SharedScript = Join-Path $DevTools 'ci' 'windows' 'remote.ps1'
if (-not (Test-Path $SharedScript)) {
    throw "devtools not found at $DevTools — git clone git@github.com:andrewtheguy/devtools.git there (or set DEVTOOLS_DIR)"
}
$env:DEVTOOLS_REPO_ROOT = $RepoRoot
# Only splat what was given: with no arguments `@Rest` passes one empty string, which
# the shared script's command ValidateSet refuses instead of defaulting to `ci`.
if ($Rest) { & $SharedScript @Rest } else { & $SharedScript }
# A script that ends without `exit` exits 0 whatever the last command did.
exit $LASTEXITCODE
