//! Windows資格情報マネージャーと旧Electron版の暗号化パスワードを扱う。
//!
//! Windows APIは生ポインタを受け取るため、このファイルに`unsafe`を隔離している。
//! 呼び出し側には通常の`String`と`io::Result`だけを公開する。

use base64::Engine;
use std::{ffi::c_void, fs, io, mem::size_of, path::Path, ptr, slice};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Credentials::{
            CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
            CRED_TYPE_GENERIC,
        },
        Cryptography::{
            BCryptCloseAlgorithmProvider, BCryptDecrypt, BCryptDestroyKey,
            BCryptGenerateSymmetricKey, BCryptGetProperty, BCryptOpenAlgorithmProvider,
            BCryptSetProperty, CryptUnprotectData, BCRYPT_AES_ALGORITHM, BCRYPT_ALG_HANDLE,
            BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO, BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
            BCRYPT_CHAINING_MODE, BCRYPT_KEY_HANDLE, BCRYPT_OBJECT_LENGTH, CRYPT_INTEGER_BLOB,
        },
    },
};

pub const TARGET: &str = "KashiharaCity.CwfChecker";
// Windowsの汎用資格情報で保存できるCredentialBlobの上限。
const MAX_CREDENTIAL_BLOB_SIZE: usize = 5 * 512;
const MAX_ELECTRON_LOCAL_STATE_SIZE: u64 = 1024 * 1024;
const MAX_ELECTRON_KEY_SIZE: usize = 1024;
const ELECTRON_KEY_PREFIX: &[u8] = b"DPAPI";
const ELECTRON_VERSION_PREFIX_LENGTH: usize = 3;
const ELECTRON_GCM_NONCE_LENGTH: usize = 12;
const ELECTRON_GCM_TAG_LENGTH: usize = 16;

/// Drop時に内容を消去する機密バイト列。
struct SensitiveBytes(Vec<u8>);

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct AlgorithmHandle(BCRYPT_ALG_HANDLE);

impl Drop for AlgorithmHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                BCryptCloseAlgorithmProvider(self.0, 0);
            }
        }
    }
}

struct KeyHandle(BCRYPT_KEY_HANDLE);

impl Drop for KeyHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                BCryptDestroyKey(self.0);
            }
        }
    }
}

/// 資格情報マネージャーから読み出したユーザー名とパスワード。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

/// 資格情報のIDを正規化し、保存や利用に適さない制御文字を拒否する。
pub fn normalize_id(id: &str, allow_empty: bool) -> Result<Option<String>, String> {
    if id.chars().any(char::is_control) {
        return Err("IDには制御文字を含めないでください。".to_owned());
    }
    let id = id.trim();
    if id.is_empty() {
        if allow_empty {
            Ok(None)
        } else {
            Err("IDを入力してください。".to_owned())
        }
    } else {
        Ok(Some(id.to_owned()))
    }
}

/// 外部から作成された不正なIDは利用せず、設定画面で再入力できる空IDへ倒す。
fn normalize_stored_id(id: &str) -> String {
    normalize_id(id, true).ok().flatten().unwrap_or_default()
}

/// Windows API用のNUL終端UTF-16文字列へ変換する。
///
/// 途中にNULがある文字列を許すと、Windows側ではそこで文字列が切れて別の
/// ターゲット名として扱われるため、入力エラーとして拒否する。
fn wide_null(value: &str) -> io::Result<Vec<u16>> {
    if value.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL文字を含む値は資格情報に使用できません。",
        ));
    }
    Ok(value.encode_utf16().chain(Some(0)).collect())
}

fn win_error() -> io::Error {
    io::Error::last_os_error()
}

/// 現行Rust版が保存したUTF-8のCredentialBlobだけを受け入れる。
fn decode_password_blob(bytes: &[u8]) -> io::Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Windows資格情報のPWがUTF-8ではありません: {error}"),
        )
    })
}

