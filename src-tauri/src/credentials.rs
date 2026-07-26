//! Windows資格情報マネージャーと旧Electron版の暗号化パスワードを扱う。
//!
//! Windows APIは生ポインタを受け取るため、このファイルに`unsafe`を隔離している。
//! 呼び出し側には通常の`String`と`io::Result`だけを公開する。

use base64::Engine;
use std::{ffi::c_void, io, ptr, slice};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Credentials::{
            CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
            CRED_TYPE_GENERIC,
        },
        Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB},
    },
};

pub const TARGET: &str = "KashiharaCity.CwfChecker";
// Windowsの汎用資格情報で保存できるCredentialBlobの上限。
const MAX_CREDENTIAL_BLOB_SIZE: usize = 5 * 512;

/// 資格情報マネージャーから読み出したユーザー名とパスワード。
#[derive(Debug, Clone)]
pub struct Credential {
    pub username: String,
    pub password: String,
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

/// 現行Rust版と旧keytar版が保存したUTF-8のCredentialBlobだけを受け入れる。
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
        let password = decode_password_blob(bytes);
        CredFree(pointer.cast::<c_void>());
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
    match previous {
        Some(credential) => write(TARGET, &credential.username, &credential.password),
        None => delete(TARGET),
    }
}

pub fn legacy_target(id: &str) -> String {
    format!("cwfchecker/{id}")
}

/// ElectronのsafeStorageがWindows DPAPIで暗号化した値を復号する。
pub fn decrypt_electron_safe_storage(base64_value: &str) -> io::Result<String> {
    let encrypted = base64::engine::general_purpose::STANDARD
        .decode(base64_value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let encrypted_size = u32::try_from(encrypted.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "旧Electron版の暗号化パスワードが大きすぎます。",
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

    // 復号結果はWindowsが確保するため、成功後にLocalFreeで必ず解放する。
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

    if output.cbData > 0 && output.pbData.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows DPAPIの復号結果が空です。",
        ));
    }
    if output.cbData as usize > MAX_CREDENTIAL_BLOB_SIZE {
        unsafe {
            ptr::write_bytes(output.pbData, 0, output.cbData as usize);
            LocalFree(output.pbData.cast::<c_void>());
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "復号したパスワードがWindows資格情報の保存上限を超えています。",
        ));
    }
    let plaintext = unsafe {
        let output_size = output.cbData as usize;
        let bytes = if output_size == 0 {
            &[]
        } else {
            slice::from_raw_parts(output.pbData, output_size)
        };
        let value = String::from_utf8(bytes.to_vec())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        // Windowsが確保した平文バッファーを消去してから解放する。
        if output_size > 0 {
            ptr::write_bytes(output.pbData, 0, output_size);
        }
        LocalFree(output.pbData.cast::<c_void>());
        value?
    };
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::{decode_password_blob, wide_null};

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
}
