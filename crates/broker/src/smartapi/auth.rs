//! Credentials and TOTP for SmartAPI's unattended daily login.

use std::collections::HashMap;
use std::path::Path;

use totp_rs::{Algorithm, Secret as TotpSecret, TOTP};

use crate::error::{BrokerError, BrokerResult};
use crate::secret::Secret;

/// Everything needed to log one Angel One account in. One value per account,
/// never a global, so several users can run side by side.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub api_key: Secret,
    pub client_code: String,
    pub mpin: Secret,
    /// Base32 seed shown as text under SmartAPI's "Enable TOTP" QR code.
    pub totp_secret: Secret,
}

pub const ENV_API_KEY: &str = "ANGEL_API_KEY";
pub const ENV_CLIENT_CODE: &str = "ANGEL_CLIENT_CODE";
pub const ENV_MPIN: &str = "ANGEL_MPIN";
pub const ENV_TOTP_SECRET: &str = "ANGEL_TOTP_SECRET";

impl Credentials {
    /// Builds credentials from key/value pairs (a parsed `secrets.env`).
    pub fn from_map(vars: &HashMap<String, String>) -> BrokerResult<Self> {
        let get = |k: &str| {
            vars.get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| BrokerError::Config(format!("{k} is missing or empty")))
        };
        let creds = Credentials {
            api_key: Secret::new(get(ENV_API_KEY)?),
            client_code: get(ENV_CLIENT_CODE)?,
            mpin: Secret::new(get(ENV_MPIN)?),
            totp_secret: Secret::new(get(ENV_TOTP_SECRET)?),
        };
        // Fail at startup, not at 08:35, if the seed is malformed.
        totp_code(&creds.totp_secret, 0)?;
        Ok(creds)
    }

    /// Reads a dotenv-style file, then lets real environment variables
    /// override it (useful with systemd credentials).
    pub fn load(path: Option<&Path>) -> BrokerResult<Self> {
        let mut vars = HashMap::new();
        if let Some(path) = path {
            check_permissions(path);
            let iter =
                dotenvy::from_path_iter(path).map_err(|e| BrokerError::Config(format!("{}: {e}", path.display())))?;
            for item in iter {
                let (k, v) = item.map_err(|e| BrokerError::Config(format!("{}: {e}", path.display())))?;
                vars.insert(k, v);
            }
        }
        for k in [ENV_API_KEY, ENV_CLIENT_CODE, ENV_MPIN, ENV_TOTP_SECRET] {
            if let Ok(v) = std::env::var(k) {
                vars.insert(k.to_string(), v);
            }
        }
        Self::from_map(&vars)
    }
}

#[cfg(unix)]
fn check_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), "secrets file is readable by other users; run chmod 600");
    }
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) {}

fn totp(secret: &Secret) -> BrokerResult<TOTP> {
    let cleaned: String = secret
        .expose()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_uppercase();
    let bytes = TotpSecret::Encoded(cleaned)
        .to_bytes()
        .map_err(|_| BrokerError::Config("ANGEL_TOTP_SECRET is not valid base32".into()))?;
    // `new_unchecked`: Angel seeds can be shorter than totp-rs's 128-bit minimum.
    Ok(TOTP::new_unchecked(Algorithm::SHA1, 6, 1, 30, bytes))
}

/// The 6-digit TOTP for `unix_secs` (SHA-1, 30 s step, as SmartAPI uses).
pub fn totp_code(secret: &Secret, unix_secs: u64) -> BrokerResult<String> {
    Ok(totp(secret)?.generate(unix_secs))
}

/// Seconds left in the current 30 s TOTP window.
pub fn totp_seconds_left(unix_secs: u64) -> u64 {
    30 - unix_secs % 30
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 SHA-1 seed "12345678901234567890" in base32.
    const RFC_SEED: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn rfc6238_vectors_truncated_to_six_digits() {
        let s = Secret::new(RFC_SEED);
        assert_eq!(totp_code(&s, 59).unwrap(), "287082");
        assert_eq!(totp_code(&s, 1111111109).unwrap(), "081804");
        assert_eq!(totp_code(&s, 1234567890).unwrap(), "005924");
    }

    #[test]
    fn tolerates_lowercase_and_spaces() {
        let s = Secret::new("gezd gnbv gy3t qojq gezd gnbv gy3t qojq");
        assert_eq!(totp_code(&s, 59).unwrap(), "287082");
    }

    #[test]
    fn rejects_bad_seed() {
        assert!(totp_code(&Secret::new("not base32!"), 0).is_err());
    }

    #[test]
    fn window_math() {
        assert_eq!(totp_seconds_left(0), 30);
        assert_eq!(totp_seconds_left(29), 1);
        assert_eq!(totp_seconds_left(31), 29);
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn from_map_requires_every_field() {
        let full = [
            (ENV_API_KEY, "k"),
            (ENV_CLIENT_CODE, "A123"),
            (ENV_MPIN, "1234"),
            (ENV_TOTP_SECRET, RFC_SEED),
        ];
        let c = Credentials::from_map(&vars(&full)).unwrap();
        assert_eq!(c.client_code, "A123");
        assert!(
            !format!("{c:?}").contains("1234"),
            "debug output must not leak the MPIN"
        );
        for skip in 0..full.len() {
            let partial: Vec<_> = full
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, kv)| *kv)
                .collect();
            assert!(Credentials::from_map(&vars(&partial)).is_err());
        }
    }

    #[test]
    fn loads_dotenv_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        std::fs::write(
            &path,
            format!(
                "# comment\nANGEL_API_KEY=key\nANGEL_CLIENT_CODE=Z99\nANGEL_MPIN=0000\nANGEL_TOTP_SECRET={RFC_SEED}\n"
            ),
        )
        .unwrap();
        let c = Credentials::load(Some(&path)).unwrap();
        assert_eq!(c.api_key.expose(), "key");
    }
}
