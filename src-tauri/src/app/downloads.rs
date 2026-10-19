//! ダウンロードの保存先検査と、Windowsでの安全な自動オープンを管理する。

use super::{has_allowed_origin, is_about_blank};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::{webview::DownloadEvent, Manager, WebviewWindowBuilder};
use url::Url;
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

/// ダウンロード直後にShellExecuteで開くと危険な、実行・スクリプト系の拡張子。
///
/// CWF側の添付拡張子制限を信頼境界とし、このリストは追加防御として使う。
/// サーバー側の添付ポリシーを変更する場合は、クライアント側の判定も再評価すること。
const BLOCKED_DOWNLOAD_EXTENSIONS: &[&str] = &[
    "bat", "cmd", "com", "cpl", "dll", "exe", "hta", "jar", "jse", "js", "lnk", "msi", "msp",
    "ps1", "reg", "scr", "sys", "url", "vbe", "vbs", "wsf", "wsh",
];

/// WebView2が利用する、一時キャッシュの親フォルダーを返す。
///
/// 橿原市の他のアプリ（KSIPなど）と同じく、利用者ごとの
/// `%LOCALAPPDATA%\KashiharaCity\<アプリ名>\temp`の下に置く。
/// LocalAppDataを解決できない環境では、OSの一時フォルダーを代わりに使う。
fn cache_root(local_data_dir: Option<PathBuf>) -> PathBuf {
    local_data_dir
        .unwrap_or_else(std::env::temp_dir)
        .join("KashiharaCity")
        .join("CwfChecker")
        .join("temp")
        .join("WebView2")
}

/// 起動ごとに空のWebView2キャッシュを用意し、0.1.9以前の版の置き場所を片付ける。
///
/// `local_data_dir`は`%LOCALAPPDATA%`、`app_local_data_dir`はその下のidentifier名の
/// フォルダー（Tauriが`data_directory`未指定のWebViewに使う既定の場所）を渡す。
pub(super) fn prepare_cache(
    local_data_dir: Option<PathBuf>,
    app_local_data_dir: Option<PathBuf>,
) -> std::io::Result<PathBuf> {
    // 旧版はメイン画面と案件画面のデータを`%TEMP%`の下へ置いていた。
    // 認証状態のCookieを含み得るため、新しい場所へ移った後も残さない。
    let temp = std::env::temp_dir();
    remove_with_empty_parents(
        &temp
            .join("KashiharaCity")
            .join("CwfChecker")
            .join("WebView2"),
        &temp,
    );
    // 旧版の設定画面は`data_directory`を指定せず、Tauri既定の場所を使っていた。
    if let Some(app_local) = app_local_data_dir {
        if let Some(base) = app_local.parent() {
            remove_with_empty_parents(&app_local.join("EBWebView"), base);
        }
    }
    reset_cache(&cache_root(local_data_dir), std::process::id())
}

/// `root`を作り直し、その下に今回のプロセス用の空フォルダーを作る。
fn reset_cache(root: &Path, process_id: u32) -> std::io::Result<PathBuf> {
    // WebView2の認証状態を次回起動へ持ち越さない。単一起動プラグインにより、
    // 同時に使用中の別プロセスのキャッシュを消す状況は避けられる。
    // 終了時に消さないのは、WebView2のプロセスが閉じた後もしばらくファイルを
    // 掴んでいて、確実に消すには終了を待たせる必要があるため。
    if root.exists() {
        let _ = fs::remove_dir_all(root);
    }
    let current = root.join(process_id.to_string());
    fs::create_dir_all(&current)?;
    Ok(current)
}

/// `dir`を中身ごと消し、`base`より下の親フォルダーを空になった分だけ消す。
///
/// 親フォルダーは`remove_dir`で消すため、ほかのアプリのデータが入っていれば残る。
fn remove_with_empty_parents(dir: &Path, base: &Path) {
    if fs::remove_dir_all(dir).is_err() {
        return;
    }
    let mut parent = dir.parent();
    while let Some(current) = parent {
        if current == base || !current.starts_with(base) || fs::remove_dir(current).is_err() {
            break;
        }
        parent = current.parent();
    }
}

