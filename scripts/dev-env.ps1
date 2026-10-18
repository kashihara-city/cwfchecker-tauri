# ビルド・テストの各スクリプトが共有する環境設定。
# Cargoの出力先と、Tauri生成物（src-tauri/gen）をtemp/へ逃がすジャンクションの操作。
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot
# .cargo/config.tomlと同じ値。作業ディレクトリがリポジトリ外でも同じ場所へ出す。
$env:CARGO_TARGET_DIR = Join-Path $root 'temp\cargo-target'

# 同じソースからどの機械でも同じexeができるようにするrustcの設定。CIも同じものを使う。
# 環境変数RUSTFLAGSは.cargo/config.tomlのrustflagsを置き換えるため、静的CRTと/Breproも
# ここに繰り返す。--remap-path-prefixは、panic位置などに入るCargoレジストリと
# リポジトリの絶対パス（機械ごとに違う）を固定の文字列へ置き換える。
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$env:RUSTFLAGS = "-C target-feature=+crt-static -C link-arg=/Brepro " +
    "--remap-path-prefix=$root=. --remap-path-prefix=$cargoHome=/cargo"

# src-tauri/genをtemp/build/tauri-genへのジャンクションにする。
# tauri-buildはgen/schemasへの書き込み先を固定しているため、作業ツリーを汚さないよう
# 実体をtemp/へ置き、ビルド中だけリンクで見せる。
function Mount-TauriGeneratedDirectory([string]$projectRoot) {
    $link = Join-Path $projectRoot 'src-tauri\gen'
    $target = Join-Path $projectRoot 'temp\build\tauri-gen'
    if (Test-Path -LiteralPath $link) {
        $item = Get-Item -LiteralPath $link -Force
        if ($item.LinkType -ne 'Junction') {
            # スクリプトを通さずcargoを実行すると実フォルダーとして生成される。
            # 中身は生成物だけなので、削除してジャンクションに置き換える。
            Remove-Item -LiteralPath $link -Recurse -Force
        }
        else { [System.IO.Directory]::Delete($link) }
    }
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    New-Item -ItemType Junction -Path $link -Target $target | Out-Null
    return $link
}

# ジャンクションだけを外す。temp/build/tauri-genの中身は次回のために残す。
function Dismount-TauriGeneratedDirectory([string]$link) {
    if (Test-Path -LiteralPath $link) {
        $item = Get-Item -LiteralPath $link -Force
        if ($item.LinkType -ne 'Junction') { throw "Refusing to remove non-junction path: $link" }
        [System.IO.Directory]::Delete($link)
    }
}
