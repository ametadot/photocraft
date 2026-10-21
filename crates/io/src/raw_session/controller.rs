//! Blocking native Camera Raw orchestration. A single host worker owns this
//! controller for a visit; the egui thread sends immutable commands and receives
//! snapshots. No method displays windows or writes a source photograph.

use super::commit::{self, Operation, Outcome, Plan};
use super::store::{CheckpointAck, RecoveryStore, StoreError};
use super::{Admission, NativeRecipe, Phase, Photo, PhotoId, Session, SourceBinding};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "render.rs"]
mod rendering;
pub use rendering::{
    DestinationReceipt, prepare_jpeg_destination, publish_jpeg, read_verified_source, remaining_rgb, render_recipe, validate_jpeg_destination,
};

#[derive(Clone, Debug)]
pub struct AdmissionReport {
    pub id: PhotoId,
    pub binding: SourceBinding,
    pub recipe: NativeRecipe,
    pub duplicate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeIssueKind {
    SourceUnavailable,
    SourceChanged,
    SidecarChanged,
    OperationUnresolved,
    AdmissionUnresolved,
}

#[derive(Clone, Debug)]
pub struct ResumeIssue {
    pub photo: Option<PhotoId>,
    pub kind: ResumeIssueKind,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ResumeReport {
    pub checkpoint: CheckpointAck,
    pub prior_checkpoint_time_ms: u64,
    pub used_fallback: bool,
    /// These rows stay in the session with their exact saved recipes. A host
    /// must not silently drop them or decode a substituted source.
    pub issues: Vec<ResumeIssue>,
}

#[derive(Clone, Debug)]
pub struct DoneReport {
    /// None is an exact semantic no-op, for which no journal or XMP is written.
    pub operation: Option<Operation>,
    pub checkpoint: CheckpointAck,
    /// Done: every sidecar saved and the session completed. Open Images: every sidecar
    /// saved and the session ready for the editor handoff ([`Controller::complete_open`]).
    /// Save edits: every sidecar saved; editing continues.
    pub completed: bool,
    /// XMP successes remain successes even when terminal publication fails.
    pub completion_error: Option<String>,
}

/// How an approved whole-session plan finishes. The plan and its consent are the same
/// for all three; only what follows a fully successful publication differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishKind {
    /// Save edits: publish, then keep editing.
    Save,
    /// Done: publish, then complete the session and reclaim its private records.
    Done,
    /// Open Images: publish, then hold the frozen session until the host reports that
    /// every retained photo reached the editor.
    Open,
}

#[derive(Clone, Debug)]
pub struct RemovalReport {
    pub operation: Option<Operation>,
    pub checkpoint: CheckpointAck,
    pub removed: bool,
    pub error: Option<String>,
}

pub struct Controller {
    store: RecoveryStore,
    session: Session,
    prepared: Option<String>,
    pending_discard: bool,
    /// Immutable boundary of a recovered visit, independent of current checkpoints.
    visit_entry: Option<Session>,
    restoring_discard: bool,
    /// Open Images published every sidecar; the session waits for the editor handoff.
    awaiting_handoff: bool,
    /// A finished action whose private records could not be removed yet.
    cleanup_warning: Option<String>,
    /// The durable store is a recovery-off journal claimed during THIS visit (see
    /// [`Self::retire_journal`]).
    journal_this_visit: bool,
    /// Open Images saved every sidecar of this finish and handed the photos on, so nothing
    /// is unsaved; it survives a later discard attempt (which only ends the session).
    opened: bool,
}

fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::Invalid(message.into())
}

fn identity(prefix: &str) -> Result<String, StoreError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| invalid(error.to_string()))?;
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    let material = format!("{}:{}:{}", std::process::id(), time.as_nanos(), counter);
    Ok(format!("{prefix}-{}", blake3::hash(material.as_bytes()).to_hex()))
}

impl Controller {
    /// Default editing mode retains recipes only in memory. No cache directory,
    /// lock, checkpoint or sidecar is created until an explicit save or opt-in.
    pub fn create_without_recovery(root: &Path) -> Result<Self, StoreError> {
        let id = identity("raw")?;
        let store = RecoveryStore::deferred(root, &id);
        let session = Session {
            schema: super::SCHEMA_VERSION,
            id,
            epoch: 0,
            generation: 0,
            cached_generation: None,
            phase: Phase::Editing,
            photos: Vec::new(),
            requested: BTreeMap::new(),
            sources: BTreeMap::new(),
            removed: BTreeSet::new(),
            view_context: serde_json::Value::Null,
        };
        Ok(Self::with(store, session))
    }

    fn with(store: RecoveryStore, session: Session) -> Self {
        Self {
            store,
            session,
            prepared: None,
            pending_discard: false,
            visit_entry: None,
            restoring_discard: false,
            awaiting_handoff: false,
            cleanup_warning: None,
            journal_this_visit: false,
            opened: false,
        }
    }

    /// Opt into long-session recovery. An explicit-save journal claimed earlier in this
    /// visit becomes the recoverable draft.
    pub fn enable_recovery(&mut self) -> Result<(), StoreError> {
        if self.store.durable() {
            self.store.set_automatic(true)?;
        } else {
            self.adopt_store(true)?;
        }
        self.journal_this_visit = false;
        Ok(())
    }

    /// Done, Open Images and removal journal their publication even with recovery off.
    /// The journal is never an editing draft, and it is reclaimed when the action ends.
    fn ensure_journal(&mut self) -> Result<(), StoreError> {
        if self.store.durable() {
            return Ok(());
        }
        self.adopt_store(false)?;
        self.journal_this_visit = true;
        Ok(())
    }

    /// An XMP publication this visit could not confirm (an unknown outcome, or a journal
    /// write whose result is unknown). Nothing can be discarded or completed until it is
    /// reconciled, which resuming the session does; its records must be kept until then.
    pub fn unresolved_publication(&self) -> bool {
        self.store.durable() && (self.store.uncertain() || self.session.photos.iter().any(|photo| photo.unknown_operation))
    }

    /// With recovery off, retire the explicit-save journal THIS visit claimed once its
    /// action ends. A resumed draft (or a journal resumed from an earlier visit) is never
    /// retired here: it may still hold recovered edits that exist nowhere else on disk.
    /// Explicit opt-out also preserves an earlier recovered draft.
    pub fn retire_journal(&mut self) -> Result<(), StoreError> {
        if !self.journal_this_visit || !self.store.durable() || self.store.automatic() {
            return Ok(());
        }
        self.disable_recovery()
    }

    fn adopt_store(&mut self, automatic: bool) -> Result<(), StoreError> {
        if self.session.phase == Phase::Completed {
            return Err(invalid("A completed Camera Raw session cannot be reopened for recovery"));
        }
        // Adopt durable ownership only once the entry is fully acknowledged.
        // A failed opt-in must leave subsequent editing in memory, as advertised.
        let root = self.store.root()?;
        let mut store = if automatic { RecoveryStore::claim(root, &self.session.id)? } else { RecoveryStore::claim_journal(root, &self.session.id)? };
        store.sidecar_publisher = self.store.sidecar_publisher.clone();
        let mut session = self.session.clone();
        session.epoch = store.epoch();
        session.generation = session.generation.max(store.next_generation()?);
        // Removals can predate this authority when recovery was off, or when a
        // prior cache was retired. Carry their lifecycle disposition forward.
        store.record_removals(&session.removed.iter().cloned().collect::<Vec<_>>())?;
        let ack = store.checkpoint(&session)?;
        store.pin_entry(ack.generation)?;
        session.cached_generation = Some(ack.generation);
        self.store = store;
        self.session = session;
        Ok(())
    }

    /// Stop automatic recovery in the host. Retire a fresh visit's draft only after
    /// its authority is safely completed; retain an earlier recovered draft.
    /// Uncertain publications must remain available for reconciliation.
    pub fn disable_recovery(&mut self) -> Result<(), StoreError> {
        self.editing()?;
        if self.pending_discard || self.session.photos.iter().any(|photo| photo.unknown_operation) {
            return Err(invalid("Resolve the pending save before turning recovery off"));
        }
        if !self.store.durable() {
            return Ok(());
        }
        // Opt-out stops future automatic acknowledgements; it does not authorize
        // deletion of the earlier recovered draft. Retain its authority so Cancel
        // can restore the immutable entry and explicit saving can finish safely.
        if self.visit_entry.is_some() {
            return Ok(());
        }
        let root = self.store.root()?.to_path_buf();
        let id = identity("raw")?;
        self.store.retire_for_memory_only()?;
        let mut store = RecoveryStore::deferred(&root, &id);
        store.sidecar_publisher = self.store.sidecar_publisher.clone();
        let retired = std::mem::replace(&mut self.store, store);
        self.note_reclaim(retired.reclaim());
        self.journal_this_visit = false;
        self.session.id = id;
        self.session.epoch = 0;
        self.session.generation = 0;
        self.session.cached_generation = None;
        self.prepared = None;
        Ok(())
    }

    /// The private records of a completed (terminal) store are removed promptly; a
    /// failure leaves them terminal and is reported once, never as a lost save.
    fn note_reclaim(&mut self, result: Result<(), StoreError>) {
        if let Err(error) = result {
            self.cleanup_warning = Some(format!(
                "The Camera Raw session finished, but its private recovery records could not be removed yet. They will not be offered again and are removed later: {error}"
            ));
        }
    }

    /// Terminal publication succeeded: release this store and reclaim its directory.
    fn reclaim_completed(&mut self) {
        let Ok(root) = self.store.root().map(Path::to_path_buf) else { return };
        let mut placeholder = RecoveryStore::deferred(&root, &self.session.id);
        placeholder.sidecar_publisher = self.store.sidecar_publisher.clone();
        let completed = std::mem::replace(&mut self.store, placeholder);
        self.note_reclaim(completed.reclaim());
    }

    /// A cleanup warning from the last finished action, reported once.
    pub fn take_cleanup_warning(&mut self) -> Option<String> {
        self.cleanup_warning.take()
    }

    /// The session has ended (Done, a completed Open Images handoff, or Cancel).
    pub fn completed(&self) -> bool {
        self.session.phase == Phase::Completed
    }

    /// Open Images saved every sidecar and still waits to record its handoff
    /// ([`Self::complete_open`] may be retried).
    pub fn awaiting_handoff(&self) -> bool {
        self.awaiting_handoff
    }

    /// Creates and acknowledges the empty entry snapshot before any admission.
    pub fn create(root: &Path) -> Result<Self, StoreError> {
        let id = identity("raw")?;
        if root.join(&id).try_exists()? {
            return Err(invalid("Generated session identity already exists"));
        }
        let store = RecoveryStore::claim(root, &id)?;
        let session = Session {
            schema: super::SCHEMA_VERSION,
            id,
            epoch: store.epoch(),
            generation: 0,
            cached_generation: None,
            phase: Phase::Editing,
            photos: Vec::new(),
            requested: BTreeMap::new(),
            sources: BTreeMap::new(),
            removed: BTreeSet::new(),
            view_context: serde_json::Value::Null,
        };
        let mut controller = Self::with(store, session);
        let ack = controller.checkpoint()?;
        controller.store.pin_entry(ack.generation)?;
        Ok(controller)
    }

    /// Claims a new epoch, reconciles journals without replay, verifies retained
    /// source identities, and durably establishes the new visit-entry snapshot.
    /// The record keeps its kind (draft or journal); see [`Self::resume_with_recovery`].
    pub fn resume(root: &Path, id: &str) -> Result<(Self, ResumeReport), StoreError> {
        Self::resume_with_recovery(root, id, false)
    }

    /// [`Self::resume`] for a host with recovery on: a resumed recovery-off journal becomes
    /// the visit's draft BEFORE the fence retires its resolved operations, so no crash point
    /// leaves its recovered edits as an "unpublished journal" that startup would collect.
    pub fn resume_with_recovery(root: &Path, id: &str, recovery_enabled: bool) -> Result<(Self, ResumeReport), StoreError> {
        Self::resume_from(RecoveryStore::claim_existing(root, id)?, recovery_enabled)
    }

    fn resume_from(store: RecoveryStore, recovery_enabled: bool) -> Result<(Self, ResumeReport), StoreError> {
        let restored = store.load()?.ok_or_else(|| invalid("Session contains no recovery checkpoint"))?;
        let checkpoint_time = restored.checkpoint_time_ms;
        let used_fallback = restored.used_fallback;
        let mut controller = Self::with(store, restored.session);
        // A member removed before the interruption never keeps a source ticket.
        let removed = controller.session.removed.clone();
        controller.session.requested.retain(|key, _| !key.strip_prefix("source-").is_some_and(|id| removed.contains(&PhotoId(id.into()))));
        let mut issues = controller.reconcile_journals()?;
        issues.extend(controller.validate_sources());
        for (key, state) in &mut controller.session.requested {
            if matches!(state, Admission::Pending) {
                let message = format!("Interrupted admission requires explicit retry or cancellation: {key}");
                *state = Admission::Unresolved(message.clone());
                issues.push(ResumeIssue { photo: None, kind: ResumeIssueKind::AdmissionUnresolved, message });
            }
        }
        for photo in &mut controller.session.photos {
            photo.entry = photo.working.clone();
            photo.committed_this_visit = false;
        }
        controller.session.phase = Phase::Editing;
        controller.changed()?;
        let checkpoint = controller.checkpoint()?;
        // A resumed session that holds recovered edits not yet saved to XMP stays a draft
        // until it is finished, even with recovery off: those edits exist nowhere else.
        let recovered_work = controller.session.photos.iter().any(|photo| photo.pending() != Ok(false));
        if recovery_enabled || recovered_work {
            controller.store.set_automatic(true)?;
        }
        controller.store.pin_entry(checkpoint.generation)?;
        controller.store.fence_recovery(checkpoint.generation)?;
        controller.visit_entry = Some(controller.session.clone());
        Ok((controller, ResumeReport { checkpoint, prior_checkpoint_time_ms: checkpoint_time, used_fallback, issues }))
    }

    /// "Discard recovered copy": the photographer's confirmed disposal of an interrupted
    /// session. Its journals are reconciled read-only first, and an uncertain publication
    /// refuses the discard. Nothing is written next to a photograph. Returns a cleanup
    /// warning when the records could not be removed yet.
    pub fn discard_recovered(root: &Path, id: &str) -> Result<Option<String>, StoreError> {
        Self::discard_recovered_listed(root, id, None)
    }

