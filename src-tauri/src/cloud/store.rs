// Persistence for the optional cloud link. Two separate homes on purpose:
// - `cloud.json` (next to installation.json): who this device is linked to and
//   the user's settings. It never contains a credential.
// - The OS keychain (behind `SecretStore`): refresh/access tokens and the
//   device signing key. Tests use `MemoryStore`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const KEY_REFRESH: &str = "refresh_token";
pub const KEY_ACCESS: &str = "access_token";
pub const KEY_DEVICE: &str = "device_key";

const KEYCHAIN_SERVICE: &str = "com.simplebash.pos.cloud";
const VAULT_ENTRY: &str = "vault";

/// Minimal secret storage so the OS keychain can be swapped for a fake.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, String>;
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
    fn delete(&self, key: &str) -> Result<(), String>;
}

/// All secrets live in ONE keychain item (a JSON object), so macOS asks for
/// permission once instead of once per secret. The object is read from the
/// keychain at most once per process and cached; writes update both.
pub struct KeyringStore;

type Vault = std::collections::BTreeMap<String, String>;

static VAULT: std::sync::OnceLock<std::sync::Mutex<Option<Vault>>> = std::sync::OnceLock::new();

impl KeyringStore {
    fn entry() -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, VAULT_ENTRY).map_err(|e| e.to_string())
    }

    fn with_vault<T>(f: impl FnOnce(&mut Vault) -> T, persist: bool) -> Result<T, String> {
        let cell = VAULT.get_or_init(|| std::sync::Mutex::new(None));
        let mut guard = cell
            .lock()
            .map_err(|_| "keychain cache lock poisoned".to_string())?;
        if guard.is_none() {
            let loaded = match Self::entry()?.get_password() {
                Ok(json) => serde_json::from_str::<Vault>(&json).map_err(|e| e.to_string())?,
                Err(keyring::Error::NoEntry) => Vault::new(),
                Err(e) => return Err(e.to_string()),
            };
            *guard = Some(loaded);
        }
        let vault = guard.as_mut().expect("vault loaded above");
        let out = f(vault);
        if persist {
            let entry = Self::entry()?;
            if vault.is_empty() {
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(e) => return Err(e.to_string()),
                }
            } else {
                let json = serde_json::to_string(vault).map_err(|e| e.to_string())?;
                entry.set_password(&json).map_err(|e| e.to_string())?;
            }
        }
        Ok(out)
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        Self::with_vault(|v| v.get(key).cloned(), false)
    }

    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        Self::with_vault(
            |v| {
                v.insert(key.to_string(), value.to_string());
            },
            true,
        )
    }

    fn delete(&self, key: &str) -> Result<(), String> {
        Self::with_vault(
            |v| {
                v.remove(key);
            },
            true,
        )
    }
}

/// In-memory fake used by tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore(pub std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// Contents of `cloud.json`. Deliberately holds no tokens or keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CloudFile {
    pub device_id: Option<String>,
    pub tenant_id: Option<String>,
    pub shop_code: Option<String>,
    pub account_email: Option<String>,
    pub linked_at: Option<String>,
    /// `None` = never chosen: telemetry defaults to on (only when cloud is enabled).
    pub telemetry_enabled: Option<bool>,
    /// Overrides the build-time `CLOUD_API_URL` (self-hosted / forks).
    pub api_url: Option<String>,
    /// Base URL of the POS cloud API that serves `/api/sync/*` when it is a
    /// different host from the identity service. Falls back to `api_url`.
    pub sync_api_url: Option<String>,
}

impl CloudFile {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("cloud.json")
    }

    pub fn load(dir: &Path) -> CloudFile {
        fs::read_to_string(Self::path(dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(Self::path(dir), text).map_err(|e| e.to_string())
    }

    pub fn is_linked(&self) -> bool {
        self.device_id.is_some() && self.tenant_id.is_some()
    }

    pub fn clear_link(&mut self) {
        self.device_id = None;
        self.tenant_id = None;
        self.shop_code = None;
        self.account_email = None;
        self.linked_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_file_round_trips_and_never_serializes_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let file = CloudFile {
            device_id: Some("dev_1".into()),
            tenant_id: Some("tnt_1".into()),
            telemetry_enabled: Some(false),
            ..Default::default()
        };
        file.save(dir.path()).unwrap();
        let raw = fs::read_to_string(CloudFile::path(dir.path())).unwrap();
        assert!(!raw.to_lowercase().contains("token"));
        assert_eq!(CloudFile::load(dir.path()), file);
    }

    #[test]
    fn missing_or_corrupt_file_loads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(CloudFile::load(dir.path()), CloudFile::default());
        fs::write(CloudFile::path(dir.path()), "{not json").unwrap();
        assert_eq!(CloudFile::load(dir.path()), CloudFile::default());
    }
}
