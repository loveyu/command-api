use anyhow::{Context, Result, bail};
use std::{fs, path::Path};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DpapiScope {
    User,
    Machine,
}

pub fn generate_and_protect(output: &Path, scope: DpapiScope) -> Result<String> {
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    protect_to_file(token.as_bytes(), output, scope)?;
    Ok(token)
}

pub fn protect_to_file(plaintext: &[u8], output: &Path, scope: DpapiScope) -> Result<()> {
    if plaintext.len() < 32 {
        bail!("Token 至少需要 32 字节");
    }
    if plaintext.len() > 4096 {
        bail!("Token 不能超过 4096 字节");
    }
    let protected = dpapi::protect(plaintext, scope)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_context(|| format!("无法创建密钥目录 {}", parent.display()))?;
    }
    fs::write(output, protected).with_context(|| format!("无法写入 DPAPI 密钥文件 {}", output.display()))
}

pub fn unprotect_from_file(path: &Path) -> Result<String> {
    let protected = fs::read(path).with_context(|| format!("无法读取 DPAPI 密钥文件 {}", path.display()))?;
    let plaintext = Zeroizing::new(dpapi::unprotect(&protected)?);
    let token = String::from_utf8(plaintext.to_vec()).context("DPAPI 密钥解密结果不是 UTF-8")?;
    validate_token(&token)?;
    Ok(token)
}

pub fn validate_token(token: &str) -> Result<()> {
    if token.len() < 32 {
        bail!("Token 至少需要 32 字节");
    }
    if token.len() > 4096 {
        bail!("Token 不能超过 4096 字节");
    }
    if token.contains(['\r', '\n', '\0']) {
        bail!("Token 不能包含换行符或 NUL 字符");
    }
    Ok(())
}

#[cfg(windows)]
mod dpapi {
    use super::DpapiScope;
    use anyhow::{Context, Result};
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
            CryptUnprotectData,
        },
    };

    pub fn protect(plaintext: &[u8], scope: DpapiScope) -> Result<Vec<u8>> {
        let mut input = blob(plaintext);
        let mut output = empty_blob();
        let flags = CRYPTPROTECT_UI_FORBIDDEN
            | if matches!(scope, DpapiScope::Machine) {
                CRYPTPROTECT_LOCAL_MACHINE
            } else {
                0
            };
        let success = unsafe {
            CryptProtectData(
                &mut input,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null(),
                flags,
                &mut output,
            )
        };
        if success == 0 {
            return Err(std::io::Error::last_os_error()).context("Windows DPAPI 加密失败");
        }
        copy_and_free(output)
    }

    pub fn unprotect(protected: &[u8]) -> Result<Vec<u8>> {
        let mut input = blob(protected);
        let mut output = empty_blob();
        let success = unsafe {
            CryptUnprotectData(
                &mut input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if success == 0 {
            return Err(std::io::Error::last_os_error()).context("Windows DPAPI 解密失败");
        }
        copy_and_free(output)
    }

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr().cast_mut(),
        }
    }

    const fn empty_blob() -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        }
    }

    fn copy_and_free(output: CRYPT_INTEGER_BLOB) -> Result<Vec<u8>> {
        let bytes = unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe {
            LocalFree(output.pbData.cast());
        }
        Ok(bytes)
    }
}

#[cfg(not(windows))]
mod dpapi {
    use super::DpapiScope;
    use anyhow::{Result, bail};

    pub fn protect(_plaintext: &[u8], _scope: DpapiScope) -> Result<Vec<u8>> {
        bail!("Windows DPAPI 密钥仅支持 Windows")
    }

    pub fn unprotect(_protected: &[u8]) -> Result<Vec<u8>> {
        bail!("Windows DPAPI 密钥仅支持 Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_weak_or_multiline_tokens() {
        assert!(validate_token("short").is_err());
        assert!(validate_token(&"x".repeat(32)).is_ok());
        assert!(validate_token(&("x".repeat(32) + "\n")).is_err());
    }
}