    /// [`Self::discard_recovered`] of exactly the state the chooser listed: refused, with
    /// nothing written, when another PhotoCraft resumed or changed the session meanwhile.
    pub fn discard_recovered_listed(root: &Path, id: &str, listed: Option<&super::store::Listed>) -> Result<Option<String>, StoreError> {
        let store = match listed {
            Some(listed) => RecoveryStore::claim_listed(root, id, listed)?,
            None => RecoveryStore::claim_existing(root, id)?,
        };
        let (mut controller, _) = Self::resume_from(store, false)?;
        // Explicit recovered-copy deletion remains terminal, unlike resumed Cancel.
        controller.visit_entry = None;
        controller.discard_visit()?;
        Ok(controller.take_cleanup_warning())
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Install the host's current write authority before preparing any mutation.
    /// It is intentionally not serialized: a recovered visit must reacquire its
    /// authority from the current host, never inherit old read/write grants.
    pub fn set_sidecar_publisher(&mut self, publisher: Option<super::SidecarPublisher>) {
        self.store.sidecar_publisher = publisher;
    }

    pub fn pending_discard(&self) -> bool {
        self.pending_discard
    }

    pub fn read_verified_source(&self, id: &PhotoId) -> Result<Vec<u8>, StoreError> {
        let binding = self.session.sources.get(id).ok_or_else(|| invalid("Photo has no retained source binding"))?;
        read_verified_source(binding)
    }

    /// Explicit recovery retry. Only the matching synthetic source ticket can
    /// clear, after the original source and bound sidecar are both revalidated.
    /// A restored filename with a different physical identity never qualifies.
    pub fn resolve_source(&mut self, id: &PhotoId) -> Result<CheckpointAck, StoreError> {
        self.editing()?;
        if !self.session.photos.iter().any(|photo| &photo.id == id) {
            return Err(invalid("Source retry member is not retained"));
        }
        let binding = self.session.sources.get(id).ok_or_else(|| invalid("Source retry has no retained binding"))?;
        let current = commit::bind(&binding.source)?;
        if !same_source(&current, binding) {
            return Err(invalid("Source retry differs from the retained physical identity or contents"));
        }
        let migrate = current != *binding && commit::legacy_unwritten_rendered(binding, &current);
        if current != *binding && !migrate {
            return Err(invalid("Source is restored, but its sidecar conflict still requires resolution"));
        }
        if migrate {
            self.changed()?;
            self.session.sources.insert(id.clone(), current);
        }
        let key = format!("source-{}", id.0);
        if matches!(self.session.requested.get(&key), Some(Admission::Unresolved(_))) {
            self.changed()?;
            self.session.requested.remove(&key);
        }
        self.checkpoint()
    }

    fn editing(&self) -> Result<(), StoreError> {
        if self.session.phase != Phase::Editing {
            return Err(invalid("Camera Raw mutations are frozen while finishing or paused"));
        }
        Ok(())
    }

    fn changed(&mut self) -> Result<(), StoreError> {
        self.session.generation = self.session.generation.checked_add(1).ok_or_else(|| invalid("Session generation exhausted"))?;
        self.prepared = None;
        Ok(())
    }

    /// Keys are stable request tickets (normally the requested path), never
    /// selection positions. Hosts submit and settle tickets in input order.
    pub fn request(&mut self, key: String) -> Result<(), StoreError> {
        self.editing()?;
        if key.is_empty() || key.len() > 32_768 || self.session.requested.len() >= super::MAX_PHOTOS {
            return Err(invalid("Invalid admission ticket or session capacity reached"));
        }
        if self.session.requested.contains_key(&key) {
            return Err(invalid("Admission ticket already exists"));
        }
        let total = self
            .session
            .requested
            .iter()
            .map(|(key, outcome)| {
                key.len()
                    + match outcome {
                        Admission::Rejected(reason) | Admission::Unresolved(reason) => reason.len(),
                        _ => 0,
                    }
            })
            .sum::<usize>();
        if total.saturating_add(key.len()) > 1024 * 1024 {
            return Err(invalid("Admission text exceeds 1 MiB recovery budget"));
        }
        self.changed()?;
        self.session.requested.insert(key, Admission::Pending);
        Ok(())
    }

    pub fn reject(&mut self, key: &str, reason: String) -> Result<(), StoreError> {
        self.settle(key, Admission::Rejected(reason))
    }

    pub fn cancel_request(&mut self, key: &str) -> Result<(), StoreError> {
        self.settle(key, Admission::Cancelled)
    }

    fn settle(&mut self, key: &str, result: Admission) -> Result<(), StoreError> {
        if !matches!(self.session.phase, Phase::Editing | Phase::Settling) {
            return Err(invalid("Admission callbacks are fenced after freezing"));
        }
        if !self.session.requested.get(key).is_some_and(|state| matches!(state, Admission::Pending | Admission::Unresolved(_))) {
            return Err(invalid("Admission ticket is not pending or unresolved"));
        }
        let reason_len = match &result {
            Admission::Rejected(reason) | Admission::Unresolved(reason) => reason.len(),
            _ => 0,
        };
        if reason_len > 32768 {
            return Err(invalid("Admission diagnostic exceeds length budget"));
        }
        let total = self
            .session
            .requested
            .iter()
            .map(|(ticket, outcome)| {
                ticket.len()
                    + if ticket != key {
                        match outcome {
                            Admission::Rejected(reason) | Admission::Unresolved(reason) => reason.len(),
                            _ => 0,
                        }
                    } else {
                        0
                    }
            })
            .sum::<usize>();
        if total.saturating_add(reason_len) > 1024 * 1024 {
            return Err(invalid("Admission diagnostics exceed 1 MiB recovery budget"));
        }
        self.changed()?;
        self.session.requested.insert(key.to_owned(), result);
        Ok(())
    }

    /// Uses the exact bytes held by the import request and proves they still
    /// match the retained native file. Complete native XMP takes precedence only
    /// after its projection provenance has passed the strict codec validation.
    pub fn admit(&mut self, key: &str, path: &Path, bytes: &[u8]) -> Result<AdmissionReport, StoreError> {
        if !matches!(self.session.phase, Phase::Editing | Phase::Settling) {
            return Err(invalid("Admission is frozen"));
        }
        if !self.session.requested.get(key).is_some_and(|state| matches!(state, Admission::Pending | Admission::Unresolved(_))) {
            return Err(invalid("No unsettled admission ticket"));
        }
        let result = self.prepare_admission(path, bytes);
        let (binding, recipe) = match result {
            Ok(value) => value,
            Err(error) => {
                self.settle(key, Admission::Unresolved(error.to_string()))?;
                return Err(error);
            }
        };
        if let Some(photo) = self
            .session
            .photos
            .iter()
            .find(|photo| self.session.sources.get(&photo.id).is_some_and(|existing| existing.source_identity == binding.source_identity))
        {
            let id = photo.id.clone();
            let recipe = photo.working.clone();
            let retained = self.session.sources.get(&id).cloned().ok_or_else(|| invalid("Duplicate has no retained binding"))?;
            self.settle(key, Admission::Duplicate(id.clone()))?;
            return Ok(AdmissionReport { id, binding: retained, recipe, duplicate: true });
        }
        if self.session.photos.len() >= super::MAX_PHOTOS {
            return Err(invalid("Session photo capacity reached"));
        }
        let originals = self.session.sources.values().map(|b| b.original.as_ref().map_or(0, Vec::len)).sum::<usize>();
        if originals.saturating_add(binding.original.as_ref().map_or(0, Vec::len)) > super::store::MAX_SESSION_XMP_BYTES {
            self.settle(key, Admission::Unresolved("Session original XMP packets exceed 2 MiB aggregate recovery budget".into()))?;
            return Err(invalid("Session original XMP packets exceed 2 MiB aggregate recovery budget"));
        }
        let current_bytes = super::store::serialized_size(&self.session, super::store::MAX_PAYLOAD_BYTES)?;
        let binding_bytes = super::store::serialized_size(&binding, super::store::MAX_PAYLOAD_BYTES)?;
        let recipe_bytes = super::store::serialized_size(&recipe, super::MAX_RECIPE_BYTES)?;
        if current_bytes.saturating_add(binding_bytes).saturating_add(recipe_bytes.saturating_mul(3)).saturating_add(2048) > super::store::MAX_PAYLOAD_BYTES {
            self.settle(key, Admission::Unresolved("Session exceeds 12 MiB encoded recovery budget".into()))?;
            return Err(invalid("Session exceeds 12 MiB encoded recovery budget"));
        }
        let id = PhotoId(identity("photo")?);
        self.settle(key, Admission::Ready(id.clone()))?;
        self.session.sources.insert(id.clone(), binding.clone());
        self.session.photos.push(Photo {
            id: id.clone(),
            working: recipe.clone(),
            committed: Some(recipe.clone()),
            entry: recipe.clone(),
            revision: 1,
            hidden: false,
            selected: false,
            committed_this_visit: false,
            unknown_operation: false,
        });
        Ok(AdmissionReport { id, binding, recipe, duplicate: false })
    }

    fn prepare_admission(&self, path: &Path, bytes: &[u8]) -> Result<(SourceBinding, NativeRecipe), StoreError> {
        let binding = commit::bind(path)?;
        if blake3::hash(bytes).to_hex().as_str() != binding.source_fingerprint {
            return Err(invalid("Imported source bytes differ from the retained native source"));
        }
        let recipe = crate::camera_raw_source::initial_recipe(bytes, binding.original.as_deref()).map_err(|error| invalid(error.to_string()))?;
        Ok((binding, recipe))
    }

    pub fn apply(&mut self, changes: Vec<(PhotoId, NativeRecipe)>) -> Result<(), StoreError> {
        self.session.apply(changes).map_err(invalid)
    }

    /// Atomically validates and applies a complete host edit/view snapshot, then
    /// acknowledges that exact state. The host coalesces queued snapshots before
    /// invoking this blocking method on its worker.
    pub fn sync_edits(
        &mut self,
        changes: Vec<(PhotoId, NativeRecipe)>,
        view_states: Vec<(PhotoId, bool, bool)>,
        view_context: serde_json::Value,
    ) -> Result<CheckpointAck, StoreError> {
        self.sync_edits_with_recovery(changes, view_states, view_context, true)
    }

    /// A retained explicit-save journal does not opt the host into automatic
    /// recovery. Ordinary snapshots may update memory while that journal stays
    /// available for reconciliation; explicit finish methods checkpoint it.
    pub fn sync_edits_with_recovery(
        &mut self,
        changes: Vec<(PhotoId, NativeRecipe)>,
        view_states: Vec<(PhotoId, bool, bool)>,
        view_context: serde_json::Value,
        recovery_enabled: bool,
    ) -> Result<CheckpointAck, StoreError> {
        self.editing()?;
        super::store::validate_session_bounds(&self.session)?;
        super::store::serialized_size(&view_context, 256 * 1024)?;
        for (_, recipe) in &changes {
            recipe.validate().map_err(invalid)?;
        }
        let mut candidate = self.session.clone();
        candidate.apply(changes).map_err(invalid)?;
        let mut ids = BTreeSet::new();
        for (id, _, _) in &view_states {
            if !ids.insert(id) || !candidate.photos.iter().any(|photo| &photo.id == id) {
                return Err(invalid("Invalid view member"));
            }
        }
        let mut changed = candidate.view_context != view_context;
        candidate.view_context = view_context;
        for (id, hidden, selected) in view_states {
            if let Some(photo) = candidate.photos.iter_mut().find(|photo| photo.id == id) {
                changed |= photo.hidden != hidden || photo.selected != selected;
                photo.hidden = hidden;
                photo.selected = selected;
            }
        }
        if changed {
            candidate.generation = candidate.generation.checked_add(1).ok_or_else(|| invalid("Generation exhausted"))?;
        }
        super::store::validate_session_bounds(&candidate)?;
        self.session = candidate;
        self.acknowledge_edits(recovery_enabled)
    }

    pub fn update_view(&mut self, states: Vec<(PhotoId, bool, bool)>) -> Result<(), StoreError> {
        self.editing()?;
        let mut ids = BTreeSet::new();
        for (id, _, _) in &states {
            if !ids.insert(id) || !self.session.photos.iter().any(|photo| &photo.id == id) {
                return Err(invalid("Invalid view member"));
            }
        }
        let changed = states
            .iter()
            .any(|(id, hidden, selected)| self.session.photos.iter().any(|photo| &photo.id == id && (photo.hidden != *hidden || photo.selected != *selected)));
        if changed {
            self.changed()?;
            for (id, hidden, selected) in states {
                if let Some(photo) = self.session.photos.iter_mut().find(|photo| photo.id == id) {
                    photo.hidden = hidden;
                    photo.selected = selected;
                }
            }
        }
        Ok(())
    }

    /// The acknowledgement belongs to the exact state serialized by this worker.
    /// Immutable generation numbers also skip orphaned interrupted publications.
    /// A lifecycle checkpoint (finish, discard, removal, resume): it keeps the store's kind,
    /// so a recovery-off publication journal stays a journal.
    pub fn checkpoint(&mut self) -> Result<CheckpointAck, StoreError> {
        self.acknowledge(true)
    }

    /// The host's acknowledgement of live edits. With recovery on, the checkpoint it writes
    /// is a recoverable draft, also when this session was resumed from a recovery-off
    /// publication journal: mark the store so the draft is offered, never collected.
    pub fn acknowledge_edits(&mut self, recovery_enabled: bool) -> Result<CheckpointAck, StoreError> {
        if recovery_enabled && self.store.durable() && self.session.phase != Phase::Completed {
            self.store.set_automatic(true)?;
            self.journal_this_visit = false;
        }
        self.acknowledge(recovery_enabled)
    }

    fn acknowledge(&mut self, recovery_enabled: bool) -> Result<CheckpointAck, StoreError> {
        if self.session.phase == Phase::Completed {
            return Err(invalid("Completed sessions cannot checkpoint"));
        }
        if !recovery_enabled || !self.store.durable() {
            super::store::validate_session_bounds(&self.session)?;
            return Ok(CheckpointAck {
                durable: false,
                session: self.session.id.clone(),
                epoch: self.session.epoch,
                generation: self.session.generation,
                checksum: String::new(),
                time_ms: 0,
                storage_pressure: None,
            });
        }
        self.session.generation = self.session.generation.max(self.store.next_generation()?);
        let ack = self.store.checkpoint(&self.session)?;
        if ack.session != self.session.id || ack.epoch != self.session.epoch || ack.generation != self.session.generation {
            return Err(StoreError::Uncertain("Checkpoint acknowledgement identity mismatch".into()));
        }
        self.session.cached_generation = Some(ack.generation);
        Ok(ack)
    }

    /// Settles/freeze/checkpoints/preflights, without any source-side publication.
    /// The returned complete Plan is the only valid input to the consent dialog.
    pub fn prepare_done(&mut self) -> Result<Plan, StoreError> {
        self.prepare_all(false)
    }

    /// Freeze and identify an explicit Save edits checkpoint. The action itself
    /// is included in the plan fingerprint, so Done consent cannot close it.
    pub fn prepare_save(&mut self) -> Result<Plan, StoreError> {
        self.prepare_all(true)
    }

    fn prepare_all(&mut self, stay_open: bool) -> Result<Plan, StoreError> {
        if self.session.phase == Phase::Editing {
            // A photo restored since it was named as changed verifies again: its ticket clears.
            self.retry_source_tickets();
            self.session.begin_finish().map_err(invalid)?;
        }
        if self.session.phase == Phase::Settling {
            self.session.freeze().map_err(invalid)?;
        }
        if self.session.phase != Phase::Frozen || self.session.photos.iter().any(|photo| photo.unknown_operation) {
            return Err(invalid("Reconcile uncertain operations before preparing Done"));
        }
        self.awaiting_handoff = false;
        self.opened = false;
        self.ensure_journal()?;
        self.checkpoint()?;
        let prepared =
            if stay_open { commit::prepare_save(&self.session, &self.session.sources) } else { commit::prepare(&self.session, &self.session.sources) };
        let plan = prepared.map_err(|error| self.name_changed_bindings(error))?;
        self.prepared = Some(plan.id.clone());
        Ok(plan)
    }

    /// A plan refused because a photo's source or sidecar changed outside PhotoCraft since it
    /// was opened: name every such photo and give it a "source" ticket, so Recovery details
    /// offers to restore it (Retry) or to discard just that photo, and the others can still
    /// be saved. Nothing is written; any other refusal is returned unchanged.
    fn name_changed_bindings(&mut self, error: StoreError) -> StoreError {
        let mut changed = Vec::new();
        for photo in &self.session.photos {
            if self.session.removed.contains(&photo.id) || photo.pending() != Ok(true) {
                continue;
            }
            if let Some(binding) = self.session.sources.get(&photo.id)
                && let Err(reason) = commit::verify_binding(binding)
            {
                let name = binding.source.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                changed.push((photo.id.clone(), name, reason.to_string()));
            }
        }
        if changed.is_empty() {
            return error;
        }
        for (id, _, reason) in &changed {
            self.session.requested.entry(format!("source-{}", id.0)).or_insert_with(|| Admission::Unresolved(reason.clone()));
        }
        let names: Vec<_> = changed.into_iter().map(|(_, name, _)| name).collect();
        invalid(format!(
            "{}: changed outside PhotoCraft since it was opened, so it cannot be saved. Restore it and choose Done again, or remove it in Recovery details (Discard this photo's changes and remove) to save the others. No XMP file was changed",
            names.join(", ")
        ))
    }

    /// "Retry recovery save" for a live session: a source ticket whose photo verifies again
    /// (for example a share that came back) is cleared. Others stay, unchanged.
    pub fn retry_source_tickets(&mut self) {
        let ids: Vec<PhotoId> = self
            .session
            .requested
            .keys()
            .filter_map(|key| key.strip_prefix("source-"))
            .map(|id| PhotoId(id.to_owned()))
            .filter(|id| self.session.photos.iter().any(|photo| &photo.id == id))
            .collect();
        for id in ids {
            let _ = self.resolve_source(&id);
        }
    }

    pub fn cancel_finish(&mut self) -> Result<(), StoreError> {
        // An acknowledged discard has tombstoned every member: only finishing it (or its
        // retry) is valid. Returning to editing would leave Cancel unable to complete.
        if self.pending_discard {
            return Err(invalid("Finish the acknowledged discard before returning to editing"));
        }
        if !matches!(self.session.phase, Phase::Settling | Phase::Frozen) {
            return Err(invalid("There is no finish decision to cancel"));
        }
        self.changed()?;
        self.session.phase = Phase::Editing;
        self.awaiting_handoff = false;
        self.opened = false;
        Ok(())
    }

    /// Open Images saved and opened every photograph of this finish (see `opened`).
    pub fn opened_images(&self) -> bool {
        self.opened
    }

    pub fn prepare_remove(&mut self, id: &PhotoId) -> Result<Plan, StoreError> {
        if self.session.phase == Phase::Editing {
            self.retry_source_tickets();
            self.session.begin_finish().map_err(invalid)?;
        }
        if self.session.phase == Phase::Settling {
            self.session.freeze().map_err(invalid)?;
        }
        if self.session.phase != Phase::Frozen || self.session.photos.iter().any(|photo| &photo.id == id && photo.unknown_operation) {
            return Err(invalid("Reconcile uncertain removal before saving"));
        }
        self.ensure_journal()?;
        self.checkpoint()?;
        let plan = commit::prepare_photo(&self.session, &self.session.sources, id)?;
        self.prepared = Some(plan.id.clone());
        Ok(plan)
    }

    /// Saves only the concretely approved member. Any failure retains that row
    /// and its acknowledged baseline; successful removal publishes a tombstone
    /// before the UI is allowed to release pixels or runtime IDs.
    pub fn commit_remove(&mut self, plan: Plan, approved_id: &str) -> Result<RemovalReport, StoreError> {
        self.commit_remove_with_progress(plan, approved_id, &mut |_, _| {})
    }

    pub fn commit_remove_with_progress(&mut self, plan: Plan, approved_id: &str, progress: &mut dyn FnMut(usize, usize)) -> Result<RemovalReport, StoreError> {
        let commit::SaveScope::Photo(id) = &plan.scope else {
            return Err(invalid("Removal requires a single-photo plan"));
        };
        let id = id.clone();
        if !self.matches_prepared(&plan, approved_id)? {
            return Err(invalid("Removal consent does not match its prepared plan"));
        }
        let operation = if plan.photos.is_empty() {
            None
        } else {
            self.session.phase = Phase::Committing;
            match self.store.commit_plan_with_progress(plan, approved_id, progress) {
                Ok(operation) => Some(operation),
                Err(error) => {
                    self.session.phase = Phase::Frozen;
                    self.prepared = None;
                    if matches!(error, StoreError::SidecarUnknown(_))
                        && let Some(photo) = self.session.photos.iter_mut().find(|photo| photo.id == id)
                    {
                        photo.unknown_operation = true;
                    }
                    return Err(error);
                }
            }
        };
        self.session.phase = Phase::Frozen;
        let mut error = None;
        if let Some(operation) = &operation {
            match self.overlay_operation(operation) {
                Ok(issues) => {
                    // Removal is scoped to this member. A confirmed retry can
                    // replace its older known failure while other members keep
                    // their unresolved journals. Unknown outcomes still set the
                    // member's guard and cannot pass the clean check below.
                    let saved = operation.outcomes.get(&id) == Some(&Outcome::Success);
                    let issues: Vec<_> = issues
                        .into_iter()
                        .filter(|issue| {
                            issue.photo.is_none() || (issue.photo.as_ref() == Some(&id) && !(saved && issue.kind == ResumeIssueKind::OperationUnresolved))
                        })
                        .collect();
                    if !issues.is_empty() {
                        error = Some(issues.iter().map(|issue| issue.message.as_str()).collect::<Vec<_>>().join("; "));
                    }
                }
                Err(failure) => {
                    self.session.phase = Phase::Frozen;
                    self.prepared = None;
                    return Err(failure);
                }
            }
        }
        let clean = self.session.photos.iter().find(|photo| photo.id == id).is_some_and(|photo| !photo.unknown_operation && photo.pending() == Ok(false));
        let removed = if clean && error.is_none() {
            match self.store.record_removal(id.clone()) {
                Ok(()) => {
                    self.session.removed.insert(id.clone());
                    self.session.photos.retain(|photo| photo.id != id);
                    self.session.sources.remove(&id);
                    self.drop_source_ticket(&id);
                    true
                }
                Err(failure) => {
                    error = Some(failure.to_string());
                    false
                }
            }
        } else {
            false
        };
        self.session.phase = Phase::Editing;
        self.changed()?;
        let checkpoint = self.checkpoint()?;
        self.store.fence_recovery(checkpoint.generation)?;
        Ok(RemovalReport { operation, checkpoint, removed, error })
    }

    /// Consent names the already displayed immutable PlanId. This never writes a
    /// different plan, even when the source or sidecar changed after preparation.
    pub fn commit_done(&mut self, plan: Plan, approved_id: &str) -> Result<DoneReport, StoreError> {
        self.commit_done_with_progress(plan, approved_id, &mut |_, _| {})
    }

    pub fn commit_done_with_progress(&mut self, plan: Plan, approved_id: &str, progress: &mut dyn FnMut(usize, usize)) -> Result<DoneReport, StoreError> {
        self.commit_all(plan, approved_id, progress, FinishKind::Done)
    }

    /// Open Images publishes the same reviewed Done plan first. A fully successful
    /// publication leaves the session frozen for the editor handoff; it completes only in
    /// [`Self::complete_open`], after every retained photo reached the editor.
    pub fn commit_open_with_progress(&mut self, plan: Plan, approved_id: &str, progress: &mut dyn FnMut(usize, usize)) -> Result<DoneReport, StoreError> {
        self.commit_all(plan, approved_id, progress, FinishKind::Open)
    }

    /// The host opened every retained photo in the editor after a successful Open
    /// Images publication: record the terminal disposition and reclaim the records.
    pub fn complete_open(&mut self) -> Result<(), StoreError> {
        if !self.awaiting_handoff || self.session.phase != Phase::Frozen {
            return Err(invalid("Open Images has not saved every sidecar for this session"));
        }
        if self.session.photos.iter().any(|photo| photo.unknown_operation || photo.pending() != Ok(false)) {
            return Err(invalid("Open Images found unsaved edits after publication; the session remains open"));
        }
        if self.store.durable() {
            self.store.complete()?;
            self.session.phase = Phase::Completed;
            self.reclaim_completed();
        } else {
            self.session.phase = Phase::Completed;
        }
        self.awaiting_handoff = false;
        Ok(())
    }

    pub fn commit_save(&mut self, plan: Plan, approved_id: &str) -> Result<DoneReport, StoreError> {
        self.commit_save_with_progress(plan, approved_id, &mut |_, _| {})
    }

    /// Publishes the exact captured recipes and acknowledges their committed
    /// baselines without retiring the recovery session or changing membership.
    /// `completed` reports Save success; the controller remains in Editing.
    pub fn commit_save_with_progress(&mut self, plan: Plan, approved_id: &str, progress: &mut dyn FnMut(usize, usize)) -> Result<DoneReport, StoreError> {
        self.commit_all(plan, approved_id, progress, FinishKind::Save)
    }

    fn commit_all(&mut self, plan: Plan, approved_id: &str, progress: &mut dyn FnMut(usize, usize), kind: FinishKind) -> Result<DoneReport, StoreError> {
        let stay_open = kind == FinishKind::Save;
        let scope = if stay_open { commit::SaveScope::SaveEdits } else { commit::SaveScope::All };
        if plan.scope != scope || !self.matches_prepared(&plan, approved_id)? {
            return Err(invalid("Done consent does not match the prepared frozen plan"));
        }
        let prior = self.store.operation_ids();
        let operation = if plan.photos.is_empty() {
            None
        } else {
            self.session.phase = Phase::Committing;
            match self.store.commit_plan_with_progress(plan, approved_id, progress) {
                Ok(operation) => Some(operation),
                Err(error) => {
                    // Publication may already have occurred. Preserve the entire
                    // attempted state and force journal reconciliation on resume.
                    self.session.phase = Phase::Frozen;
                    if matches!(error, StoreError::SidecarUnknown(_)) {
                        for photo in &mut self.session.photos {
                            if photo.pending().unwrap_or(true) {
                                photo.unknown_operation = true;
                            }
                        }
                    }
                    self.prepared = None;
                    return Err(error);
                }
            }
        };
        self.session.phase = Phase::Frozen;
        let mut completion_error = None;
        if let Some(operation) = &operation {
            // Resolve covered historical failures before building the current
            // report. An old failure successfully superseded by this reviewed
            // operation must not keep an otherwise successful Done open.
            for old_id in prior {
                let old = self.store.read_operation(&old_id)?;
                // A definite failure for a photo since reverted is not an obligation.
                let unresolved = old.plan.photos.iter().any(|planned| {
                    let outcome = old.outcomes.get(&planned.photo);
                    outcome != Some(&Outcome::Success)
                        && !self.session.removed.contains(&planned.photo)
                        && !outcome.is_some_and(|outcome| commit::failure_is_moot(&self.session, planned, outcome))
                });
                if unresolved && let Err(error) = self.store.supersede_operation(&old_id, &operation.plan.id, &self.session) {
                    completion_error.get_or_insert_with(|| error.to_string());
                }
            }
            let issues = match self.overlay_operation(operation) {
                Ok(issues) => issues,
                Err(error) => {
                    self.session.phase = Phase::Frozen;
                    self.prepared = None;
                    return Err(error);
                }
            };
            if !issues.is_empty() {
                completion_error = Some(issues.iter().map(|issue| issue.message.as_str()).collect::<Vec<_>>().join("; "));
            }
        }
        let clean = self.session.photos.iter().all(|photo| !photo.unknown_operation && photo.pending() == Ok(false));
        self.session.phase = if clean && !stay_open { Phase::Frozen } else { Phase::Editing };
        self.changed()?;
        let checkpoint = self.checkpoint()?;
        self.store.fence_recovery(checkpoint.generation)?;
        let completed = match kind {
            FinishKind::Save => clean && completion_error.is_none(),
            FinishKind::Open => {
                // Saved sidecars stay saved; the terminal record waits for the handoff, and
                // the handoff waits until that record is certain to be accepted.
                // (Without a durable store, `complete_open` records nothing to refuse.)
                if clean
                    && completion_error.is_none()
                    && self.store.durable()
                    && let Err(error) = self.store.check_complete()
                {
                    completion_error = Some(error.to_string());
                }
                self.awaiting_handoff = clean && completion_error.is_none();
                self.opened = self.awaiting_handoff;
                self.awaiting_handoff
            }
            FinishKind::Done if clean && completion_error.is_none() => match self.store.complete() {
                Ok(()) => {
                    self.session.phase = Phase::Completed;
                    self.reclaim_completed();
                    true
                }
                Err(error) => {
                    completion_error = Some(error.to_string());
                    false
                }
            },
            FinishKind::Done => false,
        };
        Ok(DoneReport { operation, checkpoint, completed, completion_error })
    }

    fn matches_prepared(&self, plan: &Plan, approved_id: &str) -> Result<bool, StoreError> {
        Ok(self.session.phase == Phase::Frozen
            && self.prepared.as_deref() == Some(approved_id)
            && plan.id == approved_id
            && commit::fingerprint_plan(plan)? == approved_id
            && plan.session == self.session.id
            && plan.epoch == self.session.epoch
            && plan.generation == self.session.generation)
    }

    /// Acknowledge the complete opt-in recovery draft before closing this visit.
    pub fn keep(&mut self) -> Result<CheckpointAck, StoreError> {
        if !self.store.durable() || !self.store.automatic() {
            return Err(invalid("Turn on long-session recovery before keeping a session"));
        }
        if self.pending_discard {
            return Err(invalid("Finish the acknowledged discard disposition before choosing another close action"));
        }
        if matches!(self.session.phase, Phase::Completed | Phase::Committing) {
            return Err(invalid("Session cannot be paused in its current phase"));
        }
        let prior = self.session.phase.clone();
        self.changed()?;
        self.session.phase = Phase::Paused;
        match self.checkpoint() {
            Ok(ack) => Ok(ack),
            Err(error) => {
                // Definite failure may leave an orphan generation; keep its
                // number and all live recipes, but allow the same visit to retry.
                if !matches!(error, StoreError::Uncertain(_) | StoreError::SidecarUnknown(_)) {
                    self.session.phase = prior;
                }
                Err(error)
            }
        }
    }

    /// Cancel: restore a resumed entry draft, or terminate a fresh draft. No sidecar
    /// is written and no earlier XMP success is undone. Removed members are fenced;
    /// uncertain publication blocks discard until reconciled. A fresh discard publishes
    /// its terminal record before reclaiming private records. Resumed discard keeps
    /// a new acknowledged checkpoint of the entry draft and actual saved successes.
    pub fn discard_visit(&mut self) -> Result<CheckpointAck, StoreError> {
        if self.pending_discard {
            return self.finish_discard();
        }
        if matches!(self.session.phase, Phase::Committing | Phase::Completed) {
            return Err(invalid("Session cannot discard in its current phase"));
        }
        if self.unresolved_publication() {
            return Err(invalid("Reconcile uncertain XMP operations before discard"));
        }
        if let Some(entry) = self.visit_entry.as_ref().filter(|_| !self.opened) {
            if !self.store.durable() {
                return Err(invalid("The recovered entry draft needs durable storage before this visit can be discarded; enable recovery or keep editing"));
            }
            let mut restored = self.session.discarded_visit().map_err(invalid)?;
            restored.view_context = entry.view_context.clone();
            let dropped: Vec<_> = restored
                .photos
                .iter()
                .filter(|photo| !photo.committed_this_visit && !entry.photos.iter().any(|old| old.id == photo.id))
                .map(|photo| photo.id.clone())
                .collect();
            self.store.record_removals(&dropped)?;
            restored.removed.extend(dropped);
            restored.photos.retain(|photo| !restored.removed.contains(&photo.id));
            for photo in &mut restored.photos {
                if let Some(old) = entry.photos.iter().find(|old| old.id == photo.id) {
                    if !photo.committed_this_visit {
                        photo.working = old.working.clone();
                    }
                    photo.hidden = old.hidden;
                    photo.selected = old.selected;
                }
            }
            restored.sources.retain(|id, _| restored.photos.iter().any(|photo| &photo.id == id));
            restored.requested.retain(|key, state| match state {
                Admission::Ready(id) | Admission::Duplicate(id) => !restored.removed.contains(id),
                _ => entry.requested.contains_key(key),
            });
            self.session = restored;
            self.prepared = None;
            self.awaiting_handoff = false;
            self.pending_discard = true;
            self.restoring_discard = true;
            return self.finish_discard();
        }
        let remove: Vec<_> = self.session.photos.iter().map(|photo| photo.id.clone()).collect();
        // Publish authority before dropping any current or entry membership.
        if self.store.durable() {
            self.store.record_removals(&remove)?;
        }
        let mut discarded = self.session.clone();
        discarded.removed.extend(remove);
        discarded.photos.clear();
        discarded.sources.retain(|id, _| !discarded.removed.contains(id));
        for state in discarded.requested.values_mut() {
            if matches!(state, Admission::Pending | Admission::Unresolved(_)) {
                *state = Admission::Cancelled;
            }
        }
        discarded.generation = discarded.generation.checked_add(1).ok_or_else(|| invalid("Generation exhausted"))?;
        discarded.phase = Phase::Frozen;
        self.session = discarded;
        self.prepared = None;
        self.awaiting_handoff = false;
        self.pending_discard = true;
        self.finish_discard()
    }

    fn finish_discard(&mut self) -> Result<CheckpointAck, StoreError> {
        let ack = self.checkpoint()?;
        if self.store.durable() {
            self.store.fence_recovery(ack.generation)?;
            if self.restoring_discard {
                self.store.set_automatic(true)?;
                self.session.phase = Phase::Paused;
                self.pending_discard = false;
                self.restoring_discard = false;
                return Ok(ack);
            }
            self.store.complete()?;
            self.session.phase = Phase::Completed;
            self.reclaim_completed();
        }
        self.session.phase = Phase::Completed;
        self.pending_discard = false;
        Ok(ack)
    }

    /// Explicit user discard (or already-clean removal). Filtering must never
    /// call this: it is a durable member disposition, not a presentation update.
    /// A removed member's "restore the original source" ticket goes with it: otherwise its
    /// unresolved ticket would block every later Done, Save edits and removal.
    fn drop_source_ticket(&mut self, id: &PhotoId) {
        self.session.requested.remove(&format!("source-{}", id.0));
    }

    pub fn remove_discard(&mut self, id: &PhotoId) -> Result<CheckpointAck, StoreError> {
        if self.pending_discard {
            return Err(invalid("Finish the acknowledged discard before removing a photograph"));
        }
        self.editing()?;
        if self.session.removed.contains(id) {
            // A previous call may have acknowledged the authoritative tombstone
            // and then failed its checkpoint. Retry that exact disposition;
            // stale UI recipes must never recreate the removed member.
            self.session.sources.remove(id);
            self.drop_source_ticket(id);
            let ack = self.checkpoint()?;
            if self.store.durable() {
                self.store.fence_recovery(ack.generation)?;
            }
            return Ok(ack);
        }
        let photo = self.session.photos.iter().find(|photo| &photo.id == id).ok_or_else(|| invalid("Unknown removal member"))?;
        if photo.unknown_operation {
            return Err(invalid("Reconcile unknown operations before removal"));
        }
        if self.store.durable() {
            self.store.record_removal(id.clone())?;
        }
        self.changed()?;
        self.session.removed.insert(id.clone());
        self.session.photos.retain(|photo| &photo.id != id);
        self.session.sources.remove(id);
        self.drop_source_ticket(id);
        let ack = self.checkpoint()?;
        if self.store.durable() {
            self.store.fence_recovery(ack.generation)?;
        }
        Ok(ack)
    }

    fn reconcile_journals(&mut self) -> Result<Vec<ResumeIssue>, StoreError> {
        let mut operations = self.store.operation_ids().into_iter().map(|id| self.store.read_operation(&id)).collect::<Result<Vec<_>, _>>()?;
        operations.sort_by_key(|operation| (operation.plan.epoch, operation.plan.generation));
        for operation in &mut operations {
            if !operation.uncertain_photos.is_empty() || operation.outcomes.values().any(|outcome| matches!(outcome, Outcome::Pending | Outcome::Unknown(_))) {
                *operation = self.store.reconcile_operation(&operation.plan.id)?;
            }
        }
        self.overlay_operations(&operations)
    }

    fn overlay_operation(&mut self, operation: &Operation) -> Result<Vec<ResumeIssue>, StoreError> {
        let mut operations = self
            .store
            .operation_ids()
            .into_iter()
            .filter(|id| id != &operation.plan.id)
            .map(|id| self.store.read_operation(&id))
            .collect::<Result<Vec<_>, _>>()?;
        operations.push(operation.clone());
        operations.sort_by_key(|op| (op.plan.epoch, op.plan.generation));
        self.overlay_operations(&operations)
    }

    fn overlay_operations(&mut self, operations: &[Operation]) -> Result<Vec<ResumeIssue>, StoreError> {
        let original = self.session.clone();
        match self.overlay_operations_inner(operations) {
            Ok(issues) => Ok(issues),
            Err(error) => {
                self.session = original;
                Err(error)
            }
        }
    }

    fn overlay_operations_inner(&mut self, operations: &[Operation]) -> Result<Vec<ResumeIssue>, StoreError> {
        let mut issues = Vec::new();
        for operation in operations {
            for planned in &operation.plan.photos {
                if self.session.removed.contains(&planned.photo) {
                    if operation.uncertain_photos.contains(&planned.photo)
                        || matches!(operation.outcomes.get(&planned.photo), Some(Outcome::Unknown(_)))
                        || (operation.outcomes.get(&planned.photo) == Some(&Outcome::Pending) && !operation.confirmed_unwritten.contains(&planned.photo))
                    {
                        return Err(invalid("Removed member has an unresolved journal operation"));
                    }
                } else if !self.session.photos.iter().any(|photo| photo.id == planned.photo) {
                    return Err(invalid("Journal member missing from recovered snapshot"));
                }
            }
        }
        for photo in &mut self.session.photos {
            let rows: Vec<_> = operations
                .iter()
                .flat_map(|operation| operation.plan.photos.iter().filter(|planned| planned.photo == photo.id).map(move |planned| (operation, planned)))
                .collect();
            if rows.is_empty() {
                if photo.unknown_operation {
                    return Err(invalid("Unknown publication has no retained journal evidence"));
                }
                continue;
            }
            let binding = self.session.sources.get_mut(&photo.id).ok_or_else(|| invalid("Journal member has no source binding"))?;
            if rows.iter().any(|(_, planned)| !same_source(binding, &planned.binding) || binding.target != planned.binding.target) {
                return Err(invalid("Journal source or target differs from retained recovery identity"));
            }
            let successes: Vec<_> = rows
                .iter()
                .filter(|(operation, planned)| operation.outcomes.get(&planned.photo) == Some(&Outcome::Success))
                .map(|(_, planned)| *planned)
                .collect();
            for pair in successes.windows(2) {
                let (Some(before), Some(after)) = (pair.first(), pair.get(1)) else {
                    return Err(invalid("Invalid journal chain"));
                };
                if after.binding.original.as_ref() != Some(&before.intended) {
                    return Err(invalid("Successful journal packets do not form a continuous baseline chain"));
                }
            }
            if let (Some(first), Some(last)) = (successes.first(), successes.last()) {
                let at_before = *binding == first.binding;
                let acknowledged = successes.iter().find(|planned| binding.original.as_ref() == Some(&planned.intended));
                if !at_before && !acknowledged.is_some_and(|planned| photo.committed.as_ref() == Some(&planned.recipe)) {
                    return Err(invalid("Recovered baseline is not on the acknowledged journal chain"));
                }
                photo.committed = Some(last.recipe.clone());
                // Replaying historical acknowledged success after a resume does
                // not change Cancel's entry baseline for this visit.
                photo.committed_this_visit |= rows.iter().any(|(operation, planned)| {
                    operation.plan.epoch == self.session.epoch && operation.outcomes.get(&planned.photo) == Some(&Outcome::Success)
                });
                if photo.revision < last.revision {
                    photo.working = last.recipe.clone();
                    photo.revision = last.revision;
                }
                binding.original = Some(last.intended.clone());
                match commit::bind(&binding.source) {
                    Ok(current) if same_source(&current, binding) && current.target == binding.target && current.original == binding.original => {
                        *binding = current
                    }
                    Ok(_) => issues.push(ResumeIssue {
                        photo: Some(photo.id.clone()),
                        kind: ResumeIssueKind::SidecarChanged,
                        message: "Source or sidecar changed after acknowledged XMP publication".into(),
                    }),
                    Err(error) => {
                        issues.push(ResumeIssue { photo: Some(photo.id.clone()), kind: ResumeIssueKind::SourceUnavailable, message: error.to_string() })
                    }
                }
            }
            photo.unknown_operation = false;
            for (operation, planned) in rows {
                let outcome = operation.outcomes.get(&photo.id).ok_or_else(|| invalid("Journal outcome missing"))?;
                if !matches!(outcome, Outcome::Success) {
                    photo.unknown_operation |= operation.uncertain_photos.contains(&photo.id)
                        || matches!(outcome, Outcome::Unknown(_))
                        || (matches!(outcome, Outcome::Pending) && !operation.confirmed_unwritten.contains(&photo.id));
                    if photo.revision < planned.revision {
                        photo.working = planned.recipe.clone();
                        photo.revision = planned.revision;
                    }
                    // The same rule as completion: a definite failure for a photo with
                    // nothing left to save is not an open obligation.
                    if !commit::failure_is_moot_for(photo, Some(&*binding), planned, outcome) {
                        issues.push(ResumeIssue {
                            photo: Some(photo.id.clone()),
                            kind: ResumeIssueKind::OperationUnresolved,
                            message: format!("Unresolved XMP operation {}: {outcome:?}", operation.plan.id),
                        });
                    }
                }
            }
        }
        Ok(issues)
    }
    fn validate_sources(&mut self) -> Vec<ResumeIssue> {
        let mut issues = Vec::new();
        for photo in &mut self.session.photos {
            let mut migrated = None;
            let issue = match self.session.sources.get(&photo.id) {
                None => Some((ResumeIssueKind::SourceUnavailable, "Retained source binding is missing".into())),
                Some(binding) => match commit::bind(&binding.source) {
                    Err(error) => Some((ResumeIssueKind::SourceUnavailable, error.to_string())),
                    Ok(current) if !same_source(&current, binding) => {
                        Some((ResumeIssueKind::SourceChanged, "Retained source has changed physical identity or contents".into()))
                    }
                    // An earlier build's name for a sidecar never written: nothing changed.
                    Ok(current) if current != *binding && commit::legacy_unwritten_rendered(binding, &current) => {
                        migrated = Some(current);
                        None
                    }
                    Ok(current) if current != *binding => {
                        Some((ResumeIssueKind::SidecarChanged, "Sidecar binding or contents changed outside the recovered baseline".into()))
                    }
                    Ok(_) => None,
                },
            };
            if let Some(current) = migrated {
                self.session.sources.insert(photo.id.clone(), current);
            }
            if let Some((kind, message)) = issue {
                self.session.requested.insert(format!("source-{}", photo.id.0), Admission::Unresolved(message.clone()));
                issues.push(ResumeIssue { photo: Some(photo.id.clone()), kind, message });
            }
        }
        issues
    }
}

fn same_source(a: &SourceBinding, b: &SourceBinding) -> bool {
    a.source == b.source && a.source_fingerprint == b.source_fingerprint && a.source_identity == b.source_identity
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        bytes: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(identity("pc-controller-test").unwrap());
            std::fs::create_dir_all(root.join("inputs")).unwrap();
            let source = root.join("inputs/photo.NEF");
            let bytes = photocraft_raw::testgen::tiff_ep("NIKON CORPORATION", 16, 16, &[1024; 256], [0, 1, 1, 2], 12, vec![]);
            std::fs::write(&source, &bytes).unwrap();
            Self { root, source, bytes }
        }

