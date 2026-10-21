//! Background jobs: long commands run on a worker thread, report progress and can be cancelled
//! (#210).
//!
//! A command opts in by doing its heavy work through [`run`] (or [`edit_job`] for a plain document
//! edit). The work closure gets a [`JobCtx`] (progress plus a cancellation flag) and runs against
//! a snapshot: documents are copy-on-write, so cloning one is O(layers). Its result is applied on
//! the UI thread as one undo step, through the same path as [`Session::execute`], so the journal,
//! history, damage and post-command hooks all behave as for a synchronous command.
//!
//! - [`Session::execute`] runs everything inline, as before: tests, the CLI and scripts see no
//!   difference.
//! - [`Session::start`] runs a job-capable command on a worker thread and returns its
//!   [`JobId`]; [`Session::poll_jobs`] (called every frame by the UI) applies finished jobs.
//!   On wasm `start` runs inline (same API, written once).
//! - [`Session::cancel_job`] sets the flag; the algorithms check it per tile, row band or
//!   iteration, and the result is discarded. The document is never touched by a worker.
//! - A panic in the worker becomes an error; the document is unchanged.
//! - While a job runs on a document, commands that would edit that document are disabled with a
//!   message naming the job (queries and document-independent commands still run).

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use photocraft_doc::{DocId, Document, LayerId};
use photocraft_raster::Interrupt;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{EngineError, Result, Session};

type Authorization = fn(&str, &Value) -> Result<()>;

thread_local! {
    // Smart-filter rendering has no Session argument. Carry the requesting session's gate
    // through synchronous rendering and explicitly reinstall it on background workers.
    static RENDER_AUTHORIZATION: std::cell::Cell<Option<Authorization>> = const { std::cell::Cell::new(None) };
}

pub(crate) struct AuthorizationScope(Option<Authorization>);

impl AuthorizationScope {
    fn enter(gate: Option<Authorization>) -> Self {
        Self(RENDER_AUTHORIZATION.with(|current| current.replace(gate.or(current.get()))))
    }
}

impl Drop for AuthorizationScope {
    fn drop(&mut self) {
        RENDER_AUTHORIZATION.with(|current| current.set(self.0));
    }
}

/// [`AuthorizationScope`] entered with `gate` (or the one already installed, so trusted work on
/// this thread keeps ambient paths usable). [`Session::edit`] and the job paths use this to carry
/// the requesting session's authority into every smart-object re-render.
pub(crate) fn render_authorization_scope(gate: Option<Authorization>) -> AuthorizationScope {
    AuthorizationScope::enter(gate)
}

/// Recheck a stored filter's command and parameters before evaluating it or using its cache.
pub(crate) fn authorize_render_command(id: &str, params: &Value) -> Result<()> {
    match RENDER_AUTHORIZATION.with(std::cell::Cell::get) {
        Some(gate) => gate(id, params),
        None => Ok(()),
    }
}

/// Lifetime of one trusted Camera Raw commit. Not serializable, clonable or a command parameter.
/// Dropping this scope expires an unused grant; the first dispatch consumes it even on failure.
pub struct CameraRawCommandScope {
    _live: Arc<()>,
}

pub(crate) struct ReviewCommandPermit {
    document: DocId,
    generation: u64,
    live: Weak<()>,
}

// Exact engine queries and session-only operations. Keep new commands fail-closed for a held
// active document; journal=false alone does not establish that a command cannot mutate it.
fn review_independent_command(id: &str) -> bool {
    matches!(
        id,
        "command.list" | "session.inspect" | "document.inspect" | "document.pixel" | "document.sampleColor"
            | "jobs.list" | "prefs.get" | "channel.list" | "path.list" | "path.info" | "layer.vectorMask.info"
            | "edit.profileInfo" | "shape.info" | "type.info" | "type.fonts" | "type.hitTest" | "brush.presets.list"
            | "brush.get" | "swatches.list" | "gradient.fill.get" | "gradient.presets.list" | "pattern.list"
            | "pattern.presets.list" | "style.presets.list" | "shape.presets.list" | "tool.presets.list"
            | "type.characterStyle.list" | "type.paragraphStyle.list" | "layerComp.list" | "measurementLog.list"
            | "notes.list" | "cloneSource.list" | "slice.list" | "variables.list" | "document.presets.list"
            | "document.presets.get" | "plugin.list" | "actions.list" | "actions.get" | "timeline.info"
            | "image.analysis.info" | "image.adjustments.colorLookup.list"
            | "file.new" | "file.openAs" | "file.open"
            | "tools.setColors" | "tools.swapColors" | "tools.defaultColors" | "tools.setBrush"
            | "tools.decreaseBrushSize" | "tools.increaseBrushSize" | "tools.decreaseBrushHardness"
            | "tools.increaseBrushHardness" | "tools.rotateBrush" | "swatches.add" | "swatches.delete"
            | "swatches.rename" | "swatches.move" | "swatches.newGroup" | "swatches.renameGroup"
            | "swatches.deleteGroup" | "swatches.use" | "swatches.reset" | "tool.presets.new"
            | "tool.presets.select" | "tool.presets.edit" | "tool.presets.reset"
            // Import ordering moves tabs while preserving each DocId, its state and active identity.
            // Native UI policy remains in its shell guard; this is not admission or activation.
            | "document.move"
    )
}

#[derive(Clone, Copy, PartialEq)]
enum ReviewReference {
    Other,
    Documents,
    OneDocument,
}

fn review_reference(key: &str) -> ReviewReference {
    let key = key.to_ascii_lowercase();
    if key.contains("document") || key == "docs" {
        ReviewReference::Documents
    } else if matches!(key.as_str(), "doc" | "source" | "destination") {
        ReviewReference::OneDocument
    } else {
        ReviewReference::Other
    }
}

/// Identifies one background job for the session's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
pub struct JobId(pub u64);

/// What a job's work closure sees: report progress, check for cancellation. Cheap to clone; all
/// clones share the same state.
#[derive(Clone, Default)]
pub struct JobCtx {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    cancel: AtomicBool,
    /// `f32` bits of the progress fraction. Non-negative floats order like their bits, so
    /// `fetch_max` keeps progress monotonic even with several reporting threads.
    progress: AtomicU32,
    message: Mutex<String>,
}

impl std::fmt::Debug for JobCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobCtx").field("progress", &self.fraction()).field("cancelled", &self.cancelled()).finish()
    }
}

impl JobCtx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Report progress in `0.0..=1.0` with a short status (empty keeps the previous one).
    /// Progress never goes backwards: a lower value than already reported is ignored.
    pub fn progress(&self, fraction: f32, message: &str) {
        if fraction.is_finite() {
            self.shared.progress.fetch_max(fraction.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
        if !message.is_empty() {
            let mut m = self.shared.message.lock().unwrap_or_else(PoisonError::into_inner);
            if *m != message {
                message.clone_into(&mut m);
            }
        }
    }

    /// Has the job been cancelled? Long loops check this and stop early.
    pub fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::Relaxed)
    }

    /// Ask the job to stop (idempotent).
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    /// `Err(Cancelled)` once cancelled, for `?` between stages.
    pub fn check(&self) -> Result<()> {
        if self.cancelled() { Err(EngineError::Cancelled) } else { Ok(()) }
    }

    /// Progress reported so far (`0.0..=1.0`).
    pub fn fraction(&self) -> f32 {
        f32::from_bits(self.shared.progress.load(Ordering::Relaxed))
    }

    /// The latest status message.
    pub fn message(&self) -> String {
        self.shared.message.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Run `f` with an algorithm-level [`Interrupt`] whose progress maps onto `lo..hi` of this
    /// job (with `message`) and whose cancellation is this job's.
    pub fn stage<R>(&self, lo: f32, hi: f32, message: &str, f: impl FnOnce(&Interrupt) -> R) -> R {
        self.progress(lo, message);
        let cancel = || self.cancelled();
        let progress = |p: f32| self.progress(lo + (hi - lo) * p, "");
        f(&Interrupt::new(&cancel, &progress))
    }
}

/// Final state of a job.
#[derive(Clone, Debug, PartialEq)]
pub enum JobOutcome {
    /// Applied; the command's result.
    Done(Value),
    /// Failed (including a panic in the worker); the document is unchanged.
    Failed(String),
    /// Cancelled; the result (if any) was discarded and the document is unchanged.
    Cancelled,
}

/// A job that has ended, as reported by [`Session::poll_jobs`].
#[derive(Clone, Debug, PartialEq)]
pub struct JobEvent {
    pub id: JobId,
    pub command: String,
    pub label: String,
    pub document: Option<DocId>,
    pub outcome: JobOutcome,
}

/// A job's state for the UI, the control channel and MCP (`jobs.list`).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub id: JobId,
    pub command: String,
    /// Shown in the progress UI ("Gaussian Blur").
    pub label: String,
    /// The document the job edits (and locks), if any.
    pub document: Option<DocId>,
    /// `running`, `done`, `failed` or `cancelled`.
    pub state: &'static str,
    pub progress: f32,
    pub message: String,
    pub elapsed_ms: f64,
    /// The command's result (done) or the error (failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type Payload = Box<dyn Any + Send>;
type Apply = Box<dyn FnOnce(&mut Session, Payload) -> Result<Value> + Send>;

/// Wall-clock milliseconds since some fixed point (0 on wasm, where `Instant` is unavailable and
/// jobs run inline anyway).
fn now_ms() -> f64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
    }
    #[cfg(target_arch = "wasm32")]
    {
        0.0
    }
}

struct Running {
    id: JobId,
    command: String,
    label: String,
    params: Value,
    journal: bool,
    doc: Option<DocId>,
    /// The document as the job saw it: applying fails if it was edited meanwhile.
    snapshot: Option<Weak<Document>>,
    ctx: JobCtx,
    started_ms: f64,
    coalesce: Option<String>,
    color_restrict: Option<usize>,
    /// The document's layer selection when the job started: applying targets it, not whatever
    /// got selected meanwhile (a Type click does that without editing the document, #7366).
    layers: Option<LayerSelection>,
    rx: std::sync::mpsc::Receiver<Result<Payload>>,
    apply: Option<Apply>,
    #[cfg(not(target_arch = "wasm32"))]
    handle: Option<std::thread::JoinHandle<()>>,
}

/// A document's active layer, selected layers and selection anchor.
#[derive(Clone, PartialEq)]
struct LayerSelection {
    active: Option<LayerId>,
    selected: Vec<LayerId>,
    anchor: Option<LayerId>,
}

impl LayerSelection {
    fn of(st: &crate::DocState) -> Self {
        Self { active: st.active_layer, selected: st.selected_layers.clone(), anchor: st.layer_anchor }
    }

    fn set(self, st: &mut crate::DocState) {
        st.active_layer = self.active;
        st.selected_layers = self.selected;
        st.layer_anchor = self.anchor;
        crate::channel_cmds::fix_view(st);
    }
}

impl Running {
    fn info(&self) -> JobInfo {
        JobInfo {
            id: self.id,
            command: self.command.clone(),
            label: self.label.clone(),
            document: self.doc,
            state: "running",
            progress: self.ctx.fraction(),
            message: self.ctx.message(),
            elapsed_ms: now_ms() - self.started_ms,
            result: None,
            error: None,
        }
    }
}

/// The session's job table (see the module docs).
#[derive(Default)]
pub struct Jobs {
    next: u64,
    /// Set while [`Session::start`] runs a command: [`run`] spawns instead of running inline.
    spawn: bool,
    /// The job a command registered while `spawn` was set (picked up by `start`).
    pending: Option<Running>,
    running: Vec<Running>,
    /// Ended jobs not yet collected by [`Session::poll_jobs`].
    events: Vec<JobEvent>,
    /// The last few ended jobs, for `jobs.list`.
    recent: Vec<JobInfo>,
    /// Workers of cancelled jobs that may still be unwinding (native).
    #[cfg(not(target_arch = "wasm32"))]
    draining: Vec<std::thread::JoinHandle<()>>,
    /// The context inline job work runs under (see [`Session::set_inline_job_ctx`]).
    inline: Option<JobCtx>,
    /// The review generation (see [`Session::next_review_generation`]).
    review_generation: u64,
    /// Documents an open job admitted as provisional (a RAW the shell will show in Camera Raw), with the review
    /// generation they were admitted under. Declared inside the job's apply, before any event can be handled, and
    /// released by the shell once it owns them.
    pub(crate) admitted: Vec<(DocId, u64)>,
}

