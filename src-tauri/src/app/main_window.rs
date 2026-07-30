//! CWFを表示するメインWebViewと、そこから開く案件ウィンドウを組み立てる。
//!
//! このファイルは認証・ダウンロード・UIの各機能を接続する場所であり、
//! 個々の状態遷移や安全性判定そのものは対応するモジュールへ委譲する。

use super::{
    auth_flow::{
        finish_authentication_if_active_load, parse_report_title, portlet_load_generation,
        portlet_post_script, process_page_report, webview_script,
    },
    downloads::{configure_download, open_external},
    has_allowed_origin,
    ui::{build_window_menu, handle_window_menu},
    AppState, APP_TITLE, MAIN_LABEL,
};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Instant,
};
use tauri::{
    webview::{NewWindowFeatures, NewWindowResponse, PageLoadEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, Wry,
};
use url::Url;

/// CWF内のリンクから開く案件ウィンドウを、世代を問わず同じ制限付きで作る。
///
/// 案件ウィンドウ内の`target="_blank"`や`window.open()`もこの関数へ戻すことで、
/// 子孫ウィンドウにオリジン制限やダウンロード検査が付かない経路を作らない。
fn handle_new_window(
    app: &AppHandle,
    state: Arc<AppState>,
    origin: String,
    url: Url,
    features: NewWindowFeatures,
) -> NewWindowResponse<Wry> {
    if !has_allowed_origin(&url, &origin) {
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

    let number = state.decision_counter.fetch_add(1, Ordering::Relaxed);
    let label = format!("decision-{number}");
    let menu = match build_window_menu(app, &label) {
        Ok(menu) => menu,
        Err(_) => return NewWindowResponse::Deny,
    };
    let decision_origin = origin.clone();
    let decision_download_origin = origin.clone();
    let app_for_popup = app.clone();
    let state_for_popup = state.clone();
    let origin_for_popup = origin;
    let decision = WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::External("about:blank".parse().expect("valid URL")),
    )
    .title(APP_TITLE)
    .window_features(features)
    .maximized(true)
    .data_directory(state.cache_root.clone())
    .on_navigation(move |url| {
        url.as_str() == "about:blank" || has_allowed_origin(url, &decision_origin)
    })
    .on_new_window(move |url, features| {
        handle_new_window(
            &app_for_popup,
            state_for_popup.clone(),
            origin_for_popup.clone(),
            url,
            features,
        )
    })
    .menu(menu)
    .on_menu_event(|window, event| {
        handle_window_menu(window, event.id().as_ref());
    });
    let decision = configure_download(
        decision,
        state.download_dir.clone(),
        decision_download_origin,
    );
    match decision.build() {
        Ok(window) => {
            state.decisions.fetch_add(1, Ordering::Relaxed);
            NewWindowResponse::Create { window }
        }
        Err(_) => NewWindowResponse::Deny,
    }
}

/// Create!Webフローのポートレットを表示するメインWebViewを作る。
pub(super) fn create_main_window(
    app: &AppHandle,
    state: Arc<AppState>,
    endpoint: Url,
) -> Result<(), String> {
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
                .is_ok_and(|load| load.allows_bootstrap(generation));
        }
        // 3. その他は、最初の外部HTTPS遷移から60秒間だけSAML IdP等を許可する。
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
                match portlet_post_script(&state_for_page_load, current_url.as_str(), generation) {
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
        handle_new_window(
            &app_for_popup,
            state_for_popup.clone(),
            origin_for_popup.clone(),
            url,
            features,
        )
    });
    configure_download(builder, download_dir, origin_for_main_download)
        .build()
        .map(|_| ())
        .map_err(|error| error.to_string())
}
