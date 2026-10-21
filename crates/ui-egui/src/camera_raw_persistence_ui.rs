//! Native persistence bridge. One bounded worker owns the controller; the frame
//! owns a recipe mirror and coalesces snapshots. Only exact replies acknowledge
//! recovery, and lifecycle replies are the only place live reviews are released.
use super::*;
use domain::commit::Plan;
use domain::controller::Controller;
use photocraft_io::raw_session::{self as domain, NativeRecipe, Session};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct LocalPhoto {
    id: domain::PhotoId,
    recipe: NativeRecipe,
    committed: Option<NativeRecipe>,
}

#[derive(Clone, PartialEq)]
struct Snapshot {
    changes: Vec<(domain::PhotoId, NativeRecipe)>,
    views: Vec<(domain::PhotoId, bool, bool)>,
    context: Value,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PhotoView {
    id: domain::PhotoId,
    scope: crate::state::CameraRawScopeState,
    navigation: crate::state::CameraRawPreviewState,
    show_before: bool,
}

/// Footer actions share one controller; opted-in Keep lives inside a decision dialog.
/// Resumed Cancel preserves the entry draft, while explicit deletion stays terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
enum Finish {
    Save,
    Done,
    Keep,
    Discard,
    Open,
}

#[derive(Clone)]
enum Command {
    RecoveryMode(bool, Snapshot),
    PrepareRemove(PhotoId, domain::PhotoId, Snapshot),
    DiscardRemove(PhotoId, domain::PhotoId),
    Requests(Vec<crate::camera_raw_admission::Request>),
    Resume(String),
    /// Read-only facts for the two recovery cards of one interrupted session.
    Inspect(String),
    /// Confirmed disposal of an interrupted session (never of a live one), as listed.
    DiscardRecovered(String, Option<domain::store::Listed>),
    Restore(PhotoId, domain::PhotoId),
    Admit(PhotoId, PathBuf, String),
    /// The last field marks an explicit "Retry recovery save" (never the automatic timer).
    Sync(u64, Snapshot, Option<Finish>, bool),
    Commit(String),
    Back,
    Leave(Finish),
}

// The reply channel holds at most one message; keep the owned recipe/import
// transaction together rather than allocating each large variant separately.
#[allow(clippy::large_enum_variant)]
enum Reply {
    RecoveryMode(bool, Session),
    RecoveryModeFailed(String, bool, Session),
    RemovalFailed(String, Session, PhotoId, domain::PhotoId),
    /// The session is None when the failure happened before a controller existed.
    Retryable(String, Option<Session>, Command),
    SaveProgress(usize, usize),
    SaveResult(Value),
    Notice(String),
    CleanupWarning(String),
    PreparedRemoval(PhotoId, Plan, Session),
    Removed(PhotoId, Session),
    Resumed(Session, domain::controller::ResumeReport),
    Restored(PhotoId, domain::PhotoId, photocraft_io::ImportResult, PathBuf, NativeRecipe),
    Discovered(Vec<domain::store::RecoveryCandidate>),
    Inspected(Result<domain::store::CandidateDetail, String>),
    Admitted(PhotoId, domain::PhotoId, Session, Option<photocraft_io::ImportResult>),
    Saved(u64, Session),
    EditsSaved(Session),
    Prepared(Plan, Session),
    /// The session ended; carries a cleanup warning when its records were kept for later removal.
    Closed(Finish, Option<String>),
    /// Cancel or app close with an XMP save that could not be confirmed: nothing could be
    /// discarded, so the session was closed and KEPT for the next start to reconcile.
    Kept(String),
    /// The chooser's listing was stale: the fresh listing, and why nothing was discarded.
    Rediscovered(Vec<domain::store::RecoveryCandidate>, String),
    /// Open Images published every sidecar; the host now hands every retained photo to
    /// the editor and reports completion with `Leave(Open)`.
    OpenSaved(Session),
    Returned(Session),
    Failed(String, Option<Session>, Option<Finish>),
}

pub(super) struct Host {
    recovery_enabled: bool,
    mode_attempt: Option<bool>,
    admitting: bool,
    cancel_loading: bool,
    /// The loading Cancel's Leave(Discard) is with the worker: only ITS failure ends the
    /// pending Cancel (an unrelated failure must not silently drop it).
    discard_in_flight: bool,
    sender: SyncSender<Command>,
    receiver: Receiver<Reply>,
    busy: bool,
    frozen: bool,
    pending: VecDeque<(PhotoId, PathBuf)>,
    request_keys: BTreeMap<PhotoId, String>,
    ledger_pending: Option<Vec<crate::camera_raw_admission::Request>>,
    photos: BTreeMap<PhotoId, LocalPhoto>,
    session: Option<Session>,
    last: Option<Snapshot>,
    revision: u64,
    acknowledged: Option<u64>,
    changed: Instant,
    first_dirty: Instant,
    saved_time: Option<std::time::SystemTime>,
    error: Option<String>,
    plan: Option<Plan>,
    close_prompt: bool,
    exit_requested: bool,
    open_ready: bool,
    discovered: Vec<domain::store::RecoveryCandidate>,
    chooser: bool,
    restoring: VecDeque<domain::PhotoId>,
    notices: Vec<String>,
    details: bool,
    held_command: Option<Command>,
    removals: VecDeque<PhotoId>,
    removing: Option<PhotoId>,
    resume_context: Option<Value>,
    transition: Option<Finish>,
    removal_retry: Option<(PhotoId, domain::PhotoId)>,
    save_progress: Option<(usize, usize)>,
    failed_admissions: VecDeque<Command>,
    transferred: std::collections::BTreeSet<PhotoId>,
    handoff: bool,
    auto_failed_revision: Option<u64>,
    /// The exit being prepared or published (Done, Open Images, Save edits).
    finishing: Option<Finish>,
    /// The interrupted session the recovery chooser shows, and its inspected files.
    chosen: Option<String>,
    inspection: Option<Result<domain::store::CandidateDetail, String>>,
    file_details: bool,
    /// "Discard recovered copy" awaits its confirmation.
    confirm_discard_recovered: bool,
    /// The session the in-flight Inspect is for; a reply for another one is dropped.
    inspecting: Option<String>,
    /// Open Images saved and handed over every photo: ending the session is not a discard.
    opened: bool,
    /// "Open saved XMP edits" re-inspects first and opens from that fresh reply only.
    open_after_inspection: Option<String>,
}

fn worker(root: PathBuf, publisher: Option<domain::SidecarPublisher>, mut recovery_enabled: bool, receiver: Receiver<Command>, sender: SyncSender<Reply>) {
    // Retry removing completed sessions' records first; then list only interrupted
    // sessions (an opted-in draft, or an explicit save that recorded a publication),
    // whatever the current preference: recovery was chosen for that session.
    let reclaim_warnings = domain::store::collect_completed(&root);
    match domain::store::discover(&root) {
        Ok(rows) => {
            if sender.send(Reply::Discovered(rows)).is_err() {
                return;
            }
        }
        Err(error) => {
            if sender.send(Reply::Failed(error.to_string(), None, None)).is_err() {
                return;
            }
        }
    }
    for warning in reclaim_warnings {
        if sender.send(Reply::Notice(warning)).is_err() {
            return;
        }
    }
    let mut controller: Option<Controller> = None;
    let mut prepared: Option<Plan> = None;
    let mut removal_runtime = None;
    let mut finishing = Finish::Done;
    while let Ok(command) = receiver.recv() {
        let mut cleanup_warning = None;
        let mode_change = matches!(command, Command::RecoveryMode(..));
        let retry = matches!(command, Command::Admit(..) | Command::Restore(..)).then(|| command.clone());
        let removal_intent = match &command {
            Command::DiscardRemove(runtime, id) => Some((*runtime, id.clone())),
            _ => None,
        };
        let result = (|| -> Result<Reply, String> {
            match &command {
                Command::Resume(id) => {
                    if controller.is_some() {
                        return Err("An active session must be closed before resuming another".into());
                    }
                    let (mut resumed, report) = Controller::resume_with_recovery(&root, id, recovery_enabled).map_err(|e| e.to_string())?;
                    resumed.set_sidecar_publisher(publisher.clone());
                    let session = resumed.session().clone();
                    controller = Some(resumed);
                    return Ok(Reply::Resumed(session, report));
                }
                Command::Inspect(id) => return Ok(Reply::Inspected(domain::store::inspect_candidate(&root, id).map_err(|e| e.to_string()))),
                Command::DiscardRecovered(id, listed) => {
                    if controller.as_ref().is_some_and(|c| c.session().id == *id) {
                        return Err("The open session cannot be discarded as a recovered copy".into());
                    }
                    match Controller::discard_recovered_listed(&root, id, listed.as_ref()) {
                        Ok(Some(warning)) => {
                            let _ = sender.send(Reply::Notice(warning));
                        }
                        Ok(None) => {}
                        // The listing is stale: nothing was written; list again and say why.
                        Err(error) => {
                            let rows = domain::store::discover(&root).map_err(|e| e.to_string())?;
                            return Ok(Reply::Rediscovered(rows, error.to_string()));
                        }
                    }
                    return domain::store::discover(&root).map(Reply::Discovered).map_err(|e| e.to_string());
                }
                _ => {}
            }
            if controller.is_none() && matches!(command, Command::Leave(Finish::Discard) | Command::Sync(_, _, Some(Finish::Discard), _)) {
                // No session was ever created: nothing durable exists and nothing was written,
                // so Cancel ends the visit without trying to create a recovery store.
                return Ok(Reply::Closed(Finish::Discard, None));
            }
            if controller.is_none() {
                // Turning recovery off never needs the cache: create the session in memory,
                // so an unwritable recovery folder cannot make the opt-out fail forever.
                let durable = recovery_enabled && !matches!(command, Command::RecoveryMode(false, _));
                let mut created = if durable { Controller::create(&root) } else { Controller::create_without_recovery(&root) }.map_err(|e| e.to_string())?;
                created.set_sidecar_publisher(publisher.clone());
                controller = Some(created);
            }
            let c = controller.as_mut().ok_or("Recovery controller unavailable")?;
            match command {
                Command::RecoveryMode(enabled, snapshot) => {
                    c.sync_edits_with_recovery(snapshot.changes, snapshot.views, snapshot.context, recovery_enabled).map_err(|e| e.to_string())?;
                    if enabled { c.enable_recovery() } else { c.disable_recovery() }.map_err(|e| e.to_string())?;
                    recovery_enabled = enabled;
                    Ok(Reply::RecoveryMode(enabled, c.session().clone()))
                }
                Command::PrepareRemove(runtime, id, snapshot) => {
                    c.sync_edits_with_recovery(snapshot.changes, snapshot.views, snapshot.context, recovery_enabled).map_err(|e| e.to_string())?;
                    let plan = match c.prepare_remove(&id) {
                        Ok(plan) => plan,
                        Err(error) => {
                            if matches!(c.session().phase, domain::Phase::Frozen | domain::Phase::Settling) && !c.pending_discard() {
                                c.cancel_finish().map_err(|e| e.to_string())?;
                            }
                            if !recovery_enabled {
                                c.retire_journal().map_err(|e| format!("{error}; explicit save journal retained: {e}"))?;
                            }
                            return Err(error.to_string());
                        }
                    };
                    prepared = Some(plan.clone());
                    removal_runtime = Some(runtime);
                    Ok(Reply::PreparedRemoval(runtime, plan, c.session().clone()))
                }
                Command::DiscardRemove(runtime, id) => {
                    if matches!(c.session().phase, domain::Phase::Frozen | domain::Phase::Settling) && !c.pending_discard() {
                        c.cancel_finish().map_err(|e| e.to_string())?;
                    }
                    c.remove_discard(&id).map_err(|e| e.to_string())?;
                    if !recovery_enabled {
                        cleanup_warning = c.retire_journal().err().map(|e| e.to_string());
                    }
                    prepared = None;
                    removal_runtime = None;
                    Ok(Reply::Removed(runtime, c.session().clone()))
                }
                Command::Requests(requests) => {
                    let receipt = serde_json::to_value(&requests).map_err(|e| e.to_string())?;
                    for request in requests {
                        if !c.session().requested.contains_key(&request.key) {
                            c.request(request.key.clone()).map_err(|e| e.to_string())?;
                        }
                        if !c.session().requested.get(&request.key).is_some_and(|s| matches!(s, domain::Admission::Pending | domain::Admission::Unresolved(_)))
                        {
                            continue;
                        }
                        use crate::camera_raw_admission::Outcome;
                        match request.outcome {
                            Outcome::Failed(error) => c.reject(&request.key, format!("{}: {error}", request.name)).map_err(|e| e.to_string())?,
                            Outcome::Cancelled => c.cancel_request(&request.key).map_err(|e| e.to_string())?,
                            Outcome::OutsideRaw => c.reject(&request.key, format!("{}: opened outside Camera Raw", request.name)).map_err(|e| e.to_string())?,
                            Outcome::Ready | Outcome::Pending => {}
                        }
                    }
                    let mut context = c.session().view_context.clone();
                    if !context.is_object() {
                        context = json!({});
                    }
                    context["requests"] = receipt;
                    c.sync_edits_with_recovery(Vec::new(), Vec::new(), context, recovery_enabled).map_err(|e| e.to_string())?;
                    Ok(Reply::Returned(c.session().clone()))
                }
                Command::Resume(_) | Command::Inspect(_) | Command::DiscardRecovered(..) => Err("Recovery dispatch failed".into()),
                Command::Restore(runtime, id) => {
                    let bytes = c.read_verified_source(&id).map_err(|e| e.to_string())?;
                    // A sidecar conflict must retain its ticket, but it does not
                    // prevent displaying edits on the verified original pixels.
                    if let Err(error) = c.resolve_source(&id) {
                        let _ = sender.send(Reply::Notice(error.to_string()));
                    }
                    let binding = c.session().sources.get(&id).ok_or("Missing retained source binding")?;
                    let recipe = c.session().photos.iter().find(|p| p.id == id).ok_or("Missing retained recipe")?.working.clone();
                    let path = binding.source.clone();
                    let name = path.file_name().and_then(|n| n.to_str()).ok_or("Source name is not UTF-8")?;
                    let mut neutral = recipe.clone();
                    neutral.adjustments = CameraRaw::default();
                    let (imported, _) = photocraft_io::raw::import_raw_recipe(name, &bytes, &neutral).map_err(|e| e.to_string())?;
                    Ok(Reply::Restored(runtime, id, imported, path, recipe))
                }
                Command::Admit(runtime, path, key) => {
                    if !c.session().requested.contains_key(&key) {
                        c.request(key.clone()).map_err(|e| e.to_string())?;
                    }
                    let bytes = match photocraft_format::read_file(&path) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            c.acknowledge_edits(recovery_enabled).map_err(|e| e.to_string())?;
                            return Err(format!("{}: {error}", path.display()));
                        }
                    };
                    let retained = match c.session().requested.get(&key) {
                        Some(domain::Admission::Ready(id) | domain::Admission::Duplicate(id)) => {
                            let photo = c.session().photos.iter().find(|p| &p.id == id).ok_or("Admitted recipe is missing")?;
                            Some(domain::controller::AdmissionReport {
                                id: id.clone(),
                                recipe: photo.working.clone(),
                                binding: c.session().sources.get(id).cloned().ok_or("Admitted source is missing")?,
                                duplicate: matches!(c.session().requested.get(&key), Some(domain::Admission::Duplicate(_))),
                            })
                        }
                        _ => None,
                    };
                    let report = match retained.map(Ok).unwrap_or_else(|| c.admit(&key, &path, &bytes)) {
                        Ok(report) => report,
                        Err(error) => {
                            c.acknowledge_edits(recovery_enabled).map_err(|e| e.to_string())?;
                            return Err(error.to_string());
                        }
                    };
                    let preview = if report.duplicate {
                        None
                    } else {
                        let verified = c.read_verified_source(&report.id).map_err(|e| e.to_string())?;
                        let mut neutral = report.recipe.clone();
                        neutral.adjustments = CameraRaw::default();
                        let name = path.file_name().and_then(|n| n.to_str()).ok_or("Source filename is not UTF-8")?;
                        Some(photocraft_io::raw::import_raw_recipe(name, &verified, &neutral).map_err(|e| e.to_string())?.0)
                    };
                    let ack = c.acknowledge_edits(recovery_enabled).map_err(|e| e.to_string())?;
                    if let Some(warning) = ack.storage_pressure {
                        let _ = sender.send(Reply::Notice(warning));
                    }
                    Ok(Reply::Admitted(runtime, report.id, c.session().clone(), preview))
                }
                Command::Sync(revision, snapshot, finish, explicit) => {
                    if finish == Some(Finish::Open) {
                        c.retry_source_tickets();
                    }
                    if finish == Some(Finish::Open)
                        && (c.session().requested.values().any(|state| matches!(state, domain::Admission::Pending | domain::Admission::Unresolved(_)))
                            || c.session().photos.iter().any(|photo| !snapshot.changes.iter().any(|(id, _)| id == &photo.id)))
                    {
                        return Err("Open Images is waiting for every retained photograph to be available; see recovery details".into());
                    }
                    if finish == Some(Finish::Discard) && c.unresolved_publication() {
                        // Nothing can be discarded or written now; keep every record and
                        // release the session so the next start reconciles and offers it.
                        let message = kept_message(c);
                        controller = None;
                        return Ok(Reply::Kept(message.into()));
                    }
                    let ack = c
                        .sync_edits_with_recovery(snapshot.changes, snapshot.views, snapshot.context, recovery_enabled)
                        .map_err(|e| if recovery_enabled { format!("Recovery save failed — changes remain open: {e}") } else { e.to_string() })?;
                    if let Some(warning) = ack.storage_pressure {
                        let _ = sender.send(Reply::Notice(warning));
                    }
                    match finish {
                        // Done and Open Images share one reviewed plan; Save edits keeps editing.
                        Some(action @ (Finish::Done | Finish::Save | Finish::Open)) => {
                            finishing = action;
                            let plan = match if action == Finish::Save { c.prepare_save() } else { c.prepare_done() } {
                                Ok(plan) => plan,
                                Err(error) => {
                                    if matches!(c.session().phase, domain::Phase::Frozen | domain::Phase::Settling) && !c.pending_discard() {
                                        c.cancel_finish().map_err(|e| e.to_string())?;
                                    }
                                    if !recovery_enabled {
                                        c.retire_journal().map_err(|e| format!("{error}; explicit save journal retained: {e}"))?;
                                    }
                                    return Err(error.to_string());
                                }
                            };
                            prepared = Some(plan.clone());
                            Ok(Reply::Prepared(plan, c.session().clone()))
                        }
                        Some(Finish::Discard) => {
                            c.discard_visit().map_err(|e| e.to_string())?;
                            Ok(Reply::Closed(Finish::Discard, None))
                        }
                        Some(Finish::Keep) => {
                            if !recovery_enabled {
                                return Err("Turn on long-session recovery before keeping this session".into());
                            }
                            c.keep().map_err(|e| e.to_string())?;
                            Ok(Reply::Closed(Finish::Keep, None))
                        }
                        None => {
                            // "Retry recovery save": a photo whose file came back verifies
                            // again and its ticket clears (never re-hashed by the timer).
                            if explicit {
                                c.retry_source_tickets();
                            }
                            // In off mode this is an explicit retry, never the
                            // automatic timer. Retry temporary-journal cleanup
                            // without publishing an unsolicited edit checkpoint.
                            if !recovery_enabled {
                                c.retire_journal().map_err(|e| e.to_string())?;
                            }
                            Ok(Reply::Saved(revision, c.session().clone()))
                        }
                    }
                }
                Command::Commit(id) => {
                    let plan = prepared.take().ok_or("No reviewed XMP plan")?;
                    if let Some(runtime) = removal_runtime.take() {
                        let report = match c.commit_remove_with_progress(plan, &id, &mut |completed, total| {
                            let _ = sender.send(Reply::SaveProgress(completed, total));
                        }) {
                            Ok(report) => report,
                            Err(error) => {
                                // Nothing more can be published from this plan: return to editing
                                // so a fresh removal can be reviewed (unknown outcomes stay guarded).
                                if matches!(c.session().phase, domain::Phase::Frozen | domain::Phase::Settling) && !c.pending_discard() {
                                    c.cancel_finish().map_err(|cancel| format!("{error}; {cancel}"))?;
                                }
                                return Err(error.to_string());
                            }
                        };
                        let _ = sender.send(Reply::SaveResult(save_receipt(&id, "remove", report.removed, report.operation.as_ref(), report.error.as_deref())));
                        if report.removed {
                            if !recovery_enabled {
                                cleanup_warning = c.retire_journal().err().map(|e| e.to_string());
                            }
                            return Ok(Reply::Removed(runtime, c.session().clone()));
                        }
                        return Err(report.error.unwrap_or_else(|| "Photograph was not removed; review its XMP result".into()));
                    }
                    let mut progress = |completed, total| {
                        let _ = sender.send(Reply::SaveProgress(completed, total));
                    };
                    let committed = match finishing {
                        Finish::Save => c.commit_save_with_progress(plan, &id, &mut progress),
                        Finish::Open => c.commit_open_with_progress(plan, &id, &mut progress),
                        _ => c.commit_done_with_progress(plan, &id, &mut progress),
                    };
                    let report = match committed {
                        Ok(report) => report,
                        Err(error) => {
                            // Nothing more can be published from this plan: return to editing so
                            // a fresh plan can be reviewed. Uncertain outcomes stay guarded.
                            if matches!(c.session().phase, domain::Phase::Frozen | domain::Phase::Settling) && !c.pending_discard() {
                                c.cancel_finish().map_err(|e| e.to_string())?;
                            }
                            return Err(error.to_string());
                        }
                    };
                    let action = match finishing {
                        Finish::Save => "saveEdits",
                        Finish::Open => "openImages",
                        _ => "done",
                    };
                    let _ = sender.send(Reply::SaveResult(save_receipt(
                        &id,
                        action,
                        report.completed,
                        report.operation.as_ref(),
                        report.completion_error.as_deref(),
                    )));
                    if report.completed {
                        match finishing {
                            Finish::Save => {
                                if !recovery_enabled {
                                    cleanup_warning = c.retire_journal().err().map(|e| e.to_string());
                                }
                                Ok(Reply::EditsSaved(c.session().clone()))
                            }
                            Finish::Open => Ok(Reply::OpenSaved(c.session().clone())),
                            _ => Ok(Reply::Closed(Finish::Done, None)),
                        }
                    } else {
                        if matches!(c.session().phase, domain::Phase::Frozen | domain::Phase::Settling) && !c.pending_discard() {
                            c.cancel_finish().map_err(|e| e.to_string())?;
                        }
                        let results = report
                            .operation
                            .map(|op| {
                                op.plan
                                    .photos
                                    .iter()
                                    .map(|p| format!("{}: {}", p.binding.target.display(), outcome_text(op.outcomes.get(&p.photo))))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        Err(format!("XMP save remains open. {}\n{results}", report.completion_error.unwrap_or_default()))
                    }
                }
                Command::Back => {
                    c.cancel_finish().map_err(|e| e.to_string())?;
                    // An unpublished recovery-off journal is retired; one that recorded
                    // an unresolved publication stays protected (not an error for Back).
                    if !recovery_enabled
                        && !c.session().photos.iter().any(|photo| photo.unknown_operation)
                        && let Err(error) = c.retire_journal()
                    {
                        cleanup_warning = Some(error.to_string());
                    }
                    prepared = None;
                    removal_runtime = None;
                    Ok(Reply::Returned(c.session().clone()))
                }
                Command::Leave(action) => {
                    // Open Images already saved every sidecar and opened the photos: ending
                    // the session afterwards is reported as what happened, not as a discard.
                    let opened = c.opened_images();
                    match action {
                        Finish::Open => c.complete_open().map_err(|e| e.to_string())?,
                        Finish::Keep => {
                            if !recovery_enabled {
                                return Err("Turn on long-session recovery before keeping this session".into());
                            }
                            c.keep().map_err(|e| e.to_string())?;
                        }
                        Finish::Discard if c.unresolved_publication() => {
                            let message = if opened { KEPT_OPEN_RECORD } else { kept_message(c) };
                            controller = None;
                            return Ok(Reply::Kept(message.into()));
                        }
                        Finish::Discard => {
                            c.discard_visit().map_err(|e| e.to_string())?;
                        }
                        _ => return Err("Invalid close action".into()),
                    }
                    Ok(Reply::Closed(if opened && action == Finish::Discard { Finish::Open } else { action }, None))
                }
            }
        })();
        let reply = result.unwrap_or_else(|error| {
            if mode_change && let Some(controller) = controller.as_ref() {
                return Reply::RecoveryModeFailed(error, recovery_enabled, controller.session().clone());
            }
            if let (Some((runtime, id)), Some(controller)) = (removal_intent, controller.as_ref()) {
                return Reply::RemovalFailed(error, controller.session().clone(), runtime, id);
            }
            if let Some(command) = retry {
                // Kept for "Retry recovery save" even when no controller could be created.
                return Reply::Retryable(error, controller.as_ref().map(|c| c.session().clone()), command);
            }
            let transition = controller.as_ref().and_then(|c| {
                if c.pending_discard() {
                    Some(Finish::Discard)
                } else if c.awaiting_handoff() {
                    // Every sidecar is saved and every photo reached the editor: only the
                    // terminal record failed. "Retry recovery save" resends Leave(Open).
                    Some(Finish::Open)
                } else {
                    None
                }
            });
            Reply::Failed(error, controller.as_ref().map(|c| c.session().clone()), transition)
        });
        // A finished session's record removal is reported with its closing reply: the
        // host is released right after it, and a later message would be lost.
        let reclaim = controller.as_mut().and_then(Controller::take_cleanup_warning);
        let reply = match (reply, reclaim) {
            (Reply::Closed(action, _), Some(warning)) => Reply::Closed(action, Some(warning)),
            (reply, Some(warning)) => {
                cleanup_warning.get_or_insert(warning);
                reply
            }
            (reply, None) => reply,
        };
        let closed = matches!(reply, Reply::Closed(..));
        // Closing acknowledges ownership release as well as durable disposition.
        // A kept/restored draft must be immediately discoverable and resumable.
        if closed {
            controller = None;
        }
        if sender.send(reply).is_err() {
            break;
        }
        if let Some(error) = cleanup_warning
            && sender.send(Reply::CleanupWarning(format!("The action completed, but its explicit save journal was retained: {error}"))).is_err()
        {
            break;
        }
    }
}

/// One per-file publication outcome as the photographer reads it: saved, failed,
/// conflict or unknown, with the reason when there is one.
fn outcome_text(outcome: Option<&domain::commit::Outcome>) -> String {
    use domain::commit::Outcome;
    match outcome {
        Some(Outcome::Success) => "saved".into(),
        Some(Outcome::Failed(reason)) => format!("failed ({reason})"),
        Some(Outcome::Conflict(reason)) => format!("conflict ({reason})"),
        Some(Outcome::Unknown(reason)) => format!("unknown ({reason})"),
        Some(Outcome::Pending) | None => "not written".into(),
    }
}

fn save_receipt(id: &str, action: &str, completed: bool, operation: Option<&domain::commit::Operation>, error: Option<&str>) -> Value {
    json!({"planId":id,"action":action,"completed":completed,"error":error,
        "outcomes":operation.map(|op| op.plan.photos.iter().map(|photo| json!({
            "photo":photo.photo,"source":photo.binding.source,"target":photo.binding.target,
            "revision":photo.revision,"outcome":op.outcomes.get(&photo.photo)
        })).collect::<Vec<_>>()).unwrap_or_default()})
}

impl Host {
    fn start(root: PathBuf, publisher: Option<domain::SidecarPublisher>, recovery_enabled: bool) -> Result<Self, String> {
        let (sender, commands) = mpsc::sync_channel(1);
        let (replies, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("camera-raw-persistence".into())
            .spawn(move || worker(root, publisher, recovery_enabled, commands, replies))
            .map_err(|e| e.to_string())?;
        let now = Instant::now();
        Ok(Self {
            recovery_enabled,
            mode_attempt: None,
            admitting: false,
            cancel_loading: false,
            discard_in_flight: false,
            sender,
            receiver,
            busy: true,
            frozen: false,
            pending: VecDeque::new(),
            request_keys: BTreeMap::new(),
            ledger_pending: None,
            photos: BTreeMap::new(),
            session: None,
            last: None,
            revision: 0,
            acknowledged: None,
            changed: now,
            first_dirty: now,
            saved_time: None,
            error: None,
            plan: None,
            close_prompt: false,
            exit_requested: false,
            open_ready: false,
            discovered: Vec::new(),
            chooser: false,
            restoring: VecDeque::new(),
            notices: Vec::new(),
            details: false,
            held_command: None,
            removals: VecDeque::new(),
            removing: None,
            resume_context: None,
            transition: None,
            removal_retry: None,
            save_progress: None,
            failed_admissions: VecDeque::new(),
            transferred: Default::default(),
            handoff: false,
            auto_failed_revision: None,
            finishing: None,
            chosen: None,
            inspection: None,
            file_details: false,
            confirm_discard_recovered: false,
            inspecting: None,
            open_after_inspection: None,
            opened: false,
        })
    }

    fn send(&mut self, command: Command) -> Result<(), String> {
        if self.busy {
            return Err("Camera Raw persistence is still working".into());
        }
        let admitting = matches!(command, Command::Requests(..) | Command::Admit(..) | Command::Restore(..) | Command::Resume(..));
        if let Err(error) = self.sender.try_send(command) {
            let (command, message) = match error {
                mpsc::TrySendError::Full(command) => (command, "Recovery worker queue is unexpectedly full"),
                mpsc::TrySendError::Disconnected(command) => (command, "Recovery worker stopped without acknowledgement"),
            };
            self.held_command = Some(command);
            return Err(message.into());
        }
        self.busy = true;
        self.admitting = admitting;
        Ok(())
    }

    fn adopt(&mut self, session: Session) {
        for local in self.photos.values_mut() {
            if let Some(photo) = session.photos.iter().find(|p| p.id == local.id) {
                local.committed = photo.committed.clone();
            }
        }
        self.session = Some(session);
    }

    fn observe(&mut self, snapshot: Snapshot) {
        if self.last.as_ref() != Some(&snapshot) {
            let now = Instant::now();
            if self.acknowledged == Some(self.revision) {
                self.first_dirty = now;
            }
            self.changed = now;
            self.revision = self.revision.saturating_add(1);
            self.last = Some(snapshot);
        }
    }
}

pub(super) fn recipe(app: &PhotocraftApp, id: PhotoId) -> Option<&NativeRecipe> {
    app.raw_filmstrip.persistence.as_ref()?.photos.get(&id).map(|p| &p.recipe)
}

pub(super) fn original(app: &PhotocraftApp, id: PhotoId) -> Option<&CameraRaw> {
    let host = app.raw_filmstrip.persistence.as_ref()?;
    let id = &host.photos.get(&id)?.id;
    host.session.as_ref()?.photos.iter().find(|photo| &photo.id == id).map(|photo| &photo.entry.adjustments)
}

pub(super) fn bindings(app: &PhotocraftApp) -> Vec<domain::SourceBinding> {
    app.raw_filmstrip.persistence.as_ref().and_then(|h| h.session.as_ref()).map(|s| s.sources.values().cloned().collect()).unwrap_or_default()
}

pub(super) fn binding(app: &PhotocraftApp, id: PhotoId) -> Option<domain::SourceBinding> {
    let host = app.raw_filmstrip.persistence.as_ref()?;
    host.session.as_ref()?.sources.get(&host.photos.get(&id)?.id).cloned()
}

pub(super) fn pending_xmp(app: &PhotocraftApp, id: PhotoId) -> bool {
    app.raw_filmstrip.persistence.as_ref().and_then(|host| host.photos.get(&id)).is_some_and(|p| p.committed.as_ref() != Some(&p.recipe))
}

pub(super) fn set_metadata(app: &mut PhotocraftApp, ids: &[PhotoId], edit: impl Fn(&mut domain::Metadata)) -> Result<(), String> {
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Native recovery is unavailable")?;
    if host.frozen {
        return Err("Finish decision is holding edits".into());
    }
    if ids.iter().any(|id| !host.photos.contains_key(id)) {
        return Err("Wait for photograph admission".into());
    }
    let mut changes = Vec::new();
    for id in ids {
        let Some(photo) = host.photos.get(id) else {
            return Err("Missing photograph".into());
        };
        let mut recipe = photo.recipe.clone();
        edit(&mut recipe.metadata);
        recipe.validate()?;
        changes.push((*id, recipe));
    }
    for (id, recipe) in changes {
        if let Some(photo) = host.photos.get_mut(&id) {
            photo.recipe = recipe;
        }
    }
    Ok(())
}

pub(super) fn set_adjustments(app: &mut PhotocraftApp, changes: &[(PhotoId, CameraRaw)]) -> Result<(), String> {
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Native recovery is unavailable")?;
    if host.frozen {
        return Err("Finish decision is holding edits".into());
    }
    let mut checked = Vec::new();
    for (id, params) in changes {
        let old = host.photos.get(id).ok_or("Wait for photograph admission")?;
        let mut next = old.recipe.clone();
        next.adjustments = params.clone();
        next.adjustments.encoding_gamma = old.recipe.adjustments.encoding_gamma;
        next.adjustments.pixel_scale = 1.0;
        next.validate()?;
        checked.push((*id, next));
    }
    for (id, next) in checked {
        if let Some(photo) = host.photos.get_mut(&id) {
            photo.recipe = next;
        }
    }
    Ok(())
}

fn recovery_enabled(app: &PhotocraftApp) -> bool {
    app.session.prefs().raw_defaults.recovery_mode == photocraft_engine::prefs::RawRecoveryMode::Cache
}

pub(super) fn queue(app: &mut PhotocraftApp, raw: &RawOpen) {
    let request = crate::camera_raw_admission::request_for_doc(app, raw.document);
    if app.raw_filmstrip.persistence.is_none() {
        let Some(root) = app.services.camera_raw_recovery_root.clone() else {
            return;
        };
        match Host::start(root, app.services.camera_raw_sidecar_publisher.clone(), recovery_enabled(app)) {
            Ok(host) => app.raw_filmstrip.persistence = Some(host),
            Err(error) => {
                app.ui.status = error;
                app.ui.status_error = true;
                return;
            }
        }
    }
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        host.request_keys.insert(raw.photo, request.map_or_else(|| format!("admission-{}", raw.photo.0), |r| r.key));
        if host.pending.len().saturating_add(host.photos.len()) >= domain::MAX_PHOTOS {
            host.error = Some("Camera Raw session capacity reached".into());
        } else if let Some(path) = &raw.path {
            host.pending.push_back((raw.photo, PathBuf::from(path)));
        } else {
            host.error = Some(format!("{}: recovery needs a retained native source path", raw.name));
        }
    }
}

pub(super) fn admission_changed(app: &mut PhotocraftApp) {
    let requests = crate::camera_raw_admission::snapshot(app);
    if requests.is_empty() {
        return;
    }
    // Once review exists, retain the complete requested batch, including mixed
    // rendered inputs and failed or outside-review outcomes. A normal editor
    // import alone must not create an empty Camera Raw recovery session.
    let retains_session = app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.session.is_some() || h.resume_context.is_some());
    if !retains_session && !requests.iter().any(|r| super::is_raw_name(&r.name) || r.outcome == crate::camera_raw_admission::Outcome::Ready) {
        return;
    }
    if app.raw_filmstrip.persistence.is_none()
        && let Some(root) = app.services.camera_raw_recovery_root.clone()
    {
        match Host::start(root, app.services.camera_raw_sidecar_publisher.clone(), recovery_enabled(app)) {
            Ok(host) => app.raw_filmstrip.persistence = Some(host),
            Err(error) => {
                app.ui.status = error;
                app.ui.status_error = true;
            }
        }
    }
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        host.ledger_pending = Some(requests);
    }
}

