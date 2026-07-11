use std::{io, ptr};

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

pub fn protect_token(token: &str) -> Result<String> {
    if token.is_empty() {
        return Ok(String::new());
    }
    let mut bytes = token.as_bytes().to_vec();
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let description: Vec<u16> = "GiantFileExchange token\0".encode_utf16().collect();
    let succeeded = unsafe {
        CryptProtectData(
            &input,
            description.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error()).context("无法使用 Windows DPAPI 加密个人令牌");
    }
    let protected = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let encoded = STANDARD.encode(protected);
    unsafe { LocalFree(output.pbData.cast()) };
    bytes.fill(0);
    Ok(encoded)
}

pub fn unprotect_token(value: &str) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    let mut bytes = STANDARD.decode(value).context("加密令牌格式无效")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let succeeded = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error()).context("无法解密个人令牌，请重新填写");
    }
    let plain = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let result = String::from_utf8(plain.to_vec()).context("解密后的令牌不是有效文本")?;
    unsafe { LocalFree(output.pbData.cast()) };
    bytes.fill(0);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip() {
        let encrypted = protect_token("test-secret-令牌").unwrap();
        assert_ne!(encrypted, "test-secret-令牌");
        assert_eq!(unprotect_token(&encrypted).unwrap(), "test-secret-令牌");
    }
}
