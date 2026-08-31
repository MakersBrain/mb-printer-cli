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

/// The permission carried by a browser bearer token.
///
/// `Print` is the original, long-lived pairing grant. `Admin` is deliberately
/// short-lived and is required for operations that alter printer state, such
/// as changing wireless settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    #[default]
    Print,
    Admin,
}

impl GrantScope {
    fn allows(self, required: Self) -> bool {
        matches!(
            (self, required),
            (Self::Admin, _) | (Self::Print, Self::Print)
        )
    }
}

/// An administrator token may never be issued or rotated for longer than ten
/// minutes. The API still needs to obtain a fresh local confirmation before it
/// calls [`AuthStore::issue_admin`] or starts an admin pairing.
pub const ADMIN_GRANT_MAX_TTL: Duration = Duration::from_secs(10 * 60);
/// A local approval is deliberately shorter than an administrator grant. It
/// binds one exact Wi-Fi change to the USB/Bluetooth connection it was
/// previewed for and may be consumed once.
pub const WIFI_APPROVAL_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub id: Uuid,
    pub origin: String,
    /// Missing in persisted stores written before scoped grants existed.
    /// Deserialising it as `Print` keeps those origin-bound grants valid.
    #[serde(default)]
    pub scope: GrantScope,
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
    pub scope: GrantScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPairing {
    salt: String,
    hash: String,
    expires_at: u64,
    #[serde(default)]
    scope: GrantScope,
}

/// A durable, non-secret description of the Wi-Fi mutation waiting for local
/// confirmation. `settings_fingerprint` must be an opaque fingerprint supplied
/// by the caller; never pass raw settings or a password to this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiApproval {
    pub id: Uuid,
    pub grant_id: Uuid,
    pub origin: String,
    pub connection_id: String,
    pub settings_fingerprint: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub approved_at: Option<u64>,
    pub consumed_at: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    grants: Vec<Grant>,
    pairing: Option<StoredPairing>,
    /// There is at most one pending local Wi-Fi confirmation. Replacing it
    /// invalidates any prior approval ID, which keeps the persisted state
    /// small while retaining a consumed marker until the next prepare.
    #[serde(default)]
    wifi_approval: Option<WifiApproval>,
}

