//! Serialized, process-owned Camera Raw checkpoint store. All paths are selected
//! beneath the injected private root; this service never accepts a sidecar path.
//! Authority is fail-closed: corrupt lifecycle records cannot resurrect old work.

use super::{PhotoId, Session};
use photocraft_format::atomic::{PublicationOutcome, atomic_write_outcome};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;
const MAX_GENERATIONS: usize = 4096;
const MAX_OPERATIONS: usize = 128;
const MAX_HISTORY: usize = 4096;
const MAX_AUTHORITY_BYTES: usize = 4 * 1024 * 1024;
const SOFT_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const RETIRED_AGE_SECS: u64 = 30 * 24 * 60 * 60;
pub(super) const MAX_PAYLOAD_BYTES: usize = 12 * 1024 * 1024;
pub(super) const MAX_BOUND_XMP_BYTES: usize = 1024 * 1024;
pub(super) const MAX_SESSION_XMP_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_PLAN_XMP_BYTES: usize = 5 * 1024 * 1024;

#[path = "bounds.rs"]
mod bounds;
pub(super) use bounds::{validate_session_bounds, validate_snapshot};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Camera Raw recovery I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("Camera Raw recovery validation: {0}")]
    Invalid(String),
    #[error("Unsupported recovery schema; original records retained: {0}")]
    Unsupported(String),
    #[error("Recovery publication may have completed; reconcile before continuing: {0}")]
    Uncertain(String),
    #[error("XMP publication was attempted but its outcome is not acknowledged: {0}")]
    SidecarUnknown(String),
    #[error("Camera Raw session is owned by another process")]
    Owned,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    number: u64,
    checksum: String,
    time_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema: u32,
    session: String,
    epoch: u64,
    terminal: bool,
    removed: BTreeSet<PhotoId>,
    generations: Vec<Generation>,
    pins: BTreeSet<u64>,
    operations: BTreeSet<String>,
    superseded_operations: std::collections::BTreeMap<String, String>,
    /// A lifecycle transition cannot be undone by falling back to an older
    /// otherwise valid edit snapshot after the latest file becomes unreadable.
    #[serde(default)]
    min_recovery_generation: u64,
    #[serde(default)]
    entry_generation: Option<u64>,
    /// True when the photographer opted into long-session recovery for this session (and
    /// for records written before this field existed). False when the store exists only as
    /// the publication journal of an explicit Done, Open Images or removal with recovery
    /// off: such a session is offered after an interruption only if it attempted a
    /// publication, never as an editing draft.
    #[serde(default = "legacy_automatic")]
    automatic: bool,
}

fn legacy_automatic() -> bool {
    true
}

/// The refusal when a listed session changed before its discard.
pub const LISTING_CHANGED: &str = "This interrupted session changed since it was listed (another PhotoCraft may have resumed it); review it again";

/// What the chooser listed for a session: its ownership epoch and newest checkpoint. A
/// discard is refused when either changed since (another PhotoCraft resumed it meanwhile).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub epoch: u64,
    pub generation: u64,
    pub checksum: String,
}

#[derive(Clone, Debug)]
pub struct RecoveryCandidate {
    pub id: String,
    /// None for a problem row.
    pub listed: Option<Listed>,
    /// Time of the checkpoint a resume would actually restore (the latest valid one).
    pub checkpoint_time_ms: Option<u64>,
    pub photo_count: Option<usize>,
    /// Bounded source samples from the acknowledged record, without opening files.
    pub source_context: Vec<String>,
    /// Enumeration retains corrupt/unreadable drafts as explicit rows.
    pub problem: Option<String>,
    /// Retained photos whose checkpoint recipe differs from its saved-XMP baseline.
    pub unsaved_photos: Option<usize>,
    /// The latest checkpoint was unusable; `checkpoint_time_ms` names an older fallback,
    /// and this is the unusable latest checkpoint's time.
    pub newer_unusable_time_ms: Option<u64>,
    /// Explicit-save journals that recorded a publication (finished or not).
    pub operations: usize,
    /// False for a recovery-off publication journal (see [`Authority::automatic`]).
    pub automatic: bool,
}

/// A session directory holding nothing but the lock a claim creates first is not a
/// session: `claim` would start it afresh. Completed reclamation removes the authority
/// last, so an interrupted removal can leave the same residue.
/// A folder that vanished while being read (reclaimed meanwhile) is residue too.
fn lifecycle_residue(dir: &Path) -> Result<bool, StoreError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        if entry?.file_name() != "ownership.lock" {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A running PhotoCraft holds the session's exclusive ownership lock for its whole
/// visit. Probing with a shared lock creates nothing and never claims the session.
fn owned_by_running_process(dir: &Path) -> Result<bool, StoreError> {
    let file = match File::open(dir.join("ownership.lock")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// A non-terminal session with nothing to recover or protect: it never acknowledged a
/// checkpoint (a failed first opt-in, a crash before the entry checkpoint), or its last
/// checkpoint retains no photograph (an interrupted Cancel or an empty session) — and it
/// holds no publication journal.
/// Only the newest recorded checkpoint decides: one that cannot be read or verified right
/// now (a sharing violation, a truncated file, a lifecycle fence) counts as work. Falling
/// back to an older, emptier checkpoint, such as the entry checkpoint, would delete a draft.
fn nothing_to_recover(dir: &Path, authority: &Authority, id: &str) -> bool {
    if !authority.operations.is_empty() || !authority.superseded_operations.is_empty() {
        return false;
    }
    let Some(newest) = authority.generations.last() else {
        return true;
    };
    if newest.number < authority.min_recovery_generation {
        return false;
    }
    let Ok(bytes) = read_record(&dir.join(format!("generation-{}.json", newest.number))) else {
        return false;
    };
    if blake3::hash(&bytes).to_hex().as_str() != newest.checksum {
        return false;
    }
    validate_snapshot(&bytes).is_ok_and(|session| {
        session.id == id && session.generation == newest.number && session.photos.iter().all(|photo| authority.removed.contains(&photo.id))
    })
}

fn session_directory(entry: &std::fs::DirEntry) -> Result<Option<String>, StoreError> {
    // Never follow a link out of the private root.
    if !entry.file_type()?.is_dir() {
        return Ok(None);
    }
    let Some(id) = entry.file_name().to_str().map(str::to_owned) else { return Ok(None) };
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
        return Ok(None);
    }
    Ok(Some(id))
}

/// Read-only listing: never claims ownership, advances an epoch or opens a
/// source. A subsequent resume revalidates authority under the real owner lock.
/// Completed sessions are never listed, and a recovery-off publication journal is
/// listed only when it recorded a publication attempt.
pub fn discover(root: &Path) -> Result<Vec<RecoveryCandidate>, StoreError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(id) = session_directory(&entry)? else { continue };
        let inspect = || -> Result<Option<RecoveryCandidate>, StoreError> {
            let path = entry.path().join("authority.json");
            if !path.try_exists()? && lifecycle_residue(&entry.path())? {
                return Ok(None);
            }
            let authority = match read_authority(&path) {
                Ok(authority) => authority,
                // Reclaimed between listing and reading: not a session any more.
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound && lifecycle_residue(&entry.path())? => return Ok(None),
                Err(error) => return Err(error),
            };
            if authority.schema != 1 || authority.session != id {
                return Err(StoreError::Invalid("Unsupported or mismatched recovery authority".into()));
            }
            if authority.terminal || (!authority.automatic && authority.operations.is_empty()) {
                return Ok(None);
            }
            // A live visit in another PhotoCraft is not an interrupted session.
            if owned_by_running_process(&entry.path())? || nothing_to_recover(&entry.path(), &authority, &id) {
                return Ok(None);
            }
            let latest = authority.generations.last().ok_or_else(|| StoreError::Invalid("No acknowledged recovery checkpoint".into()))?;
            let (generation, session) = latest_valid(&entry.path(), &authority, &id)?;
            let retained: Vec<_> = session.photos.iter().filter(|photo| !authority.removed.contains(&photo.id)).collect();
            let source_context = retained
                .iter()
                .take(3)
                .filter_map(|photo| session.sources.get(&photo.id))
                .map(|binding| binding.source.to_string_lossy().chars().take(512).collect())
                .collect();
            let unsaved = retained.iter().filter(|photo| photo.pending().unwrap_or(true)).count();
            Ok(Some(RecoveryCandidate {
                id: id.clone(),
                listed: Some(Listed { epoch: authority.epoch, generation: latest.number, checksum: latest.checksum.clone() }),
                checkpoint_time_ms: Some(generation.time_ms),
                photo_count: Some(retained.len()),
                source_context,
                problem: None,
                unsaved_photos: Some(unsaved),
                newer_unusable_time_ms: (generation.number != latest.number).then_some(latest.time_ms),
                operations: authority.operations.len(),
                automatic: authority.automatic,
            }))
        };
        match inspect() {
            Ok(Some(candidate)) => candidates.push(candidate),
            Ok(None) => {}
            Err(error) => candidates.push(RecoveryCandidate {
                id,
                listed: None,
                checkpoint_time_ms: None,
                photo_count: None,
                source_context: Vec::new(),
                problem: Some(error.to_string()),
                unsaved_photos: None,
                newer_unusable_time_ms: None,
                operations: 0,
                automatic: true,
            }),
        }
    }
    candidates.sort_by(|a, b| b.checkpoint_time_ms.cmp(&a.checkpoint_time_ms).then_with(|| a.id.cmp(&b.id)));
    Ok(candidates)
}

