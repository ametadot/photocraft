//! Native source/sidecar binding, immutable plans and sequential journalled commits.
//! This is not a transaction across files. External applications can still race the
//! final observation/replacement; our private process lock serializes PhotoCraft only.

use super::store::{RecoveryStore, StoreError};
use super::{NativeRecipe, Phase, PhotoId, Session};
use photocraft_format::atomic::PublicationOutcome;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

pub use super::SourceBinding as Binding;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedPhoto {
    pub photo: PhotoId,
    pub revision: u64,
    pub binding: Binding,
    pub intended: Vec<u8>,
    pub recipe: NativeRecipe,
    pub hidden: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub id: String,
    pub session: String,
    pub epoch: u64,
    pub generation: u64,
    pub scope: SaveScope,
    pub photos: Vec<PlannedPhoto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SaveScope {
    All,
    /// Same retained membership as Done, but the acknowledged visit stays open.
    SaveEdits,
    Photo(PhotoId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Pending,
    Success,
    Failed(String),
    Conflict(String),
    Unknown(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub schema: u32,
    pub plan: Plan,
    pub outcomes: BTreeMap<PhotoId, Outcome>,
    /// Separate from the displayed outcome: divergent bytes after a lost write
    /// response are a conflict whose publication history is still uncertain.
    pub uncertain_photos: std::collections::BTreeSet<PhotoId>,
    /// Pending plus this durable observation means expected-before bytes were
    /// seen during reconciliation. Explicit discard can then remove that photo;
    /// an unreconciled Pending journal remains protected by default.
    pub confirmed_unwritten: std::collections::BTreeSet<PhotoId>,
}

fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::Invalid(message.into())
}

fn publish_sidecar(path: &Path, bytes: &[u8]) -> std::io::Result<PublicationOutcome> {
    #[cfg(not(test))]
    {
        photocraft_format::atomic::atomic_write_outcome(path, bytes)
    }
    #[cfg(test)]
    {
        use photocraft_format::atomic::{PublicationStage, atomic_write_outcome_observed};
        atomic_write_outcome_observed(path, bytes, &|stage| {
            super::store::test_pause(match stage {
                PublicationStage::BeforeStaging => "before_staging",
                PublicationStage::BeforeReplacement => "before_replacement",
                PublicationStage::AfterReplacement => "after_replacement",
            })
        })
    }
}

pub(super) fn source_fingerprint(path: &Path) -> Result<(String, String), StoreError> {
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("Source is not a regular file"));
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).ok_or_else(|| invalid("Invalid source read length"))?);
    }
    Ok((hasher.finalize().to_hex().to_string(), physical_identity(&file)?))
}

pub(super) fn physical_identity(file: &File) -> Result<String, StoreError> {
    #[cfg(windows)]
    {
        let information = winapi_util::file::information(file)?;
        Ok(format!("windows:{}:{}:{:?}", information.volume_serial_number(), information.file_index(), information.creation_time()))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = file;
        Err(invalid("Native physical identity unavailable on this platform"))
    }
}

pub(super) fn sidecar_bytes(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(invalid("Sidecar is not a regular file"));
    }
    let limit = super::store::MAX_BOUND_XMP_BYTES;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid("Sidecar exceeds the 1 MiB per-packet recovery budget; source packet retained"));
    }
    Ok(Some(bytes))
}

/// Camera RAW extensions that own `<stem>.xmp` by the Lightroom/Bridge convention, whether
/// or not PhotoCraft decodes them: a JPEG or TIFF beside any of them never takes it.
const RAW_SIDECAR_OWNERS: &[&str] = &[
    "3fr", "ari", "arw", "bay", "braw", "cap", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "drf", "eip", "erf", "fff", "gpr", "iiq", "k25", "kdc", "mdc", "mef",
    "mos", "mrw", "nef", "nrw", "orf", "pef", "ptx", "pxn", "r3d", "raf", "raw", "rw2", "rwl", "rwz", "sr2", "srf", "srw", "x3f",
];

/// Camera Raw sources whose same-stem siblings decide who owns `<stem>.xmp`.
fn camera_raw_source_name(name: &str) -> bool {
    raw_name(name)
        || Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ["jpg", "jpeg", "jpe", "jfif", "jif", "tif", "tiff"].iter().any(|known| e.eq_ignore_ascii_case(known)))
}

fn raw_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| photocraft_raw::EXTENSIONS.iter().chain(RAW_SIDECAR_OWNERS).any(|raw| e.eq_ignore_ascii_case(raw)))
}

/// The sidecar [`bind`] chooses for `source` (an existing file or the name a new one gets),
/// by the same ownership rule (the photograph is read only to match a PhotoCraft stem packet):
/// the open-time reader and batch pairing use it so they never show or pair a different
/// sidecar than Done writes.
pub fn sidecar_target(source: &Path) -> Result<PathBuf, StoreError> {
    resolve_target(&std::fs::canonicalize(source)?)
}

fn resolve_target(source: &Path) -> Result<PathBuf, StoreError> {
    let parent = source.parent().ok_or_else(|| invalid("Source has no parent"))?;
    let conventional = source.with_extension("xmp");
    let own = source.file_name().and_then(|s| s.to_str()).ok_or_else(|| invalid("Source filename cannot be serialized losslessly"))?;
    let own_stem = source.file_stem().and_then(|s| s.to_str()).ok_or_else(|| invalid("Source filename cannot be serialized losslessly"))?;
    let stem = conventional.file_name().and_then(|s| s.to_str()).ok_or_else(|| invalid("Sidecar filename cannot be serialized losslessly"))?;
    let full = format!("{own}.xmp");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            names.push((name.to_owned(), entry.path()));
        }
    }
    let siblings: Vec<&str> = names
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| !name.eq_ignore_ascii_case(own) && camera_raw_source_name(name))
        .filter(|name| Path::new(name).file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(own_stem)))
        .collect();
    let stem_path = names.iter().find(|(name, _)| name.eq_ignore_ascii_case(stem)).map(|(_, path)| path.clone());
    let has_full = names.iter().any(|(name, _)| name.eq_ignore_ascii_case(&full));
    // Every non-RAW source (whatever its extension: admission routes by content) follows
    // the rendered rule.
    let rendered = !raw_name(own);
    if raw_name(own)
        && let Some(saved) = stem_path.as_deref().and_then(saved_kind)
        && saved != crate::camera_raw_source::SourceKind::Raw
    {
        return Err(invalid(format!(
            "{stem} holds PhotoCraft settings saved for a JPEG or TIFF, not for {own}; rename it to that file's own <name>.<ext>.xmp or move it. Nothing was changed"
        )));
    }
    // Classify stem packets that the legacy resolver would adopt. For rendered photos,
    // neither matching CONTENT kind nor absence of a RAW sibling proves photo ownership:
    // refuse those packets below before discovery or admission. Reading content kind may
    // fail, which remains an error (retry), never permission to change the binding.
    let owns_stem = !rendered
        || match stem_path.as_deref().and_then(saved_kind) {
            Some(saved) => content_kind(source)? == saved,
            None => !has_full && !siblings.iter().any(|name| raw_name(name)),
        };
    if rendered && let Some(stem_path) = stem_path.as_deref() {
        let alias = names
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&full))
            .map(|(_, path)| same_file::is_same_file(stem_path, path))
            .transpose()?
            .unwrap_or(false);
        if owns_stem || alias {
            return Err(invalid(format!(
                "Sidecar ownership is unproven: {stem} is not explicitly bound to {own}. Camera Raw discovery and admission refuse this stem packet; original files and metadata were not changed"
            )));
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for (name, path) in &names {
        if !name.eq_ignore_ascii_case(&full) && !(owns_stem && name.eq_ignore_ascii_case(stem)) {
            continue;
        }
        let path = std::fs::canonicalize(path)?;
        let mut alias = false;
        for old in &candidates {
            alias |= same_file::is_same_file(old, &path)?;
        }
        if !alias {
            candidates.push(path);
        }
    }
    if candidates.len() > 1 {
        let names: Vec<String> = candidates.iter().filter_map(|path| path.file_name().map(|name| name.to_string_lossy().into_owned())).collect();
        return Err(invalid(format!(
            "Multiple distinct sidecars require a binding decision: {} all belong to {own}; rename or move the one that does not belong to it. Nothing was changed",
            names.join(" and ")
        )));
    }
    // A new sidecar for a JPEG/TIFF is always its own <name>.<ext>.xmp.
    let target = candidates.pop().unwrap_or_else(|| if rendered { parent.join(&full) } else { conventional });
    Ok(target)
}

/// The kind admission gives a source, from its content (bounded, chunked read).
fn content_kind(source: &Path) -> Result<crate::camera_raw_source::SourceKind, StoreError> {
    let bytes = photocraft_format::read_file(source)?;
    crate::camera_raw_source::inspect(&bytes)
        .map(|inspection| inspection.kind)
        .map_err(|error| invalid(format!("Cannot confirm which file owns its sidecar: {error}")))
}