pub(super) fn finishing(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.frozen || h.close_prompt)
}

pub(super) fn request_remove(app: &mut PhotocraftApp, ids: Vec<PhotoId>) -> Result<(), String> {
    if !crate::camera_raw_admission::settled(app) {
        return Err("Wait for all requested inputs to settle before removal".into());
    }
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
    if host.frozen || host.removing.is_some() || !host.removals.is_empty() {
        return Err("Finish the current decision first".into());
    }
    if ids.is_empty() || ids.iter().any(|id| !host.photos.contains_key(id)) {
        return Err("Wait for selected photographs to finish admission".into());
    }
    host.removals = ids.into();
    Ok(())
}

fn discard_photo(app: &mut PhotocraftApp) -> Result<(), String> {
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
    let runtime = host.removing.ok_or("No photograph removal is prepared")?;
    let id = host.photos.get(&runtime).ok_or("Missing photograph")?.id.clone();
    host.send(Command::DiscardRemove(runtime, id))
}

fn snapshot(app: &mut PhotocraftApp) -> Option<Snapshot> {
    let admissions = crate::camera_raw_admission::snapshot(app);
    let params: Vec<_> = app
        .camera_raw
        .as_ref()
        .and_then(|d| d.raw_open.as_ref().map(|raw| (raw.photo, d.params.clone())))
        .into_iter()
        .chain(app.raw_filmstrip.parked.iter().map(|p| (p.raw.photo, p.params.clone())))
        .collect();
    let host = app.raw_filmstrip.persistence.as_mut()?;
    for (id, mut params) in params {
        if let Some(photo) = host.photos.get_mut(&id) {
            // The preview's inferred encoding is not a photographer's edit.
            params.encoding_gamma = photo.recipe.adjustments.encoding_gamma;
            params.pixel_scale = 1.0;
            photo.recipe.adjustments = params;
        }
    }
    let mut photo_views: Vec<_> = app
        .raw_filmstrip
        .parked
        .iter()
        .filter_map(|p| {
            Some(PhotoView { id: host.photos.get(&p.raw.photo)?.id.clone(), scope: p.scope.clone(), navigation: p.navigation, show_before: p.show_before })
        })
        .collect();
    if let Some(dialog) = app.camera_raw.as_ref()
        && let Some(photo) = dialog.raw_open.as_ref().and_then(|r| host.photos.get(&r.photo))
    {
        photo_views.push(PhotoView {
            id: photo.id.clone(),
            scope: app.ui.camera_raw_scope.clone(),
            navigation: app.ui.camera_raw_preview,
            show_before: dialog.show_before,
        });
    }
    let context = json!({"filmstrip": app.raw_filmstrip.view, "scope": app.ui.camera_raw_scope,
        "photoViews": photo_views,
        "requests": admissions,
        "manuallyHidden": app.raw_filmstrip.manually_hidden.iter().filter_map(|id|host.photos.get(id).map(|p|&p.id)).collect::<Vec<_>>(),
        "navigation": app.ui.camera_raw_preview,
        "active": app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).and_then(|r| host.photos.get(&r.photo)).map(|p| &p.id)});
    Some(Snapshot {
        changes: host.photos.values().map(|p| (p.id.clone(), p.recipe.clone())).collect(),
        views: host
            .photos
            .iter()
            .map(|(id, p)| {
                (
                    p.id.clone(),
                    app.raw_filmstrip.manually_hidden.contains(id)
                        || !app.raw_filmstrip.view.filter.shows_metadata(
                            match p.recipe.metadata.rating {
                                Some(-1) => PhotoRating::Rejected,
                                None | Some(0) => PhotoRating::Unrated,
                                Some(n) if (1..=5).contains(&n) => PhotoRating::Stars(n as u8),
                                _ => PhotoRating::Unknown,
                            },
                            &p.recipe.metadata.label,
                            p.recipe.metadata.marked,
                        ),
                    app.raw_filmstrip.selection.contains(*id),
                )
            })
            .collect(),
        context,
    })
}

