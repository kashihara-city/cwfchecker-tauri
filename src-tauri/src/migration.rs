//! 旧Electron版の設定を、Rust版の保存形式へ一度だけ移行する。
//!
//! Rust版の有効な設定を壊さない範囲で、未移行の旧設定を一度だけ取り込む。
//! 旧ファイルと旧資格情報の削除は、実際の認証成功を確認した後に`app`側で行う。

use crate::{
    credentials, registry_support,
    settings::{self, Settings},
};
use serde::Deserialize;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

const MAX_LEGACY_CONFIG_SIZE: u64 = 1024 * 1024;

pub struct LoadedSettings {
    pub settings: Settings,
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
    interval: Option<serde_json::Value>,
    #[serde(default)]
    notifybybar: Option<bool>,
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

fn legacy_text_or_existing(legacy: &str, existing: &str) -> String {
    let legacy = legacy.trim();
    if legacy.is_empty() {
        existing.trim().to_owned()
    } else {
        legacy.to_owned()
    }
}

/// 旧設定を基本にしつつ、空・欠落項目は完成済み設定から補う。
///
/// CWFAddressはGPOで配布される接続先なので、有効な既存値を常に優先する。
fn merged_legacy_settings(
    legacy: &LegacySettings,
    existing: Option<&Settings>,
) -> Result<Settings, String> {
    let existing = existing.cloned().unwrap_or_default();
    Settings {
        id: legacy_text_or_existing(&legacy.id, &existing.id),
        ad_server: legacy_text_or_existing(&legacy.ad, &existing.ad_server),
        cwf_address: if existing.cwf_address.is_empty() {
            legacy.cwfaddress.trim().to_owned()
        } else {
            existing.cwf_address
        },
        interval_minutes: legacy
            .interval
            .as_ref()
            .map(interval)
            .unwrap_or(existing.interval_minutes),
        notify_by_bar: legacy.notifybybar.unwrap_or(existing.notify_by_bar),
        shortcut: legacy_text_or_existing(&legacy.shortcut, &existing.shortcut),
    }
    .normalize()
}

/// 同じIDについて複数世代のPWが残っている場合の優先順位を一か所に固定する。
fn preferred_password(
    current: Option<String>,
    encrypted_json: Option<String>,
    legacy_keytar: Option<String>,
) -> Option<String> {
    current.or(encrypted_json).or(legacy_keytar)
}

fn should_migrate_password(settings: &Settings) -> bool {
    !settings.id.is_empty() && !settings.uses_saml()
}

fn resolve_migration(
    existing: Option<Settings>,
    migrated: io::Result<Settings>,
) -> io::Result<(Settings, Option<io::Error>)> {
    match migrated {
        Ok(settings) => Ok((settings, None)),
        Err(error) => match existing {
            Some(settings) => Ok((settings, Some(error))),
            None => Err(error),
        },
    }
}

fn migrate_legacy(path: &Path, existing: Option<&Settings>) -> io::Result<Settings> {
    if fs::metadata(path)?.len() > MAX_LEGACY_CONFIG_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "旧Electron版の設定ファイルが大きすぎます。",
        ));
    }
    let legacy: LegacySettings = serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let migrated = merged_legacy_settings(&legacy, existing)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;

    // 同じIDの現行資格情報を最優先し、なければsafeStorage、旧keytarの順に試す。
    // 別IDの現行資格情報は誤って流用しない。
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
    let encrypted_password = if migrate_password && current_password.is_none() {
        legacy
            .encpw
            .as_deref()
            .map(|value| {
                let local_state = path
                    .parent()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "旧Electron版設定の親フォルダーを取得できません。",
                        )
                    })?
                    .join("Local State");
                credentials::decrypt_electron_safe_storage(value, &local_state)
            })
            .transpose()?
    } else {
        None
    };
    let legacy_password =
        if migrate_password && current_password.is_none() && encrypted_password.is_none() {
            credentials::read(&credentials::legacy_target(&migrated.id))?
                .map(|credential| credential.password)
        } else {
            None
        };
    let password = if migrate_password {
        preferred_password(
            current_password.clone(),
            encrypted_password,
            legacy_password,
        )
    } else {
        None
    };

    let registry_snapshot = settings::snapshot()?;
    // 同じ現行資格情報を選んだ場合は再書き込みせず、旧値を採用した場合だけ変更する。
    let wrote_credential = password.is_some() && current_password != password;
    if wrote_credential {
        let password = password.as_deref().expect("password checked above");
        let credential_result =
            credentials::write_verified(credentials::TARGET, &migrated.id, password);
        if let Err(error) = credential_result {
            return Err(with_rollback_errors(
                error,
                Ok(()),
                Some(credentials::restore(current.as_ref())),
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
        let registry_rollback = settings::restore_snapshot(&registry_snapshot);
        let credential_rollback = wrote_credential.then(|| credentials::restore(current.as_ref()));
        return Err(with_rollback_errors(
            error,
            registry_rollback,
            credential_rollback,
        ));
    }

    // 読み返しで分かるのは保存データの一致まで。旧JSONと旧資格情報は、
    // Create!Webフロー側で認証成功を確認するまで残す。
    Ok(migrated)
}

/// 現行設定を読み、未移行の旧Electron版設定があれば安全な範囲で取り込む。
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
            settings: existing.unwrap_or_default(),
        });
    }

    let Some(path) = legacy_config_path().filter(|path| path.is_file()) else {
        return Ok(LoadedSettings {
            settings: existing.unwrap_or_default(),
        });
    };
    let (settings, warning) =
        resolve_migration(existing.clone(), migrate_legacy(&path, existing.as_ref()))?;
    if let Some(error) = warning {
        registry_support::show_migration_warning(&error);
    }
    Ok(LoadedSettings { settings })
}

