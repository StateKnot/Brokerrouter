use crate::error::{Error, Result};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use std::collections::HashMap;
use uuid::Uuid;

// Key IDs are included in the authenticated envelope to support rolling key rotation.
pub struct ResultCipher {
    active: String,
    keys: HashMap<String, Aes256Gcm>,
}
impl ResultCipher {
    pub fn new(active: String, encoded: HashMap<String, String>) -> Result<Self> {
        if encoded.is_empty() || encoded.len() > 16 {
            return Err(Error::Configuration);
        }
        let mut keys = HashMap::new();
        for (id, hex) in encoded {
            if id.is_empty()
                || id.len() > 32
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || hex.len() != 64
            {
                return Err(Error::Configuration);
            }
            let mut bytes = [0u8; 32];
            for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
                let pair = std::str::from_utf8(pair).map_err(|_| Error::Configuration)?;
                bytes[index] = u8::from_str_radix(pair, 16).map_err(|_| Error::Configuration)?;
            }
            keys.insert(
                id,
                Aes256Gcm::new_from_slice(&bytes).map_err(|_| Error::Configuration)?,
            );
        }
        if !keys.contains_key(&active) {
            return Err(Error::Configuration);
        }
        Ok(Self { active, keys })
    }
    pub fn from_env() -> Result<Self> {
        let active =
            std::env::var("BROKERROUTER_ACTIVE_RESULT_KEY").map_err(|_| Error::Configuration)?;
        let keys = std::env::var("BROKERROUTER_RESULT_KEYS").map_err(|_| Error::Configuration)?;
        Self::new(
            active,
            serde_json::from_str(&keys).map_err(|_| Error::Configuration)?,
        )
    }
    /// Dedicated key ring for encrypted MCP credential references. Keeping it
    /// separate from model results prevents a connector compromise from
    /// becoming historical response decryption authority (and vice versa).
    pub fn from_mcp_env() -> Result<Self> {
        let active = std::env::var("BROKERROUTER_ACTIVE_MCP_CONFIG_KEY")
            .map_err(|_| Error::Configuration)?;
        let keys =
            std::env::var("BROKERROUTER_MCP_CONFIG_KEYS").map_err(|_| Error::Configuration)?;
        Self::new(
            active,
            serde_json::from_str(&keys).map_err(|_| Error::Configuration)?,
        )
    }
    pub fn encrypt(&self, tenant: Uuid, request: Uuid, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut envelope = vec![1, self.active.len() as u8];
        envelope.extend_from_slice(self.active.as_bytes());
        let mut aad = envelope.clone();
        aad.extend_from_slice(tenant.as_bytes());
        aad.extend_from_slice(request.as_bytes());
        let ciphertext = self.keys[&self.active]
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| Error::Configuration)?;
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&ciphertext);
        Ok(envelope)
    }
    pub fn decrypt(&self, tenant: Uuid, request: Uuid, envelope: &[u8]) -> Result<Vec<u8>> {
        if envelope.len() < 30 || envelope[0] != 1 {
            return Err(Error::ResultUnavailable);
        }
        let end = 2 + usize::from(envelope[1]);
        if end > 34 || envelope.len() < end + 28 {
            return Err(Error::ResultUnavailable);
        }
        let id = std::str::from_utf8(&envelope[2..end]).map_err(|_| Error::ResultUnavailable)?;
        let cipher = self.keys.get(id).ok_or(Error::ResultUnavailable)?;
        let mut aad = envelope[..end].to_vec();
        aad.extend_from_slice(tenant.as_bytes());
        aad.extend_from_slice(request.as_bytes());
        cipher
            .decrypt(
                Nonce::from_slice(&envelope[end..end + 12]),
                Payload {
                    msg: &envelope[end + 12..],
                    aad: &aad,
                },
            )
            .map_err(|_| Error::ResultUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_results_bind_tenant_request_and_key_version() {
        let keys = HashMap::from([
            ("v1".into(), "11".repeat(32)),
            ("v2".into(), "22".repeat(32)),
        ]);
        let old = ResultCipher::new("v1".into(), keys.clone()).unwrap();
        let rotated = ResultCipher::new("v2".into(), keys).unwrap();
        let tenant = Uuid::new_v4();
        let request = Uuid::new_v4();
        let mut encrypted = old.encrypt(tenant, request, b"private response").unwrap();
        assert_eq!(
            rotated.decrypt(tenant, request, &encrypted).unwrap(),
            b"private response"
        );
        assert!(
            rotated
                .decrypt(Uuid::new_v4(), request, &encrypted)
                .is_err()
        );
        assert!(rotated.decrypt(tenant, Uuid::new_v4(), &encrypted).is_err());
        *encrypted.last_mut().unwrap() ^= 1;
        assert!(rotated.decrypt(tenant, request, &encrypted).is_err());
    }
}