#[derive(Debug)]
pub struct AuthStore {
    path: PathBuf,
    grants: HashMap<Uuid, Grant>,
    pairing: Option<StoredPairing>,
    wifi_approval: Option<WifiApproval>,
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
            wifi_approval: store.wifi_approval,
        })
    }
    /// Refresh this process's in-memory view from the durable store.
    ///
    /// The local API service and `mb-printer service` commands are separate
    /// processes. Commands such as `pair`, `pair-admin`, and `approve-wifi`
    /// update the atomically replaced JSON file, so the service must refresh
    /// while holding its API write lock immediately before it exchanges or
    /// consumes that externally-created state. Replacing the complete store
    /// (rather than only the pairing/approval fields) prevents a subsequent
    /// persist from accidentally resurrecting a grant that another process
    /// revoked or rotated.
    pub fn reload(&mut self) -> io::Result<()> {
        let refreshed = Self::load(&self.path)?;
        self.grants = refreshed.grants;
        self.pairing = refreshed.pairing;
        self.wifi_approval = refreshed.wifi_approval;
        Ok(())
    }
    pub fn begin_pairing(&mut self, ttl: Duration) -> io::Result<PairingSecret> {
        self.begin_pairing_for(GrantScope::Print, ttl)
    }
    /// Starts a one-time pairing that can only be exchanged for a short-lived
    /// administrator grant. A caller must arrange a fresh local confirmation
    /// before exposing the returned secret to a browser.
    pub fn begin_admin_pairing(&mut self, ttl: Duration) -> io::Result<PairingSecret> {
        self.begin_pairing_for(GrantScope::Admin, ttl)
    }
    fn begin_pairing_for(&mut self, scope: GrantScope, ttl: Duration) -> io::Result<PairingSecret> {
        let value = random_string(32);
        let salt = random_string(16);
        let expires_at = now().saturating_add(ttl.as_secs().min(600));
        self.pairing = Some(StoredPairing {
            hash: URL_SAFE_NO_PAD.encode(digest(&salt, &value)),
            salt,
            expires_at,
            scope,
        });
        self.persist()?;
        Ok(PairingSecret {
            value,
            expires_at,
            scope,
        })
    }
    pub fn exchange(
        &mut self,
        secret: &str,
        origin: &str,
        ttl: Duration,
    ) -> io::Result<Option<(Uuid, String)>> {
        self.exchange_for(secret, origin, ttl, GrantScope::Print)
    }
    /// Exchanges an administrator pairing secret for a short-lived,
    /// origin-bound administrator token.
    pub fn exchange_admin(
        &mut self,
        secret: &str,
        origin: &str,
        ttl: Duration,
    ) -> io::Result<Option<(Uuid, String)>> {
        self.exchange_for(secret, origin, ttl, GrantScope::Admin)
    }
    fn exchange_for(
        &mut self,
        secret: &str,
        origin: &str,
        ttl: Duration,
        scope: GrantScope,
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
            || pairing.scope != scope
            || !valid_origin(origin)
        {
            return Ok(None);
        }
        self.pairing = None;
        self.persist()?;
        self.issue(origin, scope, ttl).map(Some)
    }
    /// Issues an administrator grant after the caller has completed a fresh
    /// local confirmation. It is origin-bound and capped at
    /// [`ADMIN_GRANT_MAX_TTL`].
    pub fn issue_admin(
        &mut self,
        origin: &str,
        ttl: Duration,
    ) -> io::Result<Option<(Uuid, String)>> {
        if !valid_origin(origin) {
            return Ok(None);
        }
        self.issue(origin, GrantScope::Admin, ttl).map(Some)
    }
    fn issue(
        &mut self,
        origin: &str,
        scope: GrantScope,
        ttl: Duration,
    ) -> io::Result<(Uuid, String)> {
        let token = random_string(32);
        let salt = random_string(16);
        let id = Uuid::new_v4();
        let created_at = now();
        let grant = Grant {
            id,
            origin: origin.to_owned(),
            scope,
            token_hash: URL_SAFE_NO_PAD.encode(digest(&salt, &token)),
            token_salt: salt,
            created_at,
            expires_at: created_at.saturating_add(grant_ttl(scope, ttl)),
            revoked_at: None,
        };
        self.grants.insert(id, grant);
        self.persist()?;
        Ok((id, token))
    }
    pub fn authenticate(&self, token: &str, origin: &str) -> Option<&Grant> {
        self.authenticate_scoped(token, origin, GrantScope::Print)
    }
    /// Authenticates a token for a particular operation scope. `Admin` grants
    /// include print permission; `Print` grants never include administrator
    /// permission.
    pub fn authenticate_scoped(
        &self,
        token: &str,
        origin: &str,
        required: GrantScope,
    ) -> Option<&Grant> {
        self.grants.values().find(|g| {
            g.origin == origin
                && g.revoked_at.is_none()
                && g.expires_at >= now()
                && g.scope.allows(required)
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
    pub fn authenticate_admin(&self, token: &str, origin: &str) -> Option<&Grant> {
        self.authenticate_scoped(token, origin, GrantScope::Admin)
    }
    /// Creates the local-confirmation manifest for exactly one Wi-Fi mutation.
    ///
    /// The caller is responsible for creating `settings_fingerprint` from its
    /// canonical request without retaining raw credentials. This store writes
    /// only the opaque fingerprint, connection ID, and origin-bound admin
    /// grant identity. Preparing a new change invalidates a previous one.
    pub fn prepare_wifi_approval(
        &mut self,
        grant_id: Uuid,
        origin: &str,
        connection_id: &str,
        settings_fingerprint: &str,
    ) -> io::Result<Option<WifiApproval>> {
        if !valid_origin(origin)
            || !valid_approval_binding(connection_id)
            || !valid_approval_binding(settings_fingerprint)
            || !self.grant_allows_admin(grant_id, origin)
        {
            return Ok(None);
        }
        let created_at = now();
        let approval = WifiApproval {
            id: Uuid::new_v4(),
            grant_id,
            origin: origin.to_owned(),
            connection_id: connection_id.to_owned(),
            settings_fingerprint: settings_fingerprint.to_owned(),
            created_at,
            expires_at: created_at.saturating_add(WIFI_APPROVAL_TTL.as_secs()),
            approved_at: None,
            consumed_at: None,
        };
        self.wifi_approval = Some(approval.clone());
        self.persist()?;
        Ok(Some(approval))
    }
    /// Returns the requested approval without changing it. This lets a local
    /// CLI show the pending request before asking the person at the machine to
    /// approve it.
    pub fn wifi_approval(&self, id: Uuid) -> Option<&WifiApproval> {
        self.wifi_approval
            .as_ref()
            .filter(|approval| approval.id == id && approval.expires_at >= now())
    }
    /// Marks a currently pending request as locally approved. It does not
    /// apply any printer setting; only `consume_wifi_approval` can do that.
    pub fn approve_wifi_approval(&mut self, id: Uuid) -> io::Result<bool> {
        let Some(approval) = self.wifi_approval.as_mut() else {
            return Ok(false);
        };
        if approval.id != id || approval.expires_at < now() || approval.consumed_at.is_some() {
            return Ok(false);
        }
        approval.approved_at = Some(now());
        self.persist()?;
        Ok(true)
    }
    /// Atomically checks and consumes a local Wi-Fi approval. The caller must
    /// supply every binding from the apply request. A consumed manifest stays
    /// persisted until a subsequent prepare replaces it, preventing replay
    /// after a process restart.
    pub fn consume_wifi_approval(
        &mut self,
        id: Uuid,
        grant_id: Uuid,
        origin: &str,
        connection_id: &str,
        settings_fingerprint: &str,
    ) -> io::Result<bool> {
        if !self.grant_allows_admin(grant_id, origin) {
            return Ok(false);
        }
        let Some(approval) = self.wifi_approval.as_mut() else {
            return Ok(false);
        };
        if approval.id != id
            || approval.grant_id != grant_id
            || approval.origin != origin
            || approval.connection_id != connection_id
            || approval.settings_fingerprint != settings_fingerprint
            || approval.expires_at < now()
            || approval.approved_at.is_none()
            || approval.consumed_at.is_some()
        {
            return Ok(false);
        }
        approval.consumed_at = Some(now());
        self.persist()?;
        Ok(true)
    }
    fn grant_allows_admin(&self, id: Uuid, origin: &str) -> bool {
        self.grants.get(&id).is_some_and(|grant| {
            grant.origin == origin
                && grant.revoked_at.is_none()
                && grant.expires_at >= now()
                && grant.scope.allows(GrantScope::Admin)
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
    /// Replace a grant's bearer secret while preserving its origin and ID.
    /// The previous token stops authenticating as soon as this method returns.
    pub fn rotate(&mut self, id: Uuid, ttl: Duration) -> io::Result<Option<String>> {
        let Some(grant) = self.grants.get_mut(&id) else {
            return Ok(None);
        };
        let token = random_string(32);
        let salt = random_string(16);
        grant.token_hash = URL_SAFE_NO_PAD.encode(digest(&salt, &token));
        grant.token_salt = salt;
        grant.created_at = now();
        grant.expires_at = now().saturating_add(grant_ttl(grant.scope, ttl));
        grant.revoked_at = None;
        self.persist()?;
        Ok(Some(token))
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
                wifi_approval: self.wifi_approval.clone(),
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

fn grant_ttl(scope: GrantScope, ttl: Duration) -> u64 {
    let maximum = match scope {
        GrantScope::Print => 31_536_000,
        GrantScope::Admin => ADMIN_GRANT_MAX_TTL.as_secs(),
    };
    ttl.as_secs().min(maximum)
}

fn valid_approval_binding(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.contains('\0')
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
    fn rotation_invalidates_old_token_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.json");
        let mut auth = AuthStore::load(&path).unwrap();
        let pairing = auth.begin_pairing(Duration::from_secs(30)).unwrap();
        let (id, old) = auth
            .exchange(
                &pairing.value,
                "https://editor.example",
                Duration::from_secs(60),
            )
            .unwrap()
            .unwrap();
        let new = auth.rotate(id, Duration::from_secs(120)).unwrap().unwrap();
        assert!(auth.authenticate(&old, "https://editor.example").is_none());
        assert!(auth.authenticate(&new, "https://editor.example").is_some());
        let reloaded = AuthStore::load(path).unwrap();
        assert!(
            reloaded
                .authenticate(&old, "https://editor.example")
                .is_none()
        );
        assert!(
            reloaded
                .authenticate(&new, "https://editor.example")
                .is_some()
        );
    }
    #[test]
    fn legacy_grants_default_to_print_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.json");
        let token = "legacy-token";
        let salt = "legacy-salt";
        let grant = serde_json::json!({
            "id": Uuid::new_v4(),
            "origin": "https://editor.example",
            "token_salt": salt,
            "token_hash": URL_SAFE_NO_PAD.encode(digest(salt, token)),
            "created_at": 1,
            "expires_at": now().saturating_add(60),
            "revoked_at": null
        });
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "grants": [grant], "pairing": null })).unwrap(),
        )
        .unwrap();

        let auth = AuthStore::load(path).unwrap();
        assert!(auth.authenticate(token, "https://editor.example").is_some());
        assert!(
            auth.authenticate_admin(token, "https://editor.example")
                .is_none()
        );
        assert_eq!(auth.grants()[0].scope, GrantScope::Print);
    }
    #[test]
    fn admin_grants_are_short_lived_and_scope_aware() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.json");
        let mut auth = AuthStore::load(&path).unwrap();
        let pairing = auth.begin_pairing(Duration::from_secs(30)).unwrap();
        let (_, print_token) = auth
            .exchange(
                &pairing.value,
                "https://editor.example",
                Duration::from_secs(60),
            )
            .unwrap()
            .unwrap();
        let (admin_id, admin_token) = auth
            .issue_admin("https://editor.example", Duration::from_secs(86_400))
            .unwrap()
            .unwrap();

        assert!(
            auth.authenticate_admin(&print_token, "https://editor.example")
                .is_none()
        );
        assert!(
            auth.authenticate_admin(&admin_token, "https://editor.example")
                .is_some()
        );
        assert!(
            auth.authenticate(&admin_token, "https://editor.example")
                .is_some()
        );
        let admin = auth
            .grants()
            .into_iter()
            .find(|g| g.id == admin_id)
            .unwrap();
        assert_eq!(admin.scope, GrantScope::Admin);
        assert!(admin.expires_at <= admin.created_at + ADMIN_GRANT_MAX_TTL.as_secs());
        assert!(
            !String::from_utf8(fs::read(&path).unwrap())
                .unwrap()
                .contains(&admin_token)
        );
    }
    #[test]
    fn admin_pairing_cannot_be_exchanged_as_a_normal_grant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.json");
        let mut auth = AuthStore::load(path).unwrap();
        let pairing = auth.begin_admin_pairing(Duration::from_secs(30)).unwrap();
        assert_eq!(pairing.scope, GrantScope::Admin);
        assert!(
            auth.exchange(
                &pairing.value,
                "https://editor.example",
                Duration::from_secs(60)
            )
            .unwrap()
            .is_none()
        );
        // A scope mismatch does not consume the one-time secret, allowing the
        // intended administrator exchange to complete.
        let (_, token) = auth
            .exchange_admin(
                &pairing.value,
                "https://editor.example",
                Duration::from_secs(86_400),
            )
            .unwrap()
            .unwrap();
        assert!(
            auth.authenticate_admin(&token, "https://editor.example")
                .is_some()
        );
        let admin = auth.grants().pop().unwrap();
        assert!(admin.expires_at <= admin.created_at + ADMIN_GRANT_MAX_TTL.as_secs());
    }
    #[test]
    fn wifi_approval_is_persistent_bound_and_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.json");
        let mut auth = AuthStore::load(&path).unwrap();
        let (grant_id, _) = auth
            .issue_admin("https://editor.example", Duration::from_secs(60))
            .unwrap()
            .unwrap();
        let raw_password = "never-persist-this-wifi-password";
        let fingerprint = "sha256:only-an-opaque-settings-fingerprint";
        let approval = auth
            .prepare_wifi_approval(
                grant_id,
                "https://editor.example",
                "usb:04f9:209b:serial=E12345",
                fingerprint,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            approval.expires_at,
            approval.created_at + WIFI_APPROVAL_TTL.as_secs()
        );
        assert!(auth.wifi_approval(approval.id).is_some());
        assert!(
            auth.prepare_wifi_approval(
                Uuid::new_v4(),
                "https://editor.example",
                "usb:04f9:209b:serial=E12345",
                fingerprint,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            !String::from_utf8(fs::read(&path).unwrap())
                .unwrap()
                .contains(raw_password)
        );
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path).unwrap().permissions())
                & 0o777,
            0o600
        );

        let mut reloaded = AuthStore::load(&path).unwrap();
        assert!(reloaded.approve_wifi_approval(approval.id).unwrap());
        assert!(
            !reloaded
                .consume_wifi_approval(
                    approval.id,
                    grant_id,
                    "https://editor.example",
                    "usb:wrong-device",
                    fingerprint,
                )
                .unwrap()
        );
        assert!(
            reloaded
                .consume_wifi_approval(
                    approval.id,
                    grant_id,
                    "https://editor.example",
                    "usb:04f9:209b:serial=E12345",
                    fingerprint,
                )
                .unwrap()
        );
        let mut reloaded = AuthStore::load(&path).unwrap();
        assert!(
            !reloaded
                .consume_wifi_approval(
                    approval.id,
                    grant_id,
                    "https://editor.example",
                    "usb:04f9:209b:serial=E12345",
                    fingerprint,
                )
                .unwrap()
        );
    }
    #[test]
    fn preparing_a_new_wifi_approval_invalidates_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.json");
        let mut auth = AuthStore::load(path).unwrap();
        let (grant_id, _) = auth
            .issue_admin("https://editor.example", Duration::from_secs(60))
            .unwrap()
            .unwrap();
        let first = auth
            .prepare_wifi_approval(
                grant_id,
                "https://editor.example",
                "usb:one",
                "fingerprint-one",
            )
            .unwrap()
            .unwrap();
        let second = auth
            .prepare_wifi_approval(
                grant_id,
                "https://editor.example",
                "usb:two",
                "fingerprint-two",
            )
            .unwrap()
            .unwrap();
        assert!(auth.wifi_approval(first.id).is_none());
        assert!(auth.wifi_approval(second.id).is_some());
    }
    #[test]
    fn validates_loopback_host_independent_of_cors() {
        assert!(loopback_host("localhost:9847"));
        assert!(loopback_host("[::1]:9847"));
        assert!(!loopback_host("evil.example"));
    }
}
