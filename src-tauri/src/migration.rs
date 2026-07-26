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

/// 起動時に採用する設定と、それがレジストリへ完成保存済みかをセットで返す。
///
/// `Settings::default()`は「初回起動」と「壊れた設定からの安全な退避」の両方で使うため、
/// 値だけでは保存済みか判断できない。`persisted`がその違いを保持する。
pub struct LoadedSettings {
    pub settings: Settings,
    pub persisted: bool,
}

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

/// 元のエラーへ、失敗した補償処理の内容だけを付け足す。
///
/// 資格情報を書かなかった移行では、資格情報の復元自体が不要なので`None`を渡す。
fn with_rollback_errors(
    original: io::Error,
    registry_rollback: io::Result<()>,
    credential_rollback: Option<io::Result<()>>,
) -> io::Error {
    let mut failures = Vec::new();
    if let Err(error) = registry_rollback {
        failures.push(format!(
            "レジストリを移行前の状態へ戻せませんでした: {error}"
        ));
    }
    if let Some(Err(error)) = credential_rollback {
        failures.push(format!(
            "Windows資格情報を移行前の状態へ戻せませんでした: {error}"
        ));
    }
    if failures.is_empty() {
        // 復元に成功した場合は、アクセス拒否など元のErrorKindもそのまま返す。
        original
    } else {
        io::Error::other(format!("{original} {}", failures.join(" ")))
    }
}

/// 旧設定を基本にしつつ、既に完成している有効なCWFAddressだけは上書きしない。
fn merged_legacy_settings(
    legacy: &LegacySettings,
    existing: Option<&Settings>,
) -> Result<Settings, String> {
    let preserved_cwf_address = existing
        .map(|settings| settings.cwf_address.trim())
        .filter(|address| !address.is_empty());
    Settings {
        id: legacy.id.trim().to_owned(),
        ad_server: legacy.ad.trim().to_owned(),
        cwf_address: preserved_cwf_address
            .unwrap_or_else(|| legacy.cwfaddress.trim())
            .to_owned(),
        interval_minutes: interval(&legacy.interval),
        notify_by_bar: legacy.notifybybar,
        shortcut: if legacy.shortcut.trim().is_empty() {
            "F3".to_owned()
        } else {
            legacy.shortcut.trim().to_owned()
        },
    }
    .normalize()
}

/// 同じIDについて複数世代のPWが残っている場合の優先順位を一か所に固定する。
fn preferred_password(
    encrypted_json: Option<String>,
    legacy_keytar: Option<String>,
    current: Option<String>,
) -> Option<String> {
    encrypted_json.or(legacy_keytar).or(current)
}

fn should_migrate_password(settings: &Settings) -> bool {
    !settings.id.is_empty() && !settings.uses_saml()
}