/// 指定ターゲットの汎用資格情報を読む。未登録はエラーではなく`None`を返す。
pub fn read(target: &str) -> io::Result<Option<Credential>> {
    let target = wide_null(target)?;
    let mut pointer: *mut CREDENTIALW = ptr::null_mut();
    // CredReadWが成功した場合、pointerはCredFreeで解放する必要がある。
    let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut pointer) };
    if ok == 0 {
        let error = win_error();
        if error.raw_os_error() == Some(1168) {
            return Ok(None);
        }
        return Err(error);
    }

    if pointer.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows資格情報へのポインターが空です。",
        ));
    }

    let result = unsafe {
        let item = &*pointer;
        let username = if item.UserName.is_null() {
            String::new()
        } else {
            let mut length = 0;
            while *item.UserName.add(length) != 0 {
                length += 1;
            }
            String::from_utf16_lossy(slice::from_raw_parts(item.UserName, length))
        };

        let blob_size = item.CredentialBlobSize as usize;
        if blob_size > MAX_CREDENTIAL_BLOB_SIZE {
            CredFree(pointer.cast::<c_void>());
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows資格情報のデータが保存上限を超えています。",
            ));
        }
        let bytes = if blob_size == 0 {
            &[]
        } else if item.CredentialBlob.is_null() {
            CredFree(pointer.cast::<c_void>());
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows資格情報のデータが空です。",
            ));
        } else {
            slice::from_raw_parts(item.CredentialBlob, blob_size)
        };
        // UTF-8変換は失敗し得るが、先にResultとして保持して必ずCredFreeしてから`?`で返す。
        // 先に`?`を書くと、エラー時にWindowsが確保したpointerを解放できない。
        let password = decode_password_blob(bytes);
        CredFree(pointer.cast::<c_void>());
        let username = normalize_stored_id(&username);
        Credential {
            username,
            password: password?,
        }
    };
    Ok(Some(result))
}

/// 汎用資格情報をユーザーのWindows資格情報マネージャーへ保存する。
pub fn write(target: &str, username: &str, password: &str) -> io::Result<()> {
    let mut target = wide_null(target)?;
    let mut username = wide_null(username)?;
    let mut blob = password.as_bytes().to_vec();
    if blob.len() > MAX_CREDENTIAL_BLOB_SIZE {
        blob.fill(0);
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "パスワードがWindows資格情報の保存上限を超えています。",
        ));
    }
    let blob_size = u32::try_from(blob.len()).map_err(|_| {
        blob.fill(0);
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "パスワードがWindows資格情報に保存できる長さを超えています。",
        )
    })?;

    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        CredentialBlobSize: blob_size,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: username.as_mut_ptr(),
        ..Default::default()
    };

    // Windows APIから戻ったら、成否にかかわらずRust側の一時バッファーを消去する。
    let ok = unsafe { CredWriteW(&credential, 0) };
    blob.fill(0);
    if ok == 0 {
        Err(win_error())
    } else {
        Ok(())
    }
}

/// 書き込み成功だけでなく、同じ内容を直後に読み返せることまで確認する。
pub fn write_verified(target: &str, username: &str, password: &str) -> io::Result<()> {
    write(target, username, password)?;
    let verified = read(target)?
        .is_some_and(|actual| actual.username == username && actual.password == password);
    if verified {
        Ok(())
    } else {
        Err(io::Error::other(
            "Windows資格情報の書き込み後検証に失敗しました。",
        ))
    }
}

/// 指定ターゲットを削除する。最初から存在しない場合も成功として扱う。
pub fn delete(target: &str) -> io::Result<()> {
    let target = wide_null(target)?;
    let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
    if ok == 0 {
        let error = win_error();
        if error.raw_os_error() == Some(1168) {
            Ok(())
        } else {
            Err(error)
        }
    } else {
        Ok(())
    }
}

/// 更新失敗時などに、資格情報を変更前の状態へ戻す。
pub fn restore(previous: Option<&Credential>) -> io::Result<()> {
    // 変更前に存在したなら上書きし、存在しなかったなら今回作った項目を削除する。
    match previous {
        Some(credential) => write(TARGET, &credential.username, &credential.password),
        None => delete(TARGET),
    }
}

fn cng_result(operation: &str, status: i32) -> io::Result<()> {
    if status >= 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{operation}に失敗しました（NTSTATUS: 0x{:08X}）。",
            status as u32
        )))
    }
}

fn decrypt_dpapi(encrypted: &[u8], maximum_size: usize) -> io::Result<SensitiveBytes> {
    let encrypted_size = u32::try_from(encrypted.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows DPAPIの暗号データが大きすぎます。",
        )
    })?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted_size,
        pbData: encrypted.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };

    let ok = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(win_error());
    }

    let result = if output.cbData > 0 && output.pbData.is_null() {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows DPAPIの復号結果が空です。",
        ))
    } else if output.cbData as usize > maximum_size {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows DPAPIの復号結果が保存上限を超えています。",
        ))
    } else {
        let bytes = if output.cbData == 0 {
            Vec::new()
        } else {
            unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
        };
        Ok(SensitiveBytes(bytes))
    };

    if !output.pbData.is_null() {
        unsafe {
            if output.cbData > 0 {
                ptr::write_bytes(output.pbData, 0, output.cbData as usize);
            }
            LocalFree(output.pbData.cast::<c_void>());
        }
    }
    result
}

