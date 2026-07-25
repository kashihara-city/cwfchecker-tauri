// release版ではWindowsのコンソール画面を表示しない。debug版では標準エラーを
// 確認できるよう、通常のコンソール付きアプリとして起動する。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Tauriの構築処理はテスト可能なライブラリ側へ置き、mainは入口だけにする。
    cwfchecker_tauri_no_npm_lib::run()
}
