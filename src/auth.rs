// SPDX-License-Identifier: AGPL-3.0-or-later
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub id: Uuid,
    pub origin: String,
    pub token_salt: String,
    pub token_hash: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub revoked_at: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PairingSecret {
    pub value: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPairing {
    salt: String,
    hash: String,
    expires_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    grants: Vec<Grant>,
    pairing: Option<StoredPairing>,
}

#[derive(Debug)]
pub struct AuthStore {
    path: PathBuf,
    grants: HashMap<Uuid, Grant>,
    pairing: Option<StoredPairing>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random_string(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    rand::rng().fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}
fn digest(salt: &str, token: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"mb-printer-token-v1\0");
    h.update(salt);
    h.update(b"\0");
    h.update(token);
    h.finalize().into()
}

impl AuthStore {
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let store = if path.exists() {
            serde_json::from_slice::<Store>(&fs::read(&path)?).map_err(io::Error::other)?
        } else {
            Store::default()
        };
        Ok(Self {
            path,
            grants: store.grants.into_iter().map(|g| (g.id, g)).collect(),
            pairing: store.pairing,
        })
    }
    pub fn begin_pairing(&mut self, ttl: Duration) -> io::Result<PairingSecret> {
        let value = random_string(32);
        let salt = random_string(16);
        let expires_at = now().saturating_add(ttl.as_secs().min(600));
        self.pairing = Some(StoredPairing {
            hash: URL_SAFE_NO_PAD.encode(digest(&salt, &value)),
            salt,
            expires_at,
        });
        self.persist()?;
        Ok(PairingSecret { value, expires_at })
    }
    pub fn exchange(
        &mut self,
        secret: &str,
        origin: &str,
        ttl: Duration,
    ) -> io::Result<Option<(Uuid, String)>> {
        let Some(pairing) = self.pairing.clone() else {
            return Ok(None);
        };
        let expected = URL_SAFE_NO_PAD.decode(pairing.hash).unwrap_or_default();
        if pairing.expires_at < now() {
            self.pairing = None;
            self.persist()?;
            return Ok(None);
        }
        if expected
            .as_slice()
            .ct_eq(&digest(&pairing.salt, secret))
            .unwrap_u8()
            != 1
            || !valid_origin(origin)
        {
            return Ok(None);
        }
        self.pairing = None;
        self.persist()?;
        let token = random_string(32);
        let salt = random_string(16);
        let id = Uuid::new_v4();
        let grant = Grant {
            id,
            origin: origin.to_owned(),
            token_hash: URL_SAFE_NO_PAD.encode(digest(&salt, &token)),
            token_salt: salt,
            created_at: now(),
            expires_at: now().saturating_add(ttl.as_secs().min(31_536_000)),
            revoked_at: None,
        };
        self.grants.insert(id, grant);
        self.persist()?;
        Ok(Some((id, token)))
    }
    pub fn authenticate(&self, token: &str, origin: &str) -> Option<&Grant> {
        self.grants.values().find(|g| {
            g.origin == origin
                && g.revoked_at.is_none()
                && g.expires_at >= now()
                && URL_SAFE_NO_PAD
                    .decode(&g.token_hash)
                    .ok()
                    .is_some_and(|expected| {
                        expected
                            .as_slice()
                            .ct_eq(&digest(&g.token_salt, token))
                            .into()
                    })
        })
    }
    pub fn revoke(&mut self, id: Uuid) -> io::Result<bool> {
        let Some(g) = self.grants.get_mut(&id) else {
            return Ok(false);
        };
        g.revoked_at = Some(now());
        self.persist()?;
        Ok(true)
    }
    pub fn grants(&self) -> Vec<Grant> {
        let mut values: Vec<_> = self.grants.values().cloned().collect();
        values.sort_by_key(|g| g.created_at);
        values
    }
    fn persist(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(&Store {
                grants: self.grants(),
                pairing: self.pairing.clone(),
            })
            .map_err(io::Error::other)?,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(tmp, &self.path)
    }
}

pub fn store_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("grants.json")
}

pub fn valid_origin(origin: &str) -> bool {
    let Ok(uri) = origin.parse::<http::Uri>() else {
        return false;
    };
    let Some(scheme) = uri.scheme_str() else {
        return false;
    };
    let Some(authority) = uri.authority() else {
        return false;
    };
    uri.path() == "/"
        && uri.query().is_none()
        && (scheme == "https" || (scheme == "http" && loopback_host(authority.as_str())))
}

pub fn loopback_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    host == "localhost"
        || host.starts_with("localhost:")
        || host == "127.0.0.1"
        || host.starts_with("127.0.0.1:")
        || host == "[::1]"
        || host.starts_with("[::1]:")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_time_origin_bound_pairing_and_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.json");
        let mut auth = AuthStore::load(&path).unwrap();
        let pairing = auth.begin_pairing(Duration::from_secs(30)).unwrap();
        let (id, token) = auth
            .exchange(
                &pairing.value,
                "https://editor.example",
                Duration::from_secs(60),
            )
            .unwrap()
            .unwrap();
        assert!(
            auth.exchange(
                &pairing.value,
                "https://editor.example",
                Duration::from_secs(60)
            )
            .unwrap()
            .is_none()
        );
        assert!(
            auth.authenticate(&token, "https://editor.example")
                .is_some()
        );
        assert!(auth.authenticate(&token, "https://evil.example").is_none());
        auth.revoke(id).unwrap();
        assert!(
            auth.authenticate(&token, "https://editor.example")
                .is_none()
        );
        assert!(
            !String::from_utf8(fs::read(path).unwrap())
                .unwrap()
                .contains(&token)
        );
    }
    #[test]
    fn validates_loopback_host_independent_of_cors() {
        assert!(loopback_host("localhost:9847"));
        assert!(loopback_host("[::1]:9847"));
        assert!(!loopback_host("evil.example"));
    }
}