/// 現行設定を読み、存在しなければ旧Electron版から移行する。
pub fn load_or_migrate() -> io::Result<LoadedSettings> {
    let existing_result = registry_support::report_io(
        "アプリ設定の読み込み",
        registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
        settings::read(),
    );
    let existing = match existing_result {
        Ok(settings) => settings,
        // 旧設定があれば後で修復に使用する。なければ安全な初期値から設定してもらう。
        Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
        Err(error) => return Err(error),
    };
    let migration_completed = registry_support::report_io(
        "旧設定の移行状態の読み込み",
        registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
        settings::legacy_migration_completed(),
    )?;
    if migration_completed {
        return Ok(LoadedSettings {
            persisted: existing.is_some(),
            settings: existing.unwrap_or_default(),
        });
    }

    let Some(path) = legacy_config_path().filter(|path| path.is_file()) else {
        return Ok(LoadedSettings {
            persisted: existing.is_some(),
            settings: existing.unwrap_or_default(),
        });
    };
    if fs::metadata(&path)?.len() > MAX_LEGACY_CONFIG_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "旧Electron版の設定ファイルが大きすぎます。",
        ));
    }
    let legacy: LegacySettings = serde_json::from_slice(&fs::read(&path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let migrated = merged_legacy_settings(&legacy, existing.as_ref())
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;

    // JSON内のsafeStorageを最優先し、なければ旧keytar、最後に同じIDの現行資格情報を
    // 使用する。別IDの現行資格情報を誤って流用しない。
    let migrate_password = should_migrate_password(&migrated);
    let current = if migrate_password {
        credentials::read(credentials::TARGET)?
    } else {
        None
    };
    let current_password = current
        .as_ref()
        .filter(|credential| credential.username == migrated.id)
        .map(|credential| credential.password.clone());
    let encrypted_password = if migrate_password {
        legacy
            .encpw
            .as_deref()
            .map(credentials::decrypt_electron_safe_storage)
            .transpose()?
    } else {
        None
    };
    let legacy_password = if migrate_password && encrypted_password.is_none() {
        credentials::read(&credentials::legacy_target(&migrated.id))?
            .map(|credential| credential.password)
    } else {
        None
    };
    let password = if migrate_password {
        preferred_password(encrypted_password, legacy_password, current_password)
    } else {
        None
    };

    // 後のレジストリ移行が失敗した場合、ここで書いた時だけ資格情報を復元する。
    let wrote_credential = password.is_some();
    if let Some(password) = password {
        credentials::write(credentials::TARGET, &migrated.id, &password)?;
        let verified = credentials::read(credentials::TARGET)?
            .is_some_and(|actual| actual.username == migrated.id && actual.password == password);
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
            settings::write(&migrated),
        )?;
        let verified = registry_support::report_io(
            "移行した設定の保存後確認",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            settings::verify(&migrated),
        )?;
        registry_support::require_verified(
            "移行した設定の保存後確認",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            verified,
        )?;
        registry_support::report_io(
            "旧設定の移行済みマーカーの保存",
            registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
            settings::mark_legacy_migrated(),
        )
    })();
    if let Err(error) = registry_result {
        // 2個の復元は先に両方実行し、一方の失敗で他方を試さない状態を避ける。
        let registry_rollback = settings::restore(existing.as_ref());
        let credential_rollback = wrote_credential.then(|| credentials::restore(current.as_ref()));
        return Err(with_rollback_errors(
            error,
            registry_rollback,
            credential_rollback,
        ));
    }

    // 読み返しで分かるのは保存データの一致まで。旧JSONと旧資格情報は、
    // Create!Webフロー側で認証成功を確認するまで残す。
    Ok(LoadedSettings {
        settings: migrated,
        persisted: true,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        merged_legacy_settings, preferred_password, should_migrate_password, with_rollback_errors,
        LegacySettings,
    };
    use crate::settings::Settings;
    use serde_json::json;
    use std::io;

    #[test]
    fn keeps_the_original_error_when_rollbacks_succeed() {
        let error = with_rollback_errors(
            io::Error::new(io::ErrorKind::PermissionDenied, "original"),
            Ok(()),
            Some(Ok(())),
        );

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "original");
    }

    #[test]
    fn reports_every_failed_rollback() {
        let error = with_rollback_errors(
            io::Error::other("save failed"),
            Err(io::Error::other("registry failed")),
            Some(Err(io::Error::other("credential failed"))),
        );
        let message = error.to_string();

        assert!(message.contains("registry failed"));
        assert!(message.contains("credential failed"));
    }

    #[test]
    fn preserves_an_existing_cwf_address_when_migrating_legacy_settings() {
        let existing = Settings {
            cwf_address: "https://gpo.example/XFV20/".to_owned(),
            ..Settings::default()
        };
        let legacy = LegacySettings {
            id: "legacy-user".to_owned(),
            cwfaddress: "https://legacy.example/XFV20/".to_owned(),
            interval: json!("20"),
            ..LegacySettings::default()
        };

        let merged = merged_legacy_settings(&legacy, Some(&existing)).expect("merge");

        assert_eq!(merged.id, "legacy-user");
        assert_eq!(merged.cwf_address, "https://gpo.example/XFV20/");
        assert_eq!(merged.interval_minutes, 20);
    }

    #[test]
    fn uses_the_legacy_address_when_no_existing_address_is_configured() {
        let legacy = LegacySettings {
            cwfaddress: "https://legacy.example/XFV20/".to_owned(),
            ..LegacySettings::default()
        };

        let merged = merged_legacy_settings(&legacy, None).expect("merge");

        assert_eq!(merged.cwf_address, "https://legacy.example/XFV20/");
    }

    #[test]
    fn prefers_encrypted_json_then_keytar_then_current_password() {
        assert_eq!(
            preferred_password(
                Some("json".to_owned()),
                Some("keytar".to_owned()),
                Some("current".to_owned())
            ),
            Some("json".to_owned())
        );
        assert_eq!(
            preferred_password(None, Some("keytar".to_owned()), Some("current".to_owned())),
            Some("keytar".to_owned())
        );
        assert_eq!(
            preferred_password(None, None, Some("current".to_owned())),
            Some("current".to_owned())
        );
    }

    #[test]
    fn does_not_migrate_a_password_for_an_empty_or_saml_id() {
        assert!(!should_migrate_password(&Settings::default()));
        assert!(!should_migrate_password(&Settings {
            id: crate::settings::SAML_ID.to_owned(),
            ..Settings::default()
        }));
        assert!(should_migrate_password(&Settings {
            id: "normal-user".to_owned(),
            ..Settings::default()
        }));
    }
}
