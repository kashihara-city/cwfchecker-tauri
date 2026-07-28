//! ウィンドウ、メニュー、タスクトレイ、定期更新の操作をまとめる。

use super::{
    auth_flow::begin_portlet_load, downloads::cleanup_directory, settings_snapshot, AppState,
    APP_TITLE, MAIN_LABEL, SETTINGS_LABEL,
};
use crate::{registry_support, version_policy};
use std::{
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};
use tauri::{
    menu::{Menu, MenuBuilder, SubmenuBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_global_shortcut::{ShortcutEvent, ShortcutState};

const APP_MENU_LABEL: &str = "⚙ メニュー　";
const RELOAD_MENU_LABEL: &str = "↻ 更新　";
const CLOSE_MENU_LABEL: &str = "× 閉じる　";

/// メイン画面があれば再読込して前面へ出し、未設定なら設定画面を開く。
fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_LABEL) {
        if let Some(state) = app.try_state::<Arc<AppState>>() {
            let _ = begin_portlet_load(&window, state.inner().clone());
        }
        let _ = window.show();
        let _ = window.set_focus();
    } else {
        let _ = open_settings(app);
    }
}

/// 設定画面を1枚だけ作成し、既にあればその画面を再利用する。
pub(super) fn open_settings(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(SETTINGS_LABEL) {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }
    let menu = build_window_menu(app, SETTINGS_LABEL).map_err(|error| error.to_string())?;
    WebviewWindowBuilder::new(app, SETTINGS_LABEL, WebviewUrl::App("settings.html".into()))
        .title("CreateWebFlowChecker — 設定")
        .inner_size(600.0, 900.0)
        .center()
        .menu(menu)
        .on_menu_event(|window, event| {
            handle_window_menu(window, event.id().as_ref());
        })
        .build()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// 短時間に複数メニューイベントが来ても設定画面を重複作成しない。
fn request_open_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    if state.settings_opening.swap(true, Ordering::AcqRel) {
        return;
    }

    let app = app.clone();
    // ウィンドウ生成完了を待つ間、メニューイベントの処理を占有しない。
    thread::spawn(move || {
        let _ = open_settings(&app);
        if let Some(state) = app.try_state::<Arc<AppState>>() {
            state.settings_opening.store(false, Ordering::Release);
        }
    });
}

/// 各ウィンドウに共通する設定・更新・終了メニューを作る。
pub(super) fn build_window_menu(
    app: &AppHandle,
    window_label: &str,
) -> tauri::Result<Menu<tauri::Wry>> {
    let app_menu = SubmenuBuilder::new(app, APP_MENU_LABEL)
        .text(format!("settings:{window_label}"), "設定画面")
        .separator()
        .text(format!("quit:{window_label}"), "アプリ終了")
        .build()?;
    let mut menu = MenuBuilder::new(app)
        .item(&app_menu)
        .text(format!("reload:{window_label}"), RELOAD_MENU_LABEL)
        .text(format!("close:{window_label}"), CLOSE_MENU_LABEL);
    if version_policy::read_status().is_ok_and(|status| status.update_available) {
        menu = menu.text(
            format!("update-available:{window_label}"),
            "⬆ アップデートあり",
        );
    }
    menu.build()
}

/// タスクトレイのアイコンと、表示・終了メニューを作る。
pub(super) fn create_tray(app: &AppHandle) -> tauri::Result<()> {
    let tray_menu = MenuBuilder::new(app)
        .text("show", "表示")
        .separator()
        .text("quit", "電子決裁確認アプリ終了")
        .build()?;
    let mut tray = TrayIconBuilder::with_id("main-tray")
        .menu(&tray_menu)
        .show_menu_on_left_click(false)
        .tooltip(APP_TITLE);
    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }
    tray.on_tray_icon_event(|tray, event| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            show_main(tray.app_handle());
        }
    })
    .on_menu_event(|app, event| {
        handle_tray_menu(app, event.id().as_ref());
    })
    .build(app)?;
    Ok(())
}