/// The checkpoint a resume would restore: the newest intact one after the lifecycle
/// barrier, in the same fallback order as [`RecoveryStore::load`].
fn latest_valid(dir: &Path, authority: &Authority, id: &str) -> Result<(Generation, Session), StoreError> {
    for generation in authority.generations.iter().rev() {
        if generation.number < authority.min_recovery_generation {
            break;
        }
        let Some(bytes) = read_checkpoint(&dir.join(format!("generation-{}.json", generation.number)))? else { continue };
        if blake3::hash(&bytes).to_hex().as_str() != generation.checksum {
            continue;
        }
        match validate_snapshot(&bytes) {
            Ok(session) if session.id == id && session.generation == generation.number => return Ok((generation.clone(), session)),
            Err(error @ StoreError::Unsupported(_)) => return Err(error),
            _ => {}
        }
    }
    Err(StoreError::Invalid("No valid checkpoint after the lifecycle barrier".into()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceState {
    /// The retained photograph is at its recorded path with its recorded physical identity.
    Available,
    /// Missing, unreadable, or a different file now has that name (never substituted).
    MissingOrChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarState {
    /// No XMP file, as at the checkpoint.
    Absent,
    /// The XMP file holds exactly the bytes the checkpoint was bound to.
    AsCheckpoint,
    /// Created, edited (for example in Lightroom) or deleted after the checkpoint.
    Changed,
    Unreadable,
    /// Holds exactly the packet an acknowledged save of this session wrote (a save that
    /// finished after the last checkpoint, for example an interrupted Open Images).
    SavedByThisSession,
}

#[derive(Clone, Debug)]
pub struct CandidatePhoto {
    /// The retained source path, as recorded (open it, never a similarly named file).
    pub path: PathBuf,
    pub source: SourceState,
    pub sidecar: SidecarState,
    pub sidecar_modified_ms: Option<u64>,
    /// The checkpoint recipe differs from its saved-XMP baseline.
    pub unsaved: bool,
}

#[derive(Clone, Debug)]
pub struct CandidateDetail {
    pub id: String,
    pub checkpoint_time_ms: u64,
    pub photos: Vec<CandidatePhoto>,
}

/// Per-photo facts for the recovery cards, read-only: no claim, no epoch, no write, and
/// photographs are compared by physical identity rather than hashed (resume verifies
/// contents in full). The sidecar state compares live bytes with the checkpoint's binding.
pub fn inspect_candidate(root: &Path, id: &str) -> Result<CandidateDetail, StoreError> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
        return Err(StoreError::Invalid("Invalid session identity".into()));
    }
    let dir = root.join(id);
    let authority = read_authority(&dir.join("authority.json"))?;
    if authority.schema != 1 || authority.session != id || authority.terminal {
        return Err(StoreError::Invalid("No recoverable Camera Raw session has this identity".into()));
    }
    let (generation, session) = latest_valid(&dir, &authority, id)?;
    // Every acknowledged success per photo, from this session's own journals. The live
    // sidecar is matched against all of them (journal ids are hashes, not an order).
    let mut saved: std::collections::BTreeMap<PhotoId, Vec<(Vec<u8>, super::NativeRecipe)>> = std::collections::BTreeMap::new();
    for operation_id in &authority.operations {
        let path = dir.join(format!("operation-{operation_id}.json"));
        let Ok(operation) = read_record(&path).and_then(|bytes| serde_json::from_slice::<super::commit::Operation>(&bytes).map_err(invalid)) else { continue };
        if super::commit::validate_operation(&operation, operation_id, id).is_err() {
            continue;
        }
        for planned in &operation.plan.photos {
            if operation.outcomes.get(&planned.photo) == Some(&super::commit::Outcome::Success) {
                saved.entry(planned.photo.clone()).or_default().push((planned.intended.clone(), planned.recipe.clone()));
            }
        }
    }
    let mut photos = Vec::new();
    for photo in session.photos.iter().filter(|photo| !authority.removed.contains(&photo.id)) {
        let binding = session.sources.get(&photo.id).ok_or_else(|| StoreError::Invalid("Retained photo has no source binding".into()))?;
        let identity = File::open(&binding.source)
            .ok()
            .filter(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()))
            .and_then(|file| super::commit::physical_identity(&file).ok());
        let source = if identity.as_deref() == Some(binding.source_identity.as_str()) { SourceState::Available } else { SourceState::MissingOrChanged };
        let live = super::commit::sidecar_bytes(&binding.target);
        let journaled = match &live {
            Ok(Some(bytes)) => saved.get(&photo.id).and_then(|saves| saves.iter().find(|(intended, _)| intended == bytes)),
            _ => None,
        };
        let sidecar = match &live {
            Ok(Some(_)) if journaled.is_some() => SidecarState::SavedByThisSession,
            Ok(None) if binding.original.is_none() => SidecarState::Absent,
            Ok(bytes) if *bytes == binding.original => SidecarState::AsCheckpoint,
            Ok(_) => SidecarState::Changed,
            Err(_) => SidecarState::Unreadable,
        };
        let unsaved = match (sidecar == SidecarState::SavedByThisSession, journaled) {
            (true, Some((_, recipe))) => photo.working != *recipe,
            _ => photo.pending().unwrap_or(true),
        };
        let sidecar_modified_ms = std::fs::metadata(&binding.target)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        photos.push(CandidatePhoto { path: binding.source.clone(), source, sidecar, sidecar_modified_ms, unsaved });
    }
    Ok(CandidateDetail { id: id.to_owned(), checkpoint_time_ms: generation.time_ms, photos })
}

/// Startup reclamation, separate from the read-only [`discover`]. Removes only:
/// completed sessions whose record removal was interrupted or blocked earlier,
/// lifecycle residue holding nothing but a lock, and recovery-off publication journals
/// that never attempted a publication (an explicit save abandoned by a crash before
/// consent). A session locked by a running process, every recoverable draft and every
/// journal that recorded a publication are retained. Returns one warning per session
/// that should have been reclaimed but could not be.
pub fn collect_completed(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut warnings = Vec::new();
    for entry in entries.flatten() {
        let Ok(Some(id)) = session_directory(&entry) else { continue };
        let dir = entry.path();
        let attempt = || -> Result<bool, StoreError> {
            let authority_path = dir.join("authority.json");
            if !authority_path.try_exists()? {
                if !lifecycle_residue(&dir)? {
                    return Ok(false);
                }
                let Some(lock) = try_lock_unowned(&dir)? else { return Ok(false) };
                if !lifecycle_residue(&dir)? {
                    return Ok(false);
                }
                remove_lock_then_folder(&dir, lock, false)?;
                return Ok(true);
            }
            let authority = read_authority(&authority_path)?;
            if authority.schema != 1 || authority.session != id {
                return Ok(false);
            }
            if authority.terminal {
                let Some(lock) = try_lock_unowned(&dir)? else { return Ok(false) };
                reclaim_terminal(&dir, lock)?;
                return Ok(true);
            }
            let unpublished_journal = !authority.automatic && authority.operations.is_empty();
            if !unpublished_journal && !nothing_to_recover(&dir, &authority, &id) {
                return Ok(false);
            }
            let mut store = match RecoveryStore::claim_existing(root, &id) {
                Ok(store) => store,
                Err(StoreError::Owned) => return Ok(false),
                Err(error) => return Err(error),
            };
            // Rechecked under the owner lock before the terminal record is published.
            store.retire_without_obligations()?;
            store.reclaim()?;
            Ok(true)
        };
        if let Err(error) = attempt() {
            warnings.push(format!("Recovery records for {id} were retained: {error}"));
        }
    }
    warnings
}

/// The ownership lock of a session no running process holds, or None when it is in use.
fn try_lock_unowned(dir: &Path) -> Result<Option<File>, StoreError> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("ownership.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Remove a completed session's directory while holding its ownership lock. Only the
/// names this store writes are removed, the lock and authority last, so an interrupted
/// removal leaves either a terminal authority (never offered, collected later) or bare
/// residue. Anything unexpected keeps the directory and is reported.
fn reclaim_terminal(dir: &Path, ownership: File) -> Result<(), StoreError> {
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err(StoreError::Invalid("Recovery directory is a link; records retained".into()));
    }
    if !read_authority(&dir.join("authority.json"))?.terminal {
        return Err(StoreError::Invalid("Only a completed session's records can be removed".into()));
    }
    let mut records = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(StoreError::Invalid("Completed session holds an unrecognized entry; records retained".into()));
        };
        if matches!(name, "authority.json" | "ownership.lock") {
            continue;
        }
        let owned = ((name.starts_with("generation-") || name.starts_with("operation-")) && name.ends_with(".json"))
            || (name.starts_with('.') && name.ends_with(".pcsave.tmp"));
        if !owned || !entry.file_type()?.is_file() {
            return Err(StoreError::Invalid("Completed session holds an unrecognized entry; records retained".into()));
        }
        records.push(entry.path());
    }
    for record in records {
        std::fs::remove_file(record)?;
    }
    remove_lock_then_folder(dir, ownership, true)
}

/// Delete the lock file while still holding it (on Windows a delete-pending file cannot be
/// opened, so no other process can claim the folder meanwhile), then the authority, then
/// release the handle and remove the empty folder.
fn remove_lock_then_folder(dir: &Path, ownership: File, authority: bool) -> Result<(), StoreError> {
    std::fs::remove_file(dir.join("ownership.lock"))?;
    if authority {
        std::fs::remove_file(dir.join("authority.json"))?;
    }
    drop(ownership);
    std::fs::remove_dir(dir)?;
    Ok(())
}

