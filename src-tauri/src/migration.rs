//! 旧Electron版の設定を、Rust版の保存形式へ一度だけ移行する。
//!
//! Rust版の有効な設定を壊さない範囲で、未移行の旧設定を一度だけ取り込む。
//! 移行元JSONは削除せず、移行の成否にかかわらず二度目の自動移行は行わない。

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
        use_saml_auth: existing.use_saml_auth,
    }
    .normalize()
}

/// 移行元から現行資格情報を更新すべき場合だけ、書き込む内容を返す。
///
/// 復号できないPWのために、別IDの有効な資格情報を空PWで上書きしない。
fn credential_update(
    current: Option<&credentials::Credential>,
    legacy_id: Option<String>,
    decrypted_password: Option<String>,
) -> Option<credentials::Credential> {
    let legacy_id = legacy_id?;
    if current.is_some_and(|credential| credential.username == legacy_id) {
        return None;
    }
    match decrypted_password {
        Some(password) => Some(credentials::Credential {
            username: legacy_id,
            password,
        }),
        None if current.is_none() => Some(credentials::Credential {
            username: legacy_id,
            password: String::new(),
        }),
        None => None,
    }
}

fn decrypt_legacy_password(path: &Path, encrypted: &str) -> Option<String> {
    let local_state = path.parent()?.join("Local State");
    credentials::decrypt_electron_safe_storage(encrypted, &local_state).ok()
}

fn resolve_migration(
    existing: Option<Settings>,
    migrated: io::Result<Settings>,
) -> (Settings, Option<io::Error>) {
    match migrated {
        Ok(settings) => (settings, None),
        Err(error) => (existing.unwrap_or_default(), Some(error)),
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

    // ID/PWは一組で現行資格情報へ移す。同じIDの現行PWは維持し、なければ
    // safeStorageを試す。復号できなくても設定移行は続け、必要なら設定画面で
    // PWの再入力を求められる状態にする。
    let current = credentials::read(credentials::TARGET)?;
    let legacy_id = credentials::normalize_id(&legacy.id, true)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    let encrypted_password = if legacy_id.is_some()
        && !current.as_ref().is_some_and(|credential| {
            legacy_id
                .as_deref()
                .is_some_and(|id| credential.username == id)
        }) {
        legacy
            .encpw
            .as_deref()
            .and_then(|value| decrypt_legacy_password(path, value))
    } else {
        None
    };
    let credential_update = credential_update(current.as_ref(), legacy_id, encrypted_password);

    let registry_snapshot = settings::snapshot()?;
    let wrote_credential = credential_update.is_some();
    if let Some(desired) = credential_update.as_ref() {
        let credential_result =
            credentials::write_verified(credentials::TARGET, &desired.username, &desired.password);
        if let Err(error) = credential_result {
            return Err(with_rollback_errors(
                error,
                Ok(()),
                Some(credentials::restore(current.as_ref())),
            ));
        }
    }

    let registry_result = registry_support::report_io(
        "旧Electron版設定の保存と確認",
        registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
        settings::write_verified(&migrated),
    );
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

    Ok(migrated)
}

/// 現行設定を読み、未移行の旧Electron版設定があれば安全な範囲で取り込む。
pub fn load_or_migrate() -> io::Result<Settings> {
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
        return Ok(existing.unwrap_or_default());
    }

    let Some(path) = legacy_config_path().filter(|path| path.is_file()) else {
        return Ok(existing.unwrap_or_default());
    };

    // 自動移行は成否を問わず一度だけにする。先にマーカーを確定することで、
    // 壊れた旧JSONによる起動ごとの再試行を防ぐ。旧JSON自体は削除しない。
    registry_support::report_io(
        "旧設定の移行済みマーカーの保存",
        registry_support::SETTINGS_REGISTRY_DISPLAY_PATH,
        settings::mark_legacy_migration_completed(),
    )?;
    let (settings, warning) =
        resolve_migration(existing.clone(), migrate_legacy(&path, existing.as_ref()));
    if let Some(error) = warning {
        registry_support::show_migration_warning(&error);
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::{
        credential_update, decrypt_legacy_password, merged_legacy_settings, resolve_migration,
        with_rollback_errors, LegacySettings,
    };
    use crate::{credentials::Credential, settings::Settings};
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
            cwf_address: "https://gpo.example/XFV20/".to_owned(),
            ..Settings::default()
        };

        let (resolved, warning) = resolve_migration(
            Some(existing.clone()),
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "broken legacy JSON",
            )),
        );

        assert_eq!(resolved, existing);
        assert_eq!(warning.expect("warning").kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn uses_default_settings_when_legacy_migration_fails_without_existing_settings() {
        let (resolved, warning) = resolve_migration(
            None,
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "broken legacy JSON",
            )),
        );

        assert_eq!(resolved, Settings::default());
        assert_eq!(warning.expect("warning").kind(), io::ErrorKind::InvalidData);
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
            ad_server: "gpo-ad".to_owned(),
            cwf_address: "https://gpo.example/XFV20/".to_owned(),
            interval_minutes: 30,
            notify_by_bar: true,
            shortcut: "F4".to_owned(),
            use_saml_auth: true,
        };

        let merged =
            merged_legacy_settings(&LegacySettings::default(), Some(&existing)).expect("merge");

        assert_eq!(merged, existing);
    }

    #[test]
    fn uses_explicit_legacy_values_except_for_the_existing_cwf_address() {
        let existing = Settings {
            ad_server: "gpo-ad".to_owned(),
            cwf_address: "https://gpo.example/XFV20/".to_owned(),
            interval_minutes: 30,
            notify_by_bar: true,
            shortcut: "F4".to_owned(),
            use_saml_auth: true,
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

        assert_eq!(merged.ad_server, "legacy-ad");
        assert_eq!(merged.cwf_address, "https://gpo.example/XFV20/");
        assert_eq!(merged.interval_minutes, 20);
        assert!(!merged.notify_by_bar);
        assert_eq!(merged.shortcut, "F5");
    }

    #[test]
    fn preserves_a_matching_current_credential() {
        assert_eq!(
            credential_update(
                Some(&Credential {
                    username: "user".to_owned(),
                    password: "current".to_owned(),
                }),
                Some("user".to_owned()),
                Some("json".to_owned()),
            ),
            None
        );
    }

    #[test]
    fn migrates_a_decrypted_password_when_the_id_changed() {
        assert_eq!(
            credential_update(
                Some(&Credential {
                    username: "current-user".to_owned(),
                    password: "current".to_owned(),
                }),
                Some("legacy-user".to_owned()),
                Some("json".to_owned()),
            ),
            Some(Credential {
                username: "legacy-user".to_owned(),
                password: "json".to_owned(),
            })
        );
    }

    #[test]
    fn does_not_overwrite_another_id_with_an_empty_password() {
        assert_eq!(
            credential_update(
                Some(&Credential {
                    username: "current-user".to_owned(),
                    password: "current".to_owned(),
                }),
                Some("legacy-user".to_owned()),
                None,
            ),
            None
        );
    }

    #[test]
    fn keeps_the_legacy_id_for_password_reentry_when_no_credential_exists() {
        assert_eq!(
            credential_update(None, Some("legacy-user".to_owned()), None),
            Some(Credential {
                username: "legacy-user".to_owned(),
                password: String::new(),
            })
        );
    }

    #[test]
    fn treats_a_legacy_password_decryption_error_as_missing() {
        assert_eq!(
            decrypt_legacy_password(
                std::path::Path::new(r"C:\legacy\config.json"),
                "not valid base64!",
            ),
            None
        );
    }
}
