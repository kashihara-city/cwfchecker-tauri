//! レジストリ操作に共通する通知登録とエラー表示をまとめる。
//!
//! 通常設定そのものは`settings`モジュールが担当し、このモジュールは
//! 「失敗時に同じ形式のメッセージを見せる」というUI上の共通処理を受け持つ。

use std::{
    fmt::Display,
    fs, io,
    path::{Path, PathBuf},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND,
    MB_TASKMODAL,
};
use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
    RegKey,
};

pub const NOTIFICATION_REGISTRY_PATH: &str =
    r"Software\Classes\AppUserModelId\jp.lg.city.kashihara.cwfchecker";
pub const NOTIFICATION_REGISTRY_DISPLAY_PATH: &str =
    r"HKCU\Software\Classes\AppUserModelId\jp.lg.city.kashihara.cwfchecker";
pub const SETTINGS_REGISTRY_DISPLAY_PATH: &str = r"HKCU\Software\KashiharaCity\CwfChecker";

const DISPLAY_NAME: &str = "CreateWebFlowChecker";
const NOTIFICATION_ICON_NAME: &str = "CreateWebFlowChecker.notification.ico";
const NOTIFICATION_ICON: &[u8] = include_bytes!("../icons/icon.ico");

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// コンソールのないrelease版でも利用者にエラーを伝えられる共通MessageBox。
fn show_error_message(message: &str) {
    show_message(message, MB_ICONERROR);
}

fn show_message(message: &str, icon: u32) {
    let title = wide("CreateWebFlowChecker");
    let message = wide(message);
    // MessageBoxWは呼び出し中だけUTF-16配列を参照するため、ローカル変数の
    // titleとmessageは関数が戻るまで有効であればよい。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | icon | MB_TASKMODAL | MB_SETFOREGROUND,
        );
    }
}

pub fn show_update_available() {
    show_message("新しいバージョンが配信されています", MB_ICONINFORMATION);
}

fn format_minimum_version_required(current: &str, minimum: &str) -> String {
    format!("現在のバージョン {current} は、必要な最低バージョン {minimum} を満たしていません。")
}

pub fn show_minimum_version_required(current: &str, minimum: &str) {
    show_message(
        &format_minimum_version_required(current, minimum),
        MB_ICONWARNING,
    );
}

/// レジストリエラーの本文を一か所で組み立てる。
///
/// 単体テストではMessageBoxを実際に開かず、この文字列だけを検証できる。
pub fn format_registry_error(operation: &str, path: &str, error: &impl Display) -> String {
    format!("レジストリ操作に失敗しました。\n\n処理: {operation}\n場所: {path}\n詳細: {error}")
}

pub fn show_registry_error(operation: &str, path: &str, error: &impl Display) {
    show_error_message(&format_registry_error(operation, path, error));
}

/// Tauriの初期化など、レジストリ以外の致命的な起動エラーを表示する。
pub fn show_startup_error(error: &impl Display) {
    show_error_message(&format!("アプリの起動に失敗しました。\n\n詳細: {error}"));
}

/// 有効な現行設定で起動を続けられる場合に、旧設定だけを採用できなかったと知らせる。
pub fn show_migration_warning(error: &impl Display) {
    show_error_message(&format!(
        "旧Electron版の設定を移行できなかったため、現在の設定で起動します。\n\
         旧設定ファイルは削除していません。\n\n詳細: {error}"
    ));
}

pub fn report_io<T>(operation: &str, path: &str, result: io::Result<T>) -> io::Result<T> {
    result.inspect_err(|error| {
        show_registry_error(operation, path, error);
    })
}

/// 保存後の照合結果がfalseなら、通常のレジストリエラーと同じ形で報告する。
pub fn require_verified(operation: &str, path: &str, verified: bool) -> io::Result<()> {
    if verified {
        return Ok(());
    }
    let error = io::Error::other("保存した設定と読み返した設定が一致しません。");
    show_registry_error(operation, path, &error);
    Err(error)
}

/// exe横に通知用ICOがあれば再利用し、なければ埋め込みデータを書き出す。
///
/// ICOは通知の装飾なので、書き込み禁止フォルダーでもアプリを止めない。
fn ensure_icon_in(directory: &Path) -> Option<PathBuf> {
    let path = directory.join(NOTIFICATION_ICON_NAME);
    if path.is_file() {
        return Some(path);
    }
    fs::write(&path, NOTIFICATION_ICON).ok().map(|_| path)
}

fn notification_icon() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    ensure_icon_in(executable.parent()?)
}

/// Windows通知に必要なユーザー単位AUMID、表示名、任意のICOを登録する。
pub fn ensure_notification_registration() -> io::Result<()> {
    let icon = notification_icon();
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) =
        hkcu.create_subkey_with_flags(NOTIFICATION_REGISTRY_PATH, KEY_READ | KEY_WRITE)?;

    // DisplayNameだけでもWindows通知は表示できる。ICOを用意できなかった
    // 場合は古いIconUriを消し、Windowsの汎用アイコンへ戻す。
    key.set_value("DisplayName", &DISPLAY_NAME)?;
    if let Some(icon) = icon {
        key.set_value("IconUri", &icon.to_string_lossy().into_owned())?;
    } else if let Err(error) = key.delete_value("IconUri") {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_icon_in, format_minimum_version_required, format_registry_error, NOTIFICATION_ICON,
    };
    use std::{fs, io, time::SystemTime};

    fn test_directory(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("cwfchecker-{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn formats_a_common_registry_error() {
        let message = format_registry_error(
            "アプリ設定の保存",
            r"HKCU\Software\KashiharaCity\CwfChecker",
            &io::Error::new(io::ErrorKind::PermissionDenied, "アクセスが拒否されました"),
        );
        assert!(message.contains("レジストリ操作に失敗しました"));
        assert!(message.contains("アプリ設定の保存"));
        assert!(message.contains(r"HKCU\Software\KashiharaCity\CwfChecker"));
        assert!(message.contains("アクセスが拒否されました"));
    }

    #[test]
    fn formats_the_required_and_current_versions() {
        assert_eq!(
            format_minimum_version_required("0.1.4", "0.2.0"),
            "現在のバージョン 0.1.4 は、必要な最低バージョン 0.2.0 を満たしていません。"
        );
    }

    #[test]
    fn writes_the_embedded_icon_when_missing() {
        let directory = test_directory("icon-write");
        fs::create_dir(&directory).expect("create test directory");

        let icon = ensure_icon_in(&directory).expect("write icon");
        assert_eq!(fs::read(&icon).expect("read icon"), NOTIFICATION_ICON);

        fs::remove_file(icon).expect("remove icon");
        fs::remove_dir(directory).expect("remove test directory");
    }

    #[test]
    fn returns_none_when_the_icon_cannot_be_written() {
        let missing_directory = test_directory("missing-directory");
        assert!(ensure_icon_in(&missing_directory).is_none());
    }
}
