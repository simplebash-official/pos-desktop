// Custom document designs ("template packages") for the open shop.
//
// The cloud keeps the designs the owner made for this shop; this computer's
// document-server renders them. After a full sync cycle the agent asks the
// cloud which private designs the shop may use (`GET /api/document-packages`),
// compares that with what the local document-server already holds
// (`GET /api/packages`) and
//   * downloads and installs the designs that are missing (a new version of a
//     design is a new name, so "changed" never means "overwrite"),
//   * takes the local grant away from designs the cloud no longer lists, so
//     they stop being offered. Their files stay: documents already printed
//     with them keep their original look.
// Designs that need a newer app than the one running are skipped and counted.
//
// The decisions (`plan`) are pure; `run` only moves bytes between the two
// sides through the `Cloud` and `Local` traits, which the tests fake.

use std::collections::HashSet;

use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::http::{ErrorKind, SyncError};
use super::state::{PackagesPhase, PackagesStatus};

/// One private design the cloud says the shop may use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePackage {
    pub name: String,
    pub family: String,
    pub version: i64,
    pub kind: String,
    pub title: String,
    pub r#type: String,
    pub sha256: String,
    pub min_app_version: Option<String>,
}

/// One row of the local document-server's `GET /api/packages`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalPackage {
    pub name: String,
    pub family: String,
    pub scope: String,
    pub sha256: String,
    pub grants: Vec<String>,
}

/// The tenant key of a document-server that is not told about tenants: how a
/// desktop sidecar stores the grant of an installed design.
pub const LOCAL_TENANT: &str = "";

#[allow(async_fn_in_trait)]
pub trait Cloud {
    async fn manifest(&self) -> Result<Vec<RemotePackage>, SyncError>;
    async fn archive(&self, name: &str) -> Result<Vec<u8>, SyncError>;
}

#[allow(async_fn_in_trait)]
pub trait Local {
    async fn list(&self) -> Result<Vec<LocalPackage>, SyncError>;
    async fn install(&self, name: &str, sha256: &str, archive: Vec<u8>) -> Result<(), SyncError>;
    /// Takes the shop's grant of `family` away (the files stay).
    async fn revoke(&self, family: &str) -> Result<(), SyncError>;
}

/// What to do to bring the local server in line with the cloud.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Download and install, in manifest order.
    pub install: Vec<RemotePackage>,
    /// Families to take the local grant away from.
    pub revoke: Vec<String>,
    /// Listed by the cloud but need a newer app.
    pub needs_app_update: Vec<RemotePackage>,
}

fn invalid(code: &str, message: impl Into<String>) -> SyncError {
    SyncError::new(ErrorKind::Invalid, code, message, 0)
}

/// `1.22.0` -> (1, 22, 0). Anything else is an error: a version the app cannot
/// read must not silently count as "compatible".
fn parse_version(text: &str) -> Result<(u64, u64, u64), SyncError> {
    let parts: Vec<&str> = text.split('.').collect();
    let numbers: Option<Vec<u64>> = parts.iter().map(|p| p.parse().ok()).collect();
    match numbers.as_deref() {
        Some([major, minor, patch]) => Ok((*major, *minor, *patch)),
        _ => Err(invalid(
            "BAD_VERSION",
            format!("'{text}' is not a version like 1.22.0"),
        )),
    }
}

/// A template name is used as a URL path segment against the local server and
/// as a file name by it, and it arrives from the network: only the shape the
/// document-server generates is accepted.
fn is_template_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("doc_temp_")
        .or_else(|| name.strip_prefix("lbl_temp_"))
    else {
        return false;
    };
    rest.len() == 16
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn check_manifest(remote: &[RemotePackage]) -> Result<(), SyncError> {
    for pkg in remote {
        if !is_template_name(&pkg.name) {
            return Err(invalid(
                "BAD_MANIFEST",
                "the cloud listed a design with an invalid name",
            ));
        }
        if !is_sha256(&pkg.sha256) {
            return Err(invalid(
                "BAD_MANIFEST",
                "the cloud listed a design with an invalid digest",
            ));
        }
    }
    Ok(())
}