pub(super) fn blocked(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.frozen || h.chooser || h.close_prompt || h.details)
}

pub(super) fn retains_session(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.session.is_some() || h.resume_context.is_some())
}

pub(super) fn transferred(app: &mut PhotocraftApp, id: PhotoId) {
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        host.transferred.insert(id);
    }
}

pub(super) fn unready(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip
        .persistence
        .as_ref()
        .is_some_and(|h| app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).is_some_and(|r| !h.photos.contains_key(&r.photo)))
}

/// Work a discard would lose, wherever it is held: a live recipe or ANY retained session
/// member (restored or not, available or not) that differs from its saved XMP, an
/// uncertain publication, a resumed draft still being restored, or a pending removal or
/// close disposition.
const KEPT_UNCONFIRMED: &str = "An XMP save could not be confirmed, so nothing was discarded. Camera Raw closed and kept this session: the next time PhotoCraft starts it checks the XMP files and offers the session again";
const KEPT_RECORD: &str = "A private recovery record could not be confirmed, so nothing more was changed. Camera Raw closed; the next time PhotoCraft starts it checks its recovery records";
const KEPT_OPEN_RECORD: &str =
    "Open Images saved every XMP file and opened the photos; its completion record could not be confirmed, so the session was kept for the next start to check";

/// Which unconfirmed write keeps the session: an XMP save, or a private record.
fn kept_message(controller: &Controller) -> &'static str {
    if controller.session().photos.iter().any(|photo| photo.unknown_operation) { KEPT_UNCONFIRMED } else { KEPT_RECORD }
}

fn unsaved_work(host: &Host) -> bool {
    host.photos.values().any(|photo| photo.committed.as_ref() != Some(&photo.recipe))
        || host.session.as_ref().is_some_and(|session| {
            session.photos.iter().filter(|photo| !session.removed.contains(&photo.id)).any(|photo| photo.unknown_operation || photo.pending() != Ok(false))
        })
        || !host.failed_admissions.is_empty()
        || !host.restoring.is_empty()
        || host.resume_context.is_some()
        || host.removal_retry.is_some()
        || (host.transition == Some(Finish::Discard) && !host.opened)
}

/// What Cancel would lose: unsaved work, or an import that has not settled yet.
fn unsaved(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|host| {
        unsaved_work(host)
            || !host.pending.is_empty()
            || host.ledger_pending.is_some()
            || !app.jobs.opens.is_empty()
            || !crate::camera_raw_admission::settled(app)
    })
}

/// Cancel, Esc, the Camera Raw window's X and application exit all arrive here. With
/// unsaved edits the photographer confirms "Discard unsaved edits and close" or goes
/// "Back to editing"; without any, the session ends at once (still a normal, terminal
/// Cancel that writes no XMP). Returns false when no session exists to close.
pub(super) fn request_close(app: &mut PhotocraftApp) -> bool {
    let _ = snapshot(app);
    let dirty = unsaved(app);
    let Some(host) = app.raw_filmstrip.persistence.as_mut() else {
        return false;
    };
    if host.session.is_none() && host.pending.is_empty() {
        // No session exists, so nothing retained holds Cancel or the app exit. Cancel then
        // ends the visit (`end_sessionless_visit`); an app exit that is called off keeps it.
        return false;
    }
    // After Open Images saved and opened everything, only its completion record is pending:
    // ending the session then needs no "discard unsaved edits" question.
    let only_ending = host.transition == Some(Finish::Open) || (host.opened && host.transition == Some(Finish::Discard));
    if dirty || host.busy || (host.frozen && !only_ending) {
        host.close_prompt = true;
        return true;
    }
    if let Err(error) = finish(app, Finish::Discard) {
        if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
            host.close_prompt = true;
        }
        app.ui.status = error;
        app.ui.status_error = true;
    }
    true
}

/// After Cancel closed the review: a host with no session (and no reply on its way that
/// could still create one) ends with the visit, like Closed, so its failed admissions and
/// request tickets never reach a later review in this run.
pub(super) fn end_sessionless_visit(app: &mut PhotocraftApp) {
    if app
        .raw_filmstrip
        .persistence
        .as_ref()
        .is_some_and(|h| h.session.is_none() && h.pending.is_empty() && !h.busy && !h.chooser && h.resume_context.is_none())
    {
        app.raw_filmstrip.persistence = None;
        crate::camera_raw_admission::clear(app);
    }
}

pub(super) fn request_exit(app: &mut PhotocraftApp) -> bool {
    if !request_close(app) {
        return false;
    }
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        host.exit_requested = true;
    }
    true
}

/// A confirmed Cancel also abandons imports that have not finished: stop their jobs, then
/// discard through the same path as cancelling the loading dialog.
fn abandon_imports(app: &mut PhotocraftApp) {
    let jobs: Vec<_> = app.jobs.opens.iter().map(|tab| tab.job).collect();
    for job in jobs {
        crate::jobs_ui::cancel(app, job);
    }
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        host.close_prompt = false;
        host.cancel_loading = true;
        host.frozen = true;
        // The queues stay until the discard is known to succeed (Closed drops the host); a
        // refused discard returns to editing with every admission still accounted for.
    }
}