        fn cache(&self) -> PathBuf {
            self.root.join("recovery")
        }

        fn admitted(&self) -> (Controller, PhotoId) {
            let mut controller = Controller::create(&self.cache()).unwrap();
            controller.request("first".into()).unwrap();
            let report = controller.admit("first", &self.source, &self.bytes).unwrap();
            (controller, report.id)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn edit(controller: &mut Controller, id: &PhotoId, exposure: f32) {
        let mut recipe = controller.session.photos.iter().find(|photo| &photo.id == id).unwrap().working.clone();
        recipe.adjustments.exposure = exposure;
        controller.apply(vec![(id.clone(), recipe)]).unwrap();
    }

    #[test]
    fn memory_mode_has_no_disk_writes_until_explicit_done() {
        let fixture = Fixture::new();
        let mut c = Controller::create_without_recovery(&fixture.cache()).unwrap();
        c.request("first".into()).unwrap();
        let id = c.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        edit(&mut c, &id, 0.8);
        let ack = c.checkpoint().unwrap();
        assert!(!ack.durable);
        assert!(c.session.cached_generation.is_none());
        assert!(!fixture.cache().exists());
        assert!(!fixture.source.with_extension("xmp").exists());
        let plan = c.prepare_done().unwrap();
        let approved = plan.id.clone();
        assert!(!fixture.source.with_extension("xmp").exists());
        assert!(c.commit_done(plan, &approved).unwrap().completed);
        let packet = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        assert_eq!(super::super::xmp::load_native(&packet).unwrap().unwrap().adjustments.exposure, 0.8);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[test]
    fn memory_mode_discard_and_opt_in_preserve_source_and_open_recipe() {
        let fixture = Fixture::new();
        let mut c = Controller::create_without_recovery(&fixture.cache()).unwrap();
        c.request("first".into()).unwrap();
        let id = c.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        edit(&mut c, &id, 0.6);
        assert!(c.keep().is_err());
        c.enable_recovery().unwrap();
        assert!(c.checkpoint().unwrap().durable);
        c.disable_recovery().unwrap();
        assert_eq!(c.session.photos[0].working.adjustments.exposure, 0.6);
        assert!(!c.checkpoint().unwrap().durable);
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty());
        c.discard_visit().unwrap();
        assert!(!fixture.source.with_extension("xmp").exists());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[test]
    fn failed_opt_in_keeps_deferred_ownership_and_memory_editing() {
        let fixture = Fixture::new();
        let mut c = Controller::create_without_recovery(&fixture.cache()).unwrap();
        c.request("first".into()).unwrap();
        let id = c.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        edit(&mut c, &id, 0.8);
        // Fail snapshot validation after the disk authority has been claimed.
        c.session.schema = u32::MAX;
        assert!(c.enable_recovery().is_err());
        assert!(!c.store.durable());
        assert_eq!(c.session.epoch, 0);
        assert_eq!(c.session.cached_generation, None);
        let authority = fixture.cache().join(&c.session.id).join("authority.json");
        let failed_authority = std::fs::read(&authority).unwrap();
        c.session.schema = super::super::SCHEMA_VERSION;
        edit(&mut c, &id, 0.9);
        assert!(!c.checkpoint().unwrap().durable);
        assert_eq!(std::fs::read(&authority).unwrap(), failed_authority);
        c.enable_recovery().unwrap();
        assert!(c.checkpoint().unwrap().durable);
        assert_eq!(c.session.photos[0].working.adjustments.exposure, 0.9);
        assert!(!fixture.source.with_extension("xmp").exists());
    }

    #[test]
    fn removed_confirmed_failure_allows_opt_out_and_reentry_without_resurrection() {
        let fixture = Fixture::new();
        let (mut c, id) = fixture.admitted();
        edit(&mut c, &id, 0.5);
        c.set_sidecar_publisher(Some(std::sync::Arc::new(|_, _| Err(std::io::Error::other("controlled publication failure")))));
        let plan = c.prepare_save().unwrap();
        let approved = plan.id.clone();
        assert!(!c.commit_save(plan, &approved).unwrap().completed);
        c.remove_discard(&id).unwrap();
        c.disable_recovery().unwrap();
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty());
        c.enable_recovery().unwrap();
        assert!(c.session.photos.is_empty());
        assert!(c.session.removed.contains(&id));
        assert!(c.store.load().unwrap().unwrap().session.photos.is_empty());
        assert!(!fixture.source.with_extension("xmp").exists());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[test]
    fn disabling_recovery_refuses_an_unresolved_publication_journal() {
        let fixture = Fixture::new();
        let (mut c, id) = fixture.admitted();
        edit(&mut c, &id, 0.5);
        c.set_sidecar_publisher(Some(std::sync::Arc::new(|_, _| Err(std::io::Error::other("controlled publication failure")))));
        let plan = c.prepare_save().unwrap();
        let approved = plan.id.clone();
        assert!(!c.commit_save(plan, &approved).unwrap().completed);
        let session = c.session.id.clone();
        let authority = fixture.cache().join(&session).join("authority.json");
        let before = std::fs::read(&authority).unwrap();
        assert!(c.disable_recovery().is_err());
        assert_eq!(c.session.id, session);
        assert_eq!(std::fs::read(&authority).unwrap(), before);
        let mut recipe = c.session.photos[0].working.clone();
        recipe.adjustments.exposure = 0.7;
        assert!(!c.sync_edits_with_recovery(vec![(id, recipe)], vec![], serde_json::Value::Null, false).unwrap().durable);
        assert_eq!(std::fs::read(&authority).unwrap(), before, "off-mode edits must not extend an explicit-save journal");
        assert!(!fixture.source.with_extension("xmp").exists());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[test]
    fn production_admission_verifies_bytes_and_coalesces_physical_duplicates() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        controller.request("again".into()).unwrap();
        let duplicate = controller.admit("again", &fixture.source, &fixture.bytes).unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.id, id);
        assert_eq!(controller.session.photos.len(), 1);
        controller.request("wrong-bytes".into()).unwrap();
        assert!(controller.admit("wrong-bytes", &fixture.source, b"substituted bytes").is_err());
        assert!(matches!(controller.session.requested.get("wrong-bytes"), Some(Admission::Unresolved(_))));
        assert!(controller.prepare_done().is_err());
        controller.cancel_request("wrong-bytes").unwrap();
        assert!(controller.prepare_done().is_ok());
        assert!(controller.request("too-late".into()).is_err());
    }

    #[test]
    fn done_saves_hidden_unselected_recipe_and_native_fresh_open_uses_it() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.7853281);
        controller.update_view(vec![(id.clone(), true, false)]).unwrap();
        let expected = controller.session.photos[0].working.clone();
        let plan = controller.prepare_done().unwrap();
        assert_eq!(plan.photos.len(), 1);
        assert!(plan.photos[0].hidden);
        let consent = plan.id.clone();
        let report = controller.commit_done(plan, &consent).unwrap();
        assert!(report.completed, "{:?}", report.completion_error);
        assert_eq!(report.operation.unwrap().outcomes.get(&id), Some(&Outcome::Success));
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty());
        drop(controller);
        let (fresh, _) = fixture.admitted();
        assert_eq!(fresh.session.photos[0].working, expected);
        assert_eq!(fresh.session.photos[0].pending(), Ok(false));
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[test]
    fn no_op_done_retires_without_creating_a_sidecar() {
        let fixture = Fixture::new();
        let (mut controller, _) = fixture.admitted();
        let plan = controller.prepare_done().unwrap();
        assert!(plan.photos.is_empty());
        let consent = plan.id.clone();
        let report = controller.commit_done(plan, &consent).unwrap();
        assert!(report.completed);
        assert!(report.operation.is_none());
        assert!(!fixture.source.with_extension("xmp").exists());
    }

    #[test]
    fn save_edits_stays_open_then_cancel_preserves_only_the_saved_recipe() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.5);
        let plan = controller.prepare_save().unwrap();
        let approved = plan.id.clone();
        assert!(controller.commit_done(plan.clone(), &approved).is_err());
        assert!(controller.commit_save(plan, &approved).unwrap().completed);
        assert_eq!(controller.session.phase, Phase::Editing);
        assert_eq!(controller.session.photos[0].pending(), Ok(false));
        let saved = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        edit(&mut controller, &id, 0.75);
        assert_eq!(controller.session.photos[0].pending(), Ok(true));
        controller.discard_visit().unwrap();
        assert_eq!(std::fs::read(fixture.source.with_extension("xmp")).unwrap(), saved);
        let (fresh, _) = fixture.admitted();
        assert_eq!(fresh.session.photos[0].working.adjustments.exposure, 0.5);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[test]
    fn repeated_save_noop_and_reset_to_default_have_exact_baselines() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        for exposure in [1.25, 0.0] {
            edit(&mut controller, &id, exposure);
            let plan = controller.prepare_save().unwrap();
            let approved = plan.id.clone();
            assert!(controller.commit_save(plan, &approved).unwrap().completed);
            let saved = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
            assert_eq!(super::super::xmp::load_native(&saved).unwrap().unwrap().adjustments.exposure, exposure);
            let plan = controller.prepare_save().unwrap();
            assert!(plan.photos.is_empty());
            let approved = plan.id.clone();
            let report = controller.commit_save(plan, &approved).unwrap();
            assert!(report.completed);
            assert!(report.operation.is_none());
            assert_eq!(controller.session.phase, Phase::Editing);
            assert_eq!(std::fs::read(fixture.source.with_extension("xmp")).unwrap(), saved);
        }
        edit(&mut controller, &id, -0.75);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        assert!(controller.commit_save(plan.clone(), &approved).is_err());
        assert!(controller.commit_done(plan, &approved).unwrap().completed);
        assert_eq!(controller.session.phase, Phase::Completed);
        let (fresh, _) = fixture.admitted();
        assert_eq!(fresh.session.photos[0].working.adjustments.exposure, -0.75);
    }

    /// The current contract preserves the acknowledged entry draft when a resumed
    /// visit is cancelled. Later uncommitted edits are discarded without XMP writes.
    #[test]
    fn resumed_cancel_preserves_immutable_entry_without_xmp() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let session_id = controller.session.id.clone();
        controller.keep().unwrap();
        drop(controller);
        let (mut resumed, report) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert!(report.issues.is_empty());
        assert_eq!(resumed.session.photos[0].pending(), Ok(true));
        edit(&mut resumed, &id, 2.0);
        resumed.discard_visit().unwrap();
        assert_eq!(resumed.session.phase, Phase::Paused);
        assert_eq!(resumed.take_cleanup_warning(), None);
        drop(resumed);
        assert!(fixture.cache().join(&session_id).exists());
        let (again, _) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert_eq!(again.session.photos[0].working.adjustments.exposure, 1.0);
        drop(again);
        assert_eq!(super::super::store::discover(&fixture.cache()).unwrap().len(), 1);
        assert!(!fixture.source.with_extension("xmp").exists());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[test]
    fn lifecycle_resumed_opt_out_preserves_entry_until_explicit_disposition() {
        for done in [false, true] {
            let fixture = Fixture::new();
            let (mut c, id) = fixture.admitted();
            edit(&mut c, &id, 1.0);
            let session = c.session.id.clone();
            c.keep().unwrap();
            drop(c);
            let (mut c, _) = Controller::resume(&fixture.cache(), &session).unwrap();
            c.disable_recovery().unwrap();
            let authority = fixture.cache().join(&session).join("authority.json");
            let before = std::fs::read(&authority).unwrap();
            let generation = c.store.load().unwrap().unwrap().session.generation;
            edit(&mut c, &id, 2.0);
            assert!(!c.acknowledge_edits(false).unwrap().durable);
            assert_eq!(std::fs::read(&authority).unwrap(), before);
            assert_eq!(c.store.load().unwrap().unwrap().session.generation, generation);
            assert!(!fixture.source.with_extension("xmp").exists());
            if done {
                let plan = c.prepare_done().unwrap();
                let approved = plan.id.clone();
                assert!(c.commit_done(plan, &approved).unwrap().completed);
                assert!(!fixture.cache().join(&session).exists());
                let bytes = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
                assert_eq!(super::super::xmp::load_native(&bytes).unwrap().unwrap().adjustments.exposure, 2.0);
            } else {
                c.discard_visit().unwrap();
                drop(c);
                let (again, _) = Controller::resume(&fixture.cache(), &session).unwrap();
                assert_eq!(again.session.photos[0].working.adjustments.exposure, 1.0);
                drop(again);
                Controller::discard_recovered(&fixture.cache(), &session).unwrap();
                assert!(!fixture.cache().join(&session).exists());
                assert!(!fixture.source.with_extension("xmp").exists());
            }
            assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
        }
    }

    #[test]
    fn lifecycle_resumed_discard_overlays_commits_and_removals_not_new_unsaved_members() {
        let fixture = Fixture::new();
        let (mut c, first) = fixture.admitted();
        let mut ids = vec![first.clone()];
        for name in ["entry-second", "entry-third"] {
            let path = fixture.root.join(format!("inputs/{name}.NEF"));
            std::fs::write(&path, &fixture.bytes).unwrap();
            c.request(name.into()).unwrap();
            ids.push(c.admit(name, &path, &fixture.bytes).unwrap().id);
        }
        for id in &ids {
            edit(&mut c, id, 0.4);
        }
        let session = c.session.id.clone();
        c.keep().unwrap();
        drop(c);
        let (mut c, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        c.remove_discard(&ids[2]).unwrap();
        let added = fixture.root.join("inputs/visit-only.NEF");
        std::fs::write(&added, &fixture.bytes).unwrap();
        c.request("new".into()).unwrap();
        let new_id = c.admit("new", &added, &fixture.bytes).unwrap().id;
        edit(&mut c, &first, 2.0);
        edit(&mut c, &ids[1], 3.0);
        c.set_sidecar_publisher(Some(std::sync::Arc::new(|path, bytes| {
            if path.file_stem().and_then(|v| v.to_str()) == Some("entry-second") {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "deliberate member refusal"));
            }
            photocraft_format::atomic::atomic_write_outcome(path, bytes)
        })));
        let plan = c.prepare_save().unwrap();
        let approved = plan.id.clone();
        assert!(!c.commit_save(plan, &approved).unwrap().completed);
        let committed = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        edit(&mut c, &first, 2.5);
        edit(&mut c, &ids[1], 4.0);
        c.discard_visit().unwrap();
        drop(c);
        let (restored, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert_eq!(restored.session.photos.len(), 2);
        assert_eq!(restored.session.photos[0].working.adjustments.exposure, 2.0);
        assert_eq!(restored.session.photos[1].working.adjustments.exposure, 0.4);
        assert!(restored.session.removed.contains(&ids[2]));
        assert!(restored.session.removed.contains(&new_id));
        assert_eq!(std::fs::read(fixture.source.with_extension("xmp")).unwrap(), committed);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    /// A sidecar saved in an earlier visit stays saved when a resumed visit is cancelled:
    /// Cancel ends the session without rolling back or rewriting any XMP.
    #[test]
    fn historical_save_and_removal_survive_resumed_entry_restore() {
        let fixture = Fixture::new();
        let (mut controller, first) = fixture.admitted();
        let other = fixture.root.join("inputs/other.NEF");
        std::fs::write(&other, &fixture.bytes).unwrap();
        controller.request("second".into()).unwrap();
        let second = controller.admit("second", &other, &fixture.bytes).unwrap().id;
        edit(&mut controller, &first, 0.5);
        edit(&mut controller, &second, 0.25);
        let denied = other.with_extension("xmp");
        controller.set_sidecar_publisher(Some(std::sync::Arc::new(move |path, bytes| {
            if path.file_name() == denied.file_name() {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected second-target denial"));
            }
            photocraft_format::atomic::atomic_write_outcome(path, bytes)
        })));
        let plan = controller.prepare_save().unwrap();
        let approved = plan.id.clone();
        let report = controller.commit_save(plan, &approved).unwrap();
        assert!(!report.completed);
        let operation = report.operation.unwrap();
        assert_eq!(operation.outcomes.get(&first), Some(&Outcome::Success));
        assert!(matches!(operation.outcomes.get(&second), Some(Outcome::Failed(_))));
        assert!(!controller.store.operation_ids().is_empty(), "partial save must retain the old success journal");
        edit(&mut controller, &first, 0.75);
        controller.keep().unwrap();
        let session_id = controller.session.id.clone();
        drop(controller);
        let (mut resumed, report) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert!(report.issues.iter().any(|issue| issue.photo.as_ref() == Some(&second) && issue.kind == ResumeIssueKind::OperationUnresolved));
        assert!(!resumed.session.photos[0].committed_this_visit);
        edit(&mut resumed, &first, 0.9);
        edit(&mut resumed, &second, 1.0);
        let plan = resumed.prepare_remove(&second).unwrap();
        let approved = plan.id.clone();
        assert!(resumed.commit_remove(plan, &approved).unwrap().removed);
        assert!(!resumed.session.photos[0].committed_this_visit);
        let saved = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        assert_eq!(super::super::xmp::load_native(&saved).unwrap().unwrap().adjustments.exposure, 0.5);
        resumed.discard_visit().unwrap();
        assert_eq!(resumed.session.phase, Phase::Paused);
        drop(resumed);
        assert_eq!(std::fs::read(fixture.source.with_extension("xmp")).unwrap(), saved, "Cancel never rewrites a saved sidecar");
        let (again, _) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert_eq!(again.session.photos.len(), 1);
        assert_eq!(again.session.photos[0].working.adjustments.exposure, 0.75);
        assert!(fixture.cache().join(&session_id).exists());
    }

    #[test]
    fn removal_retry_resolves_only_its_member_and_retains_other_failed_work() {
        let fixture = Fixture::new();
        let (mut controller, first) = fixture.admitted();
        let mut ids = vec![first];
        for name in ["second", "third"] {
            let path = fixture.root.join(format!("inputs/{name}.NEF"));
            std::fs::write(&path, &fixture.bytes).unwrap();
            controller.request(name.into()).unwrap();
            ids.push(controller.admit(name, &path, &fixture.bytes).unwrap().id);
        }
        for id in &ids {
            edit(&mut controller, id, 0.5);
        }
        controller.set_sidecar_publisher(Some(std::sync::Arc::new(|path, bytes| {
            if matches!(path.file_stem().and_then(|stem| stem.to_str()), Some("second" | "third")) {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected member denial"));
            }
            photocraft_format::atomic::atomic_write_outcome(path, bytes)
        })));
        let plan = controller.prepare_save().unwrap();
        let approved = plan.id.clone();
        assert!(!controller.commit_save(plan, &approved).unwrap().completed);
        let first_packet = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        controller.set_sidecar_publisher(None);
        let plan = controller.prepare_remove(&ids[1]).unwrap();
        let approved = plan.id.clone();
        assert!(controller.commit_remove(plan, &approved).unwrap().removed);
        assert_eq!(std::fs::read(fixture.source.with_extension("xmp")).unwrap(), first_packet);
        assert!(!fixture.root.join("inputs/third.xmp").exists());
        let session = controller.session.id.clone();
        controller.keep().unwrap();
        drop(controller);
        let (resumed, report) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(!resumed.session.photos.iter().any(|photo| photo.id == ids[1]));
        assert!(report.issues.iter().any(|issue| issue.photo.as_ref() == Some(&ids[2])));
        let third = resumed.session.photos.iter().find(|photo| photo.id == ids[2]).unwrap();
        assert_eq!(third.pending(), Ok(true));
        assert_eq!(third.working.adjustments.exposure, 0.5);
        assert!(!resumed.store.operation_ids().is_empty());
    }

    #[test]
    fn rendered_sources_save_sidecars_without_rewriting_original_pixels_or_metadata() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
        for (format, sample, extension) in
            [(Format::Jpeg, SampleType::U8, "jpg"), (Format::Tiff, SampleType::U16, "tif"), (Format::Tiff, SampleType::F32, "tif")]
        {
            let mut fixture = Fixture::new();
            let mut image = Image::from_normalized(2, 2, ChannelLayout::Rgb, sample, &[0.25; 12])
                .unwrap()
                .with_icc(Some(photocraft_cms::Builtin::Srgb.profile().to_bytes().to_vec()));
            image.meta.xmp = Some(format!(
                "<r:RDF xmlns:r=\"{}\" xmlns:u=\"urn:preserve\"><r:Description u:unchanged=\"original metadata\"/></r:RDF>",
                photocraft_xmp::RDF_NS
            ));
            fixture.source = fixture.root.join(format!("inputs/rendered.{extension}"));
            fixture.bytes = photocraft_codecs::encode(&image, format, &EncodeOptions::default()).unwrap();
            std::fs::write(&fixture.source, &fixture.bytes).unwrap();
            let before = super::super::commit::source_fingerprint(&fixture.source).unwrap();
            let (mut controller, id) = fixture.admitted();
            edit(&mut controller, &id, 0.5);
            let plan = controller.prepare_save().unwrap();
            let approved = plan.id.clone();
            assert!(controller.commit_save(plan, &approved).unwrap().completed);
            edit(&mut controller, &id, 0.75);
            let plan = controller.prepare_done().unwrap();
            let approved = plan.id.clone();
            assert!(controller.commit_done(plan, &approved).unwrap().completed);
            drop(controller);
            assert_eq!(super::super::commit::source_fingerprint(&fixture.source).unwrap(), before);
            assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
            let (fresh, _) = fixture.admitted();
            assert_eq!(fresh.session.photos[0].working.adjustments.exposure, 0.75);
            assert_eq!(
                fresh.session.photos[0].working.processing.source_kind,
                if format == Format::Jpeg { crate::camera_raw_source::SourceKind::Jpeg } else { crate::camera_raw_source::SourceKind::Tiff }
            );
        }
    }

    #[test]
    fn fresh_discard_retires_only_its_own_draft() {
        let fixture = Fixture::new();
        let (mut older, old_id) = fixture.admitted();
        edit(&mut older, &old_id, 1.0);
        older.keep().unwrap();
        let older_session = older.session.id.clone();
        drop(older);
        let (mut newer, new_id) = fixture.admitted();
        edit(&mut newer, &new_id, 2.0);
        newer.discard_visit().unwrap();
        assert_eq!(newer.session.phase, Phase::Completed);
        let candidates = super::super::store::discover(&fixture.cache()).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, older_session);
    }

    #[test]
    fn interrupted_success_advances_exact_journal_recipe_and_preserves_newer_edits() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let plan = controller.prepare_done().unwrap();
        let consent = plan.id.clone();
        controller.store.commit_plan(plan, &consent).unwrap();
        // Emulate a persisted newer UI state before the journal acknowledgement
        // was delivered to its controller. It must never become the baseline.
        controller.session.phase = Phase::Editing;
        edit(&mut controller, &id, 2.0);
        controller.checkpoint().unwrap();
        let session_id = controller.session.id.clone();
        drop(controller);
        let (resumed, report) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        let photo = &resumed.session.photos[0];
        assert_eq!(photo.working.adjustments.exposure, 2.0);
        assert_eq!(photo.committed.as_ref().unwrap().adjustments.exposure, 1.0);
        assert!(photo.pending().unwrap());
    }

    #[test]
    fn substituted_source_stays_unresolved_with_the_saved_recipe() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        controller.keep().unwrap();
        let session_id = controller.session.id.clone();
        drop(controller);
        std::fs::rename(&fixture.source, fixture.root.join("inputs/retained.NEF")).unwrap();
        std::fs::write(&fixture.source, &fixture.bytes).unwrap();
        let (mut resumed, report) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert!(report.issues.iter().any(|issue| issue.kind == ResumeIssueKind::SourceChanged));
        assert_eq!(resumed.session.photos.len(), 1);
        assert_eq!(resumed.session.photos[0].working.adjustments.exposure, 1.0);
        assert!(resumed.prepare_done().is_err());
    }

    /// A completed discard can never be resurrected: when its records cannot be removed
    /// (here an unrecognized file blocks reclamation) the terminal authority remains, is
    /// reported once, is never listed or resumed, and is collected once unblocked.
    #[test]
    fn blocked_reclaim_keeps_a_terminal_record_that_is_never_offered() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        controller.keep().unwrap();
        let session_id = controller.session.id.clone();
        drop(controller);
        let (mut resumed, _) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        edit(&mut resumed, &id, 2.0);
        resumed.checkpoint().unwrap();
        let blocker = fixture.cache().join(&session_id).join("unrecognized-note.txt");
        std::fs::write(&blocker, b"not a recovery record").unwrap();
        drop(resumed);
        assert!(Controller::discard_recovered(&fixture.cache(), &session_id).unwrap().is_some_and(|w| w.contains("will not be offered again")));
        assert!(fixture.cache().join(&session_id).join("authority.json").exists());
        assert!(Controller::resume(&fixture.cache(), &session_id).is_err());
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty());
        assert_eq!(super::super::store::collect_completed(&fixture.cache()).len(), 1, "still blocked: reported, retained");
        std::fs::remove_file(&blocker).unwrap();
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        assert!(!fixture.cache().join(&session_id).exists());
    }

    #[test]
    fn pending_admission_survives_restart_and_requires_explicit_resolution() {
        let fixture = Fixture::new();
        let (mut controller, _) = fixture.admitted();
        controller.request("delayed.NEF".into()).unwrap();
        controller.keep().unwrap();
        let id = controller.session.id.clone();
        drop(controller);
        let (mut resumed, report) = Controller::resume(&fixture.cache(), &id).unwrap();
        assert!(report.issues.iter().any(|issue| issue.kind == ResumeIssueKind::AdmissionUnresolved));
        assert!(resumed.prepare_done().is_err());
        resumed.cancel_request("delayed.NEF").unwrap();
        assert!(resumed.prepare_done().is_ok());
    }

    #[test]
    fn invalid_view_in_sync_does_not_partially_apply_edits() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        let mut changed = controller.session.photos[0].working.clone();
        changed.adjustments.exposure = 1.0;
        assert!(controller.sync_edits(vec![(id, changed)], vec![(PhotoId("invalid".into()), false, true)], serde_json::Value::Null).is_err());
        assert_eq!(controller.session.photos[0].working.adjustments.exposure, 0.0);
    }

    #[test]
    fn save_and_remove_saves_only_the_chosen_member_and_fences_it() {
        let fixture = Fixture::new();
        let (mut controller, first) = fixture.admitted();
        let other = fixture.root.join("inputs/other.NEF");
        std::fs::write(&other, &fixture.bytes).unwrap();
        controller.request("second".into()).unwrap();
        let second = controller.admit("second", &other, &fixture.bytes).unwrap().id;
        edit(&mut controller, &first, 1.0);
        edit(&mut controller, &second, 2.0);
        let plan = controller.prepare_remove(&first).unwrap();
        assert_eq!(plan.photos.len(), 1);
        assert_eq!(plan.photos[0].photo, first);
        let consent = plan.id.clone();
        let report = controller.commit_remove(plan, &consent).unwrap();
        assert!(report.removed, "{:?}", report.error);
        assert!(fixture.source.with_extension("xmp").exists());
        assert!(!other.with_extension("xmp").exists());
        assert_eq!(controller.session.photos.len(), 1);
        assert_eq!(controller.session.photos[0].id, second);
        assert!(controller.session.photos[0].pending().unwrap());
        let session_id = controller.session.id.clone();
        drop(controller);
        let (resumed, _) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert_eq!(resumed.session.photos.len(), 1);
        assert!(resumed.session.removed.contains(&first));
    }

    #[test]
    fn forged_no_op_removal_cannot_bypass_approved_plan() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let mut plan = controller.prepare_remove(&id).unwrap();
        let consent = plan.id.clone();
        plan.photos.clear();
        assert!(controller.commit_remove(plan, &consent).is_err());
        assert_eq!(controller.session.photos.len(), 1);
        assert!(!fixture.source.with_extension("xmp").exists());
    }

    #[test]
    fn lost_publication_then_divergent_content_keeps_discard_guard() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let plan = controller.prepare_done().unwrap();
        let operation = Operation {
            schema: 2,
            outcomes: BTreeMap::from([(id.clone(), Outcome::Unknown("lost response".into()))]),
            uncertain_photos: BTreeSet::from([id.clone()]),
            confirmed_unwritten: BTreeSet::new(),
            plan,
        };
        controller.store.pin(operation.plan.generation).unwrap();
        controller.store.write_operation(&operation.plan.id, &operation).unwrap();
        std::fs::write(fixture.source.with_extension("xmp"), b"external divergent packet").unwrap();
        let session_id = controller.session.id.clone();
        drop(controller);
        let (mut resumed, report) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert!(report.issues.iter().any(|issue| issue.kind == ResumeIssueKind::OperationUnresolved));
        assert!(resumed.session.photos[0].unknown_operation);
        assert!(resumed.remove_discard(&id).is_err());
        assert!(resumed.discard_visit().is_err());
        assert!(resumed.keep().is_ok());
    }

    #[test]
    fn reconciled_expected_before_allows_explicit_discard_without_replay() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let plan = controller.prepare_done().unwrap();
        let operation = Operation {
            schema: 2,
            outcomes: BTreeMap::from([(id.clone(), Outcome::Pending)]),
            uncertain_photos: BTreeSet::from([id.clone()]),
            confirmed_unwritten: BTreeSet::new(),
            plan,
        };
        controller.store.pin(operation.plan.generation).unwrap();
        controller.store.write_operation(&operation.plan.id, &operation).unwrap();
        let session_id = controller.session.id.clone();
        drop(controller);
        let (mut resumed, _) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert!(!resumed.session.photos[0].unknown_operation);
        resumed.remove_discard(&id).unwrap();
        assert!(resumed.session.photos.is_empty());
        assert!(!fixture.source.with_extension("xmp").exists());
    }

    #[test]
    fn reviewed_retry_supersedes_known_failure_and_completes_in_one_decision() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let plan = controller.prepare_done().unwrap();
        let operation = Operation {
            schema: 2,
            outcomes: BTreeMap::from([(id.clone(), Outcome::Failed("injected before publication".into()))]),
            uncertain_photos: BTreeSet::new(),
            confirmed_unwritten: BTreeSet::new(),
            plan,
        };
        controller.store.pin(operation.plan.generation).unwrap();
        controller.store.write_operation(&operation.plan.id, &operation).unwrap();
        let session_id = controller.session.id.clone();
        drop(controller);
        let (mut resumed, _) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        let retry = resumed.prepare_done().unwrap();
        let consent = retry.id.clone();
        let report = resumed.commit_done(retry, &consent).unwrap();
        assert!(report.completed, "{:?}", report.completion_error);
        assert!(report.completion_error.is_none());
    }

    fn plain_xmp() -> Vec<u8> {
        format!("<r:RDF xmlns:r=\"{}\" xmlns:u=\"urn:unknown\"><r:Description u:keep=\"untouched\"/></r:RDF>", photocraft_xmp::RDF_NS).into_bytes()
    }

    #[cfg(windows)]
    #[test]
    fn case_alias_admission_coalesces_source_and_preserves_existing_sidecar_name() {
        let fixture = Fixture::new();
        let sidecar = fixture.source.with_file_name("PHOTO.XMP");
        std::fs::write(&sidecar, plain_xmp()).unwrap();
        let (mut controller, first) = fixture.admitted();
        controller.request("case-alias".into()).unwrap();
        let alias = fixture.source.with_file_name("PHOTO.nef");
        let duplicate = controller.admit("case-alias", &alias, &fixture.bytes).unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.id, first);
        assert_eq!(controller.session.photos.len(), 1);
        edit(&mut controller, &first, 0.75);
        let plan = controller.prepare_done().unwrap();
        assert_eq!(plan.photos[0].binding.target.file_name().unwrap(), "PHOTO.XMP");
        let approved = plan.id.clone();
        assert!(controller.commit_done(plan, &approved).unwrap().completed);
        let entries: Vec<_> = std::fs::read_dir(sidecar.parent().unwrap()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(entries.iter().filter(|name| name.to_string_lossy().eq_ignore_ascii_case("photo.xmp")).count(), 1);
        assert!(entries.iter().any(|name| name == "PHOTO.XMP"));
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[test]
    fn distinct_retained_sources_cannot_publish_to_the_same_sidecar() {
        let fixture = Fixture::new();
        let (mut controller, first) = fixture.admitted();
        let second_path = fixture.source.with_extension("DNG");
        std::fs::write(&second_path, &fixture.bytes).unwrap();
        controller.request("same-stem-other-source".into()).unwrap();
        let second = controller.admit("same-stem-other-source", &second_path, &fixture.bytes).unwrap().id;
        assert_ne!(first, second);
        edit(&mut controller, &first, 1.0);
        edit(&mut controller, &second, 2.0);
        let error = controller.prepare_done().unwrap_err();
        assert!(error.to_string().contains("share one sidecar"), "{error}");
        assert!(controller.store.operation_ids().is_empty());
        assert!(!fixture.source.with_extension("xmp").exists());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
        assert_eq!(std::fs::read(&second_path).unwrap(), fixture.bytes);
    }

    #[test]
    fn externally_deleted_bound_sidecar_is_not_recreated_by_stale_approved_plan() {
        let fixture = Fixture::new();
        let sidecar = fixture.source.with_extension("xmp");
        std::fs::write(&sidecar, plain_xmp()).unwrap();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        std::fs::remove_file(&sidecar).unwrap();
        let report = controller.commit_done(plan, &approved).unwrap();
        assert!(!report.completed);
        assert!(matches!(report.operation.unwrap().outcomes.get(&id), Some(Outcome::Conflict(_))));
        assert!(!sidecar.exists());
        assert!(controller.session.photos[0].pending().unwrap());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[cfg(windows)]
    #[test]
    fn unreadable_bound_sidecar_cannot_be_overwritten_after_consent() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let sidecar = fixture.source.with_extension("xmp");
        let original = plain_xmp();
        std::fs::write(&sidecar, &original).unwrap();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&sidecar).unwrap();
        let report = controller.commit_done(plan, &approved).unwrap();
        assert!(!report.completed);
        assert!(matches!(report.operation.unwrap().outcomes.get(&id), Some(Outcome::Conflict(_))));
        drop(held);
        assert_eq!(std::fs::read(&sidecar).unwrap(), original);
        assert!(controller.session.photos[0].pending().unwrap());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    #[cfg(windows)]
    #[test]
    fn xmp_success_then_denied_removal_retries_lifecycle_without_second_publication() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        let publications = std::sync::Arc::new(AtomicU64::new(0));
        let count = publications.clone();
        controller.set_sidecar_publisher(Some(std::sync::Arc::new(move |path, bytes| {
            count.fetch_add(1, Ordering::SeqCst);
            photocraft_format::atomic::atomic_write_outcome(path, bytes)
        })));
        edit(&mut controller, &id, 1.0);
        let plan = controller.prepare_remove(&id).unwrap();
        let approved = plan.id.clone();
        let authority = fixture.cache().join(&controller.session.id).join("authority.json");
        let mut held = None;
        let failed = controller.commit_remove_with_progress(plan, &approved, &mut |done, total| {
            if done == total {
                held = Some(std::fs::OpenOptions::new().read(true).share_mode(0).open(&authority).unwrap());
            }
        });
        assert!(failed.is_err());
        assert!(held.is_some());
        assert_eq!(publications.load(Ordering::SeqCst), 1);
        assert_eq!(controller.session.photos.len(), 1);
        assert!(!controller.session.removed.contains(&id));
        assert!(!controller.session.photos[0].pending().unwrap());
        assert_eq!(controller.session.photos[0].committed.as_ref().unwrap().adjustments.exposure, 1.0);
        let sidecar = fixture.source.with_extension("xmp");
        let saved = std::fs::read(&sidecar).unwrap();
        let saved_identity = commit::source_fingerprint(&sidecar).unwrap();
        drop(held);
        let retry = controller.prepare_remove(&id).unwrap();
        assert!(retry.photos.is_empty());
        let approved = retry.id.clone();
        let report = controller.commit_remove(retry, &approved).unwrap();
        assert!(report.removed);
        assert!(report.operation.is_none());
        assert_eq!(publications.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&sidecar).unwrap(), saved);
        assert_eq!(commit::source_fingerprint(&sidecar).unwrap(), saved_identity);
        let session = controller.session.id.clone();
        drop(controller);
        let (resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(resumed.session.photos.is_empty());
        assert!(resumed.session.removed.contains(&id));
    }

    #[cfg(windows)]
    #[test]
    fn denied_keep_cache_publication_preserves_edits_and_can_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let session_id = controller.session.id.clone();
        let authority = fixture.cache().join(&session_id).join("authority.json");
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(authority).unwrap();
        assert!(controller.keep().is_err(), "Real Windows sharing denial must prevent cache authority replacement");
        assert_eq!(controller.session.phase, Phase::Editing);
        assert_eq!(controller.session.photos[0].working.adjustments.exposure, 1.0);
        drop(held);
        edit(&mut controller, &id, 2.0);
        controller.keep().unwrap();
        drop(controller);
        let (resumed, _) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert_eq!(resumed.session.photos[0].working.adjustments.exposure, 2.0);
    }

    #[cfg(windows)]
    #[test]
    fn pending_discard_retries_after_real_denied_checkpoint_without_resurrection() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        // Inject the precise boundary after tombstone acknowledgement and before
        // the transition checkpoint; the following denial is a real OS handle.
        controller.store.record_removals(std::slice::from_ref(&id)).unwrap();
        controller.session.removed.insert(id);
        controller.session.photos.clear();
        controller.session.phase = Phase::Frozen;
        controller.pending_discard = true;
        let authority = fixture.cache().join(&controller.session.id).join("authority.json");
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(authority).unwrap();
        assert!(controller.discard_visit().is_err());
        assert!(controller.pending_discard());
        assert!(controller.session.photos.is_empty());
        drop(held);
        controller.discard_visit().unwrap();
        assert!(!controller.pending_discard());
        assert_eq!(controller.session.phase, Phase::Completed);
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn acknowledged_removal_retries_real_denied_checkpoint_without_recreating_member() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let session_id = controller.session.id.clone();
        // Reach the exact durable disposition boundary, then deny subsequent
        // authority replacement through a real Windows exclusive file handle.
        controller.store.record_removal(id.clone()).unwrap();
        controller.session.removed.insert(id.clone());
        controller.session.photos.clear();
        let authority = fixture.cache().join(&session_id).join("authority.json");
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(authority).unwrap();
        assert!(controller.remove_discard(&id).is_err());
        assert!(controller.session.photos.is_empty());
        drop(held);
        controller.remove_discard(&id).unwrap();
        drop(controller);
        let (resumed, _) = Controller::resume(&fixture.cache(), &session_id).unwrap();
        assert!(resumed.session.photos.is_empty());
        assert!(resumed.session.removed.contains(&id));
        assert!(!fixture.source.with_extension("xmp").exists());
    }

    #[test]
    fn protection_inventory_includes_other_owned_drafts_without_claiming_them() {
        let fixture = Fixture::new();
        let (mut older, _) = fixture.admitted();
        older.keep().unwrap();
        let old_epoch = older.session.epoch;
        let other_path = fixture.root.join("inputs/another.NEF");
        std::fs::write(&other_path, &fixture.bytes).unwrap();
        let mut current = Controller::create(&fixture.cache()).unwrap();
        current.request("current".into()).unwrap();
        current.admit("current", &other_path, &fixture.bytes).unwrap();
        current.checkpoint().unwrap();
        let current_epoch = current.session.epoch;
        let protected = super::super::store::protected_sources(&fixture.cache()).unwrap();
        assert_eq!(protected.len(), 2);
        assert_eq!(older.session.epoch, old_epoch);
        assert_eq!(current.session.epoch, current_epoch);
        std::fs::write(fixture.cache().join(&older.session.id).join("authority.json"), b"corrupt authority").unwrap();
        assert!(super::super::store::protected_sources(&fixture.cache()).is_err());
    }

    #[test]
    fn repeated_acknowledged_save_removals_retire_journals_and_old_pins() {
        let fixture = Fixture::new();
        let mut controller = Controller::create(&fixture.cache()).unwrap();
        for index in 0..40 {
            let key = format!("request-{index}");
            controller.request(key.clone()).unwrap();
            let report = controller.admit(&key, &fixture.source, &fixture.bytes).unwrap();
            edit(&mut controller, &report.id, if index % 2 == 0 { 1.0 } else { 2.0 });
            let plan = controller.prepare_remove(&report.id).unwrap();
            let approved = plan.id.clone();
            let mut progress = Vec::new();
            let result = controller.commit_remove_with_progress(plan, &approved, &mut |done, total| progress.push((done, total))).unwrap();
            assert!(result.removed);
            assert_eq!(progress, vec![(0, 1), (1, 1)]);
            assert!(controller.store.operation_ids().is_empty());
        }
        let dir = fixture.cache().join(&controller.session.id);
        let journals = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with("operation-")).count();
        assert_eq!(journals, 0);
        let outer: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("authority.json")).unwrap()).unwrap();
        let authority: serde_json::Value = serde_json::from_str(outer["payload"].as_str().unwrap()).unwrap();
        assert!(authority["pins"].as_array().unwrap().len() <= 2);
        let session = controller.session.id.clone();
        controller.keep().unwrap();
        drop(controller);
        let (resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(resumed.session.photos.is_empty());
        assert_eq!(resumed.session.removed.len(), 40);
    }

    #[test]
    fn excessive_request_text_and_sidecar_are_rejected_without_source_mutation() {
        let fixture = Fixture::new();
        let mut controller = Controller::create(&fixture.cache()).unwrap();
        for index in 0..32 {
            controller.request(format!("{index:04}{}", "x".repeat(32764))).unwrap();
        }
        assert!(controller.request("one-too-many".into()).is_err());
        assert_eq!(controller.session.requested.len(), 32);
        let sidecar = fixture.source.with_extension("xmp");
        let packet = large_valid_packet(super::super::store::MAX_BOUND_XMP_BYTES + 1);
        photocraft_xmp::XmpPacket::parse(&packet).unwrap();
        std::fs::write(&sidecar, &packet).unwrap();
        assert!(commit::bind(&fixture.source).is_err());
        assert_eq!(std::fs::read(&sidecar).unwrap(), packet);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    fn large_valid_packet(size: usize) -> Vec<u8> {
        let prefix = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:other="urn:photocraft:large-packet-test"><other:Data>"#;
        let suffix = b"</other:Data></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut bytes = prefix.to_vec();
        bytes.resize(size - suffix.len(), b'a');
        bytes.extend_from_slice(suffix);
        bytes
    }

    #[test]
    fn valid_near_limit_packet_rejects_oversized_planned_output_before_publication() {
        let fixture = Fixture::new();
        let packet = large_valid_packet(super::super::store::MAX_BOUND_XMP_BYTES - 100);
        photocraft_xmp::XmpPacket::parse(&packet).unwrap();
        let path = fixture.source.with_extension("xmp");
        std::fs::write(&path, &packet).unwrap();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let error = controller.prepare_done().unwrap_err();
        assert!(error.to_string().contains("Planned XMP exceeds"), "{error}");
        assert!(controller.store.operation_ids().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), packet);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
        assert!(controller.session.photos[0].pending().unwrap());
    }

    #[test]
    fn thousand_acknowledged_edits_keep_entry_and_bounded_checkpoint_files() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        for index in 0..1000 {
            edit(&mut controller, &id, if index % 2 == 0 { 1.0 } else { 2.0 });
            controller.checkpoint().unwrap();
        }
        let dir = fixture.cache().join(&controller.session.id);
        let generations =
            std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).filter(|entry| entry.file_name().to_string_lossy().starts_with("generation-")).count();
        assert!(generations <= 3, "latest, previous, and visit entry only; actual={generations}");
        let session = controller.session.id.clone();
        drop(controller);
        let (resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert_eq!(resumed.session.photos[0].working.adjustments.exposure, 2.0);
        assert_eq!(resumed.session.photos[0].committed.as_ref().unwrap().adjustments.exposure, 0.0);
        assert!(resumed.session.photos[0].pending().unwrap());
        assert!(!fixture.source.with_extension("xmp").exists());
    }

    #[test]
    fn exact_source_return_clears_only_its_recovery_ticket_and_keeps_sidecar_conflicts() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        controller.keep().unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        let moved = fixture.root.join("original-temporarily-unavailable.NEF");
        std::fs::rename(&fixture.source, &moved).unwrap();
        let (mut resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        let source_key = format!("source-{}", id.0);
        assert!(matches!(resumed.session.requested.get(&source_key), Some(Admission::Unresolved(_))));
        resumed.request("other-request".into()).unwrap();
        resumed.reject("other-request", "other failure".into()).unwrap();
        std::fs::rename(&moved, &fixture.source).unwrap();
        let sidecar = fixture.source.with_extension("xmp");
        std::fs::write(&sidecar, b"externally changed XMP").unwrap();
        assert!(resumed.resolve_source(&id).is_err());
        assert!(matches!(resumed.session.requested.get(&source_key), Some(Admission::Unresolved(_))));
        std::fs::remove_file(sidecar).unwrap();
        resumed.resolve_source(&id).unwrap();
        assert!(!resumed.session.requested.contains_key(&source_key));
        assert!(matches!(resumed.session.requested.get("other-request"), Some(Admission::Rejected(_))));
        assert_eq!(resumed.session.photos[0].working.adjustments.exposure, 1.0);
        assert_eq!(resumed.prepare_done().unwrap().photos.len(), 1);
    }

    #[test]
    #[ignore = "Dedicated child entry point; exercised by interrupted_publication_reconciles_after_exact_child_termination"]
    fn interrupted_commit_process_helper() {
        let root = PathBuf::from(std::env::var_os("PHOTOCRAFT_NATIVE_KILL_ROOT").unwrap());
        let session = std::env::var("PHOTOCRAFT_NATIVE_KILL_SESSION").unwrap();
        let (mut controller, _) = Controller::resume(&root, &session).unwrap();
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        controller.commit_done(plan, &approved).unwrap();
        panic!("Child reached completion without the requested boundary pause");
    }

    fn evidence_child(fixture: &Fixture, session: &str, test: &str) -> std::process::Command {
        use std::process::{Command, Stdio};
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--ignored", "--exact", test, "--nocapture"])
            .env("PHOTOCRAFT_NATIVE_KILL_ROOT", fixture.cache())
            .env("PHOTOCRAFT_NATIVE_KILL_SESSION", session)
            .env("PHOTOCRAFT_NATIVE_VERIFY_REPORT", fixture.root.join("reader-receipt.json"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        command
    }

    fn bounded_child_output(mut child: std::process::Child) -> std::process::Output {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("Child exceeded 30-second bound: {} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        }
        child.wait_with_output().unwrap()
    }

    #[cfg(windows)]
    fn wait_child_marker(child: &mut std::process::Child, marker: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !marker.exists() && std::time::Instant::now() < deadline && child.try_wait().unwrap().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !marker.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Child did not acknowledge marker {} within 30 seconds", marker.display());
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Dedicated terminal fault child; exercised by xmp_success_then_denied_terminal_retries_lifecycle_only"]
    fn terminal_retry_process_helper() {
        let root = PathBuf::from(std::env::var_os("PHOTOCRAFT_NATIVE_KILL_ROOT").unwrap());
        let session = std::env::var("PHOTOCRAFT_NATIVE_KILL_SESSION").unwrap();
        let (mut controller, _) = Controller::resume(&root, &session).unwrap();
        let publications = std::sync::Arc::new(AtomicU64::new(0));
        let count = publications.clone();
        controller.set_sidecar_publisher(Some(std::sync::Arc::new(move |path, bytes| {
            count.fetch_add(1, Ordering::SeqCst);
            photocraft_format::atomic::atomic_write_outcome(path, bytes)
        })));
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        let failed = controller.commit_done(plan, &approved).unwrap();
        assert!(!failed.completed);
        assert!(failed.completion_error.is_some());
        assert!(failed.operation.unwrap().outcomes.values().all(|outcome| *outcome == Outcome::Success));
        assert!(!controller.session.photos[0].pending().unwrap());
        assert_eq!(publications.load(Ordering::SeqCst), 1);
        let sidecar = controller.session.sources.values().next().unwrap().target.clone();
        let saved = std::fs::read(&sidecar).unwrap();
        let identity = commit::source_fingerprint(&sidecar).unwrap();
        let failure_marker = root.parent().unwrap().join("terminal-denied.marker");
        std::fs::write(failure_marker, b"XMP acknowledged; terminal publication failed").unwrap();
        let retry_marker = root.parent().unwrap().join("retry-terminal.marker");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !retry_marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(retry_marker.exists(), "Parent did not release the terminal retry");
        let retry = controller.prepare_done().unwrap();
        assert!(retry.photos.is_empty());
        let approved = retry.id.clone();
        let completed = controller.commit_done(retry, &approved).unwrap();
        assert!(completed.completed);
        assert!(completed.operation.is_none());
        assert_eq!(controller.session.phase, Phase::Completed);
        assert_eq!(publications.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&sidecar).unwrap(), saved);
        assert_eq!(commit::source_fingerprint(&sidecar).unwrap(), identity);
        assert!(super::super::store::discover(&root).unwrap().is_empty());
        std::fs::write(
            std::env::var_os("PHOTOCRAFT_NATIVE_VERIFY_REPORT").unwrap(),
            serde_json::json!({
                "status":"PASS", "writer_pid":std::process::id(), "xmp_publications":1, "terminal_failure_observed":true,
                "lifecycle_retry_only":true, "sidecar_identity_unchanged":true
            })
            .to_string(),
        )
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn xmp_success_then_denied_terminal_retries_lifecycle_only() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.25);
        controller.checkpoint().unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        let paused = fixture.root.join("terminal-paused.marker");
        let continuation = fixture.root.join("continue-terminal.marker");
        let mut child = evidence_child(&fixture, &session, "raw_session::controller::tests::terminal_retry_process_helper")
            .env("PHOTOCRAFT_NATIVE_KILL_STAGE", "before_terminal")
            .env("PHOTOCRAFT_NATIVE_KILL_MARKER", &paused)
            .env("PHOTOCRAFT_NATIVE_CONTINUE_MARKER", &continuation)
            .spawn()
            .unwrap();
        let writer_id = child.id();
        wait_child_marker(&mut child, &paused);
        assert_eq!(std::fs::read_to_string(&paused).unwrap(), format!("{writer_id}:before_terminal"));
        let authority = fixture.cache().join(&session).join("authority.json");
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&authority).unwrap();
        std::fs::write(&continuation, b"attempt terminal while denied").unwrap();
        wait_child_marker(&mut child, &fixture.root.join("terminal-denied.marker"));
        drop(held);
        std::fs::write(fixture.root.join("retry-terminal.marker"), b"retry lifecycle after denial released").unwrap();
        let output = bounded_child_output(child);
        assert!(output.status.success(), "{} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(fixture.root.join("reader-receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt["writer_pid"], writer_id);
        assert_eq!(receipt["xmp_publications"], 1);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
        assert!(Controller::resume(&fixture.cache(), &session).is_err());
        println!("NATIVE-TERMINAL-RETRY {receipt}");
    }

    #[test]
    #[ignore = "Fresh child reader and ownership contender; exercised by parent evidence tests"]
    fn fresh_recovery_process_helper() {
        let root = PathBuf::from(std::env::var_os("PHOTOCRAFT_NATIVE_KILL_ROOT").unwrap());
        let session = std::env::var("PHOTOCRAFT_NATIVE_KILL_SESSION").unwrap();
        let expected = std::env::var("PHOTOCRAFT_NATIVE_VERIFY_EXPECTATION").unwrap();
        let outcome = Controller::resume(&root, &session);
        match expected.as_str() {
            "owned" => assert!(matches!(outcome, Err(StoreError::Owned))),
            "terminal" => {
                assert!(outcome.is_err());
                assert!(super::super::store::discover(&root).unwrap().is_empty());
            }
            _ => {
                let (mut recovered, report) = outcome.unwrap();
                let photo = &recovered.session.photos[0];
                assert_eq!(photo.working.adjustments.exposure, 1.25);
                match expected.as_str() {
                    "clean" => {
                        assert!(!photo.pending().unwrap());
                        assert!(!photo.unknown_operation);
                        assert_eq!(photo.committed.as_ref().unwrap().adjustments.exposure, 1.25);
                        assert!(report.issues.is_empty(), "{:?}", report.issues);
                    }
                    "pending" => {
                        assert!(photo.pending().unwrap());
                        assert!(!photo.unknown_operation);
                        assert_eq!(photo.committed.as_ref().unwrap().adjustments.exposure, 0.0);
                    }
                    "divergent" | "unreadable" => {
                        assert!(photo.unknown_operation);
                        assert_eq!(photo.committed.as_ref().unwrap().adjustments.exposure, 0.0);
                        assert!(report.issues.iter().any(|issue| issue.kind == ResumeIssueKind::OperationUnresolved));
                        assert!(recovered.prepare_done().is_err());
                        assert!(recovered.discard_visit().is_err());
                    }
                    _ => panic!("Unknown reader expectation: {expected}"),
                }
            }
        }
        let receipt = serde_json::json!({"status":"PASS", "reader_pid":std::process::id(), "expectation":expected});
        std::fs::write(std::env::var_os("PHOTOCRAFT_NATIVE_VERIFY_REPORT").unwrap(), receipt.to_string()).unwrap();
    }

    #[test]
    fn concurrent_process_cannot_claim_or_mutate_live_owned_session() {
        let fixture = Fixture::new();
        let (mut owner, id) = fixture.admitted();
        edit(&mut owner, &id, 1.25);
        owner.checkpoint().unwrap();
        let authority = fixture.cache().join(&owner.session.id).join("authority.json");
        let original_authority = std::fs::read(&authority).unwrap();
        let original_epoch = owner.session.epoch;
        let child = evidence_child(&fixture, &owner.session.id, "raw_session::controller::tests::fresh_recovery_process_helper")
            .env("PHOTOCRAFT_NATIVE_VERIFY_EXPECTATION", "owned")
            .spawn()
            .unwrap();
        let contender = child.id();
        assert_ne!(contender, std::process::id());
        let output = bounded_child_output(child);
        assert!(output.status.success(), "{} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert_eq!(std::fs::read(&authority).unwrap(), original_authority);
        assert_eq!(owner.session.epoch, original_epoch);
        assert!(!fixture.source.with_extension("xmp").exists());
        owner.checkpoint().unwrap();
        println!(
            "NATIVE-OWNERSHIP {}",
            serde_json::json!({"status":"PASS", "owner_pid":std::process::id(), "contender_pid":contender, "authority_unchanged":true})
        );
    }

    #[test]
    fn interrupted_publication_reconciles_after_exact_child_termination() {
        let mut cases = vec![
            ("before_staging", "pending"),
            ("before_replacement", "pending"),
            ("after_replacement", "clean"),
            ("before_terminal", "clean"),
            ("after_terminal", "terminal"),
            ("after_replacement", "divergent"),
        ];
        if cfg!(windows) {
            cases.push(("after_replacement", "unreadable"));
        }
        for (stage, expectation) in cases {
            let fixture = Fixture::new();
            let (mut controller, id) = fixture.admitted();
            edit(&mut controller, &id, 1.25);
            controller.checkpoint().unwrap();
            let session = controller.session.id.clone();
            drop(controller);
            let marker = fixture.root.join("paused.marker");
            let mut command = evidence_child(&fixture, &session, "raw_session::controller::tests::interrupted_commit_process_helper");
            command.env("PHOTOCRAFT_NATIVE_KILL_STAGE", stage).env("PHOTOCRAFT_NATIVE_KILL_MARKER", &marker);
            let mut child = command.spawn().unwrap();
            let child_id = child.id();
            assert_ne!(child_id, std::process::id());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !marker.exists() && std::time::Instant::now() < deadline {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let observed = marker.exists();
            if observed {
                assert_eq!(std::fs::read_to_string(&marker).unwrap(), format!("{child_id}:{stage}"));
            }
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            assert!(
                observed,
                "Child {child_id} did not reach {stage}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!output.status.success());
            let sidecar = fixture.source.with_extension("xmp");
            let installed = matches!(stage, "after_replacement" | "before_terminal" | "after_terminal");
            assert_eq!(sidecar.exists(), installed);
            if expectation == "divergent" {
                std::fs::write(&sidecar, b"external metadata edit after interrupted publication").unwrap();
            }
            let before_resume = std::fs::read(&sidecar).ok();
            #[cfg(windows)]
            let unreadable = if expectation == "unreadable" {
                use std::os::windows::fs::OpenOptionsExt;
                Some(std::fs::OpenOptions::new().read(true).share_mode(0).open(&sidecar).unwrap())
            } else {
                None
            };
            let reader = evidence_child(&fixture, &session, "raw_session::controller::tests::fresh_recovery_process_helper")
                .env("PHOTOCRAFT_NATIVE_VERIFY_EXPECTATION", expectation)
                .spawn()
                .unwrap();
            let reader_id = reader.id();
            assert_ne!(reader_id, child_id);
            assert_ne!(reader_id, std::process::id());
            let read_output = bounded_child_output(reader);
            #[cfg(windows)]
            drop(unreadable);
            assert!(
                read_output.status.success(),
                "Reader {reader_id} at {stage}/{expectation}: {} {}",
                String::from_utf8_lossy(&read_output.stdout),
                String::from_utf8_lossy(&read_output.stderr)
            );
            let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(fixture.root.join("reader-receipt.json")).unwrap()).unwrap();
            assert_eq!(receipt["reader_pid"], reader_id);
            assert_eq!(std::fs::read(&sidecar).ok(), before_resume, "Recovery must never replay or roll back XMP");
            assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
            println!(
                "NATIVE-KILL {}",
                serde_json::json!({"status":"PASS","stage":stage,"expectation":expectation,"parent_pid":std::process::id(),"child_pid":child_id,"reader_pid":reader_id,
                "boundary_observed":true,"child_exit":output.status.code(),"installed":installed,"recovery_no_replay":true,
                "source_blake3":blake3::hash(&fixture.bytes).to_hex().to_string(),
                "xmp_before_and_after_recovery_blake3":before_resume.as_ref().map(|bytes|blake3::hash(bytes).to_hex().to_string())})
            );
        }
    }

    // ---- Candidate F: one terminal contract for Done, Open Images and Cancel ----------

    /// Session directories under the private root (the root's shared commit lock is not one).
    fn sessions(fixture: &Fixture) -> Vec<String> {
        std::fs::read_dir(fixture.cache())
            .map(|entries| entries.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    }

    /// B01/B08: Done with recovery on publishes, completes the session and promptly
    /// reclaims its private records; a fresh reader reloads the exact recipe from XMP.
    #[test]
    fn done_with_recovery_reclaims_the_session_and_leaves_nothing_to_recover() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.625);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        assert!(controller.commit_done(plan, &approved).unwrap().completed);
        assert!(controller.completed());
        assert_eq!(controller.take_cleanup_warning(), None);
        assert!(sessions(&fixture).is_empty(), "no private records remain: {:?}", sessions(&fixture));
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty());
        let packet = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        assert_eq!(super::super::xmp::load_native(&packet).unwrap().unwrap().adjustments.exposure, 0.625);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    /// B02: with recovery off, Done journals its publication only for the explicit save,
    /// and the journal is reclaimed afterwards: no draft, no chooser row.
    #[test]
    fn recovery_off_done_journal_is_reclaimed_and_never_offered() {
        let fixture = Fixture::new();
        let mut c = Controller::create_without_recovery(&fixture.cache()).unwrap();
        c.request("first".into()).unwrap();
        let id = c.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        edit(&mut c, &id, 0.3);
        assert!(sessions(&fixture).is_empty(), "editing writes nothing with recovery off");
        let plan = c.prepare_done().unwrap();
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty(), "an unpublished journal is not a recovery draft");
        let approved = plan.id.clone();
        assert!(c.commit_done(plan, &approved).unwrap().completed);
        assert!(sessions(&fixture).is_empty());
        let packet = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        assert_eq!(super::super::xmp::load_native(&packet).unwrap().unwrap().adjustments.exposure, 0.3);
    }

    /// B03: a semantic no-op Done writes no sidecar and still ends the session cleanly.
    #[test]
    fn no_op_done_writes_no_xmp_and_reclaims() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.4);
        edit(&mut controller, &id, 0.0);
        let plan = controller.prepare_done().unwrap();
        assert!(plan.photos.is_empty());
        let approved = plan.id.clone();
        let report = controller.commit_done(plan, &approved).unwrap();
        assert!(report.completed && report.operation.is_none());
        assert!(!fixture.source.with_extension("xmp").exists());
        assert!(sessions(&fixture).is_empty());
    }

    /// B04/B12: Open Images publishes the same reviewed plan first, then waits frozen for
    /// the editor handoff. Interrupted before the handoff finished, the session stays
    /// recoverable (with its saved baselines); once the host completes it, it is reclaimed.
    #[test]
    fn open_saves_xmp_before_handoff_and_completes_only_after_it() {
        for interrupted in [false, true] {
            let fixture = Fixture::new();
            let (mut controller, id) = fixture.admitted();
            edit(&mut controller, &id, 0.9);
            assert!(controller.complete_open().is_err(), "nothing is saved yet");
            let plan = controller.prepare_done().unwrap();
            let approved = plan.id.clone();
            let report = controller.commit_open_with_progress(plan, &approved, &mut |_, _| {}).unwrap();
            assert!(report.completed);
            assert!(!controller.completed(), "the handoff has not happened yet");
            assert!(controller.awaiting_handoff());
            assert_eq!(controller.session.phase, Phase::Frozen);
            let packet = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
            assert_eq!(super::super::xmp::load_native(&packet).unwrap().unwrap().adjustments.exposure, 0.9);
            assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty(), "live: not offered");
            if interrupted {
                drop(controller);
                let rows = super::super::store::discover(&fixture.cache()).unwrap();
                assert_eq!(rows.len(), 1, "an interrupted handoff is recoverable");
                assert_eq!(rows[0].unsaved_photos, Some(0), "its edits are already in the sidecar");
            } else {
                controller.complete_open().unwrap();
                assert!(controller.completed() && !controller.awaiting_handoff());
                assert!(sessions(&fixture).is_empty());
            }
        }
    }

    /// B05: a partial Open Images publication keeps the session open for editing, keeps
    /// the successful sidecar and never completes the handoff.
    #[test]
    fn partial_open_publication_keeps_the_session_and_its_successes() {
        let fixture = Fixture::new();
        let (mut controller, first) = fixture.admitted();
        let other = fixture.root.join("inputs/other.NEF");
        std::fs::write(&other, &fixture.bytes).unwrap();
        controller.request("second".into()).unwrap();
        let second = controller.admit("second", &other, &fixture.bytes).unwrap().id;
        edit(&mut controller, &first, 0.5);
        edit(&mut controller, &second, 0.25);
        let denied = other.with_extension("xmp");
        controller.set_sidecar_publisher(Some(std::sync::Arc::new(move |path, bytes| {
            if path.file_name() == denied.file_name() {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected denial"));
            }
            photocraft_format::atomic::atomic_write_outcome(path, bytes)
        })));
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        let report = controller.commit_open_with_progress(plan, &approved, &mut |_, _| {}).unwrap();
        assert!(!report.completed);
        let operation = report.operation.unwrap();
        assert_eq!(operation.outcomes.get(&first), Some(&Outcome::Success));
        assert!(matches!(operation.outcomes.get(&second), Some(Outcome::Failed(_))));
        assert!(controller.complete_open().is_err());
        assert!(!controller.completed() && !controller.awaiting_handoff());
        assert!(fixture.source.with_extension("xmp").exists());
        assert!(!other.with_extension("xmp").exists());
        drop(controller);
        assert_eq!(super::super::store::discover(&fixture.cache()).unwrap().len(), 1, "interrupted, it stays recoverable");
    }

    /// B07/B08: Cancel on a fresh dirty session writes nothing and leaves nothing behind,
    /// with recovery on and off.
    #[test]
    fn cancel_writes_no_xmp_and_leaves_no_recoverable_session() {
        for recovery in [true, false] {
            let fixture = Fixture::new();
            let mut c = if recovery { Controller::create(&fixture.cache()).unwrap() } else { Controller::create_without_recovery(&fixture.cache()).unwrap() };
            c.request("first".into()).unwrap();
            let id = c.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
            edit(&mut c, &id, 1.25);
            c.checkpoint().unwrap();
            c.discard_visit().unwrap();
            assert!(c.completed());
            assert!(sessions(&fixture).is_empty(), "recovery {recovery}: {:?}", sessions(&fixture));
            assert!(!fixture.source.with_extension("xmp").exists());
            assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
        }
    }

    /// B09: an opted-in session interrupted after an acknowledged checkpoint is offered,
    /// and only it; a recovery-off journal abandoned before consent is collected instead.
    #[test]
    fn only_interrupted_opt_in_sessions_are_offered_and_abandoned_journals_are_collected() {
        let fixture = Fixture::new();
        let (mut interrupted, id) = fixture.admitted();
        edit(&mut interrupted, &id, 0.7);
        interrupted.checkpoint().unwrap();
        let kept = interrupted.session.id.clone();
        drop(interrupted);
        let mut off = Controller::create_without_recovery(&fixture.cache()).unwrap();
        off.request("first".into()).unwrap();
        let off_id = off.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        edit(&mut off, &off_id, 0.2);
        off.prepare_done().unwrap();
        let abandoned = off.session.id.clone();
        drop(off);
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        assert_eq!(rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), vec![kept.clone()]);
        assert_eq!(rows[0].unsaved_photos, Some(1));
        assert!(rows[0].automatic);
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        assert!(!fixture.cache().join(&abandoned).exists(), "the unpublished recovery-off journal is reclaimed");
        assert!(fixture.cache().join(&kept).exists(), "the interrupted draft is retained");
        assert!(!fixture.source.with_extension("xmp").exists());
    }

    /// B11: a corrupt newest checkpoint is reported with the fallback's time, not hidden.
    #[test]
    fn discovery_reports_the_fallback_checkpoint_when_the_newest_is_corrupt() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.5);
        controller.checkpoint().unwrap();
        edit(&mut controller, &id, 0.6);
        let latest = controller.checkpoint().unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        std::fs::write(fixture.cache().join(&session).join(format!("generation-{}.json", latest.generation)), b"corrupt latest").unwrap();
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].problem.is_none());
        assert_eq!(rows[0].newer_unusable_time_ms, Some(latest.time_ms));
        assert!(rows[0].checkpoint_time_ms.is_some_and(|t| t <= latest.time_ms));
        let (resumed, report) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(report.used_fallback);
        assert_eq!(resumed.session.photos[0].working.adjustments.exposure, 0.5);
    }

    /// A recovery-off journal that recorded a publication is protected work: it is
    /// offered after an interruption and never collected automatically.
    #[test]
    fn interrupted_recovery_off_publication_is_offered_and_retained() {
        let fixture = Fixture::new();
        let mut off = Controller::create_without_recovery(&fixture.cache()).unwrap();
        off.request("first".into()).unwrap();
        let id = off.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        edit(&mut off, &id, 0.2);
        let plan = off.prepare_done().unwrap();
        let consent = plan.id.clone();
        // The journal records the publication; the process stops before acknowledging it.
        off.store.commit_plan(plan, &consent).unwrap();
        let session = off.session.id.clone();
        drop(off);
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, session);
        assert!(!rows[0].automatic && rows[0].operations == 1);
    }

    // ---- Candidate F: independent review findings (R-01 … R-09) -------------------------

    /// R-01 (review blocker): a recovery-off publication journal resumed while recovery is on
    /// becomes a recoverable draft as soon as a recovery checkpoint is written; it is never
    /// collected as an "unpublished journal" afterwards.
    #[test]
    fn resumed_journal_that_takes_recovery_checkpoints_is_a_draft_and_never_collected() {
        let fixture = Fixture::new();
        let mut off = Controller::create_without_recovery(&fixture.cache()).unwrap();
        off.request("first".into()).unwrap();
        let id = off.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        edit(&mut off, &id, 0.2);
        let plan = off.prepare_done().unwrap();
        let consent = plan.id.clone();
        off.store.commit_plan(plan, &consent).unwrap();
        let session = off.session.id.clone();
        drop(off);
        let (mut resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(resumed.store.operation_ids().is_empty(), "the resolved journal was retired on resume");
        edit(&mut resumed, &id, 0.45);
        assert!(resumed.acknowledge_edits(true).unwrap().durable);
        drop(resumed);
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].automatic, "recovery checkpoints made it a draft");
        let (restored, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert_eq!(restored.session.photos[0].working.adjustments.exposure, 0.45);
    }

    /// R-02 (review blocker): a JPEG beside a same-stem RAW binds to `<name>.<ext>.xmp`; it
    /// never reads, routes by or writes the RAW's `<stem>.xmp`. A RAW+JPEG pair saves two
    /// separate sidecars, and the RAW's existing packet is never rewritten by the JPEG.
    #[test]
    fn same_stem_raw_and_jpeg_save_to_their_own_sidecars() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        let fixture = Fixture::new();
        let (mut controller, raw) = fixture.admitted();
        let jpeg_path = fixture.root.join("inputs/photo.JPG");
        let image = Image::from_u8(4, 3, ChannelLayout::Rgb, [90, 110, 130].repeat(12)).unwrap();
        let jpeg_bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        std::fs::write(&jpeg_path, &jpeg_bytes).unwrap();
        let jpeg_binding = super::super::commit::bind(&jpeg_path).unwrap();
        assert!(jpeg_binding.target.file_name().unwrap().eq_ignore_ascii_case("photo.JPG.xmp"), "{:?}", jpeg_binding.target);
        assert!(super::super::commit::bind(&fixture.source).unwrap().target.file_name().unwrap().eq_ignore_ascii_case("photo.xmp"));
        controller.request("jpeg".into()).unwrap();
        let jpeg = controller.admit("jpeg", &jpeg_path, &jpeg_bytes).unwrap().id;
        edit(&mut controller, &raw, 0.5);
        edit(&mut controller, &jpeg, 0.25);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        assert!(controller.commit_done(plan, &approved).unwrap().completed);
        let raw_packet = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        let jpeg_packet = std::fs::read(fixture.root.join("inputs/photo.JPG.xmp")).unwrap();
        assert_eq!(super::super::xmp::load_native(&raw_packet).unwrap().unwrap().adjustments.exposure, 0.5);
        let jpeg_recipe = super::super::xmp::load_native(&jpeg_packet).unwrap().unwrap();
        assert_eq!(jpeg_recipe.adjustments.exposure, 0.25);
        assert_eq!(jpeg_recipe.processing.source_kind, crate::camera_raw_source::SourceKind::Jpeg);
        assert_eq!(std::fs::read(&jpeg_path).unwrap(), jpeg_bytes);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
    }

    /// R-02: with only the JPEG of a RAW+JPEG pair in a session, the RAW's existing sidecar is
    /// neither its baseline nor its target.
    #[test]
    fn a_lone_jpeg_never_adopts_its_raw_siblings_sidecar() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        let fixture = Fixture::new();
        let raw_sidecar = fixture.source.with_extension("xmp");
        let packet = format!(
            "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='{}' xmlns:crs='{}'><rdf:Description rdf:about='' crs:Exposure2012='1.5'/></rdf:RDF></x:xmpmeta>",
            photocraft_xmp::RDF_NS,
            photocraft_xmp::CAMERA_RAW_NS
        );
        std::fs::write(&raw_sidecar, &packet).unwrap();
        let jpeg_path = fixture.root.join("inputs/photo.jpg");
        let image = Image::from_u8(4, 3, ChannelLayout::Rgb, [70, 90, 110].repeat(12)).unwrap();
        let jpeg_bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        std::fs::write(&jpeg_path, &jpeg_bytes).unwrap();
        let mut controller = Controller::create(&fixture.cache()).unwrap();
        controller.request("jpeg".into()).unwrap();
        let report = controller.admit("jpeg", &jpeg_path, &jpeg_bytes).unwrap();
        assert_eq!(report.recipe.adjustments.exposure, 0.0, "the RAW's Exposure2012 is not the JPEG's setting");
        assert!(report.binding.original.is_none());
        edit(&mut controller, &report.id, 0.3);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        assert!(controller.commit_done(plan, &approved).unwrap().completed);
        assert_eq!(std::fs::read(&raw_sidecar).unwrap(), packet.as_bytes(), "the RAW's sidecar is untouched");
        assert!(fixture.root.join("inputs/photo.jpg.xmp").exists());
    }

    /// R-03 (review major): an earlier definite failure for a photo the photographer since
    /// reverted no longer blocks Done or Open Images; a conflict or an unknown outcome would.
    #[test]
    fn a_failed_save_for_a_reverted_photo_does_not_block_done_or_open() {
        for open in [false, true] {
            let fixture = Fixture::new();
            let (mut controller, first) = fixture.admitted();
            let other = fixture.root.join("inputs/other.NEF");
            std::fs::write(&other, &fixture.bytes).unwrap();
            controller.request("second".into()).unwrap();
            let second = controller.admit("second", &other, &fixture.bytes).unwrap().id;
            edit(&mut controller, &first, 0.5);
            edit(&mut controller, &second, 0.25);
            let denied = other.with_extension("xmp");
            controller.set_sidecar_publisher(Some(std::sync::Arc::new(move |path, bytes| {
                if path.file_name() == denied.file_name() {
                    return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected denial"));
                }
                photocraft_format::atomic::atomic_write_outcome(path, bytes)
            })));
            let plan = controller.prepare_done().unwrap();
            let approved = plan.id.clone();
            assert!(!controller.commit_done(plan, &approved).unwrap().completed);
            controller.cancel_finish().ok();
            controller.set_sidecar_publisher(None);
            edit(&mut controller, &second, 0.0);
            let plan = controller.prepare_done().unwrap();
            assert!(plan.photos.is_empty(), "nothing left to save");
            let approved = plan.id.clone();
            if open {
                assert!(controller.commit_open_with_progress(plan, &approved, &mut |_, _| {}).unwrap().completed);
                controller.complete_open().unwrap();
            } else {
                assert!(controller.commit_done(plan, &approved).unwrap().completed);
            }
            assert!(controller.completed());
            assert!(!other.with_extension("xmp").exists());
            assert!(sessions(&fixture).is_empty());
        }
    }

    /// R-04 (review major): a session a running PhotoCraft still owns is never offered.
    #[test]
    fn a_live_session_is_never_offered_as_interrupted() {
        let fixture = Fixture::new();
        let (mut live, id) = fixture.admitted();
        edit(&mut live, &id, 0.6);
        live.checkpoint().unwrap();
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty(), "owned by this live controller");
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        let session = live.session.id.clone();
        drop(live);
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        assert_eq!(rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), vec![session]);
    }

    /// R-05 (review major/minor): a reclaimed identity is never recreated; sessions with
    /// nothing to recover (no acknowledged checkpoint, or no retained photo after an
    /// interrupted Cancel) are neither offered nor block JPEG protection, and are collected.
    #[test]
    fn empty_and_reclaimed_sessions_are_never_offered_or_resurrected() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.cache().join("raw-residue")).unwrap();
        std::fs::write(fixture.cache().join("raw-residue/ownership.lock"), b"").unwrap();
        assert!(super::super::store::RecoveryStore::claim_existing(&fixture.cache(), "raw-residue").is_err());
        assert!(!fixture.cache().join("raw-residue/authority.json").exists(), "never recreated");
        drop(super::super::store::RecoveryStore::claim(&fixture.cache(), "raw-no-checkpoint").unwrap());
        let (mut cancelled, id) = fixture.admitted();
        cancelled.checkpoint().unwrap();
        cancelled.store.record_removals(&[id]).unwrap();
        let cancelled_id = cancelled.session.id.clone();
        drop(cancelled);
        assert!(super::super::store::discover(&fixture.cache()).unwrap().is_empty());
        assert!(super::super::store::protected_sources(&fixture.cache()).is_ok());
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        for gone in ["raw-residue", "raw-no-checkpoint", cancelled_id.as_str()] {
            assert!(!fixture.cache().join(gone).exists(), "{gone} was collected");
        }
    }

    /// R-07 (review minor): the chooser recognises a sidecar this session already saved
    /// after its last checkpoint (an interrupted Open Images) instead of calling it changed.
    #[test]
    fn card_facts_recognise_this_sessions_own_interrupted_save() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.8);
        let plan = controller.prepare_done().unwrap();
        let consent = plan.id.clone();
        controller.store.commit_plan(plan, &consent).unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        let detail = super::super::store::inspect_candidate(&fixture.cache(), &session).unwrap();
        assert_eq!(detail.photos[0].sidecar, super::super::store::SidecarState::SavedByThisSession);
        assert!(!detail.photos[0].unsaved, "its edit is in the sidecar");
        assert_eq!(detail.photos[0].source, super::super::store::SourceState::Available);
    }

    /// R-08 (review minor, restoring E's fence property): after a resume fences recovery at
    /// its entry checkpoint, a corrupt newer checkpoint falls back no further than that entry.
    #[test]
    fn a_corrupt_checkpoint_never_falls_back_past_the_resume_fence() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        controller.checkpoint().unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        let (mut resumed, report) = Controller::resume(&fixture.cache(), &session).unwrap();
        let fence = report.checkpoint.generation;
        edit(&mut resumed, &id, 2.0);
        let newer = resumed.checkpoint().unwrap();
        drop(resumed);
        let corrupt = |generation: u64| std::fs::write(fixture.cache().join(&session).join(format!("generation-{generation}.json")), b"corrupt").unwrap();
        corrupt(newer.generation);
        let (again, report) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(report.used_fallback);
        assert_eq!(again.session.photos[0].working.adjustments.exposure, 1.0, "the fenced entry, not an older pre-resume state");
        let entry = report.checkpoint.generation;
        drop(again);
        corrupt(entry);
        corrupt(fence);
        assert!(Controller::resume(&fixture.cache(), &session).is_err(), "nothing below the fence is restored");
    }

    /// Second review M1 (a data-loss major in 911a387): a draft whose newer checkpoints
    /// cannot be read right now (held open exclusively by antivirus or a sync client, or
    /// truncated) never looks empty by falling back to the empty entry checkpoint. It stays
    /// listed, is never collected, and once readable again resumes with its edits.
    #[test]
    fn an_unreadable_newest_checkpoint_never_makes_a_draft_look_empty() {
        for held in [false, true] {
            if held && !cfg!(windows) {
                continue;
            }
            let fixture = Fixture::new();
            let (mut controller, id) = fixture.admitted();
            edit(&mut controller, &id, 0.8);
            controller.checkpoint().unwrap();
            let session = controller.session.id.clone();
            drop(controller);
            let dir = fixture.cache().join(&session);
            let mut generations: Vec<(u64, PathBuf)> = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    let number = name.strip_prefix("generation-")?.strip_suffix(".json")?.parse().ok()?;
                    Some((number, entry.path()))
                })
                .collect();
            generations.sort();
            let envelope: serde_json::Value = serde_json::from_slice(&std::fs::read(&generations[0].1).unwrap()).unwrap();
            let entry = super::super::store::validate_snapshot(envelope["payload"].as_str().unwrap().as_bytes()).unwrap();
            assert!(entry.photos.is_empty(), "the oldest checkpoint is the empty entry");
            let newer: Vec<PathBuf> = generations[1..].iter().map(|(_, path)| path.clone()).collect();
            assert!(!newer.is_empty());
            let saved: Vec<Vec<u8>> = newer.iter().map(|path| std::fs::read(path).unwrap()).collect();
            #[cfg(windows)]
            let mut guards: Vec<std::fs::File> = Vec::new();
            #[cfg(not(windows))]
            let guards: Vec<std::fs::File> = Vec::new();
            for path in &newer {
                if held {
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::OpenOptionsExt;
                        guards.push(std::fs::OpenOptions::new().read(true).share_mode(0).open(path).unwrap());
                    }
                } else {
                    std::fs::write(path, b"truncated").unwrap();
                }
            }
            let rows = super::super::store::discover(&fixture.cache()).unwrap();
            assert!(rows.iter().any(|row| row.id == session), "held={held}: still offered: {rows:?}");
            assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
            assert!(dir.join("authority.json").exists() && newer.iter().all(|path| path.exists()), "held={held}: nothing was collected");
            drop(guards);
            for (path, bytes) in newer.iter().zip(&saved) {
                std::fs::write(path, bytes).unwrap();
            }
            let (resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
            assert_eq!(resumed.session.photos[0].working.adjustments.exposure, 0.8, "held={held}: the draft survived");
        }
    }

    /// Second review M3: a definite failure for a photo reverted since is skipped by the
    /// retry's supersede step and is not reported as an open operation, so ONE Done or Open
    /// Images completes. "mixed": both photos failed, one was reverted and the other saved
    /// by the retry; "fresh": the failed photo was reverted and another photo is saved.
    #[test]
    fn a_reverted_failure_and_a_later_success_complete_in_one_done_or_open() {
        for shape in ["mixed", "fresh"] {
            for open in [false, true] {
                let fixture = Fixture::new();
                let (mut controller, first) = fixture.admitted();
                let other = fixture.root.join("inputs/other.NEF");
                std::fs::write(&other, &fixture.bytes).unwrap();
                controller.request("second".into()).unwrap();
                let second = controller.admit("second", &other, &fixture.bytes).unwrap().id;
                if shape == "mixed" {
                    edit(&mut controller, &first, 0.5);
                }
                edit(&mut controller, &second, 0.25);
                controller
                    .set_sidecar_publisher(Some(std::sync::Arc::new(|_, _| Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected denial")))));
                let plan = controller.prepare_done().unwrap();
                let approved = plan.id.clone();
                let failed = controller.commit_done(plan, &approved).unwrap();
                assert!(!failed.completed);
                assert!(failed.operation.unwrap().outcomes.values().all(|outcome| matches!(outcome, Outcome::Failed(_))));
                controller.cancel_finish().ok();
                controller.set_sidecar_publisher(None);
                edit(&mut controller, &second, 0.0);
                if shape == "fresh" {
                    edit(&mut controller, &first, 0.5);
                }
                let plan = controller.prepare_done().unwrap();
                assert_eq!(plan.photos.len(), 1, "{shape}: only the first photo is left to save");
                let approved = plan.id.clone();
                let report = if open {
                    controller.commit_open_with_progress(plan, &approved, &mut |_, _| {}).unwrap()
                } else {
                    controller.commit_done(plan, &approved).unwrap()
                };
                assert!(report.completed, "{shape}/open={open}: one attempt completes: {:?}", report.completion_error);
                if open {
                    controller.complete_open().unwrap();
                }
                assert!(controller.completed());
                let saved = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
                assert_eq!(super::super::xmp::load_native(&saved).unwrap().unwrap().adjustments.exposure, 0.5);
                assert!(!other.with_extension("xmp").exists());
                assert!(sessions(&fixture).is_empty());
            }
        }
    }

    /// Second review M2: a conflict is never moot. With a conflicted, since-reverted photo
    /// Done still refuses, and Open Images refuses BEFORE any handoff (it never hands the
    /// photos to the editor and then fails its terminal record). Cancel still ends the
    /// session, and the sidecar keeps its bytes.
    #[cfg(windows)]
    #[test]
    fn a_conflict_still_blocks_completion_and_open_refuses_before_any_handoff() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let sidecar = fixture.source.with_extension("xmp");
        let original = plain_xmp();
        std::fs::write(&sidecar, &original).unwrap();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&sidecar).unwrap();
        let report = controller.commit_done(plan, &approved).unwrap();
        assert!(matches!(report.operation.unwrap().outcomes.get(&id), Some(Outcome::Conflict(_))));
        drop(held);
        controller.cancel_finish().ok();
        edit(&mut controller, &id, 0.0);
        for open in [true, false] {
            let plan = controller.prepare_done().unwrap();
            assert!(plan.photos.is_empty(), "nothing is left to save");
            let approved = plan.id.clone();
            let report = if open {
                controller.commit_open_with_progress(plan, &approved, &mut |_, _| {}).unwrap()
            } else {
                controller.commit_done(plan, &approved).unwrap()
            };
            assert!(!report.completed, "open={open}: the conflict still blocks completion");
            assert!(!controller.awaiting_handoff(), "open={open}: no photo is handed to the editor");
            assert!(controller.complete_open().is_err());
            controller.cancel_finish().ok();
        }
        controller.discard_visit().unwrap();
        assert!(controller.completed());
        assert_eq!(std::fs::read(&sidecar).unwrap(), original);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
        assert!(sessions(&fixture).is_empty());
    }

    /// Second review m4: who owns `<stem>.xmp` does not depend on decoder support. A JPEG
    /// beside an undecoded camera RAW (CRW, IIQ, 3FR…) never takes the RAW's Lightroom
    /// sidecar as its baseline or target.
    #[test]
    fn a_jpeg_beside_an_undecoded_raw_never_takes_its_stem_sidecar() {
        let fixture = Fixture::new();
        for raw in ["CRW", "iiq", "3fr", "srw"] {
            let dir = fixture.root.join(format!("inputs/{raw}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("photo.{raw}")), b"not decoded here").unwrap();
            std::fs::write(dir.join("photo.xmp"), plain_xmp()).unwrap();
            std::fs::write(dir.join("photo.jpg"), b"rendered photograph").unwrap();
            let binding = super::super::commit::bind(&dir.join("photo.jpg")).unwrap();
            assert!(binding.target.file_name().unwrap().eq_ignore_ascii_case("photo.jpg.xmp"), "{raw}: {:?}", binding.target);
            assert!(binding.original.is_none(), "{raw}: the RAW's sidecar is not the JPEG's baseline");
        }
    }

    /// Second review M2/M3 root cause: retries that save photos one at a time resolve the
    /// earlier failures. op1 fails X and Y; op2 saves X but fails Y; op3 saves Y. op1's X
    /// row is resolved by op2's later same-source success (not by op3), so that third save
    /// completes Done or Open Images at once instead of refusing forever.
    #[test]
    fn step_by_step_retries_resolve_earlier_failures_and_complete() {
        for open in [false, true] {
            let fixture = Fixture::new();
            let (mut controller, first) = fixture.admitted();
            let other = fixture.root.join("inputs/other.NEF");
            std::fs::write(&other, &fixture.bytes).unwrap();
            controller.request("second".into()).unwrap();
            let second = controller.admit("second", &other, &fixture.bytes).unwrap().id;
            edit(&mut controller, &first, 0.5);
            edit(&mut controller, &second, 0.25);
            let deny = |names: &'static [&'static str]| {
                Some(std::sync::Arc::new(move |path: &std::path::Path, bytes: &[u8]| {
                    if names.iter().any(|name| path.file_name().is_some_and(|file| file.eq_ignore_ascii_case(name))) {
                        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected denial"));
                    }
                    photocraft_format::atomic::atomic_write_outcome(path, bytes)
                }) as super::super::SidecarPublisher)
            };
            for denied in [&["photo.xmp", "other.xmp"][..], &["other.xmp"][..]] {
                controller.set_sidecar_publisher(deny(denied));
                let plan = controller.prepare_done().unwrap();
                let approved = plan.id.clone();
                assert!(!controller.commit_done(plan, &approved).unwrap().completed);
                controller.cancel_finish().ok();
            }
            controller.set_sidecar_publisher(None);
            let plan = controller.prepare_done().unwrap();
            assert_eq!(plan.photos.len(), 1, "only the second photo is left");
            let approved = plan.id.clone();
            let report = if open {
                controller.commit_open_with_progress(plan, &approved, &mut |_, _| {}).unwrap()
            } else {
                controller.commit_done(plan, &approved).unwrap()
            };
            assert!(report.completed, "open={open}: {:?}", report.completion_error);
            if open {
                controller.complete_open().unwrap();
            }
            assert!(controller.completed());
            for (path, exposure) in [(fixture.source.with_extension("xmp"), 0.5), (other.with_extension("xmp"), 0.25)] {
                let recipe = super::super::xmp::load_native(&std::fs::read(&path).unwrap()).unwrap().unwrap();
                assert_eq!(recipe.adjustments.exposure, exposure);
            }
            assert!(sessions(&fixture).is_empty());
        }
    }

    /// Sweep S1/S5 (pre-existing since E): a newer checkpoint that cannot be read RIGHT NOW
    /// (held exclusively by antivirus, backup or sync software) is never treated as corrupt.
    /// Resume refuses instead of restoring and fencing an older state over it, JPEG
    /// protection fails closed, and once the file is readable the draft resumes intact.
    #[cfg(windows)]
    #[test]
    fn a_checkpoint_busy_right_now_is_never_replaced_by_an_older_one() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.5);
        controller.checkpoint().unwrap();
        edit(&mut controller, &id, 0.6);
        let newest = controller.checkpoint().unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        let path = fixture.cache().join(&session).join(format!("generation-{}.json", newest.generation));
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        assert!(Controller::resume(&fixture.cache(), &session).is_err(), "no fallback over a busy newest checkpoint");
        assert!(super::super::store::protected_sources(&fixture.cache()).is_err(), "JPEG protection fails closed");
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        assert!(rows.iter().any(|row| row.id == session), "still offered (as a problem row)");
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        drop(held);
        let (resumed, report) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(!report.used_fallback, "the newest checkpoint itself");
        assert_eq!(resumed.session.photos[0].working.adjustments.exposure, 0.6);
        drop(resumed);
        assert!(super::super::store::protected_sources(&fixture.cache()).is_ok());
    }

    /// The corruption fallback E relies on still works: a newest checkpoint whose content
    /// is verified corrupt (not merely busy) falls back to the previous one.
    #[test]
    fn a_verified_corrupt_newest_checkpoint_still_falls_back() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.5);
        controller.checkpoint().unwrap();
        edit(&mut controller, &id, 0.6);
        let newest = controller.checkpoint().unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        std::fs::write(fixture.cache().join(&session).join(format!("generation-{}.json", newest.generation)), b"corrupt").unwrap();
        let (resumed, report) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(report.used_fallback);
        assert_eq!(resumed.session.photos[0].working.adjustments.exposure, 0.5);
    }

    /// Sweep S4 (pre-existing since E): an acknowledged discard has tombstoned every member,
    /// so it is never returned to editing (which left Cancel and app close unable to ever
    /// complete); a removal retry is refused, and the discard's own retry completes.
    #[cfg(windows)]
    #[test]
    fn a_pending_discard_is_never_returned_to_editing_and_its_retry_completes() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 1.0);
        controller.store.record_removals(std::slice::from_ref(&id)).unwrap();
        controller.session.removed.insert(id.clone());
        controller.session.photos.clear();
        controller.session.phase = Phase::Frozen;
        controller.pending_discard = true;
        let authority = fixture.cache().join(&controller.session.id).join("authority.json");
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(authority).unwrap();
        assert!(controller.discard_visit().is_err());
        assert!(controller.cancel_finish().is_err(), "never back to editing");
        assert!(controller.remove_discard(&id).is_err(), "no removal retry over a pending discard");
        assert_eq!(controller.session.phase, Phase::Frozen);
        assert!(controller.pending_discard());
        drop(held);
        controller.discard_visit().unwrap();
        assert!(controller.completed());
        assert!(sessions(&fixture).is_empty());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), fixture.bytes);
        assert!(!fixture.source.with_extension("xmp").exists());
    }

    /// Sweep S2 (pre-existing since E): with recovery off, the worker's cleanup after a save
    /// retires only a recovery-off journal THIS visit claimed. A resumed draft that still
    /// holds unsaved recovered edits is kept and stays offered.
    #[test]
    fn retire_journal_keeps_a_resumed_draft_and_retires_only_this_visits_journal() {
        let fixture = Fixture::new();
        let (mut draft, id) = fixture.admitted();
        edit(&mut draft, &id, 0.7);
        draft.checkpoint().unwrap();
        let session = draft.session.id.clone();
        drop(draft);
        let (mut resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        resumed.retire_journal().unwrap();
        assert!(fixture.cache().join(&session).join("authority.json").exists(), "the resumed draft is kept");
        drop(resumed);
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        let row = rows.iter().find(|row| row.id == session).expect("still offered");
        assert_eq!(row.unsaved_photos, Some(1));

        let other = fixture.root.join("inputs/other.NEF");
        std::fs::write(&other, &fixture.bytes).unwrap();
        let mut off = Controller::create_without_recovery(&fixture.cache()).unwrap();
        off.request("other".into()).unwrap();
        let photo = off.admit("other", &other, &fixture.bytes).unwrap().id;
        edit(&mut off, &photo, 0.4);
        let plan = off.prepare_save().unwrap();
        let approved = plan.id.clone();
        assert!(off.commit_save(plan, &approved).unwrap().completed);
        let journal = off.session.id.clone();
        assert!(fixture.cache().join(&journal).exists(), "this visit's explicit-save journal");
        off.retire_journal().unwrap();
        assert!(!fixture.cache().join(&journal).exists(), "retired once its save completed");
        assert_eq!(sessions(&fixture), vec![session]);
    }

    /// Sweep S7 (pre-existing since E): a recovery-off removal journal interrupted after its
    /// XMP publication, resumed with recovery ON, becomes the visit's draft BEFORE its
    /// resolved operation is retired: a crash right after the resume never leaves an
    /// "unpublished journal" that startup collects together with an unsaved photo.
    #[test]
    fn a_journal_resumed_with_recovery_on_is_a_draft_before_its_fence() {
        let fixture = Fixture::new();
        let mut off = Controller::create_without_recovery(&fixture.cache()).unwrap();
        off.request("first".into()).unwrap();
        let first = off.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        let other = fixture.root.join("inputs/other.NEF");
        std::fs::write(&other, &fixture.bytes).unwrap();
        off.request("second".into()).unwrap();
        let second = off.admit("second", &other, &fixture.bytes).unwrap().id;
        edit(&mut off, &first, 0.2);
        edit(&mut off, &second, 0.3);
        let plan = off.prepare_remove(&first).unwrap();
        let consent = plan.id.clone();
        // The removal's XMP publication is journalled; the process stops before the removal.
        off.store.commit_plan(plan, &consent).unwrap();
        let session = off.session.id.clone();
        drop(off);
        let (resumed, _) = Controller::resume_with_recovery(&fixture.cache(), &session, true).unwrap();
        drop(resumed);
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        assert!(fixture.cache().join(&session).join("authority.json").exists(), "not collected");
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        let row = rows.iter().find(|row| row.id == session).expect("offered as a draft");
        assert!(row.automatic);
        assert!(row.unsaved_photos.is_some_and(|n| n >= 1), "the unsaved second photo is protected: {row:?}");
    }

    /// Round-4 sweep (pre-existing since E): a recovered photo whose sidecar changed after
    /// the checkpoint holds a "source" ticket that blocked every Done. Discarding that one
    /// member removes its ticket too, so Done saves the other photograph, and the externally
    /// changed sidecar is never touched.
    #[test]
    fn discarding_a_changed_recovered_member_unblocks_done_for_the_others() {
        let fixture = Fixture::new();
        let (mut controller, first) = fixture.admitted();
        let other = fixture.root.join("inputs/other.NEF");
        std::fs::write(&other, &fixture.bytes).unwrap();
        controller.request("second".into()).unwrap();
        let second = controller.admit("second", &other, &fixture.bytes).unwrap().id;
        edit(&mut controller, &first, 0.5);
        edit(&mut controller, &second, 0.25);
        controller.checkpoint().unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        let external = plain_xmp();
        std::fs::write(other.with_extension("xmp"), &external).unwrap();
        let (mut resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert!(resumed.session.requested.contains_key(&format!("source-{}", second.0)), "the changed member is unresolved");
        assert!(resumed.prepare_done().is_err(), "its ticket blocks Done for every photograph");
        resumed.cancel_finish().ok();
        resumed.remove_discard(&second).unwrap();
        assert!(!resumed.session.requested.contains_key(&format!("source-{}", second.0)), "the ticket left with its member");
        let plan = resumed.prepare_done().unwrap();
        assert_eq!(plan.photos.len(), 1);
        let approved = plan.id.clone();
        assert!(resumed.commit_done(plan, &approved).unwrap().completed);
        let saved = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
        assert_eq!(super::super::xmp::load_native(&saved).unwrap().unwrap().adjustments.exposure, 0.5);
        assert_eq!(std::fs::read(other.with_extension("xmp")).unwrap(), external, "the external change is kept");
        assert_eq!(std::fs::read(&other).unwrap(), fixture.bytes);
        assert!(sessions(&fixture).is_empty());
    }

    /// Round-4 sweep (pre-existing since E): a recovery-off journal holding recovered edits,
    /// resumed with recovery still OFF, stays protected (a draft) until it is finished, so a
    /// second interruption never lets startup collect those edits as an unpublished journal.
    #[test]
    fn a_journal_with_recovered_edits_resumed_with_recovery_off_is_kept() {
        let fixture = Fixture::new();
        let mut off = Controller::create_without_recovery(&fixture.cache()).unwrap();
        off.request("first".into()).unwrap();
        let first = off.admit("first", &fixture.source, &fixture.bytes).unwrap().id;
        let other = fixture.root.join("inputs/other.NEF");
        std::fs::write(&other, &fixture.bytes).unwrap();
        off.request("second".into()).unwrap();
        let second = off.admit("second", &other, &fixture.bytes).unwrap().id;
        edit(&mut off, &first, 0.2);
        edit(&mut off, &second, 0.3);
        let plan = off.prepare_remove(&first).unwrap();
        let consent = plan.id.clone();
        off.store.commit_plan(plan, &consent).unwrap();
        let session = off.session.id.clone();
        drop(off);
        let (resumed, _) = Controller::resume_with_recovery(&fixture.cache(), &session, false).unwrap();
        drop(resumed);
        assert!(super::super::store::collect_completed(&fixture.cache()).is_empty());
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        let row = rows.iter().find(|row| row.id == session).expect("still offered");
        assert!(row.unsaved_photos.is_some_and(|n| n >= 1), "{row:?}");
    }

    /// Round-5 sweep: the JPEG/TIFF sidecar never flips with what else is in the folder. Its
    /// own `<name>.<ext>.xmp` wins; a stem sidecar is adopted only when PhotoCraft saved it for
    /// that kind (or it is foreign and no RAW shares the stem); a new one is `<name>.<ext>.xmp`;
    /// a RAW never adopts a stem sidecar holding a JPEG's saved settings.
    #[test]
    fn rendered_sidecar_binding_is_stable_whatever_else_is_in_the_folder() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        let fixture = Fixture::new();
        let image = Image::from_u8(4, 3, ChannelLayout::Rgb, [90, 110, 130].repeat(12)).unwrap();
        let jpeg_bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        let name_of = |path: &std::path::Path| path.file_name().unwrap().to_string_lossy().to_ascii_lowercase();
        let bind = |path: &std::path::Path| super::super::commit::bind(path).unwrap().target;

        // (1) A lone JPEG gets its own full-name sidecar, never <stem>.xmp.
        let lone = fixture.root.join("inputs/lone.jpg");
        std::fs::write(&lone, &jpeg_bytes).unwrap();
        assert_eq!(name_of(&bind(&lone)), "lone.jpg.xmp");

        // (2) Its saved sidecar keeps binding after a same-stem RAW is copied in later.
        let mut controller = Controller::create(&fixture.cache()).unwrap();
        controller.request("lone".into()).unwrap();
        let id = controller.admit("lone", &lone, &jpeg_bytes).unwrap().id;
        edit(&mut controller, &id, 0.3);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        assert!(controller.commit_done(plan, &approved).unwrap().completed);
        drop(controller);
        std::fs::write(fixture.root.join("inputs/lone.NEF"), &fixture.bytes).unwrap();
        assert_eq!(name_of(&bind(&lone)), "lone.jpg.xmp", "a RAW copied in later changes nothing");
        assert_eq!(name_of(&bind(&fixture.root.join("inputs/lone.NEF"))), "lone.xmp", "the RAW owns <stem>.xmp");

        // (3) A legacy stem's native JPEG kind does not identify its photograph.
        //     Both the JPEG and a RAW beside it refuse adoption without changing it.
        let old = fixture.root.join("inputs/old.jpg");
        std::fs::write(&old, &jpeg_bytes).unwrap();
        let saved = std::fs::read(fixture.root.join("inputs/lone.jpg.xmp")).unwrap();
        std::fs::write(fixture.root.join("inputs/old.xmp"), &saved).unwrap();
        std::fs::write(fixture.root.join("inputs/old.NEF"), &fixture.bytes).unwrap();
        assert!(super::super::commit::bind(&old).unwrap_err().to_string().contains("ownership"));
        assert_eq!(std::fs::read(fixture.root.join("inputs/old.xmp")).unwrap(), saved);
        let refused = super::super::commit::bind(&fixture.root.join("inputs/old.NEF")).unwrap_err().to_string();
        assert!(refused.contains("JPEG or TIFF"), "{refused}");

        // (4) A deleted RAW's leftover <stem>.xmp is never a JPEG's baseline.
        let left = fixture.root.join("inputs/left.jpg");
        std::fs::write(&left, &jpeg_bytes).unwrap();
        let mut raw_session = Controller::create(&fixture.cache()).unwrap();
        let raw = fixture.root.join("inputs/left.NEF");
        std::fs::write(&raw, &fixture.bytes).unwrap();
        raw_session.request("raw".into()).unwrap();
        let raw_id = raw_session.admit("raw", &raw, &fixture.bytes).unwrap().id;
        edit(&mut raw_session, &raw_id, 0.9);
        let plan = raw_session.prepare_done().unwrap();
        let approved = plan.id.clone();
        assert!(raw_session.commit_done(plan, &approved).unwrap().completed);
        drop(raw_session);
        std::fs::remove_file(&raw).unwrap();
        let binding = super::super::commit::bind(&left).unwrap();
        assert_eq!(name_of(&binding.target), "left.jpg.xmp");
        assert!(binding.original.is_none(), "the RAW's settings are not the JPEG's");
    }

    /// Round-5 sweep (pre-existing since E): a photo whose sidecar changed outside PhotoCraft
    /// mid-visit no longer blocks Done for the whole batch. Done names it and gives it a source
    /// ticket; discarding just that photo lets Done save the others, and restoring the file
    /// instead clears the ticket at the next Done.
    #[test]
    fn a_mid_visit_sidecar_change_is_named_and_never_blocks_the_others() {
        for restore in [false, true] {
            let fixture = Fixture::new();
            let (mut controller, first) = fixture.admitted();
            let other = fixture.root.join("inputs/other.NEF");
            std::fs::write(&other, &fixture.bytes).unwrap();
            controller.request("second".into()).unwrap();
            let second = controller.admit("second", &other, &fixture.bytes).unwrap().id;
            edit(&mut controller, &first, 0.5);
            edit(&mut controller, &second, 0.25);
            let external = plain_xmp();
            std::fs::write(other.with_extension("xmp"), &external).unwrap();
            let error = controller.prepare_done().unwrap_err().to_string();
            assert!(error.contains("other.NEF") && error.contains("changed outside PhotoCraft"), "{error}");
            assert!(controller.session.requested.contains_key(&format!("source-{}", second.0)));
            controller.cancel_finish().ok();
            if restore {
                std::fs::remove_file(other.with_extension("xmp")).unwrap();
            } else {
                controller.remove_discard(&second).unwrap();
            }
            let plan = controller.prepare_done().unwrap();
            assert_eq!(plan.photos.len(), if restore { 2 } else { 1 });
            let approved = plan.id.clone();
            assert!(controller.commit_done(plan, &approved).unwrap().completed);
            let saved = std::fs::read(fixture.source.with_extension("xmp")).unwrap();
            assert_eq!(super::super::xmp::load_native(&saved).unwrap().unwrap().adjustments.exposure, 0.5);
            if !restore {
                assert_eq!(std::fs::read(other.with_extension("xmp")).unwrap(), external, "the external change is kept");
            }
            assert!(sessions(&fixture).is_empty());
        }
    }

    /// Round-5 sweep (pre-existing since E): "Discard recovered copy" discards only the state
    /// the chooser listed. A session another PhotoCraft resumed and changed meanwhile is
    /// refused with nothing written; a fresh listing can then be discarded.
    #[test]
    fn a_stale_listing_never_discards_newer_recovered_work() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        edit(&mut controller, &id, 0.4);
        controller.checkpoint().unwrap();
        let session = controller.session.id.clone();
        drop(controller);
        let rows = super::super::store::discover(&fixture.cache()).unwrap();
        let listed = rows.iter().find(|row| row.id == session).unwrap().listed.clone().unwrap();
        let (mut other, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        edit(&mut other, &id, 0.8);
        other.checkpoint().unwrap();
        drop(other);
        let refused = Controller::discard_recovered_listed(&fixture.cache(), &session, Some(&listed)).unwrap_err().to_string();
        assert!(refused.contains("changed since it was listed"), "{refused}");
        let (kept, _) = Controller::resume(&fixture.cache(), &session).unwrap();
        assert_eq!(kept.session.photos[0].working.adjustments.exposure, 0.8, "the newer work survives");
        drop(kept);
        let fresh = super::super::store::discover(&fixture.cache()).unwrap().into_iter().find(|row| row.id == session).unwrap().listed.unwrap();
        Controller::discard_recovered_listed(&fixture.cache(), &session, Some(&fresh)).unwrap();
        assert!(sessions(&fixture).is_empty());
    }

    /// Round-6 (regression of the stable sidecar rule, fixed): a session checkpointed by an
    /// earlier build recorded a lone JPEG's never-written sidecar as `<stem>.xmp`; this build
    /// names it `<name>.jpg.xmp`. On resume that binding migrates (no ticket, nothing changed
    /// on disk) and Done saves the recovered edit to `<name>.jpg.xmp`. A sidecar that really
    /// appeared meanwhile still raises the ticket.
    #[test]
    fn an_earlier_builds_unwritten_jpeg_binding_migrates_on_resume() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        for appeared in [false, true] {
            let fixture = Fixture::new();
            let image = Image::from_u8(4, 3, ChannelLayout::Rgb, [90, 110, 130].repeat(12)).unwrap();
            let bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
            let lone = fixture.root.join("inputs/lone.jpg");
            std::fs::write(&lone, &bytes).unwrap();
            let mut controller = Controller::create(&fixture.cache()).unwrap();
            controller.request("lone".into()).unwrap();
            let id = controller.admit("lone", &lone, &bytes).unwrap().id;
            // As an earlier build recorded it: the conventional <stem>.xmp, never written.
            let binding = controller.session.sources.get_mut(&id).unwrap();
            assert!(binding.target.to_string_lossy().to_lowercase().ends_with("lone.jpg.xmp"));
            binding.target = binding.source.with_extension("xmp");
            edit(&mut controller, &id, 0.35);
            controller.checkpoint().unwrap();
            let session = controller.session.id.clone();
            drop(controller);
            if appeared {
                std::fs::write(fixture.root.join("inputs/lone.xmp"), plain_xmp()).unwrap();
            }
            let (mut resumed, _) = Controller::resume(&fixture.cache(), &session).unwrap();
            let ticket = resumed.session.requested.contains_key(&format!("source-{}", id.0));
            assert_eq!(ticket, appeared, "appeared={appeared}");
            if appeared {
                continue;
            }
            assert!(resumed.session.sources[&id].target.to_string_lossy().to_lowercase().ends_with("lone.jpg.xmp"), "migrated");
            let plan = resumed.prepare_done().unwrap();
            let approved = plan.id.clone();
            assert!(resumed.commit_done(plan, &approved).unwrap().completed);
            let saved = std::fs::read(fixture.root.join("inputs/lone.jpg.xmp")).unwrap();
            assert_eq!(super::super::xmp::load_native(&saved).unwrap().unwrap().adjustments.exposure, 0.35);
            assert!(!fixture.root.join("inputs/lone.xmp").exists());
            assert_eq!(std::fs::read(&lone).unwrap(), bytes);
        }
    }

    /// Round-6: every non-RAW source follows the rendered rule whatever its extension (a
    /// `.jfif` beside a RAW never takes the RAW's `<stem>.xmp`), and a stem packet PhotoCraft
    /// saved for this kind next to a full-name one is refused as ambiguous, never shadowed.
    #[test]
    fn jfif_and_shadowed_saves_follow_the_rendered_rule() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        let fixture = Fixture::new();
        let image = Image::from_u8(4, 3, ChannelLayout::Rgb, [70, 90, 110].repeat(12)).unwrap();
        let bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        let jfif = fixture.root.join("inputs/pair.jfif");
        std::fs::write(&jfif, &bytes).unwrap();
        std::fs::write(fixture.root.join("inputs/pair.NEF"), &fixture.bytes).unwrap();
        std::fs::write(fixture.root.join("inputs/pair.xmp"), plain_xmp()).unwrap();
        let binding = super::super::commit::bind(&jfif).unwrap();
        assert!(binding.target.to_string_lossy().to_lowercase().ends_with("pair.jfif.xmp"), "{:?}", binding.target);
        assert!(binding.original.is_none());

        let jpeg = fixture.root.join("inputs/shadow.jpg");
        std::fs::write(&jpeg, &bytes).unwrap();
        let mut controller = Controller::create(&fixture.cache()).unwrap();
        controller.request("shadow".into()).unwrap();
        let id = controller.admit("shadow", &jpeg, &bytes).unwrap().id;
        edit(&mut controller, &id, 0.2);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        assert!(controller.commit_done(plan, &approved).unwrap().completed);
        drop(controller);
        // An E-era-style native save at <stem>.xmp, plus a foreign full-name packet.
        std::fs::rename(fixture.root.join("inputs/shadow.jpg.xmp"), fixture.root.join("inputs/shadow.xmp")).unwrap();
        std::fs::write(fixture.root.join("inputs/shadow.jpg.xmp"), plain_xmp()).unwrap();
        let refused = super::super::commit::bind(&jpeg).unwrap_err().to_string();
        assert!(refused.contains("ownership"), "{refused}");
    }

    /// Round-7 (regression of 9f689ea, fixed): ownership uses the file's CONTENT kind, the kind
    /// admission records in the saved recipe, so PhotoCraft's own save never changes a binding.
    /// A JPEG named `.mpo` beside its explicit `<name>.<ext>.xmp` saves, Done completes, and the
    /// binding is the same afterwards; ambiguous stem saves are refused by the ownership tests.
    #[test]
    fn ambiguous_stem_packets_are_refused_before_rendered_admission() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        for native in [false, true] {
            let fixture = Fixture::new();
            let image = Image::from_u8(4, 3, ChannelLayout::Rgb, [60, 80, 100].repeat(12)).unwrap();
            let bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
            let shot = fixture.root.join("inputs/shot.mpo");
            std::fs::write(&shot, &bytes).unwrap();
            let stem = shot.with_extension("xmp");
            let packet = if native {
                let mut baseline = crate::raw_session::tests::recipe(0.);
                baseline.processing.source_kind = crate::camera_raw_source::SourceKind::Jpeg;
                baseline.processing.demosaic = "none".into();
                let mut working = baseline.clone();
                working.adjustments.exposure = 0.1;
                super::super::xmp::plan(None, &baseline, &working).unwrap().unwrap()
            } else {
                plain_xmp()
            };
            std::fs::write(&stem, &packet).unwrap();
            if native {
                // A same-kind native packet has no owning filename, so an MPO/JPG
                // pair cannot safely establish which photograph wrote it.
                std::fs::write(fixture.root.join("inputs/shot.jpg"), &bytes).unwrap();
            }
            let mut controller = Controller::create(&fixture.cache()).unwrap();
            controller.request("shot".into()).unwrap();
            let refused = controller.admit("shot", &shot, &bytes).unwrap_err().to_string();
            assert!(refused.contains("ownership") && refused.contains("shot.mpo") && refused.contains("shot.xmp"), "{refused}");
            assert!(controller.session.photos.is_empty());
            assert_eq!(std::fs::read(&shot).unwrap(), bytes);
            assert_eq!(std::fs::read(&stem).unwrap(), packet);
            assert!(!fixture.root.join("inputs/shot.mpo.xmp").exists(), "no implicit naming change");
        }
    }

    #[test]
    fn a_sensor_tiff_sibling_makes_raw_stem_publication_ambiguous() {
        let fixture = Fixture::new();
        let (mut controller, id) = fixture.admitted();
        let source = controller.session.sources[&id].source.clone();
        let sibling = source.with_extension("tif");
        std::fs::write(&sibling, &fixture.bytes).unwrap();
        let source_before = std::fs::read(&source).unwrap();
        let target = controller.session.sources[&id].target.clone();
        let before = std::fs::read(&target).ok();
        edit(&mut controller, &id, 0.6);
        let refused = controller.prepare_done().unwrap_err().to_string();
        assert!(refused.contains("ownership") && refused.contains(".tif"), "{refused}");
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
        assert_eq!(std::fs::read(&target).ok(), before);
    }

    #[test]
    fn a_jpeg_with_an_unlisted_extension_keeps_its_binding_across_its_own_save() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
        let fixture = Fixture::new();
        let image = Image::from_u8(4, 3, ChannelLayout::Rgb, [60, 80, 100].repeat(12)).unwrap();
        let bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        let shot = fixture.root.join("inputs/shot.mpo");
        std::fs::write(&shot, &bytes).unwrap();
        std::fs::write(fixture.root.join("inputs/shot.mpo.xmp"), plain_xmp()).unwrap();
        let before = super::super::commit::bind(&shot).unwrap().target;
        assert!(before.to_string_lossy().to_lowercase().ends_with("shot.mpo.xmp"), "the explicit full-name packet belongs to this photograph");
        let mut controller = Controller::create(&fixture.cache()).unwrap();
        controller.request("shot".into()).unwrap();
        let id = controller.admit("shot", &shot, &bytes).unwrap().id;
        edit(&mut controller, &id, 0.6);
        let plan = controller.prepare_done().unwrap();
        let approved = plan.id.clone();
        let report = controller.commit_done(plan, &approved).unwrap();
        assert!(report.completed, "PhotoCraft's own save does not disown the sidecar: {:?}", report.completion_error);
        let after = super::super::commit::bind(&shot).unwrap();
        assert_eq!(after.target, before, "same binding after the save");
        let saved = super::super::xmp::load_native(after.original.as_deref().unwrap()).unwrap().unwrap();
        assert_eq!(saved.adjustments.exposure, 0.6);
        assert_eq!(std::fs::read(&shot).unwrap(), bytes);
    }
}
