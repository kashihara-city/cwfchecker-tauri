# コンパイル前の依存検査。app.ps1とtest/rust.ps1がcargoを動かす前に呼ぶ。
# Cargo.lockのWindows向け依存が公開から一定日数経っていること（CIと同じ検査）と、
# cargo auditに脆弱性がないことを確認し、どちらかに通らなければ例外で止める。
# --lockedはlockの変更を禁じるだけで鮮度も監査も保証しないため、ここで揃える。
param(
    [int] $MinimumAgeDays = 7,
    [string] $TargetTriple = 'x86_64-pc-windows-msvc'
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$root = Split-Path (Split-Path $PSScriptRoot)
$manifest = Join-Path $root 'src-tauri\Cargo.toml'
$lock = Join-Path $root 'src-tauri\Cargo.lock'

# 1. クレートの取得と索引キャッシュの更新。fetchはbuild.rsやproc-macroを実行しない。
cargo fetch --locked --manifest-path $manifest
if ($LASTEXITCODE -ne 0) { throw 'cargo fetch --locked failed: Cargo.lock does not match Cargo.toml, or crates.io is unreachable' }

# 2. 公開後日数。-Resolveは付けず、7日未満の版があれば止める（戻すのは利用者の操作）。
& (Join-Path $PSScriptRoot 'resolve-cargo-cooldown.ps1') `
    -ManifestPath $manifest -MinimumAgeDays $MinimumAgeDays -TargetTriple $TargetTriple

# 3. 監査。脆弱性データベースを取得してから検査する。メンテナンス終了などの警告は
#    通し、脆弱性があれば止める（CIと同じ基準）。
$auditOutput = @(cargo audit --file $lock 2>&1)
$auditExit = $LASTEXITCODE
$auditOutput | ForEach-Object { "$_" }
if ($auditExit -ne 0) {
    # データベースの取得に失敗しただけ（オフライン等）なら、手元のDBで検査し直す。
    $fetchFailed = ($auditOutput -join "`n") -match "couldn't fetch advisory database|failed to fetch|Failed to fetch"
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    if (-not ($fetchFailed -and (Test-Path (Join-Path $cargoHome 'advisory-db')))) {
        throw 'cargo audit reported vulnerabilities; do not build until Cargo.lock is fixed'
    }
    Write-Warning 'advisory database could not be fetched; auditing with the local copy'
    cargo audit --no-fetch --stale --file $lock
    if ($LASTEXITCODE -ne 0) { throw 'cargo audit reported vulnerabilities; do not build until Cargo.lock is fixed' }
}