fn finish(app: &mut PhotocraftApp, action: Finish) -> Result<(), String> {
    if action == Finish::Keep {
        if !recovery_enabled(app) {
            return Err("Turn on long-session recovery before keeping this session".into());
        }
        if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
            if !host.recovery_enabled || host.busy || host.removing.is_some() || host.chooser || host.handoff || host.transition.is_some() {
                return Err("Wait for the current recovery or admission operation before keeping this session".into());
            }
            if host.plan.is_some() {
                if host.finishing != Some(Finish::Done) {
                    return Err("Finish the current decision before keeping this session".into());
                }
                return host.send(Command::Leave(Finish::Keep));
            }
        }
    }
    if app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.handoff) {
        return Err("Wait for Open Images to finish before choosing another session action".into());
    }
    if action == Finish::Open
        && let Some(host) = app.raw_filmstrip.persistence.as_ref()
    {
        let present: std::collections::BTreeSet<_> = super::filmstrip_raws(app).iter().map(|raw| raw.photo).collect();
        if !host.failed_admissions.is_empty()
            || host.session.as_ref().is_some_and(|s| {
                s.photos.iter().any(|photo| {
                    !host.photos.iter().any(|(runtime, local)| local.id == photo.id && (present.contains(runtime) || host.transferred.contains(runtime)))
                })
            })
        {
            return Err("Open Images is waiting for every retained photograph to be available; see recovery details".into());
        }
    }
    if let Some(host) = app.raw_filmstrip.persistence.as_mut()
        && let Some(transition) = host.transition
    {
        // Cancel always remains an exit: after Open Images' sidecars are saved and its
        // photos are in the editor, discarding only ends the session (nothing is unsaved).
        if action != transition && !(action == Finish::Discard && transition == Finish::Open) {
            return Err("Retry the pending close decision before choosing another action".into());
        }
        return host.send(Command::Leave(action));
    }
    if action != Finish::Discard && app.raw_filmstrip.persistence.as_ref().is_some_and(|host| host.frozen || host.plan.is_some() || host.removing.is_some()) {
        return Err("Finish the current decision first".into());
    }
    let unsettled = !app.jobs.opens.is_empty()
        || !crate::camera_raw_admission::settled(app)
        || app.raw_filmstrip.persistence.as_ref().is_some_and(|h| !h.pending.is_empty() || h.ledger_pending.is_some() || !h.restoring.is_empty());
    if action == Finish::Discard && unsettled {
        abandon_imports(app);
        return Ok(());
    }
    if !app.jobs.opens.is_empty() || !crate::camera_raw_admission::settled(app) {
        return Err("Wait for all requested imports to settle".into());
    }
    let snapshot = snapshot(app).ok_or("Recovery unavailable")?;
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
    if !host.pending.is_empty() || host.ledger_pending.is_some() || !host.restoring.is_empty() {
        return Err("Wait for photograph admission".into());
    }
    if host.frozen && action == Finish::Discard {
        return host.send(Command::Leave(action));
    }
    host.observe(snapshot.clone());
    host.send(Command::Sync(host.revision, snapshot, Some(action), false))?;
    host.finishing = Some(action);
    host.handoff = false;
    host.frozen = true;
    host.close_prompt = false;
    Ok(())
}

/// A RAW visit is under way with no review shown (loading, the recovery chooser, an empty
/// filmstrip, a prompt, a finish, the Open Images handoff). The idle host startup discovery
/// or "Open other files" leaves, with nothing offered, held or pending, is not a visit.
pub(super) fn raw_visit(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|h| {
        h.session.is_some()
            || h.chooser
            || h.details
            || h.busy
            || h.close_prompt
            || h.frozen
            || h.handoff
            || h.cancel_loading
            || h.transition.is_some()
            || h.finishing.is_some()
            || h.resume_context.is_some()
            || !h.pending.is_empty()
            || !h.restoring.is_empty()
            || h.ledger_pending.is_some()
            || !h.failed_admissions.is_empty()
    })
}

/// A request that opened in Camera Raw but was never admitted (its native ticket is still
/// pending or unresolved, with no session member), while nothing else is in progress.
pub(super) fn unadmitted(app: &PhotocraftApp, key: &str) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|host| {
        !host.busy
            && !host.frozen
            && host.transition.is_none()
            && host.session.as_ref().is_some_and(|session| {
                session.requested.get(key).is_some_and(|state| matches!(state, domain::Admission::Pending | domain::Admission::Unresolved(_)))
            })
    })
}

/// Forget a skipped, never-admitted request: its retry, its queued admission and its review.
pub(super) fn forget_admission(app: &mut PhotocraftApp, key: &str) {
    let Some(host) = app.raw_filmstrip.persistence.as_mut() else { return };
    let runtimes: Vec<PhotoId> = host.request_keys.iter().filter(|(_, k)| k.as_str() == key).map(|(runtime, _)| *runtime).collect();
    host.failed_admissions.retain(|command| !matches!(command, Command::Admit(_, _, k) if k == key));
    host.pending.retain(|(runtime, _)| !runtimes.contains(runtime));
    for runtime in &runtimes {
        host.request_keys.remove(runtime);
    }
    if host.failed_admissions.is_empty() {
        host.error = None;
    }
    for runtime in runtimes {
        remove_runtime(app, runtime);
    }
}

/// "Discard this photo's changes and remove" for a recovered photograph whose original
/// cannot be restored exactly: removes that one member (and its source ticket) so the other
/// photographs can still be saved. Its recovered edits are discarded, nothing else.
pub(super) fn discard_recovered_member(app: &mut PhotocraftApp, id: domain::PhotoId) -> Result<(), String> {
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
    if host.busy || host.frozen || host.transition.is_some() || host.removing.is_some() {
        return Err("Finish the current decision first".into());
    }
    let session = host.session.as_ref().ok_or("Recovery unavailable")?;
    let Some(photo) = session.photos.iter().find(|photo| photo.id == id) else {
        return Err("This photograph is no longer part of the session".into());
    };
    if photo.unknown_operation {
        return Err("This photograph's last XMP save could not be confirmed, so it cannot be removed until that is checked; closing keeps the session for the next start".into());
    }
    if !matches!(session.requested.get(&format!("source-{}", id.0)), Some(domain::Admission::Unresolved(_) | domain::Admission::Rejected(_))) {
        return Err("Only a photograph whose original or sidecar cannot be restored can be discarded here".into());
    }
    let runtime = host.photos.iter().find(|(_, photo)| photo.id == id).map(|(runtime, _)| *runtime).unwrap_or_else(PhotoId::fresh);
    host.removing = Some(runtime);
    host.send(Command::DiscardRemove(runtime, id))
}

/// Test view of the queues a refused discard must keep: (pending, restoring, ledger).
/// Only the Windows share-mode tests use it.
#[cfg(all(test, windows))]
pub(super) fn queued_admissions(app: &PhotocraftApp) -> (usize, usize, bool) {
    app.raw_filmstrip.persistence.as_ref().map_or((0, 0, false), |h| (h.pending.len(), h.restoring.len(), h.ledger_pending.is_some()))
}

pub(super) fn cancelling(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.close_prompt)
}

/// A discard was sent (or is waiting for imports to stop) but the session is not closed yet.
pub(super) fn closing(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.cancel_loading || h.finishing == Some(Finish::Discard) || h.transition == Some(Finish::Discard))
}

pub(super) fn cancel_loading(app: &mut PhotocraftApp) {
    let _ = snapshot(app);
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        // A resumed draft being restored is recovered work: ask, never discard silently.
        if unsaved_work(host) {
            host.close_prompt = true;
        } else {
            host.cancel_loading = true;
            host.frozen = true;
        }
    } else {
        super::cancel_review(app);
        super::sync_provisional(app);
        crate::camera_raw_admission::clear(app);
    }
}

pub(super) fn loading(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.persistence.as_ref().is_some_and(|h| !h.chooser && (h.cancel_loading || h.admitting || !h.pending.is_empty() || !h.restoring.is_empty()))
}

pub(super) fn done(app: &mut PhotocraftApp) -> Result<(), String> {
    finish(app, Finish::Done)
}

pub(super) fn save_edits(app: &mut PhotocraftApp) -> Result<(), String> {
    finish(app, Finish::Save)
}

pub(super) fn before_open(app: &mut PhotocraftApp) -> Result<bool, String> {
    let Some(host) = app.raw_filmstrip.persistence.as_ref() else {
        return Ok(true);
    };
    if host.open_ready {
        return Ok(true);
    }
    finish(app, Finish::Open)?;
    Ok(false)
}

pub(super) fn status(app: &PhotocraftApp) -> String {
    let Some(host) = app.raw_filmstrip.persistence.as_ref() else {
        return "Recovery cache unavailable".into();
    };
    if let Some(error) = &host.error {
        return error.clone();
    }
    if let Some((completed, total)) = host.save_progress {
        return format!("Saving XMP — {completed}/{total}");
    }
    if host.frozen && host.busy && host.finishing == Some(Finish::Open) {
        return tl!("Saving XMP before opening…").into();
    }
    if host.frozen && host.busy {
        return "Preparing or saving Camera Raw session…".into();
    }
    if !host.recovery_enabled {
        return "Long-session recovery is off".into();
    }
    if host.acknowledged != Some(host.revision) || !host.pending.is_empty() {
        return "Saving to recovery cache…".into();
    }
    let pending = host.photos.values().filter(|p| p.committed.as_ref() != Some(&p.recipe)).count();
    let saved = host.saved_time.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).and_then(|d| u64::try_from(d.as_millis()).ok()).unwrap_or(0);
    format!("Saved to recovery cache — {} · {pending} photos have changes not yet saved to XMP", local_time(saved))
}

