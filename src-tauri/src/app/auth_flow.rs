//! CWFの読込み世代、reloadからPOSTへの切替、SAML外部遷移を管理する。
//!
//! WebViewのイベントは到着順が前後するため、単純な認証中フラグではなく
//! 「どの更新要求か」を表す世代番号と組み合わせて状態を更新する。

use super::{
    build_portlet_endpoint, configured_origin, lock_error, settings_snapshot,
    show_windows_notification, AppState, MAIN_LABEL, NOTIFICATION_TITLE,
};
use crate::{credentials, migration};
use std::{
    fs,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, PhysicalPosition, PhysicalSize, WebviewWindow};
use url::Url;

const PORTLET_BOOTSTRAP_URL: &str = "http://tauri.localhost/portlet-bootstrap.html";
const EXTERNAL_AUTH_WINDOW: Duration = Duration::from_secs(30);
const AUTHENTICATION_ATTEMPT_WINDOW: Duration = Duration::from_secs(60);
const RELOAD_FALLBACK_DELAY: Duration = Duration::from_secs(5);
const LOAD_GENERATION_PREFIX: &str = "__CWFCHECKER_LOAD__";
const LOAD_GENERATION_STORAGE_KEY: &str = "__cwfcheckerLoadGeneration";
const PORTLET_POST_SCRIPT: &str = include_str!("../../scripts/portlet-post.js");
const WEBVIEW_SCRIPT: &str = include_str!("../../scripts/cwf-scan.js");

/// どのポートレット再読込みが現在有効かを表す。
///
/// `navigate()`は非同期なので、古いページの完了通知が新しい要求より後に届くことがある。
/// 単純なtrue/falseでは区別できないため、要求ごとに増える世代番号を使う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PortletLoadPhase {
    /// Cookieを使う通常reloadの結果を、最大5秒待っている。
    Reloading,
    /// reloadで認証できず、ローカルbootstrapから資格情報をPOSTしている。
    Posting,
    /// 現在の文書で認証成功DOMを確認し、POSTフォールバックが不要になった。
    Complete,
}

pub(super) struct PortletLoadState {
    /// 更新要求のたびに増える番号。遅れて届いたイベントを無視するために使う。
    active_generation: usize,
    /// 同じ世代で資格情報入りPOSTを二重実行しないための記録。
    posted_generation: Option<usize>,
    /// 現在世代がreload・POST・完了のどこまで進んだかを表す。
    phase: PortletLoadPhase,
}

impl PortletLoadState {
    /// WebView生成直後の初回POSTを表す状態を作る。
    pub(super) fn initial() -> Self {
        Self {
            active_generation: 1,
            posted_generation: None,
            phase: PortletLoadPhase::Posting,
        }
    }

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
    fn mark_authenticated(&mut self, generation: usize) -> bool {
        if !self.is_active(generation) {
            return false;
        }
        self.phase = PortletLoadPhase::Complete;
        true
    }

    /// 最新世代のPOST実行権を一度だけ取得する。
    ///
    /// `true`を受け取った呼び出し元だけがPW入りフォームを作成してよい。
    pub(super) fn claim_post(&mut self, generation: usize) -> bool {
        if !self.is_posting(generation) || self.posted_generation == Some(generation) {
            return false;
        }
        self.posted_generation = Some(generation);
        true
    }

    /// Rustが開始した最新世代のbootstrap遷移だけを許可する。
    pub(super) fn allows_bootstrap(&self, generation: usize) -> bool {
        self.is_posting(generation)
    }
}

#[derive(Clone, Copy)]
/// SAML認証中に、設定先以外のHTTPSへ移動できる期限を管理する。
pub(super) struct AuthenticationState {
    /// ポートレットのreloadまたはPOSTから認証結果の受信までの間だけtrueになる。
    in_progress: bool,
    /// 認証開始から数えて、この時刻を過ぎた外部遷移は最初の1回でも許可しない。
    attempt_deadline: Option<Instant>,
    /// 最初の外部HTTPS遷移時に設定する固定期限。遷移のたびには延長しない。
    external_deadline: Option<Instant>,
}

impl AuthenticationState {
    /// WebView生成直後の初回POSTでSAMLへ進める状態を作る。
    pub(super) fn initial(now: Instant) -> Self {
        Self {
            in_progress: true,
            attempt_deadline: Some(now + AUTHENTICATION_ATTEMPT_WINDOW),
            external_deadline: None,
        }
    }

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