/// All nonterminal drafts retain their source/sidecar dependencies, even when a
/// different draft is the one currently open. JPEG export uses this read-only
/// inventory immediately before its output decisions/publication. A malformed
/// authority is not permission to ignore a possibly retained source.
pub fn protected_sources(root: &Path) -> Result<Vec<super::SourceBinding>, StoreError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut protected = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(StoreError::Invalid("Recovery directory identity is not representable".into()));
        };
        // Residue of a claim or of a reclaimed session holds no binding to protect.
        if !entry.path().join("authority.json").try_exists()? && lifecycle_residue(&entry.path())? {
            continue;
        }
        let authority = match read_authority(&entry.path().join("authority.json")) {
            Ok(authority) => authority,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound && lifecycle_residue(&entry.path())? => continue,
            Err(error) => return Err(error),
        };
        if authority.schema != 1 || authority.session != id || authority.generations.len() > MAX_GENERATIONS {
            return Err(StoreError::Invalid("Unsupported or mismatched recovery authority; JPEG protection cannot be established".into()));
        }
        // Never acknowledged a checkpoint and holds no journal: no binding was ever retained.
        if authority.terminal || (authority.generations.is_empty() && authority.operations.is_empty() && authority.superseded_operations.is_empty()) {
            continue;
        }
        let mut snapshot = None;
        for generation in authority.generations.iter().rev() {
            if generation.number < authority.min_recovery_generation {
                break;
            }
            let path = entry.path().join(format!("generation-{}.json", generation.number));
            let Some(bytes) = read_checkpoint(&path).map_err(|error| {
                StoreError::Invalid(format!(
                    "Draft {id}: a recovery checkpoint cannot be read right now ({error}); JPEG protection cannot be established, try again"
                ))
            })?
            else {
                continue;
            };
            if blake3::hash(&bytes).to_hex().as_str() != generation.checksum {
                continue;
            }
            match validate_snapshot(&bytes) {
                Ok(session) if session.id == id && session.generation == generation.number => {
                    snapshot = Some(session);
                    break;
                }
                Err(error @ StoreError::Unsupported(_)) => return Err(error),
                _ => {}
            }
        }
        let snapshot = snapshot.ok_or_else(|| StoreError::Invalid(format!("Draft {id} has no valid protected-source snapshot")))?;
        for photo in &snapshot.photos {
            if !authority.removed.contains(&photo.id) {
                let binding = snapshot.sources.get(&photo.id).ok_or_else(|| StoreError::Invalid("Retained photo has no source protection binding".into()))?;
                if !protected.contains(binding) {
                    protected.push(binding.clone());
                }
            }
        }
        for operation_id in &authority.operations {
            if operation_id.len() != 64 || !operation_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(StoreError::Invalid("Invalid journal reference in recovery authority".into()));
            }
            let operation: super::commit::Operation =
                serde_json::from_slice(&read_record(&entry.path().join(format!("operation-{operation_id}.json")))?).map_err(invalid)?;
            super::commit::validate_operation(&operation, operation_id, &id)?;
            for photo in &operation.plan.photos {
                if !authority.removed.contains(&photo.photo) && !protected.contains(&photo.binding) {
                    protected.push(photo.binding.clone());
                }
            }
        }
        if protected.len() > super::MAX_PHOTOS {
            return Err(StoreError::Invalid("Too many retained source dependencies for safe JPEG inventory".into()));
        }
    }
    Ok(protected)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    checksum: String,
    payload: String,
}

#[derive(Clone, Debug)]
pub struct CheckpointAck {
    /// False for an in-memory session: no recovery record has been written.
    pub durable: bool,
    pub session: String,
    pub epoch: u64,
    pub generation: u64,
    pub checksum: String,
    pub time_ms: u64,
    /// Soft budget pressure never discards protected work or changes this ack.
    pub storage_pressure: Option<String>,
}

#[derive(Debug)]
pub struct Restored {
    pub session: Session,
    pub checkpoint_time_ms: u64,
    pub used_fallback: bool,
}

/// One owner and one mutable publisher. Holding the OS lock, rather than a PID
/// or timestamp lease, fences competing processes and survives process death.
pub struct RecoveryStore {
    dir: PathBuf,
    _ownership: Option<File>,
    authority: Authority,
    uncertain: bool,
    pub(super) sidecar_publisher: Option<super::SidecarPublisher>,
}

impl RecoveryStore {
    /// No filesystem access. Done can later claim this identity for its explicit
    /// publication journal without enabling automatic recovery while editing.
    pub(super) fn deferred(root: &Path, session: &str) -> Self {
        Self {
            dir: root.join(session),
            _ownership: None,
            uncertain: false,
            sidecar_publisher: None,
            authority: Authority {
                schema: 1,
                session: session.into(),
                epoch: 0,
                terminal: false,
                removed: BTreeSet::new(),
                generations: vec![],
                pins: BTreeSet::new(),
                operations: BTreeSet::new(),
                superseded_operations: Default::default(),
                min_recovery_generation: 0,
                entry_generation: None,
                automatic: false,
            },
        }
    }

    pub(super) fn durable(&self) -> bool {
        self._ownership.is_some()
    }

    pub(super) fn root(&self) -> Result<&Path, StoreError> {
        self.dir.parent().ok_or_else(|| StoreError::Invalid("Recovery root missing".into()))
    }

    /// Ownership of a recoverable session (opted-in recovery, or a resumed draft, which
    /// keeps the flag it was created with).
    pub fn claim(root: &Path, session: &str) -> Result<Self, StoreError> {
        Self::claim_as(root, session, true)
    }

    /// Ownership for the publication journal of one explicit Done, Open Images or
    /// removal while long-session recovery is off. It is reclaimed when that action
    /// finishes, and never offered after an interruption unless it attempted a publication.
    pub fn claim_journal(root: &Path, session: &str) -> Result<Self, StoreError> {
        Self::claim_as(root, session, false)
    }

    /// Ownership of a session that must already exist (resume, explicit discard). A
    /// reclaimed or never-created session is refused instead of being recreated empty.
    pub fn claim_existing(root: &Path, session: &str) -> Result<Self, StoreError> {
        let authority = root.join(session).join("authority.json");
        if session.is_empty() || session.len() > 128 || !session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || !authority.try_exists()? {
            return Err(StoreError::Invalid("No recoverable Camera Raw session has this identity".into()));
        }
        Self::claim_with(root, session, true, true, None)
    }

    /// [`Self::claim_existing`] that also refuses (writing nothing) when the session is no
    /// longer the state the chooser listed.
    pub fn claim_listed(root: &Path, session: &str, listed: &Listed) -> Result<Self, StoreError> {
        let authority = root.join(session).join("authority.json");
        if session.is_empty() || session.len() > 128 || !session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || !authority.try_exists()? {
            return Err(StoreError::Invalid("No recoverable Camera Raw session has this identity".into()));
        }
        Self::claim_with(root, session, true, true, Some(listed))
    }

    fn claim_as(root: &Path, session: &str, automatic: bool) -> Result<Self, StoreError> {
        Self::claim_with(root, session, automatic, false, None)
    }