/// The source kind of the PhotoCraft recipe saved in a sidecar; None for a foreign,
/// unreadable or over-budget packet (it is then judged by the folder rule alone).
fn saved_kind(path: &Path) -> Option<crate::camera_raw_source::SourceKind> {
    let bytes = sidecar_bytes(path).ok().flatten()?;
    super::xmp::load_native(&bytes).ok().flatten().map(|recipe| recipe.processing.source_kind)
}

/// Inspect real directory entries; distinct plausible packets are ambiguous even
/// when they contain identical bytes. Physical aliases of one file coalesce.
/// A RAW owns `<stem>.xmp` (two same-stem RAWs in one session still refuse to share it).
/// A JPEG or TIFF uses its explicit `<name>.<ext>.xmp`. A legacy/foreign stem packet
/// that would otherwise be adopted is refused at discovery, not merely at saving:
/// matching rendered kind does not prove ownership by this photograph. No migration
/// or alternative packet is created to bypass that ambiguity.
pub fn bind(source: &Path) -> Result<Binding, StoreError> {
    let source = std::fs::canonicalize(source)?;
    let (source_fingerprint, source_identity) = source_fingerprint(&source)?;
    let target = resolve_target(&source)?;
    if target.try_exists()? && same_file::is_same_file(&source, &target)? {
        return Err(invalid("Sidecar aliases the retained source"));
    }
    let original = sidecar_bytes(&target)?;
    let target_identity = if original.is_some() { Some(physical_identity(&File::open(&target)?)?) } else { None };
    Ok(Binding { source, source_fingerprint, source_identity, target, original, target_identity })
}

/// A binding an earlier build recorded for a JPEG/TIFF that had no sidecar yet: it named
/// `<stem>.xmp`, where this build names a new one `<name>.<ext>.xmp`. With no file at either
/// name and the same source, nothing changed: the current binding replaces it (migration).
pub(super) fn legacy_unwritten_rendered(recorded: &Binding, current: &Binding) -> bool {
    let named = |path: &Path| path.to_string_lossy().to_lowercase();
    recorded.source == current.source
        && recorded.source_fingerprint == current.source_fingerprint
        && recorded.source_identity == current.source_identity
        && recorded.source.file_name().and_then(|n| n.to_str()).is_some_and(|name| !raw_name(name))
        && named(&recorded.target) == named(&recorded.source.with_extension("xmp"))
        && recorded.original.is_none()
        && recorded.target_identity.is_none()
        && current.original.is_none()
        && !recorded.target.try_exists().unwrap_or(true)
}

/// [`verify`] for the controller: which retained photo's source or sidecar changed.
pub(super) fn verify_binding(binding: &Binding) -> Result<(), StoreError> {
    verify(binding)
}

fn verify(binding: &Binding) -> Result<(), StoreError> {
    let current = bind(&binding.source)?;
    if current != *binding {
        return Err(invalid("Source, sidecar binding or packet changed; prepare a new reviewed plan"));
    }
    Ok(())
}

/// Legacy bindings must not bypass the ownership rule enforced during discovery.
/// Until packets carry an explicit owning photograph, a rendered source's stem save
/// and two sensor sources sharing a stem require an owner decision. Refuse without
/// adopting, moving, renaming or creating a different sidecar.
fn verify_write_ownership(binding: &Binding) -> Result<(), StoreError> {
    let source = &binding.source;
    let conventional = source.with_extension("xmp");
    let target_is_stem = conventional == binding.target
        || (cfg!(windows) && conventional.to_string_lossy().eq_ignore_ascii_case(&binding.target.to_string_lossy()))
        || (conventional.try_exists()? && binding.target.try_exists()? && same_file::is_same_file(&conventional, &binding.target)?);
    if !target_is_stem {
        return Ok(());
    }
    let own = source.file_name().and_then(|s| s.to_str()).ok_or_else(|| invalid("Source filename cannot be serialized losslessly"))?;
    let sidecar = binding.target.file_name().map_or_else(|| "?".into(), |s| s.to_string_lossy().into_owned());
    if !raw_name(own) {
        return Err(invalid(format!(
            "Sidecar ownership is unproven: {sidecar} is not explicitly bound to {own}. Automatic stem-sidecar replacement is refused; original files and metadata were not changed"
        )));
    }
    let parent = source.parent().ok_or_else(|| invalid("Source has no parent"))?;
    let stem = source.file_stem().ok_or_else(|| invalid("Source has no filename stem"))?.to_string_lossy();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
        if name.eq_ignore_ascii_case(own) || !path.file_stem().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(&stem)) || !camera_raw_source_name(name)
        {
            continue;
        }
        let sensor = raw_name(name) || content_kind(&path)? == crate::camera_raw_source::SourceKind::Raw;
        if sensor {
            return Err(invalid(format!(
                "Sidecar ownership is ambiguous: {own} and {name} share {sidecar}. Automatic replacement is refused; original files and metadata were not changed"
            )));
        }
    }
    Ok(())
}

pub fn prepare(session: &Session, bindings: &BTreeMap<PhotoId, Binding>) -> Result<Plan, StoreError> {
    prepare_scope(session, bindings, SaveScope::All)
}

pub fn prepare_save(session: &Session, bindings: &BTreeMap<PhotoId, Binding>) -> Result<Plan, StoreError> {
    prepare_scope(session, bindings, SaveScope::SaveEdits)
}

pub fn prepare_photo(session: &Session, bindings: &BTreeMap<PhotoId, Binding>, photo: &PhotoId) -> Result<Plan, StoreError> {
    if session.removed.contains(photo) || !session.photos.iter().any(|member| &member.id == photo) {
        return Err(invalid("Removal save member is not retained"));
    }
    prepare_scope(session, bindings, SaveScope::Photo(photo.clone()))
}

fn prepare_scope(session: &Session, bindings: &BTreeMap<PhotoId, Binding>, scope: SaveScope) -> Result<Plan, StoreError> {
    if session.phase != Phase::Frozen || !session.current_cached() {
        return Err(invalid("Finish requires frozen, acknowledged recovery state"));
    }
    let mut photos: Vec<PlannedPhoto> = Vec::new();
    let mut packet_bytes = 0usize;
    for photo in &session.photos {
        if matches!(&scope, SaveScope::Photo(id) if id != &photo.id) {
            continue;
        }
        if session.removed.contains(&photo.id) || !photo.pending().map_err(invalid)? {
            continue;
        }
        let binding = bindings.get(&photo.id).ok_or_else(|| invalid("Missing source binding"))?;
        verify(binding)?;
        for (other_id, other) in bindings {
            if other_id == &photo.id || session.removed.contains(other_id) || other.source_identity == binding.source_identity {
                continue;
            }
            let same = other.target == binding.target
                || (cfg!(windows) && other.target.to_string_lossy().to_lowercase() == binding.target.to_string_lossy().to_lowercase())
                || (other.target.try_exists()? && binding.target.try_exists()? && same_file::is_same_file(&other.target, &binding.target)?);
            if same {
                return Err(invalid(shared_target(binding, other)));
            }
        }
        for prior in &photos {
            let same = prior.binding.target == binding.target
                || (prior.binding.target.try_exists()? && binding.target.try_exists()? && same_file::is_same_file(&prior.binding.target, &binding.target)?);
            if same {
                return Err(invalid(shared_target(binding, &prior.binding)));
            }
        }
        verify_write_ownership(binding)?;
        let baseline = photo.committed.as_ref().ok_or_else(|| invalid("Unresolved committed baseline"))?;
        let intended = super::xmp::plan(binding.original.as_deref(), baseline, &photo.working)
            .map_err(invalid)?
            .ok_or_else(|| invalid("Pending recipe produced no operation"))?;
        if intended.len() > super::store::MAX_BOUND_XMP_BYTES {
            return Err(invalid("Planned XMP exceeds the 1 MiB per-packet recovery budget; no sidecar was changed"));
        }
        packet_bytes = packet_bytes
            .checked_add(binding.original.as_ref().map_or(0, Vec::len))
            .and_then(|n| n.checked_add(intended.len()))
            .filter(|n| *n <= super::store::MAX_PLAN_XMP_BYTES)
            .ok_or_else(|| invalid("Planned XMP packets exceed the 5 MiB aggregate journal budget"))?;
        photos.push(PlannedPhoto {
            photo: photo.id.clone(),
            revision: photo.revision,
            binding: binding.clone(),
            intended,
            recipe: photo.working.clone(),
            hidden: photo.hidden,
        });
    }
    let mut plan = Plan { id: String::new(), session: session.id.clone(), epoch: session.epoch, generation: session.generation, scope, photos };
    validate_projected_baselines(session, &plan)?;
    plan.id = fingerprint_plan(&plan)?;
    Ok(plan)
}