    pub(super) fn finish_if_returned(&mut self) {
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

    pub(super) fn allow_external_https(&mut self, url: &Url, now: Instant) -> bool {
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

/// DOMレポートが現在の文書世代に属する場合だけ、読込み・認証状態へ反映する。
fn apply_page_report_state(
    load: &mut PortletLoadState,
    authentication: &mut AuthenticationState,
    generation: usize,
    authenticated: bool,
) -> bool {
    if authenticated {
        if !load.mark_authenticated(generation) {
            return false;
        }
        authentication.finish();
    } else {
        if !load.is_active(generation) {
            return false;
        }
        authentication.close_external_navigation();
    }
    true
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
pub(super) fn portlet_load_generation(url: &Url) -> Option<usize> {
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

fn load_generation_window_name(generation: usize) -> String {
    format!("{LOAD_GENERATION_PREFIX}{generation}")
}

/// 次の文書へ世代印を渡してから、現在ページを通常reloadするJavaScriptを作る。
fn portlet_reload_script(generation: usize) -> Result<String, String> {
    // CWFのsessionStorageは同一タブでIdPへ往復しても残る。window.nameは
    // bootstrapからCWFへ遷移する初回POSTでも印を渡すための補助として使う。
    let generation_text =
        serde_json::to_string(&generation.to_string()).map_err(|error| error.to_string())?;
    let window_name = serde_json::to_string(&load_generation_window_name(generation))
        .map_err(|error| error.to_string())?;
    let storage_key =
        serde_json::to_string(LOAD_GENERATION_STORAGE_KEY).map_err(|error| error.to_string())?;
    Ok(format!(
        "try {{ sessionStorage.setItem({storage_key}, {generation_text}); }} catch {{}} \
         window.name = {window_name}; window.location.reload();"
    ))
}

pub(super) fn portlet_post_script(
    state: &AppState,
    bootstrap_url: &str,
    generation: usize,
) -> Result<String, String> {
    let settings = settings_snapshot(state)?;
    let credential = credentials::read(credentials::TARGET)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "PWが保存されていません。".to_owned())?;
    if credential.username != settings.id {
        return Err("設定中のIDと保存済みPWのIDが一致しません。".to_owned());
    }

    let config = serde_json::json!({
        "bootstrapUrl": bootstrap_url,
        "windowName": load_generation_window_name(generation),
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
pub(super) fn begin_portlet_load(
    window: &WebviewWindow,
    state: Arc<AppState>,
) -> Result<(), String> {
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
        // 古い文書のスキャナーは生成時に以前の世代を控えているため、
        // 次の世代印を書いても、遅れて届く旧レポートの番号は変わらない。
        let reload_script = portlet_reload_script(generation)?;
        (generation, window.eval(&reload_script))
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
pub(super) fn finish_authentication_if_active_load(state: &AppState, generation: usize) {
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

pub(super) fn webview_script(origin: &str) -> String {
    // 値を手作業で引用符へ入れずJSON化することで、URL中の記号をJSコードとして
    // 解釈させない。静的な処理本体は別ファイルなので通常のJSとして編集できる。
    let config = serde_json::json!({
        "allowedOrigin": origin,
        "generationPrefix": LOAD_GENERATION_PREFIX,
        "generationStorageKey": LOAD_GENERATION_STORAGE_KEY,
    });
    format!("({WEBVIEW_SCRIPT})({config});")
}

#[derive(Debug)]
/// WebView内のJavaScriptがページを調査した結果。
pub(super) struct PageReport {
    /// このレポートを作った文書の更新世代。
    generation: usize,
    decision_count: usize,
    auth_count: usize,
    image_count: usize,
    content_height: usize,
    count_text: String,
}

/// document.titleへ一時的に載せたDOM調査結果を、安全な値へ変換する。
pub(super) fn parse_report_title(title: &str) -> Option<PageReport> {
    let mut parts = title.strip_prefix("__CWFCHECKER_REPORT__|")?.splitn(6, '|');
    let generation: usize = parts.next()?.parse().ok()?;
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
        generation,
        decision_count,
        auth_count,
        image_count,
        content_height,
        count_text,
    })
}

/// WebViewから受けた調査結果を、認証状態、ウィンドウサイズ、通知へ反映する。
pub(super) fn process_page_report(
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
    {
        // 世代確認と認証状態の更新を同じ区間で行い、確認直後に始まった
        // 新しいreloadを古い文書のレポートで変更しない。
        let mut load = state.portlet_load.lock().map_err(|_| lock_error())?;
        let mut authentication = state.authentication.lock().map_err(|_| lock_error())?;
        // 認証成功なら5秒タイマーを止め、未認証なら外部遷移だけ閉じる。
        if !apply_page_report_state(
            &mut load,
            &mut authentication,
            report.generation,
            report.auth_count > 0,
        ) {
            return Ok(());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rendered_content_height() {
        let report =
            parse_report_title("__CWFCHECKER_REPORT__|42|3|1|2|746|3").expect("valid report");

        assert_eq!(report.generation, 42);
        assert_eq!(report.decision_count, 3);
        assert_eq!(report.auth_count, 1);
        assert_eq!(report.image_count, 2);
        assert_eq!(report.content_height, 746);
        assert_eq!(report.count_text, "3");
    }

    #[test]
    fn sanitizes_text_received_through_the_document_title() {
        let long_text = format!("{}\nignored", "9".repeat(40));
        let title = format!("__CWFCHECKER_REPORT__|7|3|0|0|0|{long_text}");
        let report = parse_report_title(&title).expect("valid report");

        assert_eq!(report.count_text, "9".repeat(32));
    }

    #[test]
    fn generated_script_reports_each_document_only_once() {
        let script = webview_script("https://workflow.example");
        let reload_script = portlet_reload_script(42).expect("reload script");
        assert!(script.contains("\"allowedOrigin\":\"https://workflow.example\""));
        assert!(script.contains(LOAD_GENERATION_PREFIX));
        assert!(script.contains("sessionStorage"));
        assert!(script.contains("window.__cwfScanRunning || window.__cwfScanReported"));
        assert!(script.contains("window.__cwfScanReported = true;"));
        assert!(reload_script.contains("\"42\""));
        assert!(reload_script.contains("window.location.reload()"));
        assert!(PORTLET_POST_SCRIPT.contains("window.name = config.windowName"));
        assert!(PORTLET_POST_SCRIPT.contains("form.submit()"));
        assert!(PORTLET_POST_SCRIPT.contains("form.method = \"post\""));
        assert!(!PORTLET_POST_SCRIPT.contains("window.location.replace"));
    }

    #[test]
    fn allows_external_https_for_thirty_seconds_without_extending_the_deadline() {
        let start = Instant::now();
        let mut authentication = AuthenticationState::initial(start);
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
    }

    #[test]
    fn recognizes_only_the_local_post_bootstrap_page() {
        let valid = portlet_bootstrap_url(7).expect("bootstrap URL");
        assert_eq!(portlet_load_generation(&valid), Some(7));
        assert_eq!(
            portlet_load_generation(
                &Url::parse(PORTLET_BOOTSTRAP_URL).expect("initial bootstrap URL")
            ),
            Some(1)
        );
        assert_eq!(
            portlet_load_generation(
                &Url::parse("http://tauri.localhost/portlet-bootstrap.html?load=bad")
                    .expect("invalid-generation bootstrap URL")
            ),
            None
        );
        assert_eq!(
            portlet_load_generation(
                &Url::parse("https://tauri.localhost/portlet-bootstrap.html?load=7")
                    .expect("HTTPS bootstrap-like URL")
            ),
            None
        );
    }

    #[test]
    fn identifies_each_portlet_load_generation() {
        let first = portlet_bootstrap_url(2).expect("first bootstrap URL");
        let second = portlet_bootstrap_url(3).expect("second bootstrap URL");
        assert_eq!(portlet_load_generation(&first), Some(2));
        assert_eq!(portlet_load_generation(&second), Some(3));
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
        let start = Instant::now();
        let mut load = PortletLoadState {
            active_generation: 12,
            posted_generation: Some(11),
            phase: PortletLoadPhase::Reloading,
        };
        let mut authentication = AuthenticationState::initial(start);

        // 古い文書の未認証レポートは、新しい世代の外部遷移許可を閉じない。
        assert!(!apply_page_report_state(
            &mut load,
            &mut authentication,
            11,
            false
        ));
        assert!(authentication.in_progress);
        assert_eq!(load.phase, PortletLoadPhase::Reloading);
        assert!(apply_page_report_state(
            &mut load,
            &mut authentication,
            12,
            true
        ));
        assert_eq!(load.phase, PortletLoadPhase::Complete);
        assert!(!authentication.allow_external_https(
            &Url::parse("https://idp.example/login").expect("IdP URL"),
            start
        ));
        assert!(!load.begin_post_fallback(12));
    }
}
