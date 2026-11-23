//! Signed, self-contained OAuth artifacts: client ids, authorization codes, and tokens.
//!
//! Each is `base64url(json claims) . base64url(HMAC-SHA256)`, so the server keeps no
//! registry of clients or tokens.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Everything this server signs, distinguished by `typ` so one kind can never be
/// replayed as another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "typ", rename_all = "snake_case")]
pub enum Claims {
    /// A dynamically registered client; the signed claims are its `client_id`.
    Client {
        name: Option<String>,
        redirect_uris: Vec<String>,
    },
    /// An authorization code, redeemable once before `exp`.
    Code {
        jti: String,
        client_id: String,
        redirect_uri: String,
        code_challenge: String,
        exp: i64,
    },
    Access {
        client_id: String,
        exp: i64,
    },
    Refresh {
        client_id: String,
        exp: i64,
    },
    /// A signed-in dashboard browser session, carried in a cookie.
    Session {
        exp: i64,
    },
}

/// Signs and verifies [`Claims`] with one HMAC key.
#[derive(Clone)]
pub struct Signer {
    key: Vec<u8>,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Signer(..)")
    }
}

impl Signer {
    pub fn new(key: &[u8]) -> Self {
        Self { key: key.to_vec() }
    }

    fn mac(&self) -> HmacSha256 {
        HmacSha256::new_from_slice(&self.key).expect("HMAC accepts keys of any length")
    }

    pub fn sign(&self, claims: &Claims) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
        let mut mac = self.mac();
        mac.update(payload.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{payload}.{signature}")
    }

    /// The claims of a token this signer produced, or `None` for anything else.
    pub fn verify(&self, token: &str) -> Option<Claims> {
        let (payload, signature) = token.split_once('.')?;
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        let mut mac = self.mac();
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature).ok()?;
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
    }

    /// Compares `given` with `expected` in constant time.
    pub fn same_secret(&self, given: &str, expected: &str) -> bool {
        let digest = |value: &str| {
            let mut mac = self.mac();
            mac.update(value.as_bytes());
            mac
        };
        let expected = digest(expected).finalize().into_bytes();
        digest(given).verify_slice(&expected).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access() -> Claims {
        Claims::Access {
            client_id: "c".into(),
            exp: 10,
        }
    }

    #[test]
    fn round_trips_and_rejects_tampering() {
        let signer = Signer::new(b"key");
        let token = signer.sign(&access());
        assert_eq!(signer.verify(&token), Some(access()));

        assert_eq!(Signer::new(b"other").verify(&token), None);
        let (payload, signature) = token.split_once('.').unwrap();
        let forged =
            URL_SAFE_NO_PAD.encode(br#"{"typ":"access","client_id":"c","exp":99999999999}"#);
        assert_eq!(signer.verify(&format!("{forged}.{signature}")), None);
        assert_eq!(signer.verify(payload), None);
    }

    #[test]
    fn compares_secrets() {
        let signer = Signer::new(b"key");
        assert!(signer.same_secret("hunter2", "hunter2"));
        assert!(!signer.same_secret("hunter3", "hunter2"));
        assert!(!signer.same_secret("", "hunter2"));
    }
}
