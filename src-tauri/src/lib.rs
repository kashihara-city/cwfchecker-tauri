//! CreateWebFlowCheckerのライブラリ側エントリーポイント。
//!
//! 小さな`main.rs`からこの`run`関数を呼び、Tauriプラグインとアプリ固有の
//! コールバックを組み立てる。実際の画面処理は`app`モジュールへ分離している。

mod app;
mod credentials;
mod migration;
mod registry_support;
mod settings;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Windows通知はAUMIDの登録がないとAPI上は成功しても表示されないことがある。
    // ユーザー単位の登録なので管理者権限は不要。失敗を知らせた後も本体は起動する。
    if let Err(error) = registry_support::ensure_notification_registration() {
        registry_support::show_registry_error(
            "Windows通知情報の登録",
            registry_support::NOTIFICATION_REGISTRY_DISPLAY_PATH,
            &error,
        );
    }

    let result = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        // 2個目のプロセスは作らず、既存のメイン画面を前面へ戻す。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
                let _ = main.set_focus();
            }
        }))
        .plugin(
            // 登録するキー自体は保存済み設定からsetup内で読み取る。
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(app::handle_shortcut)
                .build(),
        )
        .setup(app::setup)
        .on_window_event(app::handle_window_event)
        .invoke_handler(tauri::generate_handler![
            app::get_settings,
            app::save_settings
        ])
        .run(tauri::generate_context!());
    if let Err(error) = result {
        // release版はコンソールを持たないため、panic文字列ではなく画面へ表示する。
        registry_support::show_startup_error(&error);
    }
}