/// A RAW and a JPEG (or any two photos) with the same file stem would publish one XMP.
/// Nothing is written; the message names both photographs and the shared sidecar.
fn shared_target(binding: &Binding, other: &Binding) -> String {
    let name = |path: &Path| path.file_name().map_or_else(|| "?".into(), |name| name.to_string_lossy().into_owned());
    format!(
        "Distinct retained sources share one sidecar target: {} and {} would both save to {}. Sidecar ownership must be resolved before saving; no XMP file was changed",
        name(&binding.source),
        name(&other.source),
        name(&binding.target)
    )
}

fn validate_projected_baselines(session: &Session, plan: &Plan) -> Result<(), StoreError> {
    super::store::validate_session_bounds(session)?;
    let mut projected = session.clone();
    for saved in &plan.photos {
        if saved.intended.len() > super::store::MAX_BOUND_XMP_BYTES {
            return Err(invalid("Planned XMP exceeds recovery packet budget"));
        }
        let photo = projected.photos.iter_mut().find(|p| p.id == saved.photo).ok_or_else(|| invalid("Projected save member is missing"))?;
        photo.committed = Some(saved.recipe.clone());
        let binding = projected.sources.get_mut(&saved.photo).ok_or_else(|| invalid("Projected source binding is missing"))?;
        binding.original = Some(saved.intended.clone());
        // Reserve the maximum accepted identity length before publishing a new
        // sidecar, so its later physical identity cannot overflow the checkpoint.
        binding.target_identity = Some("x".repeat(256));
    }
    super::store::validate_session_bounds(&projected)
}

pub(super) fn fingerprint_plan(plan: &Plan) -> Result<String, StoreError> {
    #[derive(Serialize)]
    struct UnsignedPlan<'a> {
        id: &'a str,
        session: &'a str,
        epoch: u64,
        generation: u64,
        scope: &'a SaveScope,
        photos: &'a [PlannedPhoto],
    }
    let unsigned = UnsignedPlan { id: "", session: &plan.session, epoch: plan.epoch, generation: plan.generation, scope: &plan.scope, photos: &plan.photos };
    struct HashWriter {
        hasher: blake3::Hasher,
        bytes: usize,
    }
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|n| *n <= 24 * 1024 * 1024)
                .ok_or_else(|| std::io::Error::other("Plan exceeds 24 MiB encoded journal budget"))?;
            self.hasher.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = HashWriter { hasher: blake3::Hasher::new(), bytes: 0 };
    serde_json::to_writer(&mut sink, &unsigned).map_err(|e| invalid(e.to_string()))?;
    Ok(sink.hasher.finalize().to_hex().to_string())
}

/// A definite publication failure (the sidecar was never replaced) stops being an
/// obligation once that photo has no pending edit and its bound sidecar is still the one
/// the failed plan expected before writing: nothing on disk or in the session depends on
/// it any more (for example the photographer reverted the edit). Conflicts, unknown
/// outcomes and uncertain photos always remain obligations.
pub(super) fn failure_is_moot(session: &Session, planned: &PlannedPhoto, outcome: &Outcome) -> bool {
    session
        .photos
        .iter()
        .find(|photo| photo.id == planned.photo)
        .is_some_and(|photo| failure_is_moot_for(photo, session.sources.get(&planned.photo), planned, outcome))
}

/// [`failure_is_moot`] for one photo and its current binding.
pub(super) fn failure_is_moot_for(photo: &super::Photo, binding: Option<&super::SourceBinding>, planned: &PlannedPhoto, outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Failed(_))
        && photo.id == planned.photo
        && photo.pending() == Ok(false)
        && binding.is_some_and(|binding| binding.original == planned.binding.original && binding.target == planned.binding.target)
}

pub(super) fn validate_operation(operation: &Operation, id: &str, session: &str) -> Result<(), StoreError> {
    let expected: std::collections::BTreeSet<_> = operation.plan.photos.iter().map(|p| &p.photo).collect();
    let actual: std::collections::BTreeSet<_> = operation.outcomes.keys().collect();
    if operation.schema != 2
        || operation.plan.id != id
        || operation.plan.session != session
        || fingerprint_plan(&operation.plan)? != id
        || expected != actual
        || expected.len() != operation.plan.photos.len()
        || operation.plan.photos.len() > super::MAX_PHOTOS
        || matches!(&operation.plan.scope, SaveScope::Photo(id) if operation.plan.photos.iter().any(|photo| &photo.photo != id))
        || operation.uncertain_photos.iter().any(|photo| !expected.contains(photo))
        || operation.confirmed_unwritten.iter().any(|photo| !expected.contains(photo) || operation.outcomes.get(photo) != Some(&Outcome::Pending))
        || !operation.uncertain_photos.is_disjoint(&operation.confirmed_unwritten)
        || operation.uncertain_photos.iter().any(|photo| operation.outcomes.get(photo) == Some(&Outcome::Success))
    {
        return Err(invalid("Invalid operation schema, identity or outcome membership"));
    }
    for photo in &operation.plan.photos {
        photo.recipe.validate().map_err(invalid)?;
        if super::xmp::load_native(&photo.intended).map_err(invalid)?.as_ref() != Some(&photo.recipe) {
            return Err(invalid("Operation output does not reconstruct its recipe"));
        }
    }
    Ok(())
}

impl RecoveryStore {
    /// The exact PlanId is the concrete batch decision. Revalidation under the
    /// cross-process commit lock precedes every publication. Journal first, sidecar
    /// second, outcome record third; only then may the controller advance baseline.
    pub fn commit_plan(&mut self, plan: Plan, approved_id: &str) -> Result<Operation, StoreError> {
        self.commit_plan_with_progress(plan, approved_id, &mut |_, _| {})
    }

    pub fn commit_plan_with_progress(&mut self, plan: Plan, approved_id: &str, progress: &mut dyn FnMut(usize, usize)) -> Result<Operation, StoreError> {
        if approved_id != plan.id || fingerprint_plan(&plan)? != plan.id || plan.session != self.session_id() || plan.epoch != self.epoch() {
            return Err(invalid("Stale or mismatched finish approval"));
        }
        let snapshot = self.commit_snapshot(plan.generation)?;
        let pending: Vec<_> = snapshot
            .photos
            .iter()
            .filter(|p| !snapshot.removed.contains(&p.id))
            .filter(|p| !matches!(&plan.scope, SaveScope::Photo(id) if id != &p.id))
            .filter_map(|p| match p.pending() {
                Ok(true) => Some(Ok(p)),
                Ok(false) => None,
                Err(error) => Some(Err(invalid(error))),
            })
            .collect::<Result<_, _>>()?;
        if pending.len() != plan.photos.len() {
            return Err(invalid("Plan membership differs from acknowledged snapshot"));
        }
        for (photo, planned) in pending.iter().zip(&plan.photos) {
            let binding = snapshot.sources.get(&photo.id).ok_or_else(|| invalid("Acknowledged snapshot lacks source binding"))?;
            let expected = super::xmp::plan(binding.original.as_deref(), photo.committed.as_ref().ok_or_else(|| invalid("Missing baseline"))?, &photo.working)
                .map_err(invalid)?;
            // Name every differing predicate (no paths or values) so a native refusal is diagnosable.
            let differs: Vec<&str> = [
                ("photo", photo.id != planned.photo),
                ("revision", photo.revision != planned.revision),
                ("recipe", photo.working != planned.recipe),
                ("source binding", *binding != planned.binding),
                ("output", expected.as_ref() != Some(&planned.intended)),
            ]
            .into_iter()
            .filter_map(|(name, differs)| differs.then_some(name))
            .collect();
            if !differs.is_empty() {
                return Err(invalid(format!("Plan does not match acknowledged photo revision, binding and output ({})", differs.join(", "))));
            }
        }
        validate_projected_baselines(&snapshot, &plan)?;
        let _lock = self.commit_ownership()?;
        let mut coordinator = super::target_coordinator::Coordinator::acquire()?;
        self.refuse_other_uncertain_targets(&plan)?;
        for photo in &plan.photos {
            verify_write_ownership(&photo.binding)?;
        }
        let mut operation = Operation {
            schema: 2,
            outcomes: plan.photos.iter().map(|p| (p.photo.clone(), Outcome::Pending)).collect(),
            uncertain_photos: plan.photos.iter().map(|photo| photo.photo.clone()).collect(),
            confirmed_unwritten: Default::default(),
            plan,
        };
        let operation_id = operation.plan.id.clone();
        if self.operation_exists(&operation_id)? {
            return Err(invalid("Existing operation requires reconciliation, not automatic replay"));
        }
        coordinator.check(self.coordination_owner(), &operation.plan)?;
        self.write_operation(&operation_id, &operation)?;
        // Journal precedes the global claim: a crash in between cannot publish,
        // and the original owner retains a complete plan to reconcile/retry.
        #[cfg(test)]
        if std::env::var_os("PHOTOCRAFT_COMMIT_STOP_BEFORE_CLAIM").is_some() {
            std::process::exit(0);
        }
        coordinator.begin(self.coordination_owner(), &operation.plan)?;
        let total = operation.plan.photos.len();
        progress(0, total);
        for (index, photo) in operation.plan.photos.iter().enumerate() {
            let outcome = match verify(&photo.binding).and_then(|()| verify_write_ownership(&photo.binding)) {
                Err(error) => Outcome::Conflict(error.to_string()),
                Ok(()) => match self
                    .sidecar_publisher
                    .as_ref()
                    .map_or_else(|| publish_sidecar(&photo.binding.target, &photo.intended), |publish| publish(&photo.binding.target, &photo.intended))
                {
                    Ok(PublicationOutcome::Published) => Outcome::Success,
                    Ok(PublicationOutcome::PublishedUnconfirmed(error)) => Outcome::Unknown(error.to_string()),
                    Err(error) => Outcome::Failed(error.to_string()),
                },
            };
            if !matches!(outcome, Outcome::Unknown(_)) {
                operation.uncertain_photos.remove(&photo.photo);
            }
            operation.outcomes.insert(photo.photo.clone(), outcome);
            if let Err(error) = self.write_operation(&operation_id, &operation) {
                return Err(StoreError::SidecarUnknown(format!("Sidecar publication attempted but outcome acknowledgement failed: {error}")));
            }
            coordinator
                .acknowledge(self.coordination_owner(), &operation)
                .map_err(|error| StoreError::SidecarUnknown(format!("Target claim acknowledgement failed; original operation retained: {error}")))?;
            progress(index + 1, total);
        }
        Ok(operation)
    }

