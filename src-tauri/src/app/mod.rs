//! Tauriアプリ全体の状態を保持し、各機能モジュールを組み立てる。
//!
//! 個別の保存処理は`settings`、`credentials`、`migration`へ分け、このファイルは
//! それらを「いつ呼ぶか」と、WebViewから来た情報をどう画面へ反映するかに集中する。

mod auth_flow;
mod downloads;
mod main_window;
mod ui;

use self::{
    auth_flow::{AuthenticationState, PortletLoadState},
    downloads::prepare_cache,
    main_window::create_main_window,
    ui::{create_tray, open_settings, start_timer},
};
use crate::{
    credentials, migration, registry_support,
    settings::{self, Settings},
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use tauri_plugin_notification::NotificationExt;
use url::Url;

pub use ui::{handle_shortcut, handle_window_event};

const MAIN_LABEL: &str = "main";
const SETTINGS_LABEL: &str = "settings";
const APP_TITLE: &str = "CreateWebFlowChecker";
const NOTIFICATION_TITLE: &str = "電子決裁確認アプリ";

/// OS差を吸収するTauriプラグインを通してWindows通知を表示する。
fn show_windows_notification(app: &AppHandle, title: &str, body: &str) -> Result<(), String> {
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|error| error.to_string())
}

/// 複数のコールバックやタイマースレッドから共有するアプリの状態。
///
/// `RwLock`は設定の読み書きを、`Mutex`は複数の値をまとめて更新する処理を、
/// `Atomic*`は単純なフラグや件数を、それぞれスレッド間で安全に共有する。
pub struct AppState {
    settings: RwLock<Settings>,
    /// `settings`が初期値だけでなく、レジストリへ完成保存済みかを表す。
    /// 保存失敗時に「旧設定を書き戻す」か「途中生成キーを消す」かの判断に使う。
    settings_persisted: AtomicBool,
    cache_root: PathBuf,
    download_dir: PathBuf,
    quitting: AtomicBool,
    settings_opening: AtomicBool,
    /// 複数スレッドからのnavigate要求順と世代番号の更新順を一致させる。
    ///
    /// `window.navigate()`はコールバックを呼ぶ可能性があるため、コールバックも使う
    /// `portlet_load`とは別のMutexにする。1個にすると同じスレッドで二重取得して
    /// デッドロックする恐れがある。
    portlet_navigation: Mutex<()>,
    /// 非同期のページ読込みが前後しても、最新の要求だけがPOSTを投入する。
    portlet_load: Mutex<PortletLoadState>,
    /// SAML利用時の外部HTTPS遷移を、最初の遷移から一定時間だけ許可する。
    authentication: Mutex<AuthenticationState>,
    decisions: AtomicUsize,
    decision_counter: AtomicUsize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// 設定画面からTauriコマンドへ渡される入力。PWは保存済み設定には含めない。
pub struct SettingsInput {
    id: String,
    password: String,
    ad_server: String,
    cwf_address: String,
    interval_minutes: u32,
    notify_by_bar: bool,
    shortcut: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
/// 設定画面へ返す表示用データ。パスワードそのものはWebViewへ渡さない。
pub struct SettingsView {
    id: String,
    ad_server: String,
    cwf_address: String,
    interval_minutes: u32,
    notify_by_bar: bool,
    shortcut: String,
    has_password: bool,
}

fn lock_error() -> String {
    "アプリの共有状態のロックに失敗しました。".to_owned()
}

fn settings_snapshot(state: &AppState) -> Result<Settings, String> {
    state
        .settings
        .read()
        .map(|value| value.clone())
        .map_err(|_| lock_error())
}

/// 認証情報を含まないポートレットの送信先URLを作る。
fn build_portlet_endpoint(settings: &Settings) -> Result<Url, String> {
    let mut url = Url::parse(&settings.cwf_address)
        .map_err(|_| "CWFAddressが正しいURLではありません。".to_owned())?;
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn configured_url(state: &AppState) -> Result<Option<Url>, String> {
    let settings = settings_snapshot(state)?;
    if settings.cwf_address.is_empty() || settings.id.is_empty() {
        return Ok(None);
    }
    let Some(credential) =
        credentials::read(credentials::TARGET).map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    if credential.username != settings.id {
        // 別IDの資格情報を誤って使用しない。設定画面でPWを再入力すれば修復できる。
        return Ok(None);
    }
    build_portlet_endpoint(&settings).map(Some)
}

fn configured_origin(state: &AppState) -> Result<Option<String>, String> {
    let settings = settings_snapshot(state)?;
    if settings.cwf_address.is_empty() {
        return Ok(None);
    }
    let url = Url::parse(&settings.cwf_address)
        .map_err(|_| "CWFAddressが正しいURLではありません。".to_owned())?;
    Ok(Some(url.origin().ascii_serialization()))
}

/// URLが設定されたCreate!Webフローと同じオリジンかを確認する。
///
/// オリジンはスキーム、ホスト、ポートの組であり、パスだけの比較より厳密である。
fn has_allowed_origin(url: &Url, allowed_origin: &str) -> bool {
    matches!(url.scheme(), "http" | "https") && url.origin().ascii_serialization() == allowed_origin
}

#[tauri::command]
/// 設定画面だけに、パスワードを除いた現在値を返すTauriコマンド。
pub fn get_settings(
    window: WebviewWindow,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<SettingsView, String> {
    if window.label() != SETTINGS_LABEL {
        return Err("許可されていないウィンドウです。".to_owned());
    }
    let settings = settings_snapshot(&state)?;
    let has_password = credentials::read(credentials::TARGET)
        .map_err(|error| error.to_string())?
        .is_some_and(|credential| credential.username == settings.id);
    Ok(SettingsView {
        id: settings.id,
        ad_server: settings.ad_server,
        cwf_address: settings.cwf_address,
        interval_minutes: settings.interval_minutes,
        notify_by_bar: settings.notify_by_bar,
        shortcut: settings.shortcut,
        has_password,
    })
}

#[tauri::command]
/// 設定画面の入力を検証し、資格情報とレジストリへ保存するTauriコマンド。
pub fn save_settings(
    app: AppHandle,
    window: WebviewWindow,
    state: tauri::State<'_, Arc<AppState>>,
    input: SettingsInput,
) -> Result<(), String> {
    if window.label() != SETTINGS_LABEL {
        return Err("許可されていないウィンドウです。".to_owned());
    }
    let settings = Settings {
        id: input.id,
        ad_server: input.ad_server,
        cwf_address: input.cwf_address,
        interval_minutes: input.interval_minutes,
        notify_by_bar: input.notify_by_bar,
        shortcut: input.shortcut,
    }
    .normalize()?;
    if settings.id.is_empty() {
        return Err("IDを入力してください。".to_owned());
    }
    // レジストリ保存が途中で失敗したときに戻せるよう、変更前を先に保存する。
    // `previous_registry=false`なら、変更前は「設定キーなし」だったことを表す。
    let previous_settings = settings_snapshot(&state)?;
    let previous_registry = state.settings_persisted.load(Ordering::Acquire);

    // 保存済みPWはWebViewへ返さず、空欄で送られた場合だけRust側で引き継ぐ。
    let existing = credentials::read(credentials::TARGET).map_err(|error| error.to_string())?;
    let password = if input.password.is_empty() {
        let existing = existing
            .as_ref()
            .ok_or_else(|| "PWを入力してください。".to_owned())?;
        if existing.username != settings.id {
            return Err("IDを変更する場合はPWも入力してください。".to_owned());
        }
        existing.password.clone()
    } else {
        input.password
    };
    credentials::write(credentials::TARGET, &settings.id, &password)
        .map_err(|error| error.to_string())?;
    // Windows APIが成功を返しても、実際に同じ内容を読めることまで確認する。
    let verified = credentials::read(credentials::TARGET)
        .map_err(|error| error.to_string())?
        .is_some_and(|value| value.username == settings.id && value.password == password);
    if !verified {
        let _ = credentials::restore(existing.as_ref());
        return Err("Windows資格情報の保存後検証に失敗しました。".to_owned());
    }

    let registry_result = (|| -> Result<(), String> {
        // 小さなクロージャーにすることで、途中の`?`をまとめて1個のResultとして扱う。
        registry_support::report_io(
            "アプリ設定の保存",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            settings::write(&settings),
        )
        .map_err(|error| error.to_string())?;
        let verified = registry_support::report_io(
            "アプリ設定の保存後確認",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            settings::verify(&settings),
        )
        .map_err(|error| error.to_string())?;
        registry_support::require_verified(
            "アプリ設定の保存後確認",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            verified,
        )
        .map_err(|error| error.to_string())
    })();
    if let Err(error) = registry_result {
        // 資格情報とレジストリは別の保存先なので、片方の復元に失敗しても
        // もう片方の復元は必ず試す。これが完全なトランザクションの代わりになる。
        let registry_rollback = registry_support::report_io(
            "アプリ設定の復元",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            settings::restore(previous_registry.then_some(&previous_settings)),
        );
        let credential_rollback = credentials::restore(existing.as_ref());
        let mut message = error;
        if let Err(rollback_error) = registry_rollback {
            message.push_str(&format!(
                "\nアプリ設定を変更前の状態へ戻せませんでした: {rollback_error}"
            ));
        }
        if let Err(rollback_error) = credential_rollback {
            message.push_str(&format!(
                "\nWindows資格情報を変更前の状態へ戻せませんでした: {rollback_error}"
            ));
        }
        return Err(message);
    }
    // 両方の永続化に成功してから、実行中アプリが参照する値を切り替える。
    *state.settings.write().map_err(|_| lock_error())? = settings;
    state.settings_persisted.store(true, Ordering::Release);

    thread::spawn(move || {
        thread::sleep(Duration::from_millis(500));
        app.restart();
    });
    Ok(())
}

/// Tauri起動時に一度だけ呼ばれ、共有状態と各UI部品を準備する。
pub fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let cache_root = prepare_cache()?;
    let download_dir = app.path().document_dir()?.join("cwf_downloads");
    // `persisted`も受け取り、初期値だけの状態とレジストリ保存済みを区別する。
    let loaded = migration::load_or_migrate()?;
    let settings = loaded.settings;
    let state = Arc::new(AppState {
        settings: RwLock::new(settings),
        settings_persisted: AtomicBool::new(loaded.persisted),
        cache_root,
        download_dir,
        quitting: AtomicBool::new(false),
        settings_opening: AtomicBool::new(false),
        portlet_navigation: Mutex::new(()),
        // 最初のローカル起動ページは世代1としてWebView生成時に指定している。
        portlet_load: Mutex::new(PortletLoadState::initial()),
        // WebView生成時の最初のPOSTでもSAMLへ進めるよう、認証試行中から始める。
        authentication: Mutex::new(AuthenticationState::initial(Instant::now())),
        decisions: AtomicUsize::new(0),
        decision_counter: AtomicUsize::new(1),
    });
    app.manage(state.clone());

    create_tray(app.handle())?;
    let shortcut_text = settings_snapshot(&state)
        .map(|settings| settings.shortcut)
        .unwrap_or_else(|_| "F3".to_owned());
    if let Ok(shortcut) = shortcut_text.parse::<tauri_plugin_global_shortcut::Shortcut>() {
        let _ = app.global_shortcut().register(shortcut);
    }

    match configured_url(&state)? {
        Some(url) => create_main_window(app.handle(), state.clone(), url)?,
        None => open_settings(app.handle())?,
    }
    start_timer(app.handle().clone(), state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_portlet_endpoint, has_allowed_origin, NOTIFICATION_TITLE};
    use crate::settings::Settings;
    use url::Url;

    #[test]
    fn accepts_only_the_configured_web_origin() {
        let allowed = "https://workflow.example";
        assert!(has_allowed_origin(
            &Url::parse("https://workflow.example/XFV20/portlet").expect("configured CWF URL"),
            allowed
        ));
        assert!(!has_allowed_origin(
            &Url::parse("https://other.example/XFV20/portlet").expect("external URL"),
            allowed
        ));
        assert!(!has_allowed_origin(
            &Url::parse("file:///C:/temp/test.html").expect("local file URL"),
            allowed
        ));
    }

    #[test]
    fn builds_a_portlet_endpoint_without_credentials_in_the_url() {
        let settings = Settings {
            id: "TESTUSER".to_owned(),
            cwf_address: "https://workflow.example/XFV20/portlet/wfportlet.jsp?old=value#fragment"
                .to_owned(),
            ..Settings::default()
        };

        let endpoint = build_portlet_endpoint(&settings).expect("endpoint");
        assert_eq!(
            endpoint.as_str(),
            "https://workflow.example/XFV20/portlet/wfportlet.jsp"
        );
        assert!(!endpoint.as_str().contains(&settings.id));
    }

    #[test]
    fn uses_the_japanese_notification_title() {
        assert_eq!(NOTIFICATION_TITLE, "電子決裁確認アプリ");
    }
}