/// What an open job may do with the document it decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// An ordinary open: the document joins the session as an editable tab.
    Normal,
    /// An interactive open that the shell shows in Camera Raw when the file turns out to be a RAW developed from its
    /// sensor data (the importer's own verdict, not the file name): such a document is declared provisional inside
    /// the job's apply, in the same step that adds it. A result of a superseded review generation is not applied.
    ReviewIfRaw,
    /// Explicit Camera Raw opening of supported RAW, JPEG and flattened TIFF photographs.
    ReviewInCameraRaw,
}

/// Internal import routing receipt, removed by the shell before showing warnings.
pub const CAMERA_RAW_REVIEW_NOTE: &str = "PhotoCraft Camera Raw review requested";

/// Read-only routing inspection for an interactive native opening. A supplied
/// path authorizes the same adjacent-packet lookup as the native review; byte-only
/// automation must pass `None`. Binding ambiguity fails before any document is
/// admitted. Unsupported embedded metadata leaves an automatic open in the editor.
/// A rendered photo whose File Handling mode is Disabled is never routed
/// automatically, so its sidecar is not read at all: an unusual sidecar cannot stop
/// that ordinary editor open.
/// Camera Raw routing once the import is known. A sensor RAW qualifies only when the
/// importer developed its sensor data: a preview fallback (CR3, compressed ORF, X-Trans,
/// sRAW, ...) could never be admitted (decode refuses it), so it stays an editor document,
/// labelled as a preview, exactly as before Camera Raw routing existed.
pub fn routes_imported(
    defaults: &crate::prefs::RawDefaults,
    source: &photocraft_io::camera_raw_source::Inspection,
    explicit: bool,
    warnings: &[String],
) -> bool {
    (source.kind != photocraft_io::camera_raw_source::SourceKind::Raw || warnings.iter().any(|w| w.contains(photocraft_io::raw::DEVELOPED_NOTE)))
        && defaults.routes_to_camera_raw(source, explicit)
}

/// Said when an explicit Camera Raw open of a RAW falls back to its embedded preview.
pub const RAW_PREVIEW_ONLY_NOTE: &str = "Camera Raw cannot develop this RAW's sensor data yet; its embedded preview opened in the editor";

pub fn camera_raw_open_inspection(
    bytes: &[u8],
    native_path: Option<&str>,
    explicit: bool,
    defaults: &crate::prefs::RawDefaults,
) -> Result<(Option<photocraft_io::camera_raw_source::Inspection>, Option<String>)> {
    let inspection = match photocraft_io::camera_raw_source::inspect(bytes) {
        Ok(inspection) => inspection,
        Err(error) if !explicit && !photocraft_io::raw::is_raw(bytes) => {
            return Ok((None, Some(format!("Camera Raw opening unavailable; opened in the editor: {error}"))));
        }
        Err(error) => return Err(EngineError::Other(error.to_string())),
    };
    let _ = (native_path, defaults);
    Ok((Some(inspection), None))
}

const RECENT: usize = 16;

impl Drop for Jobs {
    fn drop(&mut self) {
        // Workers own snapshots only; stop them so they don't burn CPU for a closed session.
        for r in self.running.iter().chain(self.pending.iter()) {
            r.ctx.cancel();
        }
    }
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs").field("running", &self.running.len()).finish()
    }
}

/// How [`Session::start`] ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Started {
    /// The command ran to completion inline (it isn't a job, or this is wasm).
    Done(Value),
    /// The command is running in the background.
    Job(JobId),
}

/// Commands that never edit the active document, allowed while a job runs on it.
fn independent_of_document(id: &str) -> bool {
    const PREFIXES: &[&str] = &["jobs.", "tools.", "view.", "window.", "help.", "brush.presets.", "gradient.presets."];
    const IDS: &[&str] = &["file.new", "file.close", "file.closeAll", "edit.preferences", "edit.colorSettings", "edit.keyboardShortcuts", "edit.menus"];
    PREFIXES.iter().any(|p| id.starts_with(p)) || IDS.contains(&id)
}

#[cfg(not(target_arch = "wasm32"))]
fn read_path(path: &str) -> Result<Vec<u8>> {
    // Bounded reads, and a clear error for a file larger than memory (#375).
    photocraft_format::read_file(std::path::Path::new(path)).map_err(|e| EngineError::Other(format!("{path}: {e}")))
}

#[cfg(target_arch = "wasm32")]
fn read_path(path: &str) -> Result<Vec<u8>> {
    Err(EngineError::Other(format!("cannot open paths on the web: {path}")))
}

/// Run a job-capable command's heavy part. `work` runs on a worker thread when the command was
/// started with [`Session::start`] (inline otherwise, and always on wasm); `apply` then runs on
/// the calling thread with its output and returns the command's result. With `lock_document`,
/// the active document is locked against other edits while the job runs, and applying fails if
/// it was edited anyway.
///
/// `work` must only read what it captured (clone the document `Arc` first): it never sees the
/// session.
pub fn run<T: Send + 'static>(
    s: &mut Session,
    label: &str,
    lock_document: bool,
    work: impl FnOnce(&JobCtx) -> Result<T> + Send + 'static,
    apply: impl FnOnce(&mut Session, T) -> Result<Value> + Send + 'static,
) -> Result<Value> {
    let authorization = s.authorize.or_else(|| RENDER_AUTHORIZATION.with(std::cell::Cell::get));
    let _authorization = AuthorizationScope::enter(authorization);
    // The job's document: stored paths it follows are resolved against it on any thread.
    let document = s.active().and_then(|d| d.path.as_deref()).map(std::path::PathBuf::from);
    let _document = crate::doc_path::scope(document.as_deref());
    if !s.jobs.spawn || cfg!(target_arch = "wasm32") {
        let t = work(&s.jobs.inline.clone().unwrap_or_default())?;
        return apply(s, t);
    }
    if s.jobs.pending.is_some() {
        return Err(EngineError::Other(format!("`{label}` started a second background job in one command")));
    }
    let (doc, snapshot, layers) = match (lock_document, s.active()) {
        (true, Some(st)) => (Some(st.doc.id), Some(Arc::downgrade(&st.doc)), Some(LayerSelection::of(st))),
        (true, None) => return Err(EngineError::NoDocument),
        (false, _) => (None, None, None),
    };
    let ctx = JobCtx::new();
    let (tx, rx) = std::sync::mpsc::channel::<Result<Payload>>();
    let apply_document = document.clone();
    let erased: Apply = Box::new(move |s, any| {
        let _authorization = AuthorizationScope::enter(authorization);
        let _document = crate::doc_path::scope(apply_document.as_deref());
        let t = any.downcast::<T>().map_err(|_| EngineError::Other("internal error: job result type mismatch".into()))?;
        apply(s, *t)
    });
    s.jobs.next += 1;
    let id = JobId(s.jobs.next);
    #[cfg(not(target_arch = "wasm32"))]
    let handle = {
        let wctx = ctx.clone();
        let label_w = label.to_string();
        // The work closure is moved into the thread. If spawning fails (thread limits), there is
        // no way to get it back, so the error is reported instead.
        let spawned = std::thread::Builder::new().name(format!("photocraft-job-{}", id.0)).spawn(move || {
            let _authorization = AuthorizationScope::enter(authorization);
            let _document = crate::doc_path::scope(document.as_deref());
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&wctx).map(|t| Box::new(t) as Payload)))
                .unwrap_or_else(|_| Err(EngineError::Other(format!("`{label_w}` failed with an internal error (logged); the document is unchanged"))));
            // The receiver is gone when the job was cancelled: nothing to report.
            let _ = tx.send(r);
        });
        Some(spawned.map_err(|e| EngineError::Other(format!("could not start a background job: {e}")))?)
    };
    #[cfg(target_arch = "wasm32")]
    {
        // Unreachable (wasm runs inline above); keep the types used.
        drop((work, tx));
    }
    s.jobs.pending = Some(Running {
        id,
        command: String::new(),
        label: label.to_string(),
        params: Value::Null,
        journal: true,
        doc,
        snapshot,
        ctx,
        started_ms: now_ms(),
        coalesce: s.coalesce_request.clone(),
        color_restrict: s.color_restrict,
        layers,
        rx,
        apply: Some(erased),
        #[cfg(not(target_arch = "wasm32"))]
        handle,
    });
    Ok(json!({"job": id.0}))
}

/// A document edit whose body is the job: like [`Session::edit`] (one undo step named `label`),
/// but `f` runs on a worker against a copy of the active document when started in the
/// background. `finish` turns `f`'s output into the command's result.
pub fn edit_job<R: Send + 'static>(
    s: &mut Session,
    label: &str,
    f: impl FnOnce(&mut Document, &mut Option<LayerId>, &JobCtx) -> Result<R> + Send + 'static,
    finish: impl FnOnce(R) -> Value + Send + 'static,
) -> Result<Value> {
    if !s.jobs.spawn || cfg!(target_arch = "wasm32") {
        // Inline: exactly `Session::edit`, as before jobs existed.
        let ctx = s.jobs.inline.clone().unwrap_or_default();
        let r = s.edit(label, |doc, active| f(doc, active, &ctx))?;
        return Ok(finish(r));
    }
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let (base, active) = (st.doc.clone(), st.active_layer);
    let label_owned = label.to_string();
    run(
        s,
        label,
        true,
        move |ctx| {
            let mut doc = (*base).clone();
            let mut active = active;
            let r = f(&mut doc, &mut active, ctx)?;
            Ok((doc, active, r))
        },
        move |s, (doc, new_active, r)| {
            s.edit(&label_owned, move |d, active| {
                *d = doc;
                *active = new_active;
                Ok(())
            })?;
            Ok(finish(r))
        },
    )
}

/// Scan every layer surface's content bounds into the shared tile bounds cache, so the first
/// draw of a document opened in the background doesn't do it on the UI thread: GPU planning
/// asks each layer for its bounds, a multi-second scan on a multi-gigabyte PSB (#4024).
/// `check` runs between layers (cancellation).
pub fn warm_layer_bounds(doc: &Document, mut check: impl FnMut() -> Result<()>) -> Result<()> {
    let mut pending: Vec<&photocraft_doc::Layer> = doc.layers.iter().collect();
    while let Some(layer) = pending.pop() {
        check()?;
        if let Some(children) = layer.children() {
            pending.extend(children);
        }
        if let Some(surface) = layer.surface() {
            photocraft_compose::bounds::content_bounds(surface);
        }
    }
    Ok(())
}

/// Where a background open reads its file from.
#[derive(Clone, Debug)]
pub enum OpenSource {
    /// File contents already in memory (web pickers, drops, automation).
    Bytes(Arc<Vec<u8>>),
    /// A path the worker reads itself (native), so even the read stays off the UI thread.
    Path(String),
}

/// The command id open jobs report (there is no engine command: opening is the shell's).
pub const OPEN_JOB: &str = "file.open";

impl Session {
    /// Run the heavy part of inline (not background) job commands under `ctx`, so another thread
    /// can cancel them: a live preview whose parameters changed stops its stale computation
    /// (`Err(Cancelled)`, the document unchanged) instead of finishing it. `None` restores the
    /// default, a context nothing can cancel.
    pub fn set_inline_job_ctx(&mut self, ctx: Option<JobCtx>) {
        self.jobs.inline = ctx;
    }

    /// Run a command, in the background when it supports it (see [`jobs`](crate::jobs)). Errors
    /// like [`Session::execute`] for unknown, disabled or failing commands.
    pub fn start(&mut self, id: &str, params: Value) -> Result<Started> {
        self.dispatch(id, params, true)
    }

    /// Open a file in the background: read (for a path) and decode on a worker, with progress
    /// (per layer for PSD/PSB) and cancellation, then add the document like
    /// [`Session::open_document`] (Color Settings policies apply). `name` names the document.
    /// The job's result is `{document, name, warnings, color}`; on wasm it runs inline. When
    /// Color Settings ask about the file's profile, the document is held back instead and the
    /// result is `{held, name, warnings, color}` (`color` the question): take it with
    /// [`Session::take_held_open`] once answered.
    pub fn start_open(&mut self, name: &str, source: OpenSource) -> Result<Started> {
        self.start_open_with(name, source, Admission::Normal)
    }

    /// The review generation: the shell bumps it when a Camera Raw review is cancelled, so that an open that was
    /// started under an earlier one cannot be applied (or kept) afterwards.
    pub fn review_generation(&self) -> u64 {
        self.jobs.review_generation
    }

