// Shops on this computer. Each SimpleBash shop the owner links gets its own
// profile: its own database folder (pos.db, document_server.db, assets,
// generated documents) and its own parked cloud link, so switching shop swaps
// the whole shop and two shops can never mix rows.
//
// `profiles.json` (next to `cloud.json`) is the registry and always exists once
// the app has started. Folders live under `shops/<profileId>/`; the id is
// generated here and is never the server's tenant id, so nothing from the
// network becomes a path. Installs from before profiles are migrated once, at
// start-up: their `db/`, `assets/` and `generated_documents/` move into the
// first profile.
//
// Only the ACTIVE profile's link lives in `cloud.json` and the active keychain
// keys, so every cloud and sync code path stays as it was. Switching parks the
// outgoing link (its tokens move to `<key>@<profileId>`) and restores the
// target's. The device key is per installation, not per shop, and stays put.
//
// Crash safety: `switching` is written first; `recover` re-runs an interrupted
// switch on the next boot. Every step is safe to repeat.

use std::fs;
use std::path::{Path, PathBuf};

use rand::RngCore;
use serde::{Deserialize, Serialize};

use super::store::{CloudFile, SecretStore, KEY_ACCESS, KEY_REFRESH};
use super::CloudError;

const FILE: &str = "profiles.json";
/// Folders of a shop's data. Before profiles they sat directly in the app data folder.
const SHOP_FOLDERS: [&str; 3] = ["db", "assets", "generated_documents"];

/// The part of a link that is not a secret, kept while the profile is inactive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parked {
    pub device_id: Option<String>,
    pub linked_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: String,
    /// The shop this database belongs to. Once set it never changes.
    pub tenant_id: Option<String>,
    pub shop_code: Option<String>,
    pub shop_name: Option<String>,
    pub account_email: Option<String>,
    pub account_name: Option<String>,
    pub last_used_at: Option<String>,
    pub parked: Option<Parked>,
}

impl Profile {
    fn new(id: String, tenant_id: Option<String>) -> Self {
        Self {
            id,
            tenant_id,
            shop_code: None,
            shop_name: None,
            account_email: None,
            account_name: None,
            last_used_at: None,
            parked: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub version: u32,
    pub active_id: String,
    /// Set while a switch to this profile is in progress.
    pub switching: Option<String>,
    pub profiles: Vec<Profile>,
}

/// Where a profile keeps its files.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileDirs {
    pub db_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub generated_docs: PathBuf,
}

/// Where an approved link for some shop must go.
#[derive(Debug, Clone, PartialEq)]
pub enum Switch {
    /// The open profile is (or becomes) that shop's.
    Stay,
    /// Another profile on this computer already belongs to that shop.
    To(String),
    /// The open profile belongs to a different shop: start a profile for this one.
    New,
}

fn new_id() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("shop_{hex}")
}

fn parked_key(key: &str, id: &str) -> String {
    format!("{key}@{id}")
}

fn keychain_err(e: String) -> CloudError {
    CloudError::new("KEYCHAIN_ERROR", e, 0)
}

fn state_err(e: impl ToString) -> CloudError {
    CloudError::new("STATE_WRITE_FAILED", e.to_string(), 0)
}

/// Copies a secret to another name (no-op when absent).
fn copy_secret(secrets: &dyn SecretStore, from: &str, to: &str) -> Result<(), CloudError> {
    if let Some(v) = secrets.get(from).map_err(keychain_err)? {
        secrets.set(to, &v).map_err(keychain_err)?;
    }
    Ok(())
}

fn move_secret(secrets: &dyn SecretStore, from: &str, to: &str) -> Result<(), CloudError> {
    copy_secret(secrets, from, to)?;
    secrets.delete(from).map_err(keychain_err)
}

