# CIと同じRust側の検査。フォーマット、Clippy、単体テストの順に流す。
. "$PSScriptRoot\..\dev-env.ps1"
$root = Split-Path (Split-Path $PSScriptRoot)
$manifest = Join-Path $root 'src-tauri\Cargo.toml'

# .cargo/config.tomlを拾わせるため、作業ディレクトリをリポジトリ内にする。
Push-Location $root
$generated = Mount-TauriGeneratedDirectory $root
try {
    cargo fmt --manifest-path $manifest --all -- --check
    if ($LASTEXITCODE -ne 0) { throw 'cargo fmt found formatting differences' }
    cargo clippy --locked --all-targets --manifest-path $manifest -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Clippy found something' }
    cargo test --locked --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw 'Rust tests failed' }
} finally {
    Dismount-TauriGeneratedDirectory $generated
    Pop-Location
}