pub fn plan(
    remote: &[RemotePackage],
    local: &[LocalPackage],
    app_version: &str,
) -> Result<Plan, SyncError> {
    let app = parse_version(app_version)?;
    let mut plan = Plan::default();

    for pkg in remote {
        if let Some(min) = &pkg.min_app_version {
            if parse_version(min)? > app {
                plan.needs_app_update.push(pkg.clone());
                continue;
            }
        }
        let present = local.iter().any(|l| {
            l.name == pkg.name
                && l.sha256.eq_ignore_ascii_case(&pkg.sha256)
                && l.grants.iter().any(|g| g == LOCAL_TENANT)
        });
        if !present {
            plan.install.push(pkg.clone());
        }
    }

    let listed: HashSet<&str> = remote.iter().map(|p| p.family.as_str()).collect();
    let mut revoked = HashSet::new();
    for l in local {
        if l.scope == "private"
            && l.grants.iter().any(|g| g == LOCAL_TENANT)
            && !listed.contains(l.family.as_str())
            && revoked.insert(l.family.as_str())
        {
            plan.revoke.push(l.family.clone());
        }
    }
    Ok(plan)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A failure that retrying the next package cannot fix: stop the whole run.
fn stops_the_run(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Offline
            | ErrorKind::Unauthorized
            | ErrorKind::Revoked
            | ErrorKind::Suspended
            | ErrorKind::Local
    )
}

fn status(
    state: PackagesPhase,
    installed: usize,
    pending: usize,
    needs_app_update: usize,
    last_error: Option<String>,
) -> PackagesStatus {
    PackagesStatus {
        state,
        installed: installed as u32,
        pending: pending as u32,
        needs_app_update: needs_app_update as u32,
        checked_at: Some(Utc::now().to_rfc3339()),
        last_error,
    }
}