    /// Read-only toward sources and XMP. This writes only the private operation
    /// record; no interrupted publication is replayed or rolled back.
    pub fn reconcile_operation(&mut self, id: &str) -> Result<Operation, StoreError> {
        let _lock = self.commit_ownership()?;
        let mut coordinator = super::target_coordinator::Coordinator::acquire()?;
        let mut operation: Operation = self.read_operation(id)?;
        if operation.schema != 2 || operation.plan.id != id || fingerprint_plan(&operation.plan)? != id || operation.plan.session != self.session_id() {
            return Err(invalid("Unsupported or corrupt operation record"));
        }
        coordinator.check(self.coordination_owner(), &operation.plan)?;
        for photo in &operation.plan.photos {
            let prior = operation.outcomes.get(&photo.photo);
            // Durable acknowledged outcomes are historical facts. Rechecking a
            // successful packet after a newer reviewed save must not downgrade
            // the older journal or invalidate the baseline chain.
            if matches!(prior, Some(Outcome::Success | Outcome::Failed(_)))
                || (matches!(prior, Some(Outcome::Conflict(_))) && !operation.uncertain_photos.contains(&photo.photo))
            {
                continue;
            }
            let uncertain = operation.uncertain_photos.contains(&photo.photo)
                || matches!(prior, Some(Outcome::Unknown(_)))
                || (prior == Some(&Outcome::Pending) && !operation.confirmed_unwritten.contains(&photo.photo));
            let outcome = match sidecar_bytes(&photo.binding.target) {
                Ok(Some(actual)) if actual == photo.intended => {
                    operation.uncertain_photos.remove(&photo.photo);
                    operation.confirmed_unwritten.remove(&photo.photo);
                    Outcome::Success
                }
                Ok(actual) if actual == photo.binding.original => {
                    operation.uncertain_photos.remove(&photo.photo);
                    operation.confirmed_unwritten.insert(photo.photo.clone());
                    Outcome::Pending
                }
                Ok(_) => {
                    operation.confirmed_unwritten.remove(&photo.photo);
                    if uncertain {
                        operation.uncertain_photos.insert(photo.photo.clone());
                    }
                    Outcome::Conflict("Observed XMP differs from both expected and intended packets".into())
                }
                Err(error) => {
                    operation.confirmed_unwritten.remove(&photo.photo);
                    operation.uncertain_photos.insert(photo.photo.clone());
                    Outcome::Unknown(error.to_string())
                }
            };
            operation.outcomes.insert(photo.photo.clone(), outcome);
        }
        self.write_operation(id, &operation)?;
        coordinator.acknowledge(self.coordination_owner(), &operation)?;
        Ok(operation)
    }
}

