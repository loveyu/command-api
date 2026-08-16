use anyhow::{Context, Result, bail};
use pbkdf2::{
    Algorithm, Params, Pbkdf2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    sync::{Arc, OnceLock},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub const DEFAULT_PBKDF2_SHA256_ROUNDS: u32 = Params::RECOMMENDED_ROUNDS;
pub const MIN_PBKDF2_SHA256_ROUNDS: u32 = Params::MIN_ROUNDS;
pub const MAX_PBKDF2_SHA256_ROUNDS: u32 = 1_000_000;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DpapiScope {
    User,
    Machine,
}

#[derive(Clone)]
pub enum TokenVerifier {
    Sha256(Zeroizing<[u8; 32]>),
    Pbkdf2Sha256 {
        phc: Zeroizing<String>,
        successful_fingerprint: Arc<OnceLock<Zeroizing<[u8; 32]>>>,
    },
}

impl std::fmt::Debug for TokenVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TokenVerifier([REDACTED])")
    }
}

impl TokenVerifier {
    pub fn from_token(token: &str) -> Self {
        Self::Sha256(Zeroizing::new(token_fingerprint(token)))
    }

    pub fn from_pbkdf2_sha256(phc: String) -> Result<Self> {
        validate_pbkdf2_sha256(&phc)?;
        Ok(Self::Pbkdf2Sha256 {
            phc: Zeroizing::new(phc),
            successful_fingerprint: Arc::new(OnceLock::new()),
        })
    }

    pub async fn verify(&self, token: &str) -> bool {
        if token.len() > 4096 || token.contains(['\r', '\n', '\0']) {
            return false;
        }
        let supplied = token_fingerprint(token);
        match self {
            Self::Sha256(expected) => supplied.ct_eq(expected.as_ref()).into(),
            Self::Pbkdf2Sha256 {
                phc,
                successful_fingerprint,
            } => {
                if let Some(expected) = successful_fingerprint.get() {
                    return supplied.ct_eq(expected.as_ref()).into();
                }

                let phc = phc.clone();
                let token = Zeroizing::new(token.to_owned());
                let verified = tokio::task::spawn_blocking(move || {
                    let Ok(parsed) = PasswordHash::new(&phc) else {
                        return false;
                    };
                    Pbkdf2::SHA256.verify_password(token.as_bytes(), &parsed).is_ok()
                })
                .await
                .unwrap_or(false);
                if verified {
                    let _ = successful_fingerprint.set(Zeroizing::new(supplied));
                }
                verified
            }
        }
    }
}

pub fn generate_pbkdf2_sha256(token: &str, rounds: u32) -> Result<String> {
    validate_token(token)?;
    if rounds > MAX_PBKDF2_SHA256_ROUNDS {
        bail!("PBKDF2 迭代次数不能超过 {MAX_PBKDF2_SHA256_ROUNDS}");
    }
    let params =
        Params::new(rounds).map_err(|_| anyhow::anyhow!("PBKDF2 迭代次数不能少于 {MIN_PBKDF2_SHA256_ROUNDS}"))?;
    let hash = Pbkdf2::new(Algorithm::Pbkdf2Sha256, params)
        .hash_password(token.as_bytes())
        .map_err(|error| anyhow::anyhow!("无法生成 PBKDF2-HMAC-SHA256 Hash: {error}"))?;
    Ok(hash.to_string())
}

fn validate_pbkdf2_sha256(phc: &str) -> Result<()> {
    let hash =
        PasswordHash::new(phc).map_err(|error| anyhow::anyhow!("auth.token.hash 不是有效 PHC 字符串: {error}"))?;
    if hash.algorithm.as_str() != "pbkdf2-sha256" {
        bail!("auth.token.hash 必须使用 pbkdf2-sha256 算法");
    }
    if hash.salt.is_none() || hash.hash.is_none() {
        bail!("auth.token.hash 必须包含 salt 和摘要");
    }
    let params =
        Params::try_from(&hash).map_err(|error| anyhow::anyhow!("auth.token.hash 的 PBKDF2 参数无效: {error}"))?;
    if params.output_len() != Params::RECOMMENDED_OUTPUT_LENGTH {
        bail!("auth.token.hash 的摘要长度必须为 32 字节");
    }
    if params.rounds() > MAX_PBKDF2_SHA256_ROUNDS {
        bail!("auth.token.hash 的 PBKDF2 迭代次数不能超过 {MAX_PBKDF2_SHA256_ROUNDS}");
    }
    Ok(())
}

fn token_fingerprint(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
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
        let input = blob(plaintext);
        let mut output = empty_blob();
        let flags = CRYPTPROTECT_UI_FORBIDDEN
            | if matches!(scope, DpapiScope::Machine) {
                CRYPTPROTECT_LOCAL_MACHINE
            } else {
                0
            };
        let success = unsafe {
            CryptProtectData(
                &input,
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
        let input = blob(protected);
        let mut output = empty_blob();
        let success = unsafe {
            CryptUnprotectData(
                &input,
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

    #[tokio::test]
    async fn pbkdf2_sha256_verifier_accepts_only_the_original_token_and_caches_success() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let hash = generate_pbkdf2_sha256(token, MIN_PBKDF2_SHA256_ROUNDS).unwrap();
        assert!(hash.starts_with("$pbkdf2-sha256$i=1000,l=32$"));
        let verifier = TokenVerifier::from_pbkdf2_sha256(hash).unwrap();

        assert!(verifier.verify(token).await);
        assert!(
            !verifier
                .verify("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
                .await
        );
        assert!(verifier.verify(token).await);
    }

    #[test]
    fn pbkdf2_sha256_rejects_invalid_or_different_phc_algorithms() {
        assert!(TokenVerifier::from_pbkdf2_sha256("not-a-phc-string".to_owned()).is_err());
        assert!(TokenVerifier::from_pbkdf2_sha256("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$YWJjZA".to_owned()).is_err());
    }
}