#[cfg(test)]
mod tests {
    use super::{
        merged_legacy_settings, preferred_password, resolve_migration, should_migrate_password,
        with_rollback_errors, LegacySettings,
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
    fn keeps_valid_existing_settings_when_legacy_migration_fails() {
        let existing = Settings {
            id: "current-user".to_owned(),
            cwf_address: "https://gpo.example/XFV20/".to_owned(),
            ..Settings::default()
        };

        let (resolved, warning) = resolve_migration(
            Some(existing.clone()),
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "broken legacy JSON",
            )),
        )
        .expect("fall back to existing settings");

        assert_eq!(resolved, existing);
        assert_eq!(warning.expect("warning").kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn fails_when_legacy_migration_fails_without_existing_settings() {
        let error = resolve_migration(
            None,
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "broken legacy JSON",
            )),
        )
        .expect_err("no settings to fall back to");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
            interval: Some(json!("20")),
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
    fn preserves_existing_values_for_empty_or_missing_legacy_fields() {
        let existing = Settings {
            id: crate::settings::SAML_ID.to_owned(),
            ad_server: "gpo-ad".to_owned(),
            cwf_address: "https://gpo.example/XFV20/".to_owned(),
            interval_minutes: 30,
            notify_by_bar: true,
            shortcut: "F4".to_owned(),
        };

        let merged =
            merged_legacy_settings(&LegacySettings::default(), Some(&existing)).expect("merge");

        assert_eq!(merged, existing);
    }

    #[test]
    fn uses_explicit_legacy_values_except_for_the_existing_cwf_address() {
        let existing = Settings {
            id: crate::settings::SAML_ID.to_owned(),
            ad_server: "gpo-ad".to_owned(),
            cwf_address: "https://gpo.example/XFV20/".to_owned(),
            interval_minutes: 30,
            notify_by_bar: true,
            shortcut: "F4".to_owned(),
        };
        let legacy = LegacySettings {
            id: "legacy-user".to_owned(),
            ad: "legacy-ad".to_owned(),
            cwfaddress: "https://legacy.example/XFV20/".to_owned(),
            interval: Some(json!("20")),
            notifybybar: Some(false),
            shortcut: "F5".to_owned(),
            encpw: None,
        };

        let merged = merged_legacy_settings(&legacy, Some(&existing)).expect("merge");

        assert_eq!(merged.id, "legacy-user");
        assert_eq!(merged.ad_server, "legacy-ad");
        assert_eq!(merged.cwf_address, "https://gpo.example/XFV20/");
        assert_eq!(merged.interval_minutes, 20);
        assert!(!merged.notify_by_bar);
        assert_eq!(merged.shortcut, "F5");
    }

    #[test]
    fn prefers_current_then_encrypted_json_then_keytar_password() {
        assert_eq!(
            preferred_password(
                Some("current".to_owned()),
                Some("json".to_owned()),
                Some("keytar".to_owned())
            ),
            Some("current".to_owned())
        );
        assert_eq!(
            preferred_password(None, Some("json".to_owned()), Some("keytar".to_owned())),
            Some("json".to_owned())
        );
        assert_eq!(
            preferred_password(None, None, Some("keytar".to_owned())),
            Some("keytar".to_owned())
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
