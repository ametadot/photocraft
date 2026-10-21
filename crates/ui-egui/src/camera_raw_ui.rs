//! Filter › Camera Raw Filter…: a large dialog like Adobe Camera Raw's (preview on the left,
//! edit panels on the right: Light, Color, Effects, Curve, Color Mixer, Color Grading, Detail).
//!
//! The preview runs [`photocraft_algo::camera_raw::develop`] on a CPU proxy of the layer
//! (≤ 900 px, `pixel_scale` keeps pixel radii true to the full image), recomputed when a
//! control changes: on a worker for the dialog's own controls, so a drag never waits for it, and
//! on the UI thread for the control channel, so a reply describes its own settings. OK runs `filter.cameraRaw` with the non-default settings, so the result is
//! one history step (or a smart filter on a smart object) and exactly replayable.
//!
//! Control channel: `ui.menu.invoke {"id":"filter.cameraRaw","params":{"ui":{"set":{…},
//! "commit":true | "cancel":true}}}`; the reply describes the dialog. `{"ui":{"saveSettings":true}}`
//! and `{"ui":{"loadSettings":true}}` run Save Settings… / Load Settings… (Camera Raw `.xmp`
//! settings files, see [`photocraft_io::camera_raw_xmp`]) through the file dialog.

use egui::{Align2, Color32, FontId, Pos2, Rect as ERect, Sense, Stroke, TextureHandle, pos2, vec2};
use photocraft_algo::camera_raw::{CameraRaw, Wheel, curve_lut};
use photocraft_algo::histogram::RgbHistogram;

use crate::camera_raw_scope_ui::{MAX_SAMPLERS, ToneZone};
use photocraft_doc::LayerId;
use photocraft_geom::Rect;
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::theme::Tokens;
use crate::widgets;

/// Blend each independent proxy pixel without changing the scalar arithmetic or
/// the zip-to-shortest behavior for malformed selection lengths.
fn blend_preview_selection(pixels: &mut [[f32; 4]], original: &[[f32; 4]], mask: &[f32]) {
    let blend = |((out, original), coverage): ((&mut [f32; 4], &[f32; 4]), &f32)| {
        for (out, original) in out.iter_mut().zip(original) {
            *out = *original + (*out - *original) * coverage;
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    if pixels.len().min(original.len()).min(mask.len()) >= 64 * 1024 {
        use rayon::prelude::*;
        pixels.par_iter_mut().zip(original.par_iter()).zip(mask.par_iter()).with_min_len(16 * 1024).for_each(blend);
        return;
    }
    pixels.iter_mut().zip(original).zip(mask).for_each(blend);
}

// Registered here, not in lib.rs: the Filmstrip is a child of the Camera Raw dialog.
#[path = "camera_raw_edit_actions.rs"]
mod edit_actions;
#[path = "camera_raw_filmstrip_ui.rs"]
mod filmstrip_ui;
#[cfg(not(target_arch = "wasm32"))]
#[path = "camera_raw_jpeg_ui.rs"]
mod jpeg_ui;
#[cfg(not(target_arch = "wasm32"))]
#[path = "camera_raw_persistence_ui.rs"]
mod persistence_ui;
#[path = "camera_raw_preferences_ui.rs"]
mod preferences_ui;
#[path = "camera_raw_selection.rs"]
mod selection_ui;

pub(crate) fn intercept_close(app: &mut PhotocraftApp) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        persistence_ui::request_exit(app)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        false
    }
}

pub(crate) fn admission_changed(app: &mut PhotocraftApp) {
    #[cfg(not(target_arch = "wasm32"))]
    persistence_ui::admission_changed(app);
    #[cfg(target_arch = "wasm32")]
    let _ = app;
}

/// See `persistence_ui::unadmitted`.
pub(crate) fn unadmitted(app: &PhotocraftApp, key: &str) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    return persistence_ui::unadmitted(app, key);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, key);
        false
    }
}

/// See `persistence_ui::forget_admission`.
pub(crate) fn forget_admission(app: &mut PhotocraftApp, key: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    persistence_ui::forget_admission(app, key);
    #[cfg(target_arch = "wasm32")]
    let _ = (app, key);
}

pub(crate) fn admission_allowed(app: &PhotocraftApp) -> Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::finishing(app) || jpeg_ui::is_running(app) || jpeg_ui::is_modal_open(app) {
        return Err("Finish the current Camera Raw save or decision before opening another batch".into());
    }
    let _ = app;
    Ok(())
}

const PROXY_SIDE: usize = 900;
const PANEL_W: f32 = 330.0;
const BANDS: [&str; 8] = ["Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta"];

pub struct CameraRawDialog {
    pub layer: LayerId,
    layer_name: String,
    pub params: CameraRaw,
    /// Settings when the dialog opened (stored settings of an edited smart filter).
    original: CameraRaw,
    target: Target,
    curve_state: crate::state::PointCurveState,
    curve_rect: Option<ERect>,
    /// Proxy pixels (straight RGBA) and size.
    pub(crate) proxy: std::sync::Arc<Vec<[f32; 4]>>,
    pub(crate) selection: Option<std::sync::Arc<Vec<f32>>>,
    pub(crate) pw: usize,
    pub(crate) ph: usize,
    full_w: usize,
    full_h: usize,
    preview_ppp: f32,
    pub(crate) detail: super::camera_raw_detail_ui::DetailPreview,
    display: std::sync::Arc<photocraft_engine::display_color::CanvasDisplay>,
    pub(crate) viewport: Option<ERect>,
    pub(crate) float: bool,
    tex: Option<TextureHandle>,
    before_tex: Option<TextureHandle>,
    before_display_key: Option<u64>,
    pub(crate) before_histogram: RgbHistogram,
    pub(crate) processed: Vec<[f32; 4]>,
    pub(crate) lab_transform: std::sync::Arc<photocraft_cms::Transform>,
    pub(crate) scope_transform: std::sync::Arc<photocraft_cms::Transform>,
    pub(crate) scope: super::camera_raw_scope_ui::ScopeView,
    pub(crate) after_histogram: RgbHistogram,
    pub(crate) preview_revision: u64,
    histogram_ms: f64,
    pub(crate) dirty: bool,
    pub show_before: bool,
    mixer_tab: usize,
    pub render_ms: f64,
    /// Set for the open-time dialog ([`Target::OpenRaw`]).
    raw_open: Option<RawOpen>,
    /// The open-time dialog's camera tone curve: Shadows / Highlights act on the raw data there,
    /// in linear light before this curve, and the preview undoes it to show the same.
    raw_curve: Option<std::sync::Arc<Vec<[f32; 2]>>>,
    /// The proxy develop running on a worker ([`Self::request_render`]).
    pending: Option<std::sync::mpsc::Receiver<Result<ProxyFrame, String>>>,
    /// The last worker failure the status bar shows, with the preview revision it failed on.
    preview_error: Option<(String, u64)>,
    /// Settings before each finished edit, so Ctrl+Z works inside the dialog (#4100).
    undo: Vec<CameraRaw>,
    redo: Vec<CameraRaw>,
    /// Params at pointer-down. One drag is one undo step.
    edit_origin: Option<CameraRaw>,
    /// Transient window state only; blank preference pages do not persist settings.
    preferences: preferences_ui::Preferences,
}

impl CameraRawDialog {
    pub fn describe(&self, view: &crate::state::CameraRawScopeState, navigation: &crate::state::CameraRawPreviewState) -> Value {
        let h = self.histogram();
        let mut result = json!({"layer": self.layer.0, "params": serde_json::to_value(&self.params).unwrap_or(Value::Null), "proxy": [self.pw, self.ph],
            "renderMs": self.render_ms, "histogramMs": self.histogram_ms, "previewRevision": self.preview_revision, "before": self.show_before,
            "histogram": {"source": if self.show_before { "before" } else { "after" }, "approximate": true, "size": [self.pw, self.ph],
                "sampleDomain": "preview-rgb", "range": [0, 1], "red": h.channels[0].as_slice(), "green": h.channels[1].as_slice(), "blue": h.channels[2].as_slice(),
                "samples": h.samples, "transparent": h.transparent, "invalid": h.invalid, "underflow": h.underflow, "overflow": h.overflow, "shadows": h.shadows, "highlights": h.highlights}});
        result["smartFilter"] = json!(match self.target {
            Target::SmartFilter(index) => Some(index),
            Target::NewFilter | Target::OpenRaw => None,
        });
        result["openingRaw"] = json!(self.raw_open.as_ref().map(|r| json!({"name": r.name, "path": r.path})));
        result["view"] = json!(navigation);
        result["sourceSize"] = json!([self.full_w, self.full_h]);
        result["previewPending"] = json!(self.render_pending());
        result["previewApproximate"] = json!(!self.detail.ready && (self.pw != self.full_w || self.ph != self.full_h));
        result["detailPending"] = json!(self.detail.pending());
        result["detailError"] = json!(self.detail.error);
        result["viewportRect"] = json!(self.viewport.map(|r| [r.left(), r.top(), r.right(), r.bottom()]));
        result["zoom"] = json!(self.viewport.map(|r| navigation.scale(r, vec2(self.full_w as f32, self.full_h as f32) / self.preview_ppp)));
        result["scope"] = serde_json::to_value(view).unwrap_or(Value::Null);
        result["hasScopeSelection"] = json!(self.selection.is_some());
        result["vectorscope"] = json!(
            self.scope
                .cache
                .as_ref()
                .map(|(_, before, selected, data)| json!({"samples":data.samples,"before":before,"selectedRegion":selected,"bins":data.counts}))
        );
        result["scopeMs"] = json!(self.scope.ms);
        result["scopeRevision"] = json!(self.scope.revision);
        result["overlayRevision"] = json!(self.scope.overlay_key.map(|k| k.0));
        result["pointerReadout"] = self.scope.pointer_sample.and_then(|p| self.readout(p, view.lab)).unwrap_or(Value::Null);
        result["samplerReadouts"] = json!(view.samplers.iter().take(MAX_SAMPLERS).map(|p| self.readout(*p, view.lab)).collect::<Vec<_>>());
        result["hoveredZone"] = json!(self.scope.hovered_zone);
        result["previewRect"] = json!(self.scope.preview_rect.map(|r| [r.left(), r.top(), r.right(), r.bottom()]));
        result["curveState"] = json!(self.curve_state);
        result["curveRect"] = json!(self.curve_rect.map(|r| [r.left(), r.top(), r.right(), r.bottom()]));
        result["hoverSample"] = json!(self.hover_sample(view.vectorscope));
        result["vectorscopeRect"] = json!(self.scope.vectorscope_rect.map(|r| [r.left(), r.top(), r.right(), r.bottom()]));
        result["scopeRect"] = json!(self.scope.rect.map(|r| [r.left(), r.top(), r.right(), r.bottom()]));
        result
    }

    pub(crate) fn histogram(&self) -> &RgbHistogram {
        if self.show_before { &self.before_histogram } else { &self.after_histogram }
    }

    /// Params for the engine: only the settings that differ from the defaults.
    pub fn command_params(&self) -> Value {
        non_defaults(&self.params)
    }

    fn image(px: &[[f32; 4]], w: usize, h: usize, display: &photocraft_engine::display_color::CanvasDisplay) -> egui::ColorImage {
        // Bands copied straight from `px`, converted in parallel (#4682), not a whole copy of the
        // proxy converted on one thread.
        super::camera_raw_detail_ui::display_image(Rect::new(0, 0, w as i32, h as i32), display, |r, out| {
            let start = (r.y0.max(0) as usize).saturating_mul(w);
            if let Some(src) = px.get(start..start.saturating_add(out.len())) {
                out.copy_from_slice(src);
            }
        })
    }

    /// Develops the proxy with the current settings and shows it, on this thread. The control
    /// channel and the first frame use this, so a reply always describes its own settings; the
    /// dialog's own controls go through [`Self::request_render`].
    pub(crate) fn render(&mut self, ctx: &egui::Context) {
        // A render still running on the worker has older settings than this one.
        self.pending = None;
        let frame = self.proxy_job().run();
        self.publish(ctx, frame);
        self.dirty = false;
    }

    fn proxy_job(&self) -> ProxyJob {
        ProxyJob {
            proxy: self.proxy.clone(),
            selection: self.selection.clone(),
            raw_curve: self.raw_curve.clone(),
            params: self.params.clone(),
            float: self.float,
            size: (self.pw, self.ph),
            full_w: self.full_w,
            display: self.display.clone(),
        }
    }

    fn publish(&mut self, ctx: &egui::Context, frame: ProxyFrame) {
        match &mut self.tex {
            Some(t) => t.set(frame.image, egui::TextureOptions::LINEAR),
            None => self.tex = Some(ctx.load_texture("camera-raw-after", frame.image, egui::TextureOptions::LINEAR)),
        }
        if self.before_display_key != Some(frame.display.key) {
            let before = Self::image(&self.proxy, self.pw, self.ph, &frame.display);
            match &mut self.before_tex {
                Some(t) => t.set(before, egui::TextureOptions::LINEAR),
                None => self.before_tex = Some(ctx.load_texture("camera-raw-before", before, egui::TextureOptions::LINEAR)),
            }
            self.before_display_key = Some(frame.display.key);
        }
        self.after_histogram = frame.histogram;
        self.histogram_ms = frame.histogram_ms;
        self.processed = frame.px;
        self.scope.cache = None;
        self.render_ms = frame.ms;
        self.preview_revision = self.preview_revision.saturating_add(1);
    }

    /// Starts developing the proxy with the current settings on a worker, unless one is already
    /// running: then [`Self::poll_render`] starts the next one with the settings of that moment.
    /// A slider drag never waits for the develop, which takes hundreds of milliseconds with
    /// Shadows or Highlights set (#5964); the preview shows the latest finished frame.
    fn request_render(&mut self, ctx: &egui::Context) {
        if !self.dirty || self.pending.is_some() {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let job = self.proxy_job();
            let (tx, rx) = std::sync::mpsc::channel::<Result<ProxyFrame, String>>();
            let repaint = ctx.clone();
            let spawned = std::thread::Builder::new().name("camera-raw-proxy".into()).spawn(move || {
                let frame = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run())).map_err(|_| "Camera Raw preview failed".to_string());
                // The dialog may have closed or rendered on its own meanwhile; then nobody listens.
                if tx.send(frame).is_ok() {
                    repaint.request_repaint();
                }
            });
            match spawned {
                Ok(_) => {
                    self.pending = Some(rx);
                    self.dirty = false;
                }
                // No thread to spare: develop here rather than show stale settings.
                Err(_) => self.render(ctx),
            }
        }
        #[cfg(target_arch = "wasm32")]
        self.render(ctx);
    }

    /// Shows the worker's frame once it is done. `Err` when the develop failed; the preview keeps
    /// the last frame then.
    fn poll_render(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let Some(rx) = &self.pending else { return Ok(()) };
        let frame = match rx.try_recv() {
            Ok(frame) => frame,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("Camera Raw preview worker stopped".to_string()),
        };
        self.pending = None;
        self.publish(ctx, frame?);
        Ok(())
    }

    pub(crate) fn render_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// Everything one develop of the proxy reads, owned, so it can run off the UI thread.
struct ProxyJob {
    proxy: std::sync::Arc<Vec<[f32; 4]>>,
    selection: Option<std::sync::Arc<Vec<f32>>>,
    raw_curve: Option<std::sync::Arc<Vec<[f32; 2]>>>,
    params: CameraRaw,
    float: bool,
    size: (usize, usize),
    full_w: usize,
    display: std::sync::Arc<photocraft_engine::display_color::CanvasDisplay>,
}

/// A developed proxy, ready to show.
struct ProxyFrame {
    px: Vec<[f32; 4]>,
    histogram: RgbHistogram,
    histogram_ms: f64,
    image: egui::ColorImage,
    display: std::sync::Arc<photocraft_engine::display_color::CanvasDisplay>,
    ms: f64,
}

impl ProxyJob {
    fn run(self) -> ProxyFrame {
        let t0 = crate::gpu_canvas::now_ms();
        let (pw, ph) = self.size;
        let mut px = self.proxy.to_vec();
        let mut p = self.params;
        p.pixel_scale = (pw as f32 / self.full_w.max(1) as f32).min(1.0);
        if let Some(curve) = &self.raw_curve {
            let tone = raw_stage(&p);
            photocraft_io::raw::retone(&mut px, pw, ph, curve, tone);
            (p.shadows, p.highlights, p.clarity, p.texture, p.dehaze) = (0.0, 0.0, 0.0, 0.0, 0.0);
        }
        photocraft_algo::camera_raw::develop(&mut px, pw, ph, &p, self.float);
        if let Some(mask) = &self.selection {
            blend_preview_selection(&mut px, &self.proxy, mask);
        }
        let histogram_start = crate::gpu_canvas::now_ms();
        let histogram = RgbHistogram::from_rgba(&px);
        let histogram_ms = crate::gpu_canvas::now_ms() - histogram_start;
        let image = CameraRawDialog::image(&px, pw, ph, &self.display);
        ProxyFrame { px, histogram, histogram_ms, image, display: self.display, ms: crate::gpu_canvas::now_ms() - t0 }
    }
}

/// Opens the dialog on the active layer.
/// Where OK writes: a new filter on the layer, or an existing smart filter (by index).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    NewFilter,
    SmartFilter(usize),
    /// An open-time RAW preview (File › Open): Open accepts every file in the
    /// original picker selection, Cancel closes all unaccepted RAWs. See [`RawOpen`].
    OpenRaw,
}

/// A raw file opened interactively, shown in Camera Raw before it is kept (as Photoshop opens
/// raws in Adobe Camera Raw first). The document is already developed with the defaults to
/// supply its preview, but remains hidden from the main editor until accepted. Open
/// re-develops it when Temperature, Tint, Exposure, Shadows, Highlights, Clarity, Texture or
/// Dehaze changed (those act on the sensor data, see [`photocraft_io::raw::import_raw_tuned`])
/// and applies the other settings as one Camera Raw step, then marks the document unmodified.
/// A batch is committed sequentially, not by launching overlapping jobs. Cancel discards the
/// whole unaccepted batch.
/// A photograph's identity for the whole Camera Raw session (GRW-1 lab cycle 2, B0). Unlike its `DocId` it survives
/// Open's re-develop (A1 -> A2), so the Filmstrip selection, the anchor and keyboard focus follow the photograph, not
/// the document. Allocated once when a RAW is queued for review, from a process-wide counter (never reused), and never
/// given to the engine or the control channel, which keep naming documents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PhotoId(pub u64);

impl PhotoId {
    pub(crate) fn fresh() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        PhotoId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// A photograph's star rating as far as PhotoCraft knows it (GRW-1 lab, Filmstrip addendum of 15 Oct: shown, never
/// written). `Unknown` (not read: no sidecar, an unreadable one, or a file supplied without a path, whose rating may live
/// inside the RAW itself) is never shown as unrated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PhotoRating {
    #[default]
    Unknown,
    Unrated,
    Stars(u8),
    Rejected,
}

impl PhotoRating {
    /// For `ui.inspect` and the control channel: `"unknown"`, `"unrated"`, `"rejected"` or the star count.
    fn json(self) -> Value {
        match self {
            PhotoRating::Unknown => json!("unknown"),
            PhotoRating::Unrated => json!("unrated"),
            PhotoRating::Stars(n) => json!(n),
            PhotoRating::Rejected => json!("rejected"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RawOpen {
    pub document: photocraft_doc::DocId,
    /// The photograph's session identity ([`PhotoId`]); carried across a re-develop by `..raw.clone()`.
    pub(crate) photo: PhotoId,
    /// Its star rating, read once from the same-stem sidecar when it was queued (never written).
    pub(crate) rating: PhotoRating,
    pub name: String,
    pub path: Option<String>,
    /// The file, when it was opened from bytes (no path to read it again from).
    pub bytes: Option<std::sync::Arc<Vec<u8>>>,
}

/// An inactive, still-unaccepted RAW review. Keep per-photo edit and view state here;
/// the real decoded document and thumbnail textures remain owned by PhotoCraft.
#[derive(Clone)]
struct ParkedRawReview {
    raw: RawOpen,
    params: CameraRaw,
    original: CameraRaw,
    scope: crate::state::CameraRawScopeState,
    navigation: crate::state::CameraRawPreviewState,
    show_before: bool,
    /// Open's commit was cancelled or failed: this review comes back before any other, so the photographer decides.
    recover: bool,
}

#[derive(Default)]
pub struct FilmstripState {
    pub(crate) admission: crate::camera_raw_admission::AdmissionState,
    #[cfg(not(target_arch = "wasm32"))]
    persistence: Option<persistence_ui::Host>,
    #[cfg(not(target_arch = "wasm32"))]
    last_persistence_result: Option<Value>,
    #[cfg(not(target_arch = "wasm32"))]
    jpeg: jpeg_ui::JpegState,
    fullscreen: bool,
    manually_hidden: std::collections::BTreeSet<PhotoId>,
    group_undo: Vec<edit_actions::GroupChange>,
    group_redo: Vec<edit_actions::GroupChange>,
    group_set: Option<(String, f32, Vec<PhotoId>)>,
    recovery_checked: bool,
    parked: std::collections::VecDeque<ParkedRawReview>,
    /// Open Images commits every RAW originally selected in the file picker.
    /// Only one development job can run at a time; continue on later UI frames.
    batch_open_all: bool,
    batch_total: usize,
    batch_completed: usize,
    /// Show/hide, orientation, extent and annotation preferences: presentation only, never photo data.
    view: filmstrip_ui::FilmstripView,
    /// Which photos are selected (the active photo is tracked by the dialog, not here).
    selection: selection_ui::Selection,
    /// What the last frame drew (strip, splitter, toggle, cards).
    frame: filmstrip_ui::Frame,
    /// Consecutive failures to build the review of a photograph (see `reopen_failed`).
    reopen_failures: Vec<(photocraft_doc::DocId, u8)>,
    /// F2 (lab cycle 5): thumbnails made for the cards' own size, at most one per photograph, and the one being made.
    thumbs: Vec<CardThumb>,
    thumb_job: Option<ThumbJob>,
    /// A thumbnail that could not be made is not asked for again in this session.
    thumb_failed: Vec<ThumbKey>,
    /// What the Filmstrip filter hid this frame (F4 lab F1).
    hidden: filmstrip_ui::Hidden,
}

/// What a card thumbnail shows (F2): the photograph, the document it was made from (a re-develop makes another), that
/// document's content, the display colour transform it was converted with and the size it was made for. A thumbnail is
/// shown only for the photograph, document, content and display it was made for; a different size is shown until its
/// replacement arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThumbKey {
    photo: PhotoId,
    document: photocraft_doc::DocId,
    content: u64,
    display: u64,
    side: u32,
}

impl ThumbKey {
    fn shows(&self, other: &ThumbKey) -> bool {
        (self.photo, self.document, self.content, self.display) == (other.photo, other.document, other.content, other.display)
    }
}

struct CardThumb {
    key: ThumbKey,
    texture: TextureHandle,
}

struct ThumbJob {
    key: ThumbKey,
    #[cfg(not(target_arch = "wasm32"))]
    result: std::sync::mpsc::Receiver<Option<egui::ColorImage>>,
}

/// The document's composite area-averaged to `side` (its longest edge), as the main compositor reduces it for every
/// other thumbnail, converted for the display the way the preview is (unmanaged only when no display transform exists).
fn card_thumb_image(doc: &photocraft_doc::Document, side: u32, display: Option<&photocraft_engine::display_color::CanvasDisplay>) -> egui::ColorImage {
    let reduced = photocraft_compose::thumbnail_buffer(doc, side);
    let pixels = display.map_or_else(|| reduced.to_rgba8(), |display| display.to_rgba8(&reduced));
    egui::ColorImage::from_rgba_unmultiplied([pixels.width as usize, pixels.height as usize], &pixels.pixels)
}

type ThumbWork = (ThumbKey, std::sync::Arc<photocraft_doc::Document>, Option<std::sync::Arc<photocraft_engine::display_color::CanvasDisplay>>);

/// The finished thumbnail, if any: kept only when its photograph still has that document and content (a stale result
/// is dropped, never shown in another card).
fn receive_card_thumb(app: &mut PhotocraftApp, ctx: &egui::Context, current: &[ThumbKey]) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(job) = &app.raw_filmstrip.thumb_job else { return };
        let image = match job.result.try_recv() {
            Ok(image) => image,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        let key = job.key;
        app.raw_filmstrip.thumb_job = None;
        store_card_thumb(app, ctx, key, image, current);
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (app, ctx, current);
}

fn store_card_thumb(app: &mut PhotocraftApp, ctx: &egui::Context, key: ThumbKey, image: Option<egui::ColorImage>, current: &[ThumbKey]) {
    let Some(image) = image else {
        app.raw_filmstrip.thumb_failed.push(key);
        return;
    };
    if !current.iter().any(|now| now.shows(&key)) {
        return;
    }
    let texture = ctx.load_texture(format!("cr-card-thumb-{}", key.photo.0), image, egui::TextureOptions::LINEAR);
    app.raw_filmstrip.thumbs.retain(|thumb| thumb.key.photo != key.photo);
    app.raw_filmstrip.thumbs.push(CardThumb { key, texture });
}

/// Start making the first wanted thumbnail when none is being made: on a worker thread (the frame never waits), or
/// inline on the web, one per frame.
fn start_card_thumb(app: &mut PhotocraftApp, ctx: &egui::Context, wanted: Option<ThumbWork>, current: &[ThumbKey]) {
    let Some((key, doc, display)) = wanted else { return };
    if app.raw_filmstrip.thumb_job.is_some() {
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = current;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let repaint = ctx.clone();
        let spawned = std::thread::Builder::new().name("camera-raw-card-thumb".into()).spawn(move || {
            let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| card_thumb_image(&doc, key.side, display.as_deref()))).ok();
            let _ = tx.send(image);
            repaint.request_repaint();
        });
        match spawned {
            Ok(_) => app.raw_filmstrip.thumb_job = Some(ThumbJob { key, result: rx }),
            Err(_) => app.raw_filmstrip.thumb_failed.push(key),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let image = card_thumb_image(&doc, key.side, display.as_deref());
        store_card_thumb(app, ctx, key, Some(image), current);
        ctx.request_repaint();
    }
}

/// Return only genuine pending RAW imports, in document order, using stable DocIds.
/// Never expose unrelated editor documents as Camera Raw requests.
fn filmstrip_raws(app: &PhotocraftApp) -> Vec<RawOpen> {
    let mut raws = Vec::new();
    if let Some(raw) = app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()) {
        raws.push(raw.clone());
    }
    raws.extend(app.pending_raw_open.iter().cloned());
    raws.extend(app.queued_raw_opens.iter().cloned());
    raws.extend(app.raw_filmstrip.parked.iter().map(|item| item.raw.clone()));
    raws.retain(|raw| document_index(app, raw.document).is_some());
    raws.sort_by_key(|raw| {
        (crate::camera_raw_admission::order_for_doc(app, raw.document).unwrap_or(u64::MAX), document_index(app, raw.document).unwrap_or(usize::MAX))
    });
    raws.dedup_by_key(|raw| raw.document);
    raws
}

/// Every decoded RAW that Open or Cancel has not yet decided, by stable `DocId`: the open review, the
/// pending, queued and parked ones, and a commit still replacing or developing its document.
/// It does not depend on which tab is active, on the Filmstrip being shown, or on a profile prompt
/// covering the dialog; the control channel uses it to keep these documents out of reach.
pub(crate) fn unaccepted_documents(app: &PhotocraftApp) -> Vec<photocraft_doc::DocId> {
    let mut held: Vec<_> = filmstrip_raws(app).iter().map(|raw| raw.document).collect();
    if let Some(commit) = app.raw_redevelop.as_ref() {
        held.push(commit.raw.document);
        held.extend(app.session.job(commit.job).and_then(|job| job.document));
    }
    held.retain(|id| document_index(app, *id).is_some());
    held.sort_by_key(|id| id.0);
    held.dedup();
    held
}

/// Could any document be unaccepted? The same places [`unaccepted_documents`] looks, without building the list.
fn raw_in_progress(app: &PhotocraftApp) -> bool {
    app.camera_raw.as_ref().is_some_and(|d| d.raw_open.is_some())
        || app.pending_raw_open.is_some()
        || !app.queued_raw_opens.is_empty()
        || !app.raw_filmstrip.parked.is_empty()
        || app.raw_redevelop.is_some()
}

/// The one Camera Raw operation on a held document that [`guard_native_command`] lets through: this command, on
/// this document, once. It is set only by [`run_owned`] and taken (used or not) by the next guarded call, so it can
/// never authorise another command, another document or a second call.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawExemption {
    document: photocraft_doc::DocId,
    command: &'static str,
}

/// Run `command` for Camera Raw on its own `document` (the one it is opening), under a [`RawExemption`] for exactly that.
fn run_owned(app: &mut PhotocraftApp, document: photocraft_doc::DocId, command: &'static str, params: Value) -> Result<Value, String> {
    // The shell exemption is not engine authority. Hold a nonserializable, one-dispatch grant
    // across app.run so direct Session callers cannot imitate this commit by command name.
    let _engine_scope = app.session.begin_camera_raw_command(document).map_err(|e| e.to_string())?;
    app.raw_exemption = Some(RawExemption { document, command });
    let result = app.run(command, params);
    app.raw_exemption = None;
    result
}

/// The command guard for everything that reaches [`PhotocraftApp::run`]: the palette, a dialog opened earlier, a floating
/// window, a tool, a control request. While a RAW waits for Open or Cancel, a command may not act on it (as the active
/// tab) or name it, whoever sent it and whenever it runs: the decision is the control channel's own
/// ([`crate::control::guard_command`]) applied where the command executes.
pub(crate) fn guard_native_command(app: &mut PhotocraftApp, id: &str, params: &Value) -> Result<(), String> {
    let exemption = app.raw_exemption.take();
    // Nothing is queued, open or committing: there is no RAW to hold (the common case, and the hot one).
    if !raw_in_progress(app) {
        return Ok(());
    }
    // The engine command `jobs.cancel` reaches Camera Raw's commit like the control method of the same name, and it
    // passes the document check below when another document is active: judge the job it names.
    if id == "jobs.cancel" {
        let protected = commit_jobs(app);
        let named = params.get("job").and_then(Value::as_u64);
        if !protected.is_empty() && named.is_none_or(|job| protected.contains(&photocraft_engine::jobs::JobId(job))) {
            return Err("refused: that would cancel Camera Raw's own commit of a RAW; stop it with Esc or Cancel in the dialog (an unaccepted RAW is waiting in Camera Raw; Open or Cancel it first)".into());
        }
    }
    let held = unaccepted_documents(app);
    if held.is_empty() {
        return Ok(());
    }
    if let Some(owned) = exemption
        && owned.command == id
        && app.session.active().is_some_and(|st| st.doc.id == owned.document)
    {
        return Ok(());
    }
    crate::control::guard_command(app, &held, id, params)
}

/// Tell the session which documents are provisional (it pauses Script Events while one is open). Called wherever the
/// set can change, and every frame; a RAW enters it in [`queue_open_dialog`], before any hook can run on it.
///
/// While the RAWs wait and the editor is blocked it also closes the command palette. The palette runs shell commands
/// (Save As, Export) that never pass [`PhotocraftApp::run`], and nothing can open it again until the review is over:
/// the shortcut is blocked, the menu is gated and the control channel refuses its command.
pub(crate) fn sync_provisional(app: &mut PhotocraftApp) {
    let held = unaccepted_documents(app);
    if !held.is_empty() && blocks_editor(app) {
        app.ui.palette_open = false;
    }
    // Export As exports whichever tab is active when it is confirmed, through the exporter and the writer and not
    // through a command: one that was opened before the RAW arrived must not be left to export it.
    if app.session.active().is_some_and(|st| held.contains(&st.doc.id)) {
        let stale: Vec<u64> = app.ui.dialogs.iter().filter(|d| d.fields.contains_key("__export")).map(|d| d.id).collect();
        for id in stale {
            let _ = crate::dialogs::cancel(app, id);
        }
    }
    // A RAW that is still decoding or developing can already be a document (its job is applied before the shell
    // handles the event) without being in `held`: scripts stay paused until its event has been handled.
    let pending = app.jobs.opens.iter().any(|tab| is_raw_name(&tab.name)) || app.raw_redevelop.is_some();
    app.session.set_provisional_pending(pending);
    app.session.set_provisional_documents(held);
    // The review session is over (nothing waits, develops or is shown): the next one starts from scratch.
    #[cfg(not(target_arch = "wasm32"))]
    let retained_session = persistence_ui::retains_session(app);
    #[cfg(target_arch = "wasm32")]
    let retained_session = false;
    if !raw_in_progress(app) && !retained_session {
        app.raw_filmstrip.selection.clear();
        app.raw_filmstrip.thumbs.clear();
        // The filter is for this review only (the sort stays): the next review never starts with photos hidden.
        app.raw_filmstrip.view.filter = Default::default();
        app.raw_filmstrip.hidden = Default::default();
        app.raw_filmstrip.thumb_job = None;
        app.raw_filmstrip.thumb_failed.clear();
    }
}

/// The active document is a RAW waiting for Open or Cancel: nothing may write it to a file or bind it to one, whichever
/// route got here (the palette's Save As, a file dialog answered later, a recorded save). Checked where the app writes
/// the active document ([`PhotocraftApp::write_document`], [`PhotocraftApp::save_automation_with`]) before anything is
/// encoded or written; the writers themselves are not touched.
pub(crate) fn refuse_to_write_unaccepted(app: &PhotocraftApp) -> Result<(), String> {
    match app.session.active() {
        Some(st) if unaccepted_documents(app).contains(&st.doc.id) => {
            Err(format!("refused: the active document is an unaccepted RAW and is not saved or bound to a file ({UNACCEPTED_RAW})"))
        }
        _ => Ok(()),
    }
}

/// The refusal text of the command guards; "unaccepted RAW" is what callers and tests look for.
const UNACCEPTED_RAW: &str = "an unaccepted RAW is waiting in Camera Raw; Open or Cancel it first";

/// The background jobs that belong to Camera Raw's own commit: the re-develop or filter step it started, and any job
/// locking a document it still holds. The control channel may not cancel these (Esc and Cancel in the dialog remain).
pub(crate) fn commit_jobs(app: &PhotocraftApp) -> Vec<photocraft_engine::jobs::JobId> {
    let mut jobs: Vec<_> = app.raw_redevelop.iter().map(|commit| commit.job).collect();
    jobs.extend(unaccepted_documents(app).into_iter().filter_map(|id| app.session.job_on(id)).map(|job| job.id));
    jobs.sort_by_key(|job| job.0);
    jobs.dedup();
    jobs
}

/// May the control channel's `ui` requests drive the open dialog? A RAW review always; a Camera Raw Filter only
/// while a RAW that landed after it opened is not the active tab, because its commit runs on the active tab.
pub(crate) fn dialog_is_drivable(app: &PhotocraftApp) -> bool {
    let Some(dialog) = app.camera_raw.as_ref() else {
        #[cfg(not(target_arch = "wasm32"))]
        return app.raw_filmstrip.persistence.is_some();
        #[cfg(target_arch = "wasm32")]
        return false;
    };
    dialog.target == Target::OpenRaw || !app.session.active().is_some_and(|st| unaccepted_documents(app).contains(&st.doc.id))
}

/// Read-only status survives a completed Done, so a lost transport response can
/// be inspected before any caller considers replaying a write.
pub(crate) fn persistence_snapshot(app: &PhotocraftApp) -> Value {
    #[cfg(not(target_arch = "wasm32"))]
    return persistence_ui::describe(app);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        json!({"available":false})
    }
}

/// Expose the same stable RAW request IDs to the authorised Camera Raw control channel.
/// This is informational only: selecting an ID is a separate validated action.
fn describe_active_raw(app: &PhotocraftApp) -> Value {
    let mut description = app.camera_raw.as_ref().map(|d| d.describe(&app.ui.camera_raw_scope, &app.ui.camera_raw_preview)).unwrap_or(Value::Null);
    if let Some(active) = app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()) {
        description["filmstrip"] = json!(
            filmstrip_raws(app)
                .iter()
                .map(|raw| json!({
                    "document": raw.document.0,
                    "name": raw.name,
                    "active": raw.document == active.document,
                    "selected": app.raw_filmstrip.selection.contains(raw.photo),
                    "rating": raw.rating.json()
                }))
                .collect::<Vec<_>>()
        );
        let raws = filmstrip_raws(app);
        let selection = &app.raw_filmstrip.selection;
        // Photographs are named by their current document: a photo no longer in the strip is named by nothing.
        let document_of = |photo: PhotoId| raws.iter().find(|raw| raw.photo == photo).map(|raw| raw.document.0);
        description["selection"] = json!({
            "active": active.document.0,
            "selected": raws.iter().filter(|raw| selection.contains(raw.photo)).map(|raw| raw.document.0).collect::<Vec<_>>(),
            "count": selection.len(),
            "anchor": selection.anchor().and_then(document_of),
            "focus": selection.focus().and_then(document_of),
        });
        let (view, frame) = (&app.raw_filmstrip.view, &app.raw_filmstrip.frame);
        let rect = |r: ERect| json!([r.min.x, r.min.y, r.max.x, r.max.y]);
        let orientation = |o: filmstrip_ui::Orientation| if o == filmstrip_ui::Orientation::Vertical { "vertical" } else { "horizontal" };
        description["filmstripView"] = json!({
            "visible": view.visible,
            "orientation": orientation(view.orientation),
            "preferredWidth": view.preferred_width,
            "preferredHeight": view.preferred_height,
            "effectiveHeight": frame.layout.strip.map_or(0.0, |strip| strip.height()),
            "menu": frame.menu.iter().map(|(o, r)| json!({"orientation": orientation(*o), "rect": rect(*r)})).collect::<Vec<_>>(),
            "effectiveWidth": frame.layout.strip.map_or(0.0, |strip| strip.width()),
            "showFilenames": view.show_filenames,
            "showEditBadges": view.show_edit_badges,
            "sort": {"by": view.sort.code(), "reverse": view.reverse},
            "filter": {"rated": view.filter.rated, "unrated": view.filter.unrated, "rejected": view.filter.rejected, "unread": view.filter.unread},
            "hidden": app.raw_filmstrip.hidden.count,
            "hiddenSelected": app.raw_filmstrip.hidden.selected.len(),
            "stripRect": frame.layout.strip.map(rect),
            "splitterRect": frame.layout.splitter.map(rect),
            "toggleRect": frame.toggle.map(rect),
            "toolbarRect": rect(frame.toolbar),
            "cards": frame.cards.iter().map(|(document, card)| json!({"document": document.0, "rect": rect(*card)})).collect::<Vec<_>>(),
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    if description.is_object() {
        description["persistence"] = persistence_ui::describe(app);
    }
    description
}

/// Switch the live Camera Raw preview without committing either photo. The new dialog is
/// fully built before removing it from its original queue; failure restores the old one.
fn select_raw_review(app: &mut PhotocraftApp, ctx: &egui::Context, document: photocraft_doc::DocId) -> Result<(), String> {
    if app.raw_redevelop.is_some() {
        return Err("Camera Raw cannot switch photos while a develop job is running".into());
    }
    let old = app.camera_raw.as_ref().ok_or("Camera Raw is not open")?;
    if old.target != Target::OpenRaw {
        return Err("Filmstrip selection is only available while opening RAW photos".into());
    }
    let old_raw = old.raw_open.as_ref().ok_or("the current RAW has no request")?;
    if old_raw.document == document {
        return Ok(());
    }
    let next = app
        .pending_raw_open
        .as_ref()
        .filter(|raw| raw.document == document)
        .cloned()
        .or_else(|| app.queued_raw_opens.iter().find(|raw| raw.document == document).cloned())
        .or_else(|| app.raw_filmstrip.parked.iter().find(|item| item.raw.document == document).map(|item| item.raw.clone()))
        .ok_or("the selected RAW is not queued for review")?;
    if document_index(app, document).is_none() {
        return Err("the selected RAW document was closed".into());
    }
    let previous_scope = app.ui.camera_raw_scope.clone();
    let previous_navigation = app.ui.camera_raw_preview;
    let old_dialog = app.camera_raw.take().ok_or("Camera Raw is not open")?;
    let parked = ParkedRawReview {
        raw: old_dialog.raw_open.as_ref().ok_or("the current RAW has no request")?.clone(),
        params: old_dialog.params.clone(),
        original: old_dialog.original.clone(),
        scope: previous_scope.clone(),
        navigation: previous_navigation,
        show_before: old_dialog.show_before,
        recover: false,
    };
    if let Err(error) = open_raw(app, ctx, next) {
        app.camera_raw = Some(old_dialog);
        app.ui.camera_raw_scope = previous_scope;
        app.ui.camera_raw_preview = previous_navigation;
        if let Some(index) = document_index(app, parked.raw.document) {
            app.session.set_active(index);
        }
        return Err(error);
    }
    if app.pending_raw_open.as_ref().is_some_and(|raw| raw.document == document) {
        app.pending_raw_open = None;
    } else if let Some(index) = app.queued_raw_opens.iter().position(|raw| raw.document == document) {
        app.queued_raw_opens.remove(index);
    } else if let Some(index) = app.raw_filmstrip.parked.iter().position(|item| item.raw.document == document)
        && let Some(saved) = app.raw_filmstrip.parked.remove(index)
    {
        restore_raw_settings(app, ctx, &saved);
    }
    app.raw_filmstrip.parked.push_back(parked);
    ctx.request_repaint();
    Ok(())
}

/// Restore only the selected request's edits. Never reuse another photo's preview texture
/// or settings. `open_raw` has recreated the proxy from the selected session document.
fn restore_raw_settings(app: &mut PhotocraftApp, ctx: &egui::Context, saved: &ParkedRawReview) {
    if let Some(dialog) = app.camera_raw.as_mut() {
        let encoding_gamma = dialog.params.encoding_gamma;
        let pixel_scale = dialog.params.pixel_scale;
        dialog.params = saved.params.clone();
        dialog.original = saved.original.clone();
        dialog.params.encoding_gamma = encoding_gamma;
        dialog.params.pixel_scale = pixel_scale;
        dialog.original.encoding_gamma = encoding_gamma;
        dialog.original.pixel_scale = pixel_scale;
        dialog.show_before = saved.show_before;
        dialog.dirty = true;
        dialog.render(ctx);
    }
    app.ui.camera_raw_scope = saved.scope.clone();
    app.ui.camera_raw_preview = saved.navigation;
}

/// The native RAW opener may inspect a same-stem XMP sidecar, but never modifies it.
/// Web/byte-only opens do not have a trusted adjacent path and keep their existing defaults.
#[cfg(not(target_arch = "wasm32"))]
fn adjacent_raw_xmp(path: Option<&str>) -> Result<Option<photocraft_io::raw_xmp::XmpImport>, String> {
    let Some((sidecar, bytes)) = adjacent_xmp_bytes(path)? else { return Ok(None) };
    let imported = photocraft_io::raw_xmp::import_adobe_raw_settings(&bytes).map_err(|e| format!("{}: {e}", sidecar.display()))?;
    Ok(Some(imported))
}

/// The image area of a preview column (16 pt margins; the toolbar row is under it).
fn preview_view(preview_body: ERect) -> ERect {
    ERect::from_min_max(
        preview_body.min + vec2(16.0, 16.0),
        pos2((preview_body.right() - 16.0).max(preview_body.left() + 17.0), (preview_body.bottom() - 48.0).max(preview_body.top() + 17.0)),
    )
}

/// The preview toolbar's row under a preview column's image.
fn toolbar_row(preview_body: ERect) -> ERect {
    ERect::from_min_max(pos2(preview_body.left() + 16.0, preview_view(preview_body).bottom() + 8.0), preview_body.right_bottom() - vec2(16.0, 4.0))
}

/// The photograph's rating from its same-stem sidecar (`xmp:Rating`), read and never written. Without a path (a file
/// supplied as bytes, e.g. by automation) or a sidecar nothing is read and the rating is unknown. Known limitation (Q4
/// P1): two RAWs of one stem with different extensions share one sidecar, so both show its rating.
fn sidecar_rating(path: Option<&str>) -> PhotoRating {
    match adjacent_xmp_bytes(path) {
        Ok(Some((_, bytes))) => match photocraft_io::raw_xmp::read_rating(&bytes) {
            Ok(None) => PhotoRating::Unrated,
            Ok(Some(-1)) => PhotoRating::Rejected,
            Ok(Some(stars @ 1..=5)) => PhotoRating::Stars(stars.unsigned_abs()),
            _ => PhotoRating::Unknown,
        },
        _ => PhotoRating::Unknown,
    }
}

/// The bytes of the same-stem `.xmp` beside `path`, read and never written: `None` without a path or a sidecar. Only a
/// regular file is read (a directory, device or FIFO named like a sidecar is no sidecar, and a FIFO would block the
/// read), and the read is capped at the parser's limit.
#[cfg(not(target_arch = "wasm32"))]
fn adjacent_xmp_bytes(path: Option<&str>) -> Result<Option<(std::path::PathBuf, Vec<u8>)>, String> {
    use std::io::Read;

    let Some(path) = path else { return Ok(None) };
    let raw = std::path::Path::new(path);
    // The sidecar Done would write, by the shared ownership rule: a JPEG or TIFF beside a RAW
    // never shows the RAW's settings. An ambiguous pair shows none here; admission names it.
    let candidates = match photocraft_io::raw_session::commit::sidecar_target(raw) {
        Ok(bound) => vec![bound],
        // Bytes opened without the file on disk (a drop or a test): the conventional names.
        // Both spellings support case-sensitive Linux directories; Windows resolves either.
        Err(_) if !raw.exists() => vec![raw.with_extension("xmp"), raw.with_extension("XMP")],
        Err(_) => return Ok(None),
    };
    for sidecar in candidates {
        match std::fs::metadata(&sidecar) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", sidecar.display())),
        }
        let file = match std::fs::File::open(&sidecar) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", sidecar.display())),
        };
        // A size check alone would race file changes; cap the actual read as well.
        let mut bytes = Vec::new();
        file.take(photocraft_io::raw_xmp::MAX_XMP_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| format!("{}: {e}", sidecar.display()))?;
        return Ok(Some((sidecar, bytes)));
    }
    Ok(None)
}

#[cfg(target_arch = "wasm32")]
fn adjacent_raw_xmp(_path: Option<&str>) -> Result<Option<photocraft_io::raw_xmp::XmpImport>, String> {
    Ok(None)
}

#[cfg(target_arch = "wasm32")]
fn adjacent_xmp_bytes(_path: Option<&str>) -> Result<Option<(std::path::PathBuf, Vec<u8>)>, String> {
    Ok(None)
}

/// Should an interactive open of `name` show the open-time Camera Raw dialog? `warnings` are
/// the import's: only a raw developed from its sensor data qualifies (not a preview fallback).
pub fn wants_open_dialog(app: &PhotocraftApp, warnings: &[String]) -> bool {
    warnings.iter().any(|w| w == photocraft_engine::jobs::CAMERA_RAW_REVIEW_NOTE)
        || app.session.prefs().raw_defaults.open_in_camera_raw && warnings.iter().any(|w| w.contains(photocraft_io::raw::DEVELOPED_NOTE))
}

/// `warnings` without the "developed with default settings" note: the open-time dialog shows
/// how the raw is developed.
pub fn without_develop_note(warnings: &[String]) -> Vec<String> {
    warnings.iter().filter(|w| !w.contains(photocraft_io::raw::DEVELOPED_NOTE) && *w != photocraft_engine::jobs::CAMERA_RAW_REVIEW_NOTE).cloned().collect()
}

/// True while a Camera Raw dialog or queued open must block the main editing UI.
/// Open-time pending state matters because a job can finish during this frame.
pub(crate) fn blocks_editor(app: &PhotocraftApp) -> bool {
    loading(app) || app.camera_raw.is_some()
        || app.pending_raw_open.is_some()
        || !app.queued_raw_opens.is_empty()
        || !app.raw_filmstrip.parked.is_empty()
        || app.raw_filmstrip.batch_open_all
        // Open's commit job: the dialog is gone and nothing is queued, but the RAW is not accepted until it lands.
        || app.raw_redevelop.is_some()
}

/// Don't reveal a decoded but unaccepted RAW as an editable document behind Camera Raw.
pub(crate) fn hides_unaccepted_raw(app: &PhotocraftApp) -> bool {
    loading(app)
        || app.pending_raw_open.is_some()
        || !app.queued_raw_opens.is_empty()
        || !app.raw_filmstrip.parked.is_empty()
        || app.raw_filmstrip.batch_open_all
        || app.camera_raw.as_ref().is_some_and(|d| d.target == Target::OpenRaw)
}

/// Queues Camera Raw for the document that *actually completed its import*.
/// Never infer ownership from the active tab: another decode may finish first.
/// Missing documents are errors rather than silently queuing a different photo.
pub fn queue_open_dialog(app: &mut PhotocraftApp, document: photocraft_doc::DocId, name: &str, path: Option<&str>, bytes: Option<&[u8]>) -> Result<(), String> {
    if document_index(app, document).is_none() {
        return Err(format!("Cannot queue Camera Raw: imported document {} is missing", document.0));
    }
    if filmstrip_raws(app).iter().any(|raw| raw.document == document) {
        return Err(format!("Cannot queue Camera Raw: document {} already has a RAW review", document.0));
    }
    // Control input queued before this RAW arrived must not be delivered to what is on screen once it is the active tab:
    // the guard judged those requests when no RAW waited.
    app.synthetic.clear();
    let raw = RawOpen {
        document,
        photo: PhotoId::fresh(),
        rating: sidecar_rating(path),
        name: name.to_string(),
        path: path.map(str::to_string),
        bytes: bytes.map(|b| std::sync::Arc::new(b.to_vec())),
    };
    #[cfg(not(target_arch = "wasm32"))]
    persistence_ui::queue(app, &raw);
    if app.camera_raw.is_none() && app.pending_raw_open.is_none() && app.queued_raw_opens.is_empty() {
        app.pending_raw_open = Some(raw);
    } else {
        app.queued_raw_opens.push_back(raw);
    }
    // Immediately: the shell fires "Open Document" next, and it must find this document provisional.
    sync_provisional(app);
    Ok(())
}

/// Start the next RAW only after the prior one has completed its background Open step.
/// `raw_redevelop` is currently a single job slot; overlapping commits would lose events.
fn take_next_raw_open(app: &mut PhotocraftApp) -> Option<RawOpen> {
    if app.camera_raw.is_some() || app.raw_redevelop.is_some() {
        return None;
    }
    app.pending_raw_open.take().or_else(|| app.queued_raw_opens.pop_front())
}

/// Opens the queued open-time dialog on its document's (only) layer.
fn open_raw(app: &mut PhotocraftApp, ctx: &egui::Context, raw: RawOpen) -> Result<(), String> {
    let index = document_index(app, raw.document).ok_or("the raw document was closed")?;
    app.session.set_active(index);
    let (layer, surf, _) = crate::distort_ui::active_pixels(app)?;
    let sidecar = adjacent_raw_xmp(raw.path.as_deref());
    let params = match &sidecar {
        Ok(Some(imported)) => imported.params.clone(),
        _ => CameraRaw::default(),
    };
    #[cfg(not(target_arch = "wasm32"))]
    let params = persistence_ui::recipe(app, raw.photo).map_or(params, |recipe| recipe.adjustments.clone());
    #[cfg(not(target_arch = "wasm32"))]
    let original = persistence_ui::original(app, raw.photo).cloned();
    open_pixels(app, ctx, layer, surf, params, Coverage::None, Target::OpenRaw)?;
    let curve = match (&raw.bytes, &raw.path) {
        (Some(b), _) => Some(photocraft_io::raw::tone_curve(b)),
        #[cfg(not(target_arch = "wasm32"))]
        (None, Some(p)) => std::fs::read(p).ok().map(|b| photocraft_io::raw::tone_curve(&b)),
        _ => None,
    };
    if let Some(d) = app.camera_raw.as_mut() {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(mut original) = original {
            original.encoding_gamma = d.params.encoding_gamma;
            original.pixel_scale = d.params.pixel_scale;
            d.original = original;
        }
        d.layer_name = raw.name.clone();
        d.raw_curve = curve.map(std::sync::Arc::new);
        d.raw_open = Some(raw);
    }
    // The parent editor may have painted the just-decoded image on the frame in
    // which the worker completed. The Camera Raw child viewport does not reliably
    // cause its parent to redraw; explicitly clear that stale canvas once.
    ctx.request_repaint_of(egui::ViewportId::ROOT);
    match sidecar {
        Ok(Some(imported)) if !imported.applied.is_empty() => {
            app.ui.status = format!(
                "Camera Raw: imported {} Adobe XMP tonal settings (read-only); {} other Camera Raw fields are not imported",
                imported.applied.len(),
                imported.unsupported_count,
            );
            app.ui.status_error = false;
        }
        Err(error) => {
            app.ui.status = format!("Camera Raw: skipped XMP ({error}); using default settings");
            app.ui.status_error = false;
        }
        _ => {}
    }
    Ok(())
}

fn document_index(app: &PhotocraftApp, id: photocraft_doc::DocId) -> Option<usize> {
    app.session.documents().iter().position(|st| st.doc.id == id)
}

/// The non-default settings of `p` as engine params.
fn non_defaults(p: &CameraRaw) -> Value {
    let full = serde_json::to_value(p).unwrap_or(json!({}));
    let def = serde_json::to_value(CameraRaw::default()).unwrap_or(json!({}));
    let mut out = serde_json::Map::new();
    if let (Value::Object(f), Value::Object(d)) = (full, def) {
        for (k, v) in f {
            if k != "pixelScale" && d.get(&k) != Some(&v) {
                out.insert(k, v);
            }
        }
    }
    Value::Object(out)
}

/// The job id of an open-time re-develop (not an engine command: the shell starts it).
pub const REDEVELOP_JOB: &str = "cameraRaw.redevelop";

/// An open-time re-develop running in the background: what to finish with when it lands.
#[derive(Clone, Debug)]
pub struct Redevelop {
    pub job: photocraft_engine::jobs::JobId,
    params: CameraRaw,
    raw: RawOpen,
    /// The job is the final Camera Raw step (`filter.cameraRaw`), not the re-develop: when it
    /// lands the document is marked unmodified.
    filter_step: bool,
}

/// The sliders a raw open applies while developing, before the tone curve (as Camera Raw does).
fn raw_stage(p: &CameraRaw) -> photocraft_io::raw::Tone {
    photocraft_io::raw::Tone { shadows: p.shadows, highlights: p.highlights, clarity: p.clarity, texture: p.texture, dehaze: p.dehaze }
}

/// Open: re-develop for raw-stage settings, apply the rest, keep the document unmodified.
///
/// Re-developing reads and decodes the whole raw again, so in the desktop app (background jobs
/// on) it runs as a job like the open itself: the reply is `{"job", "pending": true}`, the
/// document is locked meanwhile, and [`on_redevelop_event`] finishes when it lands. Elsewhere
/// (tests, the web) it runs inline.
fn commit_open_raw(app: &mut PhotocraftApp, params: &CameraRaw, raw: &RawOpen) -> Result<Value, String> {
    #[cfg(not(target_arch = "wasm32"))]
    let native = persistence_ui::recipe(app, raw.photo).cloned().zip(persistence_ui::binding(app, raw.photo));
    #[cfg(target_arch = "wasm32")]
    let native: Option<(photocraft_io::raw_session::NativeRecipe, photocraft_io::raw_session::SourceBinding)> = None;
    let complete_recipe = native.is_some();
    let index = document_index(app, raw.document).ok_or("the raw document was closed")?;
    let tuning = photocraft_io::raw::RawTuning {
        temperature: params.temperature,
        tint: params.tint,
        exposure: params.exposure,
        shadows: params.shadows,
        highlights: params.highlights,
        clarity: params.clarity,
        texture: params.texture,
        dehaze: params.dehaze,
    };
    // Without the file the raw-stage settings stay RGB adjustments, like the filter's.
    let source = match (&raw.bytes, &raw.path) {
        (Some(b), _) => Some(photocraft_engine::jobs::OpenSource::Bytes(b.clone())),
        #[cfg(not(target_arch = "wasm32"))]
        (None, Some(p)) => Some(photocraft_engine::jobs::OpenSource::Path(p.clone())),
        _ => None,
    };
    let Some(source) = source.filter(|_| native.is_some() || tuning != photocraft_io::raw::RawTuning::default()) else {
        return finish_open_raw(app, index, params.clone(), None, Some(raw));
    };
    app.session.set_active(index);
    let (name, doc_id) = (raw.name.clone(), raw.document);
    let work = move |ctx: &photocraft_engine::jobs::JobCtx| -> photocraft_engine::Result<(photocraft_io::ImportResult, bool)> {
        ctx.progress(0.0, "Reading");
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((recipe, binding)) = &native {
            let bytes =
                photocraft_io::raw_session::controller::read_verified_source(binding).map_err(|e| photocraft_engine::EngineError::Other(e.to_string()))?;
            ctx.check()?;
            return photocraft_io::raw_session::controller::render_recipe(&name, &bytes, recipe)
                .map(|result| (result, true))
                .map_err(|e| photocraft_engine::EngineError::Other(e.to_string()));
        }
        let bytes = match source {
            photocraft_engine::jobs::OpenSource::Bytes(b) => b,
            #[cfg(not(target_arch = "wasm32"))]
            photocraft_engine::jobs::OpenSource::Path(p) => {
                std::sync::Arc::new(std::fs::read(&p).map_err(|e| photocraft_engine::EngineError::Other(format!("{p}: {e}")))?)
            }
            #[cfg(target_arch = "wasm32")]
            photocraft_engine::jobs::OpenSource::Path(p) => return Err(photocraft_engine::EngineError::Other(format!("{p}: cannot read files here"))),
        };
        ctx.check()?;
        ctx.progress(0.05, "Developing");
        photocraft_io::raw::import_raw_tuned(&name, &bytes, &tuning).map_err(|e| photocraft_engine::EngineError::Other(e.to_string()))
    };
    let generation = app.session.review_generation();
    let apply = move |s: &mut photocraft_engine::Session, (r, wb): (photocraft_io::ImportResult, bool)| {
        let mut result = replace_raw_document(s, doc_id, r, wb, generation)?;
        result["completeRecipe"] = json!(complete_recipe);
        Ok(result)
    };
    let label = crate::i18n::fmt(tl!("Developing {name}"), &[("name", &raw.name)]);
    let started = if app.background_jobs {
        app.session.start_job(REDEVELOP_JOB, json!({"name": raw.name}), &label, true, work, apply).map_err(|e| e.to_string())?
    } else {
        let v = work(&photocraft_engine::jobs::JobCtx::new()).and_then(|t| apply(&mut app.session, t)).map_err(|e| e.to_string())?;
        photocraft_engine::jobs::Started::Done(v)
    };
    match started {
        photocraft_engine::jobs::Started::Done(v) => finish_redevelop(app, params, raw, &v),
        photocraft_engine::jobs::Started::Job(job) => {
            app.raw_redevelop = Some(Redevelop { job, params: params.clone(), raw: raw.clone(), filter_step: false });
            app.ui.status = label;
            app.ui.status_error = false;
            Ok(json!({"job": job.0, "pending": true}))
        }
    }
}

/// Swap the default-developed document `doc_id` for the re-developed one, at the same tab
/// position and with the same path. Returns `{document, docId, previous, warnings, wb}`; `document` is the tab
/// index at apply time only, `docId` the re-developed document's identity.
fn replace_raw_document(
    s: &mut photocraft_engine::Session,
    doc_id: photocraft_doc::DocId,
    r: photocraft_io::ImportResult,
    wb: bool,
    generation: u64,
) -> photocraft_engine::Result<Value> {
    // A result of a superseded review (the review was cancelled meanwhile) replaces nothing.
    if s.review_generation() != generation {
        return Err(photocraft_engine::EngineError::Cancelled);
    }
    let index =
        s.documents().iter().position(|st| st.doc.id == doc_id).ok_or_else(|| photocraft_engine::EngineError::Other("the raw document was closed".into()))?;
    let path = s.documents().get(index).and_then(|st| st.path.clone());
    s.close(index);
    let (opened, _) = s.open_document(r.document, path);
    let index = match s.execute("document.move", json!({"document": opened, "to": index})) {
        Ok(moved) => moved.get("document").and_then(Value::as_u64).map_or(opened, |i| i as usize),
        Err(_) => opened,
    };
    s.set_active(index);
    // The re-developed document is provisional from the moment it exists: the shell handles this job's event only
    // after every job that landed in the same poll, and the old id (now closed) no longer declares anything.
    let replacement = s.documents().get(index).map(|st| st.doc.id);
    let mut declared = s.provisional_documents();
    declared.extend(replacement);
    s.set_provisional_documents(declared);
    Ok(json!({"document": index, "docId": replacement.map(|id| id.0), "previous": doc_id.0, "warnings": r.warnings, "wb": wb}))
}

/// After the re-develop: keep the old document's view (zoom, scroll) on the new one, report
/// the import's notes, then apply the remaining settings.
fn finish_redevelop(app: &mut PhotocraftApp, params: &CameraRaw, raw: &RawOpen, v: &Value) -> Result<Value, String> {
    // By identity, not by the apply-time tab index: an earlier event of the same poll (a save's close) can have moved
    // the tabs, and the index would then name another document.
    let replacement = v.get("docId").and_then(Value::as_u64).map(photocraft_doc::DocId).ok_or("the re-develop returned no document")?;
    if let Some(request) = crate::camera_raw_admission::request_for_doc(app, raw.document) {
        crate::camera_raw_admission::bind_restored(app, replacement, &request.key);
    }
    let index = document_index(app, replacement).ok_or("the re-developed document was closed")?;
    for id in app.view_docs.iter_mut().filter(|id| **id == raw.document) {
        *id = replacement;
    }
    let warnings: Vec<String> =
        v.get("warnings").and_then(Value::as_array).map(|a| a.iter().filter_map(|w| w.as_str().map(str::to_string)).collect()).unwrap_or_default();
    crate::notices::io_warnings(app, &crate::i18n::fmt(tl!("Opened {name}"), &[("name", &raw.name)]), &without_develop_note(&warnings));
    if v.get("completeRecipe").and_then(Value::as_bool) == Some(true) {
        return finish_open_raw(app, index, CameraRaw::default(), Some(true), Some(raw));
    }
    let mut rest = params.clone();
    rest.exposure = 0.0;
    rest.shadows = 0.0;
    rest.highlights = 0.0;
    rest.clarity = 0.0;
    rest.texture = 0.0;
    rest.dehaze = 0.0;
    if v.get("wb").and_then(Value::as_bool) == Some(true) {
        rest.temperature = 0.0;
        rest.tint = 0.0;
    }
    let result = finish_open_raw(app, index, rest.clone(), Some(true), Some(raw));
    if result.is_err() {
        // The re-develop had landed (a new document id) and the Camera Raw step failed: the photograph is neither
        // accepted nor lost. It goes back to Camera Raw with the settings still to apply (nothing if it was closed).
        recover_unfinished(app, RawOpen { document: replacement, ..raw.clone() }, rest);
    }
    result
}

/// The other settings as one Camera Raw step on document `index`, then the document is marked
/// unmodified (developing is part of opening). The step runs like any filter (in the
/// background in the desktop app); the document is marked unmodified when it lands.
fn finish_open_raw(app: &mut PhotocraftApp, index: usize, rest: CameraRaw, redeveloped: Option<bool>, raw: Option<&RawOpen>) -> Result<Value, String> {
    app.session.set_active(index);
    app.sync_views();
    let mut result = json!({"document": index, "redeveloped": redeveloped.unwrap_or(false)});
    if !rest.is_identity() {
        let layer = app.session.active().and_then(|st| st.active_layer).ok_or("no layer to develop")?;
        let mut p = non_defaults(&rest);
        p["layer"] = json!(layer.0);
        let document = app.session.active().map(|st| st.doc.id).ok_or("no document")?;
        let r = run_owned(app, document, "filter.cameraRaw", p)?;
        if r.get("pending").and_then(Value::as_bool) == Some(true)
            && let (Some(job), Some(raw)) = (r.get("job").and_then(Value::as_u64), raw)
        {
            app.raw_redevelop = Some(Redevelop { job: photocraft_engine::jobs::JobId(job), params: rest.clone(), raw: raw.clone(), filter_step: true });
        }
        result["filter"] = r;
    }
    mark_unmodified(app, index);
    #[cfg(not(target_arch = "wasm32"))]
    if app.raw_redevelop.is_none()
        && let Some(raw) = raw
    {
        persistence_ui::transferred(app, raw.photo);
    }
    Ok(result)
}

fn mark_unmodified(app: &mut PhotocraftApp, index: usize) {
    let active = app.session.active_index();
    app.session.set_active(index);
    if let Some(st) = app.session.active_mut() {
        st.saved_revision = st.revision;
    }
    if let Some(i) = active {
        app.session.set_active(i);
    }
    app.sync_views();
}

/// A finished background re-develop ([`REDEVELOP_JOB`]) or open-time Camera Raw step. `true`
/// when the event was one of those (handled here).
pub fn on_redevelop_event(app: &mut PhotocraftApp, e: &photocraft_engine::jobs::JobEvent) -> bool {
    let handled = on_redevelop_event_inner(app, e);
    sync_provisional(app);
    handled
}

fn on_redevelop_event_inner(app: &mut PhotocraftApp, e: &photocraft_engine::jobs::JobEvent) -> bool {
    let Some(pending) = app.raw_redevelop.take_if(|r| r.job == e.id) else { return false };
    if pending.filter_step {
        let success = matches!(e.outcome, photocraft_engine::jobs::JobOutcome::Done(_));
        if success && let Some(index) = e.document.and_then(|id| document_index(app, id)) {
            mark_unmodified(app, index);
        }
        if app.raw_filmstrip.batch_open_all {
            if success {
                app.raw_filmstrip.batch_completed += 1;
            } else {
                // Do not silently proceed after a rejected Camera Raw filter step.
                app.raw_filmstrip.batch_open_all = false;
                app.ui.status = format!("Camera Raw: Open Images stopped while processing {}", pending.raw.name);
                app.ui.status_error = true;
            }
        }
        if !success {
            // The re-developed document (a new id) is the one left behind; the rest of the settings come back with it.
            if let Some(document) = e.document {
                recover_unfinished(app, RawOpen { document, ..pending.raw.clone() }, pending.params.clone());
            }
        }
        // The regular filter reporting still handles its own job notification.
        return false;
    }
    match &e.outcome {
        photocraft_engine::jobs::JobOutcome::Done(v) => match finish_redevelop(app, &pending.params, &pending.raw, v) {
            Ok(_) => {
                if app.raw_filmstrip.batch_open_all && app.raw_redevelop.is_none() {
                    // There was no second asynchronous Camera Raw filter job.
                    app.raw_filmstrip.batch_completed += 1;
                }
                if !app.raw_filmstrip.batch_open_all {
                    app.ui.status = crate::i18n::fmt(tl!("Opened {name}"), &[("name", &pending.raw.name)]);
                    app.ui.status_error = false;
                }
            }
            Err(err) => {
                app.raw_filmstrip.batch_open_all = false;
                crate::notices::error(app, format!("{}: {err}", e.label));
            }
        },
        photocraft_engine::jobs::JobOutcome::Failed(err) => {
            app.raw_filmstrip.batch_open_all = false;
            // Neither Open nor Cancel was chosen: the photograph goes back to Camera Raw with its adjustments,
            // and the remaining RAWs stay pending for individual review.
            recover_unfinished(app, pending.raw.clone(), pending.params.clone());
            app.sync_views();
            crate::notices::error(app, format!("{}: {err}", e.label));
        }
        photocraft_engine::jobs::JobOutcome::Cancelled => {
            app.raw_filmstrip.batch_open_all = false;
            recover_unfinished(app, pending.raw.clone(), pending.params.clone());
            app.ui.status = format!("Open Images stopped: {} is back in Camera Raw and has not been opened", pending.raw.name);
            app.ui.status_error = true;
        }
    }
    true
}

/// Open's commit did not finish (cancelled with Esc, or failed): the photograph must not become an ordinary editor
/// document by default. It is parked as a recovered review that opens next, with the adjustments the photographer
/// had chosen (for the second phase, the ones still to apply), and stays unaccepted until Open or Cancel is chosen.
/// Nothing else is opened or accepted on its behalf.
fn recover_unfinished(app: &mut PhotocraftApp, raw: RawOpen, params: CameraRaw) {
    if document_index(app, raw.document).is_none() {
        return;
    }
    let original = adjacent_raw_xmp(raw.path.as_deref()).ok().flatten().map(|imported| imported.params).unwrap_or_default();
    let review = ParkedRawReview {
        raw,
        params,
        original,
        scope: app.ui.camera_raw_scope.clone(),
        navigation: app.ui.camera_raw_preview,
        show_before: false,
        recover: true,
    };
    app.raw_filmstrip.parked.push_front(review);
}

/// The raw file extensions the importer recognises (`photocraft_raw::EXTENSIONS`, which this crate
/// only links in tests; a unit test keeps the two lists identical).
const RAW_EXTENSIONS: [&str; 12] = ["dng", "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "pef", "orf", "rw2", "raf"];

fn is_raw_name(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| RAW_EXTENSIONS.iter().any(|raw| ext.eq_ignore_ascii_case(raw)))
}

/// Cancel the Camera Raw review session, not merely the current thumbnail.
fn cancel(app: &mut PhotocraftApp) {
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::request_close(app) {
        return;
    }
    cancel_review(app);
    sync_provisional(app);
    #[cfg(not(target_arch = "wasm32"))]
    persistence_ui::end_sessionless_visit(app);
}

fn cancel_review(app: &mut PhotocraftApp) {
    let dialog = app.camera_raw.take();
    if dialog.as_ref().is_some_and(|d| d.target != Target::OpenRaw) {
        return; // Normal Camera Raw Filter: dismiss without modifying its document.
    }
    // Every open and re-develop started for this review is superseded: none may be applied or kept after Cancel.
    app.session.next_review_generation();
    app.raw_filmstrip.reopen_failures.clear();
    // Photos of this review that are still decoding must not land after Cancel and open a new
    // review (the decode's own completion queues the dialog). Other opens are left alone.
    let decoding: Vec<_> = app.jobs.opens.iter().filter(|tab| is_raw_name(&tab.name)).map(|tab| tab.job).collect();
    for job in decoding {
        crate::jobs_ui::cancel(app, job);
    }
    // Camera Raw is one modal review session for the whole file-picker selection.
    // Cancel (including Escape / OS-window close) discards every unaccepted RAW,
    // not one photo at a time. Accepted/unrelated editor tabs are untouched.
    let pending = filmstrip_raws_with_active(app, dialog.as_ref().and_then(|d| d.raw_open.as_ref()));
    app.pending_raw_open = None;
    app.queued_raw_opens.clear();
    app.raw_filmstrip.parked.clear();
    app.raw_filmstrip.selection.clear();
    app.raw_filmstrip.group_undo.clear();
    app.raw_filmstrip.group_redo.clear();
    app.raw_filmstrip.group_set = None;
    app.raw_filmstrip.manually_hidden.clear();
    app.raw_filmstrip.fullscreen = false;
    // The last frame's rectangles belong to this session; the next one must not read them.
    app.raw_filmstrip.frame = Default::default();
    app.raw_filmstrip.batch_open_all = false;
    app.raw_filmstrip.batch_total = 0;
    app.raw_filmstrip.batch_completed = 0;
    let count = pending.len();
    for raw in pending {
        crate::prefs_ui::dismiss_mismatch_for_document(app, raw.document);
        if let Some(index) = document_index(app, raw.document) {
            app.session.close(index);
        }
    }
    app.sync_views();
    app.ui.status = format!("Cancelled Camera Raw review ({} photos)", count);
    app.ui.status_error = false;
}

/// Include the active RAW after taking its dialog out of the app. A regular
/// filmstrip query would otherwise omit it and accidentally leave that file open.
fn filmstrip_raws_with_active(app: &PhotocraftApp, active: Option<&RawOpen>) -> Vec<RawOpen> {
    let mut raws = filmstrip_raws(app);
    if let Some(raw) = active
        && document_index(app, raw.document).is_some()
        && !raws.iter().any(|r| r.document == raw.document)
    {
        raws.push(raw.clone());
    }
    raws
}

/// What limits the preview: the document selection (a new filter applies through it, as the
/// engine does), a smart object's filter mask, or nothing.
enum Coverage {
    Selection,
    Mask(photocraft_doc::LayerMask),
    None,
}

pub fn open(app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
    photocraft_engine::commands::find("filter.cameraRaw").map(|c| (c.enabled)(&app.session)).unwrap_or(Err("unknown command".into()))?;
    let (layer, surf, _) = crate::distort_ui::active_pixels(app)?;
    open_pixels(app, ctx, layer, surf, CameraRaw::default(), Coverage::Selection, Target::NewFilter)
}

/// Opens Camera Raw on smart filter `index` of smart layer `layer`, with its stored settings and
/// the pixels below it; OK updates that filter (`layer.smartFilter.setParams`), as double-clicking
/// a Camera Raw smart filter does in Photoshop.
pub fn open_smart_filter(app: &mut PhotocraftApp, ctx: &egui::Context, layer: LayerId, index: usize) -> Result<(), String> {
    use photocraft_engine::lens_cmds::{RAW, raw_params};
    let st = app.session.active().ok_or("no document")?;
    let Some(photocraft_doc::LayerContent::Smart(sm)) = st.doc.layer(layer).map(|l| &l.content) else {
        return Err("the layer is not a smart object".into());
    };
    let filter = sm.smart_filters.get(index).ok_or("no such smart filter")?;
    if filter.command != RAW {
        return Err("the smart filter is not a Camera Raw filter".into());
    }
    let params = raw_params(RAW, &filter.params).map_err(|e| e.to_string())?;
    let surf = photocraft_engine::smart_cmds::render_below_filter(&st.doc, sm, index)
        .map_err(|e| e.to_string())?
        .ok_or("the smart object's contents are unavailable (missing linked file?)")?;
    let coverage = sm.filter_mask.clone().filter(|m| m.enabled).map_or(Coverage::None, Coverage::Mask);
    open_pixels(app, ctx, layer, surf, params, coverage, Target::SmartFilter(index))
}

fn open_pixels(
    app: &mut PhotocraftApp,
    ctx: &egui::Context,
    layer: LayerId,
    surf: photocraft_raster::Surface,
    params: CameraRaw,
    coverage: Coverage,
    target: Target,
) -> Result<(), String> {
    let st = app.session.active().ok_or("no document")?;
    let canvas = st.doc.bounds();
    let name = st.doc.layer(layer).map(|l| l.name.clone()).unwrap_or_default();
    let mut params = params;
    if params.encoding_gamma == 0.0 {
        params.encoding_gamma = photocraft_engine::lens_cmds::encoding_gamma(&st.doc);
    }
    // Use the engine filter domain, including pixels outside the canvas: spatial Camera Raw
    // stages (vignette, grain, local contrast) must agree with the committed result.
    let area = canvas.union(&surf.content_bounds());
    let width = area.x1.checked_sub(area.x0).filter(|n| (1..=1_048_576).contains(n));
    let height = area.y1.checked_sub(area.y0).filter(|n| (1..=1_048_576).contains(n));
    let (Some(w), Some(h)) = (width, height) else {
        return Err("Camera Raw preview bounds are too large".into());
    };
    let (w, h) = (w as usize, h as usize);
    if w.checked_mul(h).is_none_or(|pixels| pixels > 512_000_000) {
        return Err("Camera Raw preview bounds are too large".into());
    }
    let k = w.max(h).div_ceil(PROXY_SIDE).max(1);
    let (pw, ph) = (w.div_ceil(k), h.div_ceil(k));
    let mut proxy = vec![[0.0f32; 4]; pw * ph];
    let mut row = vec![[0.0f32; 4]; w];
    for py in 0..ph {
        // Box-average k×k blocks.
        let mut acc = vec![[0.0f32; 5]; pw];
        for dy in 0..k {
            let y = area.y0 + (py * k + dy).min(h - 1) as i32;
            surf.read_rgba_into(Rect::new(area.x0, y, area.x1, y + 1), &mut row);
            for (x, q) in row.iter().enumerate() {
                let a = &mut acc[x / k];
                for c in 0..4 {
                    a[c] += q[c];
                }
                a[4] += 1.0;
            }
        }
        for (x, a) in acc.iter().enumerate() {
            proxy[py * pw + x] = [a[0] / a[4], a[1] / a[4], a[2] / a[4], a[3] / a[4]];
        }
    }
    // Coverage at the centre of each proxy block.
    let sample = |value: &dyn Fn(i32, i32) -> f32| {
        (0..ph)
            .flat_map(|y| (0..pw).map(move |x| (area.x0 + (x * k + k / 2).min(w - 1) as i32, area.y0 + (y * k + k / 2).min(h - 1) as i32)))
            .map(|(x, y)| value(x, y).clamp(0.0, 1.0))
            .collect::<Vec<_>>()
    };
    let selection = match &coverage {
        Coverage::Selection => st.doc.selection.as_ref().map(|mask| sample(&|x, y| mask.sample_channel(x, y, 0))),
        Coverage::Mask(mask) => Some(sample(&|x, y| mask.value(x, y))),
        Coverage::None => None,
    };
    let source = photocraft_engine::color_cmds::composite_profile(&st.doc);
    let display = app.session.color.canvas_display(&st.doc).map_err(|e| e.to_string())?;
    let lab_transform = photocraft_cms::cached(&source, photocraft_cms::Builtin::LabD50.profile(), Default::default()).map_err(|e| e.to_string())?;
    let scope_transform = photocraft_cms::cached(&source, photocraft_cms::Builtin::Srgb.profile(), Default::default()).map_err(|e| e.to_string())?;
    if let Some(saved) = app.session.prefs().dialogs.get("filter.cameraRaw.scope")
        && saved.get("samplers").and_then(Value::as_array).is_none_or(|a| a.len() <= MAX_SAMPLERS)
        && let Ok(view) = serde_json::from_value(saved.clone())
    {
        app.ui.camera_raw_scope = view;
    }
    app.ui.camera_raw_preview = Default::default();
    app.ui.camera_raw_scope.samplers.clear();
    // Keep Camera Raw compact on open; vectorscope is an opt-in context-menu view.
    app.ui.camera_raw_scope.vectorscope = false;
    if selection.is_none() {
        app.ui.camera_raw_scope.selected_region = false;
    }
    let float = surf.format().sample == photocraft_color::SampleType::F32;
    let mut d = CameraRawDialog {
        layer,
        layer_name: name,
        original: params.clone(),
        params,
        target,
        before_histogram: RgbHistogram::from_rgba(&proxy),
        processed: Vec::new(),
        lab_transform,
        scope_transform,
        scope: Default::default(),
        after_histogram: RgbHistogram::default(),
        preview_revision: 0,
        histogram_ms: 0.0,
        proxy: std::sync::Arc::new(proxy),
        selection: selection.map(std::sync::Arc::new),
        pw,
        ph,
        full_w: w,
        full_h: h,
        preview_ppp: ctx.pixels_per_point(),
        detail: super::camera_raw_detail_ui::DetailPreview::new(
            surf,
            area,
            match coverage {
                Coverage::Selection => {
                    st.doc.selection.clone().map(super::camera_raw_detail_ui::Coverage::Selection).unwrap_or(super::camera_raw_detail_ui::Coverage::None)
                }
                Coverage::Mask(mask) => super::camera_raw_detail_ui::Coverage::Mask(mask),
                Coverage::None => super::camera_raw_detail_ui::Coverage::None,
            },
        ),
        display,
        viewport: None,
        float,
        tex: None,
        before_tex: None,
        before_display_key: None,
        dirty: true,
        show_before: false,
        mixer_tab: 1,
        curve_state: Default::default(),
        curve_rect: None,
        render_ms: 0.0,
        raw_open: None,
        raw_curve: None,
        pending: None,
        preview_error: None,
        undo: Vec::new(),
        redo: Vec::new(),
        edit_origin: None,
        preferences: Default::default(),
    };
    d.render(ctx);
    app.camera_raw = Some(d);
    Ok(())
}

/// Menu / control-channel entry point. `None` when the call isn't for this dialog.
pub fn menu(app: &mut PhotocraftApp, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if id != "filter.cameraRaw" {
        return None;
    }
    let Some(fields) = params.as_object() else {
        return Some(Err("Camera Raw params must be an object".into()));
    };
    if fields.is_empty() {
        return Some(open(app, ctx).map(|_| describe_active_raw(app)));
    }
    if let Some(sf) = params.get("smartFilter") {
        let opened = (|| {
            if fields.len() > 1 {
                return Err("smartFilter opens the dialog; send ui requests separately".to_string());
            }
            let layer = sf.get("layer").and_then(Value::as_u64).ok_or("smartFilter.layer must be a layer id")?;
            let index = sf.get("index").and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok()).ok_or("smartFilter.index must be a filter index")?;
            open_smart_filter(app, ctx, LayerId(layer), index)?;
            Ok(describe_active_raw(app))
        })();
        return Some(opened);
    }
    let ui = params.get("ui")?;
    let Some(fields) = ui.as_object() else {
        return Some(Err("Camera Raw ui must be an object".into()));
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(request) = ui.get("persistence") {
        if fields.len() != 1 {
            return Some(Err("Send a persistence action separately from edits".into()));
        }
        return Some(persistence_ui::control(app, request));
    }
    if let Some(request) = ui.get("group") {
        if fields.len() != 1 {
            return Some(Err("Send a group action separately from other edits".into()));
        }
        return Some(group_control(app, request).map(|()| describe_active_raw(app)));
    }
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::finishing(app) {
        return Some(Err("Finish decision is holding edits".into()));
    }
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::unready(app) && (fields.contains_key("set") || fields.contains_key("scope")) {
        return Some(Err("Wait for verified photograph admission before editing".into()));
    }
    for (key, value) in fields {
        match key.as_str() {
            "set" | "scope" | "view" | "filmstrip" => {}
            "selectRaw" if value.as_u64().is_some() => {}
            "selectRaw" => return Some(Err("Camera Raw selectRaw must be an unsigned document id".into())),
            "before" | "commit" | "cancel" | "saveSettings" | "loadSettings" | "undo" | "redo" if value.is_boolean() => {}
            "before" | "commit" | "cancel" | "saveSettings" | "loadSettings" | "undo" | "redo" => {
                return Some(Err(format!("Camera Raw {key} must be a boolean")));
            }
            _ => return Some(Err(format!("unknown Camera Raw ui property {key}"))),
        }
    }
    if ui.get("commit").and_then(Value::as_bool) == Some(true) && ui.get("cancel").and_then(Value::as_bool) == Some(true) {
        return Some(Err("Camera Raw cannot commit and cancel together".into()));
    }
    for (key, run) in [("saveSettings", save_settings as fn(&mut PhotocraftApp) -> Result<Value, String>), ("loadSettings", load_settings)] {
        if ui.get(key).and_then(Value::as_bool) == Some(true) {
            if fields.len() > 1 {
                return Some(Err(format!("Camera Raw {key} must be sent on its own")));
            }
            return Some(run(app));
        }
    }
    // With no Camera Raw review open, a RAW session (loading, the recovery chooser, an empty
    // filmstrip, the Open Images handoff) is driven only by persistence actions: a ui request
    // must never open a Camera Raw Filter over it and then report on the wrong dialog.
    #[cfg(not(target_arch = "wasm32"))]
    if app.camera_raw.is_none() && persistence_ui::raw_visit(app) {
        return Some(Err("No Camera Raw review is open; use a persistence action (for example cancel, resume or openOtherFiles)".into()));
    }
    let was_closed = app.camera_raw.is_none();
    let previous_view = app.ui.camera_raw_scope.clone();
    let previous_navigation = app.ui.camera_raw_preview;
    if was_closed && let Err(e) = open(app, ctx) {
        return Some(Err(e));
    }
    let result = menu_update(app, ctx, ui);
    if was_closed && result.as_ref().is_some_and(Result::is_err) {
        app.camera_raw = None;
        app.ui.camera_raw_scope = previous_view;
        app.ui.camera_raw_preview = previous_navigation;
    }
    result
}

fn menu_update(app: &mut PhotocraftApp, ctx: &egui::Context, ui: &Value) -> Option<Result<Value, String>> {
    if app.camera_raw.as_ref().is_some_and(|d| d.target == Target::OpenRaw)
        && (ui.get("undo").and_then(Value::as_bool) == Some(true) || ui.get("redo").and_then(Value::as_bool) == Some(true))
    {
        if ui.as_object().is_none_or(|fields| fields.len() != 1) {
            return Some(Err("Camera Raw history actions must be sent separately from edits".into()));
        }
        let action = if ui.get("redo").and_then(Value::as_bool) == Some(true) { "redo" } else { "undo" };
        if let Err(error) = group_control(app, &json!({"action":action})) {
            return Some(Err(error));
        }
        if let Some(dialog) = app.camera_raw.as_mut() {
            dialog.render(ctx);
        }
        return Some(Ok(describe_active_raw(app)));
    }
    if let Some(id) = ui.get("selectRaw").and_then(Value::as_u64) {
        if ui.as_object().is_none_or(|fields| fields.len() != 1) {
            return Some(Err("Camera Raw selectRaw cannot be combined with edits or commit".into()));
        }
        return Some(select_raw_review(app, ctx, photocraft_doc::DocId(id)).map(|()| describe_active_raw(app)));
    }
    // Stage both filter and presentation updates. No texture, preference or dialog mutation
    // occurs until every part of the request has passed validation.
    let d = app.camera_raw.as_ref()?;
    let mut next_params = d.params.clone();
    if let Some(set) = ui.get("set") {
        if !set.is_object() {
            return Some(Err("bad Camera Raw settings: expected an object".into()));
        }
        let mut cur = serde_json::to_value(&d.params).unwrap_or(json!({}));
        if let (Value::Object(c), Value::Object(s)) = (&mut cur, set) {
            for (k, v) in s {
                if !c.contains_key(k) {
                    return Some(Err(format!("unknown Camera Raw setting {k}")));
                }
                c.insert(k.clone(), v.clone());
            }
        }
        next_params = match serde_json::from_value::<CameraRaw>(cur) {
            Ok(p) => p,
            Err(e) => return Some(Err(format!("bad Camera Raw settings: {e}"))),
        };
        if let Err(e) = next_params.validate_changes(&d.original) {
            return Some(Err(format!("bad Camera Raw settings: {e}")));
        }
    }
    let group_change = if d.target == Target::OpenRaw
        && let Some(set) = ui.get("set")
    {
        let prepared = edit_actions::slider_group(app, &d.params, &d.params).and_then(|snapshot| edit_actions::set_fields_group(set, &snapshot.before));
        let change = match prepared {
            Ok(change) => change,
            Err(error) => return Some(Err(error)),
        };
        next_params = d
            .raw_open
            .as_ref()
            .and_then(|raw| change.after.iter().find(|(id, _)| *id == raw.photo))
            .map_or_else(|| d.params.clone(), |(_, params)| params.clone());
        Some(change)
    } else {
        None
    };
    let scope_update = match ui.get("scope") {
        Some(scope) => match super::camera_raw_scope_ui::prepare_control(&next_params, d.selection.is_some(), &app.ui.camera_raw_scope, scope) {
            Ok(update) => Some(update),
            Err(e) => return Some(Err(e)),
        },
        None => None,
    };
    let navigation = match ui.get("view") {
        Some(view) => match super::camera_raw_preview_ui::prepare(app.ui.camera_raw_preview, view) {
            Ok(next) => Some(next),
            Err(e) => return Some(Err(e)),
        },
        None => None,
    };
    let filmstrip = match ui.get("filmstrip") {
        Some(request) => match filmstrip_ui::prepare(app.raw_filmstrip.view, request) {
            Ok(next) => Some(next),
            Err(e) => return Some(Err(e)),
        },
        None => None,
    };
    if let Some(change) = group_change
        && let Err(error) = apply_group(app, change)
    {
        return Some(Err(error));
    }
    let d = app.camera_raw.as_mut()?;
    if let Some(view) = navigation {
        app.ui.camera_raw_preview = view;
    }
    if let Some(next) = filmstrip {
        app.raw_filmstrip.view = next;
    }
    if d.params.point_curve != next_params.point_curve {
        d.curve_state = Default::default();
    }
    if d.params != next_params {
        remember_edit(d);
        d.dirty = true;
        d.params = next_params;
    }
    if let Some(b) = ui.get("before").and_then(Value::as_bool) {
        d.show_before = b;
    }
    if let Some(update) = scope_update {
        update.apply(d, &mut app.ui.camera_raw_scope);
    }
    if ui.get("undo").and_then(Value::as_bool) == Some(true) {
        undo_edit(d);
    }
    if ui.get("redo").and_then(Value::as_bool) == Some(true) {
        redo_edit(d);
    }
    if d.dirty {
        d.render(ctx);
    }
    if app.ui.camera_raw_scope.vectorscope
        && let Some(d) = app.camera_raw.as_mut()
    {
        d.ensure_scope(app.ui.camera_raw_scope.selected_region);
    }
    super::camera_raw_scope_ui::persist(app, ctx);
    if ui.get("cancel").and_then(Value::as_bool) == Some(true) {
        cancel(app);
        // With unsaved edits Cancel first asks; without, the session's terminal discard is
        // still on its way. Report what is true now: poll describe() until it is closed.
        #[cfg(not(target_arch = "wasm32"))]
        if persistence_ui::cancelling(app) {
            return Some(Ok(json!({"cancelled": false, "confirmationRequired": true})));
        }
        #[cfg(not(target_arch = "wasm32"))]
        if persistence_ui::closing(app) {
            return Some(Ok(json!({"cancelled": false, "closing": true})));
        }
        return Some(Ok(json!({"cancelled": true})));
    }
    if ui.get("commit").and_then(Value::as_bool) == Some(true) {
        return Some(commit(app));
    }
    Some(Ok(describe_active_raw(app)))
}

/// Save Settings…: the dialog's settings as a Camera Raw `.xmp` settings file.
fn save_settings(app: &mut PhotocraftApp) -> Result<Value, String> {
    let d = app.camera_raw.as_ref().ok_or(tl!("Camera Raw isn't open"))?;
    let bytes = photocraft_io::camera_raw_xmp::save(&d.params).into_bytes();
    app.pick_save("Camera Raw Settings.xmp", move |app, path| {
        let write = app.services.write.as_mut().ok_or("no writer configured")?;
        write(&path, &bytes)?;
        Ok(json!({"path": path}))
    })
}

/// Load Settings…: applies a Camera Raw `.xmp` settings file's supported settings over the
/// dialog's; the status bar names how many were not supported.
fn load_settings(app: &mut PhotocraftApp) -> Result<Value, String> {
    app.camera_raw.as_ref().ok_or(tl!("Camera Raw isn't open"))?;
    app.pick_file_bytes_filtered(&["xmp"], |app, name, bytes| {
        let file = crate::file_open::display_name(&name);
        let d = app.camera_raw.as_ref().ok_or(tl!("Camera Raw isn't open"))?;
        let mut next = d.params.clone();
        let report = photocraft_io::camera_raw_xmp::load(&mut next, &bytes).map_err(|e| format!("{file}: {e}"))?;
        next.validate_changes(&d.original).map_err(|e| format!("{file}: {e}"))?;
        if d.target == Target::OpenRaw {
            let targets = edit_actions::slider_group(app, &d.params, &d.params)?.before;
            let change = edit_actions::load_settings_group(&bytes, &targets)?;
            apply_group(app, change)?;
        } else {
            let d = app.camera_raw.as_mut().ok_or(tl!("Camera Raw isn't open"))?;
            if d.params.point_curve != next.point_curve {
                d.curve_state = Default::default();
            }
            if d.params != next {
                remember_edit(d);
                d.dirty = true;
                d.params = next;
            }
        }
        app.ui.status_error = false;
        app.ui.status = if report.ignored.is_empty() {
            crate::i18n::fmt(tl!("Loaded Camera Raw settings from {file}"), &[("file", &file)])
        } else {
            let template = "Loaded Camera Raw settings from {file} ({n} not supported)";
            crate::i18n::fmt(tl!(template), &[("file", &file), ("n", &report.ignored.len().to_string())])
        };
        Ok(json!({"applied": report.applied, "ignored": report.ignored}))
    })
}

fn report_file_error(app: &mut PhotocraftApp, r: Result<Value, String>) {
    if let Err(e) = r
        && e != crate::file_dialog::CANCELLED
    {
        app.ui.status_error = true;
        app.ui.status = e;
    }
}

const RAW_UNDO_LIMIT: usize = 40;

fn remember_edit(d: &mut CameraRawDialog) {
    if d.target == Target::OpenRaw {
        return;
    }
    if d.undo.len() == RAW_UNDO_LIMIT {
        d.undo.remove(0);
    }
    d.undo.push(d.params.clone());
    d.redo.clear();
}

fn undo_edit(d: &mut CameraRawDialog) -> bool {
    let Some(prev) = d.undo.pop() else { return false };
    d.redo.push(d.params.clone());
    if d.params.point_curve != prev.point_curve {
        d.curve_state = Default::default();
    }
    d.params = prev;
    d.dirty = true;
    d.edit_origin = None;
    true
}

fn redo_edit(d: &mut CameraRawDialog) -> bool {
    let Some(next) = d.redo.pop() else { return false };
    d.undo.push(d.params.clone());
    if d.params.point_curve != next.point_curve {
        d.curve_state = Default::default();
    }
    d.params = next;
    d.dirty = true;
    d.edit_origin = None;
    true
}

fn history_keys(d: &mut CameraRawDialog, ctx: &egui::Context) {
    let cmd_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
    if ctx.input_mut(|i| i.consume_key(cmd_shift, egui::Key::Z)) {
        redo_edit(d);
    } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)) {
        undo_edit(d);
    }
}

fn commit(app: &mut PhotocraftApp) -> Result<Value, String> {
    let result = commit_dialog(app);
    sync_provisional(app);
    result
}

fn commit_dialog(app: &mut PhotocraftApp) -> Result<Value, String> {
    #[cfg(not(target_arch = "wasm32"))]
    if app.camera_raw.as_ref().is_some_and(|d| d.target == Target::OpenRaw) && !persistence_ui::before_open(app)? {
        return Ok(json!({"pending": true, "operation": "checkpoint-before-open"}));
    }
    let d = app.camera_raw.as_ref().ok_or(tl!("Camera Raw isn't open"))?;
    let total = if d.target == Target::OpenRaw { filmstrip_raws(app).len() } else { 0 };
    let start_batch = total > 1 && !app.raw_filmstrip.batch_open_all;
    // Do not race an Open Images click against RAWs still being decoded. Otherwise a
    // late import would spawn a second, unwanted Camera Raw review after batch Open.
    if d.target == Target::OpenRaw && !app.jobs.opens.is_empty() {
        return Err("Wait for the other selected RAW photos to finish importing before using Open Images".into());
    }
    let result = match d.target {
        Target::NewFilter => {
            let mut p = d.command_params();
            p["layer"] = json!(d.layer.0);
            app.run("filter.cameraRaw", p)?
        }
        Target::SmartFilter(index) => {
            d.params.validate_changes(&d.original)?;
            // Every setting, not just the non-defaults: setParams merges into the stored ones,
            // so a control reset to its default must overwrite the old value.
            let mut params = serde_json::to_value(&d.params).map_err(|e| e.to_string())?;
            if let Value::Object(m) = &mut params {
                m.remove("pixelScale");
            }
            app.run("layer.smartFilter.setParams", json!({"layer": d.layer.0, "index": index, "params": params}))?
        }
        Target::OpenRaw => {
            let raw = d.raw_open.clone().ok_or("no raw file is being opened")?;
            let params = d.params.clone();
            if start_batch {
                app.raw_filmstrip.batch_open_all = true;
                app.raw_filmstrip.batch_total = total;
                app.raw_filmstrip.batch_completed = 0;
            }
            match commit_open_raw(app, &params, &raw) {
                Ok(result) => {
                    if app.raw_filmstrip.batch_open_all && app.raw_redevelop.is_none() {
                        app.raw_filmstrip.batch_completed += 1;
                    }
                    result
                }
                Err(error) => {
                    app.raw_filmstrip.batch_open_all = false;
                    // The re-develop may already have replaced this document: that review is then recovered under its
                    // new id, and a dialog still pointing at the closed one would only fail on retry.
                    if app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).is_some_and(|raw| document_index(app, raw.document).is_none()) {
                        app.camera_raw = None;
                    }
                    return Err(error); // Otherwise leave the current review available for retry.
                }
            }
        }
    };
    app.camera_raw = None;
    // The selection is not cleared here: the review may go on (Open Images commits one photograph at a time, a
    // failed step brings the photograph back under its new document id). It ends with the session (`sync_provisional`).
    app.raw_filmstrip.frame = Default::default();
    if app.raw_filmstrip.batch_open_all {
        Ok(json!({"batch": app.raw_filmstrip.batch_total, "completed": app.raw_filmstrip.batch_completed, "pending": true, "first": result}))
    } else {
        Ok(result)
    }
}

fn temp_stops() -> Vec<Color32> {
    vec![Color32::from_rgb(70, 120, 230), Color32::from_rgb(200, 200, 200), Color32::from_rgb(235, 200, 60)]
}
fn tint_stops() -> Vec<Color32> {
    vec![Color32::from_rgb(70, 190, 80), Color32::from_rgb(200, 200, 200), Color32::from_rgb(210, 80, 200)]
}

/// One labelled slider; marks the dialog dirty when it moves.
fn row(ui: &mut egui::Ui, dirty: &mut bool, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, grad: Option<&[Color32]>) {
    let _ = slider(ui, dirty, label, v, range, grad);
}

fn slider(ui: &mut egui::Ui, dirty: &mut bool, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, grad: Option<&[Color32]>) -> egui::Response {
    let r = widgets::slider_row(ui, crate::i18n::tr_ctx(crate::i18n::current(), "cameraRaw", label), v, range, "", grad);
    if r.changed() {
        *dirty = true;
    }
    r
}

/// A Light tone slider; Alt-dragging it shows that zone's channel clipping on the preview.
fn tone_row(ui: &mut egui::Ui, dirty: &mut bool, alt: &mut Option<ToneZone>, zone: ToneZone, v: &mut f32) {
    let r = slider(ui, dirty, zone.name(), v, -zone.limit()..=zone.limit(), None);
    if r.dragged() && ui.input(|i| i.modifiers.alt) {
        *alt = Some(zone);
    }
}

// egui remembers ScrollArea offsets by Id. Giving every imported RAW a separate
// id prevents the next file in a batch from inheriting the previous file's scroll.
fn properties_scroll_salt(dialog: &CameraRawDialog) -> (&'static str, Option<u64>) {
    ("camera-raw-props", dialog.raw_open.as_ref().map(|raw| raw.document.0))
}

fn section(ui: &mut egui::Ui, title: &str, open: bool, body: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(egui::RichText::new(crate::i18n::tr_ctx(crate::i18n::current(), "cameraRaw", title)).font(FontId::proportional(13.0)))
        .id_salt(("camera-raw-section", title))
        .default_open(open)
        .show(ui, body);
}

fn wheel(ui: &mut egui::Ui, dirty: &mut bool, title: &str, w: &mut Wheel) {
    widgets::section_label(ui, crate::i18n::tr_ctx(crate::i18n::current(), "cameraRaw", title));
    let hs = widgets::hue_stops();
    row(ui, dirty, "Hue", &mut w.hue, 0.0..=360.0, Some(&hs));
    row(ui, dirty, "Saturation", &mut w.sat, 0.0..=100.0, None);
    row(ui, dirty, "Luminance", &mut w.lum, -100.0..=100.0, None);
}

// The shared Curve editor must never produce a curve `CameraRaw::validate` rejects.
const _: () = assert!(crate::point_curve::MAX_POINTS == photocraft_algo::camera_raw::MAX_CURVE_POINTS && crate::point_curve::MIN_GAP >= 1.0);

/// Small point-curve editor (master channel): click to add, drag to move, right-click to delete.
fn curve_editor(ui: &mut egui::Ui, p: &mut CameraRaw, dirty: &mut bool, state: &mut crate::state::PointCurveState) -> ERect {
    let t = Tokens::get(ui.ctx());
    let side = ui.available_width().min(260.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(side, side), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, t.radius_sm, t.field);
    for i in 1..4 {
        let f = i as f32 / 4.0;
        painter.line_segment([pos2(rect.left() + f * side, rect.top()), pos2(rect.left() + f * side, rect.bottom())], Stroke::new(1.0, t.separator));
        painter.line_segment([pos2(rect.left(), rect.top() + f * side), pos2(rect.right(), rect.top() + f * side)], Stroke::new(1.0, t.separator));
    }
    let to_screen = |x: f32, y: f32| pos2(rect.left() + x / 255.0 * side, rect.bottom() - y / 255.0 * side);
    // The identity handles are presentation until a gesture actually edits them.
    let mut points = if p.point_curve.is_empty() { vec![[0.0, 0.0], [255.0, 255.0]] } else { p.point_curve.clone() };
    if crate::point_curve::interact(ui, &resp, rect, &mut points, state).changed {
        p.point_curve = points.clone();
        *dirty = true;
    }
    // Curve (point curve after the parametric one).
    let lut = curve_lut(&points, 256);
    let pts: Vec<Pos2> = lut.iter().enumerate().map(|(i, value)| to_screen(i as f32, value * 255.0)).collect();
    painter.add(egui::Shape::line(pts, Stroke::new(1.5, t.text)));
    for (i, c) in points.iter().enumerate() {
        let center = to_screen(c[0], c[1]);
        if state.selected == Some(i) {
            painter.circle_filled(center, 3.0, t.accent);
        }
        painter.circle_stroke(center, 4.0, Stroke::new(1.5, t.accent));
    }
    rect
}

fn start_next_raw_review(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let recover_first = app.camera_raw.is_none() && app.raw_redevelop.is_none() && app.raw_filmstrip.parked.front().is_some_and(|item| item.recover);
    if !recover_first && let Some(raw) = take_next_raw_open(app) {
        match open_raw(app, ctx, raw.clone()) {
            Ok(()) => app.raw_filmstrip.reopen_failures.retain(|(id, _)| *id != raw.document),
            Err(error) => reopen_failed(app, raw, None, error),
        }
    } else if app.camera_raw.is_none()
        && app.raw_redevelop.is_none()
        && let Some(saved) = app.raw_filmstrip.parked.pop_front()
    {
        match open_raw(app, ctx, saved.raw.clone()) {
            Ok(()) => {
                app.raw_filmstrip.reopen_failures.retain(|(id, _)| *id != saved.raw.document);
                restore_raw_settings(app, ctx, &saved);
            }
            Err(error) => reopen_failed(app, saved.raw.clone(), Some(saved), error),
        }
    }
}

/// How many times in a row a photograph's review may fail to open before it is given up.
const REOPEN_ATTEMPTS: u8 = 3;

/// A photograph's Camera Raw review could not be built (the document is still there). It is never released as an
/// ordinary editable document: it stays held (parked, to be retried), and a photograph whose review keeps failing is
/// closed after [`REOPEN_ATTEMPTS`] tries with a notice, because nothing could ever show it or offer Open or Cancel.
/// The original file is untouched either way.
fn reopen_failed(app: &mut PhotocraftApp, raw: RawOpen, saved: Option<ParkedRawReview>, error: String) {
    let failures = match app.raw_filmstrip.reopen_failures.iter_mut().find(|(id, _)| *id == raw.document) {
        Some((_, count)) => {
            *count = count.saturating_add(1);
            *count
        }
        None => {
            app.raw_filmstrip.reopen_failures.push((raw.document, 1));
            1
        }
    };
    if failures >= REOPEN_ATTEMPTS {
        app.raw_filmstrip.reopen_failures.retain(|(id, _)| *id != raw.document);
        crate::prefs_ui::dismiss_mismatch_for_document(app, raw.document);
        if let Some(index) = document_index(app, raw.document) {
            app.session.close(index);
        }
        app.sync_views();
        crate::notices::error(app, format!("{}: {error}. It could not be shown in Camera Raw and was closed; the original file is unchanged.", raw.name));
        return;
    }
    if document_index(app, raw.document).is_some() {
        let review = saved.unwrap_or_else(|| ParkedRawReview {
            raw,
            params: CameraRaw::default(),
            original: CameraRaw::default(),
            scope: app.ui.camera_raw_scope.clone(),
            navigation: app.ui.camera_raw_preview,
            show_before: false,
            recover: true,
        });
        app.raw_filmstrip.parked.push_front(review);
    }
    app.ui.status = error;
    app.ui.status_error = true;
}

/// Advance at most one queued Open per frame. The existing raw_redevelop job slot
/// serializes expensive sensor development and Camera Raw filters; it must finish
/// before the next imported photo can begin. Never paint another Camera Raw window
/// for an already-accepted multi-photo Open Images request.
fn advance_batch_open(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if !app.raw_filmstrip.batch_open_all || app.raw_redevelop.is_some() {
        return;
    }
    let remaining = filmstrip_raws(app).len();
    if remaining == 0 {
        let total = app.raw_filmstrip.batch_total;
        let completed = app.raw_filmstrip.batch_completed;
        app.raw_filmstrip.batch_open_all = false;
        app.raw_filmstrip.batch_total = 0;
        app.raw_filmstrip.batch_completed = 0;
        app.ui.status = if completed == total {
            format!("Opened {total} Camera Raw photos")
        } else {
            format!("Open Images incomplete: only {completed} of {total} photos finished")
        };
        app.ui.status_error = completed != total;
        ctx.request_repaint_of(egui::ViewportId::ROOT);
        return;
    }
    start_next_raw_review(app, ctx);
    if let Some(first) = filmstrip_raws(app).first().map(|r| r.document)
        && let Err(error) = select_raw_review(app, ctx, first)
    {
        app.raw_filmstrip.batch_open_all = false;
        app.ui.status = error;
        app.ui.status_error = true;
        return;
    }
    if app.camera_raw.is_none() {
        // An open or preview failed: do not drop the other pending reviews.
        app.raw_filmstrip.batch_open_all = false;
        app.ui.status_error = true;
        return;
    }
    if let Err(error) = commit(app) {
        app.raw_filmstrip.batch_open_all = false;
        app.ui.status = format!("Open Images stopped: {error}");
        app.ui.status_error = true;
    }
    ctx.request_repaint();
}

/// Imported photographs stay staged until the complete opening batch is ready.
pub(crate) fn loading(app: &PhotocraftApp) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::cancelling(app) {
        return false;
    }
    if !app.jobs.opens.is_empty() && (!crate::camera_raw_admission::snapshot(app).is_empty() || !filmstrip_raws(app).is_empty()) {
        return true;
    }
    if crate::open_prompt::waiting(app) && !crate::camera_raw_admission::snapshot(app).is_empty() {
        return true;
    }
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::loading(app) {
        return true;
    }
    false
}

fn loading_dialog(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let rows = crate::camera_raw_admission::snapshot(app);
    let total = rows.len();
    let ready = rows.iter().filter(|r| r.outcome != crate::camera_raw_admission::Outcome::Pending).count();
    let job = app.jobs.opens.first().and_then(|o| app.session.job(o.job));
    let mut cancel_loading = false;
    egui::Modal::new(egui::Id::new("camera-raw-loading")).show(ctx, |ui| {
        ui.set_width(320.0);
        ui.heading(tl!("Opening photographs"));
        if let Some(job) = &job {
            ui.label(&job.label);
        } else {
            ui.label(tl!("Preparing Camera Raw previews…"));
        }
        if let Some(job) = &job {
            let fraction = if total == 0 { job.progress } else { (ready as f32 + job.progress) / total as f32 };
            ui.add(egui::ProgressBar::new(fraction.clamp(0.0, 1.0)).animate(true));
            if total > 0 {
                ui.label(format!("{ready} / {total}"));
            }
        } else {
            ui.spinner();
        }
        cancel_loading = widgets::secondary_button(ui, tl!("Cancel"), 90.0).clicked();
    });
    if cancel_loading || ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        let jobs: Vec<_> = app.jobs.opens.iter().map(|tab| tab.job).collect();
        for job in jobs {
            crate::jobs_ui::cancel(app, job);
        }
        #[cfg(not(target_arch = "wasm32"))]
        persistence_ui::cancel_loading(app);
        #[cfg(target_arch = "wasm32")]
        {
            cancel_review(app);
            sync_provisional(app);
        }
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(50));
}

pub fn show(app: &mut PhotocraftApp, ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    persistence_ui::poll(app, ctx);
    sync_provisional(app);
    if loading(app) {
        // The profile decision is a prerequisite for admission. It was drawn
        // by dialogs::show; covering it with another modal deadlocks opening.
        if !crate::open_prompt::waiting(app) {
            loading_dialog(app, ctx);
        }
        return;
    }
    if app.raw_filmstrip.batch_open_all {
        advance_batch_open(app, ctx);
        if app.raw_filmstrip.batch_open_all {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        }
    }
    start_next_raw_review(app, ctx);
    let Some(d) = app.camera_raw.as_ref() else {
        #[cfg(not(target_arch = "wasm32"))]
        persistence_ui::dialogs(app, ctx);
        return;
    };
    // Where the platform has no extra windows (the web build), draw over the main window.
    if ctx.embed_viewports() {
        draw(app, ctx, false);
        return;
    }
    let window_title = d.window_title();
    // A foreground Area only paints a scrim; it does NOT guarantee that earlier
    // widgets cannot receive clicks. An egui Modal blocks pointer/keyboard input in
    // the main viewport while the separate Camera Raw viewport owns the controls.
    egui::Modal::new(egui::Id::new("camera-raw-modal")).frame(egui::Frame::NONE).backdrop_color(Tokens::get(ctx).scrim).show(ctx, |_| {});
    // A window of its own (title bar, moved and resized like any window), like Adobe Camera Raw.
    let builder = egui::ViewportBuilder::default().with_title(window_title).with_inner_size([1360.0, 900.0]).with_min_inner_size([900.0, 600.0]);
    let mut close = false;
    ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("camera-raw-window"), builder, |ui, class| {
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            close = !app.camera_raw.as_mut().is_some_and(|d| d.preferences.handle_native_close(ui.ctx()));
        }
        draw(app, ui.ctx(), class != egui::ViewportClass::EmbeddedWindow);
    });
    if close && app.camera_raw.is_some() {
        cancel(app);
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }
}

impl CameraRawDialog {
    /// "Camera Raw (name)" for the open-time dialog, "Camera Raw Filter (layer)" otherwise.
    fn window_title(&self) -> String {
        if self.target == Target::OpenRaw {
            crate::i18n::fmt(tl!("Camera Raw ({name})"), &[("name", &self.layer_name)])
        } else {
            crate::i18n::fmt(tl!("Camera Raw Filter ({layer})"), &[("layer", &self.layer_name)])
        }
    }
}

/// Whether `p` carries effective develop adjustments (anything but the defaults).
fn has_edits(p: &CameraRaw) -> bool {
    // `encodingGamma` is the document's display encoding, set when the dialog opens (main 0.9.0), not an adjustment.
    non_defaults(p).as_object().is_some_and(|fields| fields.keys().any(|key| key != "encodingGamma"))
}

/// One row per unaccepted RAW, keyed by the import's real document identity. The host's
/// existing cached `layer_thumb` provides the GPU texture and owns its lifetime. The edit badge
/// is only claimed for the active photo and photos already reviewed (parked): a queued photo's
/// sidecar is not read until it is opened, so it is never reported as edited or unedited.
fn filmstrip_entries(app: &mut PhotocraftApp, ctx: &egui::Context) -> Vec<filmstrip_ui::Entry> {
    let active = app.camera_raw.as_ref().and_then(|dialog| dialog.raw_open.as_ref()).map(|raw| raw.document);
    let settings_source = app.camera_raw.as_ref().and_then(|dialog| dialog.raw_open.as_ref()).map(|raw| (raw.photo, raw.name.clone()));
    #[cfg(not(target_arch = "wasm32"))]
    let settings_source = settings_source.filter(|_| !persistence_ui::unready(app));
    let active_edited = app.camera_raw.as_ref().is_some_and(|dialog| has_edits(&dialog.params));
    let raws = filmstrip_raws(app);
    // Photographs, not documents: A1 -> A2 keeps its place. A photograph whose commit is in flight still belongs here.
    let mut present: Vec<PhotoId> = raws.iter().map(|raw| raw.photo).collect();
    present.extend(app.raw_redevelop.as_ref().map(|commit| commit.raw.photo));
    let active_photo = app.camera_raw.as_ref().and_then(|dialog| dialog.raw_open.as_ref()).map(|raw| raw.photo);
    app.raw_filmstrip.selection.retain_present(&present);
    if !raws.is_empty() {
        app.raw_filmstrip.selection.start_with(active_photo);
    }
    // F2: thumbnails made for the cards' own size replace the 64 px layer thumbnail once ready, for the photo shown and
    // the cards in view; every key says which photograph, document and content a thumbnail shows.
    let side = filmstrip_ui::thumbnail_side(&app.raw_filmstrip.frame, ctx.pixels_per_point());
    let in_view = filmstrip_ui::documents_in_view(&app.raw_filmstrip.frame);
    let keys: Vec<ThumbKey> = raws
        .iter()
        .filter_map(|raw| {
            let st = app.session.documents().iter().find(|st| st.doc.id == raw.document)?;
            let content = st.doc.layer(st.active_layer?)?.surface().map_or(0, crate::surface_fingerprint);
            let display = app.session.color.canvas_display(&st.doc).map_or(0, |display| display.key);
            Some(ThumbKey { photo: raw.photo, document: raw.document, content, display, side: side.unwrap_or(0) })
        })
        .collect();
    receive_card_thumb(app, ctx, &keys);
    app.raw_filmstrip.thumbs.retain(|thumb| keys.iter().any(|key| key.shows(&thumb.key)));
    let mut wanted: Option<ThumbWork> = None;
    let mut result = Vec::new();
    for raw in raws {
        let source = app.session.documents().iter().find(|st| st.doc.id == raw.document).map(|st| (st.doc.clone(), st.active_layer));
        let size = source.as_ref().map_or((0, 0), |(doc, _)| (doc.size.width, doc.size.height));
        let is_active = active == Some(raw.document);
        let key = keys.iter().find(|key| key.photo == raw.photo).copied();
        let card_thumb = key.and_then(|key| app.raw_filmstrip.thumbs.iter().find(|thumb| thumb.key.shows(&key)));
        // The photo shown first, then the first card in view; never one that failed or is being made.
        if let (Some(key), Some((doc, _))) = (key, &source)
            && key.side > 0
            && card_thumb.is_none_or(|thumb| thumb.key.side != key.side)
            && (is_active || in_view.contains(&raw.document))
            && !app.raw_filmstrip.thumb_failed.contains(&key)
            && app.raw_filmstrip.thumb_job.as_ref().is_none_or(|job| job.key != key)
            && (wanted.is_none() || is_active)
        {
            wanted = Some((key, doc.clone(), app.session.color.canvas_display(doc).ok()));
        }
        let unit = ERect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let (texture, uv) = match card_thumb {
            Some(thumb) => (Some(thumb.texture.id()), unit),
            None => source
                .and_then(|(doc, layer)| {
                    let layer = doc.layer(layer?)?;
                    Some(app.layer_thumb(ctx, &doc, layer))
                })
                .map_or((None, filmstrip_ui::fit_rect(unit, size)), |(texture, frame)| {
                    // Upstream returns the exact document bounds framed by this square,
                    // letterboxed thumbnail; use those bounds when sampling its picture.
                    (Some(texture), filmstrip_ui::fit_rect(unit, (frame.width().max(1), frame.height().max(1))))
                }),
        };
        // Known for the photo shown and for photos already reviewed (parked); a photo not yet shown is unknown.
        let parked = app.raw_filmstrip.parked.iter().find(|saved| saved.raw.photo == raw.photo).map(|saved| has_edits(&saved.params));
        let edited = if is_active { Some(active_edited) } else { parked };
        let adjustments = match edited {
            Some(true) => filmstrip_ui::Adjustments::Edited,
            Some(false) => filmstrip_ui::Adjustments::None,
            None => filmstrip_ui::Adjustments::Unknown,
        };
        let selected = app.raw_filmstrip.selection.contains(raw.photo);
        let rating = raw.rating;
        let (mut label, mut marked, mut pending_xmp) = (String::new(), false, false);
        #[cfg(not(target_arch = "wasm32"))]
        let rating = if let Some(recipe) = persistence_ui::recipe(app, raw.photo) {
            label = recipe.metadata.label.clone();
            marked = recipe.metadata.marked;
            pending_xmp = persistence_ui::pending_xmp(app, raw.photo);
            match recipe.metadata.rating {
                Some(-1) => PhotoRating::Rejected,
                None | Some(0) => PhotoRating::Unrated,
                Some(n) if (1..=5).contains(&n) => PhotoRating::Stars(n as u8),
                _ => PhotoRating::Unknown,
            }
        } else {
            rating
        };
        result.push(filmstrip_ui::Entry {
            document: raw.document,
            photo: raw.photo,
            name: raw.name,
            texture,
            uv,
            size,
            active: is_active,
            selected,
            settings_source: settings_source.clone(),
            can_undo: !app.raw_filmstrip.group_undo.is_empty(),
            adjustments,
            rating,
            label,
            marked,
            pending_xmp,
            filtered_out: false,
        });
    }
    // The strip's visual order (F4 lab S1): the queue, the documents and Open Images keep their own order.
    filmstrip_ui::sort_entries(&mut result, app.raw_filmstrip.view.sort, app.raw_filmstrip.view.reverse);
    // F4 lab F1: the filter hides photos from the strip only. The photo shown always stays (and says so); a hidden photo
    // stays in the review and keeps its selection; the count row says how many are hidden and selected.
    let filter = app.raw_filmstrip.view.filter;
    let mut hidden = filmstrip_ui::Hidden::default();
    result.retain_mut(|entry| {
        if filter.shows_metadata(entry.rating, &entry.label, entry.marked) && !app.raw_filmstrip.manually_hidden.contains(&entry.photo) {
            return true;
        }
        if entry.active {
            entry.filtered_out = true;
            return true;
        }
        hidden.count += 1;
        if entry.selected {
            hidden.selected.push(entry.photo);
        }
        false
    });
    app.raw_filmstrip.hidden = hidden;
    start_card_thumb(app, ctx, wanted, &keys);
    if app.raw_filmstrip.thumb_job.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(30));
    }
    result
}

/// The dialog's contents over the whole of `ctx`'s window. `own_window`: the OS window already
/// shows the title, so none is painted.
fn draw(app: &mut PhotocraftApp, ctx: &egui::Context, own_window: bool) {
    let keyboard_owned = ctx.egui_wants_keyboard_input() && !filmstrip_ui::owns_keyboard_focus(ctx);
    let previous_params = app.camera_raw.as_ref().filter(|d| d.target == Target::OpenRaw).map(|d| d.params.clone());
    #[cfg(not(target_arch = "wasm32"))]
    let persistence_blocked = persistence_ui::blocked(app) || jpeg_ui::is_modal_open(app) || app.raw_filmstrip.group_set.is_some();
    #[cfg(not(target_arch = "wasm32"))]
    let photo_unready = persistence_ui::unready(app);
    #[cfg(target_arch = "wasm32")]
    let photo_unready = false;
    #[cfg(target_arch = "wasm32")]
    let persistence_blocked = app.raw_filmstrip.group_set.is_some();
    #[cfg(not(target_arch = "wasm32"))]
    let cache_status = persistence_ui::status(app);
    #[cfg(target_arch = "wasm32")]
    let cache_status = "Recovery cache unavailable in this browser".to_string();
    // Whether a menu was open as this frame began: an Esc that closes it this frame is not a Cancel (F4 lab F1-R1).
    let menu_was_open = egui::Popup::is_any_open(ctx);
    let raw_defaults = app.session.prefs().raw_defaults.clone();
    let mut updated_defaults = None;
    let mut preferences_blocked = false;
    #[cfg(not(target_arch = "wasm32"))]
    let recovery_session = app.raw_filmstrip.persistence.is_some();
    #[cfg(target_arch = "wasm32")]
    let recovery_session = false;
    if let Some(d) = app.camera_raw.as_mut() {
        d.preferences.recovery_session = recovery_session && d.target == Target::OpenRaw;
        preferences_blocked = d.preferences.open || std::mem::take(&mut d.preferences.block_frame);
        if d.preferences.open {
            updated_defaults = preferences_ui::show(ctx, &mut d.preferences, &raw_defaults);
        }
    }
    let t = Tokens::get(ctx);
    let screen = ctx.content_rect();
    let current_display = app.session.active().and_then(|st| app.session.color.canvas_display(&st.doc).ok());
    let entries = if app.camera_raw.as_ref().is_some_and(|d| d.target == Target::OpenRaw) { filmstrip_entries(app, ctx) } else { Vec::new() };
    let mut selected_raw = None;
    let mut action: Option<&str> = None;
    let mut photo_actions = Vec::new();
    let mut reset_preview_settings = false;
    let mut reset_preview_photo = None;
    egui::Area::new(egui::Id::new("camera-raw-dialog")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        let Some(d) = app.camera_raw.as_mut() else { return };
        // Filter history remains local; retained-shoot history is one coherent group action.
        if d.target != Target::OpenRaw && !preferences_blocked && !persistence_blocked && !keyboard_owned && !menu_was_open {
            history_keys(d, ctx);
        }
        let pointer = ctx.input(|i| i.pointer.primary_down());
        if d.target != Target::OpenRaw && pointer && d.edit_origin.is_none() {
            d.edit_origin = Some(d.params.clone());
        }
        if let Some(display) = &current_display
            && display.key != d.display.key
        {
            d.display = display.clone();
            d.dirty = true;
        }
        d.curve_rect = None;
        d.scope.vectorscope_rect = None;
        if let Err(e) = d.poll_render(ctx) {
            d.preview_error = Some((e.clone(), d.preview_revision));
            app.ui.status = e;
            app.ui.status_error = true;
        }
        d.request_render(ctx);
        // A newer frame means the failure is over: take its message down, unless something else
        // has reported since.
        if let Some((e, revision)) = &d.preview_error
            && *revision != d.preview_revision
        {
            if app.ui.status_error && app.ui.status == *e {
                app.ui.status.clear();
                app.ui.status_error = false;
            }
            d.preview_error = None;
        }
        let (full, _) = ui.allocate_exact_size(screen.size(), Sense::click());
        let painter = ui.painter().clone();
        painter.rect_filled(full, 0.0, t.chrome);
        let title = ERect::from_min_size(full.min, vec2(full.width(), if own_window { 0.0 } else { 30.0 }));
        if !own_window {
            painter.rect_filled(title, 0.0, t.dock);
            painter.line_segment([title.left_bottom(), title.right_bottom()], Stroke::new(1.0, t.separator));
            painter.text(title.center(), Align2::CENTER_CENTER, d.window_title(), FontId::proportional(13.0), t.text);
        }
        // Every photo in the review, the ones the filter hides included (F1-R1): Open commits them all.
        let photos = entries.len() + app.raw_filmstrip.hidden.count;
        let ok_label = if d.target == Target::OpenRaw && photos > 1 {
            tl!("Open Images")
        } else if d.target == Target::OpenRaw {
            tl!("Open")
        } else {
            tl!("OK")
        };
        let show_cache_status = d.target == Target::OpenRaw
            && raw_defaults.recovery_mode == photocraft_engine::prefs::RawRecoveryMode::Cache
            && raw_defaults.show_recovery_progress;
        let footer_h = if show_cache_status { 76.0 } else { 48.0 };
        let body = ERect::from_min_max(pos2(full.left(), title.bottom()), pos2(full.right(), full.bottom() - footer_h));
        let foot = ERect::from_min_max(pos2(full.left(), full.bottom() - footer_h), full.max);
        // Read the gear before any review controls so its opening frame also owns input.
        let right = ERect::from_min_max(pos2(body.right() - PANEL_W, body.top()), body.max);
        let gear = ERect::from_min_size(pos2(right.right() - 42.0, right.top() + 6.0), vec2(28.0, 28.0));
        painter.rect_filled(right, 0.0, t.dock);
        let mut gear_ui = ui.new_child(egui::UiBuilder::new().max_rect(gear));
        if preferences_blocked {
            gear_ui.disable();
        }
        let response = crate::icons::button_with_icon_size(&mut gear_ui, "settings", 28.0, 18.0, d.preferences.open, tl!("Camera Raw Preferences"));
        d.preferences.gear = Some(response.rect);
        let mut opened_preferences = false;
        // One grey gear: Camera Raw Preferences, the portable XMP settings presets and
        // Before/After. Recovery settings live only in Preferences > Long-session recovery.
        egui::Popup::menu(&response).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
            if ui.button(tl!("Camera Raw Preferences…")).clicked() {
                opened_preferences = true;
                ui.close();
            }
            ui.separator();
            ui.add_enabled_ui(!persistence_blocked, |ui| {
                if ui.button(tl!("Load Settings…")).clicked() {
                    action = Some("load");
                    ui.close();
                }
                if ui.button(tl!("Save Settings…")).clicked() {
                    action = Some("save");
                    ui.close();
                }
                ui.checkbox(&mut d.show_before, tl!("Before (Y)"));
            });
        });
        if opened_preferences {
            egui::Popup::close_all(ctx);
            d.preferences.open = true;
            preferences_blocked = true;
        }
        let saved_selection = (preferences_blocked || persistence_blocked).then(|| app.raw_filmstrip.selection.clone());
        if preferences_blocked || persistence_blocked {
            ui.disable();
            // Preview, scope and Filmstrip shortcuts read InputState directly, bypassing
            // disabled widgets. Consume them for the entire open/dismiss frame.
        }
        let background_input = if preferences_blocked {
            Some(preferences_ui::BackgroundInput::block(ctx))
        } else if persistence_blocked {
            Some(preferences_ui::BackgroundInput::before_overlay(ctx))
        } else {
            None
        };
        // The show/hide control and the splitter are read before the layout, so the preview
        // reclaims or gives up the width in this same frame. The control sits in the preview toolbar after the zoom group
        // (F3, lab cycle 4): its slot comes from this frame's layout before the click is read, and the toolbar is laid out
        // again from the final layout, so the control is drawn where the next click will find it.
        let present = photos > 1;
        let (readout_widths, quick_width) = super::camera_raw_preview_ui::toolbar_widths(ui);
        let slots_for =
            |lay: &filmstrip_ui::Layout| super::camera_raw_preview_ui::toolbar_slots(toolbar_row(lay.preview_body), readout_widths, quick_width, present);
        let toggle_hit = slots_for(&filmstrip_ui::layout(body, PANEL_W, &app.raw_filmstrip.view, present)).filmstrip.map(|(_, [toggle, ..])| toggle);
        let (toggle, menu) = match toggle_hit {
            Some(rect) => {
                let (response, menu) = filmstrip_ui::toggle_input(ui, rect, &mut app.raw_filmstrip.view);
                (Some(response), menu)
            }
            None => (None, Vec::new()),
        };
        let previous_splitter = app.raw_filmstrip.frame.layout.splitter.map(|rect| (rect, app.raw_filmstrip.frame.layout.orientation));
        let splitter_hot = filmstrip_ui::splitter_input(ui, previous_splitter, body, &mut app.raw_filmstrip.view);
        let lay = filmstrip_ui::layout(body, PANEL_W, &app.raw_filmstrip.view, present);
        let slots = slots_for(&lay);
        let toggle_rect = slots.filmstrip.map(|(_, [toggle, ..])| toggle);
        let mut cards = Vec::new();
        if let Some(strip) = lay.strip {
            let strip_state = &mut app.raw_filmstrip;
            let drawn = filmstrip_ui::draw(ui, strip, &t, &mut strip_state.view, &mut strip_state.selection, &entries, &strip_state.hidden);
            (selected_raw, cards) = (drawn.activate, drawn.cards);
            photo_actions.extend(drawn.actions);
            if let Some(splitter) = lay.splitter {
                filmstrip_ui::paint_splitter(ui, splitter, &t, splitter_hot, lay.orientation);
            }
        }
        app.raw_filmstrip.frame = filmstrip_ui::Frame { layout: lay, toggle: toggle_rect, cards, menu, ..Default::default() };
        let scope = &mut app.ui.camera_raw_scope;
        // Clip both pixels and scope overlays before painting the settings dock/footer.
        let preview_body = lay.preview_body;
        let view = preview_view(preview_body);
        d.viewport = Some(view);
        d.preview_ppp = ctx.pixels_per_point();
        let size = vec2(d.full_w as f32, d.full_h as f32) / ctx.pixels_per_point();
        let navigation = &mut app.ui.camera_raw_preview;
        let mut preview = ui.new_child(egui::UiBuilder::new().max_rect(view));
        if app.raw_filmstrip.view.grid {
            preview.disable();
        }
        preview.set_clip_rect(view.intersect(ui.clip_rect()));
        preview.painter().rect_filled(view, 0.0, t.canvas);
        let (r, response, panning, zoom_box) =
            super::camera_raw_preview_ui::interact(&mut preview, navigation, view, size, &mut scope.sampler_tool, &mut reset_preview_settings);
        let reset_photo_id = response.id.with("reset-settings-photo-id");
        if response.secondary_clicked() {
            ctx.data_mut(|data| data.insert_temp(reset_photo_id, d.raw_open.as_ref().map(|raw| raw.photo)));
        }
        if reset_preview_settings {
            reset_preview_photo = ctx.data(|data| data.get_temp::<Option<PhotoId>>(reset_photo_id)).flatten();
        }
        let tex = if d.show_before { d.before_tex.as_ref() } else { d.tex.as_ref() };
        if let Some(tex) = tex {
            // Never tile a checker across the offscreen extent of a highly zoomed image.
            widgets::checker(preview.painter(), r.intersect(view), 8.0);
            preview.painter().image(tex.id(), r, ERect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            let raw_tone = d.raw_curve.is_some() && !raw_stage(&d.params).is_neutral();
            let detail_needed = !raw_tone && (r.width() * ctx.pixels_per_point() > d.pw as f32 || r.height() * ctx.pixels_per_point() > d.ph as f32);
            d.detail.paint(&preview, r, d.preview_revision, d.show_before, &d.params, detail_needed, &d.display);
            if d.detail.pending() {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            d.scope.preview_rect = Some(r);
            super::camera_raw_scope_ui::preview(&mut preview, d, scope, r, &response, panning);
        }
        if let Some(region) = zoom_box {
            preview.painter().rect_stroke(region, 0.0, egui::Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
        }
        if app.raw_filmstrip.view.grid {
            let strip = &mut app.raw_filmstrip;
            let drawn = filmstrip_ui::draw_grid(ui, view, &t, &mut strip.view, &mut strip.selection, &entries, &strip.hidden);
            selected_raw = drawn.activate.or(selected_raw);
            strip.frame.grid = Some(view);
            strip.frame.cards.extend(drawn.cards);
            photo_actions.extend(drawn.actions);
        }
        let mut nav_ui = ui.new_child(egui::UiBuilder::new().max_rect(toolbar_row(preview_body)).layout(egui::Layout::left_to_right(egui::Align::Center)));
        nav_ui.set_clip_rect(preview_body.intersect(ui.clip_rect()));
        // The detail refinement's progress and its failure are an icon in the room left before the tools; a row with no
        // room for it (the narrowest window in a long language) puts the notice in the readout's tooltip instead, never over
        // a control (C5-R1).
        let room = slots.status.width() >= 20.0;
        let notice = d.detail.error.as_ref().map(|error| format!("{}\n{error}", tl!("Full-resolution preview unavailable")));
        super::camera_raw_preview_ui::toolbar(&mut nav_ui, &slots, navigation, view, size, &mut scope.sampler_tool, notice.as_deref().filter(|_| !room));
        if let Some((divider, [_, grid, sort, filter])) = slots.filmstrip {
            let y = slots.readout.center().y;
            nav_ui.painter().line_segment([pos2(divider, y - 9.0), pos2(divider, y + 9.0)], Stroke::new(1.0, t.separator));
            if let (Some(rect), Some(response)) = (toggle_rect, &toggle) {
                filmstrip_ui::paint_toggle(&nav_ui, rect, &t, &app.raw_filmstrip.view, response);
            }
            filmstrip_ui::sort_control(&mut nav_ui, &t, sort, &mut app.raw_filmstrip.view);
            filmstrip_ui::filter_control(&mut nav_ui, &t, filter, &mut app.raw_filmstrip.view, &app.raw_filmstrip.hidden);
            filmstrip_ui::grid_control(&mut nav_ui, &t, grid, &mut app.raw_filmstrip.view);
        }
        app.raw_filmstrip.frame.toolbar = slots.extent();
        let mut status = nav_ui.new_child(egui::UiBuilder::new().max_rect(slots.status).layout(egui::Layout::right_to_left(egui::Align::Center)));
        if room && d.detail.pending() {
            status.spinner();
        }
        if let Some(error) = d.detail.error.as_ref().filter(|_| room) {
            let notice = tl!("Full-resolution preview unavailable");
            let (rect, response) = status.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
            crate::icons::paint(&status, rect, "triangle-alert", 14.0, t.text_faint);
            response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, notice));
            response.on_hover_text(format!("{notice}\n{error}"));
        }
        // Panels.
        painter.line_segment([right.left_top(), right.left_bottom()], Stroke::new(1.0, t.separator));
        let properties = ERect::from_min_max(pos2(right.left() + 14.0, right.top() + 40.0), right.max - vec2(14.0, 10.0));
        let mut props = ui.new_child(egui::UiBuilder::new().max_rect(properties));
        if photo_unready {
            props.disable();
        }
        // This header is outside the settings scroll area and follows the same Before/After
        // selector as the texture. Both derive from the same completed preview revision.
        super::camera_raw_scope_ui::header(&mut props, d, scope);
        d.scope.alt_tone = None;
        let mut dirty = false;
        // Separate RAWs must start at Exposure, not inherit the previous
        // dialog's scroll offset from egui's persistent memory.
        egui::ScrollArea::vertical().id_salt(properties_scroll_salt(d)).show(&mut props, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            let p = &mut d.params;
            let alt = &mut d.scope.alt_tone;
            let (ts, tn) = (temp_stops(), tint_stops());
            section(ui, "Light", true, |ui| {
                tone_row(ui, &mut dirty, alt, ToneZone::Exposure, &mut p.exposure);
                row(ui, &mut dirty, "Contrast", &mut p.contrast, -100.0..=100.0, None);
                tone_row(ui, &mut dirty, alt, ToneZone::Highlights, &mut p.highlights);
                tone_row(ui, &mut dirty, alt, ToneZone::Shadows, &mut p.shadows);
                tone_row(ui, &mut dirty, alt, ToneZone::Whites, &mut p.whites);
                tone_row(ui, &mut dirty, alt, ToneZone::Blacks, &mut p.blacks);
            });
            section(ui, "Color", false, |ui| {
                widgets::section_label(ui, tl!("White Balance: As Shot"));
                row(ui, &mut dirty, "Temperature", &mut p.temperature, -100.0..=100.0, Some(&ts));
                row(ui, &mut dirty, "Tint", &mut p.tint, -100.0..=100.0, Some(&tn));
                row(ui, &mut dirty, "Vibrance", &mut p.vibrance, -100.0..=100.0, None);
                row(ui, &mut dirty, "Saturation", &mut p.saturation, -100.0..=100.0, None);
            });
            section(ui, "Effects", false, |ui| {
                row(ui, &mut dirty, "Texture", &mut p.texture, -100.0..=100.0, None);
                row(ui, &mut dirty, "Clarity", &mut p.clarity, -100.0..=100.0, None);
                row(ui, &mut dirty, "Dehaze", &mut p.dehaze, -100.0..=100.0, None);
                widgets::section_label(ui, tl!("Vignetting"));
                let mut style = p.vignette_style.clone();
                if widgets::dropdown(
                    ui,
                    "cr-vig-style",
                    &mut style,
                    &[
                        ("highlightPriority".to_string(), tl!("Highlight Priority")),
                        ("colorPriority".to_string(), tl!("Color Priority")),
                        ("paintOverlay".to_string(), tl!("Paint Overlay")),
                    ],
                    200.0,
                ) {
                    p.vignette_style = style;
                    dirty = true;
                }
                row(ui, &mut dirty, "Amount", &mut p.vignette_amount, -100.0..=100.0, None);
                row(ui, &mut dirty, "Midpoint", &mut p.vignette_midpoint, 0.0..=100.0, None);
                row(ui, &mut dirty, "Roundness", &mut p.vignette_roundness, -100.0..=100.0, None);
                row(ui, &mut dirty, "Feather", &mut p.vignette_feather, 0.0..=100.0, None);
                row(ui, &mut dirty, "Highlights", &mut p.vignette_highlights, 0.0..=100.0, None);
                widgets::section_label(ui, tl!("Grain"));
                row(ui, &mut dirty, "Amount", &mut p.grain_amount, 0.0..=100.0, None);
                row(ui, &mut dirty, "Size", &mut p.grain_size, 0.0..=100.0, None);
                row(ui, &mut dirty, "Roughness", &mut p.grain_roughness, 0.0..=100.0, None);
            });
            section(ui, "Curve", false, |ui| {
                d.curve_rect = Some(curve_editor(ui, p, &mut dirty, &mut d.curve_state));
                row(ui, &mut dirty, "Highlights", &mut p.curve_highlights, -100.0..=100.0, None);
                row(ui, &mut dirty, "Lights", &mut p.curve_lights, -100.0..=100.0, None);
                row(ui, &mut dirty, "Darks", &mut p.curve_darks, -100.0..=100.0, None);
                row(ui, &mut dirty, "Shadows", &mut p.curve_shadows, -100.0..=100.0, None);
            });
            section(ui, "Color Mixer", false, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (i, source) in ["Hue", "Saturation", "Luminance"].iter().enumerate() {
                        let name = crate::i18n::tr_ctx(crate::i18n::current(), "cameraRaw", source);
                        let response = widgets::pill_tab(ui, name, d.mixer_tab == i);
                        response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, ui.is_enabled(), d.mixer_tab == i, name));
                        if response.clicked() {
                            d.mixer_tab = i;
                        }
                    }
                });
                let arr = match d.mixer_tab {
                    0 => &mut p.hsl_hue,
                    1 => &mut p.hsl_sat,
                    _ => &mut p.hsl_lum,
                };
                for (k, name) in BANDS.iter().enumerate() {
                    let c = photocraft_algo::camera_raw::HSL_BANDS[k];
                    let base = hue_color(c);
                    let grad = [Color32::from_gray(128), base];
                    row(ui, &mut dirty, name, &mut arr[k], -100.0..=100.0, Some(&grad));
                }
            });
            section(ui, "Color Grading", false, |ui| {
                wheel(ui, &mut dirty, "Shadows", &mut p.grade_shadows);
                wheel(ui, &mut dirty, "Midtones", &mut p.grade_midtones);
                wheel(ui, &mut dirty, "Highlights", &mut p.grade_highlights);
                wheel(ui, &mut dirty, "Global", &mut p.grade_global);
                row(ui, &mut dirty, "Blending", &mut p.grade_blending, 0.0..=100.0, None);
                row(ui, &mut dirty, "Balance", &mut p.grade_balance, -100.0..=100.0, None);
            });
            section(ui, "Detail", false, |ui| {
                widgets::section_label(ui, tl!("Sharpening"));
                row(ui, &mut dirty, "Amount", &mut p.sharpen_amount, 0.0..=150.0, None);
                row(ui, &mut dirty, "Radius", &mut p.sharpen_radius, 0.5..=3.0, None);
                row(ui, &mut dirty, "Detail", &mut p.sharpen_detail, 0.0..=100.0, None);
                row(ui, &mut dirty, "Masking", &mut p.sharpen_masking, 0.0..=100.0, None);
                widgets::section_label(ui, tl!("Noise Reduction"));
                row(ui, &mut dirty, "Luminance", &mut p.noise_luminance, 0.0..=100.0, None);
                row(ui, &mut dirty, "Luminance Detail", &mut p.noise_luminance_detail, 0.0..=100.0, None);
                row(ui, &mut dirty, "Color", &mut p.noise_color, 0.0..=100.0, None);
                row(ui, &mut dirty, "Color Detail", &mut p.noise_color_detail, 0.0..=100.0, None);
            });
        });
        super::camera_raw_scope_ui::floating(ctx, d, scope, right);
        if !preferences_blocked {
            super::camera_raw_scope_ui::shortcuts(ctx, scope);
        }
        d.dirty |= dirty;
        if d.dirty {
            ctx.request_repaint();
        }
        if d.render_pending() {
            // The worker asks for a repaint when it is done; this catches a missed wake-up.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        // Footer.
        painter.rect_filled(foot, 0.0, t.dock);
        painter.line_segment([foot.left_top(), foot.right_top()], Stroke::new(1.0, t.separator));
        let controls = ERect::from_min_max(pos2(foot.left(), foot.bottom() - 48.0), foot.max).shrink2(vec2(16.0, 9.0));
        let mut fu = ui.new_child(egui::UiBuilder::new().max_rect(controls).layout(egui::Layout::right_to_left(egui::Align::Center)));
        // Done and Cancel share the same themed dimensions.
        let mut buttons = vec![
            widgets::DialogButton::new(widgets::ButtonRole::Default, ok_label, 90.0),
            widgets::DialogButton::new(widgets::ButtonRole::Cancel, tl!("Cancel"), 90.0),
        ];
        if d.target == Target::OpenRaw {
            buttons.push(widgets::DialogButton::new(widgets::ButtonRole::Apply, tl!("Done"), 90.0));
        }
        if let Some(role) = widgets::dialog_buttons(&mut fu, &buttons) {
            action = match role {
                widgets::ButtonRole::Default => Some("ok"),
                widgets::ButtonRole::Apply => Some("done"),
                _ => Some("cancel"),
            };
        }
        if show_cache_status {
            let status_rect = ERect::from_min_size(foot.min + vec2(16.0, 4.0), vec2(foot.width() - 32.0, 22.0));
            let mut status_ui = ui.new_child(egui::UiBuilder::new().max_rect(status_rect));
            status_ui.add(egui::Label::new(&cache_status).truncate()).on_hover_text(&cache_status);
        }
        if let Some(selection) = saved_selection {
            app.raw_filmstrip.selection = selection;
        }
        drop(background_input);
        if opened_preferences {
            updated_defaults = preferences_ui::show(ctx, &mut d.preferences, &raw_defaults);
        }
        if !ctx.input(|i| i.pointer.primary_down())
            && let Some(origin) = d.edit_origin.take()
            && origin != d.params
        {
            let edited = std::mem::replace(&mut d.params, origin);
            remember_edit(d);
            d.params = edited;
        }
    });
    if let Some(defaults) = updated_defaults {
        match serde_json::to_value(defaults)
            .map_err(|error| error.to_string())
            .and_then(|value| app.run("prefs.set", json!({"path":"rawDefaults","value":value})))
        {
            Ok(_) => {}
            Err(error) => {
                app.ui.status = error;
                app.ui.status_error = true;
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(recovery_action) = app.camera_raw.as_mut().and_then(|d| d.preferences.recovery_action.take())
        && recovery_action == "recovery-details"
    {
        persistence_ui::show_details(app);
    }
    #[cfg(not(target_arch = "wasm32"))]
    persistence_ui::dialogs(app, ctx);
    #[cfg(not(target_arch = "wasm32"))]
    jpeg_ui::show(app, ctx);
    group_set_dialog(app, ctx);
    if preferences_blocked || persistence_blocked {
        return;
    }
    if reset_preview_settings {
        if let Some(id) = reset_preview_photo {
            photo_actions.push(filmstrip_ui::PhotoAction::ResetSettings(id));
        } else if let Some(d) = app.camera_raw.as_mut().filter(|d| d.target != Target::OpenRaw) {
            // Camera Raw Filter has one image, not a retained RAW selection.
            remember_edit(d);
            d.params = CameraRaw { encoding_gamma: d.params.encoding_gamma, pixel_scale: d.params.pixel_scale, ..Default::default() };
            d.curve_state = Default::default();
            d.dirty = true;
        }
    }
    if let Some(before) = previous_params
        && let Some(after) = app.camera_raw.as_ref().map(|d| d.params.clone())
        && before != after
    {
        let change = edit_actions::slider_group(app, &before, &after);
        if let Some(d) = app.camera_raw.as_mut() {
            d.params = before;
            d.dirty = true;
        }
        match change.and_then(|change| apply_group(app, change)) {
            Ok(()) => {}
            Err(error) => {
                app.ui.status = error;
                app.ui.status_error = true;
            }
        }
    }
    let shortcut_allowed = !keyboard_owned
        && (!ctx.egui_wants_keyboard_input() || filmstrip_ui::owns_keyboard_focus(ctx))
        && !menu_was_open
        && !egui::Popup::is_any_open(ctx)
        && !app.ui.camera_raw_scope.sampler_tool;
    if shortcut_allowed {
        if app.camera_raw.as_ref().is_some_and(|d| d.target == Target::OpenRaw) {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z)) {
                if !app.raw_filmstrip.group_redo.is_empty()
                    && let Err(error) = group_control(app, &json!({"action":"redo"}))
                {
                    app.ui.status = error;
                    app.ui.status_error = true;
                }
            } else if !app.raw_filmstrip.group_undo.is_empty() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)) {
                photo_actions.push(filmstrip_ui::PhotoAction::UndoGroup);
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F)) {
            app.raw_filmstrip.fullscreen = !app.raw_filmstrip.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(app.raw_filmstrip.fullscreen));
        }
        let active = app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.photo);
        let targets = if app.raw_filmstrip.selection.selected_ids().is_empty() {
            active.into_iter().collect()
        } else {
            app.raw_filmstrip.selection.selected_ids().to_vec()
        };
        for event in ctx.input(|i| i.events.clone()) {
            if let egui::Event::Key { key, modifiers, pressed: true, .. } = event
                && let Some(action) = filmstrip_ui::metadata_shortcut(key, modifiers, targets.clone())
            {
                photo_actions.push(action);
            }
        }
    }
    for photo_action in photo_actions {
        match settings_photo_action(app, &photo_action) {
            Ok(true) => continue,
            Err(error) => {
                app.ui.status = error;
                app.ui.status_error = true;
                continue;
            }
            Ok(false) => {}
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let filmstrip_ui::PhotoAction::Remove { targets } = &photo_action {
            if let Err(error) = persistence_ui::request_remove(app, targets.clone()) {
                app.ui.status = error;
                app.ui.status_error = true;
            }
            continue;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let filmstrip_ui::PhotoAction::SaveJpeg(id) = photo_action {
            if let Err(error) = jpeg_ui::request(app, id) {
                app.ui.status = error;
                app.ui.status_error = true;
            }
            continue;
        }
        if let Err(error) = edit_actions::route_photo_action(app, &photo_action) {
            app.ui.status = error;
            app.ui.status_error = true;
        }
    }
    super::camera_raw_scope_ui::persist(app, ctx);
    if let Some(photo) = selected_raw {
        let document = filmstrip_raws(app).iter().find(|raw| raw.photo == photo).map(|raw| raw.document);
        if let Some(Err(error)) = document.map(|document| select_raw_review(app, ctx, document)) {
            app.ui.status = format!("Camera Raw filmstrip: {error}");
            app.ui.status_error = true;
        }
        return;
    }
    if shortcut_allowed
        && ctx.input(|i| i.key_pressed(egui::Key::Y))
        && let Some(d) = app.camera_raw.as_mut()
    {
        d.show_before = !d.show_before;
    }
    // Esc closes an open menu (Sort, Filter, a card's or the Filmstrip control's) before anything else: only an Esc with no
    // menu open, at the start of this frame or now, cancels the review (F4 lab F1-R1).
    if !keyboard_owned && ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !menu_was_open && !egui::Popup::is_any_open(ctx) {
        action = Some("cancel");
    }
    if action.is_some() {
        // Closing a child viewport is not guaranteed to invalidate its parent.
        // Force the start screen or newly accepted document to paint immediately.
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }
    match action {
        #[cfg(not(target_arch = "wasm32"))]
        Some("recovery-details") => persistence_ui::show_details(app),
        #[cfg(not(target_arch = "wasm32"))]
        Some("save-edits") => {
            if let Err(error) = persistence_ui::save_edits(app) {
                app.ui.status = error;
                app.ui.status_error = true;
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        Some("done") => {
            if let Err(error) = persistence_ui::done(app) {
                app.ui.status = error;
                app.ui.status_error = true;
            }
        }
        Some("ok") => {
            if let Err(e) = commit(app) {
                app.ui.status = e;
                app.ui.status_error = true;
            }
        }
        Some("cancel") => cancel(app),
        Some("save") => {
            let r = save_settings(app);
            report_file_error(app, r);
        }
        Some("load") => {
            let r = load_settings(app);
            report_file_error(app, r);
        }
        _ => {}
    }
}

fn apply_group(app: &mut PhotocraftApp, change: edit_actions::GroupChange) -> Result<(), String> {
    if change.is_empty() {
        return Ok(());
    }
    #[cfg(not(target_arch = "wasm32"))]
    if app.raw_filmstrip.persistence.is_some() {
        persistence_ui::set_adjustments(app, &change.after)?;
    }
    edit_actions::apply_display_group(app, &change.after);
    if app.raw_filmstrip.group_undo.len() >= 100 {
        app.raw_filmstrip.group_undo.remove(0);
    }
    app.raw_filmstrip.group_undo.push(change);
    app.raw_filmstrip.group_redo.clear();
    Ok(())
}

/// Context actions keep the identities captured when the menu opened. Reset is
/// deliberately one photograph, even when that photograph belongs to a group.
fn settings_photo_action(app: &mut PhotocraftApp, action: &filmstrip_ui::PhotoAction) -> Result<bool, String> {
    use filmstrip_ui::PhotoAction;
    if !matches!(action, PhotoAction::SyncSettings { .. } | PhotoAction::ResetSettings(_) | PhotoAction::SetValue { .. } | PhotoAction::UndoGroup) {
        return Ok(false);
    }
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::finishing(app) {
        return Err("Finish decision is holding edits".into());
    }
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::unready(app) {
        return Err("Wait for verified photograph admission before editing".into());
    }
    if let PhotoAction::UndoGroup = action {
        group_control(app, &json!({"action":"undo"}))?;
        return Ok(true);
    }
    let dialog = app.camera_raw.as_ref().ok_or("Camera Raw is not open")?;
    let current = dialog.params.clone();
    let change = match action {
        PhotoAction::SyncSettings { source, targets } => {
            if dialog.raw_open.as_ref().map(|raw| raw.photo) != Some(*source) {
                return Err("The active photograph changed; reopen Sync Settings".into());
            }
            if targets.len() < 2 || !targets.contains(source) {
                return Err("Select the active photograph and at least one other photograph to sync settings".into());
            }
            let targets = edit_actions::settings_targets(app, targets, &current)?;
            edit_actions::synchronise_group(&current, &targets)?
        }
        PhotoAction::ResetSettings(id) => {
            let target = edit_actions::settings_targets(app, &[*id], &current)?;
            edit_actions::reset_group(&target)?
        }
        PhotoAction::SetValue { targets } => {
            edit_actions::settings_targets(app, targets, &current)?;
            app.raw_filmstrip.group_set = Some(("exposure".into(), 0.0, targets.clone()));
            return Ok(true);
        }
        _ => return Ok(false),
    };
    apply_group(app, change)?;
    Ok(true)
}

fn group_control(app: &mut PhotocraftApp, request: &Value) -> Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    if persistence_ui::finishing(app) {
        return Err("Finish decision is holding edits".into());
    }
    let action = request.get("action").and_then(Value::as_str).ok_or("A group action is required")?;
    if action == "undo" || action == "redo" {
        let redo = action == "redo";
        let history = if redo { &app.raw_filmstrip.group_redo } else { &app.raw_filmstrip.group_undo };
        let change = history.last().ok_or("No group edit available in that history direction")?.clone();
        let values = if redo { &change.after } else { &change.before };
        let current = app.camera_raw.as_ref().ok_or("Camera Raw is not open")?.params.clone();
        let targets: Vec<_> = values.iter().map(|(id, _)| *id).collect();
        // A removed/rebound member must not turn a group history operation into
        // a partial edit. Keep both history stacks intact until validation succeeds.
        let retained = filmstrip_raws(app);
        if targets.iter().any(|id| !retained.iter().any(|raw| raw.photo == *id)) {
            return Err("A photograph in this history step has left the Camera Raw review".into());
        }
        edit_actions::settings_targets(app, &targets, &current)?;
        #[cfg(not(target_arch = "wasm32"))]
        if app.raw_filmstrip.persistence.is_some() {
            persistence_ui::set_adjustments(app, values)?;
        }
        edit_actions::apply_display_group(app, values);
        if redo {
            app.raw_filmstrip.group_redo.pop();
            app.raw_filmstrip.group_undo.push(change);
        } else {
            app.raw_filmstrip.group_undo.pop();
            app.raw_filmstrip.group_redo.push(change);
        }
        return Ok(());
    }
    let current = app.camera_raw.as_ref().ok_or("Camera Raw is not open")?.params.clone();
    let targets = edit_actions::slider_group(app, &current, &current)?.before;
    let change = match action {
        "reset" => edit_actions::reset_group(&targets)?,
        "synchronise" => edit_actions::synchronise_group(&current, &targets)?,
        "setValue" => {
            let path = request.get("parameter").and_then(Value::as_str).ok_or("Choose a parameter")?;
            let value = request.get("value").and_then(Value::as_f64).ok_or("Enter a numeric value")?;
            edit_actions::set_value_group(path, value as f32, &targets)?
        }
        _ => return Err("Unknown group action".into()),
    };
    apply_group(app, change)
}

fn group_set_dialog(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some((mut parameter, mut value, targets)) = app.raw_filmstrip.group_set.clone() else {
        return;
    };
    let names = filmstrip_raws(app).iter().filter(|raw| targets.contains(&raw.photo)).map(|raw| raw.name.as_str()).collect::<Vec<_>>().join(", ");
    let mut action = None;
    egui::Modal::new(egui::Id::new("camera-raw-set-value")).show(ctx, |ui| {
        ui.heading(tl!("Set value"));
        widgets::elided_label(ui, &names, 420.0);
        let choices = [
            ("exposure", "Exposure"),
            ("contrast", "Contrast"),
            ("temperature", "Temperature"),
            ("tint", "Tint"),
            ("shadows", "Shadows"),
            ("highlights", "Highlights"),
            ("whites", "Whites"),
            ("blacks", "Blacks"),
            ("texture", "Texture"),
            ("clarity", "Clarity"),
            ("dehaze", "Dehaze"),
            ("vibrance", "Vibrance"),
            ("saturation", "Saturation"),
        ];
        let label = choices.iter().find(|(id, _)| *id == parameter).map_or("Exposure", |(_, label)| *label);
        egui::ComboBox::from_id_salt("camera-raw-set-parameter").selected_text(label).show_ui(ui, |ui| {
            for (id, label) in choices {
                ui.selectable_value(&mut parameter, id.to_owned(), label);
            }
        });
        ui.add(egui::DragValue::new(&mut value).speed(0.1));
        if ui.button(tl!("Set value")).clicked() {
            action = Some(true);
        }
        if ui.button(tl!("Cancel")).clicked() {
            action = Some(false);
        }
    });
    app.raw_filmstrip.group_set = Some((parameter.clone(), value, targets.clone()));
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        action = Some(false);
    }
    if let Some(apply) = action {
        let mut set = || {
            #[cfg(not(target_arch = "wasm32"))]
            if persistence_ui::finishing(app) {
                return Err("Finish decision is holding edits".into());
            }
            let current = app.camera_raw.as_ref().ok_or("Camera Raw is not open")?.params.clone();
            let targets = edit_actions::settings_targets(app, &targets, &current)?;
            let change = edit_actions::set_value_group(&parameter, value, &targets)?;
            apply_group(app, change)
        };
        if apply && let Err(error) = set() {
            app.ui.status = error;
            app.ui.status_error = true;
            return;
        }
        app.raw_filmstrip.group_set = None;
    }
}

fn hue_color(h: f32) -> Color32 {
    let h6 = (h.rem_euclid(360.0)) / 60.0;
    let x = 1.0 - ((h6 % 2.0) - 1.0).abs();
    let (r, g, b) = match h6 as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    Color32::from_rgb((r * 220.0) as u8, (g * 220.0) as u8, (b * 220.0) as u8)
}

// GRW-1 lab cycle 1 RED/GREEN probes (test-only, placed after all production code for the i18n label scanner; the same file on pristine main and on the port).
#[cfg(test)]
#[path = "camera_raw_lab_probes.rs"]
mod lab_probes;
#[cfg(all(test, not(target_arch = "wasm32")))]
#[path = "camera_raw_persistence_tests.rs"]
mod persistence_tests;
#[cfg(test)]
mod tests {
    use super::*;

    mod context_menu_tests {
        include!("camera_raw_context_menu_tests.rs");
    }

    #[test]
    fn reused_preview_buffer_discards_prior_contents_and_matches_fresh_development() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        app.run("file.new", json!({"width":300,"height":240})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("edit.fill", json!({"color":"#808040"})).unwrap();
        open(&mut app, &ctx).unwrap();
        let d = app.camera_raw.as_mut().unwrap();
        let original = d.proxy.clone();
        for (len, exposure) in [(0, 0.25), (1, -0.5), (original.len(), 0.75), (original.len() + 7, 0.0)] {
            d.processed = vec![[f32::NAN, f32::INFINITY, -1.0, 0.0]; len];
            d.params.exposure = exposure;
            let mut p = d.params.clone();
            p.pixel_scale = (d.pw as f32 / d.full_w.max(1) as f32).min(1.0);
            let mut expected = original.as_ref().clone();
            photocraft_algo::camera_raw::develop(&mut expected, d.pw, d.ph, &p, d.float);
            let revision = d.preview_revision;
            d.render(&ctx);
            assert_eq!(d.processed.len(), expected.len());
            assert!(d.processed.iter().flatten().zip(expected.iter().flatten()).all(|(a, b)| a.to_bits() == b.to_bits()));
            assert_eq!(d.after_histogram, RgbHistogram::from_rgba(&expected));
            assert_eq!(d.proxy, original);
            assert_eq!(d.preview_revision, revision + 1);
        }
    }

    mod preferences_tests {
        include!("camera_raw_preferences_tests.rs");
    }

    #[test]
    fn histogram_follows_preview_and_view_changes_do_not_render_or_edit() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 64, "height": 48})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("edit.fill", json!({"color": "#808080"})).unwrap();
        let steps = app.session.active().unwrap().history.past_len();
        let initial = menu(&mut app, &ctx, "filter.cameraRaw", &json!({})).unwrap().unwrap();
        assert_eq!(initial["histogram"]["samples"], 64 * 48);
        assert_eq!(initial["histogram"]["red"][128], 64 * 48);
        let updated = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.0}}})).unwrap().unwrap();
        assert_ne!(updated["histogram"]["red"], initial["histogram"]["red"]);
        let before = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"before": true}})).unwrap().unwrap();
        assert_eq!(before["histogram"]["source"], "before");
        assert_eq!(before["histogram"]["red"], initial["histogram"]["red"]);
        assert_eq!(before["previewRevision"], updated["previewRevision"]);
        let after = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"before": false, "set": {"exposure": 1}}})).unwrap().unwrap();
        assert_eq!(after["histogram"], updated["histogram"]);
        assert_eq!(after["previewRevision"], updated["previewRevision"]);
        assert_eq!(app.session.active().unwrap().history.past_len(), steps);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        assert_eq!(app.session.active().unwrap().history.past_len(), steps);
        let pixel = app.session.active().unwrap().doc.layer(app.session.active().unwrap().active_layer.unwrap()).unwrap().surface().unwrap().rgba(0, 0);
        assert!((pixel[0] - 128.0 / 255.0).abs() < 0.0001);
    }

    /// Closing Camera Raw's window is Cancel. Without a multi-window backend egui embeds the
    /// "window" (`ViewportClass::EmbeddedWindow`), and the close request comes from the window
    /// it is drawn in; the overlay fallback (`embed_viewports`) keeps the dialog open.
    #[test]
    fn closing_the_camera_raw_window_cancels() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        app.run("file.new", json!({"width": 64, "height": 48})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("edit.fill", json!({"color": "#808080"})).unwrap();
        let steps = app.session.active().unwrap().history.past_len();
        let frame = |app: &mut PhotocraftApp, close: bool| {
            let mut input = egui::RawInput::default();
            if close {
                let info = input.viewports.entry(egui::ViewportId::ROOT).or_default();
                info.events.push(egui::ViewportEvent::Close);
            }
            ctx.run_ui(input, |ui| show(app, ui.ctx())).textures_delta.clear();
        };
        open(&mut app, &ctx).unwrap();
        frame(&mut app, false);
        assert!(app.camera_raw.is_some(), "drawn over the main window");
        ctx.set_embed_viewports(false);
        frame(&mut app, false);
        assert!(app.camera_raw.is_some(), "drawn in its window");
        frame(&mut app, true);
        assert!(app.camera_raw.is_none(), "closing the window is Cancel");
        assert_eq!(app.session.active().unwrap().history.past_len(), steps);
    }

    /// A layer with tonal variation, a selection over part of it and the dialog open on it.
    fn worker_app() -> (PhotocraftApp, egui::Context) {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 64, "height": 48})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("edit.fill", json!({"color": "#406080"})).unwrap();
        app.run("select.rect", json!({"x": 0, "y": 0, "width": 40, "height": 30})).unwrap();
        open(&mut app, &ctx).unwrap();
        (app, ctx)
    }

    fn finish_worker(d: &mut CameraRawDialog, ctx: &egui::Context) {
        for _ in 0..2000 {
            d.poll_render(ctx).unwrap();
            if !d.render_pending() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the Camera Raw preview worker never finished");
    }

    /// The dialog's controls develop the proxy on a worker (#5964): the frame it shows is the one
    /// the UI thread would have developed, and settings changed while it runs are developed next.
    #[test]
    fn worker_preview_matches_the_synchronous_render_and_ends_on_the_latest_settings() {
        let (mut app, ctx) = worker_app();
        let d = app.camera_raw.as_mut().unwrap();
        assert!(d.selection.is_some());
        let revision = d.preview_revision;
        d.params.exposure = 1.0;
        d.dirty = true;
        d.request_render(&ctx);
        assert!(d.render_pending() && !d.dirty);
        // A drag moves on while the first develop runs; the request waits for it.
        d.params.highlights = -60.0;
        d.params.shadows = 45.0;
        d.dirty = true;
        d.request_render(&ctx);
        finish_worker(d, &ctx);
        assert_eq!(d.preview_revision, revision + 1);
        assert!(d.dirty, "the newer settings are still to be developed");
        d.request_render(&ctx);
        finish_worker(d, &ctx);
        assert_eq!(d.preview_revision, revision + 2);
        let (from_worker, histogram) = (d.processed.clone(), d.after_histogram.channels);
        d.render(&ctx);
        assert_eq!(d.processed, from_worker, "the worker develops exactly what the UI thread does");
        assert_eq!(d.after_histogram.channels, histogram);
        assert!(d.describe(&app.ui.camera_raw_scope, &app.ui.camera_raw_preview)["previewPending"] == false);
    }

    /// The control channel renders on its own thread so its reply describes its settings; a worker
    /// still running with older settings must not replace that frame afterwards.
    #[test]
    fn a_stale_worker_never_replaces_a_newer_synchronous_render() {
        let (mut app, ctx) = worker_app();
        let d = app.camera_raw.as_mut().unwrap();
        d.params.highlights = -80.0;
        d.dirty = true;
        d.request_render(&ctx);
        assert!(d.render_pending());
        let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"highlights": 0, "exposure": -1.0}}})).unwrap().unwrap();
        assert_eq!(reply["previewPending"], false);
        let d = app.camera_raw.as_mut().unwrap();
        let (revision, shown) = (d.preview_revision, d.processed.clone());
        std::thread::sleep(std::time::Duration::from_millis(200));
        d.poll_render(&ctx).unwrap();
        assert_eq!((d.preview_revision, &d.processed), (revision, &shown));
        d.render(&ctx);
        assert_eq!(d.processed, shown, "the frame shown is the one for the control channel's settings");
    }

    /// Dragging a slider in the dialog itself never develops on the UI thread: the frame that
    /// takes the change returns with the develop still running, and the preview catches up.
    #[test]
    fn a_dialog_edit_develops_off_the_ui_thread() {
        let (mut app, ctx) = worker_app();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        let frame = |app: &mut PhotocraftApp| {
            ctx.run_ui(egui::RawInput::default(), |ui| show(app, ui.ctx())).textures_delta.clear();
        };
        frame(&mut app);
        let d = app.camera_raw.as_mut().unwrap();
        let revision = d.preview_revision;
        d.params.highlights = -60.0;
        d.dirty = true;
        frame(&mut app);
        let d = app.camera_raw.as_mut().unwrap();
        assert!(d.render_pending() && d.preview_revision == revision, "the frame returned before the develop finished");
        finish_worker(d, &ctx);
        assert_eq!(d.preview_revision, revision + 1);
        assert!(!d.dirty);
        frame(&mut app);
        assert!(!app.ui.status_error, "{}", app.ui.status);
    }

    /// A failed worker develop shows its error until a later frame is published, and never takes
    /// down an error something else reported meanwhile.
    #[test]
    fn a_worker_failure_is_reported_until_the_next_frame() {
        let (mut app, ctx) = worker_app();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        let frame = |app: &mut PhotocraftApp| {
            ctx.run_ui(egui::RawInput::default(), |ui| show(app, ui.ctx())).textures_delta.clear();
        };
        let fail = |app: &mut PhotocraftApp| {
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(Err("Camera Raw preview failed".to_string())).unwrap();
            let d = app.camera_raw.as_mut().unwrap();
            d.pending = Some(rx);
            d.dirty = false;
        };
        let edit = |app: &mut PhotocraftApp, exposure: f32| {
            let d = app.camera_raw.as_mut().unwrap();
            d.params.exposure = exposure;
            d.dirty = true;
        };
        frame(&mut app);
        fail(&mut app);
        frame(&mut app);
        assert!(app.ui.status_error && app.ui.status == "Camera Raw preview failed", "{}", app.ui.status);
        // No new frame yet: the error stays.
        frame(&mut app);
        assert!(app.ui.status_error);
        edit(&mut app, 0.5);
        frame(&mut app);
        finish_worker(app.camera_raw.as_mut().unwrap(), &ctx);
        frame(&mut app);
        assert!(!app.ui.status_error && app.ui.status.is_empty(), "{}", app.ui.status);

        fail(&mut app);
        frame(&mut app);
        app.ui.status = "something else failed".into();
        edit(&mut app, 1.0);
        frame(&mut app);
        finish_worker(app.camera_raw.as_mut().unwrap(), &ctx);
        frame(&mut app);
        assert!(app.ui.status_error && app.ui.status == "something else failed", "{}", app.ui.status);
    }

    #[test]
    fn smart_filter_double_click_edits_the_stored_camera_raw_filter_in_its_own_dialog() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 64, "height": 48})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("edit.fill", json!({"color": "#808080"})).unwrap();
        app.run("layer.smartObjects.convertToSmartObject", json!({})).unwrap();
        // An older editor could save stacked curve inputs; the filter must stay editable.
        let legacy = json!([[0.0, 0.0], [60.0, 40.0], [60.0, 200.0], [255.0, 255.0]]);
        app.run("filter.cameraRaw", json!({"exposure": 1.0, "contrast": 20})).unwrap();
        app.run("layer.smartFilter.setParams", json!({"index": 0, "params": {"pointCurve": legacy}})).unwrap();
        let layer = app.session.active().unwrap().active_layer.unwrap();
        let smart = |app: &PhotocraftApp| match &app.session.active().unwrap().doc.layer(layer).unwrap().content {
            photocraft_doc::LayerContent::Smart(sm) => sm.clone(),
            _ => panic!("not a smart object"),
        };
        let steps = app.session.active().unwrap().history.past_len();

        let sm = smart(&app);
        crate::smart_ui::open_editor(&mut app, &ctx, layer.0, &sm, 0);
        assert!(app.ui.dialogs.is_empty(), "no generic parameter form");
        let opened = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {}})).unwrap().unwrap();
        assert_eq!(opened["smartFilter"], 0);
        assert_eq!(opened["params"]["exposure"], 1.0);
        assert_eq!(opened["params"]["pointCurve"], legacy);
        // The preview starts from the pixels below the filter, not the filtered cache.
        let before = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"before": true}})).unwrap().unwrap();
        assert_eq!(before["histogram"]["red"][128], 64 * 48);

        // Resetting a control to its default must overwrite the stored value.
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.0}, "before": false, "commit": true}})).unwrap();
        assert!(r.is_ok(), "{r:?}");
        assert!(app.camera_raw.is_none());
        let sm = smart(&app);
        assert_eq!(sm.smart_filters.len(), 1, "edited in place, not added");
        assert_eq!(sm.smart_filters[0].params["exposure"], 0.0);
        assert_eq!(sm.smart_filters[0].params["contrast"], 20.0);
        assert_eq!(app.session.active().unwrap().history.past_len(), steps + 1);
        app.run("edit.undo", json!({})).unwrap();
        assert_eq!(smart(&app).smart_filters[0].params["exposure"], 1.0);

        // A broken new curve is still rejected; an unknown index is an error, not a panic.
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"smartFilter": {"layer": layer.0, "index": 0}})).unwrap().unwrap();
        assert!(menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"pointCurve": [[10, 10]]}}})).unwrap().is_err());
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        assert!(menu(&mut app, &ctx, "filter.cameraRaw", &json!({"smartFilter": {"layer": layer.0, "index": 7}})).unwrap().is_err());
        assert!(menu(&mut app, &ctx, "filter.cameraRaw", &json!({"smartFilter": {"layer": "x"}})).unwrap().is_err());
    }

    #[test]
    fn distant_sparse_pixels_cannot_trigger_an_unbounded_proxy_scan() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width":64,"height":48})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        let layer = app.session.active().unwrap().active_layer.unwrap();
        let mut source = photocraft_raster::Surface::new(photocraft_color::PixelFormat::RGBA8);
        source.write_region(Rect::new(30_000, 30_000, 30_001, 30_001), &[0.5, 0.5, 0.5, 1.0]);
        let result = open_pixels(&mut app, &ctx, layer, source, CameraRaw::default(), Coverage::None, Target::NewFilter);
        assert!(result.unwrap_err().contains("bounds are too large"));
        assert!(app.camera_raw.is_none());
    }

    #[test]
    fn dialog_drives_the_engine() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 64, "height": 48})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("edit.fill", json!({"color": "#808080"})).unwrap();
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({})).unwrap().unwrap();
        assert_eq!(r["proxy"], json!([64, 48]));
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.0, "vignetteAmount": -40}}})).unwrap().unwrap();
        assert_eq!(r["params"]["exposure"], 1.0);
        assert_eq!(app.camera_raw.as_ref().unwrap().command_params(), json!({"exposure": 1.0, "vignetteAmount": -40.0}));
        let before = app.session.active().unwrap().doc.layer(app.session.active().unwrap().active_layer.unwrap()).unwrap().surface().unwrap().rgba(32, 24);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert!(app.camera_raw.is_none());
        let after = app.session.active().unwrap().doc.layer(app.session.active().unwrap().active_layer.unwrap()).unwrap().surface().unwrap().rgba(32, 24);
        assert!(after[0] > before[0] + 0.1, "{before:?} → {after:?}");
        // Cancel leaves the document alone.
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({})).unwrap().unwrap();
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        assert_eq!(r["cancelled"], true);
        assert!(menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": "x"}}})).unwrap().is_err());
    }

    /// An app whose importer is the real one, opening a synthetic DNG.
    fn raw_app() -> (PhotocraftApp, Vec<u8>) {
        use photocraft_raw::testgen::{DngSpec, mosaic, scene};
        let services = crate::Services {
            import: Some(Box::new(|name: &str, b: &[u8], depth: usize| {
                photocraft_io::import_with_svg_group_depth(name, b, depth).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            ..Default::default()
        };
        let (w, h) = (48, 32);
        let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
        spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
        (PhotocraftApp::new(photocraft_engine::Session::new(), services), spec.build())
    }

    fn mean_red(app: &PhotocraftApp) -> f32 {
        let st = app.session.active().unwrap();
        let s = st.doc.layer(st.active_layer.unwrap()).unwrap().surface().unwrap();
        (0..8).map(|i| s.rgba(4 + i * 4, 16)[0]).sum::<f32>() / 8.0
    }

    #[test]
    fn raw_opens_in_camera_raw_and_open_redevelops() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        let warnings = app.open_bytes("IMG_0001.dng", &dng).unwrap();
        assert!(warnings.iter().any(|w| w.contains(photocraft_io::raw::DEVELOPED_NOTE)), "{warnings:?}");
        // A raw has no embedded profile (ProPhoto RGB is the space it is developed into), so Ask
        // When Opening (on by default) has nothing to ask about.
        assert!(app.ui.dialogs.is_empty(), "no profile prompt for a raw");
        let raw = app.pending_raw_open.take().expect("a raw open queues the dialog");
        open_raw(&mut app, &ctx, raw).unwrap();
        let d = app.camera_raw.as_ref().unwrap();
        assert_eq!(d.target, Target::OpenRaw);
        assert_eq!(d.describe(&app.ui.camera_raw_scope, &app.ui.camera_raw_preview)["openingRaw"]["name"], "IMG_0001.dng");
        let before = mean_red(&app);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.0, "temperature": 30, "contrast": 20}}})).unwrap().unwrap();
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(r["redeveloped"], true, "{r}");
        assert!(r.get("filter").is_some(), "contrast is applied as a Camera Raw step: {r}");
        assert!(app.camera_raw.is_none());
        assert_eq!(app.session.documents().len(), 1);
        let st = app.session.active().unwrap();
        assert_eq!(st.saved_revision, st.revision, "Open leaves the document unmodified");
        assert_eq!(st.doc.depth, photocraft_color::SampleType::U16);
        assert!(mean_red(&app) > before + 0.05, "{before} → {}", mean_red(&app));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_raw_open_reads_adjacent_xmp_and_commits_imported_tonal_values() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let raw_path = std::env::temp_dir().join(format!("photocraft-raw-xmp-{}-{unique}.dng", std::process::id()));
        let xmp_path = raw_path.with_extension("xmp");
        let packet = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:RDF><rdf:Description xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" crs:Exposure2012=\"0.75\" crs:Contrast2012=\"18\" crs:Highlights2012=\"-35\" crs:Shadows2012=\"12\" crs:Whites2012=\"5\" crs:Blacks2012=\"-8\"/></rdf:RDF></x:xmpmeta>";
        std::fs::write(&xmp_path, packet).unwrap();
        let original = std::fs::read(&xmp_path).unwrap();
        let before_modified = std::fs::metadata(&xmp_path).unwrap().modified().unwrap();

        app.open_bytes("example.dng", &dng).unwrap();
        let mut raw = app.pending_raw_open.take().unwrap();
        raw.path = Some(raw_path.to_string_lossy().into_owned());
        open_raw(&mut app, &ctx, raw).unwrap();
        let dialog = app.camera_raw.as_ref().unwrap();
        assert!((dialog.params.exposure - 0.75).abs() < 0.00001);
        assert_eq!(
            (dialog.params.contrast, dialog.params.highlights, dialog.params.shadows, dialog.params.whites, dialog.params.blacks),
            (18.0, -35.0, 12.0, 5.0, -8.0)
        );
        assert_ne!(dialog.proxy.as_ref(), &dialog.processed, "the imported adjustments affect the preview pixels");
        assert!(app.ui.status.contains("imported 6 Adobe XMP tonal settings"));
        let result = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(result["redeveloped"], true, "exposure must reach the sensor re-develop stage: {result}");
        assert!(result.get("filter").is_some(), "non-exposure settings must reach the filter engine");
        assert_eq!(std::fs::read(&xmp_path).unwrap(), original);
        assert_eq!(std::fs::metadata(&xmp_path).unwrap().modified().unwrap(), before_modified);
        std::fs::remove_file(&xmp_path).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_raw_open_maps_simple_child_xmp_without_modifying_sidecar() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let raw_path = std::env::temp_dir().join(format!("photocraft-raw-xmp-child-{}-{unique}.dng", std::process::id()));
        let xmp_path = raw_path.with_extension("xmp");
        let packet = b"<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"><rdf:Description><crs:Exposure2012>0.625</crs:Exposure2012><crs:Contrast2012>12</crs:Contrast2012></rdf:Description></rdf:RDF>";
        std::fs::write(&xmp_path, packet).unwrap();
        let before_modified = std::fs::metadata(&xmp_path).unwrap().modified().unwrap();

        app.open_bytes("example.dng", &dng).unwrap();
        let mut raw = app.pending_raw_open.take().unwrap();
        raw.path = Some(raw_path.to_string_lossy().into_owned());
        open_raw(&mut app, &ctx, raw).unwrap();

        let dialog = app.camera_raw.as_ref().unwrap();
        assert_eq!(dialog.params.exposure, 0.625);
        assert_eq!(dialog.params.contrast, 12.0);
        assert!(app.ui.status.contains("imported 2 Adobe XMP tonal settings"));
        assert_eq!(std::fs::read(&xmp_path).unwrap(), packet.to_vec());
        assert_eq!(std::fs::metadata(&xmp_path).unwrap().modified().unwrap(), before_modified);
        std::fs::remove_file(&xmp_path).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn missing_or_malformed_sidecar_does_not_block_raw_open() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("photocraft-raw-xmp-bad-{}-{unique}.dng", std::process::id()));
        app.open_bytes("example.dng", &dng).unwrap();
        let mut raw = app.pending_raw_open.take().unwrap();
        raw.path = Some(path.to_string_lossy().into_owned());
        open_raw(&mut app, &ctx, raw).unwrap();
        let opened = &app.camera_raw.as_ref().unwrap().params;
        assert_eq!(*opened, CameraRaw { encoding_gamma: opened.encoding_gamma, ..CameraRaw::default() }, "defaults apart from the display encoding");
        cancel(&mut app);

        let xmp = path.with_extension("xmp");
        std::fs::write(&xmp, b"<broken>").unwrap();
        app.open_bytes("example.dng", &dng).unwrap();
        let mut raw = app.pending_raw_open.take().unwrap();
        raw.path = Some(path.to_string_lossy().into_owned());
        open_raw(&mut app, &ctx, raw).unwrap();
        let opened = &app.camera_raw.as_ref().unwrap().params;
        assert_eq!(*opened, CameraRaw { encoding_gamma: opened.encoding_gamma, ..CameraRaw::default() }, "defaults apart from the display encoding");
        assert!(app.ui.status.contains("skipped XMP"));
        std::fs::remove_file(xmp).unwrap();
    }

    /// Lab port (D-14): on main a developed RAW never raises an Embedded Profile Mismatch prompt (Ask When Opening asks
    /// before a file opens, and a developed raw has no embedded profile to ask about), so R2b's prompt-ownership tests
    /// become: no prompt is raised for the batch, and Cancel still discards the entire batch.
    #[test]
    fn raw_cancel_discards_the_whole_batch_and_raws_raise_no_profile_prompt() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("first.dng", &dng).unwrap();
        app.open_bytes("second.dng", &dng).unwrap();
        assert!(app.pending_raw_open.is_some() && app.queued_raw_opens.len() == 1, "both RAWs wait for review");
        assert!(app.ui.dialogs.is_empty(), "a developed RAW raises no profile prompt: {:?}", app.ui.dialogs.iter().map(|d| &d.fields).collect::<Vec<_>>());
        let first = take_next_raw_open(&mut app).unwrap();
        open_raw(&mut app, &ctx, first).unwrap();
        cancel(&mut app);
        assert!(app.session.documents().is_empty(), "Cancel must discard the entire batch");
        assert!(app.ui.dialogs.is_empty());
    }

    /// Lab port (D-14): accepting a RAW leaves no profile question behind (main never asks one for a developed RAW).
    #[test]
    fn accepting_raw_raises_no_profile_prompt() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("accepted.dng", &dng).unwrap();
        let raw = take_next_raw_open(&mut app).unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        let response = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}}));
        assert!(response.as_ref().is_some_and(Result::is_ok), "accepting the RAW should succeed: {response:?}");
        assert!(app.ui.dialogs.is_empty(), "no profile prompt after Open");
        assert_eq!(app.session.documents().len(), 1);
    }

    #[test]
    fn raw_review_targets_explicit_doc_id_even_when_another_tab_is_active() {
        let (mut app, _) = raw_app();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        app.run("file.new", json!({"width": 12, "height": 12})).unwrap();
        let first = app.session.documents()[0].doc.id;
        let second = app.session.documents()[1].doc.id;
        assert_ne!(first, second);
        app.session.set_active(0);
        queue_open_dialog(&mut app, second, "second.dng", None, None).unwrap();
        assert_eq!(app.session.active().unwrap().doc.id, first, "queueing must not switch tabs");
        assert_eq!(app.pending_raw_open.as_ref().map(|raw| raw.document), Some(second), "the queue must not capture the active tab");
        assert_eq!(take_next_raw_open(&mut app).unwrap().document, second);
    }

    #[test]
    fn missing_doc_cannot_be_queued_as_a_raw_review() {
        let (mut app, _) = raw_app();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let error = queue_open_dialog(&mut app, photocraft_doc::DocId(u64::MAX), "missing.dng", None, None).unwrap_err();
        assert!(error.contains("missing"));
        assert!(app.pending_raw_open.is_none());
        assert!(app.queued_raw_opens.is_empty());
    }

    #[test]
    fn batch_raw_cancel_closes_every_unaccepted_raw_but_keeps_other_tabs() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let original = app.session.documents()[0].doc.id;
        for name in ["first.dng", "second.dng", "third.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        assert_eq!(app.session.documents().len(), 4);
        assert_eq!(app.queued_raw_opens.len(), 2);
        start_next_raw_review(&mut app, &ctx);
        let third = app.queued_raw_opens[1].document;
        select_raw_review(&mut app, &ctx, third).unwrap();
        assert!(blocks_editor(&app) && hides_unaccepted_raw(&app));
        cancel(&mut app);
        assert_eq!(app.session.documents().len(), 1, "only unrelated documents remain");
        assert_eq!(app.session.documents()[0].doc.id, original);
        assert!(app.camera_raw.is_none());
        assert!(app.pending_raw_open.is_none());
        assert!(app.queued_raw_opens.is_empty());
        assert!(app.raw_filmstrip.parked.is_empty());
        assert!(!blocks_editor(&app) && !hides_unaccepted_raw(&app));
        start_next_raw_review(&mut app, &ctx);
        assert!(app.camera_raw.is_none(), "Cancel must never reopen another photo");
    }

    #[test]
    fn filmstrip_selects_real_queued_documents_and_preserves_each_photos_controls() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        for name in ["one.dng", "two.dng", "three.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let first = app.pending_raw_open.as_ref().unwrap().document;
        let second = app.queued_raw_opens[0].document;
        let third = app.queued_raw_opens[1].document;
        assert_eq!(filmstrip_raws(&app).iter().map(|raw| raw.document).collect::<Vec<_>>(), vec![first, second, third]);
        start_next_raw_review(&mut app, &ctx);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.25, "contrast": 18}}})).unwrap().unwrap();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"selectRaw": second.0}})).unwrap().unwrap();
        assert_eq!(app.camera_raw.as_ref().unwrap().raw_open.as_ref().unwrap().document, second);
        assert_eq!(app.camera_raw.as_ref().unwrap().params.exposure, 0.0, "settings from another photo must not leak");
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"contrast": -33}}})).unwrap().unwrap();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"selectRaw": third.0}})).unwrap().unwrap();
        assert_eq!(app.camera_raw.as_ref().unwrap().params.contrast, 0.0);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"selectRaw": first.0}})).unwrap().unwrap();
        assert_eq!(app.camera_raw.as_ref().unwrap().params.exposure, 1.25);
        assert_eq!(app.camera_raw.as_ref().unwrap().params.contrast, 18.0);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"selectRaw": second.0}})).unwrap().unwrap();
        assert_eq!(app.camera_raw.as_ref().unwrap().params.contrast, -33.0);
        let ids = filmstrip_raws(&app).iter().map(|raw| raw.document).collect::<Vec<_>>();
        assert_eq!(ids, vec![first, second, third]);
        cancel(&mut app);
        assert!(document_index(&app, second).is_none());
        assert_eq!(filmstrip_raws(&app).len(), 0, "Cancel discards all the RAW reviews");
        assert!(!blocks_editor(&app) && !hides_unaccepted_raw(&app));
    }

    #[test]
    fn filmstrip_open_images_commits_selected_picker_batch_without_reopening_dialog() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        for name in ["first.dng", "second.dng", "third.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let first = app.pending_raw_open.as_ref().unwrap().document;
        let second = app.queued_raw_opens[0].document;
        let third = app.queued_raw_opens[1].document;
        start_next_raw_review(&mut app, &ctx);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"contrast": 19}}})).unwrap().unwrap();
        select_raw_review(&mut app, &ctx, third).unwrap();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.75}}})).unwrap().unwrap();
        assert!(app.raw_filmstrip.parked.iter().any(|r| r.raw.document == first));
        let result = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(result["batch"], 3, "Open Images must accept the whole picker batch: {result}");
        assert!(app.camera_raw.is_none());
        for _ in 0..5 {
            advance_batch_open(&mut app, &ctx);
            if !app.raw_filmstrip.batch_open_all {
                break;
            }
        }
        assert!(!app.raw_filmstrip.batch_open_all, "the batch must finish without user clicks");
        assert!(!blocks_editor(&app) && !hides_unaccepted_raw(&app));
        assert_eq!(app.session.documents().len(), 3);
        assert_eq!(app.session.documents()[1].doc.id, second);
        assert!(document_index(&app, third).is_none(), "the raw stage should re-develop the edited third photo");
        assert!(app.session.documents().iter().all(|st| st.saved_revision == st.revision));
        assert!(app.pending_raw_open.is_none());
        assert!(app.queued_raw_opens.is_empty());
        assert!(app.raw_filmstrip.parked.is_empty());
        start_next_raw_review(&mut app, &ctx);
        assert!(app.camera_raw.is_none(), "Open Images must not return to Camera Raw");
    }

    #[test]
    fn filmstrip_rejects_stale_or_conflicting_actions_without_mutation() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        for name in ["one.dng", "two.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let first = app.pending_raw_open.as_ref().unwrap().document;
        let duplicate = queue_open_dialog(&mut app, first, "duplicate.dng", None, None).unwrap_err();
        assert!(duplicate.contains("already"));
        start_next_raw_review(&mut app, &ctx);
        let second = app.queued_raw_opens[0].document;
        let missing = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"selectRaw": u64::MAX}})).unwrap();
        assert!(missing.is_err());
        let mixed = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"selectRaw": second.0, "commit": true}})).unwrap();
        assert!(mixed.is_err());
        assert_eq!(app.camera_raw.as_ref().unwrap().raw_open.as_ref().unwrap().document, first);
        assert_eq!(app.queued_raw_opens.len(), 1);
        assert!(app.raw_filmstrip.parked.is_empty());
        assert_eq!(app.session.documents().len(), 2);
    }

    #[test]
    fn cancelling_after_switching_discards_parked_edits_and_all_batch_documents() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("first.dng", &dng).unwrap();
        app.open_bytes("second.dng", &dng).unwrap();
        let second = app.queued_raw_opens[0].document;
        start_next_raw_review(&mut app, &ctx);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 2}}})).unwrap().unwrap();
        select_raw_review(&mut app, &ctx, second).unwrap();
        assert_eq!(app.raw_filmstrip.parked.len(), 1);
        cancel(&mut app);
        assert!(app.session.documents().is_empty());
        assert!(app.camera_raw.is_none());
        start_next_raw_review(&mut app, &ctx);
        assert!(app.camera_raw.is_none());
        assert!(app.raw_filmstrip.parked.is_empty());
    }

    #[test]
    fn each_raw_in_batch_starts_with_independent_properties_scroll_state() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        for name in ["first.dng", "second.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let first = take_next_raw_open(&mut app).unwrap();
        open_raw(&mut app, &ctx, first).unwrap();
        let first_salt = properties_scroll_salt(app.camera_raw.as_ref().unwrap());
        let second = app.queued_raw_opens.front().unwrap().document;
        select_raw_review(&mut app, &ctx, second).unwrap();
        let second_salt = properties_scroll_salt(app.camera_raw.as_ref().unwrap());
        assert_ne!(first_salt, second_salt, "each RAW needs a fresh ScrollArea identity");
    }

    #[test]
    fn batch_raw_waits_for_prior_background_redevelop() {
        let (mut app, dng) = raw_app();
        app.open_bytes("first.dng", &dng).unwrap();
        app.open_bytes("second.dng", &dng).unwrap();
        let first = app.pending_raw_open.as_ref().unwrap().clone();
        app.raw_redevelop = Some(Redevelop { job: photocraft_engine::jobs::JobId(7), params: CameraRaw::default(), raw: first, filter_step: false });
        assert!(take_next_raw_open(&mut app).is_none(), "a second develop must not replace the active job");
        assert_eq!(app.queued_raw_opens.len(), 1);
        assert!(blocks_editor(&app));
        app.raw_redevelop = None;
        assert_eq!(take_next_raw_open(&mut app).unwrap().name, "first.dng");
        assert_eq!(take_next_raw_open(&mut app).unwrap().name, "second.dng");
    }

    /// Settings whose Camera Raw filter step fails validation (the curve inputs do not increase by a level), after the
    /// re-develop has already replaced the document.
    fn failing_step_params() -> CameraRaw {
        CameraRaw { exposure: 1.0, point_curve: vec![[10.0, 10.0], [10.5, 20.0]], ..CameraRaw::default() }
    }

    /// P0 Done-then-Err, synchronous path: the re-develop landed (a new document id) and the Camera Raw step then fails.
    /// The photograph must stay unaccepted and come back for review; it must never become an ordinary editable tab.
    #[test]
    fn p0_done_then_err_sync_leaves_the_replacement_unaccepted_and_recoverable() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("held.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        let result = commit_open_raw(&mut app, &failing_step_params(), &raw);
        assert!(result.is_err(), "the Camera Raw step failed: {result:?}");
        let replacement = app.session.active().unwrap().doc.id;
        assert_ne!(replacement, raw.document, "the re-develop had landed under a new id");
        sync_provisional(&mut app);
        assert!(unaccepted_documents(&app).contains(&replacement), "the replacement is still unaccepted");
        assert!(app.session.provisional_documents().contains(&replacement));
        app.camera_raw = None;
        start_next_raw_review(&mut app, &ctx);
        assert_eq!(app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.document), Some(replacement), "its review is back");
    }

    /// P0 Done-then-Err, background path: the job's Done event arrives, the step fails.
    #[test]
    fn p0_done_then_err_async_leaves_the_replacement_unaccepted_and_recoverable() {
        use photocraft_engine::jobs::{JobEvent, JobId, JobOutcome};
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("held.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        let tuning = photocraft_io::raw::RawTuning { exposure: 1.0, ..Default::default() };
        let (developed, wb) = photocraft_io::raw::import_raw_tuned("held.dng", &dng, &tuning).unwrap();
        let generation = app.session.review_generation();
        let reply = replace_raw_document(&mut app.session, raw.document, developed, wb, generation).unwrap();
        let replacement = app.session.active().unwrap().doc.id;
        app.raw_redevelop = Some(Redevelop { job: JobId(5), params: failing_step_params(), raw, filter_step: false });
        let event = JobEvent { id: JobId(5), command: REDEVELOP_JOB.into(), label: "x".into(), document: None, outcome: JobOutcome::Done(reply) };
        assert!(on_redevelop_event(&mut app, &event));
        assert!(unaccepted_documents(&app).contains(&replacement), "the replacement is still unaccepted");
        assert!(app.session.provisional_documents().contains(&replacement));
        start_next_raw_review(&mut app, &ctx);
        assert_eq!(app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.document), Some(replacement), "its review is back");
    }

    /// S2-4: a popped RAW whose review cannot be built is not released as an ordinary document. It stays held and is
    /// retried; after three failures it is closed (never accepted) and the person is told.
    #[test]
    fn s2_4_a_raw_that_will_not_open_in_review_stays_held_then_is_closed_with_a_notice() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("stuck.dng", &dng).unwrap();
        let id = app.pending_raw_open.as_ref().unwrap().document;
        let index = document_index(&app, id).unwrap();
        app.session.set_active(index);
        if let Some(st) = app.session.active_mut() {
            st.active_layer = None;
        }
        start_next_raw_review(&mut app, &ctx);
        assert!(app.camera_raw.is_none());
        assert!(unaccepted_documents(&app).contains(&id), "after the first failure it is still held, not an ordinary tab");
        for _ in 0..4 {
            start_next_raw_review(&mut app, &ctx);
        }
        assert!(document_index(&app, id).is_none(), "after repeated failures it is closed, not accepted");
        assert!(unaccepted_documents(&app).is_empty() && app.session.provisional_documents().is_empty());
        assert!(app.ui.status_error, "the person is told: {}", app.ui.status);
    }

    #[derive(Debug)]
    struct FakeDrop {
        path: std::path::PathBuf,
        bytes: Vec<u8>,
    }

    impl egui::DroppedFile for FakeDrop {
        fn path(&self) -> &std::path::Path {
            &self.path
        }
        #[cfg(not(target_arch = "wasm32"))]
        fn bytes(&self) -> Result<Vec<u8>, String> {
            Ok(self.bytes.clone())
        }
    }

    /// S2-5: a canvas drop queued before the RAW arrived must not be placed onto the waiting RAW by a later frame.
    #[test]
    fn s2_5_a_queued_canvas_drop_is_not_placed_onto_a_waiting_raw() {
        use base64::Engine as _;
        let (_, dng) = raw_app();
        let png = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
            .unwrap();
        let mut h = egui_kittest::Harness::builder().with_size(WINDOW).build_eframe(|cc| {
            PhotocraftApp::setup_context(&cc.egui_ctx, crate::theme::ThemeKind::Pro);
            raw_app().0
        });
        h.run_steps(2);
        h.state_mut().open_bytes("held.dng", &dng).unwrap();
        h.run_steps(4);
        let held = h.state().session.active().unwrap().doc.id;
        assert!(unaccepted_documents(h.state()).contains(&held), "the RAW is waiting in Camera Raw");
        let before = h.state().session.active().map(|st| (st.doc.layer_count(), st.revision));
        h.state_mut().drop_places.push_back(crate::file_open::DropPlace::File(std::sync::Arc::new(FakeDrop { path: "logo.png".into(), bytes: png })));
        h.run_steps(4);
        let after = h.state().session.active().map(|st| (st.doc.layer_count(), st.revision));
        assert_eq!(after, before, "the waiting RAW was not edited by a queued drop");
    }
    /// Done-then-Err through the dialog's own Open (synchronous path): the dialog that pointed at the replaced document
    /// is dropped, the replacement is recovered, and its review comes back with the settings still to apply.
    #[test]
    fn p0_done_then_err_through_open_drops_the_stale_dialog_and_brings_the_review_back() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("held.dng", &dng).unwrap();
        let raw = take_next_raw_open(&mut app).unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        app.camera_raw.as_mut().unwrap().params = failing_step_params();
        assert!(commit(&mut app).is_err());
        assert!(app.camera_raw.is_none(), "no dialog is left pointing at the closed document");
        let replacement = app.session.active().unwrap().doc.id;
        assert!(unaccepted_documents(&app).contains(&replacement) && app.session.provisional_documents().contains(&replacement));
        start_next_raw_review(&mut app, &ctx);
        let dialog = app.camera_raw.as_ref().expect("the review is back");
        assert_eq!(dialog.raw_open.as_ref().map(|r| r.document), Some(replacement));
        assert_eq!(dialog.params.point_curve, failing_step_params().point_curve, "the settings still to apply are kept");
    }

    /// A re-develop result of a review that was cancelled meanwhile replaces nothing: the document and its id stay.
    #[test]
    fn a_stale_redevelop_result_replaces_nothing() {
        let (mut app, dng) = raw_app();
        app.open_bytes("held.dng", &dng).unwrap();
        let old = app.session.active().unwrap().doc.id;
        let generation = app.session.review_generation();
        let tuning = photocraft_io::raw::RawTuning { exposure: 1.0, ..Default::default() };
        let (developed, wb) = photocraft_io::raw::import_raw_tuned("held.dng", &dng, &tuning).unwrap();
        app.session.next_review_generation();
        let result = replace_raw_document(&mut app.session, old, developed, wb, generation);
        assert!(matches!(result, Err(photocraft_engine::EngineError::Cancelled)));
        assert_eq!((app.session.documents().len(), app.session.active().unwrap().doc.id), (1, old));
    }

    /// An open whose result was applied (the document is in the session) before the review was cancelled, and whose
    /// event the shell has not handled yet, must not queue a review or keep the document afterwards.
    #[test]
    fn p0_a_review_cancelled_between_apply_and_event_admits_no_photograph() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.background_jobs = true;
        app.open_bytes("late.dng", &dng).unwrap();
        let job = app.jobs.opens[0].job;
        app.session.wait_job(job).unwrap();
        let id = app.session.documents()[0].doc.id;
        assert!(app.session.has_provisional(), "held by the job layer from the moment it was applied");
        assert!(unaccepted_documents(&app).is_empty(), "the shell has not seen it yet");
        // The photographer cancels the review (this is what Cancel does first) before the event is handled.
        app.session.next_review_generation();
        crate::jobs_ui::tick(&mut app, &ctx);
        sync_provisional(&mut app); // the frame does this right after the jobs tick
        assert!(app.session.documents().is_empty(), "the late photograph is closed, never accepted");
        assert!(app.pending_raw_open.is_none() && app.queued_raw_opens.is_empty() && !app.session.has_provisional());
        // Control: without a cancel the same sequence queues the review.
        app.open_bytes("kept.dng", &dng).unwrap();
        let job = app.jobs.opens[0].job;
        app.session.wait_job(job).unwrap();
        crate::jobs_ui::tick(&mut app, &ctx);
        assert_eq!(app.session.documents().len(), 1);
        let kept = app.session.documents()[0].doc.id;
        assert_ne!(kept, id);
        assert!(unaccepted_documents(&app).contains(&kept) && app.session.provisional_documents().contains(&kept));
        assert!(!app.session.admission_stale(kept), "released to the shell after queueing");
    }
    fn psd_bytes() -> Vec<u8> {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 16, "height": 12})).unwrap();
        let doc = (*s.active().unwrap().doc).clone();
        photocraft_io::export(&doc, "x.psd", &Default::default()).unwrap().bytes
    }

    /// The photographs waiting in the review queue (the one up next, then the rest).
    fn queued_ids(app: &PhotocraftApp) -> Vec<photocraft_doc::DocId> {
        app.pending_raw_open.iter().chain(app.queued_raw_opens.iter()).map(|raw| raw.document).collect()
    }

    /// P0C F-01: Cancelling a Camera Raw review leaves an unrelated non-RAW open that is still decoding alone.
    #[test]
    fn p0c_cancel_leaves_an_in_flight_psd_open_alone() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("review.dng", &dng).unwrap();
        let raw = take_next_raw_open(&mut app).unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        app.background_jobs = true;
        app.open_bytes("other.psd", &psd_bytes()).unwrap();
        let job = app.jobs.opens[0].job;
        cancel_review(&mut app);
        let _ = app.session.wait_job(job);
        crate::jobs_ui::tick(&mut app, &ctx);
        sync_provisional(&mut app);
        assert_eq!(app.session.documents().len(), 1, "the PSD opened as a normal document; the RAW review is gone");
        assert_eq!(app.session.documents()[0].doc.name, "other.psd");
        assert!(queued_ids(&app).is_empty() && !app.session.has_provisional(), "the PSD is an ordinary document, never held");
    }

    /// P0C F-02: delayed open events find their photograph by its document id, not by a tab index that an earlier
    /// event in the same batch (a drop onto a tab slot) has moved.
    #[test]
    fn p0c_events_resolve_by_document_id_after_a_tab_move() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let accepted = app.session.active().unwrap().doc.id;
        app.background_jobs = true;
        app.open_bytes("a.dng", &dng).unwrap();
        app.open_bytes("b.dng", &dng).unwrap();
        app.jobs.opens[0].slot = Some(99);
        let jobs: Vec<_> = app.jobs.opens.iter().map(|tab| tab.job).collect();
        for job in jobs {
            app.session.wait_job(job).unwrap();
        }
        let (a, b) = (app.session.documents()[1].doc.id, app.session.documents()[2].doc.id);
        crate::jobs_ui::tick(&mut app, &ctx);
        let mut queued = queued_ids(&app);
        queued.sort_by_key(|id| id.0);
        let mut expected = vec![a, b];
        expected.sort_by_key(|id| id.0);
        assert_eq!(queued, expected, "each RAW is queued under its own id, once");
        assert!(!queued.contains(&accepted), "the ordinary document is never queued");
    }

    /// P0C F-02: a review cancelled after three opens were applied closes exactly the two RAWs. Closing the first
    /// must not shift the second's event onto the ordinary document behind them.
    #[test]
    fn p0c_stale_cancel_of_two_raws_closes_exactly_those_two() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.background_jobs = true;
        app.open_bytes("a.dng", &dng).unwrap();
        app.open_bytes("b.dng", &dng).unwrap();
        app.open_bytes("p.psd", &psd_bytes()).unwrap();
        let jobs: Vec<_> = app.jobs.opens.iter().map(|tab| tab.job).collect();
        for job in jobs {
            app.session.wait_job(job).unwrap();
        }
        let ordinary = app.session.documents()[2].doc.id;
        app.session.next_review_generation();
        crate::jobs_ui::tick(&mut app, &ctx);
        sync_provisional(&mut app);
        let left: Vec<_> = app.session.documents().iter().map(|st| st.doc.id).collect();
        assert_eq!(left, vec![ordinary], "only the ordinary document remains");
        assert!(queued_ids(&app).is_empty() && !app.session.has_provisional(), "nothing is queued or held");
    }
    fn jpeg_bytes() -> Vec<u8> {
        let mut s = photocraft_engine::Session::new();
        s.execute("file.new", json!({"width": 16, "height": 12})).unwrap();
        let doc = (*s.active().unwrap().doc).clone();
        photocraft_io::export(&doc, "x.jpg", &Default::default()).unwrap().bytes
    }

    /// A finished fake open job whose result is exactly `result`, delivered to the shell as an open event.
    fn deliver_open_result(app: &mut PhotocraftApp, ctx: &egui::Context, name: &str, result: Value) {
        let started =
            app.session.start_job(photocraft_engine::jobs::OPEN_JOB, json!({"name": name}), "Opening", false, |_| Ok(()), move |_s, ()| Ok(result)).unwrap();
        let photocraft_engine::jobs::Started::Job(job) = started else { panic!("the fake open ran inline") };
        app.jobs.opens.push(crate::jobs_ui::OpenTab { job, name: name.to_string(), path: None, slot: None, order: None });
        app.session.wait_job(job).unwrap();
        crate::jobs_ui::tick(app, ctx);
    }

    /// P0D: an open result without a usable `docId` never falls back to the tab index, which may now name another
    /// document: an accepted tab must not be queued for Camera Raw, and the waiting RAW is not disturbed.
    #[test]
    fn p0d_an_open_result_without_a_usable_doc_id_acts_on_no_tab() {
        for (label, extra) in [("missing", json!({})), ("not a number", json!({"docId": "x"})), ("a closed document", json!({"docId": 987654321u64}))] {
            let (mut app, dng) = raw_app();
            let ctx = egui::Context::default();
            app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
            let accepted = app.session.documents()[0].doc.id;
            app.open_bytes("waiting.dng", &dng).unwrap();
            let waiting = app.session.documents()[1].doc.id;
            let revision = app.session.documents()[0].revision;
            let mut result = json!({"document": 0, "warnings": [photocraft_io::raw::DEVELOPED_NOTE]});
            result.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            deliver_open_result(&mut app, &ctx, "stray.dng", result);
            sync_provisional(&mut app);
            assert_eq!(queued_ids(&app), vec![waiting], "{label}: only the waiting RAW is queued, not the accepted tab");
            assert!(!app.session.provisional_documents().contains(&accepted), "{label}");
            assert_eq!((app.session.documents().len(), app.session.documents()[0].revision), (2, revision), "{label}: nothing changed");
            assert!(app.ui.status_error, "{label}: the open is reported as failed: {}", app.ui.status);
        }
    }

    /// P0D: an automation open of a RAW is held without reading an adjacent sidecar: no path is kept, the bytes are.
    #[test]
    fn p0d_an_automation_open_of_a_raw_is_held_without_a_sidecar_path() {
        let (mut app, dng) = raw_app();
        let warnings = app.open_automation_bytes("shot.dng", &dng).unwrap();
        let raw = app.pending_raw_open.as_ref().expect("queued for Camera Raw");
        assert_eq!(raw.document, app.session.active().unwrap().doc.id);
        assert!(raw.path.is_none(), "no path: nothing to read an adjacent .xmp from");
        assert!(raw.bytes.is_some(), "the file is re-developed from its own bytes");
        assert!(app.session.provisional_documents().contains(&raw.document));
        assert!(warnings.iter().any(|w| w.contains("default")), "the person is told the sidecar was not used: {warnings:?}");
    }

    /// P0D: when the review cannot be queued, only the document just added is closed and the open fails.
    #[test]
    fn p0d_a_failed_review_queue_closes_only_the_new_document() {
        let fixed = photocraft_doc::DocId::fresh();
        let services = crate::Services {
            import: Some(Box::new(move |name: &str, b: &[u8], depth: usize| {
                photocraft_io::import_with_svg_group_depth(name, b, depth)
                    .map(|mut r| {
                        r.document.id = fixed;
                        (r.document, r.warnings)
                    })
                    .map_err(|e| e.to_string())
            })),
            ..Default::default()
        };
        let (_, dng) = raw_app();
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let accepted = app.session.documents()[0].doc.id;
        app.pending_raw_open =
            Some(RawOpen { document: fixed, photo: PhotoId::fresh(), rating: PhotoRating::Unknown, name: "owner.dng".into(), path: None, bytes: None });
        let error = app.open_automation_bytes("shot.dng", &dng).unwrap_err();
        assert!(error.contains("already has a RAW review"), "{error}");
        let left: Vec<_> = app.session.documents().iter().map(|st| st.doc.id).collect();
        assert_eq!(left, vec![accepted], "the new document was closed, the accepted one stays");
    }

    /// P0D (coverage): a JPEG still decoding when a RAW review is cancelled opens as an ordinary document.
    #[test]
    fn p0d_cancel_leaves_an_in_flight_jpeg_open_alone() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("review.dng", &dng).unwrap();
        let raw = take_next_raw_open(&mut app).unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        app.background_jobs = true;
        app.open_bytes("other.jpg", &jpeg_bytes()).unwrap();
        let job = app.jobs.opens[0].job;
        cancel_review(&mut app);
        let _ = app.session.wait_job(job);
        crate::jobs_ui::tick(&mut app, &ctx);
        sync_provisional(&mut app);
        assert_eq!(app.session.documents().len(), 1, "the JPEG opened; the RAW review is gone");
        assert_eq!(app.session.documents()[0].doc.name, "other.jpg");
        assert!(queued_ids(&app).is_empty() && !app.session.has_provisional());
    }

    /// P0D (coverage): two RAWs, a PSD and a JPEG applied in one poll; cancelling closes exactly the two RAWs.
    #[test]
    fn p0d_stale_cancel_of_two_raws_leaves_a_psd_and_a_jpeg() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.background_jobs = true;
        app.open_bytes("a.dng", &dng).unwrap();
        app.open_bytes("p.psd", &psd_bytes()).unwrap();
        app.open_bytes("b.dng", &dng).unwrap();
        app.open_bytes("j.jpg", &jpeg_bytes()).unwrap();
        let jobs: Vec<_> = app.jobs.opens.iter().map(|tab| tab.job).collect();
        for job in jobs {
            app.session.wait_job(job).unwrap();
        }
        let ordinary: Vec<_> = app.session.documents().iter().filter(|st| !st.doc.name.ends_with(".dng")).map(|st| st.doc.id).collect();
        assert_eq!(ordinary.len(), 2);
        app.session.next_review_generation();
        crate::jobs_ui::tick(&mut app, &ctx);
        sync_provisional(&mut app);
        let left: Vec<_> = app.session.documents().iter().map(|st| st.doc.id).collect();
        assert_eq!(left, ordinary, "only the PSD and the JPEG remain");
        assert!(queued_ids(&app).is_empty() && !app.session.has_provisional());
    }
    fn temp_file(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("opencode");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("p0e-{tag}-{}.dng", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// P0E R03: a denying automation hook stops `file.openAs {path}` on a direct in-process `app.run`, with or without a
    /// decoder override, before the file is read, anything is admitted, or the history or status change.
    #[test]
    fn p0e_a_denying_hook_stops_file_open_as_before_any_read_or_admission() {
        let (mut app, dng) = raw_app();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        app.services.automation_command = Some(Box::new(move |id, _| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if id == "file.openAs" { Err("denied by the authorization hook".into()) } else { Ok(()) }
        }));
        app.automation_input = true;
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let before = app.session.documents().len();
        let real = temp_file("hook", &dng);
        let missing = std::env::temp_dir().join("opencode").join("p0e-does-not-exist.dng");
        for params in [
            json!({"path": real.to_string_lossy()}),
            json!({"path": real.to_string_lossy(), "as": "dng"}),
            json!({"path": missing.to_string_lossy()}),
            json!({"path": missing.to_string_lossy(), "as": "dng"}),
        ] {
            let error = app.run("file.openAs", params.clone()).unwrap_err();
            assert_eq!(error, "denied by the authorization hook", "{params}: refused by the hook, not by a read or decode");
        }
        assert_eq!(app.session.documents().len(), before, "nothing was admitted");
        assert!(!app.session.has_provisional() && app.pending_raw_open.is_none() && app.queued_raw_opens.is_empty());
        assert!(calls.load(std::sync::atomic::Ordering::SeqCst) >= 4, "the hook judged every call");
        let _ = std::fs::remove_file(real);
    }

    /// P0E R01: the RAW is held when `app.run("file.openAs")` returns; the reply names it by `docId`.
    #[test]
    fn p0e_file_open_as_holds_the_raw_in_the_same_call() {
        let (mut app, dng) = raw_app();
        let real = temp_file("held", &dng);
        let reply = app.run("file.openAs", json!({"path": real.to_string_lossy()})).unwrap();
        let id = app.session.documents().last().unwrap().doc.id;
        assert_eq!(reply["docId"].as_u64(), Some(id.0));
        assert_eq!(reply["review"], "pending");
        assert_eq!(queued_ids(&app), vec![id]);
        assert!(app.session.provisional_documents().contains(&id));
        let _ = std::fs::remove_file(real);
    }
    /// The exemption Camera Raw needs to develop the RAW it is opening is one command, on that document, once: it does not
    /// carry to another command, another document, or a second call, and an unused one is taken (not kept for later).
    #[test]
    fn the_camera_raw_exemption_covers_one_command_on_one_document_once() {
        let (mut app, dng) = raw_app();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let accepted = app.session.active().unwrap().doc.id;
        app.open_bytes("held.dng", &dng).unwrap();
        let held = app.session.active().unwrap().doc.id;
        assert_ne!(held, accepted);
        assert_eq!(unaccepted_documents(&app), vec![held]);
        let refused = |app: &mut PhotocraftApp, id: &str| guard_native_command(app, id, &json!({})).is_err();
        assert!(refused(&mut app, "filter.cameraRaw"), "no exemption: refused");
        // The right command on the right document: allowed once, then gone.
        app.raw_exemption = Some(RawExemption { document: held, command: "filter.cameraRaw" });
        assert!(!refused(&mut app, "filter.cameraRaw"));
        assert!(app.raw_exemption.is_none(), "taken by the call that used it");
        assert!(refused(&mut app, "filter.cameraRaw"), "a second call is not covered");
        // Another command: refused, and the exemption does not wait for the next one.
        app.raw_exemption = Some(RawExemption { document: held, command: "filter.cameraRaw" });
        assert!(refused(&mut app, "edit.fill"));
        assert!(app.raw_exemption.is_none(), "an unused exemption is taken as well");
        assert!(refused(&mut app, "filter.cameraRaw"));
        // Another document: the active RAW is not the one the exemption names.
        app.raw_exemption = Some(RawExemption { document: accepted, command: "filter.cameraRaw" });
        assert!(refused(&mut app, "filter.cameraRaw"), "an exemption for a different document does not cover the active RAW");
        // Through `run_owned`: the command runs, and nothing is left behind for the next command.
        let before = app.session.active().unwrap().revision;
        let layer = app.session.active().unwrap().active_layer.unwrap();
        assert!(run_owned(&mut app, held, "filter.cameraRaw", json!({"layer": layer.0, "exposure": 0.5})).is_ok(), "Camera Raw develops its own document");
        assert!(app.session.active().unwrap().revision > before);
        assert!(app.raw_exemption.is_none());
        assert!(app.run("edit.fill", json!({"color": "#ff0000"})).is_err(), "the next command is judged on its own");
        // A different document than the active one: not covered.
        assert!(run_owned(&mut app, accepted, "filter.cameraRaw", json!({"layer": layer.0, "exposure": 0.5})).is_err());
    }

    /// While Open's commit job runs nothing else is queued and the dialog is gone, but the RAW is still unaccepted: the
    /// editor stays blocked, so the native menu, the shortcuts and the palette cannot run a shell command (Save a Copy,
    /// Export As) that the command guard never sees.
    #[test]
    fn the_editor_stays_blocked_while_a_raw_commit_runs() {
        let (mut app, dng) = raw_app();
        app.open_bytes("held.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        app.raw_redevelop = Some(Redevelop { job: photocraft_engine::jobs::JobId(7), params: CameraRaw::default(), raw, filter_step: false });
        assert!(app.camera_raw.is_none() && app.pending_raw_open.is_none() && app.queued_raw_opens.is_empty() && app.raw_filmstrip.parked.is_empty());
        assert!(!unaccepted_documents(&app).is_empty(), "the committing RAW is unaccepted");
        assert!(blocks_editor(&app), "so the editor is blocked");
        for id in ["file.save", "file.saveAs", "file.saveACopy", "file.export.exportAs", "layer.export.exportAs", "edit.search"] {
            assert!(!crate::menus::modal_allows(&app, id), "{id} must not run natively while a RAW commits");
        }
        app.ui.palette_open = true;
        sync_provisional(&mut app);
        assert!(!app.ui.palette_open, "the palette is closed");
        app.raw_redevelop = None;
        assert!(!blocks_editor(&app), "once the commit has finished the editor is free again");
    }

    /// The second commit phase, cancelled: the Camera Raw step ran on the re-developed document (a new id). It goes back to
    /// Camera Raw first, under that id, with the adjustments that were still to apply, and is never accepted.
    #[test]
    fn a_cancelled_camera_raw_step_returns_the_replacement_to_review() {
        use photocraft_engine::jobs::{JobEvent, JobId, JobOutcome};
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("held.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        let tuning = photocraft_io::raw::RawTuning { exposure: 1.0, ..Default::default() };
        let (developed, white_balanced) = photocraft_io::raw::import_raw_tuned("held.dng", &dng, &tuning).unwrap();
        let generation = app.session.review_generation();
        replace_raw_document(&mut app.session, raw.document, developed, white_balanced, generation).unwrap();
        let replacement = app.session.active().unwrap().doc.id;
        assert_ne!(replacement, raw.document);
        let rest = CameraRaw { contrast: 25.0, ..CameraRaw::default() };
        app.raw_redevelop = Some(Redevelop { job: JobId(9), params: rest, raw, filter_step: true });
        for outcome in [JobOutcome::Cancelled, JobOutcome::Failed("boom".into())] {
            let event = JobEvent { id: JobId(9), command: "filter.cameraRaw".into(), label: "x".into(), document: Some(replacement), outcome };
            if app.raw_redevelop.is_none() {
                let raw =
                    RawOpen { document: replacement, photo: PhotoId::fresh(), rating: PhotoRating::Unknown, name: "held.dng".into(), path: None, bytes: None };
                app.raw_redevelop = Some(Redevelop { job: JobId(9), params: CameraRaw { contrast: 25.0, ..CameraRaw::default() }, raw, filter_step: true });
            }
            app.raw_filmstrip.parked.clear();
            app.camera_raw = None;
            assert!(!on_redevelop_event(&mut app, &event), "the regular filter reporting still handles its own job");
            assert!(app.raw_redevelop.is_none());
            assert_eq!(unaccepted_documents(&app), vec![replacement], "never accepted by default");
            assert_eq!(app.session.provisional_documents(), vec![replacement]);
            assert!(blocks_editor(&app));
            start_next_raw_review(&mut app, &ctx);
            let dialog = app.camera_raw.as_ref().expect("the review is back");
            assert_eq!(dialog.raw_open.as_ref().map(|r| r.document), Some(replacement));
            assert_eq!(dialog.params.contrast, 25.0, "the remaining adjustments are kept");
            app.camera_raw = None;
            app.raw_redevelop = None;
        }
    }

    /// The first commit phase, failed or cancelled: the photograph keeps its identity and its adjustments, goes back to
    /// Camera Raw and is never accepted.
    #[test]
    fn a_failed_or_cancelled_redevelop_returns_the_same_document_to_review() {
        use photocraft_engine::jobs::{JobEvent, JobId, JobOutcome};
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("held.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        let params = CameraRaw { exposure: 1.0, contrast: 10.0, ..CameraRaw::default() };
        for outcome in [JobOutcome::Cancelled, JobOutcome::Failed("boom".into())] {
            app.raw_redevelop = Some(Redevelop { job: JobId(4), params: params.clone(), raw: raw.clone(), filter_step: false });
            let event = JobEvent { id: JobId(4), command: REDEVELOP_JOB.into(), label: "x".into(), document: Some(raw.document), outcome };
            assert!(on_redevelop_event(&mut app, &event));
            assert_eq!(unaccepted_documents(&app), vec![raw.document], "the same document, still unaccepted");
            assert_eq!(app.session.provisional_documents(), vec![raw.document]);
            start_next_raw_review(&mut app, &ctx);
            let dialog = app.camera_raw.as_ref().expect("the review is back");
            let expected = CameraRaw { encoding_gamma: 1.8, ..params.clone() };
            assert_eq!(dialog.params, expected, "the adjustments are kept with the reopened RAW document's encoding");
            app.camera_raw = None;
        }
    }

    /// A recovered review comes back before the queued ones; no other photograph is opened or accepted for it.
    #[test]
    fn a_recovered_review_comes_back_before_the_queued_photographs() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        for name in ["first.dng", "second.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let first = app.pending_raw_open.take().unwrap();
        let second = app.queued_raw_opens.front().unwrap().document;
        recover_unfinished(&mut app, first.clone(), CameraRaw { exposure: 1.0, ..CameraRaw::default() });
        assert_eq!(unaccepted_documents(&app).len(), 2);
        start_next_raw_review(&mut app, &ctx);
        let dialog = app.camera_raw.as_ref().expect("a review opened");
        assert_eq!(dialog.raw_open.as_ref().map(|r| r.document), Some(first.document), "the recovered one first, not {second:?}");
        assert_eq!(dialog.params.exposure, 1.0);
        assert!(app.raw_filmstrip.parked.iter().all(|item| !item.recover), "the recovered review was consumed");
        assert_eq!(app.queued_raw_opens.len(), 1, "the other photograph is still queued, not opened");
        assert!(!app.raw_filmstrip.batch_open_all);
    }
    /// Old-to-new: the re-developed document replaces the committing one under a new id, and is declared provisional by
    /// the replacement itself (the shell's own bookkeeping comes later in the same poll); the closed old id declares nothing.
    #[test]
    fn replacing_the_document_declares_the_new_one_provisional_at_once() {
        let (mut app, dng) = raw_app();
        app.open_bytes("held.dng", &dng).unwrap();
        let old = app.session.active().unwrap().doc.id;
        assert_eq!(app.session.provisional_documents(), vec![old]);
        let tuning = photocraft_io::raw::RawTuning { exposure: 1.0, ..Default::default() };
        let (developed, _white_balanced) = photocraft_io::raw::import_raw_tuned("held.dng", &dng, &tuning).unwrap();
        let generation = app.session.review_generation();
        let reply = replace_raw_document(&mut app.session, old, developed, false, generation).unwrap();
        assert_eq!(reply["previous"], old.0);
        let new = app.session.active().unwrap().doc.id;
        assert_ne!(new, old, "a new identity");
        assert_eq!(app.session.provisional_documents(), vec![new], "the new document is declared by the replacement itself; the closed one is gone");
        assert!(app.session.has_provisional());
        // Once the shell has finished the commit, its own bookkeeping releases it.
        sync_provisional(&mut app);
        assert!(app.session.provisional_documents().is_empty(), "nothing holds the committed document any more");
    }
    /// The provisional set follows the review without any frame: queueing adds, Cancel and close remove, and it only
    /// ever lists open documents.
    #[test]
    fn the_provisional_set_is_exact_after_queue_cancel_and_close() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let accepted = app.session.active().unwrap().doc.id;
        assert!(app.session.provisional_documents().is_empty());
        for name in ["first.dng", "second.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let mut held = unaccepted_documents(&app);
        let mut declared = app.session.provisional_documents();
        held.sort_by_key(|id| id.0);
        declared.sort_by_key(|id| id.0);
        assert_eq!((held.len(), declared), (2, held.clone()), "queued photographs are declared before any frame or hook");
        assert!(!app.session.provisional_documents().contains(&accepted));
        start_next_raw_review(&mut app, &ctx);
        let raw = app.camera_raw.as_ref().and_then(|d| d.raw_open.clone()).unwrap();
        cancel(&mut app);
        assert!(app.session.provisional_documents().is_empty(), "Cancel releases the set in the same call");
        assert!(document_index(&app, raw.document).is_none());
        assert!(app.session.documents().iter().any(|st| st.doc.id == accepted));
    }
    #[test]
    fn unaccepted_documents_hold_waiting_parked_and_committing_raws_and_never_an_accepted_document() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        let accepted = app.session.active().unwrap().doc.id;
        assert!(unaccepted_documents(&app).is_empty(), "an ordinary document is never held");
        for name in ["first.dng", "second.dng", "third.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let held = unaccepted_documents(&app);
        assert_eq!(held.len(), 3, "pending and queued RAWs are held before any dialog exists");
        assert!(!held.contains(&accepted));
        start_next_raw_review(&mut app, &ctx);
        let second = app.queued_raw_opens[0].document;
        select_raw_review(&mut app, &ctx, second).unwrap();
        assert_eq!(app.raw_filmstrip.parked.len(), 1);
        assert_eq!(unaccepted_documents(&app), held, "reviewing, parked and queued photographs are all held, whichever tab is active");
        // The dialog lets go of a photograph whose commit is still running: its identity stays held.
        let committing = app.camera_raw.as_ref().and_then(|d| d.raw_open.clone()).unwrap();
        app.camera_raw = None;
        app.raw_redevelop =
            Some(Redevelop { job: photocraft_engine::jobs::JobId(7), params: CameraRaw::default(), raw: committing.clone(), filter_step: false });
        assert!(unaccepted_documents(&app).contains(&committing.document));
        // Once the document is replaced or closed its identity is gone and nothing stale is held.
        let index = document_index(&app, committing.document).unwrap();
        app.session.close(index);
        assert!(!unaccepted_documents(&app).contains(&committing.document));
        assert!(unaccepted_documents(&app).iter().all(|id| *id != accepted));
    }

    #[test]
    fn background_redevelop_keeps_the_tab_position_and_view() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        app.open_bytes("mid.dng", &dng).unwrap();
        app.run("file.new", json!({"width": 12, "height": 12})).unwrap();
        app.sync_views();
        let raw = app.pending_raw_open.take().unwrap();
        let old_id = raw.document;
        assert_eq!(document_index(&app, old_id), Some(1));
        app.ui.views[1].zoom = 3.0;
        open_raw(&mut app, &ctx, raw).unwrap();
        let before = mean_red(&app);
        // The re-develop and the Camera Raw step run as jobs; the UI thread only polls.
        app.background_jobs = true;
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.0, "contrast": 20}}})).unwrap().unwrap();
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(r["pending"], true, "{r}");
        assert!(app.camera_raw.is_none());
        for _ in 0..3000 {
            crate::jobs_ui::tick(&mut app, &ctx);
            if app.raw_redevelop.is_none() && !app.session.has_jobs() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.raw_redevelop.is_none() && !app.session.has_jobs(), "the jobs never finished");
        let docs = app.session.documents();
        assert_eq!(docs.len(), 3);
        assert_eq!((docs[0].doc.size.width, docs[2].doc.size.width), (10, 12), "the other tabs keep their places");
        assert_ne!(docs[1].doc.id, old_id, "re-developed");
        assert_eq!(docs[1].doc.depth, photocraft_color::SampleType::U16);
        assert_eq!(docs[1].saved_revision, docs[1].revision, "Open leaves the document unmodified");
        assert_eq!(app.session.active_index(), Some(1));
        assert_eq!(app.ui.views[1].zoom, 3.0, "the view carries over to the re-developed document");
        assert!(mean_red(&app) > before + 0.05, "{before} → {}", mean_red(&app));
    }

    #[test]
    fn background_batch_open_serializes_jobs_and_ends_without_another_dialog() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        for name in ["first.dng", "second.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        start_next_raw_review(&mut app, &ctx);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.5, "contrast": 5}}})).unwrap().unwrap();
        app.background_jobs = true;
        let result = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(result["batch"], 2);
        assert!(app.raw_filmstrip.batch_open_all);
        for _ in 0..3000 {
            crate::jobs_ui::tick(&mut app, &ctx);
            advance_batch_open(&mut app, &ctx);
            if !app.raw_filmstrip.batch_open_all && !app.session.has_jobs() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!app.raw_filmstrip.batch_open_all && !app.session.has_jobs(), "the batch must finish: {}", app.ui.status);
        assert!(!blocks_editor(&app) && !hides_unaccepted_raw(&app));
        assert_eq!(app.session.documents().len(), 2);
        assert!(app.session.documents().iter().all(|st| st.saved_revision == st.revision));
        assert!(app.camera_raw.is_none());
        assert!(app.queued_raw_opens.is_empty());
    }

    #[test]
    fn cancel_closes_the_raw_and_the_preference_skips_the_dialog() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("a.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        assert_eq!(r["cancelled"], true);
        assert!(app.session.documents().is_empty(), "Cancel closes the raw being opened");
        // Unchanged settings: Open keeps the default develop and adds no step.
        app.open_bytes("b.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(r, json!({"document": 0, "redeveloped": false}));
        // Preference off: raws open straight away.
        app.run("prefs.set", json!({"path": "rawDefaults.openInCameraRaw", "value": false})).unwrap();
        app.open_bytes("c.dng", &dng).unwrap();
        assert!(app.pending_raw_open.is_none());
        // A non-raw never queues the dialog; a closed document is an error, not a panic.
        assert!(
            open_raw(
                &mut app,
                &ctx,
                RawOpen {
                    document: photocraft_doc::DocId(u64::MAX),
                    photo: PhotoId::fresh(),
                    rating: PhotoRating::Unknown,
                    name: "x".into(),
                    path: None,
                    bytes: None
                }
            )
            .is_err()
        );
    }

    const WINDOW: egui::Vec2 = vec2(1360.0, 900.0);

    fn frame(app: &mut PhotocraftApp, ctx: &egui::Context, size: egui::Vec2, events: Vec<egui::Event>) -> egui::CursorIcon {
        frame_mod(app, ctx, size, events, egui::Modifiers::NONE)
    }

    fn frame_mod(app: &mut PhotocraftApp, ctx: &egui::Context, size: egui::Vec2, events: Vec<egui::Event>, modifiers: egui::Modifiers) -> egui::CursorIcon {
        let mut events = events;
        events.insert(0, egui::Event::ModifiersChanged(modifiers));
        let mut input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, size)), events, ..Default::default() };
        // A display scale is the window's native scale (the screen size stays in points), not a UI zoom.
        let scale = ctx.data(|d| d.get_temp::<f32>(egui::Id::new("test-native-scale")));
        input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = scale;
        let mut out = ctx.run_ui(input, |ui| show(app, ui.ctx()));
        out.textures_delta.clear();
        out.platform_output.cursor_icon
    }

    fn button(at: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE }
    }

    fn click(app: &mut PhotocraftApp, ctx: &egui::Context, at: Pos2) {
        frame(app, ctx, WINDOW, vec![egui::Event::PointerMoved(at)]);
        frame(app, ctx, WINDOW, vec![button(at, true)]);
        frame(app, ctx, WINDOW, vec![button(at, false)]);
    }

    fn drag(app: &mut PhotocraftApp, ctx: &egui::Context, from: Pos2, to: Pos2) {
        drag_sized(app, ctx, WINDOW, from, to);
    }

    fn drag_sized(app: &mut PhotocraftApp, ctx: &egui::Context, size: egui::Vec2, from: Pos2, to: Pos2) {
        frame(app, ctx, size, vec![egui::Event::PointerMoved(from)]);
        frame(app, ctx, size, vec![button(from, true)]);
        frame(app, ctx, size, vec![egui::Event::PointerMoved(from + (to - from) * 0.5)]);
        frame(app, ctx, size, vec![egui::Event::PointerMoved(to)]);
        frame(app, ctx, size, vec![button(to, false)]);
    }

    fn key(app: &mut PhotocraftApp, ctx: &egui::Context, key: egui::Key) {
        let event = |pressed| egui::Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers: egui::Modifiers::NONE };
        frame(app, ctx, WINDOW, vec![event(true)]);
        frame(app, ctx, WINDOW, vec![event(false)]);
    }

    /// Three queued RAWs with the first under review, drawn for two frames.
    fn review_session() -> (PhotocraftApp, egui::Context, [photocraft_doc::DocId; 3]) {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        for name in ["one.dng", "two.dng", "three.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let ids = [app.pending_raw_open.as_ref().unwrap().document, app.queued_raw_opens[0].document, app.queued_raw_opens[1].document];
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        (app, ctx, ids)
    }

    fn active_document(app: &PhotocraftApp) -> photocraft_doc::DocId {
        app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|raw| raw.document).unwrap()
    }

    fn preview_left(app: &PhotocraftApp) -> f32 {
        app.camera_raw.as_ref().and_then(|d| d.viewport).map(|v| v.left()).unwrap()
    }

    fn card(app: &PhotocraftApp, id: photocraft_doc::DocId) -> ERect {
        app.raw_filmstrip.frame.cards.iter().find(|(doc, _)| *doc == id).map(|(_, rect)| *rect).unwrap()
    }

    /// FS-001 / FS-002: hiding leaves no strip, splitter or gutter, the preview takes the width in
    /// the very frame of the click, and showing it again restores width, photo and edits.
    #[test]
    fn filmstrip_collapse_leaves_no_gutter_and_restore_keeps_width_photo_and_edits() {
        let (mut app, ctx, ids) = review_session();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.25}}})).unwrap().unwrap();
        frame(&mut app, &ctx, WINDOW, vec![]);
        let shown = app.raw_filmstrip.frame.clone();
        assert_eq!((shown.layout.strip.map(|s| s.width()), shown.cards.len()), (Some(176.0), 3));
        assert!(shown.layout.splitter.is_some());
        let expanded_left = preview_left(&app);
        click(&mut app, &ctx, shown.toggle.unwrap().center());
        let hidden = &app.raw_filmstrip.frame;
        assert!(!app.raw_filmstrip.view.visible);
        assert!(hidden.layout.strip.is_none() && hidden.layout.splitter.is_none() && hidden.cards.is_empty(), "no hidden strip content");
        assert_eq!(hidden.layout.preview_body.left(), 0.0, "no reserved gutter");
        assert_eq!(preview_left(&app), expanded_left - 176.0, "the preview reclaims the width in the click's own frame");
        assert_eq!((active_document(&app), app.camera_raw.as_ref().unwrap().params.exposure), (ids[0], 1.25));
        assert_eq!(app.session.documents().len(), 3, "hiding must not accept or cancel anything");
        for expected in [true, false] {
            let toggle = app.raw_filmstrip.frame.toggle.unwrap();
            click(&mut app, &ctx, toggle.center());
            assert_eq!(app.raw_filmstrip.view.visible, expected, "repeated hide and restore stay in step");
        }
        let toggle = app.raw_filmstrip.frame.toggle.unwrap();
        click(&mut app, &ctx, toggle.center());
        assert!(app.raw_filmstrip.view.visible);
        assert_eq!(app.raw_filmstrip.frame.layout.strip.map(|s| s.width()), Some(176.0));
        assert_eq!((preview_left(&app), active_document(&app), app.camera_raw.as_ref().unwrap().params.exposure), (expanded_left, ids[0], 1.25));
    }

    /// FS-003..FS-006: a drag resizes within the limits, the Fit preview reflows from the new
    /// rectangle, and a narrow window clamps the drawn width without erasing the preference.
    #[test]
    fn filmstrip_splitter_clamps_reflows_fit_and_survives_window_resizes() {
        let (mut app, ctx, _) = review_session();
        let splitter = |app: &PhotocraftApp| app.raw_filmstrip.frame.layout.splitter.unwrap().center();
        let before = preview_left(&app);
        let start = splitter(&app);
        drag(&mut app, &ctx, start, start + vec2(60.0, 0.0));
        assert_eq!(app.raw_filmstrip.view.preferred_width, 236.0);
        assert_eq!(preview_left(&app), before + 60.0, "the preview follows the drag in the same frame");
        assert!(app.ui.camera_raw_preview.zoom.is_none(), "still Fit");
        let d = app.camera_raw.as_ref().unwrap();
        let (view, image) = (d.viewport.unwrap(), d.scope.preview_rect.unwrap());
        assert!(view.expand(1.0).contains_rect(image) && (image.width() / image.height() - 1.5).abs() < 0.02, "{view:?} {image:?}");
        let start = splitter(&app);
        drag(&mut app, &ctx, start, pos2(1200.0, 400.0));
        assert_eq!(app.raw_filmstrip.view.preferred_width, filmstrip_ui::MAX_WIDTH);
        let start = splitter(&app);
        drag(&mut app, &ctx, start, pos2(0.0, 400.0));
        assert_eq!(app.raw_filmstrip.view.preferred_width, 128.0);
        let start = splitter(&app);
        drag(&mut app, &ctx, start, pos2(300.0, 400.0));
        let wanted = app.raw_filmstrip.view.preferred_width;
        frame(&mut app, &ctx, vec2(900.0, 600.0), vec![]);
        assert_eq!(app.raw_filmstrip.frame.layout.strip.map(|s| s.width()), Some(filmstrip_ui::effective_width(wanted, 900.0, PANEL_W)));
        assert_eq!(app.raw_filmstrip.view.preferred_width, wanted, "a small window does not erase the preference");
        frame(&mut app, &ctx, WINDOW, vec![]);
        assert_eq!(app.raw_filmstrip.frame.layout.strip.map(|s| s.width()), Some(wanted));
        assert_eq!(app.session.documents().len(), 3);
    }

    #[test]
    fn filmstrip_splitter_has_a_keyboard_alternative() {
        let (mut app, ctx, _) = review_session();
        let at = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
        click(&mut app, &ctx, at);
        frame(&mut app, &ctx, WINDOW, vec![]); // the repaint the focus change requests
        let mut widths = Vec::new();
        for pressed in [egui::Key::ArrowRight, egui::Key::ArrowLeft, egui::Key::ArrowLeft, egui::Key::ArrowLeft] {
            key(&mut app, &ctx, pressed);
            widths.push(app.raw_filmstrip.view.preferred_width);
        }
        assert_eq!(widths, [192.0, 176.0, 160.0, 144.0], "each arrow press moves the width by one step");
        assert_eq!(app.raw_filmstrip.frame.layout.strip.map(|s| s.width()), Some(144.0), "and the strip follows");
    }

    /// FS-010..FS-014: annotation switches are independent presentation, reserve no phantom
    /// row, never touch photo data, and the edit badge is only claimed where it is known.
    #[test]
    fn filmstrip_annotations_are_independent_and_badges_follow_real_edits() {
        let (mut app, ctx, ids) = review_session();
        // F1 (lab cycle 3): adjustments are tri-state; a photo not yet shown is unknown, never claimed either way.
        use filmstrip_ui::Adjustments::{Edited, None as Clean, Unknown};
        let edited = |app: &mut PhotocraftApp, ctx: &egui::Context| filmstrip_entries(app, ctx).iter().map(|e| e.adjustments).collect::<Vec<_>>();
        assert_eq!(edited(&mut app, &ctx), [Clean, Unknown, Unknown]);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.0}}})).unwrap().unwrap();
        select_raw_review(&mut app, &ctx, ids[1]).unwrap();
        assert_eq!(edited(&mut app, &ctx), [Edited, Clean, Unknown], "parked edits stay known, the opened photo starts clean");
        // Showing another preview preserves the editing group. Select this photo
        // explicitly before checking its independent adjustment badge.
        let second = filmstrip_raws(&app).iter().find(|raw| raw.document == ids[1]).unwrap().photo;
        app.raw_filmstrip.selection.select_only(second);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"contrast": 9}}})).unwrap().unwrap();
        assert_eq!(edited(&mut app, &ctx), [Edited, Edited, Unknown]);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"contrast": 0}}})).unwrap().unwrap();
        assert_eq!(edited(&mut app, &ctx), [Edited, Clean, Unknown], "a reset clears the badge");
        frame(&mut app, &ctx, WINDOW, vec![]);
        let full_height = card(&app, ids[0]).height();
        let (active_params, parked_params) = (app.camera_raw.as_ref().unwrap().params.clone(), app.raw_filmstrip.parked[0].params.clone());
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {"showFilenames": false}}})).unwrap().unwrap();
        assert_eq!((r["filmstripView"]["showFilenames"].as_bool(), r["filmstripView"]["showEditBadges"].as_bool()), (Some(false), Some(true)));
        frame(&mut app, &ctx, WINDOW, vec![]);
        assert_eq!(full_height - card(&app, ids[0]).height(), 16.0, "exactly the filename row (14 pt) and its gap (2 pt) disappear");
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {"showEditBadges": false}}})).unwrap().unwrap();
        assert_eq!(edited(&mut app, &ctx), [Edited, Clean, Unknown], "hiding badges does not change what is edited");
        assert_eq!((&app.camera_raw.as_ref().unwrap().params, &app.raw_filmstrip.parked[0].params), (&active_params, &parked_params));
        let bad = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {"showFilenames": true, "width": "wide"}}})).unwrap();
        assert!(bad.is_err() && !app.raw_filmstrip.view.show_filenames, "an invalid request changes nothing");
    }

    /// FS-021 / FS-022 / FS-028: thumbnails select by identity and keep each photo's edits; the
    /// keyboard moves focus without activating, Enter activates, and a badge never eats a click.
    #[test]
    fn filmstrip_clicks_and_keys_select_by_identity_and_focus_is_not_activation() {
        let (mut app, ctx, ids) = review_session();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.25}}})).unwrap().unwrap();
        frame(&mut app, &ctx, WINDOW, vec![]);
        let second = card(&app, ids[1]).center();
        click(&mut app, &ctx, second);
        assert_eq!((active_document(&app), app.camera_raw.as_ref().unwrap().params.exposure), (ids[1], 0.0));
        frame(&mut app, &ctx, WINDOW, vec![]);
        // F1: the adjustments glyph now sits in the status row under the picture; it must still not swallow the click.
        let (_, picture_box) = filmstrip_ui::card_size(176.0, (48, 32), true);
        let badge = filmstrip_ui::card_parts(card(&app, ids[0]), picture_box, (48, 32), true).adjustments;
        assert!(app.raw_filmstrip.parked.iter().any(|saved| saved.raw.document == ids[0]));
        click(&mut app, &ctx, badge.center());
        assert_eq!((active_document(&app), app.camera_raw.as_ref().unwrap().params.exposure), (ids[0], 1.25), "the badge must not swallow the selecting click");
        frame(&mut app, &ctx, WINDOW, vec![]);
        let third = card(&app, ids[2]).center();
        click(&mut app, &ctx, third);
        assert_eq!(active_document(&app), ids[2]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        key(&mut app, &ctx, egui::Key::ArrowUp);
        assert_eq!(active_document(&app), ids[2], "moving keyboard focus must not change the active photo");
        key(&mut app, &ctx, egui::Key::Enter);
        assert_eq!(active_document(&app), ids[1], "Enter activates the focused thumbnail");
        assert_eq!(filmstrip_raws(&app).iter().map(|raw| raw.document).collect::<Vec<_>>(), ids, "order is stable");
        assert_eq!(app.session.documents().len(), 3);
    }

    /// The old "N photos" heading was untranslated text and a row of dead space.
    #[test]
    fn filmstrip_has_no_heading_row_and_thumbnail_only_cards_start_at_the_top() {
        let (mut app, ctx, ids) = review_session();
        let top = |app: &PhotocraftApp| card(app, ids[0]).top() - app.raw_filmstrip.frame.layout.strip.unwrap().top();
        assert!(top(&app) < 8.0, "a heading row would push the first card down: {}", top(&app));
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {"showFilenames": false, "showEditBadges": false}}})).unwrap().unwrap();
        frame(&mut app, &ctx, WINDOW, vec![]);
        assert!(top(&app) < 8.0);
        let expected = filmstrip_ui::card_size(176.0, (48, 32), false).0;
        let actual = card(&app, ids[0]).size();
        assert!((actual - expected).length() < 0.1, "no filename row either; actual={actual:?}, expected={expected:?}");
    }

    /// The resize cursor follows the pointer, not keyboard focus, and the keys keep working.
    #[test]
    fn filmstrip_resize_cursor_follows_the_pointer_not_keyboard_focus() {
        let (mut app, ctx, _) = review_session();
        let at = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
        frame(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(at)]);
        assert_eq!(frame(&mut app, &ctx, WINDOW, vec![]), egui::CursorIcon::ResizeHorizontal, "hover");
        click(&mut app, &ctx, at);
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(pos2(100.0, 800.0))]);
        assert_ne!(frame(&mut app, &ctx, WINDOW, vec![]), egui::CursorIcon::ResizeHorizontal, "focus alone must not hold the resize cursor");
        let before = app.raw_filmstrip.view.preferred_width;
        key(&mut app, &ctx, egui::Key::ArrowRight);
        assert_eq!(app.raw_filmstrip.view.preferred_width, before + 16.0, "the splitter kept keyboard focus");
    }

    /// FS-028: a focused adjustment field keeps its arrow keys; the Filmstrip never takes them.
    #[test]
    fn arrow_keys_stay_with_a_focused_adjustment_field_not_the_filmstrip() {
        let (mut app, ctx, ids) = review_session();
        let second = card(&app, ids[1]).center();
        click(&mut app, &ctx, second); // a thumbnail now holds keyboard focus
        frame(&mut app, &ctx, WINDOW, vec![]);
        let width = app.raw_filmstrip.view.preferred_width;
        let field = adjustment_field(&mut app, &ctx, "Contrast");
        click(&mut app, &ctx, field.center());
        frame(&mut app, &ctx, WINDOW, vec![]);
        let contrast = |app: &PhotocraftApp| app.camera_raw.as_ref().unwrap().params.contrast;
        key(&mut app, &ctx, egui::Key::ArrowUp);
        assert_eq!(contrast(&app), 1.0, "the field handled the arrow (is that still the Contrast field's position?)");
        key(&mut app, &ctx, egui::Key::ArrowDown);
        key(&mut app, &ctx, egui::Key::ArrowDown);
        assert_eq!(contrast(&app), -1.0);
        assert_eq!(active_document(&app), ids[1], "arrow keys must not move or activate Filmstrip thumbnails");
        assert_eq!(app.raw_filmstrip.view.preferred_width, width);
    }

    /// At the dialog's 900 pt minimum the strip must leave the preview toolbar enough room.
    #[test]
    fn the_preview_toolbar_fits_beside_the_filmstrip_at_the_smallest_window() {
        let (mut app, ctx, _) = review_session();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {"width": 300}}})).unwrap().unwrap(); // the widest preference
        frame(&mut app, &ctx, vec2(900.0, 600.0), vec![]);
        frame(&mut app, &ctx, vec2(900.0, 600.0), vec![]);
        let f = &app.raw_filmstrip.frame;
        assert!(
            f.toolbar.right() <= f.layout.preview_body.right(),
            "the toolbar ends at {} but the preview ends at {}",
            f.toolbar.right(),
            f.layout.preview_body.right()
        );
    }

    // ---- ACR-S2 Segments A and B: real gestures, orientation, independent selection -------------

    fn set_scale(app: &mut PhotocraftApp, ctx: &egui::Context, scale: f32) {
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("test-native-scale"), scale));
        for _ in 0..3 {
            frame(app, ctx, WINDOW, vec![]);
        }
    }

    fn click_mod(app: &mut PhotocraftApp, ctx: &egui::Context, at: Pos2, modifiers: egui::Modifiers) {
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers };
        frame_mod(app, ctx, WINDOW, vec![egui::Event::PointerMoved(at)], modifiers);
        frame_mod(app, ctx, WINDOW, vec![press(true)], modifiers);
        frame_mod(app, ctx, WINDOW, vec![press(false)], modifiers);
        // egui honours a focus lock only from the second focused frame; a person is never faster than one frame.
        frame(app, ctx, WINDOW, vec![]);
    }

    fn click_card_mod(app: &mut PhotocraftApp, ctx: &egui::Context, id: photocraft_doc::DocId, modifiers: egui::Modifiers) {
        let at = card(app, id).center();
        click_mod(app, ctx, at, modifiers);
    }

    fn click_card(app: &mut PhotocraftApp, ctx: &egui::Context, id: photocraft_doc::DocId) {
        click_card_mod(app, ctx, id, egui::Modifiers::NONE);
    }
    fn key_mod(app: &mut PhotocraftApp, ctx: &egui::Context, key: egui::Key, modifiers: egui::Modifiers) {
        let event = |pressed| egui::Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers };
        frame_mod(app, ctx, WINDOW, vec![event(true)], modifiers);
        frame_mod(app, ctx, WINDOW, vec![event(false)], modifiers);
    }

    fn secondary_click(app: &mut PhotocraftApp, ctx: &egui::Context, at: Pos2) {
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE };
        frame(app, ctx, WINDOW, vec![egui::Event::PointerMoved(at)]);
        frame(app, ctx, WINDOW, vec![press(true)]);
        frame(app, ctx, WINDOW, vec![press(false)]);
        frame(app, ctx, WINDOW, vec![]);
        frame(app, ctx, WINDOW, vec![]);
    }

    fn set_view(app: &mut PhotocraftApp, ctx: &egui::Context, filmstrip: Value) {
        menu(app, ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": filmstrip}})).unwrap().unwrap();
        frame(app, ctx, WINDOW, vec![]);
    }

    fn selected(app: &PhotocraftApp) -> Vec<photocraft_doc::DocId> {
        filmstrip_raws(app).iter().filter(|raw| app.raw_filmstrip.selection.contains(raw.photo)).map(|raw| raw.document).collect()
    }

    /// The document of the photograph the selection's keyboard focus (or anchor) names.
    fn focus_doc(app: &PhotocraftApp) -> Option<photocraft_doc::DocId> {
        let photo = app.raw_filmstrip.selection.focus()?;
        filmstrip_raws(app).iter().find(|raw| raw.photo == photo).map(|raw| raw.document)
    }

    fn anchor_doc(app: &PhotocraftApp) -> Option<photocraft_doc::DocId> {
        let photo = app.raw_filmstrip.selection.anchor()?;
        filmstrip_raws(app).iter().find(|raw| raw.photo == photo).map(|raw| raw.document)
    }

    /// The photograph a queued document is (tests that drive the selection model directly).
    fn photo_of(app: &PhotocraftApp, document: photocraft_doc::DocId) -> PhotoId {
        filmstrip_raws(app).iter().find(|raw| raw.document == document).map(|raw| raw.photo).expect("the document is queued for review")
    }

    fn strip_width(app: &PhotocraftApp) -> f32 {
        app.raw_filmstrip.frame.layout.strip.map_or(0.0, |s| s.width())
    }

    /// A1 / H3 (FS-003, FS-004, FS-006): a press, many moves across relayouts and a release change
    /// the drawn width continuously by exactly the pointer travel, at every display scale.
    #[test]
    fn filmstrip_pointer_drag_across_relayouts_changes_width_continuously() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let (mut app, ctx, _) = review_session();
            set_scale(&mut app, &ctx, ppp);
            let start = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
            frame(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(start)]);
            frame(&mut app, &ctx, WINDOW, vec![button(start, true)]);
            let mut seen = vec![strip_width(&app)];
            for step in 1..=8 {
                frame(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(start + vec2(12.0 * step as f32, 0.0))]);
                frame(&mut app, &ctx, WINDOW, vec![]);
                seen.push(strip_width(&app));
            }
            frame(&mut app, &ctx, WINDOW, vec![button(start + vec2(96.0, 0.0), false)]);
            let expected: Vec<f32> = (0..=8).map(|n| 176.0 + 12.0 * n as f32).collect();
            assert_eq!(seen, expected, "ppp {ppp}: every step of the drag moves the strip by the pointer travel");
            let here = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
            assert_ne!(
                frame(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(here + vec2(200.0, 0.0))]),
                egui::CursorIcon::ResizeHorizontal,
                "ppp {ppp}: the resize cursor ends with the drag"
            );
            // Back to the minimum in one sweep: the strip stops at the limit and never collapses.
            let there = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
            drag(&mut app, &ctx, there, pos2(0.0, 400.0));
            assert_eq!((strip_width(&app), app.raw_filmstrip.view.visible), (filmstrip_ui::MIN_WIDTH, true), "ppp {ppp}: the minimum does not collapse");
            assert_eq!(app.session.documents().len(), 3);
        }
    }

    /// A1 / H1: the divider can be grabbed a few points either side of its centre, not only on it.
    #[test]
    fn filmstrip_divider_grab_tolerance_covers_a_real_hand() {
        for (offset, grabs) in [(-5.0, true), (-3.0, true), (0.0, true), (3.0, true), (5.0, true), (9.0, false)] {
            let (mut app, ctx, _) = review_session();
            let start = app.raw_filmstrip.frame.layout.splitter.unwrap().center() + vec2(offset, 0.0);
            drag(&mut app, &ctx, start, start + vec2(40.0, 0.0));
            assert_eq!(strip_width(&app) != 176.0, grabs, "a press {offset} pt from the divider's centre: {}", strip_width(&app));
        }
    }

    /// A1 / H2: what width can actually be reached depends on the window. Reported, and consistent.
    #[test]
    fn filmstrip_reachable_width_follows_the_window_at_every_scale() {
        for ppp in [1.0, 1.5, 2.0] {
            for window in [900.0, 960.0, 1100.0, 1280.0, 1632.0] {
                let size = vec2(window, 700.0);
                let (mut app, ctx, _) = review_session();
                set_scale(&mut app, &ctx, ppp);
                let start = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
                drag_sized(&mut app, &ctx, size, start, pos2(window, 400.0));
                let widest = strip_width(&app);
                let start = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
                drag_sized(&mut app, &ctx, size, start, pos2(0.0, 400.0));
                let narrowest = strip_width(&app);
                println!("REACHABLE ppp={ppp} window={window}: {narrowest}..{widest} pt");
                assert_eq!(widest, filmstrip_ui::effective_width(filmstrip_ui::MAX_WIDTH, window, PANEL_W), "{window} @ {ppp}");
                assert_eq!(narrowest, filmstrip_ui::MIN_WIDTH, "the narrowest strip is always reachable");
            }
        }
    }

    /// A2 (FS-001, FS-002, T07): a primary click toggles only visibility; nothing else changes.
    #[test]
    fn filmstrip_primary_toggle_only_visibility_and_restores_preferred_dimension() {
        let (mut app, ctx, ids) = review_session();
        set_view(&mut app, &ctx, json!({"width": 260, "height": 180}));
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.5}}})).unwrap().unwrap();
        let before = (active_document(&app), selected(&app), app.raw_filmstrip.view.preferred_width, app.raw_filmstrip.view.orientation);
        for expect_visible in [false, true] {
            // F3 (lab cycle 4): the control lives in the preview toolbar, which moves with the preview column, so each
            // click goes where the last frame drew it.
            let toggle = app.raw_filmstrip.frame.toggle.unwrap().center();
            click(&mut app, &ctx, toggle);
            assert_eq!(app.raw_filmstrip.view.visible, expect_visible);
            assert_eq!(app.raw_filmstrip.frame.layout.strip.is_some(), expect_visible, "no strip, splitter or gutter while hidden");
            assert_eq!(app.raw_filmstrip.frame.layout.splitter.is_some(), expect_visible);
        }
        assert_eq!((active_document(&app), selected(&app), app.raw_filmstrip.view.preferred_width, app.raw_filmstrip.view.orientation), before);
        assert_eq!((strip_width(&app), app.camera_raw.as_ref().unwrap().params.exposure, app.session.documents().len(), ids.len()), (260.0, 0.5, 3, 3));
    }

    /// A2 (T07, FS-029): a secondary click opens the Orientation menu and never toggles.
    #[test]
    fn filmstrip_secondary_click_opens_orientation_without_toggling() {
        for start_hidden in [false, true] {
            let (mut app, ctx, ids) = review_session();
            menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.0}}})).unwrap().unwrap();
            app.raw_filmstrip.view.visible = !start_hidden;
            frame(&mut app, &ctx, WINDOW, vec![]);
            let toggle = app.raw_filmstrip.frame.toggle.unwrap().center();
            secondary_click(&mut app, &ctx, toggle);
            assert_eq!(app.raw_filmstrip.view.visible, !start_hidden, "a right click never toggles (hidden={start_hidden})");
            let items = app.raw_filmstrip.frame.menu.clone();
            assert_eq!(items.iter().map(|(o, _)| *o).collect::<Vec<_>>(), [filmstrip_ui::Orientation::Horizontal, filmstrip_ui::Orientation::Vertical]);
            click(&mut app, &ctx, items[0].1.center());
            assert_eq!(app.raw_filmstrip.view.orientation, filmstrip_ui::Orientation::Horizontal, "choosing Horizontal works even while hidden={start_hidden}");
            assert_eq!(app.raw_filmstrip.view.visible, !start_hidden, "choosing an orientation never opens or closes the strip");
            frame(&mut app, &ctx, WINDOW, vec![]);
            assert!(app.raw_filmstrip.frame.menu.is_empty(), "the menu closes after a choice");
            assert_eq!((active_document(&app), app.camera_raw.as_ref().unwrap().params.exposure, app.session.documents().len()), (ids[0], 1.0, 3));
        }
    }

    /// A3 (T06/T07): a horizontal strip sits under the preview only, never under the settings panel.
    #[test]
    fn horizontal_strip_only_spans_preview_beneath_canvas() {
        let (mut app, ctx, ids) = review_session();
        set_view(&mut app, &ctx, json!({"orientation": "horizontal"}));
        let f = app.raw_filmstrip.frame.clone();
        let (strip, splitter) = (f.layout.strip.unwrap(), f.layout.splitter.unwrap());
        assert_eq!((strip.left(), strip.right()), (0.0, WINDOW.x - PANEL_W), "it ends where the settings panel begins");
        assert_eq!(strip.top(), f.layout.preview_body.bottom());
        assert!(splitter.contains(strip.center_top()));
        let d = app.camera_raw.as_ref().unwrap();
        assert!(d.viewport.unwrap().bottom() <= strip.top(), "the image never extends under the strip");
        assert!(
            f.toolbar.right() <= f.layout.preview_body.right() && f.toolbar.bottom() <= strip.top() + 1.0,
            "the zoom toolbar stays in the preview column: {:?}",
            f.toolbar
        );
        assert_eq!(f.cards.len(), 3);
        assert!(
            f.cards.iter().all(|(_, c)| strip.contains_rect(*c)) && f.cards.windows(2).all(|w| w[1].1.left() > w[0].1.left() && w[1].1.top() == w[0].1.top()),
            "cards run left to right in order: {:?}",
            f.cards
        );
        assert_eq!(f.cards.iter().map(|(d, _)| *d).collect::<Vec<_>>(), ids.to_vec());
    }

    /// A3: orientation, hide and restore keep photo, settings, selection and zoom.
    #[test]
    fn orientation_switch_preserves_active_id_settings_selection_and_zoom() {
        let (mut app, ctx, ids) = review_session();
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.25}}})).unwrap().unwrap();
        app.ui.camera_raw_preview.zoom = Some(2.0);
        app.ui.camera_raw_preview.center = [0.3, 0.6];
        // The 48x32 fixture always fits the view, so egui clamps the centre to the middle: only zoom and the hand tool can be compared.
        let nav = |app: &PhotocraftApp| (app.ui.camera_raw_preview.zoom, app.ui.camera_raw_preview.hand);
        let before = (active_document(&app), selected(&app), app.camera_raw.as_ref().unwrap().params.clone(), nav(&app));
        for orientation in ["horizontal", "vertical", "horizontal"] {
            set_view(&mut app, &ctx, json!({"orientation": orientation}));
            set_view(&mut app, &ctx, json!({"visible": false}));
            set_view(&mut app, &ctx, json!({"visible": true}));
            assert_eq!((active_document(&app), selected(&app), app.camera_raw.as_ref().unwrap().params.clone(), nav(&app)), before, "{orientation}");
        }
        assert_eq!(app.session.documents().len(), 3);
    }

    /// A3: the widest strip and the tallest strip leave the zoom toolbar and the settings panel intact.
    #[test]
    fn wide_strip_clamps_without_hiding_zoom_or_right_controls() {
        for (orientation, window) in
            [("vertical", vec2(960.0, 640.0)), ("vertical", vec2(1100.0, 700.0)), ("horizontal", vec2(900.0, 600.0)), ("horizontal", vec2(1360.0, 900.0))]
        {
            let (mut app, ctx, _) = review_session();
            let step = |app: &mut PhotocraftApp| frame(app, &ctx, window, vec![]);
            menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {"orientation": orientation, "width": 360, "height": 260}}}))
                .unwrap()
                .unwrap();
            step(&mut app);
            step(&mut app);
            let f = app.raw_filmstrip.frame.clone();
            assert!(
                f.toolbar.right() <= f.layout.preview_body.right() + 0.5,
                "{orientation} {window:?}: the toolbar ends at {} but the preview ends at {}",
                f.toolbar.right(),
                f.layout.preview_body.right()
            );
            assert!(
                f.layout.preview_body.width() >= 376.0 && f.layout.preview_body.height() >= 300.0 || f.layout.strip.is_none(),
                "{orientation} {window:?}: {:?}",
                f.layout
            );
            assert_eq!(app.raw_filmstrip.view.preferred_width, 360.0, "the preference is never erased by a small window");
            assert!(app.camera_raw.as_ref().unwrap().viewport.unwrap().is_positive());
        }
    }

    /// A3: along a horizontal strip Left/Right move focus; Up/Down resize it when its divider has focus.
    #[test]
    fn horizontal_strip_arrows_move_focus_along_it_and_the_divider_uses_up_down() {
        let (mut app, ctx, ids) = review_session();
        set_view(&mut app, &ctx, json!({"orientation": "horizontal"}));
        click_card(&mut app, &ctx, ids[0]);
        key(&mut app, &ctx, egui::Key::ArrowRight);
        assert_eq!(focus_doc(&app), Some(ids[1]), "Right moves focus along a horizontal strip");
        assert_eq!(active_document(&app), ids[0], "focus alone does not change the photo");
        key(&mut app, &ctx, egui::Key::Enter);
        assert_eq!(active_document(&app), ids[1], "Enter activates the focused photo");
        frame(&mut app, &ctx, WINDOW, vec![]);
        let divider = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
        click(&mut app, &ctx, divider);
        frame(&mut app, &ctx, WINDOW, vec![]);
        let before = app.raw_filmstrip.view.preferred_height;
        key(&mut app, &ctx, egui::Key::ArrowUp);
        assert_eq!(app.raw_filmstrip.view.preferred_height, before + 16.0, "Up grows a horizontal strip");
        key(&mut app, &ctx, egui::Key::ArrowDown);
        assert_eq!(app.raw_filmstrip.view.preferred_height, before);
    }

    /// A1 for the horizontal divider: a real drag changes the height continuously and stops at the limits.
    #[test]
    fn horizontal_divider_drag_resizes_height_continuously_without_collapsing() {
        let (mut app, ctx, _) = review_session();
        set_view(&mut app, &ctx, json!({"orientation": "horizontal"}));
        let height = |app: &PhotocraftApp| app.raw_filmstrip.frame.layout.strip.map_or(0.0, |s| s.height());
        let start = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
        frame(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(start)]);
        frame(&mut app, &ctx, WINDOW, vec![button(start, true)]);
        let mut seen = vec![height(&app)];
        for step in 1..=5 {
            frame(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(start - vec2(0.0, 12.0 * step as f32))]);
            frame(&mut app, &ctx, WINDOW, vec![]);
            seen.push(height(&app));
        }
        frame(&mut app, &ctx, WINDOW, vec![button(start - vec2(0.0, 60.0), false)]);
        assert_eq!(seen, (0..=5).map(|n| 136.0 + 12.0 * n as f32).collect::<Vec<_>>());
        let top = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
        drag(&mut app, &ctx, top, pos2(500.0, 0.0));
        assert_eq!(height(&app), filmstrip_ui::MAX_HEIGHT.min(WINDOW.y - 78.0 - 300.0));
        let top = app.raw_filmstrip.frame.layout.splitter.unwrap().center();
        drag(&mut app, &ctx, top, pos2(500.0, 900.0));
        assert_eq!((height(&app), app.raw_filmstrip.view.visible), (filmstrip_ui::MIN_HEIGHT, true), "the minimum height does not collapse");
    }

    // ---- ACR-S2 Segment C: identity and lifecycle (synthetic, in memory; no originals) ----------

    fn neutral_dng(neutral: [f64; 3]) -> Vec<u8> {
        use photocraft_raw::testgen::{DngSpec, mosaic, scene};
        let (w, h) = (48, 32);
        let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
        spec.as_shot_neutral = Some(neutral);
        spec.build()
    }

    fn pump(app: &mut PhotocraftApp, ctx: &egui::Context, until: impl Fn(&PhotocraftApp) -> bool) {
        for _ in 0..3000 {
            crate::jobs_ui::tick(app, ctx);
            if until(app) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the background opens did not finish: {} still running", app.jobs.opens.len());
    }

    fn mean_red_of(app: &PhotocraftApp, id: photocraft_doc::DocId) -> f32 {
        let st = app.session.documents().iter().find(|st| st.doc.id == id).unwrap();
        let s = st.doc.layer(st.active_layer.unwrap()).unwrap().surface().unwrap();
        (0..8).map(|i| s.rgba(4 + i * 4, 16)[0]).sum::<f32>() / 8.0
    }

    #[test]
    fn the_local_raw_extension_list_matches_the_importers() {
        assert_eq!(RAW_EXTENSIONS.to_vec(), photocraft_raw::EXTENSIONS.to_vec());
        assert!(
            is_raw_name("IMG_0001.NEF")
                && is_raw_name("a.dng")
                && !is_raw_name("scan.psd")
                && !is_raw_name("photo.jpg")
                && !is_raw_name("nef")
                && !is_raw_name("")
        );
    }

    /// C / T21, OPEN-004, P0-09: Cancel with other photos still decoding must leave nothing behind.
    /// A decode that lands after Cancel must neither reopen Camera Raw nor leave a document.
    #[test]
    fn cancel_while_other_photos_are_still_decoding_leaves_no_session_behind() {
        let (mut app, dng) = raw_app();
        app.background_jobs = true;
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        app.open_bytes("a.dng", &dng).unwrap();
        pump(&mut app, &ctx, |app| app.pending_raw_open.is_some());
        frame(&mut app, &ctx, WINDOW, vec![]);
        assert!(app.camera_raw.is_some(), "photo A is under review");
        app.open_bytes("b.dng", &dng).unwrap();
        app.open_bytes("c.dng", &dng).unwrap();
        assert_eq!(app.jobs.opens.len(), 2, "B and C are still decoding");
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        assert_eq!(r["cancelled"], true);
        pump(&mut app, &ctx, |app| app.jobs.opens.is_empty() && !app.session.has_jobs());
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        assert!(
            app.camera_raw.is_none() && app.pending_raw_open.is_none() && app.queued_raw_opens.is_empty(),
            "a late decode must not resurrect the cancelled session"
        );
        assert_eq!(app.session.documents().len(), 0, "no document survives Cancel, however late its decode lands");
    }

    /// C / FS-025, T18: two photos with the same file name in different folders each get their own
    /// sidecar; the third has none and must not inherit another photo's settings. Sidecars are only read.
    #[test]
    fn same_stem_in_different_directories_never_shares_a_sidecar() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("photocraft-same-stem-{}-{unique}", std::process::id()));
        let packet = |exposure: &str| {
            format!(
                "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:RDF><rdf:Description xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" crs:Exposure2012=\"{exposure}\"/></rdf:RDF></x:xmpmeta>"
            )
        };
        let paths: Vec<_> = ["x", "y", "z"].iter().map(|d| root.join(d).join("IMG_0001.dng")).collect();
        for (dir, exposure) in [("x", Some("0.75")), ("y", Some("-0.5")), ("z", None)] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            if let Some(e) = exposure {
                std::fs::write(root.join(dir).join("IMG_0001.xmp"), packet(e)).unwrap();
            }
        }
        let before: Vec<_> = ["x", "y"].iter().map(|d| std::fs::read(root.join(d).join("IMG_0001.xmp")).unwrap()).collect();
        for name in ["IMG_0001.dng", "IMG_0001.dng", "IMG_0001.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let ids = [app.pending_raw_open.as_ref().unwrap().document, app.queued_raw_opens[0].document, app.queued_raw_opens[1].document];
        app.pending_raw_open.as_mut().unwrap().path = Some(paths[0].to_string_lossy().into_owned());
        app.queued_raw_opens[0].path = Some(paths[1].to_string_lossy().into_owned());
        app.queued_raw_opens[1].path = Some(paths[2].to_string_lossy().into_owned());
        start_next_raw_review(&mut app, &ctx);
        let exposure = |app: &PhotocraftApp| app.camera_raw.as_ref().unwrap().params.exposure;
        assert_eq!((active_document(&app), exposure(&app)), (ids[0], 0.75));
        select_raw_review(&mut app, &ctx, ids[1]).unwrap();
        assert_eq!((active_document(&app), exposure(&app)), (ids[1], -0.5), "same stem, other folder: its own sidecar");
        select_raw_review(&mut app, &ctx, ids[2]).unwrap();
        assert_eq!((active_document(&app), exposure(&app)), (ids[2], 0.0), "no sidecar: nothing inherited from the photo before");
        select_raw_review(&mut app, &ctx, ids[0]).unwrap();
        assert_eq!((active_document(&app), exposure(&app)), (ids[0], 0.75), "back to the first, still its own");
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        let after: Vec<_> = ["x", "y"].iter().map(|d| std::fs::read(root.join(d).join("IMG_0001.xmp")).unwrap()).collect();
        assert_eq!(before, after, "reading, switching and Cancel never write a sidecar");
        assert!(!root.join("z").join("IMG_0001.xmp").exists(), "and none is created");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// C / T03, FS-024: whatever order the decodes finish in, every Filmstrip entry stays bound to
    /// the document that was decoded from its own bytes (checked against a standalone decode).
    #[test]
    fn out_of_order_decode_completion_keeps_each_name_bound_to_its_own_document() {
        let variants = [("c.dng", [0.40, 1.0, 0.90]), ("a.dng", [0.50, 1.0, 0.70]), ("b.dng", [0.85, 1.0, 0.45])];
        let expected: Vec<(&str, f32)> = variants
            .iter()
            .map(|(name, neutral)| {
                let (mut solo, _) = raw_app();
                solo.open_bytes(name, &neutral_dng(*neutral)).unwrap();
                let id = solo.pending_raw_open.as_ref().unwrap().document;
                (*name, mean_red_of(&solo, id))
            })
            .collect();
        assert!(
            expected[0].1 != expected[1].1 && expected[1].1 != expected[2].1 && expected[0].1 != expected[2].1,
            "the three fixtures must look different: {expected:?}"
        );
        let (mut app, _) = raw_app();
        app.background_jobs = true;
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        for (name, neutral) in &variants {
            app.open_bytes(name, &neutral_dng(*neutral)).unwrap();
        }
        pump(&mut app, &ctx, |app| app.jobs.opens.is_empty() && !app.session.has_jobs());
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        let raws = filmstrip_raws(&app);
        assert_eq!(raws.len(), 3);
        for raw in &raws {
            let want = expected.iter().find(|(n, _)| *n == raw.name).unwrap().1;
            assert_eq!(mean_red_of(&app, raw.document), want, "{} is bound to another photo's document", raw.name);
        }
        let mut docs: Vec<_> = raws.iter().map(|r| r.document).collect();
        docs.sort_by_key(|d| d.0);
        docs.dedup();
        assert_eq!(docs.len(), 3, "three photos, three distinct documents");
        println!("INTAKE c,a,b -> filmstrip order {:?}", raws.iter().map(|r| r.name.as_str()).collect::<Vec<_>>());
    }

    /// A: Cancel ends the session's geometry and selection, but the user's layout preferences stay.
    #[test]
    fn cancel_leaves_no_stale_strip_geometry_and_keeps_only_the_layout_preferences() {
        let (mut app, ctx, _) = review_session();
        set_view(&mut app, &ctx, json!({"orientation": "horizontal", "height": 200, "width": 260, "visible": false}));
        assert!(!app.raw_filmstrip.frame.cards.is_empty() || !app.raw_filmstrip.view.visible);
        set_view(&mut app, &ctx, json!({"visible": true}));
        assert_eq!(app.raw_filmstrip.frame.cards.len(), 3);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        let f = &app.raw_filmstrip.frame;
        assert!(
            f.cards.is_empty() && f.layout.strip.is_none() && f.layout.splitter.is_none() && f.toggle.is_none() && f.menu.is_empty(),
            "no stale rectangles after Cancel"
        );
        let v = app.raw_filmstrip.view;
        assert_eq!(
            (v.orientation, v.preferred_height, v.preferred_width),
            (filmstrip_ui::Orientation::Horizontal, 200.0, 260.0),
            "the layout preferences survive the session"
        );
    }

    /// B (T08, P1-04): the first selection is only the active photo, and Open/Cancel ignore it.
    #[test]
    fn initial_selection_is_the_active_photo_and_describe_reports_it() {
        let (app, _ctx, ids) = review_session();
        assert_eq!(selected(&app), vec![ids[0]]);
        let d = describe_active_raw(&app);
        assert_eq!(
            (d["selection"]["selected"].clone(), d["selection"]["active"].clone(), d["selection"]["count"].clone()),
            (json!([ids[0].0]), json!(ids[0].0), json!(1))
        );
        assert_eq!(d["filmstrip"][0]["selected"], true);
        assert_eq!(d["filmstrip"][1]["selected"], false);
    }

    /// B: a plain click selects only that photo and shows it.
    #[test]
    fn plain_click_selects_only_the_clicked_photo_and_activates_it() {
        let (mut app, ctx, ids) = review_session();
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        click_card(&mut app, &ctx, ids[1]);
        assert_eq!((active_document(&app), selected(&app), anchor_doc(&app)), (ids[1], vec![ids[1]], Some(ids[1])));
    }

    /// B (T08, P1-06): Ctrl/Cmd-click flips one member and never changes the shown photo.
    #[test]
    fn ctrl_click_toggles_one_member_without_touching_the_active_photo_or_others() {
        let (mut app, ctx, ids) = review_session();
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], vec![ids[0], ids[2]]));
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], vec![ids[0]]));
        click_card_mod(&mut app, &ctx, ids[0], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], vec![]), "the active photo can be active but not selected");
    }

    /// B (T08, P1-06): Shift-click selects the inclusive range in display order from the anchor.
    #[test]
    fn shift_range_uses_display_order_from_the_anchor() {
        let (mut app, ctx, ids) = review_session();
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::SHIFT);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], ids.to_vec()));
        click_card_mod(&mut app, &ctx, ids[1], egui::Modifiers::SHIFT);
        assert_eq!(selected(&app), vec![ids[0], ids[1]], "the range follows the anchor, not the previous click");
    }

    /// B (T09, P1-05), D2: stepping the preview with the arrows keeps the selected set. With a group selected the arrows
    /// step through the selected photos only (this test replaces the old focus-then-Enter expectation, which
    /// moved the preview to the unselected photo between them); each photo keeps its own settings.
    #[test]
    fn navigation_does_not_clear_multi_selection() {
        let (mut app, ctx, ids) = review_session();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.5}}})).unwrap().unwrap();
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        let (back, forward) = arrow_keys(&app);
        key(&mut app, &ctx, forward);
        assert_eq!(active_document(&app), ids[2], "the next selected photo is shown, B between them is skipped");
        assert_eq!(selected(&app), vec![ids[0], ids[2]], "A and C stay selected");
        assert_eq!(app.camera_raw.as_ref().unwrap().params.exposure, 0.0, "C has its own settings");
        key(&mut app, &ctx, back);
        assert_eq!(active_document(&app), ids[0]);
        assert_eq!(app.camera_raw.as_ref().unwrap().params.exposure, 0.5, "A kept its own settings");
        assert_eq!(app.session.documents().len(), 3);
    }
    /// `n` queued RAWs with the first under review, drawn for two frames.
    fn review_session_n(n: usize) -> (PhotocraftApp, egui::Context, Vec<photocraft_doc::DocId>) {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        for i in 0..n {
            app.open_bytes(&format!("photo{i}.dng"), &dng).unwrap();
        }
        let mut ids = vec![app.pending_raw_open.as_ref().unwrap().document];
        ids.extend(app.queued_raw_opens.iter().map(|raw| raw.document));
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        (app, ctx, ids)
    }

    /// Select exactly `members` (display order) and show `active`: plain-click `active`, then Ctrl-click the others.
    fn select_group(app: &mut PhotocraftApp, ctx: &egui::Context, members: &[photocraft_doc::DocId], active: photocraft_doc::DocId) {
        click_card(app, ctx, active);
        for id in members.iter().filter(|id| **id != active) {
            click_card_mod(app, ctx, *id, egui::Modifiers::COMMAND);
        }
        assert_eq!(active_document(app), active);
        assert_eq!(selected(app), members.to_vec());
    }

    /// D2-02: the active photo is Ctrl-removed from a group, photos remain below: the next selected one below is shown.
    #[test]
    fn d2_02_ctrl_removing_the_active_photo_shows_the_next_selected_below() {
        let (mut app, ctx, ids) = review_session_n(3);
        select_group(&mut app, &ctx, &ids, ids[1]);
        click_card_mod(&mut app, &ctx, ids[1], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[2], vec![ids[0], ids[2]]));
    }

    /// D2-03: nothing selected below the removed active photo: the nearest selected above is shown.
    #[test]
    fn d2_03_ctrl_removing_the_last_selected_active_photo_shows_the_one_above() {
        let (mut app, ctx, ids) = review_session_n(3);
        select_group(&mut app, &ctx, &ids, ids[2]);
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[1], vec![ids[0], ids[1]]));
    }

    /// D2-04: an unselected photo between is skipped and never becomes active.
    #[test]
    fn d2_04_the_fallback_skips_an_unselected_gap() {
        let (mut app, ctx, ids) = review_session_n(4);
        let group = [ids[0], ids[1], ids[3]];
        select_group(&mut app, &ctx, &group, ids[1]);
        click_card_mod(&mut app, &ctx, ids[1], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[3], vec![ids[0], ids[3]]));
    }

    /// D2-05: gapped group, the first photo is active and removed: the next selected one is shown.
    #[test]
    fn d2_05_removing_the_first_active_photo_of_a_gapped_group_shows_the_next_selected() {
        let (mut app, ctx, ids) = review_session_n(4);
        select_group(&mut app, &ctx, &[ids[0], ids[3]], ids[0]);
        click_card_mod(&mut app, &ctx, ids[0], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[3], vec![ids[3]]));
    }

    /// D2-06 (human capture needed): removing a selected photo that is not the active one leaves the active photo alone.
    /// Pins today's behaviour; not an Adobe claim.
    #[test]
    fn d2_06_removing_an_inactive_selected_photo_keeps_the_active_photo() {
        let (mut app, ctx, ids) = review_session_n(3);
        select_group(&mut app, &ctx, &ids, ids[0]);
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], vec![ids[0], ids[1]]));
    }

    /// D2-07 (human capture needed): removing the only selected photo is unchanged: nothing selected, the same photo shown.
    #[test]
    fn d2_07_removing_the_only_selected_photo_is_unchanged() {
        let (mut app, ctx, ids) = review_session_n(3);
        click_card_mod(&mut app, &ctx, ids[0], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], vec![]));
    }

    /// D2-08 (human capture needed): Ctrl-adding a photo grows the group and does not change the active photo.
    #[test]
    fn d2_08_ctrl_adding_a_photo_keeps_the_active_photo() {
        let (mut app, ctx, ids) = review_session_n(3);
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], vec![ids[0], ids[2]]));
    }

    fn arrow_keys(app: &PhotocraftApp) -> (egui::Key, egui::Key) {
        match app.raw_filmstrip.view.orientation {
            filmstrip_ui::Orientation::Vertical => (egui::Key::ArrowUp, egui::Key::ArrowDown),
            filmstrip_ui::Orientation::Horizontal => (egui::Key::ArrowLeft, egui::Key::ArrowRight),
        }
    }

    /// D2-10: with a group selected and a thumbnail focused, the arrows step the shown photo through the selected
    /// photos only; the group is never changed; the ends stop (wrap is unverified and stays unchanged: no wrap).
    #[test]
    fn d2_10_arrows_walk_the_selected_photos_only_and_keep_the_group() {
        let (mut app, ctx, ids) = review_session_n(4);
        let group = [ids[0], ids[1], ids[3]];
        select_group(&mut app, &ctx, &group, ids[1]);
        let (back, forward) = arrow_keys(&app);
        key(&mut app, &ctx, forward);
        assert_eq!((active_document(&app), selected(&app)), (ids[3], group.to_vec()), "forward skips the unselected photo");
        assert_eq!(focus_doc(&app), Some(ids[3]), "keyboard focus follows the shown photo");
        key(&mut app, &ctx, forward);
        assert_eq!((active_document(&app), selected(&app)), (ids[3], group.to_vec()), "the end stops");
        key(&mut app, &ctx, back);
        key(&mut app, &ctx, back);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], group.to_vec()));
        key(&mut app, &ctx, back);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], group.to_vec()), "the start stops");
        assert_eq!(app.session.documents().len(), 4);
    }

    /// D2-16: switching the Filmstrip orientation and collapsing it keeps the group, the shown photo and each photo's
    /// own settings (the horizontal arrow keys are covered by d2_10c).
    #[test]
    fn d2_16_orientation_and_collapse_keep_the_group_the_active_photo_and_settings() {
        let (mut app, ctx, ids) = review_session_n(4);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.75}}})).unwrap().unwrap();
        let group = [ids[0], ids[1], ids[3]];
        select_group(&mut app, &ctx, &group, ids[0]);
        let anchor = anchor_doc(&app);
        set_view(&mut app, &ctx, json!({"orientation": "horizontal"}));
        assert_eq!((active_document(&app), selected(&app)), (ids[0], group.to_vec()));
        set_view(&mut app, &ctx, json!({"visible": false}));
        set_view(&mut app, &ctx, json!({"visible": true, "orientation": "vertical"}));
        assert_eq!((active_document(&app), selected(&app), anchor_doc(&app)), (ids[0], group.to_vec(), anchor));
        assert_eq!(app.camera_raw.as_ref().unwrap().params.exposure, 0.75, "the shown photo kept its settings");
        click_card_mod(&mut app, &ctx, ids[0], egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (ids[1], vec![ids[1], ids[3]]));
    }

    /// D2-10 on a horizontal strip: Left/Right step the shown photo through the group. (A card loses keyboard focus when
    /// the orientation changes because its widget id follows the scroll area, which already was the case before D2; the
    /// next click focuses one again, so this test switches the orientation first.)
    #[test]
    fn d2_10c_left_and_right_walk_the_selected_photos_on_a_horizontal_strip() {
        let (mut app, ctx, ids) = review_session_n(4);
        set_view(&mut app, &ctx, json!({"orientation": "horizontal"}));
        let group = [ids[0], ids[1], ids[3]];
        select_group(&mut app, &ctx, &group, ids[1]);
        let (back, forward) = arrow_keys(&app);
        assert_eq!((back, forward), (egui::Key::ArrowLeft, egui::Key::ArrowRight));
        key(&mut app, &ctx, forward);
        assert_eq!((active_document(&app), selected(&app)), (ids[3], group.to_vec()));
        key(&mut app, &ctx, back);
        key(&mut app, &ctx, back);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], group.to_vec()));
    }

    /// D2-12: a focused adjustment field keeps its arrows even while a group is selected: neither the shown photo nor
    /// the group changes.
    #[test]
    fn d2_12_a_focused_field_keeps_its_arrows_with_a_group_selected() {
        let (mut app, ctx, ids) = review_session();
        click_card(&mut app, &ctx, ids[1]);
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        frame(&mut app, &ctx, WINDOW, vec![]);
        assert_eq!(selected(&app), vec![ids[1], ids[2]]);
        let field = adjustment_field(&mut app, &ctx, "Contrast");
        click(&mut app, &ctx, field.center());
        frame(&mut app, &ctx, WINDOW, vec![]);
        let contrast = |app: &PhotocraftApp| app.camera_raw.as_ref().unwrap().params.contrast;
        key(&mut app, &ctx, egui::Key::ArrowUp);
        assert_eq!(contrast(&app), 1.0, "the field handled the arrow");
        key(&mut app, &ctx, egui::Key::ArrowDown);
        assert_eq!(contrast(&app), 0.0);
        assert_eq!((active_document(&app), selected(&app)), (ids[1], vec![ids[1], ids[2]]));
    }

    /// Ctrl+A with a thumbnail focused selects every photo; the arrows then step through all of them.
    #[test]
    fn d2_ctrl_a_then_arrows_step_through_every_photo() {
        let (mut app, ctx, ids) = review_session_n(4);
        click_card(&mut app, &ctx, ids[1]);
        key_mod(&mut app, &ctx, egui::Key::A, egui::Modifiers::COMMAND);
        assert_eq!(selected(&app), ids);
        let (back, forward) = arrow_keys(&app);
        key(&mut app, &ctx, forward);
        assert_eq!((active_document(&app), selected(&app)), (ids[2], ids.clone()));
        key(&mut app, &ctx, back);
        key(&mut app, &ctx, back);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], ids.clone()));
    }

    /// Shift+arrow is not plain stepping: with a group selected it changes neither the shown photo nor the group.
    #[test]
    fn d2_shift_arrow_does_not_step_the_shown_photo() {
        let (mut app, ctx, ids) = review_session_n(3);
        select_group(&mut app, &ctx, &ids, ids[0]);
        let (_, forward) = arrow_keys(&app);
        key_mod(&mut app, &ctx, forward, egui::Modifiers::SHIFT);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], ids.clone()));
    }
    /// With one photo (or none) selected the arrows keep their old job: move keyboard focus, never the shown photo.
    #[test]
    fn d2_10b_arrows_with_a_single_selection_only_move_focus() {
        let (mut app, ctx, ids) = review_session_n(3);
        click_card(&mut app, &ctx, ids[1]);
        let (back, forward) = arrow_keys(&app);
        key(&mut app, &ctx, forward);
        assert_eq!(active_document(&app), ids[1]);
        assert_eq!(focus_doc(&app), Some(ids[2]));
        key(&mut app, &ctx, back);
        assert_eq!(focus_doc(&app), Some(ids[1]));
    }
    /// B / FS-028: Ctrl+A selects every photo only while a thumbnail has focus.
    #[test]
    fn select_all_needs_filmstrip_focus_and_leaves_text_fields_alone() {
        let (mut app, ctx, ids) = review_session();
        click_card(&mut app, &ctx, ids[0]);
        key_mod(&mut app, &ctx, egui::Key::A, egui::Modifiers::COMMAND);
        assert_eq!(selected(&app), ids.to_vec());
        click_card(&mut app, &ctx, ids[1]);
        assert_eq!(selected(&app), vec![ids[1]]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        let field = adjustment_field(&mut app, &ctx, "Contrast");
        click(&mut app, &ctx, field.center());
        frame(&mut app, &ctx, WINDOW, vec![]);
        key_mod(&mut app, &ctx, egui::Key::A, egui::Modifiers::COMMAND);
        assert_eq!(selected(&app), vec![ids[1]], "a focused text field keeps its own Select All");
    }

    /// B: selection is session state. Cancel clears it; Open Images still opens every photo.
    #[test]
    fn open_images_and_cancel_ignore_the_selection_and_the_next_session_starts_fresh() {
        let (mut app, ctx, ids) = review_session();
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        let photo = photo_of(&app, ids[2]);
        app.raw_filmstrip.selection.select_only(photo);
        let result = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(result["batch"], 3, "Open Images still accepts the whole batch, not just the selection: {result}");
        for _ in 0..5 {
            advance_batch_open(&mut app, &ctx);
            if !app.raw_filmstrip.batch_open_all {
                break;
            }
        }
        assert_eq!(app.session.documents().len(), 3);
        assert_eq!(app.raw_filmstrip.selection, selection_ui::Selection::default(), "the session ended, so its selection is gone");
        let (mut app, ctx, ids) = review_session();
        let photos: Vec<PhotoId> = ids.iter().map(|id| photo_of(&app, *id)).collect();
        app.raw_filmstrip.selection.select_all(&photos);
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        assert_eq!(r["cancelled"], true);
        assert_eq!((app.session.documents().len(), app.raw_filmstrip.selection.clone()), (0, selection_ui::Selection::default()));
    }

    // ---- Stage 2A regressions from the Q2 adversarial probes (A-1, A-1b, A-3, S07) --------------------------------
    // Each asserts the safe behaviour. A failing "CONTROL" or "PROBE INVALID" assertion means the set-up itself is wrong,
    // not the behaviour under test.

    /// What `stage2a_redevelop_event_after_a_lower_tab_closed` leaves behind.
    struct Stage2aStale {
        app: PhotocraftApp,
        ctx: egui::Context,
        /// B: an accepted document with unsaved work, to the right of the RAW.
        b: photocraft_doc::DocId,
        /// B's `(revision, saved_revision)` before the commit's event.
        b_before: (u64, u64),
        /// R': the re-developed document the commit's apply created.
        replacement: photocraft_doc::DocId,
        /// R''s history length right after the apply.
        replacement_steps: usize,
    }

    /// Q2 A-1 harness. Tabs [A, RAW, B]; B is an accepted document with unsaved work. The RAW's re-develop is applied
    /// exactly as `poll_jobs` applies it (`replace_raw_document`: R' at tab 1, result `{"document": 1}`) while the shell
    /// still holds the commit (`raw_redevelop`). With `shift`, tab A closes before the commit's event is handled: what a
    /// background save of A (its close prompt answered Save) that lands in the same poll does first
    /// (`jobs_ui::finish_save` -> `discard_ui::saved_document` -> `advance` -> `file.close`).
    fn stage2a_redevelop_event_after_a_lower_tab_closed(params: CameraRaw, shift: bool) -> Stage2aStale {
        use photocraft_engine::jobs::{JobEvent, JobId, JobOutcome};
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        app.open_bytes("held.dng", &dng).unwrap();
        app.run("file.new", json!({"width": 12, "height": 12})).unwrap();
        // B's unsaved work (engine calls: the shell guard is not what this probe tests).
        app.session.execute("layer.new.layer", json!({})).unwrap();
        app.session.execute("edit.fill", json!({"color": "#808080"})).unwrap();
        let b = app.session.documents()[2].doc.id;
        let b_before = (app.session.documents()[2].revision, app.session.documents()[2].saved_revision);
        assert!(app.session.documents()[2].is_dirty(), "PROBE INVALID: B must hold unsaved work");
        let raw = app.pending_raw_open.take().expect("PROBE INVALID: the RAW waits for review");
        assert_eq!(document_index(&app, raw.document), Some(1), "PROBE INVALID: the tabs must be [A, RAW, B]");
        let tuning = photocraft_io::raw::RawTuning { exposure: params.exposure, ..Default::default() };
        let (developed, wb) = photocraft_io::raw::import_raw_tuned("held.dng", &dng, &tuning).unwrap();
        let generation = app.session.review_generation();
        app.raw_redevelop = Some(Redevelop { job: JobId(5), params: params.clone(), raw: raw.clone(), filter_step: false });
        let reply = replace_raw_document(&mut app.session, raw.document, developed, wb, generation).unwrap();
        assert_eq!(reply["document"], 1, "PROBE INVALID: the replacement's apply-time tab index");
        let replacement = app.session.documents()[1].doc.id;
        let replacement_steps = app.session.documents()[1].history.past_len();
        if shift {
            app.run("file.close", json!({"document": 0})).unwrap();
            assert_eq!(document_index(&app, replacement), Some(0), "PROBE INVALID: closing A moved R' to tab 0");
        }
        let event = JobEvent {
            id: JobId(5),
            command: REDEVELOP_JOB.into(),
            label: "Developing held.dng".into(),
            document: Some(raw.document),
            outcome: JobOutcome::Done(reply),
        };
        assert!(on_redevelop_event(&mut app, &event), "PROBE INVALID: the event is the commit's");
        Stage2aStale { app, ctx, b, b_before, replacement, replacement_steps }
    }

    /// Q2 A-1 (dirty flag). The commit resolves its document by id, so a tab closed in the same poll cannot redirect it.
    #[test]
    fn stage2a_a1_a_commit_event_after_a_lower_tab_closed_never_marks_another_document_saved() {
        for shift in [false, true] {
            let label = if shift { "Q2-A1 (tab A closed between the commit's apply and its event)" } else { "CONTROL (no tab moved)" };
            let run = stage2a_redevelop_event_after_a_lower_tab_closed(CameraRaw { exposure: 1.0, ..CameraRaw::default() }, shift);
            let st = run.app.session.documents().iter().find(|st| st.doc.id == run.b).expect("B is still open");
            assert!(
                st.is_dirty() && (st.revision, st.saved_revision) == run.b_before,
                "{label}: B, an accepted document Camera Raw never owned, was marked saved by another photograph's Open: (revision, saved) {:?} -> {:?}",
                run.b_before,
                (st.revision, st.saved_revision)
            );
        }
    }

    /// Q2 A-1 (Camera Raw step). The remaining settings land on R', never on B.
    #[test]
    fn stage2a_a1_a_commit_event_after_a_lower_tab_closed_applies_the_camera_raw_step_to_its_own_photograph() {
        for shift in [false, true] {
            let label = if shift { "Q2-A1 (tab A closed between the commit's apply and its event)" } else { "CONTROL (no tab moved)" };
            let params = CameraRaw { exposure: 1.0, vignette_amount: -40.0, ..CameraRaw::default() };
            let run = stage2a_redevelop_event_after_a_lower_tab_closed(params, shift);
            let docs = run.app.session.documents();
            let b_revision = docs.iter().find(|st| st.doc.id == run.b).map(|st| st.revision);
            let r_steps = docs.iter().find(|st| st.doc.id == run.replacement).map(|st| st.history.past_len());
            let b_held = unaccepted_documents(&run.app).contains(&run.b);
            assert!(
                b_revision == Some(run.b_before.0) && !b_held && r_steps.is_some_and(|n| n > run.replacement_steps),
                "{label}: B revision {:?} -> {b_revision:?}, B held {b_held}, R' history {} -> {r_steps:?}",
                run.b_before.0,
                run.replacement_steps
            );
        }
    }

    /// Q2 A-1b (Done-then-Err). A failed step brings back R', never the accepted B:
    /// B is never parked as the recovered review, Cancel never closes B, and R' does not outlive Cancel.
    #[test]
    fn stage2a_a1b_a_failed_step_after_a_lower_tab_closed_never_takes_an_accepted_document_into_review() {
        for shift in [false, true] {
            let label = if shift { "Q2-A1b (tab A closed between the commit's apply and its event)" } else { "CONTROL (no tab moved)" };
            let mut run = stage2a_redevelop_event_after_a_lower_tab_closed(failing_step_params(), shift);
            let b_held = unaccepted_documents(&run.app).contains(&run.b);
            let r_held = unaccepted_documents(&run.app).contains(&run.replacement);
            // What the photographer then sees and does: the review that comes back, then Cancel.
            start_next_raw_review(&mut run.app, &run.ctx);
            let reviewing = run.app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.document);
            cancel(&mut run.app);
            let b_open = document_index(&run.app, run.b).is_some();
            let r_open = document_index(&run.app, run.replacement).is_some();
            assert!(
                !b_held && r_held && reviewing == Some(run.replacement) && b_open && !r_open,
                "{label}: B held {b_held}, R' held {r_held}, review reopened on {reviewing:?} (B {:?}, R' {:?}); after Cancel B open {b_open}, R' open {r_open}",
                run.b,
                run.replacement
            );
        }
    }

    /// Q2 A-1 through production code only (no direct state edits). A has unsaved work and its close prompt was answered
    /// Save, so A is written in the background (gated: still writing when Open is clicked); the RAW is under review; B is an
    /// accepted document with unsaved work to its right. Open with Exposure starts the re-develop. With `same_poll`, the
    /// save and the re-develop are applied in one poll, save first (its job is older: `running` order), and both events
    /// are handled in one tick; otherwise A's save and close are handled before Open is clicked.
    /// Returns (B still has unsaved work, sanity: A closed and the RAW replaced, documents left).
    fn stage2a_save_close_and_open(same_poll: bool) -> (bool, bool, usize) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        let a = app.session.documents()[0].doc.id;
        app.open_bytes("held.dng", &dng).unwrap();
        let old = app.pending_raw_open.as_ref().map(|raw| raw.document).expect("PROBE INVALID: the RAW waits for review");
        app.run("file.new", json!({"width": 12, "height": 12})).unwrap();
        app.session.execute("layer.new.layer", json!({})).unwrap();
        let b = app.session.documents()[2].doc.id;
        assert!(app.session.set_active(0), "PROBE INVALID: A is tab 0");
        assert!(crate::discard_ui::intercept(&mut app, "file.close", &json!({"document": 0})), "PROBE INVALID: closing A asks first");
        let gate = std::sync::Arc::new(AtomicBool::new(false));
        let worker_gate = gate.clone();
        app.services.save_file = Some(std::sync::Arc::new(move |_, _, _, ctl| {
            while !worker_gate.load(Ordering::SeqCst) {
                ctl.check().map_err(|e| e.to_string())?;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(Vec::new())
        }));
        let save = app.services.save_file.clone().expect("PROBE INVALID: the save function was just set");
        let started = crate::jobs_ui::start_save(&mut app, "/q2/a.psd".to_string(), crate::ExportSettings::default(), false, save, false).unwrap();
        assert!(started.is_none(), "PROBE INVALID: the save runs in the background");
        let save_job = app.jobs.saves[0].job;
        let raw = take_next_raw_open(&mut app).expect("PROBE INVALID: the review opens");
        open_raw(&mut app, &ctx, raw).unwrap();
        app.background_jobs = true;
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.0}}})).unwrap().unwrap();
        if !same_poll {
            gate.store(true, Ordering::SeqCst);
            app.session.wait_job(save_job).unwrap();
            crate::jobs_ui::tick(&mut app, &ctx);
        }
        let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(reply["pending"], true, "PROBE INVALID: Open runs the re-develop as a job: {reply}");
        let commit = app.raw_redevelop.as_ref().map(|c| c.job).expect("PROBE INVALID: the commit job is held");
        if same_poll {
            gate.store(true, Ordering::SeqCst);
            // poll_jobs applies ready jobs in `running` order (the save started first), then hands back both events.
            app.session.wait_job(save_job).unwrap();
        }
        app.session.wait_job(commit).unwrap();
        crate::jobs_ui::tick(&mut app, &ctx);
        sync_provisional(&mut app);
        let docs = app.session.documents();
        let sane = docs.len() == 2 && docs.iter().all(|st| st.doc.id != a && st.doc.id != old);
        let b_dirty = docs.iter().find(|st| st.doc.id == b).is_some_and(|st| st.is_dirty());
        (b_dirty, sane, docs.len())
    }

    /// Q2 A-1, production path. B keeps its unsaved work.
    #[test]
    fn stage2a_a1_full_a_same_poll_save_close_and_open_never_mark_another_tab_saved() {
        let (b_dirty, sane, left) = stage2a_save_close_and_open(false);
        assert!(sane, "PROBE INVALID (control): A must be closed by its save and the RAW replaced ({left} documents left)");
        assert!(b_dirty, "CONTROL: A closed in an earlier poll, so B keeps its unsaved work");
        let (b_dirty, sane, left) = stage2a_save_close_and_open(true);
        assert!(sane, "PROBE INVALID: A must be closed by its save and the RAW replaced ({left} documents left)");
        assert!(
            b_dirty,
            "Q2-A1 (production path): A's save-close and the RAW's re-develop landed in one poll; the re-develop's apply-time tab index then named B and marked its unsaved work saved"
        );
    }

    /// Q2 A-3 through the shell: the review's Cancel, then the preference switched off while a renamed RAW of the same
    /// selection is still decoding. Nothing of the cancelled review lands.
    #[test]
    fn stage2a_a3_a_renamed_raw_still_decoding_at_cancel_never_lands_after_review_on_open_is_switched_off() {
        for flip in [false, true] {
            let label = if flip { "Q2-A3 (preference switched off after Cancel, before the decode lands)" } else { "CONTROL (preference unchanged)" };
            let (mut app, dng) = raw_app();
            let ctx = egui::Context::default();
            app.open_bytes("review.dng", &dng).unwrap();
            let raw = take_next_raw_open(&mut app).unwrap();
            open_raw(&mut app, &ctx, raw).unwrap();
            app.background_jobs = true;
            app.open_bytes("scan.dat", &dng).unwrap();
            let job = app.jobs.opens[0].job;
            cancel(&mut app);
            assert!(app.session.job(job).is_some(), "PROBE INVALID: Cancel stops only opens with RAW-looking names, so scan.dat still runs");
            if flip {
                app.run("prefs.set", json!({"path": "rawDefaults.openInCameraRaw", "value": false})).unwrap();
            }
            let _ = app.session.wait_job(job);
            crate::jobs_ui::tick(&mut app, &ctx);
            sync_provisional(&mut app);
            let left: Vec<String> = app.session.documents().iter().map(|st| st.doc.name.clone()).collect();
            assert!(
                left.is_empty() && queued_ids(&app).is_empty() && !app.session.has_provisional(),
                "{label}: a photograph of the cancelled review survived Cancel: {left:?}"
            );
        }
    }

    /// Q2 S07: a renamed RAW of a cancelled review that lands while the next review is already open neither joins nor
    /// revives anything. The engine apply supersedes it by generation.
    #[test]
    fn stage2a_s07_a_late_renamed_raw_of_a_cancelled_review_does_not_join_the_next_review() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("first.dng", &dng).unwrap();
        let raw = take_next_raw_open(&mut app).unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        app.background_jobs = true;
        app.open_bytes("scan.dat", &dng).unwrap();
        let late = app.jobs.opens[0].job;
        cancel(&mut app);
        // The next review starts at once.
        app.background_jobs = false;
        app.open_bytes("second.dng", &dng).unwrap();
        let second = take_next_raw_open(&mut app).expect("the new review is queued");
        let second_id = second.document;
        open_raw(&mut app, &ctx, second).unwrap();
        let _ = app.session.wait_job(late);
        crate::jobs_ui::tick(&mut app, &ctx);
        sync_provisional(&mut app);
        start_next_raw_review(&mut app, &ctx);
        let ids: Vec<_> = app.session.documents().iter().map(|st| st.doc.id).collect();
        assert_eq!(ids, vec![second_id], "Q2-S07: only the new review's photograph exists");
        assert!(queued_ids(&app).is_empty() && app.raw_filmstrip.parked.is_empty(), "Q2-S07: nothing joined the new review");
        assert_eq!(app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.document), Some(second_id));
        assert_eq!(unaccepted_documents(&app), vec![second_id]);
    }

    #[test]
    fn preview_texture_uses_linear_document_display_encoding() {
        let mut doc = photocraft_doc::Document::new("test", photocraft_geom::Size::new(1, 1), photocraft_doc::ColorMode::Rgb, photocraft_doc::SampleType::F32);
        doc.icc_profile = Some(photocraft_cms::Builtin::LinearSrgb.profile().to_bytes());
        let mut color = photocraft_engine::color_cmds::ColorState::default();
        color.settings.monitor_profile = "srgb".into();
        let display = color.canvas_display(&doc).unwrap();
        let px = [[0.25, 0.25, 0.25, 1.0]];
        let preview = CameraRawDialog::image(&px, 1, 1, &display);
        let expected = display.to_rgba8(&photocraft_compose::Buffer::filled(Rect::new(0, 0, 1, 1), px[0]));
        assert_eq!(preview.pixels[0].to_array(), expected.pixels[..4]);
        assert_ne!(preview.pixels[0].r(), 64, "linear light must be encoded before display");
    }

    #[test]
    fn shadows_of_a_raw_open_act_on_the_raw_data_and_the_preview_shows_it() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("IMG_0002.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        assert!(app.camera_raw.as_ref().unwrap().raw_curve.is_some(), "the preview knows how the raw was developed");
        let mean = |px: &[[f32; 4]]| px.iter().map(|p| p[0] + p[1] + p[2]).sum::<f32>() / px.len() as f32;
        let preview_before = mean(&app.camera_raw.as_ref().unwrap().processed);
        let before = mean_red(&app);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"shadows": 100}}})).unwrap().unwrap();
        let d = app.camera_raw.as_mut().unwrap();
        d.render(&ctx);
        assert!(mean(&d.processed) > preview_before + 0.01, "{preview_before} → {}", mean(&d.processed));
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(r["redeveloped"], true, "{r}");
        assert!(r.get("filter").is_none(), "Shadows acts while developing, not as a later filter step: {r}");
        assert!(mean_red(&app) > before + 0.01, "{before} → {}", mean_red(&app));
    }

    #[test]
    fn clarity_texture_and_dehaze_of_a_raw_open_act_on_the_raw_data() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.open_bytes("IMG_0003.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().unwrap();
        open_raw(&mut app, &ctx, raw).unwrap();
        let before = mean_red(&app);
        let preview_before = app.camera_raw.as_ref().unwrap().processed.clone();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"clarity": 60, "texture": 40, "dehaze": 50}}})).unwrap().unwrap();
        let d = app.camera_raw.as_mut().unwrap();
        d.render(&ctx);
        assert_ne!(d.processed, preview_before, "the preview shows the raw-stage change");
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(r["redeveloped"], true, "{r}");
        assert!(r.get("filter").is_none(), "they act while developing, not as a later filter step: {r}");
        assert!(mean_red(&app) < before, "Dehaze darkens: {before} → {}", mean_red(&app));
    }

    #[test]
    fn save_and_load_settings_round_trip_through_xmp_files() {
        use crate::file_dialog::FileDialogAnswer;
        use std::sync::{Arc, Mutex};
        type Written = Arc<Mutex<Vec<(String, Vec<u8>)>>>;
        let written: Written = Arc::default();
        let w = written.clone();
        let foreign = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
            <rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:Clarity2012="+25" crs:RedHue="-5"/>
            </rdf:RDF></x:xmpmeta>"#;
        let answers = vec![
            Some(FileDialogAnswer::SaveTo("/out/Look.xmp".into())),
            Some(FileDialogAnswer::Contents("Look.xmp".into(), Vec::new())),
            Some(FileDialogAnswer::Contents("Other.xmp".into(), foreign.to_vec())),
            Some(FileDialogAnswer::Contents("Broken.xmp".into(), b"<nope".to_vec())),
        ];
        let services = crate::Services {
            file_dialog: Some(crate::file_dialog::fake(answers).0),
            write: Some(Box::new(move |p: &str, b: &[u8]| {
                w.lock().unwrap().push((p.to_string(), b.to_vec()));
                Ok(())
            })),
            ..Default::default()
        };
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
        let ctx = egui::Context::default();
        let frame = |app: &mut PhotocraftApp| app.poll_file_dialog(&egui::Context::default(), None);
        app.run("file.new", json!({"width": 16, "height": 16})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.run("edit.fill", json!({"color": "#808080"})).unwrap();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.75, "contrast": -20, "hslSat": [0, 0, 0, 30, 0, 0, 0, 0]}}}))
            .unwrap()
            .unwrap();
        let saved_params = app.camera_raw.as_ref().unwrap().params.clone();
        assert!(menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"saveSettings": true, "before": true}})).unwrap().is_err());
        assert_eq!(menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"saveSettings": true}})).unwrap().unwrap()["fileDialog"], "save");
        frame(&mut app);
        let (path, bytes) = written.lock().unwrap()[0].clone();
        assert_eq!(path, "/out/Look.xmp");
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains("crs:Exposure2012=\"+0.75\"") && text.contains("crs:SaturationAdjustmentGreen=\"+30\""), "{text}");
        // Reset, then load the saved file back (the fake dialog's second answer, with its bytes).
        app.camera_raw.as_mut().unwrap().params = CameraRaw::default();
        let mut svc = crate::file_dialog::fake(vec![Some(FileDialogAnswer::Contents("Look.xmp".into(), bytes))]).0;
        std::mem::swap(app.services.file_dialog.as_mut().unwrap(), &mut svc);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"loadSettings": true}})).unwrap().unwrap();
        frame(&mut app);
        assert_eq!(app.camera_raw.as_ref().unwrap().params, saved_params);
        std::mem::swap(app.services.file_dialog.as_mut().unwrap(), &mut svc);
        // The original service's next answer is the empty file: an error, settings untouched.
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"loadSettings": true}})).unwrap().unwrap();
        frame(&mut app);
        assert_eq!(app.camera_raw.as_ref().unwrap().params, saved_params);
        // A foreign preset applies what it holds over the current settings and reports the rest.
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"loadSettings": true}})).unwrap().unwrap();
        frame(&mut app);
        let p = &app.camera_raw.as_ref().unwrap().params;
        assert_eq!((p.clarity, p.exposure), (25.0, 0.75));
        assert!(app.ui.status.contains("Other.xmp") && app.ui.status.contains('1'), "{}", app.ui.status);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"loadSettings": true}})).unwrap().unwrap();
        frame(&mut app);
        assert_eq!(app.camera_raw.as_ref().unwrap().params.clarity, 25.0);
    }

    // ---- GRW-1 lab cycle 1, repair round C1-R1 (Q4 cross-examination items 4 and 7) ------------------------------

    /// Q4 item 7: an admission ends with its open's event even when the admitted document left the session before it,
    /// so a later document that reuses a persisted id is never taken for a superseded review.
    #[test]
    fn c1r1_an_admission_ends_with_its_event_even_when_its_document_left_first() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        app.background_jobs = true;
        app.open_bytes("gone.dng", &dng).unwrap();
        let job = app.jobs.opens[0].job;
        app.session.wait_job(job).unwrap();
        let id = app.session.documents()[0].doc.id;
        assert!(app.session.has_provisional(), "PROBE INVALID: the decoded RAW is admitted for review");
        app.session.close(0);
        app.session.next_review_generation();
        crate::jobs_ui::tick(&mut app, &ctx);
        assert!(!app.session.admission_stale(id), "the admission ended with the event");
        // The shell refreshes its pending flag every frame (`show` -> `sync_provisional`).
        sync_provisional(&mut app);
        assert!(!app.session.has_provisional(), "nothing provisional is left");
    }

    /// Q4 item 4: Esc and the control channel's Camera Raw Cancel each supersede the review, so a renamed RAW of it that
    /// lands later (with review-on-open switched off meanwhile) never joins the session.
    #[test]
    fn c1r1_esc_and_control_cancel_supersede_a_late_renamed_raw() {
        for route in ["esc", "control"] {
            let (mut app, ctx, _) = review_session();
            let (_, dng) = raw_app();
            app.background_jobs = true;
            app.open_bytes("scan.dat", &dng).unwrap();
            let late = app.jobs.opens.last().map(|o| o.job).expect("PROBE INVALID: the renamed RAW is decoding");
            match route {
                "esc" => key(&mut app, &ctx, egui::Key::Escape),
                _ => {
                    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}}));
                    assert!(reply.as_ref().is_some_and(Result::is_ok), "PROBE INVALID: {route} Cancel is accepted: {reply:?}");
                }
            }
            assert!(app.camera_raw.is_none(), "PROBE INVALID: {route} closed the review");
            app.run("prefs.set", json!({"path": "rawDefaults.openInCameraRaw", "value": false})).unwrap();
            let _ = app.session.wait_job(late);
            crate::jobs_ui::tick(&mut app, &ctx);
            frame(&mut app, &ctx, WINDOW, vec![]);
            let left: Vec<String> = app.session.documents().iter().map(|st| st.doc.name.clone()).collect();
            assert!(left.is_empty() && app.camera_raw.is_none(), "{route}: a photograph of the cancelled review survived: {left:?}");
        }
    }

    // ---- GRW-1 lab cycle 2 (B0): a photograph keeps its identity when Open re-develops it (A1 -> A2) -----------------
    // Written against the cycle-1 API (RED there); `selected`/`order` stay DocId views of the selection.

    /// B0-R01 (sync Done-then-Err). Photos A, B, C under review; A and C selected, A shown. Open re-develops A (a new
    /// document id, A1 -> A2) and the Camera Raw step that follows fails, so A2 comes back for review. The photograph
    /// keeps its place: A2 and C stay selected, the review is A2's, the anchor is unchanged, A2 keeps its other settings
    /// and B is never touched.
    #[test]
    fn b0_r01_a_redeveloped_photo_keeps_its_place_in_the_group_when_its_step_fails() {
        let (mut app, ctx, ids) = review_session_n(3);
        let (a1, b, c) = (ids[0], ids[1], ids[2]);
        click_card_mod(&mut app, &ctx, c, egui::Modifiers::COMMAND);
        assert_eq!((active_document(&app), selected(&app)), (a1, vec![a1, c]), "PROBE INVALID: A and C selected, A shown");
        let anchor = describe_active_raw(&app)["selection"]["anchor"].clone();
        let b_revision = app.session.documents().iter().find(|st| st.doc.id == b).map(|st| st.revision);
        app.camera_raw.as_mut().unwrap().params = CameraRaw { contrast: 10.0, ..failing_step_params() };
        let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}}));
        assert!(reply.as_ref().is_some_and(Result::is_err), "PROBE INVALID: the Camera Raw step fails: {reply:?}");
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        let a2 = active_document(&app);
        assert!(a2 != a1 && document_index(&app, a1).is_none(), "PROBE INVALID: A came back under its re-developed id");
        assert_eq!(selected(&app), vec![a2, c], "B0-R01: the re-developed photograph lost its place in the group");
        let d = describe_active_raw(&app);
        assert_eq!(d["selection"]["selected"], json!([a2.0, c.0]), "B0-R01: describe agrees: {}", d["selection"]);
        assert_eq!(d["selection"]["anchor"], anchor, "B0-R01: the anchor moved");
        assert_eq!(app.camera_raw.as_ref().unwrap().params.contrast, 10.0, "A2 keeps its other settings");
        assert_eq!(app.session.documents().iter().find(|st| st.doc.id == b).map(|st| st.revision), b_revision, "B untouched");
    }

    /// B0-R03 (background Done-then-Err). The same with background jobs: Open runs the re-develop as a job and returns
    /// "pending"; the step fails after A2 landed. The group survives the pending commit (it used to be cleared then).
    #[test]
    fn b0_r03_a_background_open_whose_step_fails_keeps_the_group() {
        let (mut app, ctx, ids) = review_session_n(3);
        let (a1, c) = (ids[0], ids[2]);
        click_card_mod(&mut app, &ctx, c, egui::Modifiers::COMMAND);
        assert_eq!(selected(&app), vec![a1, c], "PROBE INVALID: A and C selected");
        app.background_jobs = true;
        app.camera_raw.as_mut().unwrap().params = CameraRaw { contrast: 10.0, ..failing_step_params() };
        let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
        assert_eq!(reply["pending"], true, "PROBE INVALID: Open runs in the background: {reply}");
        let develop = app.raw_redevelop.as_ref().map(|r| r.job).expect("PROBE INVALID: the re-develop runs");
        app.session.wait_job(develop).unwrap();
        crate::jobs_ui::tick(&mut app, &ctx);
        if let Some(step) = app.raw_redevelop.as_ref().map(|r| r.job) {
            let _ = app.session.wait_job(step);
            crate::jobs_ui::tick(&mut app, &ctx);
        }
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        let a2 = active_document(&app);
        assert!(a2 != a1 && document_index(&app, a1).is_none(), "PROBE INVALID: A came back under its re-developed id");
        assert_eq!(selected(&app), vec![a2, c], "B0-R03: the pending commit dropped the group");
    }

    /// B0-R05: a stale callback that still names A1 after the rebind changes nothing: selecting it is refused and no
    /// photograph's selection changes.
    #[test]
    fn b0_r05_a_stale_a1_reference_after_the_rebind_is_inert() {
        let (mut app, ctx, ids) = review_session_n(3);
        let (a1, c) = (ids[0], ids[2]);
        click_card_mod(&mut app, &ctx, c, egui::Modifiers::COMMAND);
        app.camera_raw.as_mut().unwrap().params = CameraRaw { contrast: 10.0, ..failing_step_params() };
        let _ = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}}));
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        let a2 = active_document(&app);
        assert!(a2 != a1, "PROBE INVALID: A was re-developed");
        let before = (selected(&app), active_document(&app), describe_active_raw(&app)["selection"].clone());
        let stale = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"selectRaw": a1.0}}));
        assert!(stale.as_ref().is_some_and(Result::is_err), "B0-R05: selecting the closed A1 must be refused: {stale:?}");
        frame(&mut app, &ctx, WINDOW, vec![]);
        assert_eq!((selected(&app), active_document(&app), describe_active_raw(&app)["selection"].clone()), before, "B0-R05: nothing changed");
        assert!(!selected(&app).contains(&a1), "B0-R05: the selection never names a closed document");
    }

    // ---- GRW-1 lab cycle 3 (F1): the Filmstrip's read-only rating -----------------------------------------------------

    /// F1: a photograph's rating is read once from its same-stem sidecar when it is queued, never written. A known rating
    /// shows its stars; a sidecar without one is unrated; no sidecar, an invalid value or a file supplied without a path
    /// is *unknown*, never shown as unrated. The sidecars are byte-for-byte unchanged afterwards.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn f1_ratings_are_read_from_the_sidecar_and_unknown_is_never_unrated() {
        let (mut app, _) = raw_app();
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("photocraft-lab-f1-ratings-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let packet = |attrs: &str| {
            format!(
                "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:RDF><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" {attrs}/></rdf:RDF></x:xmpmeta>"
            )
        };
        let cases: [(&str, Option<String>, PhotoRating); 5] = [
            ("four", Some(packet("xmp:Rating=\"4\"")), PhotoRating::Stars(4)),
            ("unrated", Some(packet("")), PhotoRating::Unrated),
            ("nosidecar", None, PhotoRating::Unknown),
            ("invalid", Some(packet("xmp:Rating=\"2.5\"")), PhotoRating::Unknown),
            ("rejected", Some(packet("xmp:Rating=\"-1\"")), PhotoRating::Rejected),
        ];
        let mut sidecars = Vec::new();
        for (stem, xmp, _) in &cases {
            if let Some(xmp) = xmp {
                let path = dir.join(format!("{stem}.xmp"));
                std::fs::write(&path, xmp).unwrap();
                sidecars.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
        for (stem, _, _) in &cases {
            app.run("file.new", json!({"width": 8, "height": 8})).unwrap();
            let id = app.session.active().unwrap().doc.id;
            let path = dir.join(format!("{stem}.dng")).to_string_lossy().into_owned();
            queue_open_dialog(&mut app, id, &format!("{stem}.dng"), Some(&path), None).unwrap();
        }
        app.run("file.new", json!({"width": 8, "height": 8})).unwrap();
        let bytes_only = app.session.active().unwrap().doc.id;
        queue_open_dialog(&mut app, bytes_only, "automation.dng", None, Some(b"not read")).unwrap();
        let ratings: Vec<PhotoRating> = filmstrip_raws(&app).iter().map(|raw| raw.rating).collect();
        let mut want: Vec<PhotoRating> = cases.iter().map(|(_, _, r)| *r).collect();
        want.push(PhotoRating::Unknown);
        assert_eq!(ratings, want, "each photograph shows exactly what its sidecar says, and unknown stays unknown");
        for (path, original) in &sidecars {
            assert_eq!(&std::fs::read(path).unwrap(), original, "{} was never written", path.display());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- GRW-1 lab cycle 4 (F3): the preview toolbar in the owner's order of 15 Oct ---------------------------------
    // `[actual zoom] [100%] [zoom presets] | [Filmstrip] [Grid] [Sort] [Filter]`, read from what egui reports to
    // accessibility (labels, rectangles, enabled state), so the tests see what a screen reader and a pointer would.

    /// The accessible widgets in the preview toolbar's row this frame (under the image, inside the preview column), left
    /// to right: (label, rectangle, enabled).
    fn toolbar_row(app: &mut PhotocraftApp, ctx: &egui::Context, size: egui::Vec2) -> Vec<(String, ERect, bool)> {
        ctx.enable_accesskit();
        let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| show(app, ui.ctx()));
        out.textures_delta.clear();
        let column = app.raw_filmstrip.frame.layout.preview_body;
        let image = app.camera_raw.as_ref().and_then(|d| d.viewport).unwrap();
        // The toolbar row ends 4 pt above the column's bottom edge, where a horizontal strip's splitter begins.
        let row = ERect::from_min_max(pos2(column.left(), image.bottom()), column.right_bottom() - vec2(0.0, 4.0));
        let mut found: Vec<(String, ERect, bool)> = accessible(&out).into_iter().filter(|(_, rect, _)| row.contains(rect.center())).collect();
        found.sort_by(|a, b| a.1.left().total_cmp(&b.1.left()));
        found
    }

    /// Every labelled widget egui reported this frame (any layer, popups included).
    fn accessible(out: &egui::FullOutput) -> Vec<(String, ERect, bool)> {
        let update = out.platform_output.accesskit_update.as_ref().expect("accessibility output is on");
        update
            .nodes
            .iter()
            .filter_map(|(_, node)| {
                let (label, b) = (node.label().or_else(|| node.value())?, node.bounds()?);
                let rect = ERect::from_min_max(pos2(b.x0 as f32, b.y0 as f32), pos2(b.x1 as f32, b.y1 as f32));
                Some((label.to_string(), rect, !node.is_disabled()))
            })
            .collect()
    }

    fn widget(row: &[(String, ERect, bool)], label: &str) -> (ERect, bool) {
        row.iter().find(|(l, ..)| l == label).map(|(_, r, e)| (*r, *e)).unwrap_or_else(|| panic!("no {label:?} in {:?}", names(row)))
    }

    // Resolve the numeric field from its visible label so a new header row does
    // not silently retarget keyboard regression tests to a different setting.
    fn adjustment_field(app: &mut PhotocraftApp, ctx: &egui::Context, label: &str) -> ERect {
        ctx.enable_accesskit();
        let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| show(app, ui.ctx()));
        output.textures_delta.clear();
        let label_rect = widget(&accessible(&output), label).0;
        let fields: Vec<_> = output
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .filter_map(|(_, node)| {
                if node.role() != egui::accesskit::Role::SpinButton || node.is_disabled() {
                    return None;
                }
                let b = node.bounds()?;
                let rect = ERect::from_min_max(pos2(b.x0 as f32, b.y0 as f32), pos2(b.x1 as f32, b.y1 as f32));
                (rect.left() > label_rect.right() && rect.y_range().contains(label_rect.center().y)).then_some(rect)
            })
            .collect();
        assert_eq!(fields.len(), 1, "exactly one numeric field belongs to {label}: {fields:?}");
        fields[0]
    }

    fn names(row: &[(String, ERect, bool)]) -> Vec<&str> {
        row.iter().map(|(l, ..)| l.as_str()).collect()
    }

    /// The preview's actual display scale (1.0 = one image pixel per physical pixel).
    fn shown_scale(app: &PhotocraftApp) -> f32 {
        let d = app.camera_raw.as_ref().unwrap();
        app.ui.camera_raw_preview.scale(d.viewport.unwrap(), vec2(d.full_w as f32, d.full_h as f32) / d.preview_ppp)
    }

    /// The test's own oracle for the readout: a whole percentage without decimals, otherwise one decimal; "Fit (x)" while
    /// fitted. Its accessible name says it is the zoom ("Zoom: …"), so it never shares a name with the 100% button.
    fn expected_readout(app: &PhotocraftApp) -> String {
        let percent = shown_scale(app) * 100.0;
        let text = if (percent - percent.round()).abs() < 0.05 { format!("{percent:.0}%") } else { format!("{percent:.1}%") };
        if app.ui.camera_raw_preview.zoom.is_none() { format!("Zoom: Fit ({text})") } else { format!("Zoom: {text}") }
    }

    /// F3-01: the owner's order in the preview's own toolbar, the Filmstrip control after the zoom group (not in the
    /// footer, not in a sidebar corner), the Zoom and Hand tools after them; nothing overlaps; hiding the strip keeps the
    /// control in the same row, renamed.
    #[test]
    fn f3_01_the_toolbar_follows_the_owner_order_with_the_filmstrip_control_after_zoom() {
        let (mut app, ctx, ids) = review_session();
        for (visible, toggle_name) in [(true, "Collapse"), (false, "Expand")] {
            let row = toolbar_row(&mut app, &ctx, WINDOW);
            let readout = expected_readout(&app);
            assert_eq!(
                names(&row),
                vec![readout.as_str(), "100%", "Zoom", toggle_name, "Grid", "Sort", "Filter", "Zoom Tool", "Hand Tool"],
                "visible={visible}"
            );
            assert!(row.windows(2).all(|w| w[0].1.right() <= w[1].1.left()), "no two controls overlap: {row:?}");
            let toggle = app.raw_filmstrip.frame.toggle.expect("the Filmstrip control is drawn");
            assert_eq!(widget(&row, toggle_name).0, toggle, "the control a click reaches is the one drawn");
            assert!(toggle.bottom() <= WINDOW.y - 48.0, "never in the footer");
            assert_eq!(app.raw_filmstrip.view.visible, visible);
            click(&mut app, &ctx, toggle.center());
        }
        assert!(app.raw_filmstrip.view.visible, "the second click restored the strip");
        assert_eq!((active_document(&app), app.session.documents().len()), (ids[0], 3));
    }

    /// F3-02: the readout always tells the actual display scale; 100% is one image pixel per physical pixel; the presets
    /// menu sets a preset and Fit in View; none of it touches the photo, its settings or the selection.
    #[test]
    fn f3_02_the_readout_tells_the_actual_zoom_and_the_presets_set_it() {
        let (mut app, ctx, ids) = review_session();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.75}}})).unwrap().unwrap();
        let before = (active_document(&app), selected(&app), app.camera_raw.as_ref().unwrap().params.clone());
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        assert!(app.ui.camera_raw_preview.zoom.is_none());
        assert_eq!(row[0].0, expected_readout(&app), "fitted: Fit (actual %)");
        click(&mut app, &ctx, widget(&row, "100%").0.center());
        assert_eq!(shown_scale(&app), 1.0, "100% is 1:1");
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        assert_eq!(row[0].0, "Zoom: 100%");
        click(&mut app, &ctx, widget(&row, "Zoom").0.center());
        let ctx2 = ctx.clone();
        let popup = {
            let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
            let mut out = ctx2.run_ui(input, |ui| show(&mut app, ui.ctx()));
            out.textures_delta.clear();
            accessible(&out)
        };
        let presets: Vec<&str> = popup.iter().map(|(l, ..)| l.as_str()).filter(|l| l.ends_with('%') || *l == "Fit in View").collect();
        for preset in ["Fit in View", "6.25%", "12.5%", "25%", "50%", "100%", "200%", "400%", "800%", "1600%"] {
            assert!(presets.contains(&preset), "{preset} is offered: {presets:?}");
        }
        let two_hundred = popup.iter().find(|(l, r, _)| l == "200%" && !row.iter().any(|(_, rr, _)| rr == r)).unwrap().1;
        click(&mut app, &ctx, two_hundred.center());
        assert_eq!(app.ui.camera_raw_preview.zoom, Some(2.0));
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        assert_eq!(row[0].0, "Zoom: 200%");
        click(&mut app, &ctx, row[0].1.center()); // the readout fits the photo again
        assert!(app.ui.camera_raw_preview.zoom.is_none());
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        assert_eq!(row[0].0, expected_readout(&app));
        assert_eq!((active_document(&app), selected(&app), app.camera_raw.as_ref().unwrap().params.clone()), before);
        assert_eq!(app.session.documents().len(), ids.len());
    }

    /// Grid is a view of the same group and settings; its four-square toggle restores
    /// the original strip orientation, dimensions, ordering and filters on return.
    #[test]
    fn f3_03_grid_toggle_preserves_the_session_and_restores_filmstrip_context() {
        let (mut app, ctx, ids) = review_session();
        click_card_mod(&mut app, &ctx, ids[2], egui::Modifiers::COMMAND);
        let state = |app: &PhotocraftApp| {
            let cards: Vec<_> = app.raw_filmstrip.frame.cards.iter().map(|(d, _)| *d).collect();
            (active_document(app), selected(app), app.camera_raw.as_ref().unwrap().params.clone(), cards, app.session.documents().len())
        };
        let before = state(&app);
        let original_view = app.raw_filmstrip.view;
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        let (rect, enabled) = widget(&row, "Grid");
        assert!(enabled, "Grid is available");
        click(&mut app, &ctx, rect.center());
        frame(&mut app, &ctx, WINDOW, Vec::new());
        assert!(app.raw_filmstrip.view.grid);
        assert!(app.raw_filmstrip.frame.grid.is_some());
        assert!(app.raw_filmstrip.frame.layout.strip.is_none());
        assert_eq!(state(&app), before, "grid preserves membership, selection, settings and visual order");
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        click(&mut app, &ctx, widget(&row, "Grid").0.center());
        frame(&mut app, &ctx, WINDOW, Vec::new());
        assert_eq!(app.raw_filmstrip.view, original_view);
        assert!(app.raw_filmstrip.frame.layout.strip.is_some());
        assert_eq!(state(&app), before);
    }

    /// F3-04 (LF-12): while a menu is open over a focused thumbnail (its own right-click menu: a right click keeps the
    /// focus where it is) the arrows belong to the menu, never to the strip: the shown photo and the group stay as they are.
    #[test]
    fn f3_04_arrows_never_reach_the_filmstrip_while_a_menu_is_open() {
        let (mut app, ctx, ids) = review_session_n(4);
        let group = [ids[0], ids[1], ids[3]];
        select_group(&mut app, &ctx, &group, ids[1]);
        let focused = focus_doc(&app).expect("a thumbnail holds keyboard focus");
        let at = card(&app, focused).center();
        secondary_click(&mut app, &ctx, at);
        assert!(egui::Popup::is_any_open(&ctx), "the thumbnail's menu is open");
        assert_eq!(focus_doc(&app), Some(focused), "the thumbnail still holds keyboard focus");
        let (back, forward) = arrow_keys(&app);
        for arrow in [forward, back, forward] {
            key(&mut app, &ctx, arrow);
            assert_eq!((active_document(&app), selected(&app)), (ids[1], group.to_vec()), "the menu owns the arrows");
        }
    }

    /// F3-05: at the dialog's 900 x 600 minimum, with the widest strip preference, in every complete language and both
    /// orientations, every toolbar control is inside the preview column and clear of the others.
    #[test]
    fn f3_05_the_toolbar_fits_at_the_smallest_window_in_every_language_and_orientation() {
        let small = vec2(900.0, 600.0);
        let previous = crate::i18n::current();
        for lang in crate::i18n::Lang::all().filter(|l| l.complete_menus()).chain([crate::i18n::Lang::EN]) {
            for orientation in ["vertical", "horizontal"] {
                let (mut app, ctx, _) = review_session();
                crate::i18n::set_current(lang);
                menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {"orientation": orientation, "width": 360, "height": 260}}}))
                    .unwrap()
                    .unwrap();
                frame(&mut app, &ctx, small, vec![]);
                let row = toolbar_row(&mut app, &ctx, small);
                let column = app.raw_filmstrip.frame.layout.preview_body;
                assert!(row.len() >= 9, "{} {orientation}: {:?}", lang.code(), names(&row));
                for (label, rect, _) in &row {
                    assert!(
                        rect.left() >= column.left() - 0.5 && rect.right() <= column.right() + 0.5,
                        "{} {orientation}: {label:?} {rect:?} leaves the preview column {column:?}",
                        lang.code()
                    );
                }
                assert!(row.windows(2).all(|w| w[0].1.right() <= w[1].1.left() + 0.5), "{} {orientation}: overlap {row:?}", lang.code());
            }
        }
        crate::i18n::set_current(previous);
    }

    /// C5-R1 (final cross-examination R2): Enter and Space on a focused thumbnail behind its open menu belong to the menu:
    /// the photo that keyboard focus moved to is not shown underneath it.
    #[test]
    fn f3_06_enter_and_space_never_reach_a_thumbnail_behind_an_open_menu() {
        let (mut app, ctx, ids) = review_session();
        click_card(&mut app, &ctx, ids[0]);
        let (_, forward) = arrow_keys(&app);
        key(&mut app, &ctx, forward);
        assert_eq!((focus_doc(&app), active_document(&app)), (Some(ids[1]), ids[0]), "focus moved without showing the photo");
        for pressed in [egui::Key::Space, egui::Key::Enter] {
            let at = card(&app, ids[1]).center();
            secondary_click(&mut app, &ctx, at);
            assert!(egui::Popup::is_any_open(&ctx), "the thumbnail's menu is open");
            assert_eq!(focus_doc(&app), Some(ids[1]));
            key(&mut app, &ctx, pressed);
            assert_eq!((active_document(&app), selected(&app)), (ids[0], vec![ids[0]]), "{pressed:?} belongs to the menu");
        }
    }

    /// C5-R1 (final cross-examination R3): the full-resolution notice never covers a toolbar control. Where the row has
    /// room it is an icon before the tools; at the 900 x 600 minimum it moves into the readout's tooltip.
    #[test]
    fn f3_07_the_detail_notice_never_covers_a_toolbar_control() {
        let notice = "Full-resolution preview unavailable";
        for (window, room) in [(WINDOW, true), (vec2(900.0, 600.0), false)] {
            let (mut app, ctx, _) = review_session();
            // At 100% the small test photo needs no detail refinement, so the notice set here stays.
            app.ui.camera_raw_preview.zoom = Some(1.0);
            app.camera_raw.as_mut().unwrap().detail.error = Some("lab".into());
            frame(&mut app, &ctx, window, vec![]);
            let row = toolbar_row(&mut app, &ctx, window);
            assert!(row.windows(2).all(|w| w[0].1.right() <= w[1].1.left() + 0.5), "{window:?}: overlap {row:?}");
            assert_eq!(names(&row).contains(&notice), room, "{window:?}: {:?}", names(&row));
        }
    }

    // ---- GRW-1 lab cycle 5 (F2): Filmstrip thumbnails made for the card's own size -----------------------------------

    /// Two queued RAWs large enough that a card shows fewer pixels than the photo has (640 x 426), the first under review.
    fn sized_review_session() -> (PhotocraftApp, egui::Context, Vec<photocraft_doc::DocId>) {
        let (mut app, _) = raw_app();
        let (w, h) = (640, 426);
        let mut spec =
            photocraft_raw::testgen::DngSpec::cfa(w, h, photocraft_raw::testgen::mosaic(&photocraft_raw::testgen::scene(w, h), w, [0, 1, 1, 2], 0, 30000));
        spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
        let dng = spec.build();
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        for name in ["wide-one.dng", "wide-two.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let mut ids = vec![app.pending_raw_open.as_ref().unwrap().document];
        ids.extend(app.queued_raw_opens.iter().map(|raw| raw.document));
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        (app, ctx, ids)
    }

    /// The texture a card is drawn with this frame, and its size in pixels.
    fn card_texture(app: &mut PhotocraftApp, ctx: &egui::Context, document: photocraft_doc::DocId) -> Option<[usize; 2]> {
        let texture = filmstrip_entries(app, ctx).into_iter().find(|entry| entry.document == document)?.texture?;
        ctx.tex_manager().read().meta(texture).map(|meta| meta.size)
    }

    /// F2-01: the photo shown and the cards in view get a thumbnail with at least as many pixels as their card shows
    /// (or the photo has), made off the frame; until then the card keeps the 64 px one (never blank).
    #[test]
    fn f2_01_cards_in_view_get_a_thumbnail_as_sharp_as_the_card() {
        let (mut app, ctx, ids) = sized_review_session();
        let strip = app.raw_filmstrip.frame.layout.strip.unwrap();
        // The vertical picture at the default width: the strip less its padding and scroll bar, less the card inset.
        let needed = (strip.width() - 2.0 * 6.0 - 10.0 - 8.0).min(240.0) * ctx.pixels_per_point();
        let mut sizes = Vec::new();
        for _ in 0..400 {
            frame(&mut app, &ctx, WINDOW, vec![]);
            sizes = ids.iter().map(|id| card_texture(&mut app, &ctx, *id)).collect();
            assert!(sizes.iter().all(Option::is_some), "a card is never blank: {sizes:?}");
            if sizes.iter().flatten().all(|s| s[0].max(s[1]) as f32 >= needed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for (id, size) in ids.iter().zip(&sizes) {
            let [w, h] = size.unwrap();
            assert!(w.max(h) as f32 >= needed, "{id:?}: a {w}x{h} thumbnail stretched over a {needed} px card");
        }
        assert_eq!(app.session.documents().len(), ids.len(), "nothing opened, accepted or cancelled");
    }

    /// F2-02 (GREEN-only: the keys are new): a thumbnail is shown only for the photograph, document and content it was made
    /// from. A result arriving after the photo was re-developed (another document), changed (other content) or for another
    /// photograph, or converted for another display, is dropped, never shown; another size shows until its replacement arrives; a failure is not retried.
    #[test]
    fn f2_02_a_stale_thumbnail_is_never_shown() {
        let (mut app, ctx, ids) = sized_review_session();
        let photo = filmstrip_raws(&app)[0].photo;
        let now = ThumbKey { photo, document: ids[0], content: 7, display: 3, side: 192 };
        let image = || Some(egui::ColorImage::new([4, 4], vec![egui::Color32::RED; 16]));
        assert!(now.shows(&ThumbKey { side: 256, ..now }));
        for stale in
            [ThumbKey { document: ids[1], ..now }, ThumbKey { content: 8, ..now }, ThumbKey { display: 4, ..now }, ThumbKey { photo: PhotoId::fresh(), ..now }]
        {
            assert!(!now.shows(&stale));
            store_card_thumb(&mut app, &ctx, stale, image(), &[now]);
            assert!(app.raw_filmstrip.thumbs.is_empty(), "{stale:?} was dropped");
        }
        store_card_thumb(&mut app, &ctx, now, image(), &[now]);
        assert_eq!(app.raw_filmstrip.thumbs.len(), 1);
        store_card_thumb(&mut app, &ctx, ThumbKey { side: 256, ..now }, None, &[now]);
        assert!(app.raw_filmstrip.thumb_failed.contains(&ThumbKey { side: 256, ..now }), "a failure is remembered");
    }

    /// F2-03 (GREEN-only): the review's thumbnails go with it: after Cancel none is kept and none is being made.
    #[test]
    fn f2_03_thumbnails_end_with_the_review() {
        let (mut app, ctx, _) = sized_review_session();
        for _ in 0..400 {
            frame(&mut app, &ctx, WINDOW, vec![]);
            if !app.raw_filmstrip.thumbs.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!app.raw_filmstrip.thumbs.is_empty(), "a thumbnail was made");
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        frame(&mut app, &ctx, WINDOW, vec![]);
        sync_provisional(&mut app);
        assert!(app.raw_filmstrip.thumbs.is_empty() && app.raw_filmstrip.thumb_job.is_none(), "nothing outlives the review");
    }

    /// F2-04 (GREEN-only): a card thumbnail is converted for the display exactly as the preview is, so its tones match the
    /// preview (the 64 px layer thumbnail is not).
    #[test]
    fn f2_04_card_thumbnails_use_the_preview_display_conversion() {
        let (app, _, ids) = sized_review_session();
        let doc = app.session.documents().iter().find(|st| st.doc.id == ids[0]).unwrap().doc.clone();
        let display = app.session.color.canvas_display(&doc).unwrap();
        assert!(!display.is_identity(), "a RAW review is converted for the display");
        let managed = card_thumb_image(&doc, 128, Some(&display));
        let reduced = photocraft_compose::thumbnail_buffer(&doc, 128);
        let expected = display.to_rgba8(&reduced);
        assert_eq!(managed.as_raw(), &expected.pixels[..]);
        assert_ne!(card_thumb_image(&doc, 128, None).as_raw(), managed.as_raw(), "unmanaged would differ");
    }

    // ---- F4 lab, slice L1: filenames fit their own card (layout review R5) ----------------------------------------

    /// Every text painted this frame that starts with `prefix`: (text, its visible rectangle, the galley clipped to its
    /// clip rectangle).
    fn painted_texts(app: &mut PhotocraftApp, ctx: &egui::Context, size: egui::Vec2, prefix: &str) -> Vec<(String, ERect)> {
        let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| show(app, ui.ctx()));
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text().starts_with(prefix) => {
                    Some((text.galley.text().to_string(), text.visual_bounding_rect().intersect(clipped.clip_rect)))
                }
                _ => None,
            })
            .collect()
    }

    /// L1 (F4 lab): a filename is fitted to its card by its drawn width, never by a character count, and clipped to its
    /// own name row, so wide (upper-case, CJK) names stay inside their card in both orientations and never cover a
    /// neighbour. The tooltip keeps the whole name.
    #[test]
    fn l1_long_and_wide_filenames_stay_inside_their_card() {
        let (mut app, dng) = raw_app();
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
        let names = [
            "WWWWWWWWWWWWWWWWWWWWWWWWWWWWWW_UPPERCASE_WIDE_0002.dng",
            "日本語の長いファイル名のテスト写真_0003.dng",
            "a-very-long-example-sequence-photograph-name-0000-00-00-0001.dng",
        ];
        for name in names {
            app.open_bytes(name, &dng).unwrap();
        }
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        for orientation in ["vertical", "horizontal"] {
            set_view(&mut app, &ctx, json!({"orientation": orientation}));
            let ids: Vec<_> = filmstrip_raws(&app).iter().map(|raw| raw.document).collect();
            for (name, id) in names.iter().zip(&ids) {
                let prefix: String = name.chars().take(3).collect();
                let drawn = painted_texts(&mut app, &ctx, WINDOW, &prefix);
                let own = card(&app, *id);
                assert!(!drawn.is_empty(), "{orientation}: {name} is drawn");
                for (text, rect) in &drawn {
                    assert!(own.expand(0.5).contains_rect(*rect), "{orientation}: {text:?} is drawn at {rect:?}, outside its card {own:?}");
                }
            }
        }
    }

    // ---- F4 lab, slice S1: Sort (visual order only) --------------------------------------------------------------

    /// Give the queued or shown photo of `document` a rating, as if its sidecar said so (test setup only).
    fn rate(app: &mut PhotocraftApp, document: photocraft_doc::DocId, rating: PhotoRating) {
        if let Some(raw) = app.camera_raw.as_mut().and_then(|d| d.raw_open.as_mut()).filter(|raw| raw.document == document) {
            raw.rating = rating;
        }
        for raw in app.pending_raw_open.iter_mut().chain(app.queued_raw_opens.iter_mut()) {
            if raw.document == document {
                raw.rating = rating;
            }
        }
    }

    /// Click the menu item named `item` of the toolbar control whose accessible name starts with `control`, opening that
    /// control's menu first unless it is already open (the Filter menu stays open while its boxes are toggled, F1-R1).
    fn choose(app: &mut PhotocraftApp, ctx: &egui::Context, control: &str, item: &str) {
        let visible_items = |app: &mut PhotocraftApp| {
            let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| show(app, ui.ctx()));
            out.textures_delta.clear();
            accessible(&out)
        };
        let row = toolbar_row(app, ctx, WINDOW);
        let mut items = visible_items(app);
        if !egui::Popup::is_any_open(ctx) || !items.iter().any(|(l, ..)| l == item) {
            let at =
                row.iter().find(|(l, ..)| l.starts_with(control)).map(|(_, r, _)| r.center()).unwrap_or_else(|| panic!("no {control:?} in {:?}", names(&row)));
            click(app, ctx, at);
            items = visible_items(app);
        }
        let (_, rect, enabled) =
            items.iter().find(|(l, ..)| l == item).cloned().unwrap_or_else(|| {
                panic!("no menu item {item:?} after clicking {control:?}: {:?}", items.iter().map(|(l, ..)| l.as_str()).collect::<Vec<_>>())
            });
        assert!(enabled, "{item} is enabled");
        click(app, ctx, rect.center());
        frame(app, ctx, WINDOW, vec![]);
    }

    fn card_order(app: &PhotocraftApp) -> Vec<photocraft_doc::DocId> {
        app.raw_filmstrip.frame.cards.iter().map(|(document, _)| *document).collect()
    }

    /// S1 (F4 lab, LF-16): the Sort menu orders the strip by star rating and reverses it. Only the strip's order changes:
    /// the photos, their documents and settings, the shown photo, the group and the review queue (what Open Images commits,
    /// in its own order) are exactly as before. A rating not read yet is always last.
    #[test]
    fn s1_01_the_sort_menu_reorders_only_the_strip() {
        let (mut app, ctx, ids) = review_session_n(5);
        for (id, rating) in ids.iter().zip([PhotoRating::Stars(3), PhotoRating::Unknown, PhotoRating::Rejected, PhotoRating::Stars(5), PhotoRating::Unrated]) {
            rate(&mut app, *id, rating);
        }
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 0.75}}})).unwrap().unwrap();
        select_group(&mut app, &ctx, &[ids[0], ids[3]], ids[0]);
        let state = |app: &PhotocraftApp| {
            let queue: Vec<_> = filmstrip_raws(app).iter().map(|raw| (raw.document, raw.photo)).collect();
            (active_document(app), selected(app), app.camera_raw.as_ref().unwrap().params.clone(), queue, app.session.documents().len())
        };
        let before = state(&app);
        assert_eq!(card_order(&app), ids, "open order by default");
        choose(&mut app, &ctx, "Sort", "Star Rating");
        assert_eq!(card_order(&app), vec![ids[2], ids[4], ids[0], ids[3], ids[1]], "rejected, unrated, 3, 5 stars; unread last");
        assert_eq!(state(&app), before, "only the strip's order changed");
        choose(&mut app, &ctx, "Sort", "Reverse");
        assert_eq!(card_order(&app), vec![ids[3], ids[0], ids[4], ids[2], ids[1]], "reversed; unread still last");
        assert_eq!(state(&app), before);
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        assert!(names(&row).contains(&"Sort: Star Rating, Reverse"), "the control says what it does: {:?}", names(&row));
        choose(&mut app, &ctx, "Sort", "Open Order");
        choose(&mut app, &ctx, "Sort", "Reverse");
        assert_eq!(card_order(&app), ids, "back to the open order");
        assert_eq!(state(&app), before);
    }

    /// S1 (F4 lab): the arrows follow the order the strip shows: with the strip sorted by rating and reversed, stepping
    /// back from the shown photo reaches the selected photo that is drawn before it.
    #[test]
    fn s1_02_arrows_follow_the_sorted_strip() {
        let (mut app, ctx, ids) = review_session_n(4);
        for (id, rating) in ids.iter().zip([PhotoRating::Stars(1), PhotoRating::Stars(4), PhotoRating::Stars(2), PhotoRating::Stars(5)]) {
            rate(&mut app, *id, rating);
        }
        let group = [ids[0], ids[1], ids[3]];
        select_group(&mut app, &ctx, &group, ids[0]);
        let focused = focus_doc(&app);
        set_view(&mut app, &ctx, json!({"sort": {"by": "starRating", "reverse": true}}));
        assert_eq!(card_order(&app), vec![ids[3], ids[1], ids[2], ids[0]]);
        assert_eq!(focus_doc(&app), focused, "keyboard focus stays on its photo when the strip is reordered");
        let (back, forward) = arrow_keys(&app);
        key(&mut app, &ctx, back);
        assert_eq!((active_document(&app), selected(&app)), (ids[1], group.to_vec()), "back skips the unselected 2-star photo");
        key(&mut app, &ctx, back);
        assert_eq!(active_document(&app), ids[3]);
        key(&mut app, &ctx, forward);
        key(&mut app, &ctx, forward);
        assert_eq!(active_document(&app), ids[0], "forward walks the same drawn order");
    }

    // ---- F4 lab, slice F1: Filter by rating (visual only) --------------------------------------------------------

    /// F1 (F4 lab, LF-17): the Filter menu hides rated photos from the strip only. A hidden photo stays in the review (the
    /// queue Open Images commits is unchanged) and keeps its selection; the count row and the control say how many are
    /// hidden and selected; the photo shown is never hidden; Show All brings everything back.
    #[test]
    fn flt_01_the_filter_hides_by_rating_and_never_drops_a_photo() {
        let (mut app, ctx, ids) = review_session_n(5);
        for (id, rating) in ids.iter().zip([PhotoRating::Stars(3), PhotoRating::Unknown, PhotoRating::Rejected, PhotoRating::Stars(5), PhotoRating::Unrated]) {
            rate(&mut app, *id, rating);
        }
        select_group(&mut app, &ctx, &[ids[0], ids[3]], ids[0]);
        let state = |app: &PhotocraftApp| {
            let queue: Vec<_> = filmstrip_raws(app).iter().map(|raw| (raw.document, raw.photo)).collect();
            (active_document(app), selected(app), app.camera_raw.as_ref().unwrap().params.clone(), queue, app.session.documents().len())
        };
        let before = state(&app);
        choose(&mut app, &ctx, "Filter", "Rated");
        assert_eq!(card_order(&app), vec![ids[0], ids[1], ids[2], ids[4]], "the 5-star photo is hidden; the 3-star one is shown, so it stays");
        assert_eq!(state(&app), before, "nothing accepted, cancelled, closed or deselected");
        let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| show(&mut app, ui.ctx()));
        out.textures_delta.clear();
        let labels: Vec<String> = accessible(&out).into_iter().map(|(l, ..)| l).collect();
        assert!(labels.iter().any(|l| l == "1 hidden, 1 selected"), "the count row: {labels:?}");
        assert!(labels.iter().any(|l| l == "Filter: 1 hidden, 1 selected"), "the control says it filters");
        assert!(labels.iter().any(|l| l.ends_with("Shown; hidden by filter")), "the shown photo says why it is still there");
        choose(&mut app, &ctx, "Filter", "Show All");
        assert_eq!(card_order(&app), ids);
        assert_eq!(state(&app), before);
    }

    /// F1 (F4 lab): the arrows skip hidden photos, and a hidden photo's selection does not change while it is hidden:
    /// Select All adds every visible photo and keeps the hidden selected one.
    #[test]
    fn flt_02_arrows_skip_hidden_photos_and_hidden_photos_keep_their_selection() {
        let (mut app, ctx, ids) = review_session_n(4);
        for (id, rating) in ids.iter().zip([PhotoRating::Unrated, PhotoRating::Stars(2), PhotoRating::Unrated, PhotoRating::Unrated]) {
            rate(&mut app, *id, rating);
        }
        select_group(&mut app, &ctx, &[ids[0], ids[1], ids[2]], ids[0]);
        set_view(&mut app, &ctx, json!({"filter": {"rated": false}}));
        assert_eq!(card_order(&app), vec![ids[0], ids[2], ids[3]]);
        let (_, forward) = arrow_keys(&app);
        key(&mut app, &ctx, forward);
        assert_eq!(active_document(&app), ids[2], "forward skips the hidden photo");
        key_mod(&mut app, &ctx, egui::Key::A, egui::Modifiers::COMMAND);
        assert_eq!(selected(&app), ids.to_vec(), "every visible photo, and the hidden one stays selected");
    }

    /// F1 (F4 lab): the filter belongs to one review: the next review never starts with photos hidden. The sort stays.
    #[test]
    fn flt_03_the_filter_resets_for_the_next_review_and_the_sort_stays() {
        let (mut app, ctx, _) = review_session_n(3);
        set_view(&mut app, &ctx, json!({"filter": {"unread": false}, "sort": {"by": "fileName"}}));
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}})).unwrap().unwrap();
        frame(&mut app, &ctx, WINDOW, vec![]);
        sync_provisional(&mut app);
        let (_, dng) = raw_app();
        for name in ["next-one.dng", "next-two.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {}}})).unwrap().unwrap();
        assert_eq!(r["filmstripView"]["filter"], json!({"rated": true, "unrated": true, "rejected": true, "unread": true}), "the filter is reset");
        assert_eq!(r["filmstripView"]["sort"]["by"], json!("fileName"), "the sort is a lasting preference");
    }

    // ---- F4 lab, F1-R1: repairs after the independent review (B1, B3, R1, R3) ----------------------------------------

    fn frame_labels(app: &mut PhotocraftApp, ctx: &egui::Context) -> Vec<String> {
        ctx.enable_accesskit();
        let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| show(app, ui.ctx()));
        out.textures_delta.clear();
        accessible(&out).into_iter().map(|(l, ..)| l).collect()
    }

    /// F1-R1 (review B1): filtering down to the photo shown keeps the strip, its toolbar group (so Show All stays reachable)
    /// and the "Open Images" label: the review still holds, and Open still commits, every photo.
    #[test]
    fn flt_04_filtering_down_to_the_shown_photo_keeps_the_strip_and_open_images() {
        let (mut app, ctx, ids) = review_session_n(3);
        for id in &ids {
            rate(&mut app, *id, PhotoRating::Unrated);
        }
        set_view(&mut app, &ctx, json!({"filter": {"unrated": false}}));
        assert_eq!(card_order(&app), vec![ids[0]], "only the shown photo is left");
        assert!(app.raw_filmstrip.frame.layout.strip.is_some(), "the strip stays");
        let labels = frame_labels(&mut app, &ctx);
        assert!(labels.iter().any(|l| l.starts_with("Filter")), "the Filter control stays: {labels:?}");
        assert!(labels.iter().any(|l| l == "Open Images"), "the review still opens every photo: {labels:?}");
        assert!(labels.iter().any(|l| l == "2 hidden"));
        assert_eq!(filmstrip_raws(&app).len(), 3);
    }

    /// F1-R1 (review B3): Esc with the Sort or Filter menu open closes the menu, never the review; with no menu open Esc
    /// still cancels the review as before.
    #[test]
    fn flt_05_escape_closes_an_open_menu_not_the_review() {
        let (mut app, ctx, ids) = review_session_n(3);
        for control in ["Sort", "Filter"] {
            let row = toolbar_row(&mut app, &ctx, WINDOW);
            let at = row.iter().find(|(l, ..)| l.starts_with(control)).map(|(_, r, _)| r.center()).unwrap();
            click(&mut app, &ctx, at);
            assert!(egui::Popup::is_any_open(&ctx), "the {control} menu is open");
            key(&mut app, &ctx, egui::Key::Escape);
            assert!(app.camera_raw.is_some(), "{control}: Esc closed the menu, not the review");
            assert_eq!(filmstrip_raws(&app).len(), ids.len());
            frame(&mut app, &ctx, WINDOW, vec![]);
            assert!(!egui::Popup::is_any_open(&ctx), "{control}: the menu is closed");
        }
        key(&mut app, &ctx, egui::Key::Escape);
        assert!(app.camera_raw.is_none(), "with no menu open Esc still cancels the review");
    }

    /// F1-R1 (review R1): with one visible and one hidden photo selected the arrows still move along the strip; stepping
    /// through a group needs two *visible* selected photos.
    #[test]
    fn flt_06_arrows_still_move_with_one_visible_and_one_hidden_photo_selected() {
        let (mut app, ctx, ids) = review_session_n(4);
        for (id, rating) in ids.iter().zip([PhotoRating::Unrated, PhotoRating::Stars(2), PhotoRating::Unrated, PhotoRating::Unrated]) {
            rate(&mut app, *id, rating);
        }
        select_group(&mut app, &ctx, &[ids[0], ids[1]], ids[0]);
        set_view(&mut app, &ctx, json!({"filter": {"rated": false}}));
        // Focus a visible card without changing the selection: Ctrl-click it on and off.
        click_card_mod(&mut app, &ctx, ids[3], egui::Modifiers::COMMAND);
        click_card_mod(&mut app, &ctx, ids[3], egui::Modifiers::COMMAND);
        assert_eq!((selected(&app), focus_doc(&app)), (vec![ids[0], ids[1]], Some(ids[3])));
        let (back, _) = arrow_keys(&app);
        key(&mut app, &ctx, back);
        assert_eq!(focus_doc(&app), Some(ids[2]), "the arrow moves focus to the previous visible photo");
        assert_eq!(active_document(&app), ids[0], "focus alone never changes the shown photo");
    }

    /// F1-R1 (review R3): the Filter menu stays open while its boxes are toggled, so several can be set in one visit.
    #[test]
    fn flt_07_the_filter_menu_stays_open_while_its_boxes_are_toggled() {
        let (mut app, ctx, _) = review_session_n(3);
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        let at = row.iter().find(|(l, ..)| l.starts_with("Filter")).map(|(_, r, _)| r.center()).unwrap();
        click(&mut app, &ctx, at);
        for item in ["Rated", "Unrated"] {
            let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| show(&mut app, ui.ctx()));
            out.textures_delta.clear();
            let rect = accessible(&out).into_iter().find(|(l, ..)| l == item).map(|(_, r, _)| r).unwrap_or_else(|| panic!("no {item} item"));
            click(&mut app, &ctx, rect.center());
            assert!(egui::Popup::is_any_open(&ctx), "{item}: the menu is still open");
        }
        let r = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"filmstrip": {}}})).unwrap().unwrap();
        assert_eq!((r["filmstripView"]["filter"]["rated"].as_bool(), r["filmstripView"]["filter"]["unrated"].as_bool()), (Some(false), Some(false)));
    }
}

#[cfg(test)]
mod preview_blend_tests {
    use super::blend_preview_selection;

    #[test]
    fn parallel_preview_selection_preserves_scalar_bits_and_short_mask_tail() {
        let original: Vec<_> = (0..131_073).map(|i| [i as f32 / 103.0, -0.25, 0.1, 0.75]).collect();
        let processed: Vec<_> = original.iter().map(|p| [p[0] * 1.7, 0.9999, 0.37, 0.25]).collect();
        let mask: Vec<_> = (0..original.len()).map(|i| [0.0, 0.125, 0.5, 0.9999, 1.0][i % 5]).collect();
        for len in [0, 17, 65_535, 65_536, 65_537, mask.len()] {
            let mut expected = processed.clone();
            for ((out, original), coverage) in expected.iter_mut().zip(&original).zip(&mask[..len]) {
                for (out, original) in out.iter_mut().zip(original) {
                    *out = *original + (*out - *original) * coverage;
                }
            }
            let mut actual = processed.clone();
            blend_preview_selection(&mut actual, &original, &mask[..len]);
            assert!(actual.iter().flatten().zip(expected.iter().flatten()).all(|(a, b)| a.to_bits() == b.to_bits()));
        }
    }

    #[test]
    fn preview_selection_handles_exceptional_floats_and_each_short_input() {
        let values = [0.0, -0.0, f32::MIN_POSITIVE, f32::from_bits(1), f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.25, 0.75];
        let original: Vec<_> = (0..65_539).map(|i| [values[i % values.len()]; 4]).collect();
        let processed: Vec<_> = (0..original.len()).map(|i| [values[(i + 2) % values.len()]; 4]).collect();
        let mask: Vec<_> = (0..original.len()).map(|i| values[(i + 4) % values.len()]).collect();
        for (out_len, original_len, mask_len) in
            [(65_536, 65_539, 65_539), (65_539, 65_536, 65_539), (65_539, 65_539, 65_536), (17, 65_539, 65_539), (65_539, 17, 65_539), (65_539, 65_539, 17)]
        {
            let mut expected = processed[..out_len].to_vec();
            for ((out, original), coverage) in expected.iter_mut().zip(&original[..original_len]).zip(&mask[..mask_len]) {
                for (out, original) in out.iter_mut().zip(original) {
                    *out = *original + (*out - *original) * coverage;
                }
            }
            let mut actual = processed[..out_len].to_vec();
            blend_preview_selection(&mut actual, &original[..original_len], &mask[..mask_len]);
            assert!(actual.iter().flatten().zip(expected.iter().flatten()).all(|(a, b)| (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()));
        }
    }
}