    /// `must_exist`: the authority must still exist once the lock is held, so a session
    /// reclaimed between a listing and this claim is refused rather than recreated empty.
    fn claim_with(root: &Path, session: &str, automatic: bool, must_exist: bool, expected: Option<&Listed>) -> Result<Self, StoreError> {
        if session.is_empty() || session.len() > 128 || !session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(StoreError::Invalid("Invalid session identity".into()));
        }
        let dir = root.join(session);
        if must_exist && !dir.try_exists()? {
            return Err(StoreError::Invalid("No recoverable Camera Raw session has this identity".into()));
        }
        std::fs::create_dir_all(&dir)?;
        let ownership = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("ownership.lock"))?;
        ownership.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => StoreError::Owned,
            std::fs::TryLockError::Error(error) => StoreError::Io(error),
        })?;
        let authority_path = dir.join("authority.json");
        let mut authority: Authority = match read_authority(&authority_path) {
            Ok(authority) => authority,
            Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound && must_exist => {
                return Err(StoreError::Invalid("No recoverable Camera Raw session has this identity".into()));
            }
            Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                // An absent authority with old generations is not a new session.
                for entry in std::fs::read_dir(&dir)? {
                    if entry?.file_name() != "ownership.lock" {
                        return Err(StoreError::Invalid("Missing lifecycle authority; retained records require reconciliation".into()));
                    }
                }
                Authority {
                    schema: 1,
                    session: session.into(),
                    epoch: 0,
                    terminal: false,
                    removed: BTreeSet::new(),
                    generations: vec![],
                    pins: BTreeSet::new(),
                    operations: BTreeSet::new(),
                    superseded_operations: std::collections::BTreeMap::new(),
                    min_recovery_generation: 0,
                    entry_generation: None,
                    automatic,
                }
            }
            Err(e) => return Err(e),
        };
        if authority.schema != 1 || authority.session != session || authority.terminal {
            return Err(StoreError::Invalid("Unsupported, mismatched or completed session authority".into()));
        }
        if authority.generations.len() > MAX_GENERATIONS
            || authority.pins.len() > MAX_GENERATIONS
            || authority.operations.len() > MAX_OPERATIONS
            || authority.superseded_operations.len() > MAX_HISTORY
            || authority.removed.len() > super::MAX_PHOTOS
            || authority.generations.windows(2).any(|w| w.first().zip(w.get(1)).is_none_or(|(a, b)| a.number >= b.number))
            || authority.pins.iter().any(|pin| !authority.generations.iter().any(|g| &g.number == pin))
        {
            return Err(StoreError::Invalid("Broken generation or pin references".into()));
        }
        if let Some(expected) = expected
            && (authority.epoch != expected.epoch
                || authority.generations.last().map(|g| (g.number, g.checksum.as_str())) != Some((expected.generation, expected.checksum.as_str())))
        {
            return Err(StoreError::Invalid(LISTING_CHANGED.into()));
        }
        authority.epoch = authority.epoch.checked_add(1).ok_or_else(|| StoreError::Invalid("Ownership epoch exhausted".into()))?;
        write_record(&authority_path, &encode_record(&authority)?)?;
        Ok(Self { dir, _ownership: Some(ownership), authority, uncertain: false, sidecar_publisher: None })
    }

    pub fn epoch(&self) -> u64 {
        self.authority.epoch
    }

    /// A journal or authority write whose outcome could not be confirmed this visit.
    pub(super) fn uncertain(&self) -> bool {
        self.uncertain
    }

    /// True for an opted-in draft (or a legacy record); false for a recovery-off journal.
    pub(super) fn automatic(&self) -> bool {
        self.authority.automatic
    }

    /// Opting into recovery during a visit turns its explicit-save journal into a
    /// recoverable draft (and the reverse is never needed: opting out retires it).
    pub(super) fn set_automatic(&mut self, automatic: bool) -> Result<(), StoreError> {
        self.writable()?;
        if self.authority.automatic == automatic {
            return Ok(());
        }
        let mut next = self.authority.clone();
        next.automatic = automatic;
        self.publish_authority(next)
    }

    /// A recovery-off journal abandoned before any publication holds no obligation:
    /// nothing was written next to a photograph, and its edits were never a recovery draft.
    /// A session with no obligation is retired at startup: a recovery-off journal that
    /// never attempted a publication, or a session with nothing to recover (no
    /// acknowledged checkpoint, or none of its photos retained) and no journal.
    fn retire_without_obligations(&mut self) -> Result<(), StoreError> {
        self.writable()?;
        let unpublished_journal = !self.authority.automatic && self.authority.operations.is_empty() && self.authority.superseded_operations.is_empty();
        if !unpublished_journal && !nothing_to_recover(&self.dir, &self.authority, &self.authority.session) {
            return Err(StoreError::Invalid("Only a session without obligations can be retired automatically".into()));
        }
        let mut next = self.authority.clone();
        next.terminal = true;
        self.publish_authority(next)
    }

    /// Give up a completed session and remove its private records (the terminal
    /// authority is already durable). On failure the records stay terminal, are never
    /// offered again, and [`collect_completed`] retries the removal later.
    pub(super) fn reclaim(self) -> Result<(), StoreError> {
        let Some(ownership) = self._ownership else {
            return Ok(());
        };
        if !self.authority.terminal {
            return Err(StoreError::Invalid("Only a completed session's records can be removed".into()));
        }
        reclaim_terminal(&self.dir, ownership)
    }

    pub(super) fn session_id(&self) -> &str {
        &self.authority.session
    }

    pub(super) fn coordination_owner(&self) -> &Path {
        &self.dir
    }

    /// Authoritative interrupted/successful journals available after restart.
    /// Superseded records remain on disk but are excluded from active reconciliation.
    pub fn operation_ids(&self) -> Vec<String> {
        self.authority.operations.iter().cloned().collect()
    }

    pub(super) fn commit_snapshot(&self, generation: u64) -> Result<Session, StoreError> {
        self.writable()?;
        let latest = self.authority.generations.last().ok_or_else(|| StoreError::Invalid("No acknowledged checkpoint".into()))?;
        if latest.number != generation {
            return Err(StoreError::Invalid("Plan checkpoint has been superseded".into()));
        }
        let bytes = read_record(&self.generation_path(generation))?;
        if blake3::hash(&bytes).to_hex().as_str() != latest.checksum {
            return Err(StoreError::Invalid("Pinned checkpoint checksum mismatch".into()));
        }
        let snapshot = validate_snapshot(&bytes)?;
        if snapshot.phase != super::Phase::Frozen
            || snapshot.epoch != self.authority.epoch
            || snapshot.id != self.authority.session
            || snapshot.photos.iter().any(|p| self.authority.removed.contains(&p.id))
        {
            return Err(StoreError::Invalid("Checkpoint ownership or membership has changed".into()));
        }
        Ok(snapshot)
    }

    pub(super) fn commit_ownership(&self) -> Result<File, StoreError> {
        self.writable()?;
        let parent = self.dir.parent().ok_or_else(|| StoreError::Invalid("Recovery root missing".into()))?;
        super::commit::lock_commit(&parent.join("sidecar-commit.lock"))
    }

    /// Called only under this recovery root's publication lock. Another session's
    /// unconfirmed write must remain observable until that session reconciles it:
    /// otherwise a newer publication destroys the evidence needed to resolve it.
    /// This does not coordinate independent recovery roots.
    pub(super) fn refuse_other_uncertain_targets(&self, plan: &super::commit::Plan) -> Result<(), StoreError> {
        use super::commit::Outcome;
        let refusal = |message: &str| StoreError::Invalid(message.into());
        let parent = self.dir.parent().ok_or_else(|| refusal("Recovery root missing"))?;
        for (index, entry) in std::fs::read_dir(parent)?.enumerate() {
            if index >= MAX_HISTORY {
                return Err(refusal("Too many recovery records to confirm sidecar ownership; nothing was written"));
            }
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.path() == self.dir {
                continue;
            }
            let dir = entry.path();
            let authority_path = dir.join("authority.json");
            if !authority_path.try_exists()? {
                // A brand-new claim has only a lock. Anything else without an
                // authority could be an interrupted publication; fail closed.
                if lifecycle_residue(&dir)? {
                    continue;
                }
                return Err(refusal("Cannot confirm another session's publication records; nothing was written"));
            }
            let authority = read_authority(&authority_path)?;
            if authority.schema != 1 {
                return Err(refusal("Cannot confirm another session's publication schema; nothing was written"));
            }
            if authority.terminal {
                continue;
            }
            // Include orphan operation files: a journal can reach disk before its
            // authority update. Superseded journals have already been resolved.
            let mut observed = BTreeSet::new();
            for (index, record) in std::fs::read_dir(&dir)?.enumerate() {
                if index >= MAX_HISTORY + MAX_GENERATIONS {
                    return Err(refusal("Too many session records to confirm sidecar ownership; nothing was written"));
                }
                let record = record?;
                let name = record.file_name();
                let Some(id) = name.to_str().and_then(|s| s.strip_prefix("operation-")).and_then(|s| s.strip_suffix(".json")) else {
                    continue;
                };
                if authority.superseded_operations.contains_key(id) {
                    continue;
                }
                observed.insert(id.to_owned());
                let operation: super::commit::Operation = serde_json::from_slice(&read_record(&record.path())?).map_err(invalid)?;
                super::commit::validate_operation(&operation, id, &authority.session)?;
                for photo in &operation.plan.photos {
                    let uncertain = operation.uncertain_photos.contains(&photo.photo)
                        || matches!(operation.outcomes.get(&photo.photo), Some(Outcome::Unknown(_)))
                        || (operation.outcomes.get(&photo.photo) == Some(&Outcome::Pending) && !operation.confirmed_unwritten.contains(&photo.photo));
                    if !uncertain {
                        continue;
                    }
                    for next in &plan.photos {
                        let a = &photo.binding.target;
                        let b = &next.binding.target;
                        let same_name = a == b || (cfg!(windows) && a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()));
                        let alias = !same_name && a.try_exists()? && b.try_exists()? && same_file::is_same_file(a, b)?;
                        if same_name || alias {
                            return Err(refusal(
                                "Another Camera Raw session has an unconfirmed save for this sidecar; reconcile that session before saving. Nothing was written",
                            ));
                        }
                    }
                }
            }
            if authority.operations.iter().any(|id| !authority.superseded_operations.contains_key(id) && !observed.contains(id)) {
                return Err(refusal("Another session's publication record is missing; cannot confirm sidecar ownership. Nothing was written"));
            }
        }
        Ok(())
    }

    fn operation_path(&self, id: &str) -> Result<PathBuf, StoreError> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::Invalid("Invalid operation ID".into()));
        }
        Ok(self.dir.join(format!("operation-{id}.json")))
    }

    pub(super) fn operation_exists(&self, id: &str) -> Result<bool, StoreError> {
        self.operation_path(id)?.try_exists().map_err(StoreError::Io)
    }

    pub(super) fn write_operation(&mut self, id: &str, operation: &super::commit::Operation) -> Result<(), StoreError> {
        self.writable()?;
        if operation.plan.id != id
            || operation.plan.session != self.authority.session
            || !self.authority.generations.iter().any(|g| g.number == operation.plan.generation)
        {
            return Err(StoreError::Invalid("Journal identity or checkpoint reference is invalid".into()));
        }
        if self.authority.superseded_operations.contains_key(id) {
            return Err(StoreError::Invalid("Superseded operation remains historical and cannot be reactivated".into()));
        }
        if !self.authority.operations.contains(id)
            && (self.authority.operations.len() >= MAX_OPERATIONS || operation.plan.generation < self.authority.min_recovery_generation)
        {
            return Err(StoreError::Invalid("Journal capacity reached or operation predates the recovery barrier; protected work retained".into()));
        }
        let path = self.operation_path(id)?;
        let bytes = encode_record(operation)?;
        self.publish(&path, &bytes)?;
        if !self.authority.operations.contains(id) {
            let mut next = self.authority.clone();
            next.operations.insert(id.into());
            next.pins.insert(operation.plan.generation);
            self.publish_authority(next)?;
        }
        Ok(())
    }

    pub(super) fn read_operation(&self, id: &str) -> Result<super::commit::Operation, StoreError> {
        self.writable()?;
        let operation: super::commit::Operation = serde_json::from_slice(&read_record(&self.operation_path(id)?)?).map_err(invalid)?;
        super::commit::validate_operation(&operation, id, &self.authority.session)?;
        Ok(operation)
    }

    /// Explicit retry resolution after both operations have been reconciled. Every
    /// unresolved old photo must have a confirmed same-source success in a LATER reviewed
    /// operation: the new one, or an earlier retry (active or already superseded) that saved
    /// it, so step-by-step retries resolve too. Keep the old journal and pins as evidence,
    /// but do not let an obsolete failure or conflict permanently block terminal completion.
    /// Removed members need nothing, and neither does an old definite failure that is moot in
    /// the current `session` (see [`super::commit::failure_is_moot`]).
    pub fn supersede_operation(&mut self, old_id: &str, new_id: &str, session: &super::Session) -> Result<(), StoreError> {
        if old_id == new_id || !self.authority.operations.contains(old_id) || !self.authority.operations.contains(new_id) {
            return Err(StoreError::Invalid("Both distinct operations must be active".into()));
        }
        if self.authority.superseded_operations.len() >= MAX_HISTORY {
            return Err(StoreError::Invalid("Journal history pressure; protected records retained".into()));
        }
        let old = self.reconcile_operation(old_id)?;
        let new = self.reconcile_operation(new_id)?;
        let order = |operation: &super::commit::Operation| (operation.plan.epoch, operation.plan.generation);
        if order(&new) <= order(&old) {
            return Err(StoreError::Invalid("A retry must be newer than the operation it resolves".into()));
        }
        let mut later = vec![new];
        for id in self.authority.operations.iter().chain(self.authority.superseded_operations.keys()) {
            if id == old_id || id == new_id {
                continue;
            }
            // Unreadable evidence resolves nothing; the old operation then stays active.
            if let Ok(other) = self.read_operation(id)
                && order(&other) > order(&old)
            {
                later.push(other);
            }
        }
        for photo in &old.plan.photos {
            let outcome = old.outcomes.get(&photo.photo);
            if outcome == Some(&super::commit::Outcome::Success)
                || self.authority.removed.contains(&photo.photo)
                || outcome.is_some_and(|outcome| super::commit::failure_is_moot(session, photo, outcome))
            {
                continue;
            }
            let resolved = later.iter().any(|operation| {
                operation.outcomes.get(&photo.photo) == Some(&super::commit::Outcome::Success)
                    && operation.plan.photos.iter().any(|saved| {
                        saved.photo == photo.photo
                            && saved.binding.source_identity == photo.binding.source_identity
                            && saved.binding.source_fingerprint == photo.binding.source_fingerprint
                    })
            });
            if !resolved {
                return Err(StoreError::Invalid("Retry has not resolved the same source".into()));
            }
        }
        let mut next = self.authority.clone();
        next.operations.remove(old_id);
        next.superseded_operations.insert(old_id.into(), new_id.into());
        self.publish_authority(next)
    }

    /// Reserve a number beyond both authoritative records and orphaned immutable
    /// generations. An interrupted authority publication never forces overwriting
    /// its evidence. Orphans are retained for diagnosis, never automatically resumed.
    pub fn next_generation(&self) -> Result<u64, StoreError> {
        self.writable()?;
        let mut latest = self.authority.generations.last().map_or(0, |g| g.number);
        let mut count = 0usize;
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(number) = name.strip_prefix("generation-").and_then(|s| s.strip_suffix(".json")).and_then(|s| s.parse::<u64>().ok()) {
                count += 1;
                if count > MAX_GENERATIONS {
                    return Err(StoreError::Invalid("Excessive orphan records; reconciliation required".into()));
                }
                latest = latest.max(number);
            }
        }
        latest.checked_add(1).ok_or_else(|| StoreError::Invalid("Generation exhausted".into()))
    }

    pub fn checkpoint(&mut self, snapshot: &Session) -> Result<CheckpointAck, StoreError> {
        self.writable()?;
        validate_session_bounds(snapshot)?;
        if snapshot.id != self.authority.session || snapshot.epoch != self.authority.epoch {
            return Err(StoreError::Invalid("Stale session ownership".into()));
        }
        if self.authority.generations.last().is_some_and(|g| snapshot.generation <= g.number) {
            return Err(StoreError::Invalid("A newer or equal generation is already published".into()));
        }
        if self.authority.generations.len() >= MAX_GENERATIONS {
            return Err(StoreError::Invalid("Recovery retention pressure; protected generations retained".into()));
        }
        if snapshot.photos.len() > super::MAX_PHOTOS || snapshot.photos.iter().any(|p| self.authority.removed.contains(&p.id)) {
            return Err(StoreError::Invalid("Snapshot contains excessive or explicitly removed members".into()));
        }
        if !snapshot.removed.is_subset(&self.authority.removed) {
            return Err(StoreError::Invalid("Removal requires a separate acknowledged lifecycle disposition".into()));
        }
        if snapshot.requested.len() > super::MAX_PHOTOS || snapshot.sources.len() > super::MAX_PHOTOS || snapshot.removed.len() > super::MAX_PHOTOS {
            return Err(StoreError::Invalid("Excessive session collections".into()));
        }
        let bytes = encode_record(snapshot)?;
        // An acknowledgement must describe the live session exactly. A record that reads
        // back differently would make Done refuse its own checkpoint, so withhold it.
        if encode_record(&validate_snapshot(&bytes)?)? != bytes {
            return Err(StoreError::Invalid("Checkpoint would not reproduce the live session exactly; recovery record withheld".into()));
        }
        let checksum = blake3::hash(&bytes).to_hex().to_string();
        let time_ms = now_ms()?;
        let generation = Generation { number: snapshot.generation, checksum: checksum.clone(), time_ms };
        let path = self.generation_path(snapshot.generation);
        // Never overwrite a generation left by an interrupted attempt.
        if path.try_exists()? {
            return Err(StoreError::Uncertain("Generation already exists; inspect the previous publication".into()));
        }
        self.publish(&path, &bytes)?;
        let mut next = self.authority.clone();
        next.generations.push(generation);
        // Keep the latest two generations and every persistent pin. Pruning the
        // authority precedes best-effort deletion, so interruption leaves harmless
        // unreferenced files rather than broken live references.
        let keep_from = next.generations.len().saturating_sub(2);
        let obsolete: Vec<_> = next.generations.iter().take(keep_from).filter(|g| !next.pins.contains(&g.number)).map(|g| g.number).collect();
        next.generations.retain(|g| !obsolete.contains(&g.number));
        self.publish_authority(next)?;
        for number in obsolete {
            let _ = std::fs::remove_file(self.generation_path(number));
        }
        let measured = cache_size(self.dir.parent().unwrap_or(&self.dir));
        let cleanup = self.cleanup_redundant_records(measured.as_ref().is_ok_and(|bytes| *bytes > SOFT_CACHE_BYTES));
        let storage_pressure = match cleanup.and_then(|_| cache_size(self.dir.parent().unwrap_or(&self.dir))) {
            Ok(bytes) if bytes > SOFT_CACHE_BYTES => {
                Some(format!("Recovery cache uses {} MiB; soft budget is 256 MiB. Protected drafts and journals are retained.", bytes / (1024 * 1024)))
            }
            Ok(_) => None,
            Err(error) => Some(format!("Recovery cache size could not be measured: {error}; protected records retained")),
        };
        Ok(CheckpointAck {
            durable: true,
            session: snapshot.id.clone(),
            epoch: snapshot.epoch,
            generation: snapshot.generation,
            checksum,
            time_ms,
            storage_pressure,
        })
    }

    pub fn load(&self) -> Result<Option<Restored>, StoreError> {
        self.writable()?;
        for (index, generation) in self.authority.generations.iter().rev().enumerate() {
            if generation.number < self.authority.min_recovery_generation {
                break;
            }
            // Never fence an older state over a newer checkpoint that is merely busy now.
            let Some(bytes) = read_checkpoint(&self.generation_path(generation.number))? else {
                continue;
            };
            if blake3::hash(&bytes).to_hex().as_str() != generation.checksum {
                continue;
            }
            let mut session = match validate_snapshot(&bytes) {
                Ok(session) => session,
                Err(error @ StoreError::Unsupported(_)) => return Err(error),
                Err(_) => continue,
            };
            if session.id != self.authority.session || session.generation != generation.number {
                continue;
            }
            session.photos.retain(|p| !self.authority.removed.contains(&p.id));
            session.sources.retain(|id, _| !self.authority.removed.contains(id));
            session.removed.extend(self.authority.removed.iter().cloned());
            session.epoch = self.authority.epoch;
            // Never reuse a generation number, even when falling back.
            session.generation = self.next_generation()?.saturating_sub(1);
            session.cached_generation = None;

            return Ok(Some(Restored { session, checkpoint_time_ms: generation.time_ms, used_fallback: index != 0 }));
        }
        if self.authority.generations.is_empty() { Ok(None) } else { Err(StoreError::Invalid("No valid checkpoint; original records retained".into())) }
    }

    pub fn pin(&mut self, generation: u64) -> Result<(), StoreError> {
        self.writable()?;
        let record =
            self.authority.generations.iter().find(|g| g.number == generation).ok_or_else(|| StoreError::Invalid("Cannot pin unknown generation".into()))?;
        let bytes = read_record(&self.generation_path(generation))?;
        if blake3::hash(&bytes).to_hex().as_str() != record.checksum {
            return Err(StoreError::Invalid("Cannot pin corrupt generation".into()));
        }
        let mut next = self.authority.clone();
        next.pins.insert(generation);
        self.publish_authority(next)
    }

    /// The newly acknowledged entry includes the prior visit's entire retained
    /// state. Older entry pins can be released only after this publication.
    pub fn pin_entry(&mut self, generation: u64) -> Result<(), StoreError> {
        self.writable()?;
        let record = self
            .authority
            .generations
            .iter()
            .find(|g| g.number == generation)
            .ok_or_else(|| StoreError::Invalid("Entry must name an acknowledged generation".into()))?;
        if blake3::hash(&read_record(&self.generation_path(generation))?).to_hex().as_str() != record.checksum {
            return Err(StoreError::Invalid("Entry generation is corrupt".into()));
        }
        let mut next = self.authority.clone();
        next.pins.insert(generation);
        if let Some(old) = next.entry_generation.replace(generation)
            && old != next.min_recovery_generation
            && !self.operation_generations()?.contains(&old)
        {
            next.pins.remove(&old);
        }
        self.publish_authority(next)
    }

    fn operation_generations(&self) -> Result<BTreeSet<u64>, StoreError> {
        self.authority.operations.iter().map(|id| self.read_operation(id).map(|op| op.plan.generation)).collect()
    }

    /// Thirty days and 256 MiB are soft inputs for *redundant* material only.
    /// Never infer an unfinished draft or unknown operation is disposable by age.
    fn cleanup_redundant_records(&self, pressure: bool) -> Result<(), StoreError> {
        let now = SystemTime::now();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let age = entry.metadata()?.modified().ok().and_then(|time| now.duration_since(time).ok()).map_or(0, |duration| duration.as_secs());
            if !pressure && age < RETIRED_AGE_SECS {
                continue;
            }
            let removable = if let Some(number) = name.strip_prefix("generation-").and_then(|s| s.strip_suffix(".json")).and_then(|s| s.parse::<u64>().ok()) {
                number < self.authority.min_recovery_generation
                    && !self.authority.pins.contains(&number)
                    && !self.authority.generations.iter().any(|generation| generation.number == number)
            } else if let Some(id) = name.strip_prefix("operation-").and_then(|s| s.strip_suffix(".json")) {
                if self.authority.operations.contains(id) || self.authority.superseded_operations.contains_key(id) {
                    false
                } else {
                    let operation: super::commit::Operation = serde_json::from_slice(&read_record(&entry.path())?).map_err(invalid)?;
                    super::commit::validate_operation(&operation, id, &self.authority.session)?;
                    operation.plan.generation < self.authority.min_recovery_generation
                        && operation.uncertain_photos.is_empty()
                        && operation.outcomes.values().all(|outcome| *outcome == super::commit::Outcome::Success)
                }
            } else {
                false
            };
            if removable {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    /// Publish only after the transition snapshot is acknowledged. Resume entry,
    /// committed-baseline advancement and discard all fence older fallback data.
    pub fn fence_recovery(&mut self, generation: u64) -> Result<(), StoreError> {
        self.writable()?;
        if self.authority.generations.last().map(|record| record.number) != Some(generation) || generation < self.authority.min_recovery_generation {
            return Err(StoreError::Invalid("Recovery barrier requires the latest acknowledged generation".into()));
        }
        let latest = self.authority.generations.last().ok_or_else(|| StoreError::Invalid("No latest generation".into()))?;
        let bytes = read_record(&self.generation_path(generation))?;
        if blake3::hash(&bytes).to_hex().as_str() != latest.checksum {
            return Err(StoreError::Invalid("Cannot fence recovery at a corrupt checkpoint".into()));
        }
        let prior_barrier = self.authority.min_recovery_generation;
        let mut next = self.authority.clone();
        next.min_recovery_generation = generation;
        next.pins.insert(generation);
        if Some(prior_barrier) != next.entry_generation && !self.operation_generations()?.contains(&prior_barrier) {
            next.pins.remove(&prior_barrier);
        }
        self.publish_authority(next)?;
        self.retire_resolved_operations(generation)
    }

    /// Retire journals only when every obligation is resolved and a fenced
    /// snapshot contains their exact latest baselines. Unknown/failed obligations
    /// retain their complete history. Publication of authority precedes deletion.
    fn retire_resolved_operations(&mut self, generation: u64) -> Result<(), StoreError> {
        if self.authority.operations.is_empty() {
            return Ok(());
        }
        let mut operations = self.authority.operations.iter().map(|id| self.read_operation(id)).collect::<Result<Vec<_>, _>>()?;
        operations.sort_by_key(|op| (op.plan.epoch, op.plan.generation));
        if operations.iter().any(|op| {
            !op.uncertain_photos.is_empty()
                || op.outcomes.iter().any(|(id, outcome)| !self.authority.removed.contains(id) && *outcome != super::commit::Outcome::Success)
        }) {
            return Ok(());
        }
        let snapshot = validate_snapshot(&read_record(&self.generation_path(generation))?)?;
        let mut latest = std::collections::BTreeMap::new();
        for operation in &operations {
            for photo in &operation.plan.photos {
                if operation.outcomes.get(&photo.photo) == Some(&super::commit::Outcome::Success) {
                    latest.insert(&photo.photo, photo);
                }
            }
        }
        for (id, saved) in latest {
            if self.authority.removed.contains(id) {
                continue;
            }
            let photo =
                snapshot.photos.iter().find(|p| &p.id == id).ok_or_else(|| StoreError::Invalid("Journal retirement is missing a retained member".into()))?;
            let binding = snapshot.sources.get(id).ok_or_else(|| StoreError::Invalid("Journal retirement is missing a source binding".into()))?;
            if photo.unknown_operation
                || photo.committed.as_ref() != Some(&saved.recipe)
                || binding.original.as_ref() != Some(&saved.intended)
                || binding.source_identity != saved.binding.source_identity
            {
                return Err(StoreError::Invalid("Journal baseline is not durably incorporated; records retained".into()));
            }
        }
        let retired: Vec<_> = self.authority.operations.iter().chain(self.authority.superseded_operations.keys()).cloned().collect();
        let mut next = self.authority.clone();
        next.operations.clear();
        next.superseded_operations.clear();
        for operation in &operations {
            if Some(operation.plan.generation) != next.entry_generation && operation.plan.generation != next.min_recovery_generation {
                next.pins.remove(&operation.plan.generation);
            }
        }
        // Superseded generations have no remaining journal or visit reference.
        next.pins.retain(|pin| Some(*pin) == next.entry_generation || *pin == next.min_recovery_generation);
        self.publish_authority(next)?;
        for id in retired {
            let _ = std::fs::remove_file(self.operation_path(&id)?);
        }
        Ok(())
    }

    /// The caller has either acknowledged Save or explicitly approved discard.
    /// Only after this durable disposition may it release the member and callbacks.
    pub fn record_removal(&mut self, photo: PhotoId) -> Result<(), StoreError> {
        self.record_removals(&[photo])
    }

    /// One lifecycle publication for a visit discard. Validate every guard
    /// before changing any member, so a failed later row cannot strand live
    /// state against a partially changed authority.
    pub fn record_removals(&mut self, photos: &[PhotoId]) -> Result<(), StoreError> {
        self.writable()?;
        for id in &self.authority.operations {
            let operation = self.read_operation(id)?;
            if photos.iter().any(|photo| {
                operation.uncertain_photos.contains(photo)
                    || operation.outcomes.get(photo).is_some_and(|o| matches!(o, super::commit::Outcome::Unknown(_)))
                    || (operation.outcomes.get(photo) == Some(&super::commit::Outcome::Pending) && !operation.confirmed_unwritten.contains(photo))
            }) {
                return Err(StoreError::Uncertain("Resolve interrupted operations before removing this photograph".into()));
            }
        }
        let mut next = self.authority.clone();
        next.removed.extend(photos.iter().cloned());
        self.publish_authority(next)
    }

    /// A terminal transition fences all later writers and all fallback generations.
    /// The controller must first resolve and durably record outstanding operations.
    pub(super) fn retire_for_memory_only(&mut self) -> Result<(), StoreError> {
        self.writable()?;
        // This is an explicit opt-out while the controller retains every recipe
        // in memory, not Done. Never retire an unacknowledged sidecar operation.
        for id in &self.authority.operations {
            let operation = self.read_operation(id)?;
            if !operation.uncertain_photos.is_empty()
                || operation.outcomes.iter().any(|(photo, outcome)| !self.authority.removed.contains(photo) && *outcome != super::commit::Outcome::Success)
            {
                return Err(StoreError::Invalid("Resolve the pending XMP operation before turning recovery off".into()));
            }
        }
        let mut next = self.authority.clone();
        next.terminal = true;
        self.publish_authority(next)
    }

    pub fn complete(&mut self) -> Result<(), StoreError> {
        self.check_complete()?;
        let mut next = self.authority.clone();
        next.terminal = true;
        #[cfg(test)]
        test_pause("before_terminal");
        self.publish_authority(next)?;
        #[cfg(test)]
        test_pause("after_terminal");
        Ok(())
    }

    /// Every rule [`Self::complete`] enforces, publishing nothing. Open Images checks it
    /// before handing the photos to the editor, so its completion is never refused after
    /// the handoff (which would leave no exit but a retry that cannot succeed).
    pub fn check_complete(&self) -> Result<(), StoreError> {
        self.writable()?;
        let latest = self.authority.generations.last().ok_or_else(|| StoreError::Invalid("Completion requires an acknowledged snapshot".into()))?;
        let bytes = read_record(&self.generation_path(latest.number))?;
        if blake3::hash(&bytes).to_hex().as_str() != latest.checksum {
            return Err(StoreError::Invalid("Latest checkpoint is corrupt".into()));
        }
        let snapshot = validate_snapshot(&bytes)?;
        if snapshot.epoch != self.authority.epoch
            || snapshot.phase != super::Phase::Frozen
            || snapshot.requested.values().any(|a| matches!(a, super::Admission::Pending | super::Admission::Unresolved(_)))
        {
            return Err(StoreError::Invalid("Completion requires settled and frozen current ownership".into()));
        }
        let mut successes = Vec::new();
        for id in &self.authority.operations {
            let operation = self.read_operation(id)?;
            let unresolved = operation.plan.photos.iter().any(|planned| {
                let outcome = operation.outcomes.get(&planned.photo);
                !self.authority.removed.contains(&planned.photo)
                    && outcome != Some(&super::commit::Outcome::Success)
                    && !outcome.is_some_and(|outcome| super::commit::failure_is_moot(&snapshot, planned, outcome))
            });
            if !operation.uncertain_photos.is_empty() || unresolved {
                return Err(StoreError::Invalid("Unresolved sidecar operation prevents completion".into()));
            }
            successes.extend(operation.plan.photos);
        }
        for photo in &snapshot.photos {
            if !self.authority.removed.contains(&photo.id)
                && photo.pending().map_err(StoreError::Invalid)?
                && !successes.iter().any(|saved| {
                    saved.photo == photo.id
                        && saved.recipe == photo.working
                        && saved.revision == photo.revision
                        && snapshot.sources.get(&photo.id) == Some(&saved.binding)
                })
            {
                return Err(StoreError::Invalid("Current edits still require an acknowledged XMP operation".into()));
            }
        }
        Ok(())
    }

    fn generation_path(&self, generation: u64) -> PathBuf {
        self.dir.join(format!("generation-{generation}.json"))
    }

    fn writable(&self) -> Result<(), StoreError> {
        if !self.durable() {
            return Err(StoreError::Invalid("Automatic recovery is off; no durable store has been claimed".into()));
        }
        if self.uncertain {
            return Err(StoreError::Uncertain("Reopen and inspect publication records".into()));
        }
        if self.authority.terminal {
            return Err(StoreError::Invalid("Session is completed".into()));
        }
        Ok(())
    }

    fn publish_authority(&mut self, authority: Authority) -> Result<(), StoreError> {
        serialized_size(&authority, MAX_AUTHORITY_BYTES)?;
        let bytes = encode_record(&authority)?;
        self.publish(&self.dir.join("authority.json"), &bytes)?;
        self.authority = authority;
        Ok(())
    }

    fn publish(&mut self, path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
        let result = write_record(path, bytes);
        if matches!(result, Err(StoreError::Uncertain(_))) {
            self.uncertain = true;
        }
        result
    }
}

fn invalid(error: serde_json::Error) -> StoreError {
    StoreError::Invalid(error.to_string())
}

struct LimitedBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let size = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|size| *size <= self.limit)
            .ok_or_else(|| std::io::Error::other("Recovery record exceeds limit"))?;
        if size > self.bytes.capacity() {
            let capacity = size.max(self.bytes.capacity().saturating_mul(2)).min(self.limit);
            self.bytes.try_reserve_exact(capacity.saturating_sub(self.bytes.len())).map_err(std::io::Error::other)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode_record(value: &impl Serialize) -> Result<Vec<u8>, StoreError> {
    let mut sink = LimitedBytes { bytes: Vec::new(), limit: MAX_RECORD_BYTES as usize };
    serde_json::to_writer(&mut sink, value).map_err(invalid)?;
    Ok(sink.bytes)
}

pub(super) fn serialized_size(value: &impl Serialize, limit: usize) -> Result<usize, StoreError> {
    struct Count {
        size: usize,
        limit: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.size = self
                .size
                .checked_add(bytes.len())
                .filter(|n| *n <= self.limit)
                .ok_or_else(|| std::io::Error::other("Recovery payload exceeds bounded storage budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Count { size: 0, limit };
    serde_json::to_writer(&mut sink, value).map_err(invalid)?;
    Ok(sink.size)
}

fn cache_size(root: &Path) -> Result<u64, StoreError> {
    let mut bytes = 0u64;
    let mut count = 0usize;
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            count += 1;
            if count > 100_000 {
                return Err(StoreError::Invalid("Recovery inventory exceeds 100,000 entries".into()));
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() {
                bytes = bytes.checked_add(entry.metadata()?.len()).ok_or_else(|| StoreError::Invalid("Recovery size overflow".into()))?;
            }
        }
    }
    Ok(bytes)
}

/// Compiled only into this crate's test binary. The parent test terminates only
/// the exact child it spawned after this file-boundary acknowledgement.
#[cfg(test)]
pub(super) fn test_pause(stage: &str) {
    if std::env::var("PHOTOCRAFT_NATIVE_KILL_STAGE").as_deref() != Ok(stage) {
        return;
    }
    let marker = std::env::var_os("PHOTOCRAFT_NATIVE_KILL_MARKER").expect("test marker path");
    std::fs::write(marker, format!("{}:{stage}", std::process::id())).expect("publish process boundary marker");
    let continuation = std::env::var_os("PHOTOCRAFT_NATIVE_CONTINUE_MARKER").map(PathBuf::from);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(path) = &continuation {
            if path.exists() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "Test continuation marker was not released within 30 seconds");
        }
        std::thread::park_timeout(std::time::Duration::from_secs(1));
    }
}

fn now_ms() -> Result<u64, StoreError> {
    let time = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| StoreError::Invalid(e.to_string()))?;
    u64::try_from(time.as_millis()).map_err(|e| StoreError::Invalid(e.to_string()))
}

