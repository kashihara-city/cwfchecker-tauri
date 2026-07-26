//! Tauriアプリの画面、タスクトレイ、タイマー、ダウンロードをまとめて管理する。
//!
//! 個別の保存処理は`settings`、`credentials`、`migration`へ分け、このファイルは
//! それらを「いつ呼ぶか」と、WebViewから来た情報をどう画面へ反映するかに集中する。

use crate::{
    credentials, migration, registry_support,
    settings::{self, Settings},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{
    menu::{Menu, MenuBuilder, SubmenuBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::{DownloadEvent, NewWindowResponse, PageLoadEvent},
    AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutEvent, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use url::Url;
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

const MAIN_LABEL: &str = "main";
const SETTINGS_LABEL: &str = "settings";
const APP_TITLE: &str = "CreateWebFlowChecker";
const NOTIFICATION_TITLE: &str = "電子決裁確認アプリ";
const PORTLET_BOOTSTRAP_URL: &str = "http://tauri.localhost/portlet-bootstrap.html";
const EXTERNAL_AUTH_WINDOW: Duration = Duration::from_secs(30);
const AUTHENTICATION_ATTEMPT_WINDOW: Duration = Duration::from_secs(60);
const RELOAD_FALLBACK_DELAY: Duration = Duration::from_secs(5);
const PORTLET_POST_SCRIPT: &str = include_str!("../scripts/portlet-post.js");
const WEBVIEW_SCRIPT: &str = include_str!("../scripts/cwf-scan.js");
/// ダウンロード直後にShellExecuteで開くと危険な、実行・スクリプト系の拡張子。
const BLOCKED_DOWNLOAD_EXTENSIONS: &[&str] = &[
    "bat", "cmd", "com", "cpl", "dll", "exe", "hta", "jar", "jse", "js", "lnk", "msi", "msp",
    "ps1", "reg", "scr", "sys", "url", "vbe", "vbs", "wsf", "wsh",
];

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

/// どのポートレット再読込みが現在有効かを表す。
///
/// `navigate()`は非同期なので、古いページの完了通知が新しい要求より後に届くことがある。
/// 単純なtrue/falseでは区別できないため、要求ごとに増える世代番号を使う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PortletLoadPhase {
    Reloading,
    Posting,
    Complete,
}

struct PortletLoadState {
    active_generation: usize,
    posted_generation: Option<usize>,
    phase: PortletLoadPhase,
}

impl PortletLoadState {
    /// 次の世代を、Cookieを使った通常reloadとして開始する。
    fn begin_next_generation(&mut self) -> usize {
        // 0と1は使わず、初回WebViewを世代1、明示的な再読込みを世代2以降にする。
        let current = self.active_generation.wrapping_add(1).max(2);
        self.active_generation = current;
        self.phase = PortletLoadPhase::Reloading;
        current
    }

    fn is_active(&self, generation: usize) -> bool {
        self.active_generation == generation
    }

    /// 5秒後も同じ世代がreload待ちの場合だけ、POSTへの切替権を取得する。
    fn begin_post_fallback(&mut self, generation: usize) -> bool {
        if !self.is_active(generation) || self.phase != PortletLoadPhase::Reloading {
            return false;
        }
        self.phase = PortletLoadPhase::Posting;
        true
    }

    fn is_posting(&self, generation: usize) -> bool {
        self.is_active(generation) && self.phase == PortletLoadPhase::Posting
    }

    /// 現在の世代で認証成功DOMを確認し、待機中のフォールバックを無効にする。
    fn mark_authenticated(&mut self) {
        self.phase = PortletLoadPhase::Complete;
    }

    /// 最新世代のPOST実行権を一度だけ取得する。
    ///
    /// `true`を受け取った呼び出し元だけがPW入りフォームを作成してよい。
    fn claim_post(&mut self, generation: usize) -> bool {
        if !self.is_posting(generation) || self.posted_generation == Some(generation) {
            return false;
        }
        self.posted_generation = Some(generation);
        true
    }
}

#[derive(Clone, Copy)]
/// SAML認証中に、設定先以外のHTTPSへ移動できる期限を管理する。
struct AuthenticationState {
    /// ポートレットPOSTから認証結果の受信までの間だけtrueになる。
    in_progress: bool,
    /// 認証開始から数えて、この時刻を過ぎた外部遷移は最初の1回でも許可しない。
    attempt_deadline: Option<Instant>,
    /// 最初の外部HTTPS遷移時に設定する固定期限。遷移のたびには延長しない。
    external_deadline: Option<Instant>,
}

impl AuthenticationState {
    /// 新しいポートレット読込みでは、前回の期限を必ず捨てて認証をやり直す。
    fn begin(&mut self, now: Instant) {
        self.in_progress = true;
        self.attempt_deadline = Some(now + AUTHENTICATION_ATTEMPT_WINDOW);
        self.external_deadline = None;
    }

    fn finish(&mut self) {
        self.in_progress = false;
        self.attempt_deadline = None;
        self.external_deadline = None;
    }

    /// 外部遷移は閉じるが、同じ更新のPOSTフォールバックで使う絶対期限は残す。
    fn close_external_navigation(&mut self) {
        self.in_progress = false;
        self.external_deadline = None;
    }

    fn finish_if_returned(&mut self) {
        // `None`は「まだIdPへ出る前」なので、最初のPOST先到達では閉じない。
        // `Some`なら一度IdP等へ出た後なので、設定先へ戻った時点で閉じる。
        if self.external_deadline.is_some() {
            self.close_external_navigation();
        }
    }

    /// reloadと同じ絶対期限のまま、POST用の外部遷移許可を開き直す。
    fn resume_for_fallback(&mut self, now: Instant) -> bool {
        if self.attempt_deadline.is_none_or(|deadline| now > deadline) {
            self.finish();
            return false;
        }
        self.in_progress = true;
        self.external_deadline = None;
        true
    }

    fn allow_external_https(&mut self, url: &Url, now: Instant) -> bool {
        if !self.in_progress || url.scheme() != "https" {
            return false;
        }
        let Some(attempt_deadline) = self.attempt_deadline else {
            self.finish();
            return false;
        };
        if now > attempt_deadline {
            // POST後に結果レポートが届かなくても、古い認証試行を後から再利用させない。
            self.finish();
            return false;
        }
        match self.external_deadline {
            Some(deadline) => now <= deadline,
            None => {
                // ここで一度だけ期限を決める。開始からの絶対上限を越えないよう、
                // 「外部へ出てから30秒」と「認証開始から60秒」の早い方を使う。
                self.external_deadline = Some((now + EXTERNAL_AUTH_WINDOW).min(attempt_deadline));
                true
            }
        }
    }
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

#[derive(Debug)]
/// WebView内のJavaScriptがページを調査した結果。
pub struct PageReport {
    decision_count: usize,
    auth_count: usize,
    image_count: usize,
    content_height: usize,
    count_text: String,
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

/// Tauriが同梱HTMLを配信する、POST開始専用ページかを判定する。
fn is_portlet_bootstrap_url(url: &Url) -> bool {
    url.scheme() == "http"
        && url.host_str() == Some("tauri.localhost")
        && url.path() == "/portlet-bootstrap.html"
}

fn portlet_bootstrap_url(generation: usize) -> Result<Url, String> {
    let mut url = Url::parse(PORTLET_BOOTSTRAP_URL)
        .map_err(|_| "POST開始ページのURLが不正です。".to_owned())?;
    // `?load=N`はサーバーへ送る値ではなく、遅れて届いたWebViewイベントを
    // どの読込み要求のものかRust側で見分けるための印。
    url.query_pairs_mut()
        .append_pair("load", &generation.to_string());
    Ok(url)
}

/// bootstrap URLに付けた`load`クエリから世代番号を取り出す。
fn portlet_load_generation(url: &Url) -> Option<usize> {
    if !is_portlet_bootstrap_url(url) {
        return None;
    }
    // WebView生成時の初回ページだけはクエリなしで、世代1として扱う。
    if url.query().is_none() {
        return Some(1);
    }
    url.query_pairs()
        .find(|(name, _)| name == "load")
        .and_then(|(_, value)| value.parse().ok())
}

/// ローカルページ上で一度だけ実行する、公式例と同じhiddenフォームPOSTを作る。
///
/// IDとPWはURLへ連結せず、`application/x-www-form-urlencoded`のPOST本文として
/// WebView2から送信する。JSON文字列化により、引用符などをJavaScriptとして
/// 解釈させない。
fn portlet_post_script(state: &AppState, bootstrap_url: &str) -> Result<String, String> {
    let settings = settings_snapshot(state)?;
    let credential = credentials::read(credentials::TARGET)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "PWが保存されていません。".to_owned())?;
    if credential.username != settings.id {
        return Err("設定中のIDと保存済みPWのIDが一致しません。".to_owned());
    }

    let config = serde_json::json!({
        "bootstrapUrl": bootstrap_url,
        "action": build_portlet_endpoint(&settings)?.as_str(),
        "fields": [
            ["view", "recv"],
            ["loginid", settings.id.as_str()],
            ["pwd", credential.password.as_str()],
            ["ldapsvr", settings.ad_server.as_str()],
        ],
    });
    Ok(format!("({PORTLET_POST_SCRIPT})({config});"))
}

/// reloadが5秒以内に認証成功しなかった場合だけ、従来のPOST読込みへ切り替える。
fn begin_portlet_post(
    window: &WebviewWindow,
    state: &AppState,
    generation: usize,
) -> Result<(), String> {
    // 世代確認からnavigateまでを直列化し、別の更新要求に追い越されないようにする。
    let _navigation = state.portlet_navigation.lock().map_err(|_| lock_error())?;
    let bootstrap = portlet_bootstrap_url(generation)?;
    let should_post = state
        .portlet_load
        .lock()
        .map_err(|_| lock_error())?
        .begin_post_fallback(generation);
    if !should_post {
        // 認証成功済み、または新しい更新が始まっているため、このタイマーは何もしない。
        return Ok(());
    }
    if !state
        .authentication
        .lock()
        .map_err(|_| lock_error())?
        .resume_for_fallback(Instant::now())
    {
        return Err("認証試行の有効期限が切れました。".to_owned());
    }
    if let Err(error) = window.navigate(bootstrap) {
        finish_authentication_if_active_load(state, generation);
        return Err(error.to_string());
    }
    Ok(())
}

/// 新しい世代を採番し、現在のページを無条件にreloadする。
fn begin_portlet_load(window: &WebviewWindow, state: Arc<AppState>) -> Result<(), String> {
    let (generation, reload_result) = {
        // reload要求と世代採番の順序を一致させる。
        let _navigation = state.portlet_navigation.lock().map_err(|_| lock_error())?;
        let generation = state
            .portlet_load
            .lock()
            .map_err(|_| lock_error())?
            .begin_next_generation();
        state
            .authentication
            .lock()
            .map_err(|_| lock_error())?
            .begin(Instant::now());
        // reloadはon_navigationを同期的に呼ぶ可能性があるため、各状態のガードは手放しておく。
        (generation, window.reload())
    };
    if let Err(error) = reload_result {
        eprintln!("通常reloadを開始できなかったためPOSTへ切り替えます: {error}");
        return begin_portlet_post(window, &state, generation);
    }

    let window = window.clone();
    thread::spawn(move || {
        thread::sleep(RELOAD_FALLBACK_DELAY);
        if let Err(error) = begin_portlet_post(&window, &state, generation) {
            eprintln!("通常reload後のPOSTフォールバックに失敗しました: {error}");
        }
    });
    Ok(())
}

/// 指定世代がまだ最新なら、その読込みで始めた外部HTTPS許可を終了する。
///
/// 古い世代の失敗通知で新しい認証試行まで閉じないため、世代確認と終了をセットで行う。
fn finish_authentication_if_active_load(state: &AppState, generation: usize) {
    let is_active = state
        .portlet_load
        .lock()
        .is_ok_and(|load| load.is_active(generation));
    if is_active {
        if let Ok(mut authentication) = state.authentication.lock() {
            authentication.finish();
        }
    }
}

fn webview_script(origin: &str) -> String {
    // 値を手作業で引用符へ入れずJSON化することで、URL中の記号をJSコードとして
    // 解釈させない。静的な処理本体は別ファイルなので通常のJSとして編集できる。
    let origin = serde_json::to_string(origin).unwrap_or_else(|_| "\"\"".to_owned());
    format!("({WEBVIEW_SCRIPT})({origin});")
}

fn cache_root() -> PathBuf {
    std::env::temp_dir()
        .join("KashiharaCity")
        .join("CwfChecker")
        .join("WebView2")
}

fn prepare_cache() -> std::io::Result<PathBuf> {
    let root = cache_root();
    // WebView2の認証状態を次回起動へ持ち越さない。単一起動プラグインにより、
    // 同時に使用中の別プロセスのキャッシュを消す状況は避けられる。
    if root.exists() {
        let _ = fs::remove_dir_all(&root);
    }
    let current = root.join(std::process::id().to_string());
    fs::create_dir_all(&current)?;
    Ok(current)
}

/// アプリ管理のダウンロードフォルダーを残し、中身だけをすべて削除する。
fn cleanup_directory(path: &Path) -> std::io::Result<()> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // 1件が使用中でも他のファイルは消せるよう、最初のエラーだけ覚えて処理を続ける。
    let mut first_error = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                first_error.get_or_insert(error);
                continue;
            }
        };
        let candidate = entry.path();
        let result = if candidate.is_dir() {
            fs::remove_dir_all(candidate)
        } else {
            fs::remove_file(candidate)
        };
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Windowsの関連付けアプリでファイルまたはURLを開く。
fn open_external(value: &str) -> Result<(), String> {
    let operation: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            value.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        Err("Windowsで対象を開けませんでした。".to_owned())
    } else {
        Ok(())
    }
}

