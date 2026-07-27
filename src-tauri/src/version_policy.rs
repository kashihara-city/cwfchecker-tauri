//! 実行中バージョンの登録と、GPOで配布されるバージョン方針の判定を担当する。

use crate::settings::REGISTRY_PATH;
use std::{cmp::Ordering, io};
use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
    RegKey,
};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const VALUE_APP_VERSION: &str = "AppVersion";
pub const VALUE_LATEST_VERSION: &str = "LatestVersion";
pub const VALUE_MINIMUM_VERSION: &str = "MinimumVersion";
const MAX_VERSION_COMPONENTS: usize = 8;

#[derive(Debug, Clone, Eq, PartialEq)]
struct Version(Vec<u64>);

impl Version {
    fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("バージョンが空です。".to_owned());
        }
        let parts: Vec<_> = value.split('.').collect();
        if parts.len() > MAX_VERSION_COMPONENTS {
            return Err("バージョンの区切りが多すぎます。".to_owned());
        }
        let mut components = Vec::with_capacity(parts.len());
        for part in parts {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("バージョンはピリオド区切りの数値で指定してください。".to_owned());
            }
            components.push(
                part.parse()
                    .map_err(|_| "バージョンの数値が大きすぎます。".to_owned())?,
            );
        }
        while components.last() == Some(&0) {
            components.pop();
        }
        Ok(Self(components))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let count = self.0.len().max(other.0.len());
        (0..count)
            .map(|index| {
                self.0
                    .get(index)
                    .copied()
                    .unwrap_or(0)
                    .cmp(&other.0.get(index).copied().unwrap_or(0))
            })
            .find(|ordering| *ordering != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct PolicyStatus {
    pub update_available: bool,
    pub minimum_required: Option<String>,
}

fn evaluate(
    current: &str,
    latest: Option<&str>,
    minimum: Option<&str>,
) -> Result<PolicyStatus, String> {
    let current = Version::parse(current)?;
    let latest_version = latest.map(Version::parse).transpose()?;
    let minimum_version = minimum.map(Version::parse).transpose()?;
    Ok(PolicyStatus {
        update_available: latest_version.is_some_and(|version| current < version),
        minimum_required: minimum
            .zip(minimum_version)
            .filter(|(_, version)| current < *version)
            .map(|(value, _)| value.trim().to_owned()),
    })
}

fn read_optional_version(key: &RegKey, name: &str) -> Option<String> {
    match key.get_value::<String, _>(name) {
        Ok(value) if Version::parse(&value).is_ok() => Some(value),
        Ok(value) => {
            eprintln!("{name}の値が正しいバージョンではないため無視します: {value}");
            None
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            if error.kind() == io::ErrorKind::InvalidData {
                eprintln!("{name}の型がREG_SZではないため無視します。");
            }
            None
        }
        Err(error) => {
            eprintln!("{name}を読み込めないため無視します: {error}");
            None
        }
    }
}

fn read_status_at(path: &str) -> io::Result<PolicyStatus> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey_with_flags(path, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PolicyStatus::default()),
        Err(error) => return Err(error),
    };
    let latest = read_optional_version(&key, VALUE_LATEST_VERSION);
    let minimum = read_optional_version(&key, VALUE_MINIMUM_VERSION);
    evaluate(CURRENT_VERSION, latest.as_deref(), minimum.as_deref())
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))
}

pub fn read_status() -> io::Result<PolicyStatus> {
    read_status_at(REGISTRY_PATH)
}

fn register_current_version_at(path: &str) -> io::Result<()> {
    Version::parse(CURRENT_VERSION)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey_with_flags(path, KEY_READ | KEY_WRITE)?;
    key.set_value(VALUE_APP_VERSION, &CURRENT_VERSION)?;
    let stored: String = key.get_value(VALUE_APP_VERSION)?;
    if stored == CURRENT_VERSION {
        Ok(())
    } else {
        Err(io::Error::other(
            "登録したアプリバージョンを正しく読み返せませんでした。",
        ))
    }
}

pub fn register_current_version() -> io::Result<()> {
    register_current_version_at(REGISTRY_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn compares_numeric_version_components() {
        assert!(Version::parse("0.1.9").unwrap() < Version::parse("0.1.10").unwrap());
        assert!(Version::parse("1.9.0").unwrap() < Version::parse("2.0.0").unwrap());
        assert_eq!(
            Version::parse("1.2").unwrap(),
            Version::parse("1.2.0").unwrap()
        );
    }

    #[test]
    fn rejects_non_numeric_policy_versions() {
        for value in ["", "1..2", "v1.2.3", "1.2-beta", "1. 2"] {
            assert!(Version::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn evaluates_latest_and_minimum_versions_independently() {
        assert_eq!(
            evaluate("1.2.3", Some("1.3.0"), Some("1.0")).unwrap(),
            PolicyStatus {
                update_available: true,
                minimum_required: None,
            }
        );
        assert_eq!(
            evaluate("1.2.3", Some("1.2.3"), Some("2.0")).unwrap(),
            PolicyStatus {
                update_available: false,
                minimum_required: Some("2.0".to_owned()),
            }
        );
    }

    #[test]
    fn registers_and_reads_policy_values_in_an_isolated_key() {
        let test_root = r"Software\KashiharaCity\CwfCheckerTests";
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = format!(r"{test_root}\{nonce}");
        register_current_version_at(&path).expect("register current version");
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu
            .open_subkey_with_flags(&path, KEY_READ | KEY_WRITE)
            .unwrap();
        assert_eq!(
            key.get_value::<String, _>(VALUE_APP_VERSION).unwrap(),
            CURRENT_VERSION
        );
        key.set_value(VALUE_LATEST_VERSION, &"99.0").unwrap();
        key.set_value(VALUE_MINIMUM_VERSION, &"0.0").unwrap();
        assert_eq!(
            read_status_at(&path).unwrap(),
            PolicyStatus {
                update_available: true,
                minimum_required: None,
            }
        );
        drop(key);
        hkcu.delete_subkey_all(&path).expect("remove test key");
        let _ = hkcu.delete_subkey(test_root);
    }
}