    /// Supersede every review started so far (see [`Session::review_generation`]).
    pub fn next_review_generation(&mut self) {
        self.jobs.review_generation = self.jobs.review_generation.wrapping_add(1);
    }

    /// Was `id` admitted under a review generation that has since been superseded?
    pub fn admission_stale(&self, id: DocId) -> bool {
        self.jobs.admitted.iter().any(|(admitted, generation)| *admitted == id && *generation != self.jobs.review_generation)
    }

    /// The shell owns `id` now (queued for review, or closed): it is no longer held by the job layer.
    pub fn release_admission(&mut self, id: DocId) {
        self.jobs.admitted.retain(|(admitted, _)| *admitted != id);
    }

    /// Admit `id`, decoded without a job (the inline open path: the web build, or background jobs off), for a Camera Raw
    /// review under the current review generation, exactly as an open job's apply does for a RAW it decoded. A Cancel
    /// while the photograph still waits behind a profile prompt (the shell's held opens) then supersedes it too.
    pub fn admit_for_review(&mut self, id: DocId) {
        let generation = self.jobs.review_generation;
        self.jobs.admitted.retain(|(admitted, _)| *admitted != id);
        self.jobs.admitted.push((id, generation));
    }

    /// [`Session::start_open`] with an explicit [`Admission`].
    pub fn start_open_with(&mut self, name: &str, source: OpenSource, admission: Admission) -> Result<Started> {
        let native_path = match &source {
            OpenSource::Path(path) if admission != Admission::Normal => Some(path.clone()),
            _ => None,
        };
        self.start_open_with_sidecar_path(name, source, admission, native_path)
    }

    /// Interactive opening of bytes already read from a native path. The caller
    /// supplies that path only with native adjacent-sidecar read authority.
    /// Automation bytes use [`Session::start_open_with`] and cannot infer a path
    /// from a display name.
    pub fn start_open_with_sidecar_path(&mut self, name: &str, source: OpenSource, admission: Admission, native_path: Option<String>) -> Result<Started> {
        let review = admission != Admission::Normal;
        let explicit = admission == Admission::ReviewInCameraRaw;
        let initial_raw_defaults = self.prefs().raw_defaults.clone();
        let generation = self.jobs.review_generation;
        let label = format!("Opening {name}");
        let name_w = name.to_string();
        let name_a = name.to_string();
        let max_svg_group_depth = self.prefs().file_handling.rasterize_svg_groups_deeper_than as usize;
        self.start_job(
            OPEN_JOB,
            json!({"name": name}),
            &label,
            false,
            move |ctx| {
                ctx.progress(0.0, "Reading");
                let bytes = match source {
                    OpenSource::Bytes(b) => b,
                    OpenSource::Path(p) => Arc::new(read_path(&p)?),
                };
                ctx.check()?;
                ctx.progress(0.02, "Decoding");
                let (inspection, routing_warning) =
                    if review { camera_raw_open_inspection(&bytes, native_path.as_deref(), explicit, &initial_raw_defaults)? } else { (None, None) };
                let intended_review = inspection.as_ref().is_some_and(|source| initial_raw_defaults.routes_to_camera_raw(source, explicit));
                let mut imported = ctx
                    .stage(0.02, 0.95, "Decoding", |ctl| photocraft_io::import_with_svg_group_depth_and_interrupt(&name_w, &bytes, max_svg_group_depth, ctl))
                    .map_err(|e| match e {
                        photocraft_io::IoError::Cancelled => EngineError::Cancelled,
                        e => EngineError::Other(e.to_string()),
                    })?;
                ctx.progress(0.95, "Preparing layers");
                warm_layer_bounds(&imported.document, || ctx.check())?;
                imported.warnings.extend(routing_warning);
                Ok((imported, inspection, intended_review))
            },
            move |s, (mut r, inspection, intended_review): (photocraft_io::ImportResult, Option<photocraft_io::camera_raw_source::Inspection>, bool)| {
                let developed = r.warnings.iter().any(|w| w.contains(photocraft_io::raw::DEVELOPED_NOTE));
                let raw_review = inspection.as_ref().is_some_and(|source| routes_imported(&s.prefs().raw_defaults, source, explicit, &r.warnings));
                if explicit && !raw_review && inspection.as_ref().is_some_and(|i| i.kind == photocraft_io::camera_raw_source::SourceKind::Raw) {
                    r.warnings.push(RAW_PREVIEW_ONLY_NOTE.into());
                }
                if raw_review {
                    r.warnings.push(CAMERA_RAW_REVIEW_NOTE.into());
                    r.source_read_only = true;
                }
                // Checked before anything is admitted, for any RAW started for a review: a review that was cancelled
                // meanwhile gets no RAW, even when review-on-open was switched off since (the preference is read at apply
                // time). Any other open (a PSD, a JPEG) is unrelated and proceeds.
                if review && (developed || intended_review || raw_review) && s.jobs.review_generation != generation {
                    return Err(EngineError::Cancelled);
                }
                // Retain upstream File Handling ACEScg tagging before profile prompts.
                crate::file_cmds::tag_exr_colorspace(s, &name_a, &mut r.document);
                // Ask When Opening: held back until the prompt is answered (`take_held_open`). A
                // developed raw has no embedded profile to ask about.
                if let Some(question) = s.color.open_question(&r.document).filter(|_| !developed && !raw_review) {
                    let held = s.hold_open(r.document, r.source_read_only);
                    return Ok(json!({"held": held, "name": name_a, "warnings": r.warnings, "color": question}));
                }
                let imported = r.document.id;
                let (index, color) = s.open_document(r.document, None);
                // The id the session actually gave it (an incoming id is remapped when another document owns it).
                let id = s.documents().get(index).map_or(imported, |st| st.doc.id);
                if raw_review {
                    s.jobs.admitted.push((id, generation));
                }
                if let Some(st) = s.active_mut() {
                    st.source_read_only = r.source_read_only;
                }
                Ok(json!({"document": index, "docId": id.0, "name": name_a, "warnings": r.warnings, "color": color}))
            },
        )
    }