impl Registry {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(FILE)
    }

    pub fn exists(dir: &Path) -> bool {
        Self::path(dir).exists()
    }

    /// The saved registry. Errors if it is missing or unreadable: start-up
    /// (`recover`) is what creates it, and nothing may guess a replacement.
    pub fn load(dir: &Path) -> Result<Registry, CloudError> {
        let text = fs::read_to_string(Self::path(dir))
            .map_err(|e| CloudError::new("PROFILES_MISSING", e.to_string(), 0))?;
        let reg: Registry = serde_json::from_str(&text)
            .map_err(|e| CloudError::new("PROFILES_CORRUPT", e.to_string(), 0))?;
        if reg.get(&reg.active_id).is_none() {
            return Err(CloudError::new(
                "PROFILES_CORRUPT",
                "the open shop is not in the list",
                0,
            ));
        }
        Ok(reg)
    }

    /// One-time migration of an install from before profiles: its single shop
    /// becomes the first profile, bound to the shop it is linked to (if any),
    /// and its data folders move into that profile.
    pub fn migrate(dir: &Path) -> Result<Registry, CloudError> {
        let file = CloudFile::load(dir);
        let id = new_id();
        let mut first = Profile::new(id.clone(), file.tenant_id.clone());
        first.shop_code = file.shop_code;
        first.account_email = file.account_email;
        first.account_name = file.account_name;
        let reg = Registry {
            version: 1,
            active_id: id,
            switching: None,
            profiles: vec![first],
        };
        reg.save(dir)?;
        Ok(reg)
    }

    /// Moves folders left over from before profiles into the first profile.
    /// Nothing to do once they are gone; safe to repeat after a crash.
    pub fn move_old_folders(&self, dir: &Path) -> Result<(), CloudError> {
        let first = &self.profiles[0].id;
        for name in SHOP_FOLDERS {
            let from = dir.join(name);
            if !from.exists() {
                continue;
            }
            let to = dir.join("shops").join(first).join(name);
            if to.exists() {
                return Err(CloudError::new(
                    "MIGRATION_CONFLICT",
                    format!("{name} exists in both the old and the new location"),
                    0,
                ));
            }
            fs::create_dir_all(to.parent().expect("has a parent")).map_err(state_err)?;
            fs::rename(&from, &to).map_err(state_err)?;
        }
        Ok(())
    }

    pub fn save(&self, dir: &Path) -> Result<(), CloudError> {
        fs::create_dir_all(dir).map_err(state_err)?;
        let text = serde_json::to_string_pretty(self).map_err(state_err)?;
        // Write then rename so a crash never leaves half a registry.
        let tmp = dir.join(format!("{FILE}.tmp"));
        fs::write(&tmp, text).map_err(state_err)?;
        fs::rename(&tmp, Self::path(dir)).map_err(state_err)
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    pub fn active(&self) -> &Profile {
        self.get(&self.active_id)
            .expect("load() guarantees the active profile exists")
    }

    fn active_mut(&mut self) -> &mut Profile {
        let id = self.active_id.clone();
        self.get_mut(&id)
            .expect("load() guarantees the active profile exists")
    }

    pub fn dirs_for(&self, data_dir: &Path, id: &str) -> ProfileDirs {
        let root = data_dir.join("shops").join(id);
        ProfileDirs {
            db_dir: root.join("db"),
            assets_dir: root.join("assets"),
            generated_docs: root.join("generated_documents"),
        }
    }

    pub fn active_dirs(&self, data_dir: &Path) -> ProfileDirs {
        self.dirs_for(data_dir, &self.active_id)
    }

    /// Which profile an approved link for `tenant` belongs to.
    ///
    /// - the open profile is bound to `tenant` → stay
    /// - another profile is bound to `tenant` → that one
    /// - the open profile is bound to a different shop → a new profile
    /// - the open profile is not bound yet (offline use so far) → stay and bind
    pub fn switch_target(&self, tenant: &str) -> Switch {
        let active = self.active();
        if active.tenant_id.as_deref() == Some(tenant) {
            return Switch::Stay;
        }
        if let Some(p) = self
            .profiles
            .iter()
            .find(|p| p.tenant_id.as_deref() == Some(tenant))
        {
            return Switch::To(p.id.clone());
        }
        if active.tenant_id.is_none() {
            return Switch::Stay;
        }
        Switch::New
    }

    /// Saves the active profile's link (non-secret fields here, tokens in the
    /// keychain under `<key>@<id>`) and clears it from `cloud.json`. Repeating
    /// it, or running it with nothing linked, changes nothing.
    pub fn park_active(&mut self, dir: &Path, secrets: &dyn SecretStore) -> Result<(), CloudError> {
        let mut file = CloudFile::load(dir);
        if !file.is_linked() {
            return Ok(());
        }
        let id = self.active_id.clone();
        for key in [KEY_ACCESS, KEY_REFRESH] {
            copy_secret(secrets, key, &parked_key(key, &id))?;
        }
        let p = self.active_mut();
        p.tenant_id = file.tenant_id.clone();
        p.shop_code = file.shop_code.clone();
        p.account_email = file.account_email.clone();
        p.account_name = file.account_name.clone();
        p.parked = Some(Parked {
            device_id: file.device_id.clone(),
            linked_at: file.linked_at.clone(),
        });
        self.save(dir)?;
        file.clear_link();
        file.account_name = None;
        file.save(dir).map_err(state_err)?;
        for key in [KEY_ACCESS, KEY_REFRESH] {
            secrets.delete(key).map_err(keychain_err)?;
        }
        Ok(())
    }

    /// Puts `id`'s parked link back into `cloud.json` and the active keychain
    /// keys (or leaves the computer unlinked if it has none).
    fn restore(
        &mut self,
        dir: &Path,
        secrets: &dyn SecretStore,
        id: &str,
    ) -> Result<(), CloudError> {
        let target = self
            .get(id)
            .cloned()
            .ok_or_else(|| CloudError::new("PROFILE_NOT_FOUND", "unknown shop", 0))?;
        let mut file = CloudFile::load(dir);
        match target.parked {
            Some(parked) => {
                file.device_id = parked.device_id;
                file.tenant_id = target.tenant_id.clone();
                file.shop_code = target.shop_code.clone();
                file.account_email = target.account_email.clone();
                file.account_name = target.account_name.clone();
                file.linked_at = parked.linked_at;
                file.save(dir).map_err(state_err)?;
                for key in [KEY_ACCESS, KEY_REFRESH] {
                    move_secret(secrets, &parked_key(key, id), key)?;
                }
                self.get_mut(id).expect("checked above").parked = None;
            }
            None => {
                // Already restored by an interrupted earlier run: keep it.
                let restored = file.is_linked() && file.tenant_id == target.tenant_id;
                if !restored {
                    file.clear_link();
                    file.account_name = None;
                    file.save(dir).map_err(state_err)?;
                }
            }
        }
        Ok(())
    }

    /// Makes `target` the active profile. Does not restart anything.
    pub fn activate(
        &mut self,
        dir: &Path,
        secrets: &dyn SecretStore,
        target: &str,
    ) -> Result<(), CloudError> {
        if self.get(target).is_none() {
            return Err(CloudError::new("PROFILE_NOT_FOUND", "unknown shop", 0));
        }
        self.switching = Some(target.to_string());
        self.save(dir)?;
        if self.active_id != target {
            self.park_active(dir, secrets)?;
        }
        self.restore(dir, secrets, target)?;
        self.active_id = target.to_string();
        self.switching = None;
        self.active_mut().last_used_at = Some(chrono::Utc::now().to_rfc3339());
        self.save(dir)
    }

    /// Starts moving to the shop that was just approved (`Switch::To` or
    /// `Switch::New`): parks the current link and makes the target the open
    /// profile. The caller then writes the new link and calls `finish_new_link`.
    pub fn begin_new_link(
        &mut self,
        dir: &Path,
        secrets: &dyn SecretStore,
        switch: Switch,
        tenant: &str,
    ) -> Result<(), CloudError> {
        let target = match switch {
            Switch::Stay => return Ok(()),
            Switch::To(id) => id,
            Switch::New => {
                let id = new_id();
                self.profiles
                    .push(Profile::new(id.clone(), Some(tenant.to_string())));
                id
            }
        };
        self.switching = Some(target.clone());
        self.save(dir)?;
        self.park_active(dir, secrets)?;
        // The fresh approval replaces whatever the profile had parked.
        if self.get(&target).is_some_and(|p| p.parked.is_some()) {
            for key in [KEY_ACCESS, KEY_REFRESH] {
                secrets
                    .delete(&parked_key(key, &target))
                    .map_err(keychain_err)?;
            }
            self.get_mut(&target).expect("checked above").parked = None;
        }
        self.active_id = target;
        self.save(dir)
    }

    /// Records the shop details of the link now in `cloud.json` on the active
    /// profile, binding it to the shop, and ends any switch in progress.
    pub fn finish_new_link(
        &mut self,
        dir: &Path,
        file: &CloudFile,
        shop_name: String,
    ) -> Result<(), CloudError> {
        self.switching = None;
        let p = self.active_mut();
        p.tenant_id = file.tenant_id.clone();
        p.shop_code = file.shop_code.clone();
        p.shop_name = Some(shop_name);
        p.account_email = file.account_email.clone();
        p.account_name = file.account_name.clone();
        p.parked = None;
        p.last_used_at = Some(chrono::Utc::now().to_rfc3339());
        self.save(dir)
    }
}