pub(super) fn poll(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if !app.raw_filmstrip.recovery_checked {
        app.raw_filmstrip.recovery_checked = true;
        // Recovery is offered for sessions that had it on, whatever today's preference:
        // look whenever recovery is on or the private root holds any session record.
        if app.raw_filmstrip.persistence.is_none()
            && let Some(root) = app.services.camera_raw_recovery_root.clone()
            && (recovery_enabled(app) || std::fs::read_dir(&root).is_ok_and(|mut entries| entries.next().is_some()))
        {
            match Host::start(root, app.services.camera_raw_sidecar_publisher.clone(), recovery_enabled(app)) {
                Ok(host) => app.raw_filmstrip.persistence = Some(host),
                Err(error) => {
                    app.ui.status = error;
                    app.ui.status_error = true;
                }
            }
        }
    }
    let reply = app.raw_filmstrip.persistence.as_ref().map(|host| host.receiver.try_recv());
    match reply {
        Some(Ok(reply)) => {
            if !matches!(reply, Reply::SaveProgress(..) | Reply::SaveResult(..) | Reply::Notice(..) | Reply::CleanupWarning(..))
                && let Some(host) = app.raw_filmstrip.persistence.as_mut()
            {
                host.busy = false;
                host.admitting = false;
                host.save_progress = None;
            }
            match reply {
                Reply::RecoveryModeFailed(error, enabled, session) => {
                    let mode = if enabled { "cache" } else { "off" };
                    if let Err(pref_error) = app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":mode})) {
                        app.ui.status = pref_error;
                        app.ui.status_error = true;
                    }
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.mode_attempt = None;
                        host.error = Some(format!("Recovery mode was not changed: {error}"));
                        host.adopt(session);
                    }
                }
                Reply::RecoveryMode(enabled, session) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.recovery_enabled = enabled;
                        host.mode_attempt = None;
                        host.acknowledged = None;
                        host.saved_time = None;
                        host.error = None;
                        host.adopt(session);
                    }
                }
                Reply::SaveResult(receipt) => app.raw_filmstrip.last_persistence_result = Some(receipt),
                Reply::RemovalFailed(error, session, runtime, id) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        // A transient failure (for example a denied private record) is retried
                        // by "Retry recovery save"; a deterministic refusal (an unconfirmed save)
                        // is refused before sending (`discard_recovered_member`, `prepare_remove`).
                        host.error = Some(error);
                        host.removing = Some(runtime);
                        host.removal_retry = Some((runtime, id));
                        host.plan = None;
                        host.frozen = true;
                        host.adopt(session);
                    }
                }
                Reply::Retryable(error, session, command) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        let name = match &command {
                            Command::Admit(_, path, _) => path.display().to_string(),
                            Command::Restore(_, id) => session
                                .as_ref()
                                .and_then(|s| s.sources.get(id))
                                .map(|b| b.source.display().to_string())
                                .unwrap_or_else(|| "Retained photograph".into()),
                            _ => "Photograph".into(),
                        };
                        let message = format!("{name}: {error}. Correct the file problem and choose Retry recovery save.");
                        host.error = Some(message.clone());
                        if !host.notices.contains(&message) {
                            host.notices.push(message);
                        }
                        host.failed_admissions.push_back(command);
                        if let Some(session) = session {
                            host.adopt(session);
                        }
                    }
                }
                Reply::SaveProgress(completed, total) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.save_progress = Some((completed, total));
                    }
                }
                Reply::Notice(message) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut()
                        && !host.notices.contains(&message)
                    {
                        host.notices.push(message);
                    }
                }
                Reply::CleanupWarning(message) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.error = Some(message);
                        host.details = true;
                    }
                }
                Reply::PreparedRemoval(runtime, plan, session) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.removing = Some(runtime);
                        host.plan = Some(plan);
                        host.frozen = true;
                        host.adopt(session);
                    }
                }
                Reply::Removed(runtime, session) => {
                    remove_runtime(app, runtime);
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.restoring.retain(|id| !session.removed.contains(id));
                        host.failed_admissions.retain(|command| !matches!(command, Command::Restore(_, id) if session.removed.contains(id)));
                        host.photos.remove(&runtime);
                        host.removing = None;
                        host.removal_retry = None;
                        host.plan = None;
                        host.frozen = false;
                        host.error = None;
                        host.adopt(session);
                    }
                }
                Reply::Rediscovered(rows, error) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.chooser = !rows.is_empty();
                        host.chosen = rows.first().map(|row| row.id.clone());
                        host.inspection = None;
                        host.confirm_discard_recovered = false;
                        host.discovered = rows;
                        host.error = Some(error);
                    }
                }
                Reply::Discovered(rows) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.chooser = !rows.is_empty();
                        host.chosen = rows.first().map(|row| row.id.clone());
                        host.inspection = None;
                        host.confirm_discard_recovered = false;
                        host.discovered = rows;
                    }
                }
                Reply::Inspected(result) => {
                    let mut open = None;
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        let requested = host.inspecting.take();
                        if requested.is_some() && requested == host.chosen {
                            if let (Ok(detail), Some(id)) = (&result, host.open_after_inspection.take()) {
                                (detail.id == id).then(|| open = Some(id));
                            }
                            host.inspection = Some(result);
                        } else {
                            // For a session no longer chosen: inspect the chosen one afresh.
                            host.inspection = None;
                            host.open_after_inspection = None;
                        }
                    }
                    if let Some(id) = open
                        && let Err(error) = open_saved_xmp_now(app, &id)
                        && let Some(host) = app.raw_filmstrip.persistence.as_mut()
                    {
                        host.error = Some(error);
                    }
                }
                Reply::Resumed(session, report) => {
                    app.raw_filmstrip.selection.restore_ids(&[], None, None);
                    if let Some(requests) = session.view_context.get("requests").and_then(|v| serde_json::from_value(v.clone()).ok()) {
                        crate::camera_raw_admission::restore(app, requests);
                    }
                    let unavailable: Vec<_> = report
                        .issues
                        .iter()
                        .filter(|i| {
                            matches!(i.kind, domain::controller::ResumeIssueKind::SourceUnavailable | domain::controller::ResumeIssueKind::SourceChanged)
                        })
                        .filter_map(|i| i.photo.clone())
                        .collect();
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.resume_context = Some(session.view_context.clone());
                        host.chooser = false;
                        host.notices = report.issues.into_iter().map(|i| i.message).collect();
                        if report.used_fallback {
                            host.notices.push(format!("Recovered older checkpoint from {}", checkpoint_time(report.prior_checkpoint_time_ms)));
                        }
                        host.restoring = session.photos.iter().filter(|p| !unavailable.contains(&p.id)).map(|p| p.id.clone()).collect();
                        host.failed_admissions = unavailable.into_iter().map(|id| Command::Restore(PhotoId::fresh(), id)).collect();
                        if let Some(view) = session.view_context.get("filmstrip").and_then(|v| serde_json::from_value(v.clone()).ok()) {
                            app.raw_filmstrip.view = view;
                        }
                        if let Some(view) = session.view_context.get("scope").and_then(|v| serde_json::from_value(v.clone()).ok()) {
                            app.ui.camera_raw_scope = view;
                        }
                        if let Some(view) = session.view_context.get("navigation").and_then(|v| serde_json::from_value(v.clone()).ok()) {
                            app.ui.camera_raw_preview = view;
                        }
                        host.adopt(session);
                    }
                }
                Reply::Restored(runtime, id, imported, path, recipe) => {
                    let saved_view = app
                        .raw_filmstrip
                        .persistence
                        .as_ref()
                        .and_then(|h| h.session.as_ref())
                        .and_then(|s| s.view_context.get("photoViews"))
                        .and_then(|v| serde_json::from_value::<Vec<PhotoView>>(v.clone()).ok())
                        .and_then(|views| views.into_iter().find(|v| v.id == id));
                    let saved_params = recipe.adjustments.clone();
                    let document = imported.document.id;
                    let request_key = app.raw_filmstrip.persistence.as_ref().and_then(|h| h.session.as_ref()).and_then(|s| {
                        s.requested.iter().find_map(|(key, state)| {
                            matches!(state,domain::Admission::Ready(photo) | domain::Admission::Duplicate(photo) if photo == &id).then(|| key.clone())
                        })
                    });
                    if let Some(key) = &request_key {
                        crate::camera_raw_admission::bind_restored(app, document, key);
                    }
                    let name = imported.document.name.clone();
                    app.session.add_document(imported.document, None);
                    let raw =
                        RawOpen { document, photo: runtime, rating: PhotoRating::Unknown, name, path: Some(path.to_string_lossy().into_owned()), bytes: None };
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        let saved = host.session.as_ref().and_then(|s| s.photos.iter().find(|p| p.id == id));
                        let committed = saved.and_then(|p| p.committed.clone());
                        if saved.is_some_and(|p| p.selected) {
                            app.raw_filmstrip.selection.toggle(runtime);
                        }
                        if host
                            .session
                            .as_ref()
                            .and_then(|s| s.view_context.get("manuallyHidden"))
                            .and_then(Value::as_array)
                            .is_some_and(|ids| ids.iter().any(|value| serde_json::from_value::<domain::PhotoId>(value.clone()).ok().as_ref() == Some(&id)))
                        {
                            app.raw_filmstrip.manually_hidden.insert(runtime);
                        }
                        host.photos.insert(runtime, LocalPhoto { id, recipe, committed });
                        if host.failed_admissions.is_empty() {
                            host.error = None;
                        }
                        host.auto_failed_revision = None;
                    }
                    if let Some(view) = saved_view {
                        let original = original(app, runtime).cloned().unwrap_or_else(|| saved_params.clone());
                        app.raw_filmstrip.parked.push_back(ParkedRawReview {
                            raw,
                            params: saved_params,
                            original,
                            scope: view.scope,
                            navigation: view.navigation,
                            show_before: view.show_before,
                            recover: false,
                        });
                    } else if app.camera_raw.is_none() && app.pending_raw_open.is_none() {
                        app.pending_raw_open = Some(raw);
                    } else {
                        app.queued_raw_opens.push_back(raw);
                    }
                    super::sync_provisional(app);
                    app.sync_views();
                }
                Reply::Admitted(runtime, id, session, preview) => {
                    if app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.photos.values().any(|p| p.id == id)) {
                        remove_runtime(app, runtime);
                        if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                            host.adopt(session);
                        }
                        return;
                    }
                    let photo = session.photos.iter().find(|p| p.id == id).cloned();
                    if let (Some(host), Some(photo)) = (app.raw_filmstrip.persistence.as_mut(), photo) {
                        host.photos.insert(runtime, LocalPhoto { id, recipe: photo.working.clone(), committed: photo.committed });
                        if host.failed_admissions.is_empty() {
                            host.error = None;
                        }
                        host.auto_failed_revision = None;
                        host.adopt(session);
                        if let Some(d) = app.camera_raw.as_mut().filter(|d| d.raw_open.as_ref().is_some_and(|r| r.photo == runtime)) {
                            d.params = photo.working.adjustments.clone();
                            d.original = d.params.clone();
                            d.dirty = true;
                        }
                        if let Some(p) = app.raw_filmstrip.parked.iter_mut().find(|p| p.raw.photo == runtime) {
                            p.params = photo.working.adjustments;
                            p.original = p.params.clone();
                        }
                    }
                    if let Some(preview) = preview
                        && let Err(error) = replace_preview(app, ctx, runtime, preview)
                        && let Some(host) = app.raw_filmstrip.persistence.as_mut()
                    {
                        host.error = Some(error);
                        host.photos.remove(&runtime);
                    }
                }
                Reply::Saved(revision, session) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.acknowledged = (host.recovery_enabled && session.cached_generation.is_some()).then_some(revision);
                        host.saved_time = host.acknowledged.map(|_| std::time::SystemTime::now());
                        if host.failed_admissions.is_empty() {
                            host.error = None;
                        }
                        host.auto_failed_revision = None;
                        host.adopt(session);
                    }
                }
                Reply::Prepared(plan, session) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        // A semantic no-op writes nothing, so there is nothing to consent to.
                        let empty = plan.photos.is_empty().then(|| plan.id.clone());
                        host.plan = Some(plan);
                        host.adopt(session);
                        if let Some(id) = empty
                            && let Err(error) = host.send(Command::Commit(id))
                        {
                            host.error = Some(error);
                        }
                    }
                }
                Reply::EditsSaved(session) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.frozen = false;
                        host.finishing = None;
                        host.plan = None;
                        host.error = None;
                        host.auto_failed_revision = None;
                        host.save_progress = None;
                        host.adopt(session);
                    }
                    app.ui.status = tl!("Saved edits to XMP; Camera Raw remains open").into();
                    app.ui.status_error = false;
                }
                Reply::Kept(message) => {
                    let exit_requested = app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.exit_requested);
                    super::cancel_review(app);
                    app.raw_filmstrip.persistence = None;
                    super::sync_provisional(app);
                    crate::camera_raw_admission::clear(app);
                    app.ui.status = message;
                    app.ui.status_error = true;
                    if exit_requested {
                        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
                    }
                    return;
                }
                Reply::Closed(action, cleanup) => {
                    let exit_requested = app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.exit_requested);
                    super::cancel_review(app);
                    app.raw_filmstrip.persistence = None;
                    super::sync_provisional(app);
                    crate::camera_raw_admission::clear(app);
                    let status = match action {
                        Finish::Keep => "Kept Camera Raw edits in recovery and closed; no XMP was written",
                        Finish::Done => "Saved XMP and closed Camera Raw",
                        Finish::Discard => "Discarded this visit's edits; earlier recovered drafts and saved XMP are preserved",
                        Finish::Open => "Saved XMP and opened the Camera Raw images",
                        Finish::Save => "Saved edits to XMP",
                    };
                    app.ui.status = match cleanup {
                        Some(warning) => format!("{status}. {warning}"),
                        None => status.into(),
                    };
                    app.ui.status_error = false;
                    if exit_requested {
                        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
                    }
                    return;
                }
                Reply::OpenSaved(session) => {
                    // Every sidecar is saved and the session holds the refreshed bindings the
                    // re-render verifies; only now hand the whole retained batch to the editor.
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.adopt(session);
                        host.plan = None;
                        host.open_ready = true;
                        host.handoff = true;
                        host.opened = true;
                    }
                    let failed = match super::filmstrip_raws(app).first().map(|r| r.document) {
                        Some(first) => super::select_raw_review(app, ctx, first).and_then(|()| super::commit(app).map(|_| ())).err(),
                        None => None,
                    };
                    if let Some(error) = failed {
                        handoff_failed(app, error);
                    }
                }
                Reply::Returned(session) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        host.opened = false;
                        host.frozen = false;
                        host.plan = None;
                        host.finishing = None;
                        if host.removing.take().is_some() {
                            host.removals.clear();
                        }
                        host.adopt(session);
                    }
                }
                Reply::Failed(error, session, transition) => {
                    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                        if std::mem::take(&mut host.discard_in_flight) {
                            host.cancel_loading = false;
                        }
                        host.error = Some(error);
                        host.auto_failed_revision = Some(host.revision);
                        host.transition = transition;
                        // The confirmed close did not happen and no retry will finish it: the
                        // application exit it carried is given up too (an open prompt keeps its
                        // own answer; a pending discard keeps the exit for its retry).
                        if !host.close_prompt && !host.cancel_loading && transition != Some(Finish::Discard) {
                            host.exit_requested = false;
                        }
                        if transition == Some(Finish::Discard) {
                            host.pending.clear();
                            host.restoring.clear();
                            host.ledger_pending = None;
                        }
                        host.handoff = false;
                        host.open_ready = false;
                        if let Some(session) = session {
                            host.removal_retry = host.removing.and_then(|runtime| {
                                let id = &host.photos.get(&runtime)?.id;
                                session.removed.contains(id).then(|| (runtime, id.clone()))
                            });
                            host.frozen = session.phase != domain::Phase::Editing;
                            if !host.frozen {
                                // The exit did not happen; nothing is being finished any more.
                                host.finishing = None;
                            }
                            if host.removal_retry.is_some() {
                                host.frozen = true;
                                host.plan = None;
                            } else if !host.frozen {
                                host.plan = None;
                                host.removing = None;
                                host.removals.clear();
                            }
                            if transition == Some(Finish::Discard) {
                                // The discard tombstoned every member: no removal is left to
                                // retry, and "Retry recovery save" finishes the discard.
                                host.plan = None;
                                host.removing = None;
                                host.removal_retry = None;
                                host.removals.clear();
                                host.finishing = Some(Finish::Discard);
                                host.frozen = true;
                            }
                            host.adopt(session);
                        } else if !host.cancel_loading {
                            // No controller exists: nothing is being finished or frozen.
                            host.frozen = false;
                            host.finishing = None;
                        }
                    }
                }
            }
        }
        Some(Err(mpsc::TryRecvError::Disconnected)) => {
            if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                host.error = Some("Recovery worker stopped without acknowledgement; changes remain open".into());
                host.busy = false;
            }
        }
        _ => {}
    }
    if let Some(host) = app.raw_filmstrip.persistence.as_mut()
        && host.chooser
        && !host.busy
        && host.inspection.is_none()
        && let Some(id) = host.chosen.clone()
        && host.discovered.iter().any(|row| row.id == id && row.problem.is_none())
    {
        // Mark the request so a refusal is not resent every frame.
        host.inspection = Some(Err(tl!("Checking files…").into()));
        host.inspecting = Some(id.clone());
        if let Err(error) = host.send(Command::Inspect(id)) {
            host.inspecting = None;
            host.inspection = Some(Err(error));
        }
    }
    let handoff_complete = !super::raw_in_progress(app);
    let handoff_failed = app.ui.status_error && !app.raw_filmstrip.batch_open_all && app.raw_redevelop.is_none() && !handoff_complete;
    if let Some(host) = app.raw_filmstrip.persistence.as_mut()
        && host.open_ready
        && !host.busy
    {
        if handoff_complete {
            host.open_ready = false;
            if let Err(error) = host.send(Command::Leave(Finish::Open)) {
                host.error = Some(error);
            }
        } else if handoff_failed {
            let error = app.ui.status.clone();
            self::handoff_failed(app, error);
        }
    }
    let resume_view = app.raw_filmstrip.persistence.as_ref().and_then(|h| (!h.busy && h.restoring.is_empty()).then(|| h.resume_context.clone()).flatten());
    if let Some(context) = resume_view
        && app.camera_raw.is_some()
    {
        let active = context.get("active").and_then(|v| serde_json::from_value::<domain::PhotoId>(v.clone()).ok());
        let runtime = active.and_then(|id| app.raw_filmstrip.persistence.as_ref()?.photos.iter().find(|(_, p)| p.id == id).map(|(runtime, _)| *runtime));
        let document = runtime.and_then(|id| super::filmstrip_raws(app).iter().find(|r| r.photo == id).map(|r| r.document));
        if let Some(document) = document
            && let Err(error) = super::select_raw_review(app, ctx, document)
        {
            app.ui.status = error;
            app.ui.status_error = true;
        }
        if let Some(view) = context.get("scope").and_then(|v| serde_json::from_value(v.clone()).ok()) {
            app.ui.camera_raw_scope = view;
        }
        if let Some(view) = context.get("navigation").and_then(|v| serde_json::from_value(v.clone()).ok()) {
            app.ui.camera_raw_preview = view;
        }
        if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
            host.resume_context = None;
        }
    }
    if let Some(host) = app.raw_filmstrip.persistence.as_mut()
        && host.cancel_loading
        && !host.busy
    {
        if let Err(error) = host.send(Command::Leave(Finish::Discard)) {
            host.cancel_loading = false;
            host.error = Some(error);
        } else {
            host.discard_in_flight = true;
        }
        return;
    }
    let desired_recovery = recovery_enabled(app);
    let admissions_settled = crate::camera_raw_admission::settled(app);
    let order: BTreeMap<_, _> =
        super::filmstrip_raws(app).iter().map(|r| (r.photo, crate::camera_raw_admission::order_for_doc(app, r.document).unwrap_or(u64::MAX))).collect();
    if let Some(snapshot) = snapshot(app) {
        let Some(host) = app.raw_filmstrip.persistence.as_mut() else {
            return;
        };
        if !host.frozen && host.resume_context.is_none() {
            host.observe(snapshot);
        }
        if !host.busy && !host.frozen && !host.chooser && host.held_command.is_none() {
            let command = if host.recovery_enabled != desired_recovery
                && host.mode_attempt != Some(desired_recovery)
                && host.resume_context.is_none()
                && let Some(snapshot) = host.last.clone()
            {
                // Marked as attempted only when actually sent (right after a resume there is
                // no snapshot yet; the change then waits instead of being dropped).
                host.mode_attempt = Some(desired_recovery);
                Some(Command::RecoveryMode(desired_recovery, snapshot))
            } else if let Some(requests) = host.ledger_pending.take() {
                Some(Command::Requests(requests))
            } else if let Some(runtime) = host.removals.pop_front() {
                host.photos.get(&runtime).map(|p| {
                    Command::PrepareRemove(
                        runtime,
                        p.id.clone(),
                        host.last.clone().unwrap_or(Snapshot { changes: Vec::new(), views: Vec::new(), context: Value::Null }),
                    )
                })
            } else if host.recovery_enabled
                && host.auto_failed_revision != Some(host.revision)
                && host.resume_context.is_none()
                && !host.photos.is_empty()
                && host.acknowledged != Some(host.revision)
                && (host.changed.elapsed() >= Duration::from_secs(1) || host.first_dirty.elapsed() >= Duration::from_secs(5))
            {
                host.last.clone().map(|snapshot| Command::Sync(host.revision, snapshot, None, false))
            } else if let Some(id) = host.restoring.pop_front() {
                Some(Command::Restore(PhotoId::fresh(), id))
            } else if admissions_settled {
                host.pending.make_contiguous().sort_by_key(|(id, _)| order.get(id).copied().unwrap_or(u64::MAX));
                host.pending.pop_front().map(|(id, path)| {
                    let key = host.request_keys.get(&id).cloned().unwrap_or_else(|| format!("admission-{}", id.0));
                    Command::Admit(id, path, key)
                })
            } else {
                None
            };
            if let Some(command) = command {
                if matches!(command, Command::PrepareRemove(..)) {
                    host.frozen = true;
                }
                if let Err(error) = host.send(command) {
                    host.error = Some(error);
                }
            }
        }
        if host.busy || (host.recovery_enabled && host.acknowledged != Some(host.revision)) {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

/// Open Images saved every sidecar but could not hand every photo to the editor: the
/// session returns to editing, protected and still holding the saved baselines. Photos
/// that already reached the editor stay there; retrying opens only the rest.
fn handoff_failed(app: &mut PhotocraftApp, error: String) {
    let waiting = super::filmstrip_raws(app).len();
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        let moved = host.transferred.len();
        host.open_ready = false;
        host.handoff = false;
        host.finishing = None;
        host.error = Some(format!("Open Images saved every XMP file but opened only {moved} of {} photos: {error}", moved + waiting));
        if let Err(send) = host.send(Command::Back) {
            host.error = Some(send);
        }
    }
    app.ui.status = error;
    app.ui.status_error = true;
}

fn replace_preview(app: &mut PhotocraftApp, ctx: &egui::Context, id: PhotoId, imported: photocraft_io::ImportResult) -> Result<(), String> {
    let raw = super::filmstrip_raws(app).into_iter().find(|r| r.photo == id).ok_or("The admitted review was closed")?;
    let was_active = app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).is_some_and(|r| r.photo == id);
    let generation = app.session.review_generation();
    let result = super::replace_raw_document(&mut app.session, raw.document, imported, false, generation).map_err(|e| e.to_string())?;
    let document = photocraft_doc::DocId(result.get("docId").and_then(Value::as_u64).ok_or("Verified preview has no document identity")?);
    if let Some(request) = crate::camera_raw_admission::request_for_doc(app, raw.document) {
        crate::camera_raw_admission::bind_restored(app, document, &request.key);
    }
    for raw in app.pending_raw_open.iter_mut().chain(app.queued_raw_opens.iter_mut()) {
        if raw.photo == id {
            raw.document = document;
        }
    }
    for parked in &mut app.raw_filmstrip.parked {
        if parked.raw.photo == id {
            parked.raw.document = document;
        }
    }
    if was_active {
        app.camera_raw = None;
        super::open_raw(app, ctx, RawOpen { document, ..raw })?;
    }
    super::sync_provisional(app);
    app.sync_views();
    Ok(())
}