pub(super) fn write_record(path: &Path, payload: &[u8]) -> Result<(), StoreError> {
    #[derive(Serialize)]
    struct BorrowedEnvelope<'a> {
        checksum: String,
        payload: &'a str,
    }
    let envelope = BorrowedEnvelope {
        checksum: blake3::hash(payload).to_hex().to_string(),
        payload: std::str::from_utf8(payload).map_err(|e| StoreError::Invalid(e.to_string()))?,
    };
    let bytes = encode_record(&envelope)?;
    match atomic_write_outcome(path, &bytes)? {
        PublicationOutcome::Published => Ok(()),
        PublicationOutcome::PublishedUnconfirmed(error) => Err(StoreError::Uncertain(error.to_string())),
    }
}

pub(super) fn read_record(path: &Path) -> Result<Vec<u8>, StoreError> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(StoreError::Invalid("Recovery record is not a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(StoreError::Invalid("Recovery record exceeds limit".into()));
    }
    let envelope: Envelope = serde_json::from_slice(&bytes).map_err(invalid)?;
    if blake3::hash(envelope.payload.as_bytes()).to_hex().as_str() != envelope.checksum {
        return Err(StoreError::Invalid("Recovery checksum mismatch".into()));
    }
    Ok(envelope.payload.into_bytes())
}