/// Run once at start-up, before anything reads the link: creates the registry
/// (migrating an install from before profiles), moves old folders into place,
/// and finishes a switch that was interrupted.
pub fn recover(dir: &Path, secrets: &dyn SecretStore) -> Result<Registry, CloudError> {
    let mut reg = if Registry::exists(dir) {
        Registry::load(dir)?
    } else {
        Registry::migrate(dir)?
    };
    reg.move_old_folders(dir)?;
    if let Some(target) = reg.switching.clone() {
        reg.activate(dir, secrets, &target)?;
    }
    Ok(reg)
}

/// What the webview sees of one shop on this computer. Never a token.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileView {
    pub id: String,
    pub shop_name: Option<String>,
    pub shop_code: Option<String>,
    pub account_email: Option<String>,
    pub active: bool,
    /// Has a cloud link (parked for inactive profiles).
    pub linked: bool,
    pub last_used_at: Option<String>,
}

pub fn views(reg: &Registry, file: &CloudFile) -> Vec<ProfileView> {
    let mut out: Vec<ProfileView> = reg
        .profiles
        .iter()
        .map(|p| {
            let active = p.id == reg.active_id;
            ProfileView {
                id: p.id.clone(),
                shop_name: p.shop_name.clone(),
                shop_code: p.shop_code.clone(),
                account_email: p.account_email.clone(),
                active,
                linked: if active {
                    file.is_linked()
                } else {
                    p.parked.is_some()
                },
                last_used_at: p.last_used_at.clone(),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| b.last_used_at.cmp(&a.last_used_at))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::super::store::MemoryStore;
    use super::*;

    fn linked(tenant: &str, code: &str, email: &str) -> CloudFile {
        CloudFile {
            device_id: Some(format!("dev_{tenant}")),
            tenant_id: Some(tenant.into()),
            shop_code: Some(code.into()),
            account_email: Some(email.into()),
            linked_at: Some("2026-01-01T00:00:00Z".into()),
            ..CloudFile::default()
        }
    }

    fn set_tokens(s: &MemoryStore, tag: &str) {
        s.set(KEY_ACCESS, &format!("acc_{tag}")).unwrap();
        s.set(KEY_REFRESH, &format!("ref_{tag}")).unwrap();
    }

    /// An install linked to shop A, migrated, with its tokens in the keychain.
    fn shop_a(dir: &Path, s: &MemoryStore) -> Registry {
        linked("tnt_a", "shop-a", "a@x.lk").save(dir).unwrap();
        set_tokens(s, "a");
        Registry::migrate(dir).unwrap()
    }

    /// Links shop B on top of the open one, the way `complete_link` does.
    fn link_b(dir: &Path, s: &MemoryStore, reg: &mut Registry) {
        let switch = reg.switch_target("tnt_b");
        reg.begin_new_link(dir, s, switch, "tnt_b").unwrap();
        linked("tnt_b", "shop-b", "b@x.lk").save(dir).unwrap();
        set_tokens(s, "b");
        let file = CloudFile::load(dir);
        reg.finish_new_link(dir, &file, "Shop B".into()).unwrap();
    }

    #[test]
    fn migration_makes_the_install_the_first_profile_and_moves_its_folders() {
        let dir = tempfile::tempdir().unwrap();
        linked("tnt_a", "shop-a", "a@x.lk")
            .save(dir.path())
            .unwrap();
        for name in SHOP_FOLDERS {
            fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        fs::write(dir.path().join("db/pos.db"), "rows").unwrap();

        let reg = recover(dir.path(), &MemoryStore::default()).unwrap();
        let id = reg.active_id.clone();
        assert!(id.starts_with("shop_"));
        assert_eq!(reg.active().tenant_id.as_deref(), Some("tnt_a"));
        let dirs = reg.active_dirs(dir.path());
        assert_eq!(
            fs::read_to_string(dirs.db_dir.join("pos.db")).unwrap(),
            "rows"
        );
        assert!(dirs.assets_dir.exists() && dirs.generated_docs.exists());
        for name in SHOP_FOLDERS {
            assert!(!dir.path().join(name).exists(), "{name} left behind");
        }
        // The next start reads the registry back and changes nothing.
        assert_eq!(recover(dir.path(), &MemoryStore::default()).unwrap(), reg);
    }

    #[test]
    fn an_unlinked_install_migrates_unbound() {
        let dir = tempfile::tempdir().unwrap();
        let reg = recover(dir.path(), &MemoryStore::default()).unwrap();
        assert_eq!(reg.active().tenant_id, None);
    }

    #[test]
    fn migration_never_overwrites_folders_that_exist_in_both_places() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::migrate(dir.path()).unwrap();
        fs::create_dir_all(dir.path().join("db")).unwrap();
        fs::create_dir_all(reg.active_dirs(dir.path()).db_dir).unwrap();
        assert_eq!(
            reg.move_old_folders(dir.path()).unwrap_err().code,
            "MIGRATION_CONFLICT"
        );
    }

    #[test]
    fn a_missing_or_broken_registry_is_an_error_not_a_fresh_start() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Registry::load(dir.path()).unwrap_err().code,
            "PROFILES_MISSING"
        );
        fs::write(Registry::path(dir.path()), "{nope").unwrap();
        assert_eq!(
            Registry::load(dir.path()).unwrap_err().code,
            "PROFILES_CORRUPT"
        );
        assert!(recover(dir.path(), &MemoryStore::default()).is_err());
    }

    #[test]
    fn switch_target_rules() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::migrate(dir.path()).unwrap();
        // Unbound offline database binds to the first shop it links to.
        assert_eq!(reg.switch_target("tnt_a"), Switch::Stay);
        reg.active_mut().tenant_id = Some("tnt_a".into());
        assert_eq!(reg.switch_target("tnt_a"), Switch::Stay);
        assert_eq!(reg.switch_target("tnt_b"), Switch::New);
        reg.profiles
            .push(Profile::new("shop_c".into(), Some("tnt_c".into())));
        assert_eq!(reg.switch_target("tnt_c"), Switch::To("shop_c".into()));
        // An unbound open profile also moves to an existing profile for the shop.
        reg.active_mut().tenant_id = None;
        assert_eq!(reg.switch_target("tnt_c"), Switch::To("shop_c".into()));
    }

    #[test]
    fn a_tenant_id_from_the_server_never_becomes_a_folder_name() {
        let dir = tempfile::tempdir().unwrap();
        let s = MemoryStore::default();
        let mut reg = shop_a(dir.path(), &s);
        reg.begin_new_link(dir.path(), &s, Switch::New, "../../evil")
            .unwrap();
        let db = reg.active_dirs(dir.path()).db_dir;
        assert!(db.starts_with(dir.path().join("shops")));
        assert!(!db.to_string_lossy().contains("evil"));
    }

    #[test]
    fn new_link_parks_the_old_shop_and_keeps_its_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let s = MemoryStore::default();
        let mut reg = shop_a(dir.path(), &s);
        let a = reg.active_id.clone();

        let switch = reg.switch_target("tnt_b");
        reg.begin_new_link(dir.path(), &s, switch, "tnt_b").unwrap();
        // The old link is parked, nothing of it is left active.
        assert!(!CloudFile::load(dir.path()).is_linked());
        assert!(s.get(KEY_ACCESS).unwrap().is_none());
        assert_eq!(
            s.get(&format!("access_token@{a}")).unwrap().as_deref(),
            Some("acc_a")
        );
        assert!(reg.switching.is_some());

        linked("tnt_b", "shop-b", "b@x.lk")
            .save(dir.path())
            .unwrap();
        set_tokens(&s, "b");
        let file = CloudFile::load(dir.path());
        reg.finish_new_link(dir.path(), &file, "Shop B".into())
            .unwrap();
        assert_ne!(reg.active_id, a);
        assert_eq!(reg.switching, None);
        assert_eq!(reg.active().shop_name.as_deref(), Some("Shop B"));
        assert_eq!(reg.get(&a).unwrap().tenant_id.as_deref(), Some("tnt_a"));
    }

    #[test]
    fn activating_the_old_shop_restores_its_own_link_and_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let s = MemoryStore::default();
        let mut reg = shop_a(dir.path(), &s);
        let a = reg.active_id.clone();
        link_b(dir.path(), &s, &mut reg);
        let b = reg.active_id.clone();

        reg.activate(dir.path(), &s, &a).unwrap();
        let file = CloudFile::load(dir.path());
        assert_eq!(file.tenant_id.as_deref(), Some("tnt_a"));
        assert_eq!(file.device_id.as_deref(), Some("dev_tnt_a"));
        assert_eq!(s.get(KEY_ACCESS).unwrap().as_deref(), Some("acc_a"));
        assert_eq!(s.get(KEY_REFRESH).unwrap().as_deref(), Some("ref_a"));
        // B is parked now, with its own tokens.
        assert_eq!(
            s.get(&format!("access_token@{b}")).unwrap().as_deref(),
            Some("acc_b")
        );
        assert!(reg.get(&b).unwrap().parked.is_some());
        assert_eq!(reg.active_id, a);
        assert_eq!(reg.switching, None);
    }

    #[test]
    fn activating_an_unlinked_profile_leaves_the_computer_unlinked() {
        let dir = tempfile::tempdir().unwrap();
        let s = MemoryStore::default();
        let mut reg = shop_a(dir.path(), &s);
        reg.profiles
            .push(Profile::new("shop_b".into(), Some("tnt_b".into())));
        reg.activate(dir.path(), &s, "shop_b").unwrap();
        assert!(!CloudFile::load(dir.path()).is_linked());
        // Unlinked, but still bound: it can only ever sync with that shop.
        assert_eq!(reg.active().tenant_id.as_deref(), Some("tnt_b"));
    }

    #[test]
    fn unknown_profile_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = MemoryStore::default();
        let mut reg = Registry::migrate(dir.path()).unwrap();
        assert_eq!(
            reg.activate(dir.path(), &s, "nope").unwrap_err().code,
            "PROFILE_NOT_FOUND"
        );
        assert_eq!(reg.switching, None);
    }

    #[test]
    fn an_interrupted_switch_is_finished_on_the_next_boot() {
        let dir = tempfile::tempdir().unwrap();
        let s = MemoryStore::default();
        let mut reg = shop_a(dir.path(), &s);
        let a = reg.active_id.clone();
        reg.profiles
            .push(Profile::new("shop_b".into(), Some("tnt_b".into())));
        // Crash right after the marker and the park, before restore.
        reg.switching = Some("shop_b".into());
        reg.save(dir.path()).unwrap();
        reg.park_active(dir.path(), &s).unwrap();

        let mut reg = recover(dir.path(), &s).unwrap();
        assert_eq!(reg.active_id, "shop_b");
        assert_eq!(reg.switching, None);
        assert!(!CloudFile::load(dir.path()).is_linked());
        // Shop A is safe and can be switched back to.
        reg.activate(dir.path(), &s, &a).unwrap();
        assert_eq!(s.get(KEY_ACCESS).unwrap().as_deref(), Some("acc_a"));
    }

    #[test]
    fn repeating_a_finished_activation_does_not_unlink_the_shop() {
        let dir = tempfile::tempdir().unwrap();
        let s = MemoryStore::default();
        let mut reg = shop_a(dir.path(), &s);
        let a = reg.active_id.clone();
        // Re-run of a switch whose restore had already happened.
        reg.switching = Some(a.clone());
        reg.activate(dir.path(), &s, &a).unwrap();
        assert!(CloudFile::load(dir.path()).is_linked());
        assert_eq!(s.get(KEY_ACCESS).unwrap().as_deref(), Some("acc_a"));
    }

    #[test]
    fn views_list_the_open_shop_first_and_flag_parked_links() {
        let dir = tempfile::tempdir().unwrap();
        let s = MemoryStore::default();
        let mut reg = shop_a(dir.path(), &s);
        let a = reg.active_id.clone();
        link_b(dir.path(), &s, &mut reg);
        let file = CloudFile::load(dir.path());
        let v = views(&reg, &file);
        assert_eq!(v[0].id, reg.active_id);
        assert!(v[0].active && v[0].linked);
        assert_eq!(v[1].id, a);
        assert!(!v[1].active && v[1].linked);
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.to_lowercase().contains("token"));
    }
}