/// A checkpoint or file time in local time with its UTC offset, for example
/// "2026-10-20 10:16:05 (UTC+11:00)". Without a local zone (non-Windows builds) it is UTC,
/// labelled as such; details always also show UTC.
fn local_time(milliseconds: u64) -> String {
    #[cfg(windows)]
    if let Some(text) = windows_local_time(milliseconds) {
        return text;
    }
    checkpoint_time(milliseconds)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (bounded integer arithmetic).
#[cfg(windows)]
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(windows)]
fn windows_local_time(milliseconds: u64) -> Option<String> {
    let (year, month, day, seconds) = civil_utc(milliseconds);
    let utc = winsafe::SYSTEMTIME {
        wYear: u16::try_from(year).ok()?,
        wMonth: u16::try_from(month).ok()?,
        wDay: u16::try_from(day).ok()?,
        wHour: u16::try_from(seconds / 3600).ok()?,
        wMinute: u16::try_from(seconds / 60 % 60).ok()?,
        wSecond: u16::try_from(seconds % 60).ok()?,
        ..Default::default()
    };
    // The zone rules of that date (daylight saving included), not of today.
    let local = winsafe::SystemTimeToTzSpecificLocalTime(None, &utc).ok()?;
    let utc_minutes = days_from_civil(year, month, day) * 1440 + i64::try_from(seconds / 60).ok()?;
    let local_minutes =
        days_from_civil(i64::from(local.wYear), i64::from(local.wMonth), i64::from(local.wDay)) * 1440 + i64::from(local.wHour) * 60 + i64::from(local.wMinute);
    let offset = local_minutes - utc_minutes;
    let sign = if offset < 0 { '-' } else { '+' };
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} (UTC{sign}{:02}:{:02})",
        local.wYear,
        local.wMonth,
        local.wDay,
        local.wHour,
        local.wMinute,
        local.wSecond,
        offset.abs() / 60,
        offset.abs() % 60
    ))
}

/// UTC civil date and seconds of the day.
fn civil_utc(milliseconds: u64) -> (i64, i64, i64, u64) {
    let days = (milliseconds / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day, milliseconds / 1000 % 86_400)
}

fn checkpoint_time(milliseconds: u64) -> String {
    let (year, month, day, seconds) = civil_utc(milliseconds);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", seconds / 3600, seconds / 60 % 60, seconds % 60)
}

pub(super) fn dialogs(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some(host) = app.raw_filmstrip.persistence.as_ref() else {
        return;
    };
    let plan = host.plan.as_ref().map(|p| {
        (p.id.clone(), p.photos.len(), p.photos.iter().filter(|p| p.binding.original.is_some()).count(), p.photos.iter().filter(|p| p.hidden).count())
    });
    let close = host.close_prompt;
    let busy = host.busy;
    let chooser = host.chooser;
    let removing = host.removing;
    let save_only = host.plan.as_ref().is_some_and(|p| p.scope == domain::commit::SaveScope::SaveEdits);
    let opening = host.finishing == Some(Finish::Open);
    let mut choice = None;
    let mut retry = false;
    let mut dismiss_details = false;
    let mut open_details = false;
    let mut skip_request = None;
    let requests = crate::camera_raw_admission::snapshot(app);
    let skippable: BTreeMap<_, _> = requests
        .iter()
        .filter_map(|row| match row.outcome {
            crate::camera_raw_admission::Outcome::Pending => Some((row.key.clone(), crate::camera_raw_admission::can_cancel_request(app, &row.key))),
            // Opened, but its admission was refused: skipping it lets the others be saved.
            crate::camera_raw_admission::Outcome::Ready if unadmitted(app, &row.key) => {
                Some((row.key.clone(), crate::camera_raw_admission::can_skip_unadmitted(app, &row.key)))
            }
            _ => None,
        })
        .collect();
    let mut discard_source = None;
    if !super::loading(app) && !host.details && !chooser && host.error.is_some() {
        // Above the full-screen Camera Raw dialog (itself a foreground area), every frame.
        let attention = egui::Id::new("camera-raw-recovery-attention");
        ctx.move_to_top(egui::LayerId::new(egui::Order::Foreground, attention));
        egui::Window::new(tl!("Camera Raw recovery needs attention")).id(attention).order(egui::Order::Foreground).collapsible(false).show(ctx, |ui| {
            ui.label(tl!("Review unavailable files, interrupted imports, and recovery messages before finishing."));
            if ui.button(tl!("Show recovery details")).clicked() {
                open_details = true;
            }
        });
    }
    if host.details {
        // A modal, so it is drawn above the Camera Raw dialog rather than hidden beneath it.
        egui::Modal::new(egui::Id::new("camera-raw-recovery-details")).show(ctx, |ui| {
            ui.heading(tl!("Camera Raw recovery details"));
            if let Some(error) = &host.error {
                ui.label(error);
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for notice in &host.notices { ui.label(notice); }
                for row in &requests {
                    ui.push_id(&row.key, |ui| {
                        ui.separator();
                        ui.strong(&row.name);
                        if let Some(path) = &row.path { ui.label(path); }
                        use crate::camera_raw_admission::Outcome;
                        ui.label(match &row.outcome {
                            Outcome::Pending => "Import has not finished".to_owned(),
                            Outcome::Ready => "Opened in Camera Raw".to_owned(),
                            Outcome::OutsideRaw => "Opened outside Camera Raw".to_owned(),
                            Outcome::Failed(error) => format!("Could not open: {error}"),
                            Outcome::Cancelled => "Import cancelled or skipped".to_owned(),
                        });
                        if let Some(eligibility) = skippable.get(&row.key) {
                            ui.label(tl!("Skip removes this unfinished import from the current request. You can open the file again later."));
                            let button = ui.add_enabled(!busy && eligibility.is_ok(), egui::Button::new("Skip this unfinished import"));
                            if button.clicked() { skip_request = Some(row.key.clone()); }
                            if let Err(reason) = eligibility { button.on_disabled_hover_text(reason); }
                        }
                    });
                }
                if let Some(session) = &host.session {
                    for (key, state) in &session.requested {
                        if !matches!(state, domain::Admission::Unresolved(_) | domain::Admission::Rejected(_)) { continue; }
                        ui.separator();
                        if let Some(row) = requests.iter().find(|row| row.key == *key) {
                            ui.strong(&row.name);
                            if let Some(path) = &row.path { ui.label(path); }
                        } else if let Some(id) = key.strip_prefix("source-") {
                            if let Some((_, binding)) = session.sources.iter().find(|(photo, _)| photo.0 == id) {
                                ui.strong(binding.source.display().to_string());
                            } else { ui.label(tl!("A recovered photograph has no retained source binding.")); }
                        } else { ui.label(key); }
                        match state {
                            domain::Admission::Unresolved(reason) | domain::Admission::Rejected(reason) => { ui.label(reason); }
                            _ => {}
                        }
                        if let Some(id) = key.strip_prefix("source-") {
                            ui.label(tl!("Restore the original file at the recorded path, then choose Retry recovery save. Changed file contents or identity cannot be accepted as the original source."));
                            if session.photos.iter().any(|photo| photo.id.0 == id && !photo.unknown_operation)
                                && ui.add_enabled(!busy && !host.frozen, egui::Button::new(tl!("Discard this photo's changes and remove"))).clicked()
                            {
                                discard_source = Some(domain::PhotoId(id.to_owned()));
                            }
                        }
                    }
                }
            });
            if ui.add_enabled(!busy, egui::Button::new(tl!("Retry recovery save"))).clicked() {
                retry = true;
            }
            if ui.button(tl!("Close details")).clicked() {
                dismiss_details = true;
            }
        });
    }
    let mut action = None;
    if !super::loading(app) && !chooser && !close && plan.is_none() && host.session.is_some() && super::filmstrip_raws(app).is_empty() {
        egui::Window::new(tl!("Camera Raw session")).anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO).collapsible(false).show(ctx, |ui| {
            ui.label(tl!("No photographs are currently available to preview. Recovery details retain any unresolved work."));
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy && !host.frozen, egui::Button::new(tl!("Done"))).clicked() {
                    action = Some("done");
                }
                // A pending close decision (for example a failed Open Images record) never
                // leaves this window without an exit.
                if ui
                    .add_enabled(
                        !busy && (!host.frozen || host.transition.is_some() || host.removal_retry.is_some()),
                        egui::Button::new(tl!("Close Camera Raw session")),
                    )
                    .clicked()
                {
                    action = Some("ask-close");
                }
            });
        });
    }
    if chooser {
        choice = recovery_chooser(host, ctx, busy);
    } else if removing.is_some() && plan.is_some() {
        egui::Modal::new(egui::Id::new("remove-camera-raw-photo")).show(ctx, |ui| {
            ui.heading(tl!("Remove photograph from session"));
            if let Some(path) =
                removing.and_then(|id| host.photos.get(&id)).and_then(|p| host.session.as_ref()?.sources.get(&p.id)).map(|b| b.source.display().to_string())
            {
                ui.label(path);
            }
            ui.label(tl!("This changes session membership. Hide and filters keep photographs in the session."));
            ui.add_enabled_ui(!busy, |ui| {
                if ui.button(tl!("Save XMP and remove")).clicked() {
                    action = Some("commit");
                }
                if ui.button(tl!("Discard this photo's changes and remove")).clicked() {
                    action = Some("discard-photo");
                }
                if ui.button(tl!("Keep in session")).clicked() {
                    action = Some("back");
                }
            });
        });
    } else if let Some((_, count, existing, hidden)) = &plan {
        egui::Modal::new(egui::Id::new("finish-camera-raw")).show(ctx, |ui| {
            ui.heading(if save_only {
                tl!("Save edits")
            } else if opening {
                tl!("Save and open images")
            } else {
                tl!("Finish Camera Raw session")
            });
            ui.label(format!("{count} photos have changes to save.\nUpdate {existing} existing XMP files and create {} new XMP files.", count - existing));
            if *hidden > 0 {
                ui.label(format!("Includes {hidden} photos hidden by the current view."));
            }
            ui.label(tl!("Original image files remain unchanged."));
            if opening {
                ui.label(tl!("After saving, every photo in this session opens in the editor in the order it was opened, whatever the filter or selection."));
            }
            egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                if let Some(plan) = &host.plan {
                    for photo in &plan.photos {
                        ui.label(photo.binding.target.display().to_string());
                    }
                }
            });
            ui.add_enabled_ui(!busy, |ui| {
                if host.recovery_enabled && !opening && !save_only && ui.button(tl!("Keep in recovery and close")).clicked() {
                    action = Some("keep");
                }
                if let Some(role) = crate::widgets::dialog_buttons(
                    ui,
                    &[
                        crate::widgets::DialogButton::new(
                            crate::widgets::ButtonRole::Default,
                            if save_only {
                                tl!("Save XMP and keep editing")
                            } else if opening {
                                tl!("Save XMP and open images")
                            } else {
                                tl!("Save XMP and close")
                            },
                            170.0,
                        ),
                        crate::widgets::DialogButton::new(crate::widgets::ButtonRole::Cancel, tl!("Back to editing"), 120.0),
                    ],
                ) {
                    action = Some(if role == crate::widgets::ButtonRole::Default { "commit" } else { "back" });
                }
            });
        });
    } else if close {
        egui::Modal::new(egui::Id::new("close-camera-raw")).show(ctx, |ui| {
            ui.heading(tl!("Discard unsaved edits?"));
            ui.label(tl!("Discard this visit's changes and close. Earlier recovered drafts and XMP files saved during this visit stay preserved."));
            ui.add_enabled_ui(!busy, |ui| {
                if host.recovery_enabled && ui.button(tl!("Keep in recovery and close")).clicked() {
                    action = Some("keep");
                }
                if let Some(role) = crate::widgets::dialog_buttons(
                    ui,
                    &[
                        crate::widgets::DialogButton::new(crate::widgets::ButtonRole::Alternate, tl!("Discard unsaved edits and close"), 210.0),
                        crate::widgets::DialogButton::new(crate::widgets::ButtonRole::Default, tl!("Back to editing"), 120.0),
                    ],
                ) {
                    action = Some(if role == crate::widgets::ButtonRole::Alternate { "discard" } else { "editing" });
                }
            });
        });
    }
    if action.is_none() && !busy && removing.is_none() && (plan.is_some() || close) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
    {
        action = Some(if plan.is_some() { "commit" } else { "editing" });
    }
    // Escape closes the recovery details first; it never cancels the session underneath.
    if host.details && !close && plan.is_none() && !chooser && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        dismiss_details = true;
    }
    if (plan.is_some() || close) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        action = Some(if plan.is_some() { "back" } else { "editing" });
    }
    let result = match action {
        Some("done") => done(app),
        Some("keep") => finish(app, Finish::Keep),
        Some("ask-close") => {
            request_close(app);
            Ok(())
        }
        Some("discard-photo") => discard_photo(app),
        Some("files") => open_other_files(app),
        Some("cancel-opening") => cancel_opening(app),
        Some("commit") => match (app.raw_filmstrip.persistence.as_mut(), plan) {
            (Some(host), Some((id, ..))) => host.send(Command::Commit(id)),
            _ => Ok(()),
        },
        Some("back") => app.raw_filmstrip.persistence.as_mut().map_or(Ok(()), |host| host.send(Command::Back)),
        Some("discard") => finish(app, Finish::Discard),
        Some("editing") => {
            if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
                host.close_prompt = false;
                host.exit_requested = false;
            }
            Ok(())
        }
        _ => Ok(()),
    };
    if let Err(error) = result {
        app.ui.status = error;
        app.ui.status_error = true;
    }
    if (dismiss_details || open_details)
        && let Some(host) = app.raw_filmstrip.persistence.as_mut()
    {
        host.details = open_details;
    }
    if let Some(id) = discard_source {
        match discard_recovered_member(app, id) {
            Ok(()) => {
                app.ui.status = "Discarding that photograph's recovered changes and removing it from the session".into();
                app.ui.status_error = false;
            }
            Err(error) => {
                app.ui.status = error;
                app.ui.status_error = true;
            }
        }
    }
    if let Some(key) = skip_request {
        match crate::camera_raw_admission::skip(app, &key) {
            Ok(()) => {
                app.ui.status = "Skipped unfinished import; saving the updated recovery request".into();
                app.ui.status_error = false;
            }
            Err(error) => {
                app.ui.status = error;
                app.ui.status_error = true;
            }
        }
    }
    if retry && let Err(error) = retry_cache(app) {
        app.ui.status = error;
        app.ui.status_error = true;
    }
    if let Some(choice) = choice
        && let Err(error) = chooser_action(app, choice)
    {
        app.ui.status = error.clone();
        app.ui.status_error = true;
        if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
            host.error = Some(error);
        }
    }
}