/// 自動実行につながりやすいファイル形式かを、最終拡張子で判定する。
fn is_blocked_download(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    // Windowsは末尾の空白やピリオドを正規化するため、判定前にも除去する。
    // コロンは代替データストリーム指定になり得るので許可しない。
    let normalized = name.trim_end_matches([' ', '.']);
    if normalized.contains(':') {
        return true;
    }
    Path::new(normalized)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            BLOCKED_DOWNLOAD_EXTENSIONS
                .iter()
                .any(|blocked| extension.eq_ignore_ascii_case(blocked))
        })
}

/// URLが設定されたCreate!Webフローと同じオリジンかを確認する。
///
/// オリジンはスキーム、ホスト、ポートの組であり、パスだけの比較より厳密である。
fn has_allowed_origin(url: &Url, allowed_origin: &str) -> bool {
    matches!(url.scheme(), "http" | "https") && url.origin().ascii_serialization() == allowed_origin
}

/// ダウンロードを開始したページ自体が、信頼する設定先かを確認する。
fn can_download_from_page(url: &Url, allowed_origin: &str) -> bool {
    has_allowed_origin(url, allowed_origin)
}

/// 完了したファイルが、実体として管理対象のダウンロードフォルダー内にあるか確認する。
fn is_managed_download(path: &Path, download_dir: &Path) -> bool {
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    let Ok(download_dir) = fs::canonicalize(download_dir) else {
        return false;
    };
    path != download_dir && path.starts_with(download_dir)
}