    /// Start a job that isn't an engine command (opening a file, a shell operation, a test's fake
    /// job): `work` runs on a worker, `apply` on this thread when [`Session::poll_jobs`] or
    /// [`Session::wait_job`] picks the result up. `command` and `params` identify it in
    /// [`JobInfo`]; it is not journaled. With `lock_document` the active document is locked as
    /// for a command. Runs inline on wasm.
    pub fn start_job<T: Send + 'static>(
        &mut self,
        command: &str,
        params: Value,
        label: &str,
        lock_document: bool,
        work: impl FnOnce(&JobCtx) -> Result<T> + Send + 'static,
        apply: impl FnOnce(&mut Session, T) -> Result<Value> + Send + 'static,
    ) -> Result<Started> {
        self.jobs.spawn = true;
        let r = run(self, label, lock_document, work, apply);
        self.jobs.spawn = false;
        let pending = self.jobs.pending.take();
        match (r, pending) {
            (Err(e), Some(job)) => {
                job.ctx.cancel();
                Err(e)
            }
            (Err(e), None) => Err(self.explain_missing_layer(e)),
            (Ok(_), Some(mut job)) => {
                job.command = command.to_string();
                job.params = params;
                job.journal = false;
                let id = job.id;
                self.jobs.running.push(job);
                Ok(Started::Job(id))
            }
            (Ok(v), None) => Ok(Started::Done(v)),
        }
    }

    /// Apply finished background jobs (on this thread) and return every job that ended since
    /// the last call. Cheap when nothing runs; the UI calls it every frame.
    pub fn poll_jobs(&mut self) -> Vec<JobEvent> {
        let mut i = 0;
        while i < self.jobs.running.len() {
            match self.jobs.running.get(i).map(|r| r.rx.try_recv()) {
                Some(Ok(r)) => {
                    let job = self.jobs.running.remove(i);
                    self.finish_job(job, r);
                }
                Some(Err(std::sync::mpsc::TryRecvError::Empty)) => i += 1,
                Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                    let job = self.jobs.running.remove(i);
                    let label = job.label.clone();
                    self.finish_job(job, Err(EngineError::Other(format!("`{label}` stopped unexpectedly; the document is unchanged"))));
                }
                None => break,
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.jobs.draining.retain(|h| !h.is_finished());
        std::mem::take(&mut self.jobs.events)
    }

    /// Block until job `id` ends, apply it, and return its result (the control channel's and
    /// MCP's default `wait`, and the CLI). `Err(Cancelled)` if it was cancelled.
    pub fn wait_job(&mut self, id: JobId) -> Result<Value> {
        if let Some(i) = self.jobs.running.iter().position(|r| r.id == id) {
            let job = self.jobs.running.remove(i);
            let r = job.rx.recv().unwrap_or_else(|_| Err(EngineError::Other(format!("`{}` stopped unexpectedly", job.label))));
            self.finish_job(job, r);
        }
        match self.jobs.recent.iter().rev().find(|j| j.id == id) {
            Some(j) if j.state == "done" => Ok(j.result.clone().unwrap_or(Value::Null)),
            Some(j) if j.state == "cancelled" => Err(EngineError::Cancelled),
            Some(j) => Err(EngineError::Other(j.error.clone().unwrap_or_else(|| "the job failed".into()))),
            None => Err(EngineError::Other(format!("no job {}", id.0))),
        }
    }

    /// Cancel job `id`: it stops at its next check, its result is discarded and the document
    /// stays unchanged. The document is unlocked at once. False if no such job is running.
    pub fn cancel_job(&mut self, id: JobId) -> bool {
        let Some(i) = self.jobs.running.iter().position(|r| r.id == id) else { return false };
        // A print worker can already have handed the job to the OS. Discarding its result
        // would falsely report cancellation; removal belongs to the system print queue.
        if matches!(self.jobs.running[i].command.as_str(), "file.print" | "file.printOneCopy") {
            return false;
        }
        let job = self.jobs.running.remove(i);
        job.ctx.cancel();
        self.end_job(&job, JobOutcome::Cancelled);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(h) = job.handle {
            self.jobs.draining.push(h);
        }
        true
    }

    /// Cancel every running job (e.g. before quitting).
    pub fn cancel_all_jobs(&mut self) {
        let ids: Vec<JobId> = self.jobs.running.iter().map(|r| r.id).collect();
        for id in ids {
            self.cancel_job(id);
        }
    }

    /// Is any job running? (Cheap: the UI asks every frame.)
    pub fn has_jobs(&self) -> bool {
        !self.jobs.running.is_empty()
    }

    /// Running job `id`.
    pub fn job(&self, id: JobId) -> Option<JobInfo> {
        self.jobs.running.iter().find(|r| r.id == id).map(Running::info)
    }

    /// Running jobs.
    pub fn jobs(&self) -> Vec<JobInfo> {
        self.jobs.running.iter().map(Running::info).collect()
    }

    /// Running jobs followed by the last few that ended (newest last), for `jobs.list`.
    pub fn jobs_with_recent(&self) -> Vec<JobInfo> {
        self.jobs().into_iter().chain(self.jobs.recent.iter().cloned()).collect()
    }

    /// The running job that locks document `doc`, if any.
    pub fn job_on(&self, doc: DocId) -> Option<JobInfo> {
        self.jobs.running.iter().find(|r| r.doc == Some(doc)).map(Running::info)
    }

    /// The running job that locks the active document, if any.
    pub fn active_job(&self) -> Option<JobInfo> {
        self.active().and_then(|st| self.job_on(st.doc.id))
    }

    /// Wait for background workers of cancelled jobs to exit (tests and benchmarks measure
    /// cancel latency with it). Native only; returns at once on wasm.
    pub fn join_cancelled_jobs(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        for h in self.jobs.draining.drain(..) {
            let _ = h.join();
        }
    }

    /// Why command `id` can't run now because of a background job (None = no conflict).
    pub(crate) fn job_conflict(&self, id: &str, journal: bool) -> Option<String> {
        if !journal || independent_of_document(id) {
            return None;
        }
        let job = self.active_job()?;
        Some(format!("“{}” is still running on this document; wait for it to finish or cancel it (Esc)", job.label))
    }

    /// Cancel the jobs editing document `doc` (it is closing).
    pub(crate) fn cancel_jobs_on(&mut self, doc: DocId) {
        let ids: Vec<JobId> = self.jobs.running.iter().filter(|r| r.doc == Some(doc)).map(|r| r.id).collect();
        for id in ids {
            self.cancel_job(id);
        }
    }

    /// Grant the next dispatch of `filter.cameraRaw` to its trusted Rust review owner. The owner
    /// must keep the returned scope alive through that call. This is not exposed as an engine
    /// command or JSON flag, and does not authorize nested dispatch or a background operation.
    pub fn begin_camera_raw_command(&mut self, document: DocId) -> Result<CameraRawCommandScope> {
        self.review_command = None;
        if self.active().is_none_or(|st| st.doc.id != document) || !self.is_review_held(document) || self.admission_stale(document) {
            return Err(EngineError::Other("Camera Raw no longer owns this active review document".into()));
        }
        let live = Arc::new(());
        self.review_command = Some(ReviewCommandPermit { document, generation: self.review_generation(), live: Arc::downgrade(&live) });
        Ok(CameraRawCommandScope { _live: live })
    }

    fn review_reference_is_held(&self, value: &Value, kind: ReviewReference, depth: usize, active_id_field: bool) -> bool {
        if depth > 24 {
            return true;
        }
        match value {
            Value::Object(fields) => fields.iter().any(|(key, value)| {
                // Selected root fields are active DocId assertions or a command-specific "new"
                // destination. Neither is a tab selector; nested document references still count.
                !(depth == 0 && active_id_field && key == "document") && self.review_reference_is_held(value, review_reference(key), depth + 1, false)
            }),
            Value::Array(items) if kind == ReviewReference::Documents => items.iter().any(|value| self.review_reference_is_held(value, kind, depth + 1, false)),
            Value::Array(items) if kind == ReviewReference::OneDocument => {
                items.iter().filter(|value| value.is_object()).any(|value| self.review_reference_is_held(value, ReviewReference::Other, depth + 1, false))
            }
            Value::Array(items) => items.iter().any(|value| self.review_reference_is_held(value, ReviewReference::Other, depth + 1, false)),
            Value::Number(number) if kind != ReviewReference::Other => {
                // Channel commands accept rounded numeric indices as well as integer JSON.
                let index = number.as_u64().or_else(|| number.as_f64().filter(|n| n.is_finite() && (0.0..1e15).contains(n)).map(|n| n.round() as u64));
                index.is_none_or(|index| usize::try_from(index).ok().and_then(|i| self.docs.get(i)).is_some_and(|st| self.is_review_held(st.doc.id)))
            }
            Value::String(name) if kind != ReviewReference::Other => self.docs.iter().any(|st| {
                self.is_review_held(st.doc.id)
                    && (st.doc.name == *name
                        || st.path.as_deref().is_some_and(|path| {
                            path == name || std::path::Path::new(path).file_name().is_some_and(|file| file.to_string_lossy() == name.as_str())
                        }))
            }),
            _ => false,
        }
    }

    /// Dispatch policy is independent of the frontend and uses both apply-time admission and
    /// the shell's replacement/parked ownership. An unrelated held RAW does not block accepted edits.
    fn guard_review_command(&self, id: &str, params: &Value, active: Option<DocId>) -> Result<()> {
        if !self.docs.iter().any(|st| self.is_review_held(st.doc.id)) {
            return Ok(());
        }
        let refused = || EngineError::Other(format!("`{id}` cannot act on an unaccepted RAW; Open or Cancel it in Camera Raw first"));
        // Browse already checks has_provisional before each script step and returns its
        // established structured stopped result. Retain that response before target scanning.
        // Dispatch still skips held-document preprocessing and housekeeping.
        if id == "file.scripts.browse" {
            return Ok(());
        }
        // The manager stores bindings; it never runs them. Its edit_prefs call reapplies history
        // limits globally, so permit it only when every held document keeps its limits/records.
        if id == "file.scripts.scriptEventsManager" {
            let limits = &self.prefs().performance;
            let states = limits.history_states.max(1) as usize;
            let bytes = limits.history_budget_bytes();
            let unchanged = self.docs.iter().filter(|st| self.is_review_held(st.doc.id)).all(|st| {
                st.history.max_states == states
                    && st.history.max_bytes == bytes
                    && (st.history.past_len() <= 1 || bytes == 0 || st.history.pixel_bytes(&st.doc) <= bytes)
            });
            return if unchanged { Ok(()) } else { Err(refused()) };
        }
        if id == "jobs.cancel" {
            let named = params.get("job").and_then(Value::as_u64);
            return if self.jobs.running.iter().any(|job| named.is_none_or(|named| job.id.0 == named) && job.doc.is_some_and(|doc| self.is_review_held(doc))) {
                Err(refused())
            } else {
                Ok(())
            };
        }
        if review_independent_command(id) {
            return Ok(());
        }
        if matches!(
            id,
            "file.closeAll"
                | "file.closeOthers"
                | "channel.merge"
                | "actions.play"
                | "plugin.run"
                | "window.nextDocument"
                | "window.previousDocument"
                | "edit.purge.undo"
                | "edit.purge.histories"
                | "edit.purge.all"
                // apply_prefs changes history limits and may trim byte pressure in every
                // document, including a held one that is not active. These indirect callers
                // have the same global effect as prefs.set/reset.
                | "prefs.set"
                | "prefs.reset"
                | "actions.rename"
                | "actions.load"
                | "edit.keyboardShortcuts"
                | "edit.menus"
                | "edit.toolbar"
        ) || id.starts_with("file.automate.")
            || id.starts_with("file.scripts.")
            || (id == "edit.checkSpelling" && matches!(params.get("action").and_then(Value::as_str), Some("add" | "addToDictionary" | "removeFromDictionary")))
        {
            return Err(refused());
        }
        let skip_root_document = matches!(id, "layer.fill.solidColor.set" | "layer.fill.pattern.set" | "layer.layerMask.edit")
            || (matches!(id, "channel.duplicate" | "layer.duplicate") && params.get("document").and_then(Value::as_str) == Some("new"));
        if self.review_reference_is_held(params, ReviewReference::Other, 0, skip_root_document) {
            return Err(refused());
        }
        let named_tab =
            |key: &str| params.get(key).and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok()).and_then(|i| self.docs.get(i)).map(|st| st.doc.id);
        let target = match id {
            "document.activate" => named_tab("document"),
            "file.close" | "file.revealInFinder" => named_tab("document").or(active),
            "layer.copyToDocument" => named_tab("source").or(active),
            _ => active,
        };
        if target.is_some_and(|doc| self.is_review_held(doc)) {
            return Err(refused());
        }
        // Closing/saving a smart child may commit into an implicit parent not present in JSON.
        if matches!(id, "file.close" | "layer.smartObjects.saveContents")
            && self.smart_links.iter().any(|link| Some(link.child) == target && self.is_review_held(link.parent))
        {
            return Err(refused());
        }
        Ok(())
    }

    /// Run the command `id`: inline, or (with `background`) as a job when it is one.
    pub(crate) fn dispatch(&mut self, id: &str, params: Value, background: bool) -> Result<Started> {
        // Take before lookup, validation or authorization: even a failed/unknown dispatch must
        // not leave authority available to a later call. Only this stack frame can use it.
        let permit = self.review_command.take();
        let review_owned = permit.is_some_and(|permit| {
            id == "filter.cameraRaw"
                && permit.live.upgrade().is_some()
                && permit.generation == self.review_generation()
                && self.active().is_some_and(|st| st.doc.id == permit.document)
                && self.is_review_held(permit.document)
                && !self.admission_stale(permit.document)
        });
        let spec = crate::commands::find(id).ok_or_else(|| EngineError::UnknownCommand(id.to_string()))?;
        // Params are named fields: anything but an object (or null, meaning none) is a caller
        // mistake, not "use every default".
        if !(params.is_object() || params.is_null()) {
            return Err(EngineError::BadParams { cmd: id.to_string(), msg: "params must be a JSON object".into() });
        }
        // Untrusted sessions gate every command, including the ones a command runs on its own
        // behalf (`file.automate.conditionalModeChange` runs `image.mode.*`), before any side
        // effect.
        if let Some(gate) = self.authorize {
            gate(id, &params)?;
        }
        let _authorization = AuthorizationScope::enter(self.authorize);
        let _document = crate::doc_path::scope(self.active().and_then(|d| d.path.as_deref()).map(std::path::Path::new));
        if !review_owned {
            self.guard_review_command(id, &params, self.active().map(|st| st.doc.id))?;
        }
        // Last Filter runs as the filter it repeats: journaled, recorded in an action, faded and
        // run as a job like that filter, so a recorded step replays the same filter anywhere
        // (#3089), as Transform Again records its transform (#1440). With no filter yet it falls
        // through to its own precondition.
        if id == "filter.lastFilter"
            && let Some((filter, p)) = crate::filters::last_filter(self).filter(|(f, _)| f != id)
        {
            return self.dispatch(&filter, p, background);
        }
        if id == "file.new" {
            crate::document_preset_cmds::validate_new_params(self, &params)?;
        }
        crate::param_check::check(spec, &params)?;
        crate::filters::validate_params(id, &params)?;
        // A floating selection drops before any other command (Undo puts it back instead).
        if (review_owned || !self.active().is_some_and(|st| self.is_review_held(st.doc.id)))
            && let Some(v) = crate::float_cmds::before_command(self, id)?
        {
            return Ok(Started::Done(v));
        }
        // Pixel commands follow the Channels panel target unless the caller names one.
        let run_params = self.admit(id, spec, &params)?;
        self.coalesce_request = params.get("coalesce").and_then(Value::as_str).map(str::to_string);
        self.color_restrict = crate::channel_cmds::color_restriction(self, id, &run_params);
        self.jobs.spawn = background;
        // Commands this one runs on its own behalf journal after this mark (#3067).
        let nested_from = self.journal.len();
        // A replayable command is recorded as one step, so recording pauses while it runs: an
        // action it plays (a script step) must not flush the steps it ran so far into the action
        // being recorded, nor move the recording mark past the entries it is about to replace.
        let recording = if spec.journal && crate::actions_cmds::replayable(id) { self.actions.recording.take() } else { None };
        let before = self.active().map(|st| st.doc.clone());
        // Last-resort guard (AGENTS.md, Never crash): a command that panics anyway fails with an
        // error instead of taking the app down. `edit` only commits a document after its closure
        // returns, so the documents are unchanged.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (spec.run)(self, &run_params)))
            .unwrap_or_else(|_| Err(EngineError::Other(format!("`{id}` failed with an internal error (logged); the document is unchanged"))));
        if self.actions.recording.is_none() {
            self.actions.recording = recording;
        }
        self.jobs.spawn = false;
        self.coalesce_request = None;
        self.color_restrict = None;
        let pending = self.jobs.pending.take();
        match (r, pending) {
            (Err(e), Some(job)) => {
                job.ctx.cancel();
                Err(e)
            }
            (Err(e), None) => Err(self.explain_missing_layer(e)),
            (Ok(_), Some(mut job)) => {
                job.command = id.to_string();
                job.params = params;
                job.journal = spec.journal;
                let jid = job.id;
                self.jobs.running.push(job);
                Ok(Started::Job(jid))
            }
            (Ok(v), None) => {
                if let Some(before) = before {
                    self.add_relayout_damage(&before);
                }
                self.after_command(id, params, spec.journal, Some(nested_from), review_owned);
                Ok(Started::Done(v))
            }
        }
    }

    /// Grows the damage a command reported for the pixels it changed by what effects laid out
    /// over a layer's frame repaint when it moved ([`photocraft_compose::relayout_damage`]): a
    /// stroke on a layer with a Gradient Overlay reports the stroke, but the gradient is laid out
    /// again over the whole grown layer (#4053). `before` is the active document before.
    fn add_relayout_damage(&mut self, before: &Arc<Document>) {
        let Some(st) = self.active_mut().filter(|st| st.doc.id == before.id && !Arc::ptr_eq(&st.doc, before)) else { return };
        let Some(d) = st.last_damage.filter(|d| !d.is_empty()) else { return };
        let extra = photocraft_compose::relayout_damage(before, &st.doc);
        if !extra.is_empty() {
            st.last_damage = Some(d.union(&extra));
        }
    }

    /// A command that failed on a layer id the document doesn't have says which ids it does have
    /// (#5058): ids are not positions (an opened PNG's Background is not layer 1), and an agent
    /// should not need `document.inspect` to recover.
    fn explain_missing_layer(&self, e: EngineError) -> EngineError {
        match (e, self.active()) {
            (EngineError::NoLayer(id), Some(st)) if st.doc.layer(id).is_none() => {
                EngineError::UnknownLayer { id, known: crate::commands::layer_listing(&st.doc) }
            }
            (e, _) => e,
        }
    }

    /// Bookkeeping after a command (or a job's apply) succeeded.
    ///
    /// `nested_from` is the journal length when the command started. A replayable command stands
    /// for the commands it ran itself (Straighten runs Image Rotation and Crop): their entries are
    /// dropped, so a recorded action replays the command once, not its steps as well (#3067).
    /// Edit › Transform › Again (and Again on a Copy) is not replayable and keeps the concrete
    /// steps it ran instead.
    fn after_command(&mut self, id: &str, params: Value, journal: bool, nested_from: Option<usize>, review_owned: bool) {
        // A layer-mask view ends when another layer becomes active (#196).
        // Queries and session-only commands must not repair or drop held document state.
        if review_owned || !self.active().is_some_and(|st| self.is_review_held(st.doc.id)) {
            if let Some(st) = self.active_mut() {
                crate::mask_view_cmds::fix(st);
            }
            crate::edit_menu_cmds::after_command(self, id);
            crate::automate_cmds::after_command(self, id);
        }
        self.sync_preset_store();
        if !journal {
            return;
        }
        if let Some(from) = nested_from.filter(|_| crate::actions_cmds::replayable(id)) {
            self.journal.truncate(from);
        }
        if !crate::brush_cmds::coalesce_journal(self, id, &params) {
            let params = match id {
                _ if self.actions.recording.is_none() => params,
                "layer.select" => crate::layer_multi_cmds::recorded_selection(self, params),
                "layerComp.apply" => crate::comps_cmds::recorded_apply(self, params),
                _ => params,
            };
            self.journal.push((id.to_string(), params));
        }
    }

    /// Apply (or report the failure of) a job whose worker has delivered `r`.
    fn finish_job(&mut self, mut job: Running, r: Result<Payload>) {
        let apply = job.apply.take();
        let outcome = match r {
            Err(EngineError::Cancelled) => JobOutcome::Cancelled,
            Err(e) => JobOutcome::Failed(e.to_string()),
            Ok(_) if job.ctx.cancelled() => JobOutcome::Cancelled,
            Ok(payload) => match self.apply_job(&job, apply, payload) {
                Ok(v) => JobOutcome::Done(v),
                Err(EngineError::Cancelled) => JobOutcome::Cancelled,
                Err(e) => JobOutcome::Failed(e.to_string()),
            },
        };
        self.end_job(&job, outcome);
    }

    fn apply_job(&mut self, job: &Running, apply: Option<Apply>, payload: Payload) -> Result<Value> {
        let Running { label, doc, snapshot, coalesce, color_restrict, layers, command, params, journal, .. } = job;
        // A registered engine edit can become held while its worker runs. Check before active
        // assignment or apply hooks. Shell-owned RAW redevelop/open closures keep their own
        // generation checks; the synchronous Camera Raw command never transfers its grant here.
        if crate::commands::find(command).is_some() {
            self.guard_review_command(command, params, *doc)?;
        }
        let prev_active = self.active;
        let target = match doc {
            Some(d) => {
                let i =
                    self.documents().iter().position(|st| st.doc.id == *d).ok_or_else(|| EngineError::Other(format!("`{label}`: the document was closed")))?;
                let unchanged = snapshot.as_ref().is_some_and(|w| self.documents().get(i).is_some_and(|st| Weak::ptr_eq(w, &Arc::downgrade(&st.doc))));
                if !unchanged {
                    return Err(EngineError::Other(format!("`{label}`: the document changed while it ran; the result was discarded")));
                }
                self.active = Some(i);
                Some(i)
            }
            None => None,
        };
        // Selecting a layer doesn't change the document, so the snapshot check above lets it
        // through. Apply against the layers the job started on: the colour restriction and any
        // `active`-based edit land on them (#7366). The user's selection is put back below.
        let user_layers = match (target, layers) {
            (Some(i), Some(started)) => self.docs.get_mut(i).and_then(|st| {
                let now = LayerSelection::of(st);
                (now != *started).then(|| {
                    started.clone().set(st);
                    now
                })
            }),
            _ => None,
        };
        self.coalesce_request = coalesce.clone();
        self.color_restrict = *color_restrict;
        // Apply-time hooks can dispatch commands too. Keep the same single-step recording
        // contract as synchronous dispatch without truncating work done while the worker ran.
        let nested_from = self.journal.len();
        let recording = if *journal && crate::actions_cmds::replayable(command) { self.actions.recording.take() } else { None };
        let r = match apply {
            Some(f) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self, payload)))
                .unwrap_or_else(|_| Err(EngineError::Other(format!("`{label}` failed with an internal error (logged); the document is unchanged")))),
            None => Err(EngineError::Other("internal error: job applied twice".into())),
        };
        if self.actions.recording.is_none() {
            self.actions.recording = recording;
        }
        self.coalesce_request = None;
        self.color_restrict = None;
        // Finish successful command bookkeeping on the document the job edited.
        // Restoring the viewed document first would attribute Fade and slice updates
        // to that document instead, when the user switched tabs mid-job.
        if r.is_ok() {
            self.after_command(command, params.clone(), *journal, Some(nested_from), false);
        }
        // Give the user back the layers they selected while the job ran, unless the job chose a
        // selection itself (a new layer, say) or one of their layers is gone.
        if let (Some(started), Some(user), Some(st)) = (layers, user_layers, target.and_then(|i| self.docs.get_mut(i))) {
            let kept = LayerSelection::of(st) == *started;
            let exist = user.active.iter().chain(&user.selected).all(|id| st.doc.layer(*id).is_some());
            if (r.is_err() || kept) && exist {
                user.set(st);
            }
        }
        // Keep the user's active document unless the job opened a new one.
        // This also restores the selection when the apply step returned an error.
        if target.is_some() && self.active == target {
            self.active = prev_active.filter(|i| *i < self.docs.len()).or(self.active);
        }
        r
    }

    fn end_job(&mut self, job: &Running, outcome: JobOutcome) {
        let (state, result, error) = match &outcome {
            JobOutcome::Done(v) => ("done", Some(v.clone()), None),
            JobOutcome::Failed(e) => ("failed", None, Some(e.clone())),
            JobOutcome::Cancelled => ("cancelled", None, None),
        };
        let mut info = job.info();
        info.state = state;
        info.result = result;
        info.error = error;
        if state == "done" {
            info.progress = 1.0;
        }
        self.jobs.recent.push(info);
        if self.jobs.recent.len() > RECENT {
            let extra = self.jobs.recent.len() - RECENT;
            self.jobs.recent.drain(..extra);
        }
        self.jobs.events.push(JobEvent { id: job.id, command: job.command.clone(), label: job.label.clone(), document: job.doc, outcome });
    }
}