/// What the photographer chose in the interrupted-session chooser.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Choice {
    Select(String),
    ToggleFileDetails,
    ResumeCache(String),
    OpenSavedXmp(String),
    AskDiscard,
    KeepRecovered,
    DiscardRecovered(String),
    OpenOtherFiles,
    CancelOpening,
}

const CARD_A_TIP: &str = "Use this for work kept in recovery, after PhotoCraft closed unexpectedly, or when an Open Images, Done, or Cancel operation was interrupted. It may restore work that was never saved to an XMP sidecar. Changes since the last acknowledged checkpoint may not be available.";
const CARD_B_TIP: &str = "Choose this to use the saved sidecar versions instead of the unsaved recovery checkpoint. Not every photograph may have an XMP file; those without one use their current file/default settings. This does not restore unsaved cached changes.";

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// A recorded source path for display and reopening, without the `\\?\` prefix.
fn plain_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| text.strip_prefix(r"\\?\").map(str::to_owned))
        .unwrap_or_else(|| text.into_owned())
}

/// "Recover Camera Raw session": one selected interrupted session at a time, two equal,
/// independently actionable cards (recovered cache edits; saved XMP edits read from disk),
/// and the ordinary ways out. Read-only until the photographer chooses.
fn recovery_chooser(host: &Host, ctx: &egui::Context, busy: bool) -> Option<Choice> {
    use domain::store::{SidecarState, SourceState};
    let mut choice = None;
    // While only the read-only file check runs (it can be slow on an unreachable share),
    // the photographer can still leave: Open other files, Cancel opening and Escape.
    let leave_ok = host.inspecting.is_some();
    let popup_was_open = egui::Popup::is_any_open(ctx);
    let row = host.chosen.as_ref().and_then(|id| host.discovered.iter().find(|row| &row.id == id)).or_else(|| host.discovered.first());
    let height = (ctx.content_rect().height() - 96.0).max(160.0);
    // A definite width: a modal is otherwise as wide as its widest row, and the cards and
    // buttons size themselves from the width they are given.
    let width = (ctx.content_rect().width() - 64.0).clamp(240.0, 760.0);
    egui::Modal::new(egui::Id::new("resume-camera-raw")).show(ctx, |ui| {
        ui.set_width(width);
        ui.heading(tl!("Recover Camera Raw session"));
        egui::ScrollArea::vertical().max_height(height).auto_shrink([false, true]).show(ui, |ui| {
            ui.set_width(width - 16.0);
            ui.add_enabled_ui(!busy || leave_ok, |ui| {
                if host.confirm_discard_recovered {
                    ui.strong(tl!("Discard recovered copy?"));
                    ui.label(tl!(
                        "This permanently removes the recovery checkpoint of this session. Saved XMP files and original photographs are not changed."
                    ));
                    ui.horizontal(|ui| {
                        if let Some(row) = row
                            && ui.button(tl!("Discard recovered copy")).clicked()
                        {
                            choice = Some(Choice::DiscardRecovered(row.id.clone()));
                        }
                        if ui.button(tl!("Keep recovered copy")).clicked() {
                            choice = Some(Choice::KeepRecovered);
                        }
                    });
                    return;
                }
                ui.label(tl!("An earlier Camera Raw session did not finish normally. Choose which version of its edits to open."));
                let summary = |row: &domain::store::RecoveryCandidate| {
                    let count = row.photo_count.map_or_else(|| "?".into(), |n| n.to_string());
                    let time = row.checkpoint_time_ms.map_or_else(|| "?".into(), local_time);
                    crate::i18n::fmt(tl!("{count} photos · last checkpoint {time}"), &[("count", &count), ("time", &time)])
                };
                if host.discovered.len() > 1 {
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(tl!("Session"));
                            egui::ComboBox::from_id_salt("camera-raw-recovery-session").selected_text(row.map(summary).unwrap_or_default()).show_ui(ui, |ui| {
                                for candidate in &host.discovered {
                                    let selected = row.is_some_and(|row| row.id == candidate.id);
                                    if ui.selectable_label(selected, summary(candidate)).clicked() && !selected {
                                        choice = Some(Choice::Select(candidate.id.clone()));
                                    }
                                }
                            });
                        })
                    });
                }
                let Some(row) = row else { return };
                ui.strong(summary(row));
                let names: Vec<_> = row.source_context.iter().map(|path| file_name(std::path::Path::new(path))).collect();
                if !names.is_empty() {
                    let more = row.photo_count.is_some_and(|n| n > names.len());
                    ui.label(format!("{}{}", names.join(", "), if more { ", …" } else { "" }));
                }
                if let Some(unsaved) = row.unsaved_photos.filter(|n| *n > 0) {
                    ui.label(crate::i18n::fmt(tl!("{count} photos have edits not saved to XMP"), &[("count", &unsaved.to_string())]));
                }
                if let Some(time) = row.checkpoint_time_ms.filter(|_| row.newer_unusable_time_ms.is_some()) {
                    ui.label(crate::i18n::fmt(tl!("The newest checkpoint is unusable; the one from {time} is used instead."), &[("time", &local_time(time))]));
                }
                if row.operations > 0 {
                    ui.label(tl!("An XMP save in this session did not finish. Its files are checked again before anything else is written."));
                }
                if let Some(problem) = &row.problem {
                    ui.colored_label(ui.visuals().error_fg_color, problem);
                }
                // A refused Resume or Discard is reported here, where the choice was made.
                if let Some(error) = &host.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                let detail = host.inspection.as_ref().and_then(|result| result.as_ref().ok()).filter(|detail| detail.id == row.id);
                let mut sources_available = false;
                match (&host.inspection, detail) {
                    (_, Some(detail)) => {
                        let missing = detail.photos.iter().filter(|p| p.source == SourceState::MissingOrChanged).count();
                        let no_xmp = detail.photos.iter().filter(|p| p.sidecar == SidecarState::Absent).count();
                        let changed = detail.photos.iter().filter(|p| matches!(p.sidecar, SidecarState::Changed | SidecarState::Unreadable)).count();
                        sources_available = missing < detail.photos.len();
                        if missing > 0 {
                            ui.label(crate::i18n::fmt(
                                tl!("{n} of {total} photos are missing or changed"),
                                &[("n", &missing.to_string()), ("total", &detail.photos.len().to_string())],
                            ));
                        }
                        if no_xmp > 0 {
                            ui.label(crate::i18n::fmt(tl!("{n} photos have no XMP file"), &[("n", &no_xmp.to_string())]));
                        }
                        if changed > 0 {
                            ui.label(crate::i18n::fmt(tl!("{n} XMP files changed after the checkpoint"), &[("n", &changed.to_string())]));
                            ui.label(tl!("Some XMP files changed after the checkpoint, for example in Lightroom. Review them before choosing."));
                        }
                        let toggle = if host.file_details { tl!("Hide file details") } else { tl!("Show file details") };
                        if ui.button(toggle).clicked() {
                            choice = Some(Choice::ToggleFileDetails);
                        }
                        if host.file_details {
                            egui::ScrollArea::vertical().id_salt("camera-raw-recovery-files").max_height(160.0).show(ui, |ui| {
                                for photo in &detail.photos {
                                    let source = if photo.source == SourceState::Available { tl!("Photo available") } else { tl!("Photo missing or changed") };
                                    let sidecar = match photo.sidecar {
                                        SidecarState::Absent => tl!("No XMP file"),
                                        SidecarState::AsCheckpoint => tl!("XMP as at the checkpoint"),
                                        SidecarState::Changed => tl!("XMP changed after the checkpoint"),
                                        SidecarState::Unreadable => tl!("XMP unreadable"),
                                        SidecarState::SavedByThisSession => tl!("Saved edits"),
                                    };
                                    let modified =
                                        photo.sidecar_modified_ms.map(|ms| format!(" · {} · {}", local_time(ms), checkpoint_time(ms))).unwrap_or_default();
                                    ui.label(format!("{} — {source} · {sidecar}{modified}", file_name(&photo.path))).on_hover_text(plain_path(&photo.path));
                                }
                                if let Some(time) = row.checkpoint_time_ms {
                                    ui.label(checkpoint_time(time));
                                }
                            });
                        }
                    }
                    (Some(Err(error)), None) => {
                        ui.label(error.as_str());
                    }
                    _ => {
                        ui.label(tl!("Checking files…"));
                    }
                }
                ui.add_space(8.0);
                let cache_ok = row.problem.is_none();
                let xmp_ok = detail.is_some() && sources_available;
                let width = ui.available_width();
                let stacked = width < 520.0;
                let card_width = if stacked { width } else { (width - 12.0) / 2.0 };
                let card = |ui: &mut egui::Ui, small: &str, title: &str, text: &str, button: &str, tip: &str, enabled: bool, reason: Option<&str>| -> bool {
                    let mut clicked = false;
                    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                        // Top-down inside the card, whatever the row layout, so its text wraps.
                        ui.vertical(|ui| {
                            ui.set_width((card_width - 22.0).max(120.0));
                            ui.small(small);
                            ui.strong(title);
                            ui.label(text);
                            ui.add_space(6.0);
                            let response = ui.add_enabled(enabled, egui::Button::new(button).wrap().min_size(egui::vec2((card_width - 22.0).max(120.0), 34.0)));
                            let response = response.on_hover_text(tip);
                            let response = match reason {
                                Some(reason) if !enabled => response.on_disabled_hover_text(reason),
                                _ => response,
                            };
                            clicked = response.clicked();
                        });
                    });
                    clicked
                };
                let layout = if stacked { egui::Layout::top_down(egui::Align::Min) } else { egui::Layout::left_to_right(egui::Align::Min) };
                ui.with_layout(layout, |ui| {
                    if card(
                        ui,
                        tl!("Cache version"),
                        tl!("Recovered edits"),
                        tl!("Restore edits from the latest usable PhotoCraft recovery checkpoint."),
                        tl!("Resume recovered edits"),
                        tl!(CARD_A_TIP),
                        cache_ok && !busy,
                        Some(tl!("No usable checkpoint can be opened.")),
                    ) {
                        choice = Some(Choice::ResumeCache(row.id.clone()));
                    }
                    if card(
                        ui,
                        tl!("XMP sidecar version"),
                        tl!("Saved edits"),
                        tl!("Reopen the original photographs with the supported edits currently saved in their XMP files."),
                        tl!("Open saved XMP edits"),
                        tl!(CARD_B_TIP),
                        xmp_ok && !busy,
                        Some(if detail.is_some() { tl!("None of the photographs is available.") } else { tl!("Checking files…") }),
                    ) {
                        choice = Some(Choice::OpenSavedXmp(row.id.clone()));
                    }
                });
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button(tl!("Open other files")).clicked() {
                        choice = Some(Choice::OpenOtherFiles);
                    }
                    if ui.button(tl!("Cancel opening")).clicked() {
                        choice = Some(Choice::CancelOpening);
                    }
                    if ui.add_enabled(!busy, egui::Button::new(tl!("Discard recovered copy…"))).clicked() {
                        choice = Some(Choice::AskDiscard);
                    }
                });
            });
        });
    });
    // Escape never chooses a version: it backs out of a confirmation, else cancels opening.
    if choice.is_none()
        && (!busy || leave_ok)
        && !popup_was_open
        && !egui::Popup::is_any_open(ctx)
        && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        choice = Some(if host.confirm_discard_recovered { Choice::KeepRecovered } else { Choice::CancelOpening });
    }
    choice
}