/// One reconciliation. `report` receives the progress ("N still to download")
/// so the Settings screen can show it while the work runs. Returns the final
/// status; a failure that stops the run is returned as `Err` (the caller
/// decides how to show it), one that only affects a single package ends in an
/// `Error` status after the other packages were still handled.
pub async fn run(
    cloud: &impl Cloud,
    local: &impl Local,
    app_version: &str,
    report: &impl Fn(PackagesStatus),
) -> Result<PackagesStatus, SyncError> {
    let remote = cloud.manifest().await?;
    check_manifest(&remote)?;
    let present = local.list().await?;
    let plan = plan(&remote, &present, app_version)?;

    let skipped = plan.needs_app_update.len();
    let wanted = remote.len() - skipped;
    let mut remaining = plan.install.len();
    if remaining > 0 {
        report(status(
            PackagesPhase::Pending,
            wanted - remaining,
            remaining,
            skipped,
            None,
        ));
    }

    let mut first_error: Option<SyncError> = None;
    for pkg in &plan.install {
        let result = install_one(cloud, local, pkg).await;
        match result {
            Ok(()) => {
                remaining -= 1;
                report(status(
                    PackagesPhase::Pending,
                    wanted - remaining,
                    remaining,
                    skipped,
                    None,
                ));
            }
            Err(e) if stops_the_run(e.kind) => return Err(e),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    for family in &plan.revoke {
        local.revoke(family).await?;
    }

    let failed = remaining;
    Ok(match first_error {
        Some(e) => status(
            PackagesPhase::Error,
            wanted - failed,
            failed,
            skipped,
            Some(e.to_string()),
        ),
        None if skipped > 0 => status(PackagesPhase::NeedsAppUpdate, wanted, 0, skipped, None),
        None => status(PackagesPhase::UpToDate, wanted, 0, 0, None),
    })
}

async fn install_one(
    cloud: &impl Cloud,
    local: &impl Local,
    pkg: &RemotePackage,
) -> Result<(), SyncError> {
    let archive = cloud.archive(&pkg.name).await?;
    if !sha256_hex(&archive).eq_ignore_ascii_case(&pkg.sha256) {
        return Err(invalid(
            "PACKAGE_DIGEST_MISMATCH",
            format!("the download of '{}' does not match its digest", pkg.title),
        ));
    }
    local.install(&pkg.name, &pkg.sha256, archive).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn name(n: char) -> String {
        format!("doc_temp_{}", n.to_string().repeat(16))
    }

    fn remote(n: char, family: &str, sha: &str, min: Option<&str>) -> RemotePackage {
        RemotePackage {
            name: name(n),
            family: family.into(),
            version: 1,
            kind: "a4-invoice".into(),
            title: format!("Design {n}"),
            r#type: "document".into(),
            sha256: sha.into(),
            min_app_version: min.map(str::to_string),
        }
    }

    fn local(n: char, family: &str, sha: &str, scope: &str, granted: bool) -> LocalPackage {
        LocalPackage {
            name: name(n),
            family: family.into(),
            scope: scope.into(),
            sha256: sha.into(),
            grants: if granted { vec![String::new()] } else { vec![] },
        }
    }

    #[test]
    fn a_missing_design_is_installed_and_a_present_one_is_left_alone() {
        let remote = [
            remote('a', "fam_a", SHA_A, None),
            remote('b', "fam_b", SHA_A, None),
        ];
        let local = [local('a', "fam_a", SHA_A, "private", true)];
        let plan = plan(&remote, &local, "1.22.0").unwrap();
        assert_eq!(plan.install, vec![remote[1].clone()]);
        assert!(plan.revoke.is_empty() && plan.needs_app_update.is_empty());
    }

    #[test]
    fn a_design_whose_grant_was_taken_away_is_installed_again() {
        let remote = [remote('a', "fam_a", SHA_A, None)];
        let local = [local('a', "fam_a", SHA_A, "private", false)];
        assert_eq!(plan(&remote, &local, "1.22.0").unwrap().install.len(), 1);
    }

    #[test]
    fn a_design_with_a_different_digest_is_installed_again_not_trusted() {
        let other = "b".repeat(64);
        let remote = [remote('a', "fam_a", SHA_A, None)];
        let local = [local('a', "fam_a", &other, "private", true)];
        assert_eq!(plan(&remote, &local, "1.22.0").unwrap().install.len(), 1);
    }

    #[test]
    fn families_the_cloud_no_longer_lists_lose_their_grant_once() {
        let remote = [remote('a', "fam_a", SHA_A, None)];
        let local = [
            local('a', "fam_a", SHA_A, "private", true),
            local('b', "fam_gone", SHA_A, "private", true),
            local('c', "fam_gone", SHA_A, "private", true),
            local('d', "fam_old", SHA_A, "private", false),
            local('e', "fam_bundled", SHA_A, "global", true),
        ];
        let plan = plan(&remote, &local, "1.22.0").unwrap();
        assert_eq!(plan.revoke, vec!["fam_gone".to_string()]);
    }

    #[test]
    fn a_design_needing_a_newer_app_is_skipped_and_its_family_is_kept() {
        let listed = [remote('b', "fam_a", SHA_A, Some("1.23.0"))];
        let local = [local('a', "fam_a", SHA_A, "private", true)];
        let plan = plan(&listed, &local, "1.22.5").unwrap();
        assert!(plan.install.is_empty());
        assert!(plan.revoke.is_empty());
        assert_eq!(plan.needs_app_update.len(), 1);
        // The same version and a lower one are fine.
        let ok = [remote('b', "fam_a", SHA_A, Some("1.22.5"))];
        assert_eq!(plan_install_len(&ok, "1.22.5"), 1);
        assert_eq!(plan_install_len(&ok, "1.22.4"), 0);
    }

    fn plan_install_len(remote: &[RemotePackage], app: &str) -> usize {
        plan(remote, &[], app).unwrap().install.len()
    }

    #[test]
    fn unreadable_versions_are_errors_not_compatibility() {
        let bad = [remote('a', "fam_a", SHA_A, Some("soon"))];
        assert_eq!(plan(&bad, &[], "1.22.0").unwrap_err().code, "BAD_VERSION");
        assert_eq!(
            plan(&[], &[], "1.22.0-beta").unwrap_err().code,
            "BAD_VERSION"
        );
    }

    #[test]
    fn names_and_digests_from_the_network_are_checked() {
        assert!(is_template_name(&name('a')));
        assert!(is_template_name("lbl_temp_abcdefgh-_123456"));
        for bad in [
            "",
            "doc_temp_short",
            "../doc_temp_aaaaaaaaaaaaaaaa",
            "doc_temp_aaaaaaaaaaaaaaa/",
        ] {
            assert!(!is_template_name(bad), "{bad}");
        }
        let mut pkg = remote('a', "fam_a", SHA_A, None);
        pkg.name = "../etc".into();
        assert_eq!(check_manifest(&[pkg]).unwrap_err().code, "BAD_MANIFEST");
        let mut pkg = remote('a', "fam_a", "zz", None);
        pkg.sha256 = "zz".into();
        assert_eq!(check_manifest(&[pkg]).unwrap_err().code, "BAD_MANIFEST");
    }

    // ----- run() against fakes --------------------------------------------

    struct FakeCloud {
        manifest: Vec<RemotePackage>,
        archives: Vec<(String, Vec<u8>)>,
        fail_manifest: Option<SyncError>,
    }

    impl Cloud for FakeCloud {
        async fn manifest(&self) -> Result<Vec<RemotePackage>, SyncError> {
            match &self.fail_manifest {
                Some(e) => Err(e.clone()),
                None => Ok(self.manifest.clone()),
            }
        }
        async fn archive(&self, name: &str) -> Result<Vec<u8>, SyncError> {
            self.archives
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| SyncError::new(ErrorKind::Invalid, "NOT_FOUND", "no archive", 404))
        }
    }

    #[derive(Default)]
    struct FakeLocal {
        present: Vec<LocalPackage>,
        installed: RefCell<Vec<String>>,
        revoked: RefCell<Vec<String>>,
        reject_install: Option<SyncError>,
    }

    impl Local for FakeLocal {
        async fn list(&self) -> Result<Vec<LocalPackage>, SyncError> {
            Ok(self.present.clone())
        }
        async fn install(
            &self,
            name: &str,
            sha256: &str,
            archive: Vec<u8>,
        ) -> Result<(), SyncError> {
            assert_eq!(sha256_hex(&archive), sha256);
            if let Some(e) = &self.reject_install {
                return Err(e.clone());
            }
            self.installed.borrow_mut().push(name.to_string());
            Ok(())
        }
        async fn revoke(&self, family: &str) -> Result<(), SyncError> {
            self.revoked.borrow_mut().push(family.to_string());
            Ok(())
        }
    }

    fn with_archive(
        n: char,
        family: &str,
        min: Option<&str>,
    ) -> (RemotePackage, (String, Vec<u8>)) {
        let bytes = format!("zip of {n}").into_bytes();
        let pkg = remote(n, family, &sha256_hex(&bytes), min);
        (pkg.clone(), (pkg.name, bytes))
    }

    #[tokio::test]
    async fn run_installs_revokes_and_reports_progress() {
        let (a, a_zip) = with_archive('a', "fam_a", None);
        let (b, b_zip) = with_archive('b', "fam_b", None);
        let cloud = FakeCloud {
            manifest: vec![a, b],
            archives: vec![a_zip, b_zip],
            fail_manifest: None,
        };
        let local = FakeLocal {
            present: vec![local('z', "fam_gone", SHA_A, "private", true)],
            ..FakeLocal::default()
        };
        let seen = RefCell::new(Vec::new());
        let result = run(&cloud, &local, "1.22.0", &|s| {
            seen.borrow_mut().push((s.state, s.installed, s.pending))
        })
        .await
        .unwrap();

        assert_eq!(*local.installed.borrow(), vec![name('a'), name('b')]);
        assert_eq!(*local.revoked.borrow(), vec!["fam_gone".to_string()]);
        assert_eq!(
            *seen.borrow(),
            vec![
                (PackagesPhase::Pending, 0, 2),
                (PackagesPhase::Pending, 1, 1),
                (PackagesPhase::Pending, 2, 0),
            ]
        );
        assert_eq!(
            (result.state, result.installed, result.pending),
            (PackagesPhase::UpToDate, 2, 0)
        );
        assert!(result.checked_at.is_some() && result.last_error.is_none());
    }

    #[tokio::test]
    async fn run_with_nothing_to_do_is_up_to_date_and_silent() {
        let (a, a_zip) = with_archive('a', "fam_a", None);
        let cloud = FakeCloud {
            manifest: vec![a.clone()],
            archives: vec![a_zip],
            fail_manifest: None,
        };
        let local = FakeLocal {
            present: vec![local('a', "fam_a", &a.sha256, "private", true)],
            ..FakeLocal::default()
        };
        let calls = RefCell::new(0);
        let result = run(&cloud, &local, "1.22.0", &|_| *calls.borrow_mut() += 1)
            .await
            .unwrap();
        assert_eq!(
            (*calls.borrow(), result.state, result.installed),
            (0, PackagesPhase::UpToDate, 1)
        );
        assert!(local.installed.borrow().is_empty());
    }

    #[tokio::test]
    async fn run_reports_designs_that_need_an_app_update() {
        let (a, a_zip) = with_archive('a', "fam_a", Some("9.0.0"));
        let (b, b_zip) = with_archive('b', "fam_b", None);
        let cloud = FakeCloud {
            manifest: vec![a, b],
            archives: vec![a_zip, b_zip],
            fail_manifest: None,
        };
        let local = FakeLocal::default();
        let result = run(&cloud, &local, "1.22.0", &|_| {}).await.unwrap();
        assert_eq!(*local.installed.borrow(), vec![name('b')]);
        assert_eq!(
            (result.state, result.installed, result.needs_app_update),
            (PackagesPhase::NeedsAppUpdate, 1, 1)
        );
    }

    #[tokio::test]
    async fn a_corrupted_download_is_not_installed() {
        let (a, _) = with_archive('a', "fam_a", None);
        let cloud = FakeCloud {
            manifest: vec![a.clone()],
            archives: vec![(a.name.clone(), b"tampered".to_vec())],
            fail_manifest: None,
        };
        let local = FakeLocal::default();
        let result = run(&cloud, &local, "1.22.0", &|_| {}).await.unwrap();
        assert!(local.installed.borrow().is_empty());
        assert_eq!((result.state, result.pending), (PackagesPhase::Error, 1));
        assert!(result
            .last_error
            .unwrap()
            .contains("PACKAGE_DIGEST_MISMATCH"));
    }

    #[tokio::test]
    async fn one_bad_package_does_not_block_the_others() {
        let (a, _) = with_archive('a', "fam_a", None);
        let (b, b_zip) = with_archive('b', "fam_b", None);
        let cloud = FakeCloud {
            manifest: vec![a, b],
            archives: vec![b_zip],
            fail_manifest: None,
        };
        let local = FakeLocal::default();
        let result = run(&cloud, &local, "1.22.0", &|_| {}).await.unwrap();
        assert_eq!(*local.installed.borrow(), vec![name('b')]);
        assert_eq!(
            (result.state, result.installed, result.pending),
            (PackagesPhase::Error, 1, 1)
        );
    }

    #[tokio::test]
    async fn a_suspended_account_stops_the_run_untouched() {
        let suspended = SyncError::new(ErrorKind::Suspended, "ACCOUNT_SUSPENDED", "off", 403);
        let cloud = FakeCloud {
            manifest: vec![],
            archives: vec![],
            fail_manifest: Some(suspended.clone()),
        };
        let local = FakeLocal {
            present: vec![local('z', "fam_gone", SHA_A, "private", true)],
            ..FakeLocal::default()
        };
        let err = run(&cloud, &local, "1.22.0", &|_| {}).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Suspended);
        assert!(
            local.revoked.borrow().is_empty(),
            "nothing is revoked when the cloud cannot answer"
        );
    }

    #[tokio::test]
    async fn a_local_server_failure_stops_the_run() {
        let (a, a_zip) = with_archive('a', "fam_a", None);
        let (b, b_zip) = with_archive('b', "fam_b", None);
        let cloud = FakeCloud {
            manifest: vec![a, b],
            archives: vec![a_zip, b_zip],
            fail_manifest: None,
        };
        let local = FakeLocal {
            reject_install: Some(SyncError::new(
                ErrorKind::Local,
                "LOCAL_UNAVAILABLE",
                "down",
                0,
            )),
            ..FakeLocal::default()
        };
        assert_eq!(
            run(&cloud, &local, "1.22.0", &|_| {})
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Local
        );
    }
}