/// 添付の保存先、危険拡張子、ダウンロード後の自動起動を各WebViewへ設定する。
fn configure_download(
    builder: WebviewWindowBuilder<'_, tauri::Wry, impl Manager<tauri::Wry>>,
    download_dir: PathBuf,
    allowed_origin: String,
) -> WebviewWindowBuilder<'_, tauri::Wry, impl Manager<tauri::Wry>> {
    builder.on_download(move |webview, event| {
        match event {
            DownloadEvent::Requested { destination, .. } => {
                // ダウンロードURLではなく「現在表示中のページ」を検査する。
                // これによりIdPや認証中の外部ページからの自動保存・起動を防ぐ。
                if !webview
                    .url()
                    .is_ok_and(|url| can_download_from_page(&url, &allowed_origin))
                {
                    return false;
                }
                // フォルダーが消されていても、次の添付時に作り直す。
                if fs::create_dir_all(&download_dir).is_err() {
                    return false;
                }
                let filename = destination
                    .file_name()
                    .map(|name| name.to_owned())
                    .unwrap_or_else(|| "download".into());
                if is_blocked_download(Path::new(&filename)) {
                    // 添付を自動で開く設計のため、実行・スクリプト形式は
                    // ダウンロード自体を中止し、誤実行を防ぐ。
                    return false;
                }
                *destination = download_dir.join(filename);
            }
            DownloadEvent::Finished {
                path: Some(path),
                success: true,
                ..
            } if !is_blocked_download(&path)
                && is_managed_download(&path, &download_dir)
                && webview
                    .url()
                    .is_ok_and(|url| can_download_from_page(&url, &allowed_origin)) =>
            {
                // Requested後にページや保存先が変わった場合にも備え、
                // Windowsで開く直前にオリジン・保存先・拡張子をすべて再確認する。
                let _ = open_external(&path.to_string_lossy());
            }
            _ => {}
        }
        true
    })
}

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
fn open_settings(app: &AppHandle) -> Result<(), String> {
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

/// Create!Webフローのポートレットを表示するメインWebViewを作る。
fn create_main_window(app: &AppHandle, state: Arc<AppState>, endpoint: Url) -> Result<(), String> {
    let origin = endpoint.origin().ascii_serialization();
    let script = webview_script(&origin);
    // 各`move`クロージャーは渡した値の所有権を持つため、同じ値を複数の
    // コールバックで使う分だけcloneしておく。Arcは参照カウントを増やし、
    // Stringは短いオリジン文字列を複製する。
    let origin_for_main_navigation = origin.clone();
    let origin_for_popup = origin.clone();
    let origin_for_main_download = origin.clone();
    let app_for_popup = app.clone();
    let state_for_navigation = state.clone();
    let state_for_page_load = state.clone();
    let state_for_popup = state.clone();
    let state_for_report = state.clone();
    let download_dir = state.download_dir.clone();
    let cache = state.cache_root.clone();
    let menu = build_window_menu(app, MAIN_LABEL).map_err(|error| error.to_string())?;

    let builder = WebviewWindowBuilder::new(
        app,
        MAIN_LABEL,
        WebviewUrl::App("portlet-bootstrap.html".into()),
    )
    .title(APP_TITLE)
    .inner_size(600.0, 520.0)
    .position(0.0, 0.0)
    .visible(false)
    .data_directory(cache)
    .initialization_script(script)
    .menu(menu)
    .on_navigation(move |url| {
        // 1. 設定先は常に許可する。ただしIdPから戻った場合は外部許可を閉じる。
        if has_allowed_origin(url, &origin_for_main_navigation) {
            if let Ok(mut authentication) = state_for_navigation.authentication.lock() {
                // 最初のPOST先では閉じず、IdP等へ一度出た後の帰還時だけ閉じる。
                authentication.finish_if_returned();
            }
            return true;
        }
        // 2. ローカルbootstrapは、Rustが現在開始した世代だけ許可する。
        if let Some(generation) = portlet_load_generation(url) {
            return state_for_navigation
                .portlet_load
                .lock()
                .is_ok_and(|load| load.is_posting(generation));
        }
        // 3. その他は、最初の外部HTTPS遷移から30秒間だけSAML IdP等を許可する。
        state_for_navigation
            .authentication
            .lock()
            .is_ok_and(|mut authentication| {
                authentication.allow_external_https(url, Instant::now())
            })
    })
    .on_menu_event(|window, event| {
        handle_window_menu(window, event.id().as_ref());
    })
    .on_page_load(move |window, payload| {
        if !matches!(payload.event(), PageLoadEvent::Finished) {
            return;
        }
        // `window.url()`は、イベント発生後に別ページへ進んでいると新しいURLを返す。
        // 遅れて届いた完了イベントを正しい読込み世代と結び付けるため、
        // このイベント自身が持つURLを使う。
        let current_url = payload.url();
        if let Some(generation) = portlet_load_generation(current_url) {
            // 最新世代の遷移につき一度だけ、PWをPOST本文へ載せる。
            // MutexGuardを`should_post`の計算で手放してからevalするため、
            // eval中のコールバックが同じMutexを取得してもデッドロックしない。
            let should_post = state_for_page_load
                .portlet_load
                .lock()
                .is_ok_and(|mut load| load.claim_post(generation));
            if should_post {
                match portlet_post_script(&state_for_page_load, current_url.as_str()) {
                    Ok(script) => {
                        if let Err(error) = window.eval(&script) {
                            finish_authentication_if_active_load(&state_for_page_load, generation);
                            eprintln!("ポートレットのPOST開始に失敗しました: {error}");
                        }
                    }
                    Err(error) => {
                        finish_authentication_if_active_load(&state_for_page_load, generation);
                        eprintln!("ポートレットのPOST情報を作成できませんでした: {error}");
                    }
                }
            }
        } else if has_allowed_origin(current_url, &origin) {
            let _ = window.eval("window.__cwfScan?.()");
        }
    })
    .on_document_title_changed(move |window, title| {
        if let Some(report) = parse_report_title(&title) {
            let _ = process_page_report(window.app_handle(), &window, &state_for_report, report);
            let _ = window.set_title(APP_TITLE);
        }
    })
    .on_new_window(move |url, features| {
        if !has_allowed_origin(&url, &origin_for_popup) {
            // 外部リンクは資格情報を持つWebViewへ読み込まず、通常ブラウザへ分離する。
            if matches!(url.scheme(), "http" | "https") {
                let _ = open_external(url.as_str());
            }
            return NewWindowResponse::Deny;
        }
        if url.path().ends_with("/XFV20/login") {
            let _ = open_external(url.as_str());
            return NewWindowResponse::Deny;
        }

        let number = state_for_popup
            .decision_counter
            .fetch_add(1, Ordering::Relaxed);
        let label = format!("decision-{number}");
        let menu = match build_window_menu(&app_for_popup, &label) {
            Ok(menu) => menu,
            Err(_) => return NewWindowResponse::Deny,
        };
        let decision_origin = origin_for_popup.clone();
        let decision_download_origin = origin_for_popup.clone();
        let decision = WebviewWindowBuilder::new(
            &app_for_popup,
            label,
            WebviewUrl::External("about:blank".parse().expect("valid URL")),
        )
        .title(APP_TITLE)
        .window_features(features)
        .data_directory(state_for_popup.cache_root.clone())
        .on_navigation(move |url| {
            url.as_str() == "about:blank" || has_allowed_origin(url, &decision_origin)
        })
        .menu(menu)
        .on_menu_event(|window, event| {
            handle_window_menu(window, event.id().as_ref());
        });
        let decision = configure_download(
            decision,
            state_for_popup.download_dir.clone(),
            decision_download_origin,
        );
        match decision.build() {
            Ok(window) => {
                state_for_popup.decisions.fetch_add(1, Ordering::Relaxed);
                let _ = window.maximize();
                NewWindowResponse::Create { window }
            }
            Err(_) => NewWindowResponse::Deny,
        }
    });
    configure_download(builder, download_dir, origin_for_main_download)
        .build()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn build_window_menu(app: &AppHandle, window_label: &str) -> tauri::Result<Menu<tauri::Wry>> {
    let app_menu = SubmenuBuilder::new(app, "メニュー")
        .text(format!("settings:{window_label}"), "設定画面")
        .separator()
        .text(format!("quit:{window_label}"), "アプリ終了")
        .build()?;
    MenuBuilder::new(app)
        .item(&app_menu)
        .text(format!("reload:{window_label}"), "更新")
        .text(format!("close:{window_label}"), "閉じる")
        .build()
}

/// タスクトレイのアイコンと、表示・終了メニューを作る。
fn create_tray(app: &AppHandle) -> tauri::Result<()> {
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

fn start_timer(app: AppHandle, state: Arc<AppState>) {
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

fn parse_report_title(title: &str) -> Option<PageReport> {
    let mut parts = title.strip_prefix("__CWFCHECKER_REPORT__|")?.splitn(5, '|');
    let decision_count: usize = parts.next()?.parse().ok()?;
    let auth_count: usize = parts.next()?.parse().ok()?;
    let image_count: usize = parts.next()?.parse().ok()?;
    let content_height: usize = parts.next()?.parse().ok()?;
    // document.titleはサーバー側ページが変更できるため、通知へ出す文字列は
    // 制御文字を除去し、長さも制限する。空なら数えた案件数を使用する。
    let mut count_text: String = parts
        .next()?
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(32)
        .collect();
    if count_text.is_empty() {
        count_text = decision_count.to_string();
    }
    Some(PageReport {
        decision_count,
        auth_count,
        image_count,
        content_height,
        count_text,
    })
}

/// WebViewから受けた調査結果を、ウィンドウサイズと通知へ反映する。
fn process_page_report(
    app: &AppHandle,
    window: &WebviewWindow,
    state: &Arc<AppState>,
    report: PageReport,
) -> Result<(), String> {
    if window.label() != MAIN_LABEL {
        return Err("許可されていないウィンドウです。".to_owned());
    }
    let expected =
        configured_origin(state)?.ok_or_else(|| "CWFAddressが設定されていません。".to_owned())?;
    let actual = window
        .url()
        .map_err(|error| error.to_string())?
        .origin()
        .ascii_serialization();
    if actual != expected {
        return Err("許可されていないオリジンです。".to_owned());
    }
    if report.auth_count > 0 {
        // 認証成功DOMを確認できた世代では、5秒タイマーからPOSTへ切り替えない。
        state
            .portlet_load
            .lock()
            .map_err(|_| lock_error())?
            .mark_authenticated();
        state
            .authentication
            .lock()
            .map_err(|_| lock_error())?
            .finish();
    } else {
        // 同一オリジンのエラー画面等では外部遷移だけ閉じ、POSTへ引き継ぐ絶対期限は残す。
        state
            .authentication
            .lock()
            .map_err(|_| lock_error())?
            .close_external_navigation();
    }
    let rows = report.decision_count.clamp(1, 30);
    let requested_height = if report.image_count > 0 && report.content_height > 0 {
        report.content_height
    } else {
        290 + rows * 35
    };
    let maximum_height = window
        .current_monitor()
        .ok()
        .flatten()
        .map(|monitor| monitor.size().height.saturating_sub(60) as usize)
        .unwrap_or(requested_height);
    let height = requested_height.min(maximum_height);
    window
        .set_size(PhysicalSize::new(600, height as u32))
        .map_err(|error| error.to_string())?;
    let _ = window.set_position(PhysicalPosition::new(0, 0));

    if report.decision_count > 0 {
        let settings = settings_snapshot(state)?;
        let visible = window.is_visible().unwrap_or(false);
        if settings.notify_by_bar && !visible {
            let body = format!("{} 件処理待ちです。", report.count_text);
            if let Err(error) = show_windows_notification(app, NOTIFICATION_TITLE, &body) {
                eprintln!("Windows通知の表示に失敗しました: {error}");
            }
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
    if report.auth_count > 0 {
        // 認証成功の目印を同一オリジンのページで確認できた時点でのみ、
        // 移行元を削除する。単なる書き込み後照合だけでは削除しない。
        if let Ok(current) = settings_snapshot(state) {
            let _ = credentials::delete(&credentials::legacy_target(&current.id));
        }
        if let Some(path) = migration::legacy_config_path() {
            let _ = fs::remove_file(path);
        }
    }
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
        portlet_load: Mutex::new(PortletLoadState {
            active_generation: 1,
            posted_generation: None,
            phase: PortletLoadPhase::Posting,
        }),
        // WebView生成時の最初のPOSTでもSAMLへ進めるよう、認証試行中から始める。
        authentication: Mutex::new(AuthenticationState {
            in_progress: true,
            attempt_deadline: Some(Instant::now() + AUTHENTICATION_ATTEMPT_WINDOW),
            external_deadline: None,
        }),
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

fn handle_window_menu(window: &tauri::Window, id: &str) {
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
            if main.is_visible().unwrap_or(false)
                && app
                    .webview_windows()
                    .keys()
                    .all(|label| !label.starts_with("decision-"))
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
    use super::{
        build_portlet_endpoint, can_download_from_page, cleanup_directory, has_allowed_origin,
        is_blocked_download, is_managed_download, is_portlet_bootstrap_url, parse_report_title,
        portlet_bootstrap_url, portlet_load_generation, webview_script, AuthenticationState,
        PortletLoadPhase, PortletLoadState, AUTHENTICATION_ATTEMPT_WINDOW, EXTERNAL_AUTH_WINDOW,
        NOTIFICATION_TITLE, PORTLET_POST_SCRIPT,
    };
    use crate::settings::Settings;
    use std::{
        fs,
        path::Path,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use url::Url;

    #[test]
    fn parses_rendered_content_height() {
        let report = parse_report_title("__CWFCHECKER_REPORT__|3|1|2|746|3").expect("valid report");

        assert_eq!(report.decision_count, 3);
        assert_eq!(report.auth_count, 1);
        assert_eq!(report.image_count, 2);
        assert_eq!(report.content_height, 746);
        assert_eq!(report.count_text, "3");
    }

    #[test]
    fn sanitizes_text_received_through_the_document_title() {
        let long_text = format!("{}\nignored", "9".repeat(40));
        let title = format!("__CWFCHECKER_REPORT__|3|0|0|0|{long_text}");
        let report = parse_report_title(&title).expect("valid report");

        assert_eq!(report.count_text, "9".repeat(32));
    }

    #[test]
    fn generated_script_reports_each_document_only_once() {
        let script = webview_script("https://workflow.example");

        assert!(script.contains("window.__cwfScanRunning || window.__cwfScanReported"));
        assert!(script.contains("window.__cwfScanReported = true;"));
        assert!(PORTLET_POST_SCRIPT.contains("form.submit()"));
    }

    #[test]
    fn uses_the_japanese_notification_title() {
        assert_eq!(NOTIFICATION_TITLE, "電子決裁確認アプリ");
    }

    #[test]
    fn blocks_executable_downloads_and_windows_name_tricks() {
        assert!(is_blocked_download(Path::new("attachment.EXE")));
        assert!(is_blocked_download(Path::new("attachment.cmd.")));
        assert!(is_blocked_download(Path::new("attachment.txt:payload.exe")));
        assert!(!is_blocked_download(Path::new("attachment.pdf")));
    }

    #[test]
    fn accepts_only_the_configured_web_origin() {
        let allowed = "https://workflow.example";
        assert!(has_allowed_origin(
            &Url::parse("https://workflow.example/XFV20/portlet").expect("URL"),
            allowed
        ));
        assert!(!has_allowed_origin(
            &Url::parse("https://other.example/XFV20/portlet").expect("URL"),
            allowed
        ));
        assert!(!has_allowed_origin(
            &Url::parse("file:///C:/temp/test.html").expect("URL"),
            allowed
        ));
    }

    #[test]
    fn allows_external_https_for_thirty_seconds_without_extending_the_deadline() {
        let start = Instant::now();
        let mut authentication = AuthenticationState {
            in_progress: true,
            attempt_deadline: Some(start + AUTHENTICATION_ATTEMPT_WINDOW),
            external_deadline: None,
        };
        let idp = Url::parse("https://idp.example/login").expect("IdP URL");
        let federation = Url::parse("https://federation.example/continue").expect("federation URL");

        // 最初の設定先ページでは、まだ外部へ出ていないため認証試行を閉じない。
        authentication.finish_if_returned();
        assert!(authentication.in_progress);
        assert!(authentication.allow_external_https(&idp, start));
        let deadline = authentication.external_deadline;
        assert_eq!(deadline, Some(start + EXTERNAL_AUTH_WINDOW));
        assert!(authentication.allow_external_https(
            &federation,
            start + EXTERNAL_AUTH_WINDOW - Duration::from_secs(1)
        ));
        assert_eq!(authentication.external_deadline, deadline);
        assert!(!authentication.allow_external_https(
            &federation,
            start + EXTERNAL_AUTH_WINDOW + Duration::from_millis(1)
        ));

        // 外部へ出た後、設定先へ戻った時点で期限前でも閉じる。
        authentication.finish_if_returned();
        assert!(!authentication.in_progress);
        assert!(authentication.external_deadline.is_none());
        // 同じ更新のPOSTフォールバックで再開できるよう、絶対期限は保持する。
        assert_eq!(
            authentication.attempt_deadline,
            Some(start + AUTHENTICATION_ATTEMPT_WINDOW)
        );
        assert!(authentication.resume_for_fallback(start + Duration::from_secs(5)));
        assert!(authentication.in_progress);
        assert_eq!(
            authentication.attempt_deadline,
            Some(start + AUTHENTICATION_ATTEMPT_WINDOW)
        );
        authentication.finish();
        assert!(authentication.attempt_deadline.is_none());
    }

    #[test]
    fn rejects_external_https_after_the_authentication_attempt_deadline() {
        let start = Instant::now();
        let mut authentication = AuthenticationState {
            in_progress: false,
            attempt_deadline: None,
            external_deadline: None,
        };
        let idp = Url::parse("https://idp.example/login").expect("IdP URL");

        authentication.begin(start);
        let late_first_navigation = start + AUTHENTICATION_ATTEMPT_WINDOW - Duration::from_secs(10);
        assert!(authentication.allow_external_https(&idp, late_first_navigation));
        // 外部遷移が遅く始まっても、30秒を足して絶対上限を越えてはならない。
        assert_eq!(
            authentication.external_deadline,
            Some(start + AUTHENTICATION_ATTEMPT_WINDOW)
        );
        assert!(!authentication.allow_external_https(
            &idp,
            start + AUTHENTICATION_ATTEMPT_WINDOW + Duration::from_millis(1)
        ));
        assert!(!authentication.in_progress);
        assert!(authentication.attempt_deadline.is_none());
        assert!(authentication.external_deadline.is_none());
    }

    #[test]
    fn rejects_downloads_started_from_external_pages() {
        let allowed = "https://workflow.example";

        assert!(can_download_from_page(
            &Url::parse("https://workflow.example/XFV20/decision").expect("CWF URL"),
            allowed
        ));
        assert!(!can_download_from_page(
            &Url::parse("https://idp.example/error").expect("IdP URL"),
            allowed
        ));
    }

    #[test]
    fn accepts_only_completed_downloads_inside_the_managed_directory() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cwfchecker-download-check-{}-{nonce}",
            std::process::id()
        ));
        let managed = root.join("managed");
        let outside = root.join("outside.pdf");
        let inside = managed.join("inside.pdf");
        fs::create_dir_all(&managed).expect("create managed directory");
        fs::write(&inside, b"inside").expect("write managed file");
        fs::write(&outside, b"outside").expect("write outside file");

        assert!(is_managed_download(&inside, &managed));
        assert!(!is_managed_download(&outside, &managed));
        assert!(!is_managed_download(&managed, &managed));

        fs::remove_dir_all(root).expect("remove test root");
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
    fn recognizes_only_the_local_post_bootstrap_page() {
        assert!(is_portlet_bootstrap_url(
            &Url::parse("http://tauri.localhost/portlet-bootstrap.html").expect("URL")
        ));
        assert!(!is_portlet_bootstrap_url(
            &Url::parse("https://workflow.example/portlet-bootstrap.html").expect("URL")
        ));
    }

    #[test]
    fn identifies_each_portlet_load_generation() {
        let url = portlet_bootstrap_url(42).expect("bootstrap URL");

        assert_eq!(portlet_load_generation(&url), Some(42));
        assert_eq!(
            portlet_load_generation(
                &Url::parse("http://tauri.localhost/portlet-bootstrap.html").expect("URL")
            ),
            Some(1)
        );
        assert_eq!(
            portlet_load_generation(
                &Url::parse("http://tauri.localhost/portlet-bootstrap.html?load=old").expect("URL")
            ),
            None
        );
    }

    #[test]
    fn reload_falls_back_to_post_only_once_for_the_latest_generation() {
        let mut load = PortletLoadState {
            active_generation: 8,
            posted_generation: Some(7),
            phase: PortletLoadPhase::Posting,
        };

        assert!(!load.claim_post(7));
        assert!(load.claim_post(8));
        assert!(!load.claim_post(8));

        let current = load.begin_next_generation();
        assert_eq!(current, 9);
        assert!(load.is_active(9));
        assert_eq!(load.phase, PortletLoadPhase::Reloading);
        assert!(!load.claim_post(9));
        assert!(!load.begin_post_fallback(8));
        assert!(load.begin_post_fallback(9));
        assert!(!load.begin_post_fallback(9));
        assert!(load.claim_post(9));
        assert!(!load.claim_post(9));
    }

    #[test]
    fn authentication_report_stops_the_reload_fallback() {
        let mut load = PortletLoadState {
            active_generation: 12,
            posted_generation: Some(11),
            phase: PortletLoadPhase::Reloading,
        };

        load.mark_authenticated();

        assert_eq!(load.phase, PortletLoadPhase::Complete);
        assert!(!load.begin_post_fallback(12));
    }

    #[test]
    fn removes_nested_download_contents_but_keeps_the_root() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("cwfchecker-cleanup-{}-{nonce}", std::process::id()));
        let nested = root.join("expanded").join("nested");
        fs::create_dir_all(&nested).expect("create nested directory");
        fs::write(root.join("attachment.pdf"), b"file").expect("write top-level file");
        fs::write(nested.join("document.txt"), b"file").expect("write nested file");

        cleanup_directory(&root).expect("clean directory");

        assert!(root.is_dir());
        assert_eq!(fs::read_dir(&root).expect("read root").count(), 0);
        fs::remove_dir(root).expect("remove test root");
    }
}
