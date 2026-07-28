//! アプリ設定の形式、入力値の検証、レジストリへの読み書きを担当する。
//!
//! パスワードはこのモジュールでは扱わず、`credentials`モジュールを通して
//! Windows資格情報マネージャーへ保存する。

use std::io;
use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
    RegKey, RegValue,
};

pub const REGISTRY_PATH: &str = r"Software\KashiharaCity\CwfChecker";
const SCHEMA_VERSION: u32 = 1;
const LEGACY_MIGRATION_VERSION: u32 = 1;
const VALUE_AD_SERVER: &str = "AdServer";
const VALUE_CWF_ADDRESS: &str = "CwfAddress";
const VALUE_INTERVAL_MINUTES: &str = "IntervalMinutes";
const VALUE_NOTIFY_BY_BAR: &str = "NotifyByBar";
const VALUE_SHORTCUT: &str = "Shortcut";
const VALUE_USE_SAML_AUTH: &str = "UseSAMLAuth";
const VALUE_SCHEMA_VERSION: &str = "SchemaVersion";
const LEGACY_MIGRATION_VALUE: &str = "LegacyMigrationVersion";
pub const MAX_INTERVAL_MINUTES: u32 = 360;
// 同じキーにあるAppVersion・LatestVersion・MinimumVersionはversion_policyの管理値。
// 一般設定の保存失敗時にGPO値や実行版の記録を巻き戻さないため、ここには含めない。
// UseSAMLAuthも管理者配布値なので、アプリの書込み・復元対象には含めない。
const WRITTEN_SETTING_VALUE_NAMES: [&str; 6] = [
    VALUE_AD_SERVER,
    VALUE_CWF_ADDRESS,
    VALUE_INTERVAL_MINUTES,
    VALUE_NOTIFY_BY_BAR,
    VALUE_SHORTCUT,
    VALUE_SCHEMA_VERSION,
];

/// アプリが変更する各値の、書き込み前の型とバイト列。
///
/// キー全体を削除せず値単位で復元することで、GPOや将来追加された別の値を巻き込まない。
#[derive(PartialEq)]
pub struct RegistrySnapshot {
    values: Vec<(&'static str, Option<RegValue>)>,
}

fn migration_version_completed(version: u32) -> bool {
    version >= LEGACY_MIGRATION_VERSION
}

/// Rust内部と設定画面のJavaScriptで共有する、パスワード以外の設定。
///
/// `rename_all`により、Rust側の`cwf_address`はJavaScript側で`cwfAddress`になる。
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub ad_server: String,
    pub cwf_address: String,
    pub interval_minutes: u32,
    pub notify_by_bar: bool,
    pub shortcut: String,
    pub use_saml_auth: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ad_server: String::new(),
            cwf_address: String::new(),
            interval_minutes: 15,
            notify_by_bar: false,
            shortcut: "F3".to_owned(),
            use_saml_auth: false,
        }
    }
}

