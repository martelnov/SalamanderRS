//! An explicitly unconfined local administrator session requires HTTP Basic auth.
//! Never place this service on an unencrypted network: use loopback + SSH tunnel.

use axum::http::HeaderValue;
use base64::{engine::general_purpose::STANDARD, Engine as _};

pub(super) struct AdminPassword(String);

// Do not accidentally log a secret through AppState's Debug implementation.
impl std::fmt::Debug for AdminPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminPassword([redacted])")
    }
}

impl AdminPassword {
    pub(super) fn from_env() -> Result<Self, String> {
        let value = std::env::var("SALAMANDER_ADMIN_PASSWORD")
            .map_err(|_| "--unrestricted vyžaduje SALAMANDER_ADMIN_PASSWORD (min. 16 znaků).".to_owned())?;
        if value.chars().count() < 16 || (value.contains('\r') || value.contains('\n')) {
            return Err("SALAMANDER_ADMIN_PASSWORD musí mít alespoň 16 znaků a nesmí obsahovat nový řádek.".into());
        }
        Ok(Self(value))
    }

    pub(super) fn accepts(&self, header: Option<&HeaderValue>) -> bool {
        let Some(value) = header.and_then(|h| h.to_str().ok()) else { return false; };
        let Some(encoded) = value.strip_prefix("Basic ") else { return false; };
        let Ok(decoded) = STANDARD.decode(encoded) else { return false; };
        let Some(colon) = decoded.iter().position(|b| *b == b':') else { return false; };
        let user = &decoded[..colon];
        let pass = &decoded[colon + 1..];
        user == b"admin" && constant_time_equal(pass, self.0.as_bytes())
    }
}

fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut difference = 0;
    for (&left, &right) in a.iter().zip(b) { difference |= left ^ right; }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_requires_correct_user_and_secret() {
        let password = AdminPassword("an-admin-password-of-adequate-length".into());
        let good = HeaderValue::from_str(&format!(
            "Basic {}", STANDARD.encode("admin:an-admin-password-of-adequate-length")
        )).unwrap();
        let bad_user = HeaderValue::from_str(&format!(
            "Basic {}", STANDARD.encode("root:an-admin-password-of-adequate-length")
        )).unwrap();
        let bad_password = HeaderValue::from_str(&format!(
            "Basic {}", STANDARD.encode("admin:not-the-password")
        )).unwrap();
        assert!(password.accepts(Some(&good)));
        assert!(!password.accepts(Some(&bad_user)));
        assert!(!password.accepts(Some(&bad_password)));
        assert!(!password.accepts(None));
    }
}