fn read_electron_master_key(local_state_path: &Path) -> io::Result<SensitiveBytes> {
    if fs::metadata(local_state_path)?.len() > MAX_ELECTRON_LOCAL_STATE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "旧Electron版のLocal Stateが大きすぎます。",
        ));
    }
    let local_state_bytes = fs::read(local_state_path)?;
    if local_state_bytes.len() as u64 > MAX_ELECTRON_LOCAL_STATE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "旧Electron版のLocal Stateが大きすぎます。",
        ));
    }
    let local_state: serde_json::Value = serde_json::from_slice(&local_state_bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let encoded_key = local_state
        .pointer("/os_crypt/encrypted_key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "旧Electron版のLocal Stateに暗号鍵がありません。",
            )
        })?;
    let mut wrapped_key = SensitiveBytes(
        base64::engine::general_purpose::STANDARD
            .decode(encoded_key)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    );
    let protected_key = wrapped_key
        .0
        .strip_prefix(ELECTRON_KEY_PREFIX)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "旧Electron版の暗号鍵がDPAPI形式ではありません。",
            )
        })?;
    let key = decrypt_dpapi(protected_key, MAX_ELECTRON_KEY_SIZE)?;
    wrapped_key.0.fill(0);
    if key.0.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "旧Electron版のAES暗号鍵が256ビットではありません。",
        ));
    }
    Ok(key)
}

fn decrypt_aes_gcm(
    key: &[u8],
    nonce: &[u8],
    ciphertext: &[u8],
    tag: &[u8],
) -> io::Result<SensitiveBytes> {
    let mut algorithm = AlgorithmHandle(ptr::null_mut());
    cng_result("AES暗号プロバイダーの初期化", unsafe {
        BCryptOpenAlgorithmProvider(&mut algorithm.0, BCRYPT_AES_ALGORITHM, ptr::null(), 0)
    })?;

    let chaining_mode: Vec<u16> = "ChainingModeGCM".encode_utf16().chain(Some(0)).collect();
    cng_result("AES-GCMモードの設定", unsafe {
        BCryptSetProperty(
            algorithm.0,
            BCRYPT_CHAINING_MODE,
            chaining_mode.as_ptr().cast(),
            u32::try_from(chaining_mode.len() * size_of::<u16>()).expect("固定文字列のバイト長"),
            0,
        )
    })?;

    let mut object_length = 0_u32;
    let mut returned_length = 0_u32;
    cng_result("AES鍵領域サイズの取得", unsafe {
        BCryptGetProperty(
            algorithm.0,
            BCRYPT_OBJECT_LENGTH,
            (&mut object_length as *mut u32).cast(),
            u32::try_from(size_of::<u32>()).expect("u32のバイト長"),
            &mut returned_length,
            0,
        )
    })?;
    if returned_length != size_of::<u32>() as u32 || object_length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windowsから有効なAES鍵領域サイズが返されませんでした。",
        ));
    }

    let mut key_object = SensitiveBytes(vec![0; object_length as usize]);
    let mut key_handle = KeyHandle(ptr::null_mut());
    cng_result("AES鍵の生成", unsafe {
        BCryptGenerateSymmetricKey(
            algorithm.0,
            &mut key_handle.0,
            key_object.0.as_mut_ptr(),
            object_length,
            key.as_ptr(),
            u32::try_from(key.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "AES暗号鍵が大きすぎます。")
            })?,
            0,
        )
    })?;

    let mut authentication = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
        cbSize: size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
        dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
        pbNonce: nonce.as_ptr().cast_mut(),
        cbNonce: u32::try_from(nonce.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "nonceが大きすぎます。"))?,
        pbTag: tag.as_ptr().cast_mut(),
        cbTag: u32::try_from(tag.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "認証タグが大きすぎます。"))?,
        ..Default::default()
    };
    let mut plaintext = SensitiveBytes(vec![0; ciphertext.len()]);
    let mut plaintext_length = 0_u32;
    cng_result("旧Electron版パスワードのAES-GCM復号", unsafe {
        BCryptDecrypt(
            key_handle.0,
            ciphertext.as_ptr(),
            u32::try_from(ciphertext.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "暗号データが大きすぎます。")
            })?,
            (&mut authentication as *mut BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO).cast(),
            ptr::null_mut(),
            0,
            plaintext.0.as_mut_ptr(),
            u32::try_from(plaintext.0.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "復号領域が大きすぎます。")
            })?,
            &mut plaintext_length,
            0,
        )
    })?;
    if plaintext_length as usize != plaintext.0.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AES-GCMの復号結果サイズが暗号文と一致しません。",
        ));
    }
    Ok(plaintext)
}