/// アプリ管理のダウンロードフォルダーを残し、中身だけをすべて削除する。
pub(super) fn cleanup_directory(path: &Path) -> std::io::Result<()> {
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
pub(super) fn open_external(value: &str) -> Result<(), String> {
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

/// ダウンロードを信頼してよいかを、表示中のページとダウンロードURLから判定する。
///
/// 表示中のページが設定先なら、そこから始まったダウンロードを許可する。
/// ページがまだ`about:blank`の空ウィンドウ（`window.open()`直後にフォームを
/// POSTした応答が添付だった場合など）は、空ページ自体に出所の情報がないため、
/// 代わりにダウンロードURLが設定先であることを求める。空ウィンドウは設定先の
/// ページからしか作られず（`new_window_policy`）、そこへの遷移も`on_navigation`で
/// 設定先に限られているので、外部ページが入り込む余地はない。
fn is_trusted_download(page: &Url, download: &Url, allowed_origin: &str) -> bool {
    has_allowed_origin(page, allowed_origin)
        || (is_about_blank(page) && has_allowed_origin(download, allowed_origin))
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
pub(super) fn configure_download(
    builder: WebviewWindowBuilder<'_, tauri::Wry, impl Manager<tauri::Wry>>,
    download_dir: PathBuf,
    allowed_origin: String,
) -> WebviewWindowBuilder<'_, tauri::Wry, impl Manager<tauri::Wry>> {
    builder.on_download(move |webview, event| {
        match event {
            DownloadEvent::Requested {
                url, destination, ..
            } => {
                // 「現在表示中のページ」を軸に検査し、IdPや認証中の外部ページからの
                // 自動保存・起動を防ぐ。空ウィンドウへの応答だけはダウンロードURLで補う。
                if !webview
                    .url()
                    .is_ok_and(|page| is_trusted_download(&page, &url, &allowed_origin))
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
                url,
                path: Some(path),
                success: true,
                ..
            } if !is_blocked_download(&path)
                && is_managed_download(&path, &download_dir)
                && webview
                    .url()
                    .is_ok_and(|page| is_trusted_download(&page, &url, &allowed_origin)) =>
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

#[cfg(test)]
mod tests {
    use super::{
        cache_root, cleanup_directory, is_blocked_download, is_managed_download,
        is_trusted_download, remove_with_empty_parents, reset_cache,
    };
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };
    use url::Url;

    fn unique_test_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("cwfchecker-{name}-test-{unique}"))
    }

    #[test]
    fn places_the_webview_cache_beside_other_kashihara_city_apps() {
        let local = PathBuf::from(r"C:\Users\someone\AppData\Local");
        assert_eq!(
            cache_root(Some(local.clone())),
            local.join(r"KashiharaCity\CwfChecker\temp\WebView2")
        );
        assert!(cache_root(None).starts_with(std::env::temp_dir()));
    }

    #[test]
    fn resets_only_the_webview_cache_for_the_new_process() {
        let root = unique_test_root("cache");
        let app_temp = root.join(r"KashiharaCity\CwfChecker\temp");
        let cache = app_temp.join("WebView2");
        let previous = cache.join("1000");
        let sibling = app_temp.join("other");
        fs::create_dir_all(previous.join(r"EBWebView\Default")).expect("create previous cache");
        fs::write(previous.join(r"EBWebView\Default\Cookies"), b"old").expect("write cookie");
        fs::create_dir_all(&sibling).expect("create sibling");

        let current = reset_cache(&cache, 2000).expect("reset cache");

        assert_eq!(current, cache.join("2000"));
        assert!(current.is_dir());
        assert!(!previous.exists());
        // temp\の下にWebView2以外が置かれても巻き込まない。
        assert!(sibling.is_dir());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn removes_legacy_data_and_only_the_parents_left_empty() {
        let root = unique_test_root("legacy");
        let legacy = root.join(r"KashiharaCity\CwfChecker\WebView2");
        let other_app = root.join(r"KashiharaCity\ksip");
        fs::create_dir_all(legacy.join("1000")).expect("create legacy cache");
        fs::create_dir_all(&other_app).expect("create other app folder");

        remove_with_empty_parents(&legacy, &root);

        assert!(!root.join(r"KashiharaCity\CwfChecker").exists());
        // ほかのアプリのフォルダーが残る親と、基点そのものは消さない。
        assert!(other_app.is_dir());
        assert!(root.is_dir());

        fs::remove_dir_all(&other_app).expect("remove other app folder");
        fs::create_dir_all(&legacy).expect("recreate legacy cache");
        remove_with_empty_parents(&legacy, &root);
        assert!(!root.join("KashiharaCity").exists());
        assert!(root.is_dir());
        fs::remove_dir(root).expect("remove test root");
    }

    #[test]
    fn blocks_executable_downloads_and_windows_name_tricks() {
        assert!(is_blocked_download(Path::new("payload.exe")));
        assert!(is_blocked_download(Path::new("payload.ExE. ")));
        assert!(is_blocked_download(Path::new("document.pdf:payload.exe")));
        assert!(!is_blocked_download(Path::new("document.pdf")));
    }

    #[test]
    fn trusts_downloads_from_the_configured_page_or_into_a_blank_popup_it_opened() {
        let configured = "https://cwf.example";
        let url = |value: &str| Url::parse(value).expect("test URL");
        let cwf_page = url("https://cwf.example/XFV20/receive/spf/approve_form");
        let cwf_file = url("https://cwf.example/XFV20/file");
        let external_file = url("https://idp.example/file");
        // 設定先のページから始まったダウンロードは、ファイルの行き先を問わず許可する。
        assert!(is_trusted_download(&cwf_page, &cwf_file, configured));
        assert!(is_trusted_download(&cwf_page, &external_file, configured));
        // 外部ページ（IdP等）からは、設定先のファイルでも拒否する。
        assert!(!is_trusted_download(
            &url("https://idp.example/login"),
            &cwf_file,
            configured
        ));
        // 空ウィンドウへPOSTした応答が添付になった場合は、ダウンロードURLで判定する。
        assert!(is_trusted_download(
            &url("about:blank"),
            &cwf_file,
            configured
        ));
        assert!(!is_trusted_download(
            &url("about:blank"),
            &external_file,
            configured
        ));
    }

    #[test]
    fn accepts_only_completed_downloads_inside_the_managed_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cwfchecker-download-test-{unique}"));
        let download_dir = root.join("downloads");
        let outside = root.join("outside.pdf");
        fs::create_dir_all(&download_dir).expect("create managed directory");
        fs::write(download_dir.join("inside.pdf"), b"inside").expect("write managed file");
        fs::write(&outside, b"outside").expect("write outside file");

        assert!(is_managed_download(
            &download_dir.join("inside.pdf"),
            &download_dir
        ));
        assert!(!is_managed_download(&outside, &download_dir));
        assert!(!is_managed_download(
            &download_dir.join("missing.pdf"),
            &download_dir
        ));

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn removes_nested_download_contents_but_keeps_the_root() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cwfchecker-cleanup-test-{unique}"));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("create nested directory");
        fs::write(root.join("file.pdf"), b"file").expect("write top-level file");
        fs::write(nested.join("nested.pdf"), b"nested").expect("write nested file");

        cleanup_directory(&root).expect("clean directory");

        assert!(root.exists());
        assert_eq!(fs::read_dir(&root).expect("read root").count(), 0);
        fs::remove_dir(root).expect("remove test root");
    }
}