pub(super) fn lock_commit(path: &Path) -> Result<File, StoreError> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => StoreError::Owned,
        std::fs::TryLockError::Error(error) => StoreError::Io(error),
    })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_session::{Admission, Photo};
    use std::collections::BTreeSet;
    fn fixture() -> (PathBuf, RecoveryStore, Session, BTreeMap<PhotoId, Binding>) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!("pc-commit-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(root.join("inputs")).unwrap();
        let source = root.join("inputs/photo.NEF");
        std::fs::write(&source, b"synthetic source identity, not a decodable NEF").unwrap();
        let mut store = RecoveryStore::claim(&root.join("recovery"), "session").unwrap();
        let id = PhotoId("photo".into());
        let bindings = BTreeMap::from([(id.clone(), bind(&source).unwrap())]);
        let baseline = crate::raw_session::tests::recipe(0.);
        let working = crate::raw_session::tests::recipe(0.7853281);
        let mut session = Session {
            schema: 1,
            id: "session".into(),
            epoch: store.epoch(),
            generation: 1,
            cached_generation: None,
            phase: Phase::Frozen,
            photos: vec![Photo {
                id: id.clone(),
                working: working.clone(),
                committed: Some(baseline),
                entry: working,
                revision: 1,
                hidden: true,
                selected: false,
                committed_this_visit: false,
                unknown_operation: false,
            }],
            requested: BTreeMap::from([("source".into(), Admission::Ready(id))]),
            sources: bindings.clone(),
            removed: BTreeSet::new(),
            view_context: serde_json::Value::Null,
        };
        let ack = store.checkpoint(&session).unwrap();
        assert!(session.acknowledge_cache(&ack.session, ack.epoch, ack.generation));
        (root, store, session, bindings)
    }
    #[test]
    fn staging_disk_full_preserves_old_sidecar_and_records_unwritten_failure() {
        publication_fault_case("disk-full");
    }

    #[test]
    fn staging_file_sync_failure_preserves_old_sidecar_and_records_unwritten_failure() {
        publication_fault_case("file-sync");
    }

    #[test]
    fn directory_sync_failure_records_uncertainty_then_reconciles_without_replay() {
        publication_fault_case("directory-sync");
    }

    fn publication_fault_case(boundary: &'static str) {
        use photocraft_format::atomic::{AtomicFs, RealFs, RenameRetry, atomic_write_outcome_with};
        struct FaultFs(&'static str);
        impl AtomicFs for FaultFs {
            fn write_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
                use std::io::Write;
                let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
                if self.0 == "disk-full" {
                    file.write_all(&bytes[..bytes.len() / 2])?;
                    // Inject ENOSPC/ERROR_DISK_FULL after a real partial staging
                    // write. Never consume the user's free disk space.
                    return Err(std::io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 }));
                }
                file.write_all(bytes)?;
                if self.0 == "file-sync" {
                    // Complete staging write, then fail exactly where sync_all
                    // would acknowledge durability, before replacement.
                    return Err(std::io::Error::other("injected staging file sync failure"));
                }
                file.sync_all()
            }
            fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
                RealFs.rename(from, to)
            }
            fn remove(&self, path: &Path) -> std::io::Result<()> {
                RealFs.remove(path)
            }
            fn copy_metadata(&self, from: &Path, to: &Path) -> std::io::Result<()> {
                RealFs.copy_metadata(from, to)
            }
            fn sync_dir(&self, _: &Path) -> std::io::Result<()> {
                Err(std::io::Error::other("injected post-replacement directory sync failure"))
            }
        }
        let (root, mut store, mut session, _) = fixture();
        let source = root.join("inputs/photo.NEF");
        let target = root.join("inputs/photo.xmp");
        let source_before = std::fs::read(&source).unwrap();
        let old = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about=''/></rdf:RDF></x:xmpmeta>";
        std::fs::write(&target, old).unwrap();
        let photo = PhotoId("photo".into());
        let bindings = BTreeMap::from([(photo.clone(), bind(&source).unwrap())]);
        session.sources = bindings.clone();
        session.generation += 1;
        let ack = store.checkpoint(&session).unwrap();
        assert!(session.acknowledge_cache(&ack.session, ack.epoch, ack.generation));
        let plan = prepare(&session, &bindings).unwrap();
        let intended = plan.photos[0].intended.clone();
        let id = plan.id.clone();
        store.sidecar_publisher = Some(std::sync::Arc::new(move |path, bytes| {
            atomic_write_outcome_with(&FaultFs(boundary), RenameRetry { attempts: 0, first_delay_ms: 0 }, path, bytes, &|_| {})
        }));
        let operation = store.commit_plan(plan, &id).unwrap();
        if boundary == "directory-sync" {
            assert!(matches!(operation.outcomes[&photo], Outcome::Unknown(_)));
            assert!(operation.uncertain_photos.contains(&photo));
            assert_eq!(std::fs::read(&target).unwrap(), intended);
        } else {
            assert!(matches!(operation.outcomes[&photo], Outcome::Failed(_)));
            assert!(!operation.uncertain_photos.contains(&photo));
            assert_eq!(std::fs::read(&target).unwrap(), old);
        }
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
        assert_eq!(std::fs::read_dir(root.join("inputs")).unwrap().count(), 2, "partial staging file removed");
        drop(store);
        let bytes_before_reconcile = std::fs::read(&target).unwrap();
        let mut resumed = RecoveryStore::claim(&root.join("recovery"), "session").unwrap();
        // Any accidental replay is an immediate failure, including one whose bytes match.
        resumed.sidecar_publisher = Some(std::sync::Arc::new(|_, _| panic!("reconciliation must not publish")));
        let reconciled = resumed.reconcile_operation(&id).unwrap();
        if boundary == "directory-sync" {
            assert_eq!(reconciled.outcomes[&photo], Outcome::Success);
            assert!(!reconciled.uncertain_photos.contains(&photo));
        } else {
            assert!(matches!(reconciled.outcomes[&photo], Outcome::Failed(_)));
        }
        assert_eq!(std::fs::read(&target).unwrap(), bytes_before_reconcile);
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
        drop(resumed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn another_process_uncertain_publication_refuses_a_second_writer() {
        cross_root_process_case(false);
    }

    #[test]
    fn abrupt_process_exit_after_publication_blocks_other_recovery_root() {
        cross_root_process_case(true);
    }

    fn cross_root_process_case(crash: bool) {
        const CHILD_ROOT: &str = "PHOTOCRAFT_SAFETY_CHILD_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let mut first = RecoveryStore::claim(&root.join("recovery"), "session").unwrap();
            let mut session = first.load().unwrap().unwrap().session;
            session.epoch = first.epoch();
            session.generation += 1;
            first.checkpoint(&session).unwrap();
            session.cached_generation = Some(session.generation);
            let plan = prepare(&session, &session.sources).unwrap();
            let id = plan.id.clone();
            first.sidecar_publisher = Some(std::sync::Arc::new(move |path, bytes| {
                photocraft_format::atomic::atomic_write_outcome(path, bytes)?;
                if crash {
                    std::process::exit(0);
                }
                Ok(PublicationOutcome::PublishedUnconfirmed(std::io::Error::other("injected lost acknowledgement")))
            }));
            let result = first.commit_plan(plan, &id).unwrap();
            assert!(matches!(result.outcomes.get(&PhotoId("photo".into())), Some(Outcome::Unknown(_))));
            return;
        }
        let (root, first, mut snapshot, _) = fixture();
        drop(first);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                if crash {
                    "raw_session::commit::tests::abrupt_process_exit_after_publication_blocks_other_recovery_root"
                } else {
                    "raw_session::commit::tests::another_process_uncertain_publication_refuses_a_second_writer"
                },
                "--nocapture",
            ])
            .env(CHILD_ROOT, &root)
            .env("PHOTOCRAFT_TARGET_COORDINATION_ROOT", super::super::target_coordinator::root().unwrap())
            .status()
            .unwrap();
        assert!(status.success());
        let source = root.join("inputs/photo.NEF");
        let original_source = std::fs::read(&source).unwrap();
        let original_packet = std::fs::read(source.with_extension("xmp")).unwrap();
        let mut second = RecoveryStore::claim(&root.join("independent-recovery"), "second").unwrap();
        snapshot.id = "second".into();
        snapshot.epoch = second.epoch();
        snapshot.sources = BTreeMap::from([(PhotoId("photo".into()), bind(&source).unwrap())]);
        snapshot.photos[0].committed = Some(snapshot.photos[0].working.clone());
        snapshot.photos[0].working = crate::raw_session::tests::recipe(1.25);
        second.checkpoint(&snapshot).unwrap();
        let plan = prepare(&snapshot, &snapshot.sources).unwrap();
        let id = plan.id.clone();
        let result = second.commit_plan(plan, &id);
        assert!(result.is_err(), "a second process must not overwrite an unconfirmed publication: {result:?}");
        assert!(result.unwrap_err().to_string().contains("unresolved target claim"), "refusal identifies the cross-root protecting claim");
        assert_eq!(std::fs::read(&source).unwrap(), original_source);
        assert_eq!(std::fs::read(source.with_extension("xmp")).unwrap(), original_packet);
        assert!(second.operation_ids().is_empty(), "refusal precedes the second journal");
        // Explicit reconciliation establishes the first result; only then may the second save proceed.
        let mut first = RecoveryStore::claim(&root.join("recovery"), "session").unwrap();
        for id in first.operation_ids() {
            first.reconcile_operation(&id).unwrap();
        }
        let plan = prepare(&snapshot, &snapshot.sources).unwrap();
        let id = plan.id.clone();
        assert_eq!(second.commit_plan(plan, &id).unwrap().outcomes.get(&PhotoId("photo".into())), Some(&Outcome::Success));
        drop(first);
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_name_alias_does_not_authorize_replacing_a_rendered_stem_packet() {
        let (root, store, _, _) = fixture();
        let source = root.join("inputs/rendered.jpg");
        std::fs::write(&source, b"source identity").unwrap();
        let stem = source.with_extension("xmp");
        std::fs::write(&stem, b"external metadata").unwrap();
        std::fs::hard_link(&stem, root.join("inputs/rendered.jpg.xmp")).unwrap();
        let error = bind(&source).unwrap_err().to_string();
        assert!(error.contains("ownership"), "{error}");
        assert_eq!(std::fs::read(&stem).unwrap(), b"external metadata");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn abrupt_exit_after_private_journal_before_global_claim_recovers_and_saves() {
        const CHILD: &str = "PHOTOCRAFT_PRECLAIM_CHILD_ROOT";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let mut store = RecoveryStore::claim(&root.join("recovery"), "session").unwrap();
            let mut session = store.load().unwrap().unwrap().session;
            session.epoch = store.epoch();
            session.generation += 1;
            store.checkpoint(&session).unwrap();
            session.cached_generation = Some(session.generation);
            let plan = prepare(&session, &session.sources).unwrap();
            let id = plan.id.clone();
            let result = store.commit_plan(plan, &id);
            assert!(result.is_err(), "requested child exit must precede publication");
            return;
        }
        let (root, store, _, _) = fixture();
        drop(store);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "raw_session::commit::tests::abrupt_exit_after_private_journal_before_global_claim_recovers_and_saves", "--nocapture"])
            .env(CHILD, &root)
            .env("PHOTOCRAFT_COMMIT_STOP_BEFORE_CLAIM", "1")
            .env("PHOTOCRAFT_TARGET_COORDINATION_ROOT", super::super::target_coordinator::root().unwrap())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!root.join("inputs/photo.xmp").exists());
        let source_before = std::fs::read(root.join("inputs/photo.NEF")).unwrap();
        let (mut controller, _) = super::super::controller::Controller::resume(&root.join("recovery"), "session").unwrap();
        assert!(!controller.unresolved_publication());
        let plan = controller.prepare_done().unwrap();
        let id = plan.id.clone();
        assert!(controller.commit_done(plan, &id).unwrap().completed);
        assert!(root.join("inputs/photo.xmp").exists());
        assert_eq!(std::fs::read(root.join("inputs/photo.NEF")).unwrap(), source_before);
        drop(controller);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rendered_unproven_stem_refuses_discovery_admission_and_pending_save() {
        use super::super::controller::Controller;
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        let image = Image::from_u8(4, 3, ChannelLayout::Rgb, [80, 100, 120].repeat(12)).unwrap();
        let bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        let baseline = crate::camera_raw_source::initial_recipe(&bytes, None).unwrap();
        let mut edited = baseline.clone();
        edited.adjustments.exposure = 0.75;
        let native = super::super::xmp::plan(None, &baseline, &edited).unwrap().unwrap();
        let foreign = br#"<r:RDF xmlns:r="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:c="http://ns.adobe.com/camera-raw-settings/1.0/"><r:Description c:Exposure2012="2.0"/></r:RDF>"#.to_vec();
        for packet in [&foreign, &native] {
            let (root, store, _, _) = fixture();
            let source = root.join("inputs/rendered.jpg");
            let stem = root.join("inputs/rendered.xmp");
            let full = root.join("inputs/rendered.jpg.xmp");
            std::fs::write(&source, &bytes).unwrap();
            let mut controller = Controller::create(&root.join("other-recovery")).unwrap();
            controller.request("rendered".into()).unwrap();
            let id = controller.admit("rendered", &source, &bytes).unwrap().id;
            controller.apply(vec![(id, edited.clone())]).unwrap();
            std::fs::write(&stem, packet).unwrap();
            assert!(sidecar_target(&source).unwrap_err().to_string().contains("ownership"));
            assert!(bind(&source).is_err());
            assert!(controller.prepare_done().is_err());
            assert_eq!(controller.session().photos[0].working, edited, "refusal keeps pending edits");
            let mut reopen = Controller::create(&root.join("reopen-recovery")).unwrap();
            reopen.request("rendered".into()).unwrap();
            assert!(reopen.admit("rendered", &source, &bytes).is_err());
            assert!(reopen.session().photos.is_empty(), "unproven metadata is never admitted as this photo's recipe");
            assert!(!full.exists(), "no silent alternative sidecar");
            assert_eq!(std::fs::read(&stem).unwrap(), *packet);
            assert_eq!(std::fs::read(&source).unwrap(), bytes);
            drop(reopen);
            drop(controller);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn all_approved_plan_predicates_have_independent_negative_controls() {
        for field in ["session", "epoch", "generation", "photo", "revision", "recipe", "binding", "intended"] {
            let (root, mut store, session, bindings) = fixture();
            let mut plan = prepare(&session, &bindings).unwrap();
            match field {
                "session" => plan.session.push_str("-other"),
                "epoch" => plan.epoch += 1,
                "generation" => plan.generation += 1,
                "photo" => plan.photos[0].photo.0.push_str("-other"),
                "revision" => plan.photos[0].revision += 1,
                "recipe" => plan.photos[0].recipe = crate::raw_session::tests::recipe(1.75),
                "binding" => plan.photos[0].binding.source_fingerprint.push('0'),
                _ => {
                    assert_eq!(field, "intended");
                    plan.photos[0].intended.push(b' ');
                }
            }
            // Rehash the corrupt plan so this tests the actual predicate, not only
            // the outer integrity/approval mismatch gate.
            plan.id = fingerprint_plan(&plan).unwrap();
            let id = plan.id.clone();
            assert!(store.commit_plan(plan, &id).is_err(), "{field} must independently refuse");
            assert!(store.operation_ids().is_empty(), "{field}: no journal");
            assert!(!root.join("inputs/photo.xmp").exists(), "{field}: no sidecar");
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    fn authority_fault_refuses(fault: &str) {
        let (root, mut store, mut session, bindings) = fixture();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        let mut other = RecoveryStore::claim(&root.join("recovery"), "other").unwrap();
        session.id = "other".into();
        session.epoch = other.epoch();
        other.checkpoint(&session).unwrap();
        let other_plan = prepare(&session, &bindings).unwrap();
        let other_id = other_plan.id.clone();
        let operation = Operation {
            schema: 2,
            outcomes: other_plan.photos.iter().map(|p| (p.photo.clone(), Outcome::Pending)).collect(),
            uncertain_photos: other_plan.photos.iter().map(|p| p.photo.clone()).collect(),
            confirmed_unwritten: Default::default(),
            plan: other_plan,
        };
        other.write_operation(&other_id, &operation).unwrap();
        drop(other);
        let authority = root.join("recovery/other/authority.json");
        let operation_path = root.join("recovery/other").join(format!("operation-{other_id}.json"));
        let saved_authority = std::fs::read(&authority).unwrap();
        let saved_operation = std::fs::read(&operation_path).unwrap();
        let source_before = std::fs::read(root.join("inputs/photo.NEF")).unwrap();
        #[cfg(windows)]
        let mut held: Option<File> = None;
        #[cfg(not(windows))]
        let held: Option<File> = None;
        match fault {
            "malformed" => std::fs::write(&authority, b"{").unwrap(),
            "missing" => std::fs::remove_file(&authority).unwrap(),
            "missing-operation" => std::fs::remove_file(&operation_path).unwrap(),
            "orphan" => {
                let bytes = super::super::store::read_record(&authority).unwrap();
                let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                value["operations"] = serde_json::json!([]);
                super::super::store::write_record(&authority, &serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => {
                assert_eq!(fault, "unreadable");
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    held = Some(OpenOptions::new().read(true).share_mode(0).open(&authority).unwrap());
                }
                #[cfg(not(windows))]
                {
                    std::fs::remove_file(&authority).unwrap();
                    std::fs::create_dir(&authority).unwrap();
                }
            }
        }
        assert!(store.commit_plan(plan.clone(), &id).is_err(), "{fault}: fail closed");
        assert!(store.operation_ids().is_empty(), "{fault}: no new journal");
        assert!(!root.join("inputs/photo.xmp").exists(), "{fault}: no sidecar");
        drop(held);
        // Deliberate, trustworthy repair in this test: restore exactly the bytes
        // preserved before fault injection, then have the original owner reconcile.
        #[cfg(not(windows))]
        if fault == "unreadable" {
            std::fs::remove_dir(&authority).unwrap();
        }
        std::fs::write(&authority, saved_authority).unwrap();
        std::fs::write(&operation_path, saved_operation).unwrap();
        let mut other = RecoveryStore::claim(&root.join("recovery"), "other").unwrap();
        let reconciled = other.reconcile_operation(&other_id).unwrap();
        assert!(reconciled.confirmed_unwritten.contains(&PhotoId("photo".into())));
        assert_eq!(store.commit_plan(plan, &id).unwrap().outcomes.get(&PhotoId("photo".into())), Some(&Outcome::Success));
        assert_eq!(std::fs::read(root.join("inputs/photo.NEF")).unwrap(), source_before);
        drop(other);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_competing_authority_refuses_publication() {
        authority_fault_refuses("malformed");
    }
    #[test]
    fn missing_competing_authority_with_residue_refuses_publication() {
        authority_fault_refuses("missing");
    }
    #[test]
    fn unreadable_competing_authority_refuses_publication() {
        authority_fault_refuses("unreadable");
    }
    #[test]
    fn missing_competing_operation_refuses_publication() {
        authority_fault_refuses("missing-operation");
    }
    #[test]
    fn orphan_pending_operation_refuses_publication() {
        authority_fault_refuses("orphan");
    }

    #[test]
    fn a_same_stem_sensor_arriving_after_approval_refuses_before_publication() {
        let (root, mut store, session, bindings) = fixture();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        let source = root.join("inputs/photo.NEF");
        let original = std::fs::read(&source).unwrap();
        std::fs::write(root.join("inputs/photo.ARW"), &original).unwrap();
        let error = store.commit_plan(plan, &id).unwrap_err().to_string();
        assert!(error.contains("ownership") && error.contains("photo.ARW"), "{error}");
        assert!(store.operation_ids().is_empty());
        assert!(!source.with_extension("xmp").exists());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_native_commit_journals_and_reconstructs_without_cache() {
        let (root, mut store, session, bindings) = fixture();
        let source_before = std::fs::read(root.join("inputs/photo.NEF")).unwrap();
        let plan = prepare(&session, &bindings).unwrap();
        assert_eq!(plan.photos.len(), 1);
        assert!(plan.photos[0].hidden);
        let id = plan.id.clone();
        let outcome = store.commit_plan(plan, &id).unwrap();
        assert_eq!(outcome.outcomes.get(&PhotoId("photo".into())), Some(&Outcome::Success));
        let xmp = std::fs::read(root.join("inputs/photo.xmp")).unwrap();
        assert_eq!(super::super::xmp::load_native(&xmp).unwrap(), Some(session.photos[0].working.clone()));
        assert_eq!(std::fs::read(root.join("inputs/photo.NEF")).unwrap(), source_before);
        let reconciled = store.reconcile_operation(&id).unwrap();
        assert_eq!(reconciled.outcomes, outcome.outcomes);
        assert_eq!(std::fs::read(root.join("inputs/photo.xmp")).unwrap(), xmp);
        store.complete().unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn external_create_after_approval_is_conflict_and_keeps_source_and_packet() {
        let (root, mut store, session, bindings) = fixture();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        let external = b"external packet created after consent";
        std::fs::write(root.join("inputs/photo.xmp"), external).unwrap();
        let outcome = store.commit_plan(plan, &id).unwrap();
        assert!(matches!(outcome.outcomes.get(&PhotoId("photo".into())), Some(Outcome::Conflict(_))));
        assert_eq!(std::fs::read(root.join("inputs/photo.xmp")).unwrap(), external);
        assert!(store.complete().is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn distinct_full_filename_and_stem_sidecars_are_ambiguous_even_with_equal_bytes() {
        let (root, store, _, _) = fixture();
        std::fs::write(root.join("inputs/photo.xmp"), b"same").unwrap();
        std::fs::write(root.join("inputs/photo.NEF.xmp"), b"same").unwrap();
        assert!(bind(&root.join("inputs/photo.NEF")).is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn interrupted_operation_reconciles_without_replay() {
        let (root, mut store, session, bindings) = fixture();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        store.pin(plan.generation).unwrap();
        let operation = Operation {
            schema: 2,
            outcomes: plan.photos.iter().map(|p| (p.photo.clone(), Outcome::Pending)).collect(),
            plan,
            uncertain_photos: Default::default(),
            confirmed_unwritten: Default::default(),
        };
        store.write_operation(&id, &operation).unwrap();
        drop(store);
        let mut resumed = RecoveryStore::claim(&root.join("recovery"), "session").unwrap();
        let result = resumed.reconcile_operation(&id).unwrap();
        assert_eq!(result.outcomes.get(&PhotoId("photo".into())), Some(&Outcome::Pending));
        assert!(!root.join("inputs/photo.xmp").exists());
        assert!(resumed.commit_plan(result.plan, &id).is_err());
        drop(resumed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removed_photo_and_superseded_snapshot_invalidate_approval() {
        let (root, mut store, mut session, bindings) = fixture();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        session.generation += 1;
        store.checkpoint(&session).unwrap();
        assert!(store.commit_plan(plan, &id).is_err());
        session.cached_generation = Some(session.generation);
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        store.record_removal(PhotoId("photo".into())).unwrap();
        assert!(store.commit_plan(plan, &id).is_err());
        assert!(!root.join("inputs/photo.xmp").exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn byte_identical_replaced_source_does_not_inherit_binding() {
        let (root, store, _, bindings) = fixture();
        let source = root.join("inputs/photo.NEF");
        let bytes = std::fs::read(&source).unwrap();
        std::fs::rename(&source, root.join("inputs/preserved.NEF")).unwrap();
        std::fs::write(&source, bytes).unwrap();
        assert!(verify(bindings.get(&PhotoId("photo".into())).unwrap()).is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_outcomes_cannot_complete_an_operation() {
        let (root, mut store, session, bindings) = fixture();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        let operation = Operation { schema: 2, plan, outcomes: BTreeMap::new(), uncertain_photos: Default::default(), confirmed_unwritten: Default::default() };
        store.write_operation(&id, &operation).unwrap();
        assert!(store.complete().is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkpoint_cannot_bypass_uncertain_removal_guard() {
        let (root, mut store, mut session, bindings) = fixture();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        let photo = PhotoId("photo".into());
        let operation = Operation {
            schema: 2,
            plan,
            outcomes: BTreeMap::from([(photo.clone(), Outcome::Unknown("lost response".into()))]),
            uncertain_photos: Default::default(),
            confirmed_unwritten: Default::default(),
        };
        store.write_operation(&id, &operation).unwrap();
        assert!(store.record_removal(photo.clone()).is_err());
        session.photos.clear();
        session.removed.insert(photo);
        session.generation += 1;
        assert!(store.checkpoint(&session).is_err());
        assert!(store.complete().is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_old_binding_cannot_complete_rebound_photo() {
        let (root, mut store, mut session, bindings) = fixture();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        store.commit_plan(plan, &id).unwrap();
        std::fs::write(root.join("inputs/other.NEF"), b"different source").unwrap();
        session.sources.insert(PhotoId("photo".into()), bind(&root.join("inputs/other.NEF")).unwrap());
        session.generation += 1;
        store.checkpoint(&session).unwrap();
        assert!(store.complete().is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_edits_cannot_complete_without_operation_but_explicit_discard_can() {
        let (root, mut store, session, bindings) = fixture();
        assert!(store.complete().is_err());
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        std::fs::write(root.join("inputs/photo.xmp"), b"external conflict").unwrap();
        let operation = store.commit_plan(plan, &id).unwrap();
        assert!(matches!(operation.outcomes.get(&PhotoId("photo".into())), Some(Outcome::Conflict(_))));
        store.record_removal(PhotoId("photo".into())).unwrap();
        store.complete().unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_reviewed_retry_supersedes_conflict_without_reactivating_it() {
        let (root, mut store, mut session, bindings) = fixture();
        let old_plan = prepare(&session, &bindings).unwrap();
        let old_id = old_plan.id.clone();
        let external = format!("<rdf:RDF xmlns:rdf=\"{}\" xmlns:u=\"urn:unrelated\"><rdf:Description u:Keep=\"yes\"/></rdf:RDF>", photocraft_xmp::RDF_NS);
        std::fs::write(root.join("inputs/photo.xmp"), external).unwrap();
        store.commit_plan(old_plan, &old_id).unwrap();
        session.generation = 2;
        session.sources.insert(PhotoId("photo".into()), bind(&root.join("inputs/photo.NEF")).unwrap());
        let ack = store.checkpoint(&session).unwrap();
        assert!(session.acknowledge_cache(&ack.session, ack.epoch, ack.generation));
        let new_plan = prepare(&session, &session.sources).unwrap();
        let new_id = new_plan.id.clone();
        store.commit_plan(new_plan, &new_id).unwrap();
        assert!(store.complete().is_err(), "historical conflict needs explicit durable resolution");
        store.supersede_operation(&old_id, &new_id, &session).unwrap();
        assert_eq!(store.operation_ids(), vec![new_id]);
        assert!(store.reconcile_operation(&old_id).is_err());
        store.complete().unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Synthetic camera-profile numbers with the 16–17 significant digits a fitted RAW
    /// colour matrix and tone curve carry. Deterministic; not taken from any photograph.
    fn full_precision_profile() -> ([[f64; 3]; 3], Vec<[f64; 2]>) {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut matrix = [[0.0; 3]; 3];
        for (row, line) in matrix.iter_mut().enumerate() {
            for (column, value) in line.iter_mut().enumerate() {
                let jitter = (next() - 0.5) * 0.4;
                *value = if row == column { 0.85 + jitter } else { jitter * 0.5 };
            }
        }
        // Linear-light knots as `ToneCurve::knots` returns them (squares of stored roots).
        let mut curve = vec![[0.0, 0.0]];
        let (mut x, mut y) = (0.0f64, 0.0f64);
        for _ in 0..30 {
            x += 0.005 + next() * 0.025;
            y += 0.005 + next() * 0.025;
            curve.push([x.sqrt() * x.sqrt(), y.sqrt() * y.sqrt()]);
        }
        curve.push([1.0, 1.0]);
        (matrix, curve)
    }

    /// Candidate F, native Done failure (Sony ARW, recovery on): the plan is built from the
    /// in-memory session and checked against the same session read back from the
    /// acknowledged checkpoint. Both must hold the identical recipe, so the checkpoint
    /// encoding must reproduce every number exactly, including a fitted camera profile.
    #[test]
    fn full_precision_camera_profile_survives_the_acknowledged_checkpoint_and_done() {
        let (root, mut store, mut session, bindings) = fixture();
        let (matrix, curve) = full_precision_profile();
        let photo = &mut session.photos[0];
        for recipe in [&mut photo.working, &mut photo.entry].into_iter().chain(photo.committed.as_mut()) {
            recipe.processing.camera_matrix = Some(matrix);
            recipe.processing.camera_tone_curve = curve.clone();
        }
        photo.working.adjustments.exposure = 0.07407427;
        photo.working.validate().unwrap();
        session.generation += 1;
        let ack = store.checkpoint(&session).unwrap();
        assert!(session.acknowledge_cache(&ack.session, ack.epoch, ack.generation));
        let restored = store.commit_snapshot(session.generation).unwrap();
        let plan = prepare(&session, &bindings).unwrap();
        let id = plan.id.clone();
        let outcome = store.commit_plan(plan, &id).unwrap_or_else(|error| panic!("Done refused its own acknowledged checkpoint: {error}"));
        assert_eq!(outcome.outcomes.get(&PhotoId("photo".into())), Some(&Outcome::Success));
        assert!(restored.photos == session.photos, "the acknowledged checkpoint reproduces the live recipes exactly");
        let xmp = std::fs::read(root.join("inputs/photo.xmp")).unwrap();
        assert_eq!(super::super::xmp::load_native(&xmp).unwrap(), Some(session.photos[0].working.clone()), "a fresh reader reconstructs the exact recipe");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Root cause of the native failure: every value a recipe can hold must survive a JSON
    /// round trip bit for bit (checkpoints, journals and the native XMP payload are JSON).
    #[test]
    fn recipe_numbers_survive_a_json_round_trip_bit_for_bit() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut lost = Vec::new();
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let wide = (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            let narrow = wide as f32 * 5.0;
            if serde_json::from_str::<f64>(&serde_json::to_string(&wide).unwrap()).unwrap().to_bits() != wide.to_bits() {
                lost.push(format!("f64 {wide:?}"));
            }
            if serde_json::from_str::<f32>(&serde_json::to_string(&narrow).unwrap()).unwrap().to_bits() != narrow.to_bits() {
                lost.push(format!("f32 {narrow:?}"));
            }
        }
        assert!(lost.is_empty(), "{} of 40000 values changed in a JSON round trip, e.g. {:?}", lost.len(), lost.iter().take(3).collect::<Vec<_>>());
    }

    /// Diagnostic harness for the captured native failure. Reads a copied acknowledged
    /// checkpoint (`PHOTOCRAFT_F_CAPTURED_GENERATION`) and a disposable copy of the Sony
    /// source and its sidecar (`PHOTOCRAFT_F_DISPOSABLE_SOURCE`). It prints only field names,
    /// counts and digests: no paths, file bytes or embedded metadata. It never touches the
    /// captured originals; the publication goes to the disposable sidecar copy.
    #[test]
    #[ignore = "needs the captured native checkpoint and a disposable source copy"]
    fn captured_native_done_failure_diagnosis() {
        let generation = std::env::var_os("PHOTOCRAFT_F_CAPTURED_GENERATION").expect("captured generation path");
        let source = PathBuf::from(std::env::var_os("PHOTOCRAFT_F_DISPOSABLE_SOURCE").expect("disposable source path"));
        let envelope: serde_json::Value = serde_json::from_slice(&std::fs::read(generation).unwrap()).unwrap();
        let payload = envelope["payload"].as_str().unwrap();
        let captured = super::super::store::validate_snapshot(payload.as_bytes()).unwrap();
        let extension = source.extension().and_then(|e| e.to_str()).unwrap().to_ascii_lowercase();
        let (index, member) = captured
            .photos
            .iter()
            .enumerate()
            .find(|(_, p)| {
                captured.sources.get(&p.id).and_then(|b| b.source.extension()).and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref()
                    == Some(extension.as_str())
                    && p.pending().unwrap()
            })
            .expect("a pending captured photo of the disposable source's kind");
        let digest = blake3::hash(member.id.0.as_bytes()).to_hex().chars().take(12).collect::<String>();
        println!("captured member #{index} (id digest {digest}), pending");
        // 1. Which number literals in the captured text a JSON reader does not reproduce:
        //    shortest round-trip text is unique, so a changed literal proves the live value
        //    differed from the value read back from the acknowledged checkpoint.
        let mut changed = std::collections::BTreeMap::<String, usize>::new();
        for (name, part) in [("working", &member.working), ("committed", member.committed.as_ref().unwrap())] {
            let original = literal_numbers(&exact_member_text(payload, index, name));
            let reread = literal_numbers(&serde_json::to_string(part).unwrap());
            assert_eq!(original.len(), reread.len(), "{name} literal count");
            for ((field, a), (_, b)) in original.iter().zip(&reread) {
                if a != b {
                    *changed.entry(format!("{name}.{field}")).or_default() += 1;
                }
            }
        }
        println!("literals not reproduced by the JSON reader: {changed:?}");
        // 2. Reconstruct the live recipes exactly (str::parse is correctly rounded) and replay
        //    checkpoint -> prepare -> commit against the disposable source copy.
        let exact = |name: &str| exact_recipe(payload, index, name, if name == "working" { &member.working } else { member.committed.as_ref().unwrap() });
        let working = exact("working");
        let committed = exact("committed");
        let root = source.parent().unwrap().parent().unwrap().join(format!("diag-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut store = RecoveryStore::claim(&root.join("recovery"), "diagnosis").unwrap();
        let id = PhotoId("captured".into());
        let bindings = BTreeMap::from([(id.clone(), bind(&source).unwrap())]);
        let mut session = Session {
            schema: 1,
            id: "diagnosis".into(),
            epoch: store.epoch(),
            generation: 1,
            cached_generation: None,
            phase: Phase::Frozen,
            photos: vec![Photo {
                id: id.clone(),
                working: working.clone(),
                committed: Some(committed),
                entry: working.clone(),
                revision: member.revision,
                hidden: false,
                selected: true,
                committed_this_visit: false,
                unknown_operation: false,
            }],
            requested: BTreeMap::from([("captured".into(), Admission::Ready(id.clone()))]),
            sources: bindings.clone(),
            removed: BTreeSet::new(),
            view_context: serde_json::Value::Null,
        };
        let ack = store.checkpoint(&session).unwrap();
        assert!(session.acknowledge_cache(&ack.session, ack.epoch, ack.generation));
        let plan = prepare(&session, &bindings).unwrap();
        let snapshot = store.commit_snapshot(plan.generation).unwrap();
        let (photo, planned) = (&snapshot.photos[0], &plan.photos[0]);
        let binding = snapshot.sources.get(&photo.id).unwrap();
        let expected = super::super::xmp::plan(binding.original.as_deref(), photo.committed.as_ref().unwrap(), &photo.working).unwrap();
        let predicates = [
            ("photo id", photo.id == planned.photo),
            ("revision", photo.revision == planned.revision),
            ("working recipe", photo.working == planned.recipe),
            ("source binding", *binding == planned.binding),
            ("regenerated XMP output", expected.as_ref() == Some(&planned.intended)),
        ];
        for (name, holds) in predicates {
            println!("predicate {name}: {}", if holds { "matches" } else { "DIFFERS" });
            assert!(holds, "captured Sony checkpoint mismatch: {name}");
        }
        let id_text = plan.id.clone();
        let sidecar_before = std::fs::read(&bindings[&id].target).ok();
        let source_before = blake3::hash(&std::fs::read(&source).unwrap());
        match store.commit_plan(plan, &id_text) {
            Ok(operation) => {
                println!("commit outcome: {:?}", operation.outcomes.get(&id).map(|o| matches!(o, Outcome::Success)));
                assert!(matches!(operation.outcomes.get(&id), Some(Outcome::Success)), "Sony publication must be acknowledged");
                let saved = std::fs::read(&bindings[&id].target).unwrap();
                println!(
                    "sidecar changed: {}; reopened recipe exact: {}",
                    Some(&saved) != sidecar_before.as_ref(),
                    super::super::xmp::load_native(&saved).unwrap().as_ref() == Some(&working)
                );
                assert_ne!(Some(&saved), sidecar_before.as_ref(), "the intended Sony edit must reach XMP");
                assert_eq!(
                    super::super::xmp::load_native(&saved).unwrap().as_ref(),
                    Some(&working),
                    "fresh XMP reconstruction must match every captured native field"
                );
            }
            Err(error) => panic!("captured Sony commit refused: {error}"),
        }
        assert_eq!(blake3::hash(&std::fs::read(&source).unwrap()), source_before, "the source photograph is never written");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The exact JSON text of one recipe member of `photos[index]` in the captured payload.
    fn exact_member_text(payload: &str, index: usize, name: &str) -> String {
        #[derive(serde::Deserialize)]
        struct Wire<'a> {
            #[serde(borrow)]
            photos: Vec<std::collections::BTreeMap<String, &'a serde_json::value::RawValue>>,
        }
        let wire: Wire<'_> = serde_json::from_str(payload).unwrap();
        wire.photos[index][name].get().to_owned()
    }

    /// Number literals in document order, keyed by the nearest enclosing field name.
    fn literal_numbers(text: &str) -> Vec<(String, String)> {
        let (mut out, mut key, mut chars) = (Vec::new(), String::new(), text.char_indices().peekable());
        while let Some((start, c)) = chars.next() {
            if c == '"' {
                let mut s = String::new();
                while let Some((_, c)) = chars.next() {
                    if c == '\\' {
                        chars.next();
                    } else if c == '"' {
                        break;
                    } else {
                        s.push(c);
                    }
                }
                if chars.peek().is_some_and(|(_, c)| *c == ':') {
                    key = s;
                }
            } else if c == '-' || c.is_ascii_digit() {
                let mut end = start + c.len_utf8();
                while let Some((i, c)) = chars.peek().copied() {
                    if c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-') {
                        end = i + c.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push((key.clone(), text[start..end].to_owned()));
            }
        }
        out
    }

    /// Rebuild a captured recipe's 64-bit profile numbers with correctly rounded parsing
    /// (`str::parse`): the exact values the live process held when it wrote this shortest
    /// round-trip text. 32-bit adjustment text has at most nine digits and reads exactly.
    fn exact_recipe(payload: &str, index: usize, name: &str, parsed: &NativeRecipe) -> NativeRecipe {
        let numbers = literal_numbers(&exact_member_text(payload, index, name));
        let mut recipe = parsed.clone();
        let wide = |field: &str| numbers.iter().filter(|(k, _)| k == field).map(|(_, v)| v.parse::<f64>().unwrap()).collect::<Vec<_>>();
        if let Some(matrix) = recipe.processing.camera_matrix.as_mut() {
            for (slot, value) in matrix.iter_mut().flatten().zip(wide("camera_matrix")) {
                *slot = value;
            }
        }
        for (slot, value) in recipe.processing.camera_tone_curve.iter_mut().flatten().zip(wide("camera_tone_curve")) {
            *slot = value;
        }
        if let super::super::WhiteBalance::Multipliers(values) = &mut recipe.processing.white_balance {
            for (slot, value) in values.iter_mut().zip(wide("Multipliers")) {
                *slot = value;
            }
        }
        recipe
    }
}
