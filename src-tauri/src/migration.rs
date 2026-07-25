//! 旧Electron版の設定を、Rust版の保存形式へ一度だけ移行する。
//!
//! Rust版のレジストリ設定が既に完成している場合は何も移行しない。
//! 旧ファイルと旧資格情報の削除は、実際の認証成功を確認した後に`app`側で行う。

use crate::{
    credentials, registry_support,
    settings::{self, Settings},
};
use serde::Deserialize;
use std::{env, fs, io, path::PathBuf};

const MAX_LEGACY_CONFIG_SIZE: u64 = 1024 * 1024;

/// Electron版のconfig.jsonで使用されていたフィールド名。
///
/// バージョンによって値が欠けても読み込めるよう、各項目にdefaultを指定する。
#[derive(Debug, Default, Deserialize)]
struct LegacySettings {
    #[serde(default)]
    id: String,
    #[serde(default)]
    ad: String,
    #[serde(default)]
    cwfaddress: String,
    #[serde(default)]
    interval: serde_json::Value,
    #[serde(default)]
    notifybybar: bool,
    #[serde(default)]
    shortcut: String,
    #[serde(default)]
    encpw: Option<String>,
}

/// 旧Electron版が使用したユーザー別config.jsonの場所を返す。
pub fn legacy_config_path() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|root| {
        PathBuf::from(root)
            .join("createwebflowchecker")
            .join("config.json")
    })
}

fn interval(value: &serde_json::Value) -> u32 {
    // Electron版には数値と文字列の両方で保存された世代がある。
    value
        .as_u64()
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
        .or_else(|| value.as_str()?.parse().ok())
        .unwrap_or(15)
        .max(15)
}

/// 現行設定を読み、存在しなければ旧Electron版から移行する。
pub fn load_or_migrate() -> io::Result<Settings> {
    if let Some(existing) = registry_support::report_io(
        "アプリ設定の読み込み",
        registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
        settings::read(),
    )? {
        return Ok(existing);
    }

    let Some(path) = legacy_config_path().filter(|path| path.is_file()) else {
        return Ok(Settings::default());
    };
    if fs::metadata(&path)?.len() > MAX_LEGACY_CONFIG_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "旧Electron版の設定ファイルが大きすぎます。",
        ));
    }
    let legacy: LegacySettings = serde_json::from_slice(&fs::read(&path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let settings = Settings {
        id: legacy.id.trim().to_owned(),
        ad_server: legacy.ad.trim().to_owned(),
        cwf_address: legacy.cwfaddress.trim().to_owned(),
        interval_minutes: interval(&legacy.interval),
        notify_by_bar: legacy.notifybybar,
        shortcut: if legacy.shortcut.trim().is_empty() {
            "F3".to_owned()
        } else {
            legacy.shortcut.trim().to_owned()
        },
    }
    .normalize()
    .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;

    // 現行ターゲットに別IDの資格情報が残っていても、そのパスワードを新しいIDへ
    // 誤って流用しない。IDが一致しなければ旧keytar、次にsafeStorageを試す。
    let current = credentials::read(credentials::TARGET)?;
    let current_password = current
        .as_ref()
        .filter(|credential| credential.username == settings.id)
        .map(|credential| credential.password.clone());
    let legacy_password = if current_password.is_none() && !settings.id.is_empty() {
        credentials::read(&credentials::legacy_target(&settings.id))?
            .map(|credential| credential.password)
    } else {
        None
    };
    let password = match (
        current_password.or(legacy_password),
        legacy.encpw.as_deref(),
    ) {
        (some @ Some(_), _) => some,
        (None, Some(encrypted)) => Some(credentials::decrypt_electron_safe_storage(encrypted)?),
        (None, None) => None,
    };

    let wrote_credential = password.is_some();
    if let Some(password) = password {
        credentials::write(credentials::TARGET, &settings.id, &password)?;
        let verified = credentials::read(credentials::TARGET)?
            .is_some_and(|actual| actual.username == settings.id && actual.password == password);
        if !verified {
            let _ = credentials::restore(current.as_ref());
            return Err(io::Error::other(
                "Windows資格情報の書き込み後検証に失敗しました。",
            ));
        }
    }

    let registry_result = (|| -> io::Result<()> {
        registry_support::report_io(
            "旧Electron版設定の移行",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            settings::write(&settings),
        )?;
        let verified = registry_support::report_io(
            "移行した設定の保存後確認",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            settings::verify(&settings),
        )?;
        registry_support::require_verified(
            "移行した設定の保存後確認",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            verified,
        )
    })();
    if let Err(error) = registry_result {
        if wrote_credential {
            credentials::restore(current.as_ref()).map_err(|rollback_error| {
                io::Error::other(format!(
                    "{error} Windows資格情報を移行前の状態へ戻せませんでした: {rollback_error}"
                ))
            })?;
        }
        return Err(error);
    }

    // 読み返しで分かるのは保存データの一致まで。旧JSONと旧資格情報は、
    // Create!Webフロー側で認証成功を確認するまで残す。
    Ok(settings)
}