impl Settings {
    /// 余分な空白を除去し、保存してよい値かを検証する。
    ///
    /// この関数を読み書きの境界で必ず通すことで、レジストリ、メモリ、
    /// 設定画面の間で異なる形式の値を持たないようにしている。
    pub fn normalize(mut self) -> Result<Self, String> {
        if self.ad_server.chars().any(char::is_control) {
            return Err("AD Serverには制御文字を含めないでください。".to_owned());
        }
        self.ad_server = self.ad_server.trim().to_owned();
        self.cwf_address = self.cwf_address.trim().to_owned();
        self.shortcut = self.shortcut.trim().to_owned();

        self.interval_minutes = self.interval_minutes.clamp(15, MAX_INTERVAL_MINUTES);
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

/// 旧設定の自動移行を既に試行したか、旧ファイルとは独立したマーカーで判定する。
///
/// 旧ファイルは削除しないため、ファイルの有無だけでは同じ移行を繰り返してしまう。
pub fn legacy_migration_completed() -> io::Result<bool> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(REGISTRY_PATH, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match key.get_value::<u32, _>(LEGACY_MIGRATION_VALUE) {
        // 新しい版で移行済みなら、古い版へ戻しても再移行しない。
        Ok(version) => Ok(migration_version_completed(version)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// 自動移行の直前に、成否を問わず再試行しないためのマーカーを保存する。
pub fn mark_legacy_migration_completed() -> io::Result<()> {
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

/// 現行SchemaVersionの設定を読む。
pub fn read() -> io::Result<Option<Settings>> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    // AppVersionの登録だけでも起動時にキーが作られるため、キーの存在を設定済みの
    // 判定には使えない。保存完了マーカーのSchemaVersionを下で確認する。
    let key = match hkcu.open_subkey_with_flags(REGISTRY_PATH, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    // SchemaVersionがない場合は、書き込み途中で中断された設定として扱う。
    let schema: u32 = match key.get_value(VALUE_SCHEMA_VERSION) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if schema != SCHEMA_VERSION {
        return Ok(None);
    }

    // 個別の値が欠けた場合は既定値で補う。キー全体が完成済みかどうかは
    // 上のSchemaVersionで判定済みなので、古い版で項目が増えても読み込める。
    let interval_minutes = key
        .get_value::<u32, _>(VALUE_INTERVAL_MINUTES)
        .unwrap_or(15);
    let notify = key.get_value::<u32, _>(VALUE_NOTIFY_BY_BAR).unwrap_or(0);
    let use_saml_auth = key.get_value::<u32, _>(VALUE_USE_SAML_AUTH).unwrap_or(0) != 0;
    let settings = Settings {
        ad_server: key.get_value(VALUE_AD_SERVER).unwrap_or_default(),
        cwf_address: key.get_value(VALUE_CWF_ADDRESS).unwrap_or_default(),
        interval_minutes,
        notify_by_bar: notify != 0,
        shortcut: key
            .get_value(VALUE_SHORTCUT)
            .unwrap_or_else(|_| "F3".to_owned()),
        use_saml_auth,
    }
    // レジストリはregedit等でも変更できるため、保存時だけでなく読込時にも
    // 同じ正規化・検証を行い、メモリへ不正な設定を持ち込まない。
    .normalize()
    .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    Ok(Some(settings))
}

/// 検証済みの設定を、指定したキーへ完成マーカーを最後に付ける順序で保存する。
///
/// 本番とテストで同じ書き込み処理を使い、書き込む値がスナップショット対象から
/// 漏れていないことをテスト用キーで確認できるようにする。
fn write_at(path: &str, settings: &Settings) -> io::Result<()> {
    let settings = settings
        .clone()
        .normalize()
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey_with_flags(path, KEY_READ | KEY_WRITE)?;

    // 既存設定を更新する場合も、最初に完成マーカーを外す。
    // 途中のset_valueで失敗しても、次回起動時に半端な設定を採用しないためである。
    if let Err(error) = key.delete_value(VALUE_SCHEMA_VERSION) {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    key.set_value(VALUE_AD_SERVER, &settings.ad_server)?;
    key.set_value(VALUE_CWF_ADDRESS, &settings.cwf_address)?;
    key.set_value(VALUE_INTERVAL_MINUTES, &settings.interval_minutes)?;
    key.set_value(VALUE_NOTIFY_BY_BAR, &(settings.notify_by_bar as u32))?;
    key.set_value(VALUE_SHORTCUT, &settings.shortcut)?;

    // すべての値を書けた場合だけ、最後に完成マーカーを戻す。
    key.set_value(VALUE_SCHEMA_VERSION, &SCHEMA_VERSION)?;
    Ok(())
}

/// 検証済みの設定を、完成マーカーを最後に付ける順序で保存する。
pub fn write(settings: &Settings) -> io::Result<()> {
    write_at(REGISTRY_PATH, settings)
}

fn value_is_missing(key: &RegKey, name: &str) -> io::Result<bool> {
    match key.get_raw_value(name) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// SAMLで初期設定画面を省略するとき、未登録の利用者設定だけを既定値で補う。
///
/// 旧Electron版の移行後に呼ぶことで、移行値やGPO配布値を上書きしない。
fn write_missing_saml_defaults_at(path: &str, settings: &Settings) -> io::Result<bool> {
    if !settings.use_saml_auth || settings.cwf_address.is_empty() {
        return Ok(false);
    }
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey_with_flags(path, KEY_READ | KEY_WRITE)?;
    let mut wrote = false;
    if value_is_missing(&key, VALUE_INTERVAL_MINUTES)? {
        key.set_value(VALUE_INTERVAL_MINUTES, &settings.interval_minutes)?;
        wrote = true;
    }
    if value_is_missing(&key, VALUE_NOTIFY_BY_BAR)? {
        key.set_value(VALUE_NOTIFY_BY_BAR, &(settings.notify_by_bar as u32))?;
        wrote = true;
    }
    if value_is_missing(&key, VALUE_SHORTCUT)? {
        key.set_value(VALUE_SHORTCUT, &settings.shortcut)?;
        wrote = true;
    }
    Ok(wrote)
}

pub fn write_missing_saml_defaults(settings: &Settings) -> io::Result<bool> {
    write_missing_saml_defaults_at(REGISTRY_PATH, settings)
}

fn managed_values_equal(actual: &Settings, expected: &Settings) -> bool {
    let mut expected = expected.clone();
    expected.use_saml_auth = actual.use_saml_auth;
    actual == &expected
}

/// 設定を書いた直後に、アプリが管理する値を読み戻して一致を確認する。
pub fn write_verified(expected: &Settings) -> io::Result<()> {
    write(expected)?;
    if read()?
        .as_ref()
        .is_some_and(|actual| managed_values_equal(actual, expected))
    {
        Ok(())
    } else {
        Err(io::Error::other(
            "保存した設定と読み返した設定が一致しません。",
        ))
    }
}

/// 書き込み前の各値を、型も含めて読み取る。キーなしは全項目`None`として保持する。
fn snapshot_at(path: &str) -> io::Result<RegistrySnapshot> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(path, KEY_READ) {
        Ok(key) => Some(key),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut values = Vec::with_capacity(WRITTEN_SETTING_VALUE_NAMES.len());
    for name in WRITTEN_SETTING_VALUE_NAMES {
        let value = match key.as_ref().map(|key| key.get_raw_value(name)) {
            Some(Ok(value)) => Some(value),
            Some(Err(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Some(Err(error)) => return Err(error),
            None => None,
        };
        values.push((name, value));
    }
    Ok(RegistrySnapshot { values })
}

pub fn snapshot() -> io::Result<RegistrySnapshot> {
    snapshot_at(REGISTRY_PATH)
}

/// アプリが変更した値だけを書き込み前へ戻し、サブキー自体や未知の値は削除しない。
fn restore_snapshot_at(path: &str, snapshot: &RegistrySnapshot) -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey_with_flags(path, KEY_READ | KEY_WRITE)?;
    let mut first_error = None;
    for (name, value) in &snapshot.values {
        let result = match value {
            Some(value) => key.set_raw_value(name, value),
            None => match key.delete_value(name) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        if first_error.is_none() {
            if let Err(error) = result {
                first_error = Some(error);
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    if &snapshot_at(path)? == snapshot {
        Ok(())
    } else {
        Err(io::Error::other(
            "変更前のレジストリ値と復元後の値が一致しません。",
        ))
    }
}

pub fn restore_snapshot(snapshot: &RegistrySnapshot) -> io::Result<()> {
    restore_snapshot_at(REGISTRY_PATH, snapshot)
}

#[cfg(test)]
mod tests {
    use super::{
        managed_values_equal, migration_version_completed, restore_snapshot_at, snapshot_at,
        write_at, write_missing_saml_defaults_at, Settings, MAX_INTERVAL_MINUTES,
        WRITTEN_SETTING_VALUE_NAMES,
    };
    use std::time::SystemTime;
    use winreg::{
        enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
        RegKey,
    };

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
            cwf_address: "  https://workflow.example/XFV20/  ".to_owned(),
            interval_minutes: 0,
            shortcut: "  F3  ".to_owned(),
            ..Settings::default()
        }
        .normalize()
        .expect("normalized settings");

        assert_eq!(settings.cwf_address, "https://workflow.example/XFV20/");
        assert_eq!(settings.interval_minutes, 15);
        assert_eq!(settings.shortcut, "F3");
    }

    #[test]
    fn clamps_interval_to_supported_range() {
        let below_minimum = Settings {
            interval_minutes: 1,
            ..Settings::default()
        }
        .normalize()
        .expect("minimum interval");
        let above_maximum = Settings {
            interval_minutes: u32::MAX,
            ..Settings::default()
        }
        .normalize()
        .expect("maximum interval");

        assert_eq!(below_minimum.interval_minutes, 15);
        assert_eq!(above_maximum.interval_minutes, MAX_INTERVAL_MINUTES);
    }

    #[test]
    fn rejects_control_characters_in_ad_server() {
        let ad_server_error = Settings {
            ad_server: "AD SERVER\r".to_owned(),
            ..Settings::default()
        }
        .normalize()
        .unwrap_err();

        assert!(ad_server_error.contains("AD Server"));
    }

    #[test]
    fn defaults_to_normal_authentication() {
        assert!(!Settings::default().use_saml_auth);
    }

    #[test]
    fn verification_ignores_the_administrator_managed_saml_flag() {
        let expected = Settings::default();
        let actual = Settings {
            use_saml_auth: true,
            ..expected.clone()
        };

        assert!(managed_values_equal(&actual, &expected));
    }

    #[test]
    fn verification_detects_a_mismatch_in_an_app_managed_value() {
        let expected = Settings::default();
        let actual = Settings {
            shortcut: "F4".to_owned(),
            ..expected.clone()
        };

        assert!(!managed_values_equal(&actual, &expected));
    }

    #[test]
    fn treats_newer_migration_markers_as_completed() {
        assert!(!migration_version_completed(0));
        assert!(migration_version_completed(1));
        assert!(migration_version_completed(2));
    }

    #[test]
    fn every_written_registry_value_is_managed_by_snapshots() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let test_root = r"Software\KashiharaCity\CwfCheckerTests";
        let path = format!(r"{test_root}\written-values-{}-{nonce}", std::process::id());
        let settings = Settings {
            ad_server: "ADSERVER".to_owned(),
            cwf_address: "https://workflow.example/XFV20/".to_owned(),
            interval_minutes: 30,
            notify_by_bar: true,
            shortcut: "SHIFT+F2".to_owned(),
            use_saml_auth: false,
        };

        write_at(&path, &settings).expect("write settings to test key");
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu
            .open_subkey_with_flags(&path, KEY_READ)
            .expect("open test key");
        let unmanaged: Vec<String> = key
            .enum_values()
            .map(|entry| entry.expect("enumerate written value").0)
            .filter(|name| !WRITTEN_SETTING_VALUE_NAMES.contains(&name.as_str()))
            .collect();

        assert!(
            unmanaged.is_empty(),
            "write_at wrote values missing from WRITTEN_SETTING_VALUE_NAMES: {unmanaged:?}"
        );
        drop(key);
        hkcu.delete_subkey_all(&path).expect("remove test key");
        let _ = hkcu.delete_subkey(test_root);
    }

    #[test]
    fn fills_only_missing_saml_defaults_after_migration() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let test_root = r"Software\KashiharaCity\CwfCheckerTests";
        let path = format!(r"{test_root}\saml-defaults-{}-{nonce}", std::process::id());
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu
            .create_subkey_with_flags(&path, KEY_READ | KEY_WRITE)
            .expect("create test key");
        assert!(!write_missing_saml_defaults_at(&path, &Settings::default())
            .expect("normal authentication is unchanged"));
        key.set_value("IntervalMinutes", &30_u32)
            .expect("set migrated interval");
        key.set_value("Unmanaged", &"keep")
            .expect("set unmanaged value");
        drop(key);
        let settings = Settings {
            cwf_address: "https://workflow.example/XFV20/".to_owned(),
            use_saml_auth: true,
            ..Settings::default()
        };

        assert!(write_missing_saml_defaults_at(&path, &settings).expect("write missing defaults"));
        assert!(!write_missing_saml_defaults_at(&path, &settings).expect("second write is a no-op"));

        let key = hkcu
            .open_subkey_with_flags(&path, KEY_READ)
            .expect("open test key");
        assert_eq!(
            key.get_value::<u32, _>("IntervalMinutes")
                .expect("preserved migrated interval"),
            30
        );
        assert_eq!(
            key.get_value::<u32, _>("NotifyByBar")
                .expect("default notification"),
            0
        );
        assert_eq!(
            key.get_value::<String, _>("Shortcut")
                .expect("default shortcut"),
            "F3"
        );
        assert_eq!(
            key.get_value::<String, _>("Unmanaged")
                .expect("unmanaged value"),
            "keep"
        );
        drop(key);
        hkcu.delete_subkey_all(&path).expect("remove test key");
        let _ = hkcu.delete_subkey(test_root);
    }

    #[test]
    fn restores_only_managed_registry_values_with_their_original_types() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let test_root = r"Software\KashiharaCity\CwfCheckerTests";
        let path = format!(r"{test_root}\snapshot-{}-{nonce}", std::process::id());
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu
            .create_subkey_with_flags(&path, KEY_READ | KEY_WRITE)
            .expect("create test key");
        key.set_value("Id", &"before").expect("set ID");
        key.set_value("SchemaVersion", &1_u32).expect("set schema");
        key.set_value("Unmanaged", &"keep")
            .expect("set unmanaged value");
        let snapshot = snapshot_at(&path).expect("snapshot");

        key.set_value("Id", &"after").expect("change ID");
        key.set_value("SchemaVersion", &"wrong type")
            .expect("change schema type");
        key.set_value("AdServer", &"temporary")
            .expect("add managed value");
        restore_snapshot_at(&path, &snapshot).expect("restore");

        assert_eq!(
            key.get_value::<String, _>("Id").expect("read unmanaged ID"),
            "after"
        );
        assert_eq!(
            key.get_value::<u32, _>("SchemaVersion")
                .expect("read schema"),
            1
        );
        assert!(key.get_raw_value("AdServer").is_err());
        assert_eq!(
            key.get_value::<String, _>("Unmanaged")
                .expect("read unmanaged value"),
            "keep"
        );
        drop(key);
        hkcu.delete_subkey_all(&path).expect("remove test key");
        let _ = hkcu.delete_subkey(test_root);
    }
}
