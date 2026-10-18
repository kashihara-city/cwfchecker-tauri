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
fn cache_root() -> PathBuf {
    std::env::temp_dir()
        .join("KashiharaCity")
        .join("CwfChecker")
        .join("WebView2")
}

/// 起動ごとに空のWebView2キャッシュを用意する。
pub(super) fn prepare_cache() -> std::io::Result<PathBuf> {
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
    use super::{cleanup_directory, is_blocked_download, is_managed_download, is_trusted_download};
    use std::{
        fs,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };
    use url::Url;

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
