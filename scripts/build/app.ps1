# Tauriアプリをリリースビルドし、release/CreateWebFlowChecker.exeへ置く。
param([switch]$Clean)
. "$PSScriptRoot\..\dev-env.ps1"
$root = Split-Path (Split-Path $PSScriptRoot)
$manifest = Join-Path $root 'src-tauri\Cargo.toml'
$release = Join-Path $root 'release'
$exeName = 'CreateWebFlowChecker.exe'

# release/から起動中のコピーがあると、置き換え時にファイルがロックされる。
function Stop-ReleaseCopies {
    foreach ($running in @(Get-CimInstance Win32_Process -Filter "Name = '$exeName'" -ErrorAction SilentlyContinue |
            Where-Object { $_.ExecutablePath -like "$release\*" })) {
        Write-Host "Stopping $($running.Name) ($($running.ProcessId)) before replacing the executable"
        Stop-Process -Id $running.ProcessId -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $running.ProcessId -Timeout 10 -ErrorAction SilentlyContinue
    }
}

# コンパイル前に、依存の公開後日数とcargo auditを通す（CIと同じ順序）。不合格なら進まない。
& "$PSScriptRoot\..\deps\verify.ps1"

# .cargo/config.tomlのrustflags（静的CRT）を効かせるため、作業ディレクトリをリポジトリ内にする。
Push-Location $root
$generated = Mount-TauriGeneratedDirectory $root
try {
    if ($Clean) {
        # releaseプロファイルの生成物をすべて捨てて作り直す。
        cargo clean --release --manifest-path $manifest
        if ($LASTEXITCODE -ne 0) { throw 'cargo clean failed' }
    }
    cargo build --release --locked --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw 'Tauri build failed' }
} finally {
    Dismount-TauriGeneratedDirectory $generated
    Pop-Location
}

New-Item -ItemType Directory -Force -Path $release | Out-Null
Stop-ReleaseCopies
# exeだけを置き換える。初回起動時にexe横へ書き出される通知アイコンはそのまま残す。
$built = Join-Path $env:CARGO_TARGET_DIR "release\$exeName"
$placed = Join-Path $release $exeName
# 停止直後はexeのハンドルがまだ閉じていないことがあるので、短く待ちながら数回試す。
$copied = $false
for ($attempt = 1; $attempt -le 10 -and -not $copied; $attempt++) {
    try { Copy-Item -LiteralPath $built -Destination $placed -Force -ErrorAction Stop; $copied = $true }
    catch { if ($attempt -eq 10) { throw }; Start-Sleep -Milliseconds 500 }
}
# 上書きでも消して置き直しても、NTFSは同名ファイルの古い作成日時を引き継ぐため、
# エクスプローラーの「日付」欄が何か月も前に見える。作成日時を更新日時に揃える。
$item = Get-Item -LiteralPath $placed
$item.CreationTime = $item.LastWriteTime
$hash = (Get-FileHash -LiteralPath $placed -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "Built $placed"
Write-Host "sha256: $hash"
