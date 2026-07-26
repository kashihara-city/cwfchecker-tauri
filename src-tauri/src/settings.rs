//! アプリ設定の形式、入力値の検証、レジストリへの読み書きを担当する。
//!
//! パスワードはこのモジュールでは扱わず、`credentials`モジュールを通して
//! Windows資格情報マネージャーへ保存する。

use serde::{Deserialize, Serialize};
use std::io;
use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
    RegKey,
};

pub const REGISTRY_PATH: &str = r"Software\KashiharaCity\CwfChecker";
const SCHEMA_VERSION: u32 = 1;
const LEGACY_MIGRATION_VERSION: u32 = 1;
const LEGACY_MIGRATION_VALUE: &str = "LegacyMigrationVersion";
pub const SAML_ID: &str = "SAML";

/// Rust内部と設定画面のJavaScriptで共有する、パスワード以外の設定。
///
/// `rename_all`により、Rust側の`cwf_address`はJavaScript側で`cwfAddress`になる。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub id: String,
    pub ad_server: String,
    pub cwf_address: String,
    pub interval_minutes: u32,
    pub notify_by_bar: bool,
    pub shortcut: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            id: String::new(),
            ad_server: String::new(),
            cwf_address: String::new(),
            interval_minutes: 15,
            notify_by_bar: false,
            shortcut: "F3".to_owned(),
        }
    }
}

impl Settings {
    /// `SAML`は実在する資格情報ではなく、CWF側でSAMLを開始するための予約ID。
    pub fn uses_saml(&self) -> bool {
        self.id == SAML_ID
    }

    /// 余分な空白を除去し、保存してよい値かを検証する。
    ///
    /// この関数を読み書きの境界で必ず通すことで、レジストリ、メモリ、
    /// 設定画面の間で異なる形式の値を持たないようにしている。
    pub fn normalize(mut self) -> Result<Self, String> {
        self.id = self.id.trim().to_owned();
        self.ad_server = self.ad_server.trim().to_owned();
        self.cwf_address = self.cwf_address.trim().to_owned();
        self.shortcut = self.shortcut.trim().to_owned();

        if self.interval_minutes < 15 {
            self.interval_minutes = 15;
        }
        if self.shortcut.is_empty() {
            self.shortcut = "F3".to_owned();
        }
        if self
            .shortcut
            .parse::<tauri_plugin_global_shortcut::Shortcut>()
            .is_err()
        {
            return Err(
                "ショートカットキーの記法が正しくありません。修飾キーとキーは「+」で区切ってください（例: SHIFT+F2）。"
                    .to_owned(),
            );
        }
        if !self.cwf_address.is_empty() {
            let url = url::Url::parse(&self.cwf_address)
                .map_err(|_| "CWFAddressが正しいURLではありません。".to_owned())?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err("CWFAddressはhttpまたはhttpsで指定してください。".to_owned());
            }
            if url.host().is_none() {
                return Err("CWFAddressにサーバー名を指定してください。".to_owned());
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err("CWFAddressにはユーザー名やパスワードを含めないでください。".to_owned());
            }
        }
        Ok(self)
    }
}