fn decrypt_electron_v10(encrypted: &[u8], local_state_path: &Path) -> io::Result<SensitiveBytes> {
    let minimum_size =
        ELECTRON_VERSION_PREFIX_LENGTH + ELECTRON_GCM_NONCE_LENGTH + ELECTRON_GCM_TAG_LENGTH;
    if encrypted.len() < minimum_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "旧Electron版のAES-GCM暗号データが短すぎます。",
        ));
    }
    let nonce_start = ELECTRON_VERSION_PREFIX_LENGTH;
    let ciphertext_start = nonce_start + ELECTRON_GCM_NONCE_LENGTH;
    let tag_start = encrypted.len() - ELECTRON_GCM_TAG_LENGTH;
    if tag_start - ciphertext_start > MAX_CREDENTIAL_BLOB_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "復号したパスワードがWindows資格情報の保存上限を超えています。",
        ));
    }
    let key = read_electron_master_key(local_state_path)?;
    decrypt_aes_gcm(
        &key.0,
        &encrypted[nonce_start..ciphertext_start],
        &encrypted[ciphertext_start..tag_start],
        &encrypted[tag_start..],
    )
}

fn sensitive_utf8(mut bytes: SensitiveBytes) -> io::Result<String> {
    match String::from_utf8(std::mem::take(&mut bytes.0)) {
        Ok(value) => Ok(value),
        Err(error) => {
            let utf8_error = error.utf8_error();
            let mut invalid_bytes = error.into_bytes();
            invalid_bytes.fill(0);
            Err(io::Error::new(io::ErrorKind::InvalidData, utf8_error))
        }
    }
}

/// ElectronのsafeStorageが暗号化した値を、世代に応じたWindows APIで復号する。
pub fn decrypt_electron_safe_storage(
    base64_value: &str,
    local_state_path: &Path,
) -> io::Result<String> {
    let encrypted = SensitiveBytes(
        base64::engine::general_purpose::STANDARD
            .decode(base64_value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    );
    let plaintext = if encrypted.0.starts_with(b"v10") || encrypted.0.starts_with(b"v11") {
        decrypt_electron_v10(&encrypted.0, local_state_path)?
    } else {
        decrypt_dpapi(&encrypted.0, MAX_CREDENTIAL_BLOB_SIZE)?
    };
    sensitive_utf8(plaintext)
}

#[cfg(test)]
mod tests {
    use super::{decode_password_blob, decrypt_aes_gcm, normalize_stored_id, wide_null};

    #[test]
    fn accepts_only_utf8_password_blobs() {
        assert_eq!(
            decode_password_blob("pässword".as_bytes()).expect("UTF-8"),
            "pässword"
        );
        assert!(decode_password_blob(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn rejects_embedded_nul_in_windows_strings() {
        assert!(wide_null("safe").is_ok());
        assert!(wide_null("unsafe\0suffix").is_err());
    }

    #[test]
    fn treats_an_invalid_stored_id_as_empty() {
        assert_eq!(normalize_stored_id("  USER  "), "USER");
        assert_eq!(normalize_stored_id("USER\u{0007}"), "");
    }

    #[test]
    fn decrypts_and_authenticates_an_aes_256_gcm_vector() {
        // NIST SP 800-38D系の、256ビットゼロ鍵・96ビットゼロnonce・空平文。
        let key = [0; 32];
        let nonce = [0; 12];
        let tag = [
            0x53, 0x0f, 0x8a, 0xfb, 0xc7, 0x45, 0x36, 0xb9, 0xa9, 0x63, 0xb4, 0xf1, 0xc4, 0xcb,
            0x73, 0x8b,
        ];

        let plaintext = decrypt_aes_gcm(&key, &nonce, &[], &tag).expect("authenticated plaintext");
        assert!(plaintext.0.is_empty());

        let mut invalid_tag = tag;
        invalid_tag[0] ^= 1;
        assert!(decrypt_aes_gcm(&key, &nonce, &[], &invalid_tag).is_err());
    }
}
