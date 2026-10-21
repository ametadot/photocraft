//! Cooperative native writer coordination, independent of the recovery root.
//!
//! A single never-unlinked lock protects a bounded durable registry. Claims survive
//! process exit and are retired only after the private operation acknowledges its
//! outcome. Dynamic same-file checks cover aliases of a newly replaced inode.
//! This coordinates participating writers for one OS user, not external editors.

use super::commit::{Operation, Outcome, Plan, physical_identity};
use super::store::StoreError;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CLAIMS: usize = 4096;
const INITIALIZED: &[u8] = b"PhotoCraft target coordinator v1: registry initialized\n";

#[cfg(test)]
fn init_boundary(stage: &str) {
    if std::env::var("PHOTOCRAFT_COORDINATOR_INIT_STOP").is_ok_and(|requested| requested == stage) {
        std::process::exit(0);
    }
}

fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::Invalid(message.into())
}

fn parent_file(path: &Path) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0200_0000); // FILE_FLAG_BACKUP_SEMANTICS: directory handle.
    }
    Ok(options.open(path)?)
}

fn has_one_link(file: &File) -> Result<bool, StoreError> {
    #[cfg(windows)]
    {
        Ok(winapi_util::file::information(file)?.number_of_links() == 1)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(file.metadata()?.nlink() == 1)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = file;
        Ok(false)
    }
}

type Created = (u64, u32);

fn created(file: &File) -> Option<Created> {
    let time = file.metadata().ok()?.created().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((time.as_secs(), time.subsec_nanos()))
}

fn same_incarnation(identity: &str, birth: Option<Created>, other: &str, other_birth: Option<Created>) -> bool {
    identity == other && !matches!((birth, other_birth), (Some(a), Some(b)) if a != b)
}

fn target_key(target: &Path) -> Result<(PathBuf, String, String, Option<Created>), StoreError> {
    // Normalize the FINAL existing filename too (including Windows short names),
    // not only its parent. Distinct normalized names are used by the one-link proof.
    let target = if target.try_exists()? { std::fs::canonicalize(target)? } else { target.to_owned() };
    let parent = target.parent().ok_or_else(|| invalid("Sidecar target has no parent"))?;
    let parent = std::fs::canonicalize(parent)?;
    let filename = target.file_name().and_then(|n| n.to_str()).ok_or_else(|| invalid("Sidecar target name is not Unicode"))?;
    let key_name = if cfg!(windows) { filename.to_lowercase() } else { filename.to_owned() };
    let handle = parent_file(&parent)?;
    Ok((parent.join(filename), physical_identity(&handle)?, key_name, created(&handle)))
}

