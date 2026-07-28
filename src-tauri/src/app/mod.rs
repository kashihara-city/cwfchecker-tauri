//! Tauriアプリ全体の状態を保持し、各機能モジュールを組み立てる。
//!
//! 認証、ダウンロード、画面操作を子モジュールへ分け、このファイルは
//! 共有状態、設定コマンド、起動時の初期化を管理する。

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
    version_policy::CURRENT_VERSION,
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};
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
    version: &'static str,
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

fn normalize_id(id: String, allow_empty: bool) -> Result<Option<String>, String> {
    if id.chars().any(char::is_control) {
        return Err("IDには制御文字を含めないでください。".to_owned());
    }
    let id = id.trim().to_owned();
    if id.is_empty() {
        if allow_empty {
            Ok(None)
        } else {
            Err("IDを入力してください。".to_owned())
        }
    } else {
        Ok(Some(id))
    }
}

/// `withGlobalTauri`はリモートのCWF画面にもIPCブリッジを公開する。
///
/// capabilityだけでなく、すべてのTauriコマンドでこの検査を行い、
/// 同梱した設定画面以外からアプリ定義コマンドを呼べない状態を維持する。
fn ensure_settings_window(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == SETTINGS_LABEL {
        Ok(())
    } else {
        Err("許可されていないウィンドウです。".to_owned())
    }
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
    if settings.cwf_address.is_empty() {
        return Ok(None);
    }
    if settings.use_saml_auth {
        return build_portlet_endpoint(&settings).map(Some);
    }
    let Some(credential) =
        credentials::read(credentials::TARGET).map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    if credential.username.trim().is_empty()
        || (!settings.use_saml_auth && credential.password.is_empty())
    {
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
    ensure_settings_window(&window)?;
    let settings = settings_snapshot(&state)?;
    let id = credentials::read(credentials::TARGET)
        .map_err(|error| error.to_string())?
        .map(|credential| credential.username)
        .unwrap_or_default();
    Ok(SettingsView {
        id,
        ad_server: settings.ad_server,
        cwf_address: settings.cwf_address,
        interval_minutes: settings.interval_minutes,
        notify_by_bar: settings.notify_by_bar,
        shortcut: settings.shortcut,
        version: CURRENT_VERSION,
    })
}

#[tauri::command]
/// 設定画面の入力を検証し、ショートカット、資格情報、レジストリへ反映する。
pub fn save_settings(
    app: AppHandle,
    window: WebviewWindow,
    state: tauri::State<'_, Arc<AppState>>,
    input: SettingsInput,
) -> Result<(), String> {
    ensure_settings_window(&window)?;
    let use_saml_auth = settings_snapshot(&state)?.use_saml_auth;
    let id = normalize_id(input.id, use_saml_auth)?;
    let settings = Settings {
        ad_server: input.ad_server,
        cwf_address: input.cwf_address,
        interval_minutes: input.interval_minutes,
        notify_by_bar: input.notify_by_bar,
        shortcut: input.shortcut,
        use_saml_auth,
    }
    .normalize()?;
    // レジストリ保存が途中で失敗したときに、GPO値も含む変更前の型と値へ戻す。
    let registry_snapshot = registry_support::report_io(
        "アプリ設定の変更前状態の読み込み",
        registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
        settings::snapshot(),
    )
    .map_err(|error| error.to_string())?;

    // SAMLの空IDでは資格情報を変更しない。IDがあればID/PWを一組で保存し、
    // PW空欄では同じIDの既存値を維持する。
    let existing = credentials::read(credentials::TARGET).map_err(|error| error.to_string())?;
    let wrote_credential = id.is_some();
    if let Some(id) = id.as_deref() {
        let password = if input.password.is_empty() {
            if let Some(existing) = existing.as_ref().filter(|value| value.username == id) {
                if !settings.use_saml_auth && existing.password.is_empty() {
                    return Err("PWを入力してください。".to_owned());
                }
                existing.password.clone()
            } else if settings.use_saml_auth {
                String::new()
            } else {
                return Err("IDを変更する場合はPWも入力してください。".to_owned());
            }
        } else {
            input.password
        };
        if let Err(error) = credentials::write_verified(credentials::TARGET, id, &password) {
            let mut message = error.to_string();
            if let Err(rollback_error) = credentials::restore(existing.as_ref()) {
                message.push_str(&format!(
                    "\nWindows資格情報を変更前の状態へ戻せませんでした: {rollback_error}"
                ));
            }
            return Err(message);
        }
    }

    // 既にこのアプリが登録済みなら再登録しない。変更時は旧キーを残したまま
    // 新キーを先に登録し、競合していれば永続化せず設定画面へエラーを返す。
    let shortcut = settings
        .shortcut
        .parse::<Shortcut>()
        .expect("normalize済みのショートカット");
    let registered_for_save = if app.global_shortcut().is_registered(shortcut) {
        false
    } else if let Err(error) = app.global_shortcut().register(shortcut) {
        let mut message = format!(
            "ショートカットキー「{}」を登録できませんでした。ほかのアプリで使用されていないキーを指定してください。\n{error}",
            settings.shortcut
        );
        if wrote_credential {
            if let Err(rollback_error) = credentials::restore(existing.as_ref()) {
                message.push_str(&format!(
                    "\nWindows資格情報を変更前の状態へ戻せませんでした: {rollback_error}"
                ));
            }
        }
        return Err(message);
    } else {
        true
    };

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
            settings::restore_snapshot(&registry_snapshot),
        );
        let credential_rollback = wrote_credential.then(|| credentials::restore(existing.as_ref()));
        let mut message = error;
        if registered_for_save {
            if let Err(rollback_error) = app.global_shortcut().unregister(shortcut) {
                message.push_str(&format!(
                    "\n新しいショートカットキーの登録を解除できませんでした: {rollback_error}"
                ));
            }
        }
        if let Err(rollback_error) = registry_rollback {
            message.push_str(&format!(
                "\nアプリ設定を変更前の状態へ戻せませんでした: {rollback_error}"
            ));
        }
        if let Some(Err(rollback_error)) = credential_rollback {
            message.push_str(&format!(
                "\nWindows資格情報を変更前の状態へ戻せませんでした: {rollback_error}"
            ));
        }
        return Err(message);
    }
    // 両方の永続化に成功してから、実行中アプリが参照する値を切り替える。
    *state.settings.write().map_err(|_| lock_error())? = settings;

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
    let loaded = migration::load_or_migrate()?;
    let settings = loaded.settings;
    registry_support::report_io(
        "SAML既定設定の保存",
        registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
        settings::write_missing_saml_defaults(&settings),
    )?;
    let state = Arc::new(AppState {
        settings: RwLock::new(settings),
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
    use super::{
        build_portlet_endpoint, has_allowed_origin, normalize_id, CURRENT_VERSION,
        NOTIFICATION_TITLE,
    };
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
    fn normalizes_and_validates_credential_ids() {
        assert_eq!(
            normalize_id("  USER  ".to_owned(), false).expect("ID"),
            Some("USER".to_owned())
        );
        assert!(normalize_id("  ".to_owned(), false)
            .unwrap_err()
            .contains("ID"));
        assert_eq!(
            normalize_id("  ".to_owned(), true).expect("optional SAML ID"),
            None
        );
        assert!(normalize_id("USER\u{0007}".to_owned(), true)
            .unwrap_err()
            .contains("制御文字"));
    }

    #[test]
    fn builds_a_portlet_endpoint_without_credentials_in_the_url() {
        let settings = Settings {
            cwf_address: "https://workflow.example/XFV20/portlet/wfportlet.jsp?old=value#fragment"
                .to_owned(),
            ..Settings::default()
        };

        let endpoint = build_portlet_endpoint(&settings).expect("endpoint");
        assert_eq!(
            endpoint.as_str(),
            "https://workflow.example/XFV20/portlet/wfportlet.jsp"
        );
    }

    #[test]
    fn uses_the_japanese_notification_title() {
        assert_eq!(NOTIFICATION_TITLE, "電子決裁確認アプリ");
    }

    #[test]
    fn exposes_the_cargo_package_version() {
        assert_eq!(CURRENT_VERSION, env!("CARGO_PKG_VERSION"));
    }
}