/// A checkpoint read for a fallback decision. Verified corruption (a bad envelope, size or
/// checksum) or a record that is gone for good returns None, so the caller may fall back to
/// an older checkpoint. A record that cannot be read right now (a sharing violation, access
/// denied) is an error: it is never silently replaced by an older, emptier state.
fn read_checkpoint(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match read_record(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(StoreError::Io(error)) if error.kind() != std::io::ErrorKind::NotFound => Err(StoreError::Io(error)),
        Err(_) => Ok(None),
    }
}

fn read_authority(path: &Path) -> Result<Authority, StoreError> {
    let bytes = read_record(path)?;
    if bytes.len() > MAX_AUTHORITY_BYTES {
        return Err(StoreError::Invalid("Recovery authority exceeds 4 MiB bounded reference budget".into()));
    }
    let authority: Authority = serde_json::from_slice(&bytes).map_err(invalid)?;
    if authority.generations.len() > MAX_GENERATIONS
        || authority.pins.len() > MAX_GENERATIONS
        || authority.operations.len() > MAX_OPERATIONS
        || authority.superseded_operations.len() > MAX_HISTORY
        || authority.removed.len() > super::MAX_PHOTOS
        || authority.generations.windows(2).any(|pair| pair[0].number >= pair[1].number)
        || authority.pins.iter().any(|pin| !authority.generations.iter().any(|g| &g.number == pin))
        || authority.entry_generation.is_some_and(|entry| !authority.pins.contains(&entry))
        || authority
            .operations
            .iter()
            .chain(authority.superseded_operations.keys())
            .chain(authority.superseded_operations.values())
            .any(|id| id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(StoreError::Invalid("Recovery authority has excessive or invalid references".into()));
    }
    Ok(authority)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_session::{Admission, Phase};
    use std::collections::BTreeMap;

    #[test]
    fn serialization_stops_at_limit_before_allocating_full_output() {
        let mut sink = LimitedBytes { bytes: Vec::new(), limit: 128 };
        assert!(serde_json::to_writer(&mut sink, [255u8; 1024].as_slice()).is_err());
        assert!(sink.bytes.len() <= 128);
        assert!(sink.bytes.capacity() <= 128);
    }

    #[test]
    fn rejected_structural_budget_preserves_the_previous_authority_and_snapshot() {
        let root = root("resource-budget");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        store.checkpoint(&snapshot(store.epoch(), 1)).unwrap();
        let before = std::fs::read(root.join("session/authority.json")).unwrap();
        let mut oversized = snapshot(store.epoch(), 2);
        oversized.requested.insert("ticket".into(), Admission::Rejected("x".repeat(32769)));
        assert!(store.checkpoint(&oversized).is_err());
        assert_eq!(std::fs::read(root.join("session/authority.json")).unwrap(), before);
        assert_eq!(store.load().unwrap().unwrap().session.generation, 1);
        assert!(!root.join("session/generation-2.json").exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_snapshot_keeps_large_numeric_packets_out_of_value_trees() {
        let mut candidate = snapshot(1, 1);
        candidate.sources.insert(
            PhotoId("retained".into()),
            super::super::SourceBinding {
                source: PathBuf::from("source.nef"),
                source_fingerprint: "0".repeat(64),
                source_identity: "identity".into(),
                target: PathBuf::from("source.xmp"),
                original: Some(vec![255; MAX_BOUND_XMP_BYTES]),
                target_identity: Some("sidecar".into()),
            },
        );
        let encoded = encode_record(&candidate).unwrap();
        let decoded = validate_snapshot(&encoded).unwrap();
        assert_eq!(decoded.sources.values().next().unwrap().original.as_ref().unwrap().len(), MAX_BOUND_XMP_BYTES);
        candidate.view_context = serde_json::Value::String("x".repeat(256 * 1024));
        assert!(validate_snapshot(&encode_record(&candidate).unwrap()).is_err());
    }

    #[test]
    fn older_large_acknowledged_snapshot_is_retained_instead_of_silent_fallback() {
        let root = root("older-capacity");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        store.checkpoint(&snapshot(store.epoch(), 1)).unwrap();
        store.checkpoint(&snapshot(store.epoch(), 2)).unwrap();
        let mut older = snapshot(store.epoch(), 2);
        older.sources.insert(
            PhotoId("retained".into()),
            super::super::SourceBinding {
                source: PathBuf::from("source.nef"),
                source_fingerprint: "0".repeat(64),
                source_identity: "identity".into(),
                target: PathBuf::from("source.xmp"),
                original: Some(vec![b' '; MAX_BOUND_XMP_BYTES + 1]),
                target_identity: Some("sidecar".into()),
            },
        );
        let bytes = encode_record(&older).unwrap();
        write_record(&store.generation_path(2), &bytes).unwrap();
        let mut authority = store.authority.clone();
        authority.generations.last_mut().unwrap().checksum = blake3::hash(&bytes).to_hex().to_string();
        store.publish_authority(authority).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Unsupported(_))));
        assert!(store.generation_path(2).exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn envelope_serialization_observes_escaped_size_budget() {
        #[derive(Serialize)]
        struct Payload<'a> {
            payload: &'a str,
        }
        let text = "\\\"".repeat(64);
        let exact = serde_json::to_vec(&Payload { payload: &text }).unwrap();
        assert_eq!(serialized_size(&Payload { payload: &text }, exact.len()).unwrap(), exact.len());
        assert!(serialized_size(&Payload { payload: &text }, exact.len() - 1).is_err());
    }

    #[test]
    fn aged_cleanup_removes_only_unreferenced_records_below_the_barrier() {
        let root = root("age-budget");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        store.checkpoint(&snapshot(store.epoch(), 1)).unwrap();
        store.pin_entry(1).unwrap();
        store.fence_recovery(1).unwrap();
        let redundant = store.generation_path(0);
        let orphan = store.generation_path(99);
        std::fs::write(&redundant, b"superseded redundant checkpoint").unwrap();
        std::fs::write(&orphan, b"unresolved orphan checkpoint").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(RETIRED_AGE_SECS + 60);
        for path in [&redundant, &orphan, &store.generation_path(1)] {
            std::fs::OpenOptions::new().write(true).open(path).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
        }
        store.cleanup_redundant_records(false).unwrap();
        assert!(!redundant.exists());
        assert!(orphan.exists());
        assert!(store.generation_path(1).exists());
        let restored = store.load().unwrap().unwrap();
        assert!(!restored.used_fallback);
        assert_eq!(restored.session.requested.get("rejected"), Some(&Admission::Rejected("fixture".into())));
        assert_eq!(restored.session.generation, 99, "Reserve beyond the retained orphan without adopting its contents");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_capacity_fails_before_new_record_or_authority_publication() {
        let root = root("journal-budget");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        store.checkpoint(&snapshot(store.epoch(), 1)).unwrap();
        let operation = |index: usize| {
            let mut plan = super::super::commit::Plan {
                id: String::new(),
                session: "session".into(),
                epoch: 1,
                generation: 1,
                scope: super::super::commit::SaveScope::Photo(PhotoId(format!("photo-{index}"))),
                photos: Vec::new(),
            };
            plan.id = super::super::commit::fingerprint_plan(&plan).unwrap();
            super::super::commit::Operation {
                schema: 2,
                plan,
                outcomes: Default::default(),
                uncertain_photos: Default::default(),
                confirmed_unwritten: Default::default(),
            }
        };
        for index in 0..MAX_OPERATIONS {
            let op = operation(index);
            store.write_operation(&op.plan.id, &op).unwrap();
        }
        let before = std::fs::read(root.join("session/authority.json")).unwrap();
        let excess = operation(MAX_OPERATIONS);
        assert!(store.write_operation(&excess.plan.id, &excess).is_err());
        assert!(!store.operation_path(&excess.plan.id).unwrap().exists());
        assert_eq!(std::fs::read(root.join("session/authority.json")).unwrap(), before);
        assert_eq!(store.operation_ids().len(), MAX_OPERATIONS);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    fn root(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!("pc-raw-{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)))
    }
    fn snapshot(epoch: u64, generation: u64) -> Session {
        Session {
            schema: 1,
            id: "session".into(),
            epoch,
            generation,
            cached_generation: None,
            phase: Phase::Frozen,
            photos: vec![],
            requested: BTreeMap::from([("rejected".into(), Admission::Rejected("fixture".into()))]),
            sources: BTreeMap::new(),
            removed: BTreeSet::new(),
            view_context: serde_json::Value::Null,
        }
    }
    #[test]
    fn native_publication_orders_and_reopens_under_new_ownership() {
        let root = root("reopen");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        let old_epoch = store.epoch();
        assert!(matches!(RecoveryStore::claim(&root, "session"), Err(StoreError::Owned)));
        let ack = store.checkpoint(&snapshot(old_epoch, 2)).unwrap();
        assert_eq!(ack.generation, 2);
        assert!(store.checkpoint(&snapshot(old_epoch, 1)).is_err());
        store.pin(2).unwrap();
        drop(store);
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        assert!(store.epoch() > old_epoch);
        assert!(store.checkpoint(&snapshot(old_epoch, 3)).is_err());
        assert_eq!(store.load().unwrap().unwrap().session.generation, 2);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_edit_checkpoint_then_finish_publishes_new_frozen_generation() {
        let root = root("finish-barrier");
        let mut store = RecoveryStore::claim(&root, "session-one").unwrap();
        let mut session = super::super::tests::session();
        session.epoch = store.epoch();
        let editing = store.checkpoint(&session).unwrap();
        session.acknowledge_cache(&editing.session, editing.epoch, editing.generation);
        assert!(session.current_cached());
        session.begin_finish().unwrap();
        session.freeze().unwrap();
        assert!(!session.current_cached());
        let frozen = store.checkpoint(&session).unwrap();
        assert!(frozen.generation > editing.generation);
        assert_eq!(store.commit_snapshot(frozen.generation).unwrap().phase, Phase::Frozen);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn corrupt_latest_falls_back_but_terminal_authority_prevents_resurrection() {
        let root = root("fallback");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        store.checkpoint(&snapshot(store.epoch(), 1)).unwrap();
        store.checkpoint(&snapshot(store.epoch(), 2)).unwrap();
        std::fs::write(store.generation_path(2), b"damaged").unwrap();
        assert!(store.load().unwrap().unwrap().used_fallback);
        assert!(store.complete().is_err(), "corrupt newest checkpoint cannot be silently retired");
        store.checkpoint(&snapshot(store.epoch(), 3)).unwrap();
        store.complete().unwrap();
        assert!(store.checkpoint(&snapshot(store.epoch(), 3)).is_err());
        drop(store);
        assert!(RecoveryStore::claim(&root, "session").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn missing_or_corrupt_authority_preserves_records_and_blocks_fallback() {
        let root = root("authority");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        store.checkpoint(&snapshot(store.epoch(), 1)).unwrap();
        drop(store);
        let authority = root.join("session/authority.json");
        std::fs::write(&authority, b"damaged").unwrap();
        assert!(RecoveryStore::claim(&root, "session").is_err());
        std::fs::remove_file(authority).unwrap();
        assert!(RecoveryStore::claim(&root, "session").is_err());
        assert!(root.join("session/generation-1.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_keeps_entry_pin_and_removal_survives_fallback() {
        let root = root("retention");
        let mut store = RecoveryStore::claim(&root, "session-one").unwrap();
        let mut snapshot = super::super::tests::session();
        snapshot.epoch = store.epoch();
        store.checkpoint(&snapshot).unwrap();
        store.pin(1).unwrap();
        for number in 2..=32 {
            snapshot.generation = number;
            store.checkpoint(&snapshot).unwrap();
        }
        assert_eq!(store.authority.generations.len(), 3);
        assert!(store.generation_path(1).exists());
        store.record_removal(PhotoId("photo-one".into())).unwrap();
        std::fs::write(store.generation_path(32), b"corrupt latest").unwrap();
        let restored = store.load().unwrap().unwrap();
        assert!(restored.used_fallback);
        assert!(restored.session.photos.is_empty());
        assert!(restored.session.removed.contains(&PhotoId("photo-one".into())));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn orphan_generation_does_not_force_overwrite_or_prevent_progress() {
        let root = root("orphan");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        store.checkpoint(&snapshot(store.epoch(), 1)).unwrap();
        write_record(&store.generation_path(2), &serde_json::to_vec(&snapshot(store.epoch(), 2)).unwrap()).unwrap();
        drop(store);
        let mut resumed = RecoveryStore::claim(&root, "session").unwrap();
        assert_eq!(resumed.next_generation().unwrap(), 3);
        resumed.checkpoint(&snapshot(resumed.epoch(), 3)).unwrap();
        assert!(resumed.generation_path(2).exists(), "orphan retained for reconciliation");
        drop(resumed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "Dedicated child process entry point; exercised by independent_process_restart"]
    fn checkpoint_process_helper() {
        let root = PathBuf::from(std::env::var_os("PHOTOCRAFT_RECOVERY_TEST_ROOT").expect("isolated test root"));
        let mode = std::env::var("PHOTOCRAFT_RECOVERY_TEST_MODE").unwrap();
        let mut store = RecoveryStore::claim(&root, "session-one").unwrap();
        if mode == "write" {
            let mut snapshot = super::super::tests::session();
            snapshot.epoch = store.epoch();
            let ack = store.checkpoint(&snapshot).unwrap();
            assert_eq!(ack.generation, 1);
            let mut marker = File::create(root.join("acknowledged")).unwrap();
            std::io::Write::write_all(&mut marker, ack.checksum.as_bytes()).unwrap();
            marker.sync_all().unwrap();
            let mut wait = [0u8; 1];
            let _ = std::io::stdin().read(&mut wait); // parent kills only this recorded child
        } else {
            let restored = store.load().unwrap().unwrap();
            assert_eq!(restored.session.photos[0].working, super::super::tests::recipe(1.));
            assert_eq!(restored.session.photos[0].committed, Some(super::super::tests::recipe(0.)));
            assert!(restored.session.photos[0].hidden);
            std::fs::write(root.join("restored"), serde_json::to_vec(&restored.session).unwrap()).unwrap();
        }
    }

    #[test]
    fn independent_process_restart() {
        use std::process::{Command, Stdio};
        let root = root("process");
        let command = |mode: &str| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--ignored", "--exact", "raw_session::store::tests::checkpoint_process_helper", "--nocapture"])
                .env("PHOTOCRAFT_RECOVERY_TEST_ROOT", &root)
                .env("PHOTOCRAFT_RECOVERY_TEST_MODE", mode)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            command
        };
        let mut writer = command("write").spawn().unwrap();
        let pid = writer.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !root.join("acknowledged").exists() && std::time::Instant::now() < deadline {
            if writer.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let acknowledged = root.join("acknowledged").exists();
        let _ = writer.kill();
        writer.wait().unwrap();
        assert!(acknowledged, "dedicated writer {pid} did not acknowledge");
        let reader = command("read").output().unwrap();
        assert!(reader.status.success(), "{}", String::from_utf8_lossy(&reader.stderr));
        assert!(root.join("restored").exists());
        println!("acknowledged writer PID {pid} terminated; independent reader restored generation 1");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn future_session_schema_is_not_silently_replaced_by_older_checkpoint() {
        let root = root("future");
        let mut store = RecoveryStore::claim(&root, "session").unwrap();
        store.checkpoint(&snapshot(store.epoch(), 1)).unwrap();
        store.checkpoint(&snapshot(store.epoch(), 2)).unwrap();
        let mut future = snapshot(store.epoch(), 2);
        future.schema = 2;
        let bytes = serde_json::to_vec(&future).unwrap();
        write_record(&store.generation_path(2), &bytes).unwrap();
        let mut authority = store.authority.clone();
        authority.generations.last_mut().unwrap().checksum = blake3::hash(&bytes).to_hex().to_string();
        store.publish_authority(authority).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Unsupported(_))));
        assert!(store.generation_path(2).exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