/// Engine commands for the job table (the control channel and MCP use them too).
pub fn specs() -> Vec<crate::commands::CommandSpec> {
    use crate::commands::CommandSpec;
    vec![
        CommandSpec {
            id: "jobs.list",
            label: "List Background Jobs",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: |_| Ok(()),
            run: |s, _| Ok(json!({"jobs": s.jobs_with_recent()})),
            journal: false,
        },
        CommandSpec {
            id: "jobs.cancel",
            label: "Cancel Background Job",
            menu: &[],
            shortcut: None,
            params: r##"{"job":u64? (default: every running job)}"##,
            enabled: |_| Ok(()),
            run: |s, p| match p.get("job") {
                None | Some(Value::Null) => {
                    let n = s.jobs.running.len();
                    s.cancel_all_jobs();
                    Ok(json!({"cancelled": n.saturating_sub(s.jobs.running.len())}))
                }
                Some(v) => {
                    let id = v.as_u64().ok_or_else(|| EngineError::BadParams { cmd: "jobs.cancel".into(), msg: "`job` must be a job id".into() })?;
                    if s.cancel_job(JobId(id)) {
                        Ok(json!({"cancelled": 1}))
                    } else if s.job(JobId(id)).is_some() {
                        Err(EngineError::Other("printing cannot be cancelled here; use the system print queue".into()))
                    } else {
                        Err(EngineError::Other(format!("no running job {id}")))
                    }
                }
            },
            journal: false,
        },
    ]
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;

/// Ownership of a document an open job admits (S2 P0): decided inside the job's apply, before any event is handled.
#[cfg(test)]
mod ownership_tests {
    use super::*;

    fn dng() -> Vec<u8> {
        use photocraft_raw::testgen::{DngSpec, mosaic, scene};
        let (w, h) = (48, 32);
        let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
        spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
        spec.build()
    }

    fn psd() -> Vec<u8> {
        let mut src = Session::new();
        src.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
        let doc = (*src.active().unwrap().doc).clone();
        photocraft_io::export(&doc, "x.psd", &Default::default()).unwrap().bytes
    }

    fn open(s: &mut Session, name: &str, bytes: Vec<u8>, admission: Admission) -> JobId {
        match s.start_open_with(name, OpenSource::Bytes(Arc::new(bytes)), admission).unwrap() {
            Started::Job(id) => id,
            Started::Done(_) => panic!("the open ran inline"),
        }
    }

    #[test]
    fn a_raw_open_is_declared_provisional_by_its_own_apply_before_any_event() {
        let mut s = Session::new();
        let id = open(&mut s, "shot.dng", dng(), Admission::ReviewIfRaw);
        s.wait_job(id).unwrap();
        assert_eq!(s.documents().len(), 1);
        let doc = s.documents()[0].doc.id;
        assert!(s.has_provisional(), "scripts are already paused: the shell has not seen the result yet");
        assert_eq!(s.provisional_documents(), vec![doc]);
        // The shell declaring and clearing its own list does not touch the admission.
        s.set_provisional_documents(Vec::new());
        s.set_provisional_pending(false);
        assert!(s.has_provisional());
        s.release_admission(doc);
        assert!(!s.has_provisional() && s.provisional_documents().is_empty());
    }

    #[test]
    fn the_decision_is_the_importers_not_the_file_name() {
        let mut s = Session::new();
        let id = open(&mut s, "scan.dat", dng(), Admission::ReviewIfRaw);
        s.wait_job(id).unwrap();
        assert!(s.has_provisional(), "a RAW under a name that does not look like one is still admitted");
    }

    #[test]
    fn ordinary_opens_and_non_raw_files_are_never_admitted() {
        let mut s = Session::new();
        let id = open(&mut s, "shot.dng", dng(), Admission::Normal);
        s.wait_job(id).unwrap();
        assert!(!s.has_provisional(), "a Normal open (CLI, MCP, scripts) is an editable document");
        let id = open(&mut s, "plain.psd", psd(), Admission::ReviewIfRaw);
        s.wait_job(id).unwrap();
        assert_eq!(s.documents().len(), 2);
        assert!(!s.has_provisional(), "a file that is not a developed RAW is not held");
    }

    #[test]
    fn a_result_of_a_superseded_review_admits_nothing() {
        let mut s = Session::new();
        let id = open(&mut s, "shot.dng", dng(), Admission::ReviewIfRaw);
        s.next_review_generation();
        assert!(matches!(s.wait_job(id), Err(EngineError::Cancelled)));
        assert!(s.documents().is_empty(), "no document was admitted");
        assert!(!s.has_provisional());
        // An ordinary open started before the bump is not a review and is unaffected.
        let id = open(&mut s, "other.dng", dng(), Admission::Normal);
        s.next_review_generation();
        s.wait_job(id).unwrap();
        assert_eq!(s.documents().len(), 1);
    }

    /// P0C F-01: a review cancel supersedes RAW opens only; a PSD started for review that is still decoding opens.
    #[test]
    fn a_non_raw_review_open_survives_a_review_cancel() {
        let mut s = Session::new();
        let id = open(&mut s, "plain.psd", psd(), Admission::ReviewIfRaw);
        s.next_review_generation();
        let result = s.wait_job(id).unwrap();
        assert_eq!(s.documents().len(), 1);
        assert!(!s.has_provisional());
        assert_eq!(result["docId"], s.documents()[0].doc.id.0, "the result names the document by its id, next to the tab index");
        assert_eq!(result["document"], 0);
    }

    /// P0D (coverage): a JPEG started for review survives a review cancel, like a PSD.
    #[test]
    fn a_jpeg_review_open_survives_a_review_cancel() {
        let mut src = Session::new();
        src.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
        let doc = (*src.active().unwrap().doc).clone();
        let jpeg = photocraft_io::export(&doc, "x.jpg", &Default::default()).unwrap().bytes;
        let mut s = Session::new();
        let id = open(&mut s, "plain.jpg", jpeg, Admission::ReviewIfRaw);
        s.next_review_generation();
        let result = s.wait_job(id).unwrap();
        assert_eq!(s.documents().len(), 1);
        assert!(!s.has_provisional());
        assert_eq!(result["docId"], s.documents()[0].doc.id.0);
    }
    #[test]
    fn an_admission_goes_stale_when_its_review_is_superseded() {
        let mut s = Session::new();
        let id = open(&mut s, "shot.dng", dng(), Admission::ReviewIfRaw);
        s.wait_job(id).unwrap();
        let doc = s.documents()[0].doc.id;
        assert!(!s.admission_stale(doc));
        s.next_review_generation();
        assert!(s.admission_stale(doc));
        s.release_admission(doc);
        assert!(!s.admission_stale(doc), "released: no longer held by the job layer");
    }

    #[test]
    fn a_cancelled_open_admits_nothing() {
        let mut s = Session::new();
        let id = open(&mut s, "shot.dng", dng(), Admission::ReviewIfRaw);
        assert!(s.cancel_job(id));
        s.join_cancelled_jobs();
        assert!(s.documents().is_empty() && !s.has_provisional());
    }
    /// Stage 2A (Q2 A-3). A RAW under a non-RAW name (`scan.dat`) started for a review that is then
    /// cancelled must not be admitted, even when `rawDefaults.openInCameraRaw` is switched off before its result is
    /// applied.
    #[test]
    fn stage2a_a3_a_renamed_raw_of_a_cancelled_review_is_not_admitted_after_review_on_open_is_switched_off() {
        let mut s = Session::new();
        let id = open(&mut s, "scan.dat", dng(), Admission::ReviewIfRaw);
        s.next_review_generation();
        assert!(matches!(s.wait_job(id), Err(EngineError::Cancelled)), "CONTROL: superseded while the preference is on");
        assert!(s.documents().is_empty(), "CONTROL: nothing admitted");
        let mut s = Session::new();
        let id = open(&mut s, "scan.dat", dng(), Admission::ReviewIfRaw);
        s.next_review_generation();
        s.edit_prefs(|p| p.raw_defaults.open_in_camera_raw = false);
        let applied = s.wait_job(id).is_ok();
        assert!(
            !applied && s.documents().is_empty(),
            "Q2-A3: a RAW of the cancelled review generation was applied (ok={applied}) and left {} editable document(s)",
            s.documents().len()
        );
    }
}

/// Next-gate probes use production decode/admission and direct engine dispatch. They are intentionally
/// not shell/control tests. Native channels give an explicit prepared-result barrier without a sleep.
#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod next_gate_direct_engine_tests {
    use super::*;
    use std::sync::mpsc::{Sender, channel};
    use std::time::Duration;

    fn dng() -> Vec<u8> {
        use photocraft_raw::testgen::{DngSpec, mosaic, scene};
        let (w, h) = (48, 32);
        let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
        spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
        spec.build()
    }

    fn start_raw(s: &mut Session, name: &str) -> JobId {
        match s.start_open_with(name, OpenSource::Bytes(Arc::new(dng())), Admission::ReviewIfRaw).unwrap() {
            Started::Job(id) => id,
            Started::Done(_) => panic!("native probe requires a real background open"),
        }
    }

    fn open_raw(s: &mut Session, name: &str) -> DocId {
        let job = start_raw(s, name);
        let (tx, prepared) = prepared_result(s, job);
        assert!(tx.send(prepared).is_ok());
        drop(tx);
        let result = s.wait_job(job).unwrap();
        let id = DocId(result["docId"].as_u64().unwrap());
        let index = result["document"].as_u64().unwrap() as usize;
        assert_eq!(s.documents()[index].doc.id, id, "use the actual admitted id, not the decoded id or a presumed tab");
        assert!(s.provisional_documents().contains(&id));
        assert!(s.poll_jobs().iter().any(|event| event.id == job && matches!(event.outcome, JobOutcome::Done(_))));
        assert!(!s.has_jobs());
        id
    }

    fn activate_id(s: &mut Session, id: DocId) {
        let index = s.documents().iter().position(|st| st.doc.id == id).expect("probe target must still exist");
        assert!(s.set_active(index));
        assert_eq!(s.active().unwrap().doc.id, id);
    }

    fn accepted_control() -> (Session, DocId) {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8, "name": "NG accepted control"})).unwrap();
        s.execute("layer.new.layer", json!({"name": "NG recognizable control"})).unwrap();
        s.execute("edit.fill", json!({"color": "#763A19"})).unwrap();
        let id = s.active().unwrap().doc.id;
        assert!(s.active().unwrap().is_dirty());
        assert!(!s.has_provisional());
        (s, id)
    }

    fn document_state(st: &crate::DocState) -> Value {
        let mut pixels = blake3::Hasher::new();
        for (_, _, layer) in st.doc.walk() {
            pixels.update(&layer.id.0.to_le_bytes());
            if let Some(surface) = layer.surface() {
                for sample in surface.read_region(st.doc.bounds()) {
                    pixels.update(&sample.to_bits().to_le_bytes());
                }
            }
        }
        json!({
            "docId": st.doc.id.0, "path": st.path, "readOnlySource": st.source_read_only,
            "savedRevision": st.saved_revision, "dirty": st.is_dirty(),
            "pixelAndLayerFingerprint": pixels.finalize().to_hex().to_string(),
            "inspect": crate::inspect::document(st),
        })
    }

    fn state(s: &Session) -> Value {
        json!({
            "documents": s.documents().iter().map(document_state).collect::<Vec<_>>(),
            "activeId": s.active().map(|st| st.doc.id.0),
            "provisional": s.provisional_documents().iter().map(|id| id.0).collect::<Vec<_>>(),
            "admitted": s.jobs.admitted.iter().map(|(id, generation)| (id.0, generation)).collect::<Vec<_>>(),
            "generation": s.review_generation(), "pending": s.provisional_pending,
            "journal": s.journal, "runningJobs": s.jobs.running.len(),
            "pendingJob": s.jobs.pending.is_some(), "queuedEvents": s.jobs.events.len(),
        })
    }

    fn positive_edit(s: &mut Session, control: DocId, command: &str, params: Value) {
        activate_id(s, control);
        let before = document_state(s.active().unwrap());
        let doc = s.active().unwrap().doc.clone();
        let (revision, saved, steps) = {
            let st = s.active().unwrap();
            (st.revision, st.saved_revision, st.history.past_len())
        };
        let result = s.execute(command, params).expect("accepted positive control must execute the same command");
        let st = s.active().unwrap();
        assert_eq!(st.doc.id, control);
        assert_eq!(st.revision, revision + 1);
        assert_eq!(st.saved_revision, saved);
        assert_eq!(st.history.past_len(), steps + 1);
        assert!(st.is_dirty());
        assert!(!Arc::ptr_eq(&st.doc, &doc));
        let after = document_state(st);
        assert_ne!(after, before);
        assert_ne!(
            after["pixelAndLayerFingerprint"], before["pixelAndLayerFingerprint"],
            "the accepted positive control must change layer/content bytes, not only revision/history bookkeeping"
        );
        let edited_doc = st.doc.clone();
        assert!(s.undo(), "accepted edit can be undone");
        assert_eq!(s.active().unwrap().history.past_len(), steps);
        assert_eq!(document_state(s.active().unwrap())["pixelAndLayerFingerprint"], before["pixelAndLayerFingerprint"]);
        assert!(s.redo(), "accepted edit can be redone");
        assert!(Arc::ptr_eq(&s.active().unwrap().doc, &edited_doc));
        assert_eq!(s.active().unwrap().saved_revision, saved);
        assert!(s.active().unwrap().is_dirty());
        eprintln!("NG_POSITIVE {}", json!({"command": command, "result": result, "before": before, "after": after}));
    }

    fn assert_pending_refusal(command: &str, params: Value) {
        let (mut s, control) = accepted_control();
        let raw = open_raw(&mut s, "ng-pending.dng");
        positive_edit(&mut s, control, command, params.clone());
        let control_before = document_state(s.active().unwrap());
        activate_id(&mut s, raw);
        let before = state(&s);
        let snapshot = s.active().unwrap().doc.clone();
        let result = s.execute(command, params);
        let after = state(&s);
        eprintln!("NG_DIRECT {}", json!({"command": command, "result": format!("{result:?}"), "before": before, "after": after}));
        let control_after = s.documents().iter().find(|st| st.doc.id == control).unwrap();
        assert_eq!(document_state(control_after), control_before, "unrelated dirty control must be preserved");
        assert!(
            result.is_err() && before == after && Arc::ptr_eq(&s.active().unwrap().doc, &snapshot),
            "NG direct-engine invariant: {command} must refuse a production-admitted pending RAW before mutation; result={result:?}"
        );
    }

    #[test]
    fn ng_engine_01_direct_layer_edit_cannot_mutate_pending_raw() {
        assert_pending_refusal("layer.new.layer", json!({"name": "NG unauthorized pending layer"}));
    }

    #[test]
    fn ng_engine_02_direct_camera_raw_command_cannot_bypass_pending_ownership() {
        // A command-name exemption is insufficient: review-owned commit and arbitrary direct calls
        // reach the engine with the same id. The positive control proves these settings edit pixels.
        assert_pending_refusal("filter.cameraRaw", json!({"exposure": 0.25}));
    }

    /// Take the real worker's successfully decoded payload before production can apply it. Joining
    /// proves the worker is quiescent. Requeue the identical payload only after the test's cancel
    /// transition; poll_jobs/wait_job still invoke the original production apply closure.
    fn prepared_result(s: &mut Session, id: JobId) -> (Sender<Result<Payload>>, Result<Payload>) {
        let running = s.jobs.running.iter_mut().find(|job| job.id == id).unwrap();
        let payload = running.rx.recv_timeout(Duration::from_secs(30)).expect("watchdog: production decode did not prepare a result");
        assert!(payload.is_ok(), "real synthetic RAW decode must succeed before the cancellation barrier");
        if let Some(handle) = running.handle.take() {
            handle.join().expect("production decoder worker must exit normally");
        }
        let (tx, rx) = channel();
        running.rx = rx;
        (tx, payload)
    }

    fn superseded_before_apply(poll: bool) {
        let (mut s, control) = accepted_control();
        let before = document_state(s.active().unwrap());
        let journal = s.journal.clone();
        let job = start_raw(&mut s, "ng-stale.dng");
        let (tx, prepared) = prepared_result(&mut s, job);
        assert_eq!(s.documents().len(), 1, "the barrier is before real admission");
        assert!(!s.has_provisional());
        let old_generation = s.review_generation();
        s.next_review_generation();
        assert_ne!(s.review_generation(), old_generation);
        assert!(tx.send(prepared).is_ok(), "prepared result receiver must still exist before apply");
        drop(tx);
        if poll {
            let events = s.poll_jobs();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].id, job);
            assert_eq!(events[0].outcome, JobOutcome::Cancelled);
        } else {
            assert!(matches!(s.wait_job(job), Err(EngineError::Cancelled)));
            let events = s.poll_jobs();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].id, job);
            assert_eq!(events[0].outcome, JobOutcome::Cancelled);
        }
        assert_eq!(s.documents().len(), 1);
        assert_eq!(document_state(s.active().unwrap()), before);
        assert_eq!(s.active().unwrap().doc.id, control);
        assert_eq!(s.journal, journal);
        assert!(!s.has_jobs() && !s.has_provisional());
        assert!(s.jobs.admitted.is_empty() && s.jobs.pending.is_none());
        s.join_cancelled_jobs();
        assert!(s.jobs.draining.is_empty());
        assert!(s.poll_jobs().is_empty(), "completion queue was drained; no delayed event remains");
        eprintln!("NG_PREAPPLY {}", json!({"poll": poll, "discardedJob": job.0, "oldGeneration": old_generation, "state": state(&s)}));

        let fresh = open_raw(&mut s, "ng-fresh-generation.dng");
        assert_ne!(fresh, control);
        assert!(!s.admission_stale(fresh));
        assert_eq!(s.provisional_documents(), vec![fresh]);
        assert_eq!(s.documents().iter().find(|st| st.doc.id == control).map(document_state).unwrap(), before);
        s.release_admission(fresh);
        assert!(!s.has_provisional());
        positive_edit(&mut s, fresh, "layer.new.layer", json!({"name": "NG accepted RAW edit"}));
    }

    #[test]
    fn ng_engine_03_prepared_stale_raw_is_discarded_through_poll_jobs() {
        superseded_before_apply(true);
    }

    #[test]
    fn ng_engine_04_prepared_stale_raw_is_discarded_through_wait_job() {
        superseded_before_apply(false);
    }

    #[test]
    fn ng_engine_05_cancel_prepared_raw_drops_late_completion_and_preserves_control() {
        let (mut s, control) = accepted_control();
        let before = state(&s);
        let job = start_raw(&mut s, "ng-cancelled.dng");
        let (tx, prepared) = prepared_result(&mut s, job);
        assert!(s.cancel_job(job));
        // Cancellation owns the receiver. Even an already-computed payload cannot be queued later.
        assert!(tx.send(prepared).is_err());
        drop(tx);
        s.join_cancelled_jobs();
        let events = s.poll_jobs();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, job);
        assert_eq!(events[0].outcome, JobOutcome::Cancelled);
        assert!(matches!(s.wait_job(job), Err(EngineError::Cancelled)));
        assert_eq!(state(&s), before);
        assert_eq!(s.active().unwrap().doc.id, control);
        assert!(s.jobs.draining.is_empty());
        assert!(s.poll_jobs().is_empty());
        eprintln!("NG_CANCEL_PREPARED {}", state(&s));
    }

    #[test]
    fn ng_engine_06_stale_layer_target_does_not_retarget_after_review_cancel() {
        // Session::execute has no generic DocId parameter; use the command's real layer-id
        // contract. This covers a stale layer reference, not a claimed DocId dispatch API.
        let (mut s, control) = accepted_control();
        let raw = open_raw(&mut s, "ng-old-review.dng");
        let layer = s.active().unwrap().active_layer.unwrap();
        let old_index = s.documents().iter().position(|st| st.doc.id == raw).unwrap();
        s.next_review_generation();
        s.close(old_index).unwrap();
        s.release_admission(raw);
        let fresh = open_raw(&mut s, "ng-new-review.dng");
        assert_ne!(fresh, raw);
        assert_eq!(s.active_index(), Some(old_index), "the old tab index was reused by a different photograph");
        assert!(s.documents().iter().all(|st| st.doc.id != raw));
        let before = state(&s);
        let result = s.execute("layer.renameLayer", json!({"layer": layer.0, "name": "NG stale must not rename"}));
        assert!(result.is_err(), "stale layer identity must not fall back to the new active photograph");
        assert_eq!(state(&s), before);
        activate_id(&mut s, control);
        let before = state(&s);
        assert!(s.execute("document.activate", json!({"document": u64::MAX})).is_err());
        assert_eq!(state(&s), before, "invalid explicit tab target cannot select another document");
        eprintln!("NG_STALE_TARGET {}", json!({"oldDocId": raw.0, "freshDocId": fresh.0, "oldLayerId": layer.0, "state": state(&s)}));
    }

    #[test]
    fn ng_engine_07_owned_filter_is_synchronous_and_one_dispatch_only() {
        let (mut s, control) = accepted_control();
        let raw = open_raw(&mut s, "ng-owned.dng");
        let before = document_state(s.active().unwrap());
        let scope = s.begin_camera_raw_command(raw).unwrap();
        let revision = s.active().unwrap().revision;
        s.execute("filter.cameraRaw", json!({"exposure": 0.5})).unwrap();
        assert_eq!(s.active().unwrap().revision, revision + 1);
        assert_ne!(document_state(s.active().unwrap())["pixelAndLayerFingerprint"], before["pixelAndLayerFingerprint"]);
        let once = state(&s);
        assert!(s.execute("filter.cameraRaw", json!({"exposure": 0.5})).is_err());
        assert_eq!(state(&s), once, "keeping the scope alive cannot reuse consumed authority");
        drop(scope);
        let _scope = s.begin_camera_raw_command(raw).unwrap();
        assert!(matches!(s.start("filter.cameraRaw", json!({"exposure": -0.25})).unwrap(), Started::Done(_)));
        assert!(!s.has_jobs(), "Camera Raw's registered filter is synchronous even via start");
        assert_eq!(s.provisional_documents(), vec![raw], "owned adjustment does not accept the document");
        activate_id(&mut s, control);
        assert!(s.begin_camera_raw_command(raw).is_err(), "grant requires the actual active owner");
    }

    #[test]
    fn ng_engine_08_expired_stale_and_failed_scopes_do_not_leak_authority() {
        let (mut s, control) = accepted_control();
        let raw = open_raw(&mut s, "ng-scope.dng");
        for (command, params) in [
            ("layer.new.layer", json!({})),
            ("unknown.nextGate", json!({})),
            ("filter.cameraRaw", json!([])),
            ("filter.cameraRaw", json!({"exposure": "invalid"})),
        ] {
            let _scope = s.begin_camera_raw_command(raw).unwrap();
            let before = state(&s);
            assert!(s.execute(command, params).is_err());
            assert_eq!(state(&s), before);
            assert!(s.execute("filter.cameraRaw", json!({"exposure": 0.5})).is_err());
            assert_eq!(state(&s), before);
        }
        drop(s.begin_camera_raw_command(raw).unwrap());
        assert!(s.execute("filter.cameraRaw", json!({"exposure": 0.5})).is_err());
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = s.begin_camera_raw_command(raw).unwrap();
            panic!("NG test-only unwind before dispatch");
        }));
        assert!(unwind.is_err());
        let after_unwind = state(&s);
        assert!(s.execute("filter.cameraRaw", json!({"exposure": 0.5})).is_err(), "unwinding drops an unused scope");
        assert_eq!(state(&s), after_unwind);
        let _scope = s.begin_camera_raw_command(raw).unwrap();
        activate_id(&mut s, control);
        s.execute("document.inspect", json!({})).unwrap();
        activate_id(&mut s, raw);
        assert!(s.execute("filter.cameraRaw", json!({"exposure": 0.5})).is_err(), "different document consumes the grant");
        let _scope = s.begin_camera_raw_command(raw).unwrap();
        s.authorize = Some(|_, _| Err(EngineError::Other("NG authorization refusal".into())));
        assert!(s.execute("filter.cameraRaw", json!({"exposure": 0.5})).is_err());
        s.authorize = None;
        assert!(s.execute("filter.cameraRaw", json!({"exposure": 0.5})).is_err());
        let _scope = s.begin_camera_raw_command(raw).unwrap();
        s.next_review_generation();
        let before = state(&s);
        assert!(s.execute("filter.cameraRaw", json!({"exposure": 0.5})).is_err());
        assert_eq!(state(&s), before);
        assert!(s.begin_camera_raw_command(raw).is_err(), "stale admission cannot receive a fresh grant");
    }

    #[test]
    fn ng_engine_09_named_held_sources_destinations_and_dynamic_commands_are_refused() {
        let (mut s, control) = accepted_control();
        let raw = open_raw(&mut s, "ng-named.dng");
        let index = s.documents().iter().position(|st| st.doc.id == raw).unwrap();
        let name = s.active().unwrap().doc.name.clone();
        let layer = s.active().unwrap().active_layer.unwrap();
        activate_id(&mut s, control);
        for (command, params) in [
            ("layer.copyToDocument", json!({"source": index, "document": 0, "layers": [layer.0]})),
            ("layer.copyToDocument", json!({"source": 0, "document": index})),
            ("select.saveSelection", json!({"document": index as f64, "channel": "new"})),
            ("channel.duplicate", json!({"document": index, "channel": "red"})),
            ("image.adjustments.matchColor", json!({"source": index})),
            ("filter.distort.displace", json!({"mapDocument": name})),
            ("file.close", json!({"document": index})),
            ("document.activate", json!({"document": index})),
            ("file.closeAll", json!({})),
            ("actions.play", json!({})),
            ("edit.purge.all", json!({})),
        ] {
            assert!(crate::commands::find(command).is_some(), "real registered command: {command}");
            let before = state(&s);
            let error = s.execute(command, params).unwrap_err().to_string();
            assert!(error.contains("unaccepted RAW"), "{command}: reached ownership guard, not invalid-param failure: {error}");
            assert_eq!(state(&s), before, "{command}");
        }
        positive_edit(&mut s, control, "layer.new.layer", json!({"name": "NG accepted during held review"}));
    }

    #[test]
    fn ng_engine_10_read_session_new_and_reorder_preserve_adversarial_held_state() {
        let (mut s, held) = accepted_control();
        s.execute("select.all", json!({})).unwrap();
        s.execute("select.float", json!({"dx": 2, "dy": 1})).unwrap();
        assert!(s.active().unwrap().floating.is_some());
        let stale_layer = LayerId::fresh();
        let st = s.active_mut().unwrap();
        st.channel_view.layer_mask = Some(crate::mask_view_cmds::LayerMaskView { layer: stale_layer, mode: crate::mask_view_cmds::MaskViewMode::Gray });
        Arc::make_mut(&mut st.doc).slices.list.push(photocraft_doc::slices::Slice {
            id: 1,
            origin: photocraft_doc::slices::SliceOrigin::Layer,
            layer: Some(stale_layer),
            rect: photocraft_geom::Rect::new(0, 0, 3, 3),
            ..Default::default()
        });
        s.set_provisional_documents(vec![held]);
        let before = document_state(s.active().unwrap());
        let doc = s.active().unwrap().doc.clone();
        let mask = s.active().unwrap().channel_view.layer_mask;
        for (command, params) in [
            ("document.inspect", json!({})),
            ("jobs.list", json!({})),
            ("tools.setColors", json!({"foreground": "#ff0000"})),
            ("file.new", json!({"width": 4, "height": 4})),
        ] {
            activate_id(&mut s, held);
            s.execute(command, params).unwrap();
            let st = s.documents().iter().find(|st| st.doc.id == held).unwrap();
            assert!(Arc::ptr_eq(&st.doc, &doc), "{command} cannot trigger slice refresh or floating drop");
            assert_eq!(document_state(st), before, "{command}");
            assert_eq!(st.channel_view.layer_mask, mask);
            assert!(st.floating.is_some());
        }
        activate_id(&mut s, held);
        let mut before_docs: Vec<_> = s.documents().iter().map(|st| (st.doc.id.0, document_state(st))).collect();
        before_docs.sort_by_key(|(id, _)| *id);
        let admitted = s.jobs.admitted.clone();
        let generation = s.review_generation();
        let journal = s.journal.clone();
        s.execute("document.move", json!({"document": 0, "to": 1})).unwrap();
        let mut after_docs: Vec<_> = s.documents().iter().map(|st| (st.doc.id.0, document_state(st))).collect();
        after_docs.sort_by_key(|(id, _)| *id);
        assert_eq!(after_docs, before_docs);
        assert_eq!(s.active().unwrap().doc.id, held);
        assert_eq!(s.active_index(), Some(1));
        assert_eq!(s.provisional_documents(), vec![held]);
        assert_eq!(s.jobs.admitted, admitted);
        assert_eq!(s.review_generation(), generation);
        assert_eq!(s.journal, journal);
        assert!(s.active().unwrap().floating.is_some());
        assert_eq!(s.active().unwrap().channel_view.layer_mask, mask);
        assert!(s.execute("document.move", json!({"document": 1.0, "to": 0})).is_err(), "strict tab syntax remains unchanged");
        // Positive: the existing hooks still run normally once this same document is accepted.
        s.set_provisional_documents(Vec::new());
        s.execute("document.inspect", json!({})).unwrap();
        assert!(s.active().unwrap().floating.is_none());
        assert!(s.active().unwrap().channel_view.layer_mask.is_none());
        assert!(s.active().unwrap().doc.slices.list.is_empty());
    }

    #[test]
    fn ng_engine_11_prepared_engine_edit_rechecks_target_without_global_generation_block() {
        for held_target in [true, false] {
            let (mut s, _) = accepted_control();
            s.edit("NG textured pixels", |doc, active| {
                let surface = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
                let mut pixels = Vec::new();
                for y in 0..8 {
                    for x in 0..8 {
                        let v = ((x * 7 + y * 13) % 31) as f32 / 30.0;
                        pixels.extend_from_slice(&[v, 1.0 - v, 0.3, 1.0]);
                    }
                }
                surface.write_region(photocraft_geom::Rect::new(0, 0, 8, 8), &pixels);
                Ok(())
            })
            .unwrap();
            let target = s.active().unwrap().doc.id;
            let before = document_state(s.active().unwrap());
            let raw = open_raw(&mut s, "ng-other-review.dng");
            activate_id(&mut s, target);
            let job = match s.start("filter.blur.gaussianBlur", json!({"radius": 2})).unwrap() {
                Started::Job(job) => job,
                Started::Done(_) => panic!("real registered worker required"),
            };
            let (tx, prepared) = prepared_result(&mut s, job);
            if held_target {
                s.set_provisional_documents(vec![target]);
            }
            s.next_review_generation();
            activate_id(&mut s, raw);
            let active = s.active().unwrap().doc.id;
            let journal = s.journal.clone();
            assert!(tx.send(prepared).is_ok());
            drop(tx);
            let result = s.wait_job(job);
            assert_eq!(s.active().unwrap().doc.id, active, "apply rejection/success restores the actual active photograph");
            let after = document_state(s.documents().iter().find(|st| st.doc.id == target).unwrap());
            if held_target {
                assert!(result.unwrap_err().to_string().contains("unaccepted RAW"));
                assert_eq!(after, before);
                assert_eq!(s.journal, journal);
            } else {
                result.unwrap();
                assert_ne!(after["pixelAndLayerFingerprint"], before["pixelAndLayerFingerprint"]);
                assert_eq!(s.journal.len(), journal.len() + 1, "accepted job survives unrelated review generation change");
            }
            assert!(s.provisional_documents().contains(&raw));
            assert!(!s.has_jobs());
            s.poll_jobs();
            s.join_cancelled_jobs();
        }
    }

    #[test]
    fn ng_engine_12_cancel_command_cannot_take_held_job_but_can_cancel_accepted_job() {
        for held_target in [true, false] {
            let (mut s, control) = accepted_control();
            let raw = open_raw(&mut s, "ng-cancel-owner.dng");
            activate_id(&mut s, control);
            let job = match s.start("filter.blur.gaussianBlur", json!({"radius": 2})).unwrap() {
                Started::Job(job) => job,
                Started::Done(_) => panic!("real registered worker required"),
            };
            let (tx, prepared) = prepared_result(&mut s, job);
            if held_target {
                s.set_provisional_documents(vec![control]);
                for params in [json!({"job": job.0}), json!({})] {
                    let before = state(&s);
                    assert!(s.execute("jobs.cancel", params).unwrap_err().to_string().contains("unaccepted RAW"));
                    assert_eq!(state(&s), before);
                }
                assert!(s.cancel_job(job), "trusted review owner can still cancel its own work");
            } else {
                activate_id(&mut s, raw);
                assert_eq!(s.execute("jobs.cancel", json!({"job": job.0})).unwrap()["cancelled"], 1);
            }
            assert!(tx.send(prepared).is_err());
            s.join_cancelled_jobs();
            assert!(!s.has_jobs());
            assert!(s.poll_jobs().iter().any(|event| event.id == job && matches!(event.outcome, JobOutcome::Cancelled)));
        }
    }

    #[test]
    fn ng_engine_13_named_new_is_not_a_global_sentinel_and_active_docid_is_not_an_index() {
        let (mut s, _) = accepted_control();
        // Adversarial IDs deliberately distinguish a stale-dialog DocId from tab index 1.
        Arc::make_mut(&mut s.active_mut().unwrap().doc).id = DocId(1);
        let accepted = DocId(1);
        s.execute("layer.newFillLayer.solidColor", json!({"color": "#123456"})).unwrap();
        let fill = s.active().unwrap().active_layer.unwrap();
        let raw = open_raw(&mut s, "ng-name.dng");
        Arc::make_mut(&mut s.active_mut().unwrap().doc).name = "new".into();
        let held_index = s.active_index().unwrap();
        assert_eq!(held_index, 1);
        activate_id(&mut s, accepted);
        let before = state(&s);
        let error = s.execute("filter.distort.displace", json!({"mapDocument": "new"})).unwrap_err().to_string();
        assert!(error.contains("unaccepted RAW"), "a real held document called new is still protected: {error}");
        assert_eq!(state(&s), before);
        let held_before = document_state(s.documents().get(held_index).unwrap());
        let revision = s.active().unwrap().revision;
        let color_before = format!("{:?}", s.active().unwrap().doc.layer(fill).unwrap().content);
        s.execute("layer.fill.solidColor.set", json!({"document": accepted.0, "layer": fill.0, "color": "#abcdef"})).unwrap();
        assert_eq!(s.active().unwrap().revision, revision + 1);
        assert_ne!(format!("{:?}", s.active().unwrap().doc.layer(fill).unwrap().content), color_before, "the accepted fill content really changed");
        assert_eq!(document_state(s.documents().get(held_index).unwrap()), held_before);
        assert_eq!(s.active().unwrap().doc.id, accepted);
        assert!(s.provisional_documents().contains(&raw));
    }

    // Compare user document state and retained history, not the session journal: successful
    // script manager/browse commands retain their original journaled command behavior.
    fn documents_with_history(s: &Session) -> Value {
        json!(
            s.documents()
                .iter()
                .map(|st| json!({
                    "document": document_state(st),
                    "pointer": Arc::as_ptr(&st.doc) as usize,
                    "revision": st.revision,
                    "historyStates": st.history.max_states,
                    "historyBytes": st.history.max_bytes,
                    "pastEntries": st.history.entries(),
                    "pastPointers": (0..st.history.past_len()).map(|i| Arc::as_ptr(&st.history.state(i).unwrap()) as usize).collect::<Vec<_>>(),
                    "redoLabels": st.history.redo_labels().collect::<Vec<_>>(),
                    "pixelBytes": st.history.pixel_bytes(&st.doc),
                }))
                .collect::<Vec<_>>()
        )
    }

    fn owned_history(s: &mut Session, raw: DocId) {
        activate_id(s, raw);
        for exposure in [0.1, 0.2, 0.3, 0.4] {
            let _scope = s.begin_camera_raw_command(raw).unwrap();
            s.execute("filter.cameraRaw", json!({"exposure": exposure})).unwrap();
        }
        assert_eq!(s.active().unwrap().history.past_len(), 4);
    }

    fn script_binding() -> Value {
        json!({"add": {"event": "openDocument", "steps": [["layer.new.layer", {"name": "NG explicit event positive"}]]}})
    }

    #[test]
    fn ng_engine_15_script_compatibility_preserves_held_raw_and_structured_stop() {
        let (mut s, accepted) = accepted_control();
        let raw = open_raw(&mut s, "ng-script-compatibility.dng");
        owned_history(&mut s, raw);
        let before = documents_with_history(&s);
        let event_log = s.file_menu.event_log.clone();
        let manager = s.execute("file.scripts.scriptEventsManager", script_binding()).unwrap();
        assert_eq!(manager["enabled"], true);
        assert_eq!(manager["bindings"].as_array().unwrap().len(), 1);
        assert_eq!(s.prefs().script_events.bindings.len(), 1);
        assert_eq!(documents_with_history(&s), before);
        let held_index = s.documents().iter().position(|st| st.doc.id == raw).unwrap();
        for active in [accepted, raw] {
            activate_id(&mut s, active);
            let before = documents_with_history(&s);
            assert_eq!(crate::automate_cmds::fire_event(&mut s, "openDocument"), 0);
            let browse = s
                .execute(
                    "file.scripts.browse",
                    json!({"steps": [
                        ["layer.new.layer", {"name": "NG must never run"}],
                        ["document.activate", {"document": held_index}],
                        ["layer.new.layer", {}],
                        ["file.close", {"document": held_index}]
                    ]}),
                )
                .unwrap();
            assert_eq!(browse["ok"], false);
            assert_eq!(browse["steps"], 4);
            let results = browse["results"].as_array().unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0]["command"], "layer.new.layer");
            assert!(results[0]["error"].as_str().unwrap().contains("stopped"));
            assert_eq!(documents_with_history(&s), before);
            assert_eq!(s.active().unwrap().doc.id, active);
            assert_eq!(s.file_menu.event_log, event_log);
            assert!(s.provisional_documents().contains(&raw));
        }
        let held_before = document_state(&s.documents()[held_index]);
        s.release_admission(raw);
        activate_id(&mut s, accepted);
        let layers = s.active().unwrap().doc.layer_count();
        assert_eq!(crate::automate_cmds::fire_event(&mut s, "openDocument"), 1, "explicit positive after release, not automatic replay");
        assert_eq!(s.active().unwrap().doc.layer_count(), layers + 1);
        assert_eq!(document_state(&s.documents()[held_index]), held_before);
        assert_eq!(s.file_menu.event_log.len(), event_log.len() + 1);
    }

    #[test]
    fn ng_engine_16_script_manager_refuses_any_held_history_cap_mismatch() {
        for mismatch_states in [true, false] {
            let (mut s, accepted) = accepted_control();
            let first = open_raw(&mut s, "ng-script-first.dng");
            let second = open_raw(&mut s, "ng-script-second.dng");
            // Trusted test-only adversarial state: ordinary admitted histories use matching
            // preference limits. Ensure the predicate covers every held document, not active only.
            if mismatch_states {
                s.active_mut().unwrap().history.max_states += 1;
            } else {
                s.active_mut().unwrap().history.max_bytes -= 1;
            }
            for active in [accepted, first, second] {
                activate_id(&mut s, active);
                let before = documents_with_history(&s);
                let prefs = s.prefs_value();
                let session = state(&s);
                let error = s.execute("file.scripts.scriptEventsManager", script_binding()).unwrap_err().to_string();
                assert!(error.contains("unaccepted RAW"));
                assert_eq!(documents_with_history(&s), before);
                assert_eq!(s.prefs_value(), prefs);
                assert_eq!(state(&s), session);
            }
            s.release_admission(first);
            s.release_admission(second);
            s.execute("file.scripts.scriptEventsManager", script_binding()).unwrap();
            let limits = &s.prefs().performance;
            assert!(
                s.documents()
                    .iter()
                    .all(|st| { st.history.max_states == limits.history_states.max(1) as usize && st.history.max_bytes == limits.history_budget_bytes() }),
                "the existing manager reapplies limits normally after release"
            );
        }
    }

    #[test]
    fn ng_engine_17_script_manager_refuses_held_history_byte_pressure_with_matching_caps() {
        let (mut s, accepted) = accepted_control();
        let pressured = open_raw(&mut s, "ng-script-pressure.dng");
        owned_history(&mut s, pressured);
        let other = open_raw(&mut s, "ng-script-other.dng");
        // Trusted test-only setup bypasses normal apply_prefs/trim to probe its preflight. This
        // is not a claim that normal preference commands can create this admitted state.
        s.prefs.edit(|prefs| prefs.performance.memory_usage_mb = 1);
        let bytes = s.prefs().performance.history_budget_bytes();
        for st in &mut s.docs {
            st.history.max_bytes = bytes;
        }
        let pressured_index = s.documents().iter().position(|st| st.doc.id == pressured).unwrap();
        let pressured_state = &s.documents()[pressured_index];
        assert!(pressured_state.history.pixel_bytes(&pressured_state.doc) > bytes);
        let mut would_trim = pressured_state.history.clone();
        let dropped = would_trim.trim(&pressured_state.doc);
        assert!(dropped > 0, "positive oracle: unchanged caps alone would still trim real history records");
        let past_before = pressured_state.history.past_len();
        for active in [accepted, other, pressured] {
            activate_id(&mut s, active);
            let before = documents_with_history(&s);
            let prefs = s.prefs_value();
            let session = state(&s);
            let error = s.execute("file.scripts.scriptEventsManager", script_binding()).unwrap_err().to_string();
            assert!(error.contains("unaccepted RAW"));
            assert_eq!(documents_with_history(&s), before);
            assert_eq!(s.prefs_value(), prefs);
            assert_eq!(state(&s), session);
        }
        s.release_admission(pressured);
        s.release_admission(other);
        s.execute("file.scripts.scriptEventsManager", script_binding()).unwrap();
        assert_eq!(s.documents()[pressured_index].history.past_len(), past_before - dropped);
        assert_eq!(s.prefs().script_events.bindings.len(), 1);
        eprintln!("NG_SCRIPT_PRESSURE {}", json!({"budgetBytes": bytes, "pastBefore": past_before, "droppedAfterRelease": dropped}));
    }
}