/// 旧設定を既に永続化したかを、旧ファイルとは独立したマーカーで判定する。
///
/// 旧ファイルはCWFでの認証成功まで残すため、ファイルの有無だけでは再起動時に
/// 同じ移行を繰り返してしまう。
pub fn legacy_migration_completed() -> io::Result<bool> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(REGISTRY_PATH, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match key.get_value::<u32, _>(LEGACY_MIGRATION_VALUE) {
        Ok(version) => Ok(version == LEGACY_MIGRATION_VERSION),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// 旧設定の永続化が完了した最後に、一度限りの移行マーカーを保存する。
pub fn mark_legacy_migrated() -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey_with_flags(REGISTRY_PATH, KEY_READ | KEY_WRITE)?;
    key.set_value(LEGACY_MIGRATION_VALUE, &LEGACY_MIGRATION_VERSION)?;
    match key.get_value::<u32, _>(LEGACY_MIGRATION_VALUE) {
        Ok(version) if version == LEGACY_MIGRATION_VERSION => Ok(()),
        Ok(_) => {
            let _ = key.delete_value(LEGACY_MIGRATION_VALUE);
            Err(io::Error::other(
                "旧設定の移行済みマーカーが保存値と一致しません。",
            ))
        }
        Err(error) => {
            let _ = key.delete_value(LEGACY_MIGRATION_VALUE);
            Err(error)
        }
    }
}

/// 現行SchemaVersionの設定を読む。キーなし・未完成・旧形式は`None`を返す。
pub fn read() -> io::Result<Option<Settings>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(REGISTRY_PATH, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    // SchemaVersionがない場合は、書き込み途中で中断された設定として扱う。
    let schema: u32 = match key.get_value("SchemaVersion") {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if schema != SCHEMA_VERSION {
        return Ok(None);
    }

    // 個別の値が欠けた場合は既定値で補う。キー全体が完成済みかどうかは
    // 上のSchemaVersionで判定済みなので、古い版で項目が増えても読み込める。
    let interval_minutes = key.get_value::<u32, _>("IntervalMinutes").unwrap_or(15);
    let notify = key.get_value::<u32, _>("NotifyByBar").unwrap_or(0);
    let settings = Settings {
        id: key.get_value("Id").unwrap_or_default(),
        ad_server: key.get_value("AdServer").unwrap_or_default(),
        cwf_address: key.get_value("CwfAddress").unwrap_or_default(),
        interval_minutes,
        notify_by_bar: notify != 0,
        shortcut: key
            .get_value("Shortcut")
            .unwrap_or_else(|_| "F3".to_owned()),
    }
    // レジストリはregedit等でも変更できるため、保存時だけでなく読込時にも
    // 同じ正規化・検証を行い、メモリへ不正な設定を持ち込まない。
    .normalize()
    .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    Ok(Some(settings))
}

/// 検証済みの設定を、完成マーカーを最後に付ける順序で保存する。
pub fn write(settings: &Settings) -> io::Result<()> {
    let settings = settings
        .clone()
        .normalize()
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey_with_flags(REGISTRY_PATH, KEY_READ | KEY_WRITE)?;

    // 既存設定を更新する場合も、最初に完成マーカーを外す。
    // 途中のset_valueで失敗しても、次回起動時に半端な設定を採用しないためである。
    if let Err(error) = key.delete_value("SchemaVersion") {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    key.set_value("Id", &settings.id)?;
    key.set_value("AdServer", &settings.ad_server)?;
    key.set_value("CwfAddress", &settings.cwf_address)?;
    key.set_value("IntervalMinutes", &settings.interval_minutes)?;
    key.set_value("NotifyByBar", &(settings.notify_by_bar as u32))?;
    key.set_value("Shortcut", &settings.shortcut)?;

    // すべての値を書けた場合だけ、最後に完成マーカーを戻す。
    key.set_value("SchemaVersion", &SCHEMA_VERSION)?;
    Ok(())
}

/// 保存した値をレジストリから読み直し、期待値と完全に一致するか調べる。
pub fn verify(expected: &Settings) -> io::Result<bool> {
    Ok(read()?.as_ref() == Some(expected))
}

/// 設定保存失敗時に、以前の完成済み設定または「設定なし」の状態へ戻す。
pub fn restore(previous: Option<&Settings>) -> io::Result<()> {
    if let Some(previous) = previous {
        // 単に書き戻すだけでなく、読み直した値まで一致して初めて復元成功とする。
        write(previous)?;
        if verify(previous)? {
            return Ok(());
        }
        return Err(io::Error::other(
            "変更前のアプリ設定と復元後の設定が一致しません。",
        ));
    }

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    // `None`は変更前に完成済み設定がなかったことを表す。
    // 保存途中の値を残すと次回移行の判断を乱すため、アプリ専用キーごと削除する。
    match hkcu.delete_subkey_all(REGISTRY_PATH) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, SAML_ID};

    #[test]
    fn accepts_shortcut_with_plus_separator() {
        let settings = Settings {
            shortcut: "SHIFT+F2".to_owned(),
            ..Settings::default()
        };

        assert!(settings.normalize().is_ok());
    }

    #[test]
    fn rejects_shortcut_with_space_separator() {
        let settings = Settings {
            shortcut: "SHIFT F2".to_owned(),
            ..Settings::default()
        };

        let error = settings.normalize().unwrap_err();
        assert!(error.contains("SHIFT+F2"));
    }

    #[test]
    fn rejects_credentials_embedded_in_the_server_url() {
        let settings = Settings {
            cwf_address: "https://user:password@example.invalid/XFV20/".to_owned(),
            ..Settings::default()
        };

        let error = settings.normalize().unwrap_err();
        assert!(error.contains("ユーザー名やパスワード"));
    }

    #[test]
    fn normalizes_values_loaded_from_storage() {
        let settings = Settings {
            id: "  USER  ".to_owned(),
            cwf_address: "  https://workflow.example/XFV20/  ".to_owned(),
            interval_minutes: 0,
            shortcut: "  F3  ".to_owned(),
            ..Settings::default()
        }
        .normalize()
        .expect("normalized settings");

        assert_eq!(settings.id, "USER");
        assert_eq!(settings.cwf_address, "https://workflow.example/XFV20/");
        assert_eq!(settings.interval_minutes, 15);
        assert_eq!(settings.shortcut, "F3");
    }

    #[test]
    fn recognizes_only_the_reserved_saml_id() {
        let mut settings = Settings {
            id: SAML_ID.to_owned(),
            ..Settings::default()
        };
        assert!(settings.uses_saml());

        settings.id = "saml".to_owned();
        assert!(!settings.uses_saml());
    }
}