/// The explicit override is also required for UI/integration test binaries linking
/// non-test IO. Child processes inherit it. It must not be derived from recovery.
pub(super) fn root() -> Result<PathBuf, StoreError> {
    if let Some(path) = std::env::var_os("PHOTOCRAFT_TARGET_COORDINATION_ROOT") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(invalid("Target coordination root must be absolute"));
        }
        return Ok(path);
    }
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!("photocraft-target-tests-{}", std::process::id())))
    }
    #[cfg(not(test))]
    {
        #[cfg(windows)]
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"));
        let base = base.filter(|path| path.is_absolute()).ok_or_else(|| invalid("Stable private target coordination directory unavailable"))?;
        Ok(base.join("PhotoCraft/sidecar-coordination-v1"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    owner: PathBuf,
    session: String,
    operation: String,
    photo: super::PhotoId,
    target: PathBuf,
    parent_identity: String,
    // Legacy or unsupported birth times remain conservative. A known difference
    // distinguishes recycled directory inodes without changing source bindings.
    #[serde(default)]
    parent_created: Option<Created>,
    filename: String,
    physical: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    schema: u32,
    claims: Vec<Claim>,
}

pub(super) struct Coordinator {
    _lock: File,
    path: PathBuf,
    state: Registry,
}

impl Coordinator {
    pub(super) fn acquire() -> Result<Self, StoreError> {
        Self::at(&root()?)
    }

    fn at(root: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(root)?;
        if std::fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(invalid("Linked coordinator root refused"));
        }
        let lock_path = root.join("ownership.lock");
        if lock_path.try_exists()? && std::fs::symlink_metadata(&lock_path)?.file_type().is_symlink() {
            return Err(invalid("Linked coordinator lock refused"));
        }
        let lock = match OpenOptions::new().read(true).write(true).create_new(true).open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new().read(true).write(true).open(&lock_path)?,
            Err(error) => return Err(error.into()),
        };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < until => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Owned),
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        let mut marker = Vec::new();
        (&lock).take(INITIALIZED.len() as u64 + 1).read_to_end(&mut marker)?;
        if !marker.is_empty() && marker != INITIALIZED {
            return Err(invalid("Invalid coordinator initialization marker; records retained"));
        }
        #[cfg(test)]
        init_boundary("before-registry");
        let path = root.join("registry.json");
        if path.try_exists()? && std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(invalid("Linked coordinator registry refused"));
        }
        let mut needs_registry = false;
        let state = match super::store::read_record(&path) {
            Ok(bytes) => {
                if bytes.len() as u64 > MAX_BYTES {
                    return Err(invalid("Target registry exceeds bound"));
                }
                serde_json::from_slice::<Registry>(&bytes).map_err(|e| invalid(format!("Invalid target registry: {e}")))?
            }
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound && marker.is_empty() => {
                needs_registry = true;
                Registry { schema: 1, claims: vec![] }
            }
            Err(error) => return Err(invalid(format!("Target registry missing or unreadable; publication refused: {error}"))),
        };
        if state.schema != 1
            || state.claims.len() > MAX_CLAIMS
            || state.claims.iter().any(|c| {
                !c.owner.is_absolute()
                    || !c.target.is_absolute()
                    || c.operation.len() != 64
                    || !c.operation.bytes().all(|b| b.is_ascii_hexdigit())
                    || c.session.is_empty()
                    || c.parent_identity.is_empty()
                    || c.parent_created.is_some_and(|(_, nanos)| nanos >= 1_000_000_000)
                    || c.filename.is_empty()
            })
        {
            return Err(invalid("Invalid target coordination authority"));
        }
        let mut coordinator = Self { _lock: lock, path, state };
        if needs_registry {
            coordinator.persist()?;
        }
        #[cfg(test)]
        init_boundary("after-registry");
        if marker.is_empty() {
            // The marker is never replaced/unlinked: publish and sync it only after
            // an authoritative registry is durable, before returning for any claim.
            (&coordinator._lock).write_all(INITIALIZED)?;
            coordinator._lock.sync_all()?;
        }
        Ok(coordinator)
    }

    fn persist(&mut self) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(&self.state).map_err(|e| invalid(e.to_string()))?;
        if bytes.len() as u64 > MAX_BYTES || self.state.claims.len() > MAX_CLAIMS {
            return Err(invalid("Target registry capacity reached; obligations preserved"));
        }
        super::store::write_record(&self.path, &bytes)
    }

    fn claims(owner: &Path, plan: &Plan) -> Result<Vec<Claim>, StoreError> {
        let owner = std::fs::canonicalize(owner)?;
        plan.photos
            .iter()
            .map(|photo| {
                let (target, parent_identity, filename, parent_created) = target_key(&photo.binding.target)?;
                Ok(Claim {
                    owner: owner.clone(),
                    session: plan.session.clone(),
                    operation: plan.id.clone(),
                    photo: photo.photo.clone(),
                    target,
                    parent_identity,
                    parent_created,
                    filename,
                    physical: photo.binding.target_identity.clone(),
                })
            })
            .collect()
    }

    fn overlaps(a: &Claim, b: &Claim) -> Result<bool, StoreError> {
        // Exact paths remain reserved even when their parent directory was replaced.
        // An absent Windows target has no final-name canonicalization; normalize
        // its full path case consistently with the existing filename key.
        let exact_path =
            if cfg!(windows) { a.target.to_string_lossy().to_lowercase() == b.target.to_string_lossy().to_lowercase() } else { a.target == b.target };
        // Equal/unknown creation times never relax a captured identity conflict.
        if exact_path
            || (same_incarnation(&a.parent_identity, a.parent_created, &b.parent_identity, b.parent_created) && a.filename == b.filename)
            || (a.physical.is_some() && a.physical == b.physical)
        {
            return Ok(true);
        }
        // An absent proposed target cannot be a physical alias. Check it first:
        // an unavailable unrelated old target must not prevent creating this one.
        if !b.target.try_exists()? || !a.target.try_exists()? {
            return Ok(false);
        }
        // A one-link regular file cannot be the same inode as a distinct regular
        // directory entry. Verify the prior entry is not a symlink and its parent
        // has not been substituted before using this proof. This permits an
        // independent ordinary XMP save while an unrelated target is read-locked.
        if has_one_link(&File::open(&b.target)?)?
            && std::fs::symlink_metadata(&a.target)?.file_type().is_file()
            && a.target.parent().is_some_and(|parent| {
                parent_file(parent).is_ok_and(|file| {
                    physical_identity(&file).is_ok_and(|identity| same_incarnation(&a.parent_identity, a.parent_created, &identity, created(&file)))
                })
            })
        {
            return Ok(false);
        }
        // Resolve now, not just against the inode captured before atomic replacement.
        // Multi-link/unknown identities remain fail-closed on inspection errors.
        Ok(same_file::is_same_file(&a.target, &b.target)?)
    }

    pub(super) fn check(&self, owner: &Path, plan: &Plan) -> Result<(), StoreError> {
        for proposed in Self::claims(owner, plan)? {
            for prior in &self.state.claims {
                if prior.owner == proposed.owner && prior.operation == proposed.operation {
                    continue;
                }
                if Self::overlaps(prior, &proposed)? {
                    return Err(invalid(
                        "Another Camera Raw save has an unresolved target claim; reconcile its original session before saving. Nothing was written",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn begin(&mut self, owner: &Path, plan: &Plan) -> Result<(), StoreError> {
        self.check(owner, plan)?;
        let claims = Self::claims(owner, plan)?;
        if self.state.claims.iter().any(|c| claims.iter().any(|n| c.owner == n.owner && c.operation == n.operation)) {
            return Err(invalid("Existing target claim requires reconciliation, not replay"));
        }
        self.state.claims.extend(claims);
        self.persist()
    }

    /// Call only AFTER the corresponding complete operation record is acknowledged.
    pub(super) fn acknowledge(&mut self, owner: &Path, operation: &Operation) -> Result<(), StoreError> {
        let owner = std::fs::canonicalize(owner)?;
        self.state.claims.retain(|claim| {
            if claim.owner != owner || claim.operation != operation.plan.id {
                return true;
            }
            operation.uncertain_photos.contains(&claim.photo)
                || matches!(operation.outcomes.get(&claim.photo), Some(Outcome::Unknown(_)))
                || (operation.outcomes.get(&claim.photo) == Some(&Outcome::Pending) && !operation.confirmed_unwritten.contains(&claim.photo))
        });
        self.persist()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!("pc-coordinator-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn claim(root: &Path, name: &str) -> Claim {
        Claim {
            owner: root.to_owned(),
            session: "session".into(),
            operation: "a".repeat(64),
            photo: super::super::PhotoId("p".into()),
            target: root.join(name),
            parent_identity: physical_identity(&parent_file(root).unwrap()).unwrap(),
            parent_created: created(&parent_file(root).unwrap()),
            filename: name.into(),
            physical: None,
        }
    }

    #[test]
    fn recycled_parent_identity_does_not_block_an_independent_missing_target() {
        let root = fixture();
        let mut prior = claim(&root, "photo.xmp");
        let mut next = prior.clone();
        // Deterministic inode reuse: distinct paths, same captured device/inode,
        // different known directory births. No claim is discarded or rewritten.
        next.target = root.join("new-parent/photo.xmp");
        prior.parent_created = Some((100, 1));
        next.parent_created = Some((100, 2));
        assert!(!Coordinator::overlaps(&prior, &next).unwrap());
        next.target = prior.target.clone();
        assert!(Coordinator::overlaps(&prior, &next).unwrap(), "exact path remains reserved after parent replacement");
        next.target = root.join("new-parent/photo.xmp");
        for (a, b) in [(Some((100, 1)), Some((100, 1))), (None, Some((100, 2))), (Some((100, 1)), None), (None, None)] {
            prior.parent_created = a;
            next.parent_created = b;
            assert!(Coordinator::overlaps(&prior, &next).unwrap(), "equal/unknown birth is conservative");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn absent_target_case_variant_remains_reserved_after_parent_replacement() {
        let root = fixture();
        let prior = claim(&root, "Photo.XMP");
        let mut next = claim(&root, "photo.xmp");
        next.parent_identity = "replaced-parent".into();
        next.parent_created = Some((1, 2));
        assert!(!prior.target.exists() && !next.target.exists());
        assert!(Coordinator::overlaps(&prior, &next).unwrap(), "case spelling cannot bypass the exact logical target reservation");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_parent_identity_and_registry_roundtrip_remain_conservative() {
        let root = fixture();
        let mut prior = claim(&root, "photo.xmp");
        prior.parent_created = Some((100, 1));
        let mut encoded = serde_json::to_value(&prior).unwrap();
        encoded.as_object_mut().unwrap().remove("parent_created");
        let legacy: Claim = serde_json::from_value(encoded).unwrap();
        assert_eq!(legacy.parent_created, None);
        let mut next = prior.clone();
        next.target = root.join("other/photo.xmp");
        next.parent_created = Some((100, 2));
        assert!(Coordinator::overlaps(&legacy, &next).unwrap());
        let mut coordinator = Coordinator::at(&root.join("coordination")).unwrap();
        coordinator.state.claims.push(prior);
        coordinator.persist().unwrap();
        drop(coordinator);
        let coordinator = Coordinator::at(&root.join("coordination")).unwrap();
        assert_eq!(coordinator.state.claims[0].parent_created, Some((100, 1)));
        assert!(!Coordinator::overlaps(&coordinator.state.claims[0], &next).unwrap());
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn known_parent_replacement_never_satisfies_incarnation_proof() {
        let root = fixture();
        let file = parent_file(&root).unwrap();
        let identity = physical_identity(&file).unwrap();
        let birth = created(&file);
        assert!(same_incarnation(&identity, birth, &identity, birth));
        assert!(!same_incarnation(&identity, Some((100, 1)), &identity, Some((100, 2))));
        assert!(same_incarnation(&identity, None, &identity, birth));
        drop(file);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dynamic_alias_of_replaced_inode_is_still_protected() {
        let root = fixture();
        let path = root.join("photo.xmp");
        std::fs::write(&path, b"old").unwrap();
        let mut prior = claim(&root, "photo.xmp");
        prior.physical = Some(physical_identity(&File::open(&path).unwrap()).unwrap());
        photocraft_format::atomic::atomic_write(&path, b"new").unwrap();
        let alias = root.join("alias.xmp");
        std::fs::hard_link(&path, &alias).unwrap();
        let mut next = claim(&root, "alias.xmp");
        next.physical = Some(physical_identity(&File::open(&alias).unwrap()).unwrap());
        assert_ne!(prior.physical, next.physical);
        assert!(Coordinator::overlaps(&prior, &next).unwrap());
        assert!(!Coordinator::overlaps(&prior, &claim(&root, "unrelated.xmp")).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn existing_short_name_normalizes_to_same_final_target_key() {
        use std::os::windows::process::CommandExt;
        let root = fixture();
        let long = root.join("a long sidecar filename.xmp");
        std::fs::write(&long, b"packet").unwrap();
        // Constant command, no path interpolation: ask cmd for this fixture's 8.3
        // spelling. Filesystems with 8.3 disabled report the long name unchanged.
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c"])
            .raw_arg("for %I in (\"a long sidecar filename.xmp\") do @echo %~sI")
            .creation_flags(0x0800_0000)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let short = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        let short = if short.is_absolute() { short } else { root.join(short) };
        assert_eq!(target_key(&long).unwrap(), target_key(&short).unwrap());
        if short.to_string_lossy().contains('~') {
            println!("TARGET-SHORT-NAME PASS: actual alternate short spelling canonicalizes to same key");
        } else {
            println!("TARGET-SHORT-NAME ENVIRONMENT BLOCKED: filesystem did not supply an alternate 8.3 spelling; canonical-name equality only");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn unavailable_distinct_regular_target_does_not_block_independent_work() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture();
        std::fs::write(root.join("locked.xmp"), b"uncertain").unwrap();
        std::fs::write(root.join("independent.xmp"), b"independent").unwrap();
        let prior = claim(&root, "locked.xmp");
        let held = OpenOptions::new().read(true).share_mode(0).open(&prior.target).unwrap();
        assert!(!Coordinator::overlaps(&prior, &claim(&root, "new.xmp")).unwrap());
        assert!(!Coordinator::overlaps(&prior, &claim(&root, "independent.xmp")).unwrap());
        assert!(Coordinator::overlaps(&prior, &claim(&root, "locked.xmp")).unwrap());
        let mut replaced_parent = prior.clone();
        let (seconds, nanos) = replaced_parent.parent_created.expect("Windows creation time available");
        replaced_parent.parent_created = Some((seconds ^ 1, nanos));
        assert!(
            Coordinator::overlaps(&replaced_parent, &claim(&root, "independent.xmp")).is_err(),
            "known parent incarnation mismatch cannot use the one-link independence shortcut while prior target is inaccessible"
        );
        drop(held);
        std::fs::hard_link(root.join("locked.xmp"), root.join("alias.xmp")).unwrap();
        let held = OpenOptions::new().read(true).share_mode(0).open(&prior.target).unwrap();
        assert!(Coordinator::overlaps(&prior, &claim(&root, "alias.xmp")).is_err(), "unreadable physical alias never becomes safe");
        drop(held);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durable_claim_survives_handle_exit_without_private_journal() {
        let root = fixture();
        let mut coordinator = Coordinator::at(&root.join("coordination")).unwrap();
        coordinator.state.claims.push(claim(&root, "photo.xmp"));
        coordinator.persist().unwrap();
        drop(coordinator);
        let again = Coordinator::at(&root.join("coordination")).unwrap();
        assert_eq!(again.state.claims.len(), 1);
        assert!(Coordinator::overlaps(&again.state.claims[0], &claim(&root, "photo.xmp")).unwrap());
        drop(again);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_registry_beside_existing_lock_refuses_reinitialization() {
        let root = fixture();
        drop(Coordinator::at(&root).unwrap());
        std::fs::remove_file(root.join("registry.json")).unwrap();
        assert!(Coordinator::at(&root).is_err());
        assert!(!root.join("registry.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn initialization_crash_before_or_after_empty_registry_can_restart() {
        const CHILD: &str = "PHOTOCRAFT_COORDINATOR_INIT_CHILD_ROOT";
        if let Some(root) = std::env::var_os(CHILD) {
            let _ = Coordinator::at(Path::new(&root)).unwrap();
            assert!(std::env::var_os("PHOTOCRAFT_COORDINATOR_INIT_STOP").is_none(), "child must stop at requested initialization boundary");
            return;
        }
        for stage in ["before-registry", "after-registry"] {
            let root = fixture();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "raw_session::target_coordinator::tests::initialization_crash_before_or_after_empty_registry_can_restart", "--nocapture"])
                .env(CHILD, &root)
                .env("PHOTOCRAFT_COORDINATOR_INIT_STOP", stage)
                .status()
                .unwrap();
            assert!(status.success());
            assert!(std::fs::read(root.join("ownership.lock")).unwrap().is_empty());
            let resumed = Coordinator::at(&root).unwrap();
            assert!(resumed.state.claims.is_empty());
            drop(resumed);
            assert_eq!(std::fs::read(root.join("ownership.lock")).unwrap(), INITIALIZED);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn corrupt_initialization_marker_never_authorizes_registry_reset() {
        let root = fixture();
        std::fs::write(root.join("ownership.lock"), b"partial-marker").unwrap();
        assert!(Coordinator::at(&root).is_err());
        assert!(!root.join("registry.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_registry_refuses_reinitialization() {
        let root = fixture();
        drop(Coordinator::at(&root).unwrap());
        std::fs::write(root.join("registry.json"), b"{}").unwrap();
        assert!(Coordinator::at(&root).is_err());
        assert_eq!(std::fs::read(root.join("registry.json")).unwrap(), b"{}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn unreadable_registry_refuses_and_failed_retirement_preserves_durable_claim() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture();
        let mut coordinator = Coordinator::at(&root.join("coordination")).unwrap();
        coordinator.state.claims.push(claim(&root, "photo.xmp"));
        coordinator.persist().unwrap();
        let registry = root.join("coordination/registry.json");
        let held = OpenOptions::new().read(true).share_mode(0).open(&registry).unwrap();
        coordinator.state.claims.clear();
        assert!(coordinator.persist().is_err());
        drop(coordinator);
        assert!(Coordinator::at(&root.join("coordination")).is_err());
        drop(held);
        let reopened = Coordinator::at(&root.join("coordination")).unwrap();
        assert_eq!(reopened.state.claims.len(), 1, "failed acknowledgement never retires durable protection");
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_identity_is_stable_across_empty_registry_reopens() {
        let root = fixture();
        drop(Coordinator::at(&root).unwrap());
        let before = physical_identity(&File::open(root.join("ownership.lock")).unwrap()).unwrap();
        drop(Coordinator::at(&root).unwrap());
        assert_eq!(physical_identity(&File::open(root.join("ownership.lock")).unwrap()).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
    }
}
