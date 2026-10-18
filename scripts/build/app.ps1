# Tauriアプリをリリースビルドし、temp/release/CreateWebFlowChecker.exeへ置く。
param([switch]$Clean)
. "$PSScriptRoot\..\dev-env.ps1"
$root = Split-Path (Split-Path $PSScriptRoot)
$manifest = Join-Path $root 'src-tauri\Cargo.toml'
$release = Join-Path $root 'temp\release'
$exeName = 'CreateWebFlowChecker.exe'

# temp/release/から起動中のコピーがあると、置き換え時にファイルがロックされる。
function Stop-ReleaseCopies {
    foreach ($running in @(Get-CimInstance Win32_Process -Filter "Name = '$exeName'" -ErrorAction SilentlyContinue |
            Where-Object { $_.ExecutablePath -like "$release\*" })) {
        Write-Host "Stopping $($running.Name) ($($running.ProcessId)) before replacing the executable"
        Stop-Process -Id $running.ProcessId -Force -ErrorAction SilentlyContinue
        try { (Get-Process -Id $running.ProcessId -ErrorAction Stop).WaitForExit(5000) | Out-Null } catch {}
    }
}

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
Copy-Item -LiteralPath $built -Destination $placed -Force
$hash = (Get-FileHash -LiteralPath $placed -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "Built $placed"
Write-Host "sha256: $hash"