/// 設定された間隔で、操作中でないメイン画面を更新する。
pub(super) fn start_timer(app: AppHandle, state: Arc<AppState>) {
    // 専用スレッドは最小15分眠り、案件画面や設定画面を操作中でない場合だけ更新する。
    // UI操作そのものはTauriのスレッドセーフなAppHandle経由で依頼する。
    thread::spawn(move || loop {
        let minutes = settings_snapshot(&state)
            .map(|settings| settings.interval_minutes)
            .unwrap_or(15)
            .max(15);
        thread::sleep(Duration::from_secs(minutes as u64 * 60));
        if state.quitting.load(Ordering::Relaxed)
            || state.decisions.load(Ordering::Relaxed) > 0
            || app.get_webview_window(SETTINGS_LABEL).is_some()
        {
            continue;
        }
        if let Some(window) = app.get_webview_window(MAIN_LABEL) {
            let _ = begin_portlet_load(&window, state.clone());
        }
    });
}

/// ウィンドウ名を含むメニューIDを検査してから、対象画面を操作する。
pub(super) fn handle_window_menu(window: &tauri::Window, id: &str) {
    // メニューIDに対象ウィンドウ名を含め、別ウィンドウの操作を取り違えない。
    let Some((action, target_label)) = id.split_once(':') else {
        return;
    };
    if target_label != window.label() {
        return;
    }

    let app = window.app_handle();
    match action {
        "settings" => {
            request_open_settings(app);
        }
        "reload" => {
            if let Some(webview) = app.get_webview_window(window.label()) {
                if window.label() == MAIN_LABEL {
                    if let Some(state) = app.try_state::<Arc<AppState>>() {
                        let _ = begin_portlet_load(&webview, state.inner().clone());
                    }
                } else {
                    let _ = webview.eval("window.location.reload()");
                }
            }
        }
        "close" => {
            let _ = window.close();
        }
        "quit" => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                state.quitting.store(true, Ordering::Relaxed);
            }
            app.exit(0);
        }
        "update-available" => {
            registry_support::show_update_available();
        }
        _ => {}
    }
}

fn handle_tray_menu(app: &AppHandle, id: &str) {
    match id {
        "show" => show_main(app),
        "quit" => {
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                state.quitting.store(true, Ordering::Relaxed);
            }
            app.exit(0);
        }
        _ => {}
    }
}

fn only_main_window<S: AsRef<str>>(labels: impl IntoIterator<Item = S>) -> bool {
    labels.into_iter().all(|label| label.as_ref() == MAIN_LABEL)
}

/// 閉じるボタンを「常駐」に置き換え、案件画面終了時には一時ファイルを片付ける。
pub fn handle_window_event(window: &tauri::Window, event: &WindowEvent) {
    let app = window.app_handle();
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    if window.label() == MAIN_LABEL {
        if let WindowEvent::CloseRequested { api, .. } = event {
            if !state.quitting.load(Ordering::Relaxed) {
                api.prevent_close();
                let _ = window.hide();
            }
        }
    } else if window.label().starts_with("decision-") && matches!(event, WindowEvent::Destroyed) {
        // saturating_sub相当で、予期しない重複イベントでもusizeの最大値へ
        // アンダーフローしないようにする。
        let previous = state
            .decisions
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_sub(1))
            })
            .unwrap_or(0);
        if previous <= 1 {
            if let Err(error) = cleanup_directory(&state.download_dir) {
                eprintln!("ダウンロードフォルダーを空にできませんでした: {error}");
            }
            show_main(app);
        }
    }
}

/// グローバルショートカットが押されたとき、メイン画面の表示状態を切り替える。
pub fn handle_shortcut(
    app: &AppHandle,
    _shortcut: &tauri_plugin_global_shortcut::Shortcut,
    event: ShortcutEvent,
) {
    if event.state == ShortcutState::Pressed {
        if let Some(main) = app.get_webview_window(MAIN_LABEL) {
            if main.is_visible().unwrap_or(false) && only_main_window(app.webview_windows().keys())
            {
                let _ = main.hide();
            } else {
                show_main(app);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::only_main_window;

    #[test]
    fn allows_hiding_when_only_the_main_window_exists() {
        assert!(only_main_window(["main"]));
    }

    #[test]
    fn prevents_hiding_when_any_other_window_exists() {
        assert!(!only_main_window(["main", "settings"]));
        assert!(!only_main_window(["main", "decision-1"]));
        assert!(!only_main_window(["main", "decision-child-1"]));
    }
}