fn open_other_files(app: &mut PhotocraftApp) -> Result<(), String> {
    // An ordinary new browse: the interrupted session stays on disk, unchanged.
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        host.chooser = false;
        host.confirm_discard_recovered = false;
        // A file check still in flight must not reopen the recovered photos over the new
        // browse: its late reply is dropped (see Reply::Inspected).
        host.inspecting = None;
        host.open_after_inspection = None;
    }
    Ok(())
}

fn cancel_opening(app: &mut PhotocraftApp) -> Result<(), String> {
    // Dismiss the chooser and the incoming review; the interrupted draft is not touched.
    super::cancel_review(app);
    super::sync_provisional(app);
    app.raw_filmstrip.persistence = None;
    Ok(())
}

fn chooser_action(app: &mut PhotocraftApp, choice: Choice) -> Result<(), String> {
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
    if !host.chooser {
        return Err("No interrupted session is being offered".into());
    }
    let local = matches!(choice, Choice::ToggleFileDetails | Choice::AskDiscard | Choice::KeepRecovered);
    let leaving = matches!(choice, Choice::OpenOtherFiles | Choice::CancelOpening) && host.inspecting.is_some();
    if host.busy && !local && !leaving {
        return Err("Wait for the current recovery check to finish".into());
    }
    if !local {
        host.error = None;
    }
    let known = |host: &Host, id: &str| host.discovered.iter().any(|row| row.id == id);
    match choice {
        Choice::Select(id) if known(host, &id) => {
            host.chosen = Some(id);
            host.inspection = None;
            host.confirm_discard_recovered = false;
            Ok(())
        }
        Choice::ToggleFileDetails => {
            host.file_details = !host.file_details;
            Ok(())
        }
        Choice::AskDiscard => {
            host.confirm_discard_recovered = true;
            Ok(())
        }
        Choice::KeepRecovered => {
            host.confirm_discard_recovered = false;
            Ok(())
        }
        Choice::DiscardRecovered(id) if known(host, &id) && host.confirm_discard_recovered => {
            host.confirm_discard_recovered = false;
            let listed = host.discovered.iter().find(|row| row.id == id).and_then(|row| row.listed.clone());
            host.send(Command::DiscardRecovered(id, listed))
        }
        Choice::ResumeCache(id) if known(host, &id) => resume(app, id),
        Choice::OpenSavedXmp(id) if known(host, &id) => open_saved_xmp(app, &id),
        Choice::OpenOtherFiles => open_other_files(app),
        Choice::CancelOpening => cancel_opening(app),
        _ => Err("Choose an interrupted session that is still offered".into()),
    }
}

/// "Open saved XMP edits": re-check the session's files first (a photograph may have been
/// replaced while the chooser was open) and open from that fresh check only.
fn open_saved_xmp(app: &mut PhotocraftApp, id: &str) -> Result<(), String> {
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
    if !host.inspection.as_ref().is_some_and(|r| r.as_ref().is_ok_and(|d| d.id == id)) {
        return Err("Wait for the session's files to be checked".into());
    }
    host.chosen = Some(id.to_owned());
    host.inspection = Some(Err(tl!("Checking files…").into()));
    host.inspecting = Some(id.to_owned());
    host.open_after_inspection = Some(id.to_owned());
    if let Err(error) = host.send(Command::Inspect(id.to_owned())) {
        host.inspecting = None;
        host.open_after_inspection = None;
        host.inspection = None;
        return Err(error);
    }
    Ok(())
}

/// Reopen exactly the recorded originals through the explicit Camera Raw route, so each
/// photo's current sidecar is read live with the usual binding, metadata and conflict
/// checks, whatever today's File Handling preference (layered TIFF still stays in the
/// editor). The recovery copy is left as it is until the photographer discards it.
fn open_saved_xmp_now(app: &mut PhotocraftApp, id: &str) -> Result<(), String> {
    let host = app.raw_filmstrip.persistence.as_ref().ok_or("Recovery unavailable")?;
    let Some(Ok(detail)) = host.inspection.as_ref().filter(|r| r.as_ref().is_ok_and(|d| d.id == id)) else {
        return Err("Wait for the session's files to be checked".into());
    };
    let paths: Vec<String> = detail.photos.iter().filter(|p| p.source == domain::store::SourceState::Available).map(|p| plain_path(&p.path)).collect();
    let missing: Vec<String> = detail.photos.iter().filter(|p| p.source != domain::store::SourceState::Available).map(|p| file_name(&p.path)).collect();
    if paths.is_empty() {
        return Err(tl!("None of the photographs is available.").into());
    }
    super::cancel_review(app);
    crate::camera_raw_admission::clear(app);
    super::sync_provisional(app);
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        host.chooser = false;
        host.pending.clear();
        host.ledger_pending = None;
        host.request_keys.clear();
        host.photos.clear();
        host.last = None;
        if !missing.is_empty() {
            host.notices.push(format!("Not reopened because the original is missing or changed: {}", missing.join(", ")));
        }
    }
    app.open_paths_mode(&paths, true);
    Ok(())
}

pub(super) fn show_details(app: &mut PhotocraftApp) {
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        host.details = true;
    }
}

fn resume(app: &mut PhotocraftApp, id: String) -> Result<(), String> {
    let host = app.raw_filmstrip.persistence.as_ref().ok_or("Recovery unavailable")?;
    if host.busy || !host.chooser || !host.discovered.iter().any(|row| row.id == id) {
        return Err("Choose an available recovery session first".into());
    }
    // Fence all provisional imports before replacing their ledger. The GUI and
    // control channel must perform the same transition into the old draft.
    super::cancel_review(app);
    crate::camera_raw_admission::clear(app);
    super::sync_provisional(app);
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
    host.pending.clear();
    host.ledger_pending = None;
    host.request_keys.clear();
    host.photos.clear();
    host.last = None;
    host.send(Command::Resume(id))
}

fn retry_cache(app: &mut PhotocraftApp) -> Result<(), String> {
    if let Some(host) = app.raw_filmstrip.persistence.as_mut() {
        // A pending close decision outranks a removal retry: a discard already tombstoned
        // every member, so only finishing it can complete.
        if let Some(action) = host.transition {
            return host.send(Command::Leave(action));
        }
        if let Some((runtime, id)) = host.removal_retry.clone() {
            return host.send(Command::DiscardRemove(runtime, id));
        }
        if host.busy {
            return Err("Wait for the current recovery operation".into());
        }
        if let Some(command) = host.failed_admissions.pop_front() {
            return host.send(command);
        }
    }
    if let Some(command) = app.raw_filmstrip.persistence.as_mut().and_then(|h| h.held_command.take()) {
        return app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?.send(command);
    }
    let snapshot = snapshot(app).ok_or("Recovery unavailable")?;
    let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
    if host.frozen {
        return Err("Return to editing or keep the frozen session before retrying".into());
    }
    host.observe(snapshot.clone());
    host.send(Command::Sync(host.revision, snapshot, None, true))
}

pub(super) fn control(app: &mut PhotocraftApp, request: &Value) -> Result<Value, String> {
    let action = request.get("action").and_then(Value::as_str).ok_or("A persistence action is required")?;
    if action != "inspect" && app.raw_filmstrip.persistence.as_ref().is_some_and(|h| h.handoff) {
        return Err("Wait for Open Images to finish before choosing another session action".into());
    }
    match action {
        "skipRequest" => {
            let key = request.get("requestKey").and_then(Value::as_str).ok_or("An exact unfinished request identity is required")?;
            crate::camera_raw_admission::skip(app, key)?;
        }
        // Recovery details' "Discard this photo's changes and remove", for a photograph whose
        // original or sidecar cannot be restored.
        "discardMember" => {
            if request.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err("Discarding a photograph's changes needs confirm: true".into());
            }
            let id = request.get("photoId").and_then(Value::as_str).ok_or("The photograph's session identity (photos[].id) is required")?;
            discard_recovered_member(app, domain::PhotoId(id.to_owned()))?;
        }
        // Cancel as the GUI does it: asks (describe().cancelPrompt) whenever anything is unsaved.
        "cancel" => {
            if super::loading(app) {
                cancel_loading(app);
            } else {
                // As the Camera Raw Cancel: asks when anything is unsaved, else closes.
                super::cancel(app);
            }
        }
        "remove" => request_remove(app, vec![PhotoId(request.get("photo").and_then(Value::as_u64).ok_or("A photo identity is required")?)])?,
        "discardPhoto" => discard_photo(app)?,
        "inspect" => {}
        "done" => done(app)?,
        "keep" => finish(app, Finish::Keep)?,
        "saveEdits" => save_edits(app)?,
        "retryCache" => retry_cache(app)?,
        "discard" => {
            let host = app.raw_filmstrip.persistence.as_ref().ok_or("Recovery unavailable")?;
            if host.chooser || host.plan.is_some() {
                return Err("Finish the current decision first".into());
            }
            let confirmed = host.close_prompt || request.get("confirm").and_then(Value::as_bool) == Some(true);
            if !confirmed && unsaved(app) {
                return Err("Discarding unsaved edits needs the Cancel confirmation (send cancel first) or confirm: true".into());
            }
            finish(app, Finish::Discard)?
        }
        // The Cancel prompt's "Back to editing".
        "keepEditing" => {
            let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
            if !host.close_prompt {
                return Err("No discard confirmation is open".into());
            }
            host.close_prompt = false;
            host.exit_requested = false;
        }
        "back" => {
            let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
            if host.plan.is_none() || host.transition.is_some() {
                return Err("Back is available only while a reviewed plan is shown".into());
            }
            host.send(Command::Back)?
        }
        "save" => {
            let id = request.get("planId").and_then(Value::as_str).ok_or("The reviewed plan identity is required")?;
            let host = app.raw_filmstrip.persistence.as_mut().ok_or("Recovery unavailable")?;
            if host.plan.as_ref().is_none_or(|p| p.id != id) {
                return Err("The reviewed plan is no longer current".into());
            }
            host.send(Command::Commit(id.to_owned()))?;
        }
        "openFiles" | "openOtherFiles" => chooser_action(app, Choice::OpenOtherFiles)?,
        "cancelOpening" => chooser_action(app, Choice::CancelOpening)?,
        "resume" | "resumeRecovered" => {
            let id = request.get("sessionId").and_then(Value::as_str).ok_or("Choose an interrupted session")?.to_owned();
            chooser_action(app, Choice::ResumeCache(id))?;
        }
        "inspectSession" => {
            let id = request.get("sessionId").and_then(Value::as_str).ok_or("Choose an interrupted session")?.to_owned();
            chooser_action(app, Choice::Select(id))?;
        }
        "openSavedXmp" => {
            let id = request.get("sessionId").and_then(Value::as_str).ok_or("Choose an interrupted session")?.to_owned();
            chooser_action(app, Choice::OpenSavedXmp(id))?;
        }
        "discardRecovered" => {
            // The control channel names the session and its confirmation in one request.
            let id = request.get("sessionId").and_then(Value::as_str).ok_or("Choose an interrupted session")?.to_owned();
            if request.get("confirm").and_then(Value::as_bool) != Some(true) {
                return Err("Discarding a recovered copy needs confirm: true".into());
            }
            if !app.raw_filmstrip.persistence.as_ref().is_some_and(|host| host.discovered.iter().any(|row| row.id == id)) {
                return Err("Choose an interrupted session that is still offered".into());
            }
            chooser_action(app, Choice::AskDiscard)?;
            chooser_action(app, Choice::DiscardRecovered(id))?;
        }
        _ => return Err("Unknown Camera Raw persistence action".into()),
    }
    Ok(describe(app))
}

pub(super) fn describe(app: &PhotocraftApp) -> Value {
    let Some(host) = app.raw_filmstrip.persistence.as_ref() else {
        return json!({"available":false,"lastSave":app.raw_filmstrip.last_persistence_result});
    };
    json!({"available":true,"recoveryEnabled":host.recovery_enabled,"busy":host.busy,"frozen":host.frozen,"status":status(app),"error":host.error,
        "lastSave":app.raw_filmstrip.last_persistence_result,
        "revision":host.revision,"acknowledged":host.acknowledged,"session":host.session.as_ref().map(|s|&s.id),
        "nativeRequests":host.session.as_ref().map(|s|&s.requested),"failedAdmissionCount":host.failed_admissions.len(),"handoff":host.handoff,
        "plan":host.plan,"finishing":host.finishing,"transition":host.transition,"cancelPrompt":host.close_prompt,"exitRequested":host.exit_requested,"openSaved":host.open_ready,
        "notices":host.notices,"chooser":host.chooser,"chosen":host.chosen,"confirmDiscardRecovered":host.confirm_discard_recovered,
        "discovered":host.discovered.iter().map(|r|json!({"id":r.id,"count":r.photo_count,"time":r.checkpoint_time_ms,"problem":r.problem,
            "unsavedPhotos":r.unsaved_photos,"newerUnusableTime":r.newer_unusable_time_ms,"operations":r.operations,"automatic":r.automatic,
            "names":r.source_context.iter().map(|p|file_name(std::path::Path::new(p))).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "inspection":host.inspection.as_ref().map(|result| match result {
            Ok(detail) => json!({"id":detail.id,"checkpointTime":detail.checkpoint_time_ms,"photos":detail.photos.iter().map(|p|json!({
                "name":file_name(&p.path),"source":format!("{:?}",p.source),"sidecar":format!("{:?}",p.sidecar),"sidecarModified":p.sidecar_modified_ms,"unsaved":p.unsaved})).collect::<Vec<_>>()}),
            Err(error) => json!({"error":error}),
        }),
        "requests":crate::camera_raw_admission::snapshot(app),
        "photos":host.photos.iter().map(|(id,p)|json!({"photo":id.0,"id":p.id,"recipe":p.recipe,"pendingXmp":p.committed.as_ref()!=Some(&p.recipe)})).collect::<Vec<_>>()})
}

fn remove_runtime(app: &mut PhotocraftApp, id: PhotoId) {
    let document = super::filmstrip_raws(app).into_iter().find(|raw| raw.photo == id).map(|raw| raw.document);
    if app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).is_some_and(|r| r.photo == id) {
        app.camera_raw = None;
    }
    if app.pending_raw_open.as_ref().is_some_and(|r| r.photo == id) {
        app.pending_raw_open = None;
    }
    app.queued_raw_opens.retain(|r| r.photo != id);
    app.raw_filmstrip.parked.retain(|p| p.raw.photo != id);
    if let Some(index) = document.and_then(|id| super::document_index(app, id)) {
        app.session.close(index);
    }
    super::sync_provisional(app);
    app.sync_views();
}
