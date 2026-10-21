//! Headless production-bridge acceptance. Context::run_ui renders an embedded
//! egui tree only; these tests never start eframe, native windows or pickers.
// Test-only fixture helpers should fail the test immediately on setup errors.
#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[test]
fn lifecycle_keep_from_done_plan_acknowledges_all_edits_without_sidecars() {
    let fixture = Fixture::new();
    let source = fixture.source("keep-from-done.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 1.25);
    control(&mut app, "done");
    let plan = wait_for(&mut app, &ctx, "reviewed Done plan", |_, state| !state["plan"].is_null() && state["busy"] == false);
    let session = plan["session"].as_str().unwrap().to_owned();
    control(&mut app, "keep");
    closed(&mut app, &ctx);
    assert!(app.ui.status.contains("Kept"));
    assert!(!source.with_extension("xmp").exists());
    assert_eq!(photocraft_io::raw_session::store::discover(&fixture.cache()).unwrap().len(), 1, "close acknowledgement releases retained ownership");
    let (recovered, _) = photocraft_io::raw_session::controller::Controller::resume(&fixture.cache(), &session).unwrap();
    assert_eq!(recovered.session().photos[0].working.adjustments.exposure, 1.25);
    assert_eq!(std::fs::read(source).unwrap(), fixture.bytes);
}

#[test]
fn lifecycle_recovery_off_cannot_claim_keep_or_create_checkpoint() {
    let fixture = Fixture::new();
    let source = fixture.source("keep-off.dng");
    let mut app = fixture.app();
    app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"off"})).unwrap();
    let ctx = context();
    open(&mut app, &source);
    wait_for(&mut app, &ctx, "memory admission", |app, state| app.camera_raw.is_some() && state["busy"] == false);
    exposure(&mut app, &ctx, 0.75);
    assert!(persistence_ui::control(&mut app, &json!({"action":"keep"})).is_err());
    assert!(app.camera_raw.is_some());
    assert!(session_dirs(&fixture).is_empty());
    assert!(!source.with_extension("xmp").exists());
}

pub(super) struct Fixture {
    root: PathBuf,
    bytes: Vec<u8>,
}

impl Fixture {
    pub(super) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!("pc-persistence-ui-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(root.join("inputs")).unwrap();
        use photocraft_raw::testgen::{DngSpec, mosaic, scene};
        let (width, height) = (32, 24);
        let mut spec = DngSpec::cfa(width, height, mosaic(&scene(width, height), width, [0, 1, 1, 2], 0, 30000));
        spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
        Self { root, bytes: spec.build() }
    }

    pub(super) fn cache(&self) -> PathBuf {
        self.root.join("recovery")
    }

    pub(super) fn source(&self, name: &str) -> PathBuf {
        let path = self.root.join("inputs").join(name);
        std::fs::write(&path, &self.bytes).unwrap();
        path
    }

    pub(super) fn app(&self) -> PhotocraftApp {
        self.app_at_cache(self.cache())
    }

    fn app_at_cache(&self, cache: PathBuf) -> PhotocraftApp {
        let services = crate::Services {
            camera_raw_recovery_root: Some(cache),
            import: Some(Box::new(|name, bytes, depth| {
                photocraft_io::import_with_svg_group_depth(name, bytes, depth)
                    .map(|result| (result.document, result.warnings))
                    .map_err(|error| error.to_string())
            })),
            ..Default::default()
        };
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
        app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"cache"})).unwrap();
        app.background_jobs = false;
        app
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Retained failed bridge fixture and subprocess logs: {}", self.root.display());
            return;
        }
        // A host's detached worker releases its owned handles after its channels
        // disconnect. Cleanup is bounded and affects only this test's directory.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.root).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub(super) fn context() -> egui::Context {
    let context = egui::Context::default();
    PhotocraftApp::setup_context(&context, crate::theme::ThemeKind::Pro);
    context.set_embed_viewports(true);
    context
}

fn frame(app: &mut PhotocraftApp, context: &egui::Context) {
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, vec2(1360.0, 900.0))), ..Default::default() };
    let mut result = context.run_ui(input, |ui| show(app, ui.ctx()));
    result.textures_delta.clear();
}

fn wait_for(app: &mut PhotocraftApp, context: &egui::Context, description: &str, ready: impl Fn(&PhotocraftApp, &Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        frame(app, context);
        let state = persistence_ui::describe(app);
        if ready(app, &state) {
            return state;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for {description}: {state}; status={}", app.ui.status);
        assert!(state["busy"] == true || state.get("error").is_none_or(Value::is_null), "{description}: {state}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn settled(app: &mut PhotocraftApp, context: &egui::Context, count: usize) -> Value {
    wait_for(app, context, "admitted, automatically acknowledged recipes", |_, state| {
        state["photos"].as_array().is_some_and(|photos| photos.len() == count)
            && state["busy"] == false
            && !state["acknowledged"].is_null()
            && state["acknowledged"] == state["revision"]
    })
}

pub(super) fn open(app: &mut PhotocraftApp, path: &Path) {
    let bytes = std::fs::read(path).unwrap();
    app.open_bytes_from(path.file_name().unwrap().to_str().unwrap(), &bytes, Some(path.to_str().unwrap())).unwrap();
}

fn runtime(state: &Value, index: usize) -> PhotoId {
    PhotoId(state["photos"][index]["photo"].as_u64().unwrap())
}

fn control(app: &mut PhotocraftApp, action: &str) -> Value {
    persistence_ui::control(app, &json!({"action":action})).unwrap()
}

fn exposure(app: &mut PhotocraftApp, context: &egui::Context, value: f32) {
    menu(app, context, "filter.cameraRaw", &json!({"ui":{"set":{"exposure":value}}})).unwrap().unwrap();
}

fn closed(app: &mut PhotocraftApp, context: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        frame(app, context);
        let state = persistence_ui::describe(app);
        if state["available"] == false && app.camera_raw.is_none() {
            return;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for acknowledged lifecycle closure: {state}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The process stops after an acknowledged checkpoint without Done, Open Images or Cancel:
/// the host goes away and its worker releases the session lock. Nothing terminal is
/// recorded, so the session is offered for recovery after an interruption.
pub(super) fn interrupt(app: PhotocraftApp, fixture: &Fixture, session: &str) {
    drop(app);
    let lock = fixture.cache().join(session).join("ownership.lock");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&lock).unwrap();
        if file.try_lock().is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "the interrupted worker kept its session lock");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Session directories left under the private root (its shared commit lock is not one).
pub(super) fn session_dirs(fixture: &Fixture) -> Vec<String> {
    std::fs::read_dir(fixture.cache())
        .map(|entries| entries.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

fn resume_saved(root: &Path, id: &str) -> photocraft_io::raw_session::controller::Controller {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match photocraft_io::raw_session::controller::Controller::resume(root, id) {
            Ok((controller, _)) => return controller,
            Err(photocraft_io::raw_session::store::StoreError::Owned) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => panic!("Cannot resume acknowledged test draft: {error}"),
        }
    }
}

fn save_edits(app: &mut PhotocraftApp, context: &egui::Context) -> Value {
    control(app, "saveEdits");
    let plan = wait_for(app, context, "Save edits plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    assert_eq!(plan["plan"]["scope"], "SaveEdits");
    persistence_ui::control(app, &json!({"action":"save", "planId":plan["plan"]["id"]})).unwrap();
    let saved = wait_for(app, context, "Save edits acknowledged without closing", |_, state| {
        state["busy"] == false && state["frozen"] == false && state["plan"].is_null()
    });
    assert!(saved["finishing"].is_null(), "Save edits is over: {saved}");
    saved
}

#[test]
fn bridge_save_edits_keeps_review_and_cancel_preserves_the_saved_recipe() {
    let fixture = Fixture::new();
    let source = fixture.source("save-continue-cancel.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    let initial = settled(&mut app, &ctx, 1);
    let runtime = runtime(&initial, 0);
    exposure(&mut app, &ctx, 0.5);
    settled(&mut app, &ctx, 1);
    let saved = save_edits(&mut app, &ctx);
    assert_eq!(saved["photos"][0]["pendingXmp"], false);
    assert_eq!(saved["session"], initial["session"]);
    assert_eq!(filmstrip_raws(&app)[0].photo, runtime);
    assert!(app.camera_raw.is_some());
    assert_eq!(app.session.provisional_documents().len(), 1);
    let packet = std::fs::read(source.with_extension("xmp")).unwrap();
    let recipe = photocraft_io::raw_session::xmp::load_native(&packet).unwrap().unwrap();
    assert_eq!(recipe.adjustments.exposure, 0.5);
    exposure(&mut app, &ctx, 0.75);
    assert_eq!(settled(&mut app, &ctx, 1)["photos"][0]["pendingXmp"], true);
    persistence_ui::control(&mut app, &json!({"action":"discard","confirm":true})).unwrap();
    closed(&mut app, &ctx);
    assert_eq!(std::fs::read(source.with_extension("xmp")).unwrap(), packet, "Cancel cannot roll back or rewrite an explicit Save");
    assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
    drop(app);
    let mut fresh = fixture.app_at_cache(fixture.root.join("fresh-cache"));
    open(&mut fresh, &source);
    assert_eq!(settled(&mut fresh, &ctx, 1)["photos"][0]["recipe"]["adjustments"]["exposure"], 0.5);
}

#[test]
fn bridge_save_four_independent_recipes_then_done_and_reopen_without_cache() {
    let fixture = Fixture::new();
    let sources: Vec<_> = (1..=4).map(|n| fixture.source(&format!("P{n}.dng"))).collect();
    let mut app = fixture.app();
    let ctx = context();
    for path in &sources {
        open(&mut app, path);
    }
    settled(&mut app, &ctx, 4);
    let raws = filmstrip_raws(&app);
    for (raw, (exp, contrast)) in raws.iter().zip([(0.5, 7.0), (-0.75, -11.0), (1.25, 19.0), (-1.0, 0.0)]) {
        select_raw_review(&mut app, &ctx, raw.document).unwrap();
        app.raw_filmstrip.selection.select_only(raw.photo);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"set":{"exposure":exp,"contrast":contrast}}})).unwrap().unwrap();
    }
    settled(&mut app, &ctx, 4);
    let state = save_edits(&mut app, &ctx);
    assert!(state["photos"].as_array().unwrap().iter().all(|p| p["pendingXmp"] == false));
    assert_eq!(app.session.provisional_documents().len(), 4);
    select_raw_review(&mut app, &ctx, raws[0].document).unwrap();
    app.raw_filmstrip.selection.select_only(raws[0].photo);
    exposure(&mut app, &ctx, 0.75);
    settled(&mut app, &ctx, 4);
    control(&mut app, "done");
    let plan = wait_for(&mut app, &ctx, "Done only saves later edit", |_, state| state["busy"] == false && !state["plan"].is_null());
    assert_eq!(plan["plan"]["photos"].as_array().unwrap().len(), 1);
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    closed(&mut app, &ctx);
    assert!(app.session.documents().is_empty());
    drop(app);
    let mut fresh = fixture.app_at_cache(fixture.root.join("fresh-four-cache"));
    for path in &sources {
        open(&mut fresh, path);
    }
    settled(&mut fresh, &ctx, 4);
    for (raw, (exp, contrast)) in filmstrip_raws(&fresh).iter().zip([(0.75, 7.0), (-0.75, -11.0), (1.25, 19.0), (-1.0, 0.0)]) {
        let recipe = persistence_ui::recipe(&fresh, raw.photo).unwrap();
        assert_eq!(recipe.adjustments.exposure, exp);
        assert_eq!(recipe.adjustments.contrast, contrast);
    }
}

#[test]
fn bridge_sidecar_publisher_denial_survives_worker_and_native_confirmation() {
    for action in ["saveEdits", "done", "remove"] {
        let fixture = Fixture::new();
        let source = fixture.source("read-authority-only.dng");
        let mut app = fixture.app();
        let attempts = std::sync::Arc::new(AtomicU64::new(0));
        let observed = attempts.clone();
        app.services.camera_raw_sidecar_publisher = Some(std::sync::Arc::new(move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "write authority is absent"))
        }));
        let ctx = context();
        open(&mut app, &source);
        let ready = settled(&mut app, &ctx, 1);
        exposure(&mut app, &ctx, 1.25);
        settled(&mut app, &ctx, 1);
        persistence_ui::control(&mut app, &json!({"action":action,"photo":runtime(&ready,0).0})).unwrap();
        let plan = wait_for(&mut app, &ctx, "denial test plan", |_, state| state["busy"] == false && !state["plan"].is_null());
        persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
        let failed = wait_for(&mut app, &ctx, "publisher denial", |_, state| state["busy"] == false && state["error"].is_string());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(failed["photos"][0]["pendingXmp"], true);
        assert!(app.camera_raw.is_some());
        assert!(!source.with_extension("xmp").exists());
        assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
    }
}

#[test]
fn bridge_verified_absent_or_zero_rating_is_unrated_and_matches_zero_filter() {
    for has_zero in [false, true] {
        let fixture = Fixture::new();
        let source = fixture.source("unrated.dng");
        if has_zero {
            std::fs::write(source.with_extension("xmp"), b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='0'/></rdf:RDF></x:xmpmeta>").unwrap();
        }
        let mut app = fixture.app();
        let ctx = context();
        open(&mut app, &source);
        settled(&mut app, &ctx, 1);
        app.raw_filmstrip.view.filter = filmstrip_ui::RatingFilter {
            rated: false,
            unrated: true,
            rejected: false,
            unread: false,
            stars: filmstrip_ui::StarComparison::Exactly(0),
            ..Default::default()
        };
        let entries = filmstrip_entries(&mut app, &ctx);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rating, PhotoRating::Unrated);
        assert!(!entries[0].filtered_out);
    }
}

#[test]
fn bridge_initial_handoff_failure_can_retry_after_same_source_returns() {
    let fixture = Fixture::new();
    let source = fixture.source("handoff.dng");
    let missing = source.with_extension("held");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    commit(&mut app).unwrap();
    std::fs::rename(&source, &missing).unwrap();
    wait_for(&mut app, &ctx, "synchronous Open failure releases handoff intent", |app, state| {
        app.ui.status_error && state["handoff"] == false && state["busy"] == false
    });
    assert!(app.camera_raw.is_some());
    assert_eq!(app.session.provisional_documents().len(), 1);
    std::fs::rename(&missing, &source).unwrap();
    commit(&mut app).unwrap();
    wait_for(&mut app, &ctx, "successful Open retry", |app, _| app.camera_raw.is_none() && app.session.provisional_documents().is_empty());
    assert_eq!(app.session.documents().len(), 1);
    assert!(!source.with_extension("xmp").exists());
}

#[test]
fn bridge_partial_handoff_retry_preserves_already_transferred_document() {
    let fixture = Fixture::new();
    let first = fixture.source("first-transfer.dng");
    let second = fixture.source("second-transfer.dng");
    let moved = second.with_extension("held");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &first);
    open(&mut app, &second);
    settled(&mut app, &ctx, 2);
    let photo_ids: Vec<_> = filmstrip_raws(&app).into_iter().map(|raw| raw.photo).collect();
    app.raw_filmstrip.selection.select_all(&photo_ids);
    exposure(&mut app, &ctx, 0.5);
    group_control(&mut app, &json!({"action":"undo"})).unwrap();
    settled(&mut app, &ctx, 2);
    assert_eq!(app.raw_filmstrip.group_redo.len(), 1);
    commit(&mut app).unwrap();
    std::fs::rename(&second, &moved).unwrap();
    wait_for(&mut app, &ctx, "second handoff fails after first succeeds", |app, state| {
        app.ui.status_error && state["busy"] == false && state["handoff"] == false && app.session.provisional_documents().len() == 1
    });
    assert_eq!(app.session.documents().len(), 2);
    let transferred = app.session.documents().iter().find(|doc| !app.session.provisional_documents().contains(&doc.doc.id)).unwrap().doc.clone();
    // The native host keeps transferred recipes for save/handoff bookkeeping;
    // history may only edit photographs still held by this Camera Raw review.
    let before: Vec<_> = photo_ids.iter().map(|id| persistence_ui::recipe(&app, *id).cloned()).collect();
    let histories = (app.raw_filmstrip.group_undo.clone(), app.raw_filmstrip.group_redo.clone());
    assert!(group_control(&mut app, &json!({"action":"redo"})).unwrap_err().contains("left the Camera Raw review"));
    assert_eq!(photo_ids.iter().map(|id| persistence_ui::recipe(&app, *id).cloned()).collect::<Vec<_>>(), before);
    assert_eq!((app.raw_filmstrip.group_undo.clone(), app.raw_filmstrip.group_redo.clone()), histories);
    std::fs::rename(&moved, &second).unwrap();
    commit(&mut app).unwrap();
    closed(&mut app, &ctx);
    assert_eq!(app.session.documents().len(), 2);
    assert!(app.session.provisional_documents().is_empty());
    assert!(
        app.session.documents().iter().any(|doc| std::sync::Arc::ptr_eq(&doc.doc, &transferred)),
        "retry must not redevelop or duplicate the transferred document"
    );
    assert!(!first.with_extension("xmp").exists() && !second.with_extension("xmp").exists());
}

fn rendered_source(fixture: &Fixture, format: photocraft_codecs::Format, embedded: bool) -> PathBuf {
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Image, encode};
    let mut image = Image::from_u8(16, 12, ChannelLayout::Rgb, [80, 120, 160].repeat(16 * 12)).unwrap();
    if embedded {
        image.meta.xmp = Some("<r:RDF xmlns:r='http://www.w3.org/1999/02/22-rdf-syntax-ns#' xmlns:c='http://ns.adobe.com/camera-raw-settings/1.0/'><r:Description c:HasSettings='True' c:Exposure2012='0.75'/></r:RDF>".into());
    }
    let extension = if format == photocraft_codecs::Format::Jpeg { "jpg" } else { "tif" };
    let path = fixture.root.join("inputs").join(format!("rendered-{extension}.{extension}"));
    std::fs::write(&path, encode(&image, format, &EncodeOptions::default()).unwrap()).unwrap();
    path
}

#[test]
fn bridge_rendered_opening_defaults_embedded_and_explicit_routes() {
    for format in [photocraft_codecs::Format::Jpeg, photocraft_codecs::Format::Tiff] {
        let fixture = Fixture::new();
        let source = rendered_source(&fixture, format, false);
        let mut ordinary = fixture.app();
        open(&mut ordinary, &source);
        assert_eq!(ordinary.session.documents().len(), 1);
        assert!(ordinary.session.provisional_documents().is_empty());
        assert!(ordinary.pending_raw_open.is_none());
        drop(ordinary);
        let mut explicit = fixture.app_at_cache(fixture.root.join("explicit-cache"));
        explicit.run("prefs.set", json!({"path":"rawDefaults.jpegHandling","value":"disabled"})).unwrap();
        explicit.run("prefs.set", json!({"path":"rawDefaults.tiffHandling","value":"disabled"})).unwrap();
        explicit.open_path_camera_raw(source.to_str().unwrap()).unwrap();
        let ctx = context();
        settled(&mut explicit, &ctx, 1);
        assert_eq!(explicit.session.provisional_documents().len(), 1);
        drop(explicit);
        let embedded = rendered_source(&fixture, format, true);
        let mut automatic = fixture.app_at_cache(fixture.root.join("embedded-cache"));
        open(&mut automatic, &embedded);
        let state = settled(&mut automatic, &ctx, 1);
        assert_eq!(state["photos"][0]["recipe"]["adjustments"]["exposure"], 0.75);
        assert!(!embedded.with_extension("xmp").exists());
    }
}

#[test]
fn bridge_mixed_formats_save_and_reopen_sidecars_using_default_routing() {
    let fixture = Fixture::new();
    let raw = fixture.source("sensor.dng");
    let jpeg = rendered_source(&fixture, photocraft_codecs::Format::Jpeg, false);
    let tiff = rendered_source(&fixture, photocraft_codecs::Format::Tiff, false);
    let originals: Vec<_> = [&raw, &jpeg, &tiff].into_iter().map(|p| std::fs::read(p).unwrap()).collect();
    let mut app = fixture.app();
    app.run("prefs.set", json!({"path":"rawDefaults.jpegHandling","value":"all"})).unwrap();
    app.run("prefs.set", json!({"path":"rawDefaults.tiffHandling","value":"all"})).unwrap();
    for path in [&raw, &jpeg, &tiff] {
        open(&mut app, path);
    }
    let ctx = context();
    let admitted = settled(&mut app, &ctx, 3);
    assert_eq!(admitted["nativeRequests"].as_object().unwrap().len(), 3);
    for (photo, expected) in filmstrip_raws(&app).into_iter().zip([0.5, -0.75, 1.25]) {
        select_raw_review(&mut app, &ctx, photo.document).unwrap();
        app.raw_filmstrip.selection.select_only(photo.photo);
        exposure(&mut app, &ctx, expected);
    }
    settled(&mut app, &ctx, 3);
    save_edits(&mut app, &ctx);
    control(&mut app, "discard");
    closed(&mut app, &ctx);
    drop(app);
    let mut reopened = fixture.app_at_cache(fixture.root.join("reopen-mixed-cache"));
    for (path, original) in [&raw, &jpeg, &tiff].into_iter().zip(originals) {
        assert_eq!(std::fs::read(path).unwrap(), original);
        open(&mut reopened, path);
    }
    settled(&mut reopened, &ctx, 3);
    for (photo, expected) in filmstrip_raws(&reopened).into_iter().zip([0.5, -0.75, 1.25]) {
        assert_eq!(persistence_ui::recipe(&reopened, photo.photo).unwrap().adjustments.exposure, expected);
    }
}

#[test]
fn bridge_layered_tiff_stays_in_editor_with_layers_for_automatic_and_explicit_open() {
    let fixture = Fixture::new();
    let mut session = photocraft_engine::Session::new();
    session.execute("file.new", json!({"width":16,"height":12})).unwrap();
    session.execute("layer.new.layer", json!({"name":"Protected upper layer"})).unwrap();
    let doc = &session.active().unwrap().doc;
    let expected: Vec<_> = doc.layers.iter().map(|l| l.name.clone()).collect();
    let exported = photocraft_io::export(doc, "layers.tif", &photocraft_io::ExportOptions { tiff_layers: true, ..Default::default() }).unwrap();
    let source = fixture.root.join("inputs/layers.tif");
    std::fs::write(&source, &exported.bytes).unwrap();
    for explicit in [false, true] {
        let mut app = fixture.app();
        app.run("prefs.set", json!({"path":"rawDefaults.tiffHandling","value":"all"})).unwrap();
        if explicit {
            app.open_path_camera_raw(source.to_str().unwrap()).unwrap();
        } else {
            open(&mut app, &source);
        }
        assert!(app.pending_raw_open.is_none());
        assert!(app.session.provisional_documents().is_empty());
        assert_eq!(app.session.active().unwrap().doc.layers.iter().map(|l| l.name.clone()).collect::<Vec<_>>(), expected);
        assert_eq!(std::fs::read(&source).unwrap(), exported.bytes);
    }
}

#[test]
fn bridge_control_routes_report_denial_and_preserve_last_completed_receipt() {
    fn request(app: &mut PhotocraftApp, ctx: &egui::Context, method: &str, params: Value) -> Value {
        let (request, _) = crate::control::ControlRequest::new(method, params);
        match crate::control::handle(app, ctx, &request) {
            crate::control::Outcome::Done(value) => value,
            _ => panic!("expected an immediate control acknowledgement"),
        }
    }
    let fixture = Fixture::new();
    let source = fixture.source("control.dng");
    let mut app = fixture.app();
    app.services.camera_raw_sidecar_publisher =
        Some(std::sync::Arc::new(|_, _| Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no write root"))));
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 1.25);
    settled(&mut app, &ctx, 1);
    let synthetic = request(&mut app, &ctx, "ui.click", json!({"x":5,"y":5}));
    assert_eq!(synthetic["ok"], false, "generic synthetic input is unavailable during held review");
    let response = request(&mut app, &ctx, "ui.menu.invoke", json!({"id":"filter.cameraRaw","params":{"ui":{"persistence":{"action":"saveEdits"}}}}));
    assert_eq!(response["ok"], true);
    let plan = wait_for(&mut app, &ctx, "control save plan", |_, s| s["busy"] == false && !s["plan"].is_null());
    let response = request(
        &mut app,
        &ctx,
        "ui.menu.invoke",
        json!({"id":"filter.cameraRaw","params":{"ui":{"persistence":{"action":"save","planId":plan["plan"]["id"]}}}}),
    );
    assert_eq!(response["ok"], true, "this acknowledges dispatch only");
    wait_for(&mut app, &ctx, "control write denial", |_, s| s["busy"] == false && s["error"].is_string());
    let inspected = request(&mut app, &ctx, "ui.inspect", json!({}));
    assert_eq!(inspected["result"]["cameraRawPersistence"]["lastSave"]["completed"], false);
    assert!(inspected["result"]["cameraRawPersistence"]["lastSave"]["outcomes"][0]["outcome"].get("Failed").is_some());
    assert!(!source.with_extension("xmp").exists());
}

#[test]
fn bridge_automatic_cache_acknowledges_edits_without_writing_xmp() {
    let fixture = Fixture::new();
    let source = fixture.source("automatic.dng");
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &source);
    let initial = settled(&mut app, &context, 1);
    assert!(!initial["photos"][0]["pendingXmp"].as_bool().unwrap());
    exposure(&mut app, &context, 1.25);
    let saved = settled(&mut app, &context, 1);
    assert_eq!(saved["photos"][0]["recipe"]["adjustments"]["exposure"], 1.25);
    assert_eq!(saved["photos"][0]["pendingXmp"], true);
    assert!(saved["status"].as_str().unwrap().starts_with("Saved to recovery cache"));
    assert!(!source.with_extension("xmp").exists());
    assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
}

#[test]
fn bridge_previous_reply_does_not_acknowledge_newer_edits() {
    let fixture = Fixture::new();
    let source = fixture.source("reply-order.dng");
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &source);
    settled(&mut app, &context, 1);
    exposure(&mut app, &context, 1.0);
    frame(&mut app, &context);
    let sent = control(&mut app, "retryCache");
    exposure(&mut app, &context, 2.0);
    frame(&mut app, &context);
    let newer = persistence_ui::describe(&app);
    assert!(newer["revision"].as_u64().unwrap() > sent["revision"].as_u64().unwrap());
    assert_ne!(newer["acknowledged"], newer["revision"]);
    let saved = settled(&mut app, &context, 1);
    assert_eq!(saved["photos"][0]["recipe"]["adjustments"]["exposure"], 2.0);
}

#[test]
fn bridge_done_writes_full_metadata_and_fresh_file_open_reconstructs_it() {
    let fixture = Fixture::new();
    let source = fixture.source("done.dng");
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &source);
    let state = settled(&mut app, &context, 1);
    let id = runtime(&state, 0);
    persistence_ui::set_metadata(&mut app, &[id], |metadata| {
        metadata.rating = Some(4);
        metadata.label = "Client literal label".into();
        metadata.marked = true;
    })
    .unwrap();
    exposure(&mut app, &context, 0.7853281);
    settled(&mut app, &context, 1);
    let expected = persistence_ui::recipe(&app, id).unwrap().clone();
    control(&mut app, "done");
    let prepared = wait_for(&mut app, &context, "concrete Done plan", |_, state| !state["plan"].is_null() && state["busy"] == false);
    assert_eq!(prepared["plan"]["photos"].as_array().unwrap().len(), 1);
    persistence_ui::control(&mut app, &json!({"action":"save","planId":prepared["plan"]["id"]})).unwrap();
    closed(&mut app, &context);
    assert_eq!(photocraft_io::raw_session::xmp::load_native(&std::fs::read(source.with_extension("xmp")).unwrap()).unwrap(), Some(expected.clone()));
    assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
    drop(app);
    let mut fresh = fixture.app();
    open(&mut fresh, &source);
    let state = settled(&mut fresh, &context, 1);
    assert_eq!(persistence_ui::recipe(&fresh, runtime(&state, 0)), Some(&expected));
    assert_eq!(state["photos"][0]["pendingXmp"], false);
}

#[test]
fn bridge_duplicate_physical_input_coalesces_and_accounts_for_both_requests() {
    let fixture = Fixture::new();
    let source = fixture.source("one.dng");
    let alias = fixture.root.join("inputs/alias.dng");
    std::fs::hard_link(&source, &alias).unwrap();
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &source);
    open(&mut app, &alias);
    let state = settled(&mut app, &context, 1);
    assert_eq!(state["requests"].as_array().unwrap().len(), 2);
    assert_eq!(filmstrip_raws(&app).len(), 1);
    assert_eq!(app.session.documents().len(), 1);
}

/// B09/B10 (Candidate F): an interrupted opted-in session is offered; "Resume recovered
/// edits" restores the acknowledged recipe; Cancel preserves that entry draft.
#[test]
fn bridge_interrupted_session_resumed_cancel_preserves_entry_draft() {
    let fixture = Fixture::new();
    let source = fixture.source("resume.dng");
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &source);
    settled(&mut app, &context, 1);
    exposure(&mut app, &context, 1.0);
    let saved = settled(&mut app, &context, 1);
    let session = saved["session"].clone();
    interrupt(app, &fixture, session.as_str().unwrap());
    let mut resumed = fixture.app();
    let offered = wait_for(&mut resumed, &context, "read-only recovery discovery", |_, state| state["chooser"] == true && state["busy"] == false);
    assert_eq!(offered["discovered"][0]["unsavedPhotos"], 1);
    persistence_ui::control(&mut resumed, &json!({"action":"resume","sessionId":session})).unwrap();
    let restored = settled(&mut resumed, &context, 1);
    assert_eq!(restored["photos"][0]["recipe"]["adjustments"]["exposure"], 1.0);
    assert_eq!(restored["photos"][0]["pendingXmp"], true);
    resumed.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"off"})).unwrap();
    let off = wait_for(&mut resumed, &context, "resumed opt-out", |_, state| state["busy"] == false && state["recoveryEnabled"] == false);
    assert!(off["acknowledged"].is_null(), "off mode must not advertise a current checkpoint");
    let authority = fixture.cache().join(session.as_str().unwrap()).join("authority.json");
    let before = std::fs::read(&authority).unwrap();
    exposure(&mut resumed, &context, 2.0);
    let edited =
        wait_for(&mut resumed, &context, "off-mode edit", |_, state| state["busy"] == false && state["photos"][0]["recipe"]["adjustments"]["exposure"] == 2.0);
    assert!(edited["acknowledged"].is_null());
    assert_eq!(std::fs::read(&authority).unwrap(), before);
    persistence_ui::control(&mut resumed, &json!({"action":"discard","confirm":true})).unwrap();
    closed(&mut resumed, &context);
    drop(resumed);
    let (again, _) = photocraft_io::raw_session::controller::Controller::resume(&fixture.cache(), session.as_str().unwrap()).unwrap();
    assert_eq!(again.session().photos[0].working.adjustments.exposure, 1.0);
    assert!(!source.with_extension("xmp").exists());
}

#[test]
fn bridge_failed_import_is_accounted_without_dropping_other_valid_photos() {
    let fixture = Fixture::new();
    let source = fixture.source("good.dng");
    let bad = fixture.root.join("inputs/bad.dng");
    std::fs::write(&bad, b"not a raw image").unwrap();
    let mut app = fixture.app();
    let context = context();
    assert!(app.open_bytes_from("bad.dng", b"not a raw image", Some(bad.to_str().unwrap())).is_err());
    open(&mut app, &source);
    let state = settled(&mut app, &context, 1);
    assert_eq!(state["requests"].as_array().unwrap().len(), 2);
    assert!(state["requests"].as_array().unwrap().iter().any(|request| request["outcome"].get("Failed").is_some()));
    control(&mut app, "done");
    // Nothing changed: the no-op plan needs no consent, writes nothing and ends the session.
    closed(&mut app, &context);
    let receipt = persistence_ui::describe(&app);
    assert_eq!(receipt["lastSave"]["action"], "done");
    assert_eq!(receipt["lastSave"]["completed"], true);
    assert!(receipt["lastSave"]["outcomes"].as_array().unwrap().is_empty());
    assert!(!source.with_extension("xmp").exists());
    assert!(session_dirs(&fixture).is_empty());
}

#[test]
fn bridge_resume_replaces_incoming_provisional_batch_and_its_ledger() {
    let fixture = Fixture::new();
    let old = fixture.source("old-draft.dng");
    let incoming = fixture.source("incoming.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &old);
    let saved = settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 1.0);
    settled(&mut app, &ctx, 1);
    interrupt(app, &fixture, saved["session"].as_str().unwrap());
    let mut resumed = fixture.app();
    let ctx = context();
    open(&mut resumed, &incoming);
    wait_for(&mut resumed, &ctx, "recovery choice with incoming batch", |_, state| state["chooser"] == true && state["busy"] == false);
    persistence_ui::control(&mut resumed, &json!({"action":"resume","sessionId":saved["session"]})).unwrap();
    let restored = settled(&mut resumed, &ctx, 1);
    assert_eq!(restored["requests"].as_array().unwrap().len(), 1);
    assert_eq!(restored["requests"][0]["name"], "old-draft.dng");
    assert_eq!(restored["photos"][0]["recipe"]["adjustments"]["exposure"], 1.0);
    assert_eq!(resumed.session.documents().len(), 1);
    assert!(filmstrip_raws(&resumed).iter().all(|raw| raw.name == "old-draft.dng"));
    control(&mut resumed, "done");
    wait_for(&mut resumed, &ctx, "Done has no incoming pending ticket", |_, state| state["busy"] == false && !state["plan"].is_null());
    assert!(!incoming.with_extension("xmp").exists());
}

#[test]
fn bridge_retry_admission_after_sidecar_ambiguity_resumes_automatic_saving() {
    let fixture = Fixture::new();
    let source = fixture.source("ambiguous.dng");
    let packet = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about=''/></rdf:RDF></x:xmpmeta>";
    std::fs::write(source.with_extension("xmp"), packet).unwrap();
    let other = source.with_file_name("ambiguous.dng.xmp");
    std::fs::write(&other, packet).unwrap();
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &source);
    wait_for(&mut app, &context, "failed binding retained for retry", |_, state| state["failedAdmissionCount"] == 1 && state["busy"] == false);
    assert!(persistence_ui::before_open(&mut app).is_err());
    std::fs::remove_file(other).unwrap();
    control(&mut app, "retryCache");
    settled(&mut app, &context, 1);
    exposure(&mut app, &context, 1.25);
    let saved = settled(&mut app, &context, 1);
    assert_eq!(saved["photos"][0]["recipe"]["adjustments"]["exposure"], 1.25);
    assert_eq!(saved["failedAdmissionCount"], 0);
    assert_eq!(std::fs::read(source.with_extension("xmp")).unwrap(), packet);
}

#[test]
fn bridge_missing_retained_source_blocks_handoff_then_verified_retry_restores_it() {
    let fixture = Fixture::new();
    let first = fixture.source("available.dng");
    let second = fixture.source("missing.dng");
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &first);
    open(&mut app, &second);
    let saved = settled(&mut app, &context, 2);
    interrupt(app, &fixture, saved["session"].as_str().unwrap());
    let moved = fixture.root.join("missing-offline.dng");
    std::fs::rename(&second, &moved).unwrap();
    let mut app = fixture.app();
    wait_for(&mut app, &context, "discover missing-source draft", |_, state| state["chooser"] == true && state["busy"] == false);
    persistence_ui::control(&mut app, &json!({"action":"resume","sessionId":saved["session"]})).unwrap();
    settled(&mut app, &context, 1);
    assert!(commit(&mut app).is_err(), "A missing retained photo cannot be silently omitted");
    assert_eq!(app.session.provisional_documents().len(), 1);
    std::fs::rename(&moved, &second).unwrap();
    control(&mut app, "retryCache");
    settled(&mut app, &context, 2);
    commit(&mut app).unwrap();
    for action in ["back", "done", "keep", "discard", "remove"] {
        assert!(persistence_ui::control(&mut app, &json!({"action":action,"photo":runtime(&saved,0).0})).is_err(), "{action} must not thaw Open Images");
    }
    assert!(menu(&mut app, &context, "filter.cameraRaw", &json!({"ui":{"set":{"exposure":3.0}}})).unwrap().is_err());
    closed(&mut app, &context);
    assert_eq!(app.session.documents().len(), 2);
    assert!(app.session.provisional_documents().is_empty());
    // Unedited photos: Open Images' no-op save writes no sidecar, and the session ends.
    assert!(!first.with_extension("xmp").exists() && !second.with_extension("xmp").exists());
    assert!(session_dirs(&fixture).is_empty());
}

#[test]
fn bridge_failed_preflight_returns_to_editing_and_keeps_the_conflicting_sidecar() {
    for remove in [false, true] {
        let fixture = Fixture::new();
        let source = fixture.source("external-change.dng");
        let mut app = fixture.app();
        let context = context();
        open(&mut app, &source);
        let admitted = settled(&mut app, &context, 1);
        exposure(&mut app, &context, 1.25);
        settled(&mut app, &context, 1);
        let external = b"external sidecar created after admission";
        std::fs::write(source.with_extension("xmp"), external).unwrap();
        if remove {
            persistence_ui::control(&mut app, &json!({"action":"remove","photo":runtime(&admitted,0).0})).unwrap();
        } else {
            control(&mut app, "done");
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let failed = loop {
            frame(&mut app, &context);
            let state = persistence_ui::describe(&app);
            if state["busy"] == false && state["error"].is_string() {
                break state;
            }
            assert!(Instant::now() < deadline, "preflight never finished: {state}");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(failed["frozen"], false, "preflight failure must return to editing");
        assert!(failed["plan"].is_null());
        assert_eq!(failed["photos"].as_array().unwrap().len(), 1);
        exposure(&mut app, &context, 1.5);
        // The preflight refusal stays visible; wait only for the newer edit's checkpoint.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            frame(&mut app, &context);
            let state = persistence_ui::describe(&app);
            if state["busy"] == false && !state["acknowledged"].is_null() && state["acknowledged"] == state["revision"] {
                break;
            }
            assert!(Instant::now() < deadline, "the newer edit was never acknowledged: {state}");
            std::thread::sleep(Duration::from_millis(5));
        }
        interrupt(app, &fixture, admitted["session"].as_str().unwrap());
        assert_eq!(std::fs::read(source.with_extension("xmp")).unwrap(), external);
        let saved = resume_saved(&fixture.cache(), admitted["session"].as_str().unwrap());
        assert_eq!(saved.session().photos[0].working.adjustments.exposure, 1.5);
    }
}

/// B04 (Candidate F, supersedes E's "Open Images keeps an unsaved recovery draft"): Open
/// Images reviews and saves the same XMP plan as Done first, then hands the batch to the
/// editor, and the completed session leaves nothing to recover.
#[test]
fn bridge_open_images_saves_xmp_before_handoff_and_leaves_nothing_to_recover() {
    let fixture = Fixture::new();
    let source = fixture.source("editor.dng");
    let before = std::fs::read(&source).unwrap();
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &source);
    settled(&mut app, &context, 1);
    exposure(&mut app, &context, 1.0);
    settled(&mut app, &context, 1);
    commit(&mut app).unwrap();
    let plan = wait_for(&mut app, &context, "Open Images XMP plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    assert_eq!(plan["finishing"], "Open");
    assert_eq!(plan["plan"]["photos"].as_array().unwrap().len(), 1);
    assert!(!source.with_extension("xmp").exists(), "nothing is written before consent");
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    closed(&mut app, &context);
    assert_eq!(app.session.documents().len(), 1);
    assert!(app.session.provisional_documents().is_empty());
    let packet = std::fs::read(source.with_extension("xmp")).unwrap();
    assert_eq!(photocraft_io::raw_session::xmp::load_native(&packet).unwrap().unwrap().adjustments.exposure, 1.0);
    assert_eq!(std::fs::read(&source).unwrap(), before);
    assert_eq!(app.raw_filmstrip.last_persistence_result.as_ref().unwrap()["action"], "openImages");
    assert!(session_dirs(&fixture).is_empty(), "{:?}", session_dirs(&fixture));
}

#[test]
fn bridge_guarded_save_and_remove_keeps_other_member_pending() {
    let fixture = Fixture::new();
    let first = fixture.source("remove.dng");
    let second = fixture.source("kept.dng");
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &first);
    open(&mut app, &second);
    let state = settled(&mut app, &context, 2);
    let id = runtime(&state, 0);
    let other = runtime(&state, 1);
    persistence_ui::set_metadata(&mut app, &[id, other], |metadata| metadata.rating = Some(3)).unwrap();
    settled(&mut app, &context, 2);
    persistence_ui::control(&mut app, &json!({"action":"remove","photo":id.0})).unwrap();
    let prepared = wait_for(&mut app, &context, "guarded single-member removal", |_, state| !state["plan"].is_null() && state["busy"] == false);
    assert_eq!(prepared["plan"]["photos"].as_array().unwrap().len(), 1);
    persistence_ui::control(&mut app, &json!({"action":"save","planId":prepared["plan"]["id"]})).unwrap();
    let retained = settled(&mut app, &context, 1);
    assert_eq!(runtime(&retained, 0), other);
    assert_eq!(retained["photos"][0]["pendingXmp"], true);
    assert_eq!(filmstrip_raws(&app).len(), 1);
    assert!(first.with_extension("xmp").exists());
    assert!(!second.with_extension("xmp").exists());
}

#[test]
#[ignore = "requires PHOTOCRAFT_RAW_UI_FIXTURE_MANIFEST pointing to the verified 22-NEF manifest; full production decoding is intentionally explicit"]
fn bridge_real_twenty_two_nef_disposable_admission() {
    let manifest = std::env::var_os("PHOTOCRAFT_RAW_UI_FIXTURE_MANIFEST").expect("Set the verified 22-NEF fixture manifest path");
    let records: Vec<Value> = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    let fixture = Fixture::new();
    let mut copied = Vec::new();
    for record in records {
        let pristine = PathBuf::from(record["copy"].as_str().unwrap());
        let target = fixture.root.join("inputs").join(pristine.file_name().unwrap());
        assert!(record["source_unchanged"].as_bool().unwrap(), "Fixture manifest contains an unverified source");
        std::fs::copy(&pristine, &target).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().len(), record["bytes"].as_u64().unwrap());
        copied.push((pristine, target));
    }
    let mut paths: Vec<_> = copied
        .iter()
        .filter(|(_, path)| path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("nef")))
        .map(|(_, path)| path.clone())
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 22);
    let mut app = fixture.app();
    let context = context();
    for path in &paths {
        open(&mut app, path);
    }
    frame(&mut app, &context);
    assert!(persistence_ui::control(&mut app, &json!({"action":"done"})).is_err(), "Done cannot freeze the real batch before native admission settles");
    assert!(persistence_ui::describe(&app)["plan"].is_null());
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        frame(&mut app, &context);
        let state = persistence_ui::describe(&app);
        assert!(state.get("error").is_none_or(Value::is_null), "Real NEF admission: {state}");
        if state["photos"].as_array().is_some_and(|photos| photos.len() == 22)
            && state["busy"] == false
            && !state["acknowledged"].is_null()
            && state["acknowledged"] == state["revision"]
        {
            break;
        }
        assert!(Instant::now() < deadline, "22-NEF admission timed out: {state}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(filmstrip_raws(&app).len(), 22);
    assert_eq!(crate::camera_raw_admission::snapshot(&app).len(), 22);
    assert!(crate::camera_raw_admission::settled(&app));
    assert_eq!(app.session.provisional_documents().len(), 22);
    for (pristine, copy) in &copied {
        assert_eq!(std::fs::read(pristine).unwrap(), std::fs::read(copy).unwrap(), "Admission modified a retained source/sidecar");
    }
    // Candidate F: the normal Cancel ends the unedited session; nothing is left to recover.
    control(&mut app, "discard");
    closed(&mut app, &context);
    assert!(session_dirs(&fixture).is_empty());
    println!(
        "Production bridge admitted all 22 verified NEF copies; requests settled, all provisional members retained, recovery acknowledged, source and sidecar bytes unchanged"
    );
}

// Each helper is a Rust test executable with embedded egui only. The parent owns
// its Child handle and kills only the cache writer it spawned after its receipt.
const PROCESS_HELPER: &str = "camera_raw_ui::persistence_tests::bridge_process_helper";

fn child(fixture: &Fixture, mode: &str, selection: &str) -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", PROCESS_HELPER, "--ignored", "--nocapture"])
        .env("PHOTOCRAFT_BRIDGE_TEST_ROOT", &fixture.root)
        .env("PHOTOCRAFT_BRIDGE_TEST_MODE", mode)
        .env("PHOTOCRAFT_BRIDGE_TEST_SELECTION", selection)
        .stdout(std::fs::File::create(fixture.root.join(format!("{mode}-{selection}.stdout"))).unwrap())
        .stderr(std::fs::File::create(fixture.root.join(format!("{mode}-{selection}.stderr"))).unwrap());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command.spawn().unwrap()
}

fn child_receipt(child: &mut std::process::Child, fixture: &Fixture, name: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(bytes) = std::fs::read(fixture.root.join(name))
            && let Ok(receipt) = serde_json::from_slice(&bytes)
        {
            return receipt;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("Helper exited before {name}: {status}; logs in {}", fixture.root.display());
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Helper timed out before {name}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child_success(child: &mut std::process::Child, fixture: &Fixture) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "Helper failed: {status}; logs in {}", fixture.root.display());
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Helper did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn live_view_receipt(app: &PhotocraftApp) -> Value {
    let active = app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.name.clone());
    let mut selected: Vec<_> = filmstrip_raws(app).iter().filter(|r| app.raw_filmstrip.selection.contains(r.photo)).map(|r| r.name.clone()).collect();
    selected.sort();
    let mut views = std::collections::BTreeMap::new();
    for parked in &app.raw_filmstrip.parked {
        views.insert(parked.raw.name.clone(), json!({"scope":parked.scope,"navigation":parked.navigation,"before":parked.show_before}));
    }
    if let Some(dialog) = &app.camera_raw
        && let Some(raw) = &dialog.raw_open
    {
        views.insert(raw.name.clone(), json!({"scope":app.ui.camera_raw_scope,"navigation":app.ui.camera_raw_preview,"before":dialog.show_before}));
    }
    let recipes: std::collections::BTreeMap<_, _> =
        filmstrip_raws(app).iter().map(|raw| (raw.name.clone(), serde_json::to_value(persistence_ui::recipe(app, raw.photo).unwrap()).unwrap())).collect();
    json!({"active":active,"selected":selected,"views":views,"recipes":recipes})
}

#[test]
fn bridge_a05_independent_process_recovers_acknowledged_work_and_view_state() {
    for selection in ["empty", "one"] {
        let fixture = Fixture::new();
        let first = fixture.source("first.dng");
        let second = fixture.source("second.dng");
        let mut writer = child(&fixture, "cache-writer", selection);
        let receipt = child_receipt(&mut writer, &fixture, "cache-ack.json");
        assert_eq!(receipt["pid"], writer.id());
        assert!(writer.try_wait().unwrap().is_none());
        writer.kill().unwrap();
        writer.wait().unwrap();
        let mut reader = child(&fixture, "resume-reader", selection);
        child_success(&mut reader, &fixture);
        let verified: Value = serde_json::from_slice(&std::fs::read(fixture.root.join("resume-pass.json")).unwrap()).unwrap();
        assert_ne!(verified["pid"], receipt["pid"]);
        assert_eq!(verified["view"], receipt["view"]);
        for path in [first, second] {
            assert_eq!(std::fs::read(&path).unwrap(), fixture.bytes);
            assert!(!path.with_extension("xmp").exists());
        }
    }
}

#[test]
fn bridge_a10_independent_process_done_reloads_without_original_cache() {
    let fixture = Fixture::new();
    let source = fixture.source("first.dng");
    let mut writer = child(&fixture, "done-writer", "one");
    child_success(&mut writer, &fixture);
    let expected: Value = serde_json::from_slice(&std::fs::read(fixture.root.join("done-receipt.json")).unwrap()).unwrap();
    assert!(source.with_extension("xmp").exists());
    let mut reader = child(&fixture, "sidecar-reader", "one");
    child_success(&mut reader, &fixture);
    let verified: Value = serde_json::from_slice(&std::fs::read(fixture.root.join("sidecar-pass.json")).unwrap()).unwrap();
    assert_ne!(verified["pid"], expected["pid"]);
    assert_eq!(verified["recipe"], expected["recipe"]);
    assert_eq!(verified["cache"], fixture.root.join("fresh-file-cache").to_string_lossy().as_ref());
    assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
}

#[test]
#[ignore = "exact subprocess helper; launched by the A05/A10 parent tests with an isolated root"]
fn bridge_process_helper() {
    let Some(root) = std::env::var_os("PHOTOCRAFT_BRIDGE_TEST_ROOT") else { return };
    let root = PathBuf::from(root);
    let mode = std::env::var("PHOTOCRAFT_BRIDGE_TEST_MODE").unwrap();
    let selection = std::env::var("PHOTOCRAFT_BRIDGE_TEST_SELECTION").unwrap();
    let fixture = std::mem::ManuallyDrop::new(Fixture { bytes: std::fs::read(root.join("inputs/first.dng")).unwrap(), root });
    let context = context();
    let source = fixture.root.join("inputs/first.dng");
    if mode == "cache-writer" {
        let mut app = fixture.app();
        app.open_paths(&[source.to_string_lossy().into_owned(), fixture.root.join("inputs/second.dng").to_string_lossy().into_owned()]);
        settled(&mut app, &context, 2);
        let raws = filmstrip_raws(&app);
        for (index, raw) in raws.iter().enumerate() {
            select_raw_review(&mut app, &context, raw.document).unwrap();
            app.raw_filmstrip.selection.select_only(raw.photo);
            exposure(&mut app, &context, 0.75 + index as f32);
            app.ui.camera_raw_scope.shadows = index == 0;
            app.ui.camera_raw_scope.highlights = index == 1;
            app.ui.camera_raw_preview.zoom = Some(2.0 + index as f32);
            app.camera_raw.as_mut().unwrap().show_before = index == 0;
        }
        if selection == "empty" {
            app.raw_filmstrip.selection.restore_ids(&[], None, None);
        } else {
            app.raw_filmstrip.selection.select_only(raws[0].photo);
        }
        let saved = settled(&mut app, &context, 2);
        let receipt = json!({"pid":std::process::id(),"session":saved["session"],"view":live_view_receipt(&app)});
        std::fs::write(fixture.root.join("cache-ack.json"), serde_json::to_vec(&receipt).unwrap()).unwrap();
        // The parent deliberately terminates this recorded process after ack.
        loop {
            std::thread::sleep(Duration::from_millis(100));
        }
    } else if mode == "resume-reader" {
        let receipt: Value = serde_json::from_slice(&std::fs::read(fixture.root.join("cache-ack.json")).unwrap()).unwrap();
        let mut app = fixture.app();
        wait_for(&mut app, &context, "independent recovery discovery", |_, state| state["chooser"] == true && state["busy"] == false);
        persistence_ui::control(&mut app, &json!({"action":"resume","sessionId":receipt["session"]})).unwrap();
        wait_for(&mut app, &context, "independent restored views and acknowledged edits", |app, state| {
            if selection == "empty" {
                assert_eq!(app.raw_filmstrip.selection.len(), 0, "Empty selected set changed while photos arrived");
            }
            state["photos"].as_array().is_some_and(|p| p.len() == 2)
                && app.camera_raw.is_some()
                && state["busy"] == false
                && !state["acknowledged"].is_null()
                && state["acknowledged"] == state["revision"]
        });
        let view = live_view_receipt(&app);
        assert_eq!(view, receipt["view"]);
        std::fs::write(fixture.root.join("resume-pass.json"), serde_json::to_vec(&json!({"pid":std::process::id(),"view":view})).unwrap()).unwrap();
    } else if mode == "done-writer" {
        let mut app = fixture.app();
        open(&mut app, &source);
        let state = settled(&mut app, &context, 1);
        let id = runtime(&state, 0);
        exposure(&mut app, &context, 0.5);
        settled(&mut app, &context, 1);
        save_edits(&mut app, &context);
        assert!(app.camera_raw.is_some());
        exposure(&mut app, &context, 0.7853281);
        persistence_ui::set_metadata(&mut app, &[id], |m| {
            m.rating = Some(4);
            m.label = "Independent process literal label".into();
            m.marked = true;
        })
        .unwrap();
        settled(&mut app, &context, 1);
        let recipe = persistence_ui::recipe(&app, id).unwrap().clone();
        control(&mut app, "done");
        let state = wait_for(&mut app, &context, "independent Done confirmation", |_, state| !state["plan"].is_null() && state["busy"] == false);
        persistence_ui::control(&mut app, &json!({"action":"save","planId":state["plan"]["id"]})).unwrap();
        closed(&mut app, &context);
        std::fs::write(fixture.root.join("done-receipt.json"), serde_json::to_vec(&json!({"pid":std::process::id(),"recipe":recipe})).unwrap()).unwrap();
    } else if mode == "sidecar-reader" {
        let receipt: Value = serde_json::from_slice(&std::fs::read(fixture.root.join("done-receipt.json")).unwrap()).unwrap();
        let cache = fixture.root.join("fresh-file-cache");
        assert!(!cache.exists());
        let mut app = fixture.app_at_cache(cache.clone());
        open(&mut app, &source);
        let state = settled(&mut app, &context, 1);
        assert_eq!(state["photos"][0]["recipe"], receipt["recipe"]);
        assert_eq!(state["photos"][0]["pendingXmp"], false);
        std::fs::write(
            fixture.root.join("sidecar-pass.json"),
            serde_json::to_vec(&json!({"pid":std::process::id(),"cache":cache,"recipe":state["photos"][0]["recipe"]})).unwrap(),
        )
        .unwrap();
    } else {
        panic!("Unknown bridge helper mode");
    }
}

#[cfg(windows)]
#[test]
fn bridge_windows_denied_removal_checkpoint_keeps_member_until_retry_ack() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let source = fixture.source("denied-remove.dng");
    let mut app = fixture.app();
    let context = context();
    open(&mut app, &source);
    let state = settled(&mut app, &context, 1);
    let id = runtime(&state, 0);
    exposure(&mut app, &context, 1.5);
    settled(&mut app, &context, 1);
    persistence_ui::control(&mut app, &json!({"action":"remove","photo":id.0})).unwrap();
    let prepared = wait_for(&mut app, &context, "prepared removal", |_, state| !state["plan"].is_null() && state["busy"] == false);
    let authority = fixture.cache().join(prepared["session"].as_str().unwrap()).join("authority.json");
    let denied = std::fs::OpenOptions::new().read(true).share_mode(0).open(&authority).unwrap();
    control(&mut app, "discardPhoto");
    wait_for(&mut app, &context, "denied removal recovery publication", |_, state| !state["error"].is_null() && state["busy"] == false);
    assert_eq!(filmstrip_raws(&app).len(), 1);
    assert_eq!(app.session.documents().len(), 1);
    assert!(!source.with_extension("xmp").exists());
    drop(denied);
    control(&mut app, "retryCache");
    wait_for(&mut app, &context, "removal retry acknowledged", |_, state| {
        state["photos"].as_array().is_some_and(Vec::is_empty) && state["busy"] == false && state["error"].is_null()
    });
    assert!(filmstrip_raws(&app).is_empty());
    assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
}

#[test]
fn default_mode_edits_without_cache_and_done_still_saves_xmp() {
    let fixture = Fixture::new();
    let source = fixture.source("default-mode.dng");
    let mut app = fixture.app();
    app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"off"})).unwrap();
    let ctx = context();
    open(&mut app, &source);
    wait_for(&mut app, &ctx, "memory admission", |app, state| {
        app.camera_raw.is_some() && state["busy"] == false && state["photos"].as_array().is_some_and(|p| p.len() == 1)
    });
    exposure(&mut app, &ctx, 0.7);
    for _ in 0..5 {
        frame(&mut app, &ctx);
    }
    assert!(!fixture.cache().exists(), "default editing must not create a recovery root");
    assert!(!source.with_extension("xmp").exists());
    assert_eq!(persistence_ui::describe(&app)["recoveryEnabled"], false);
    control(&mut app, "done");
    let state = wait_for(&mut app, &ctx, "explicit Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    assert!(!source.with_extension("xmp").exists(), "planning waits for explicit XMP approval");
    persistence_ui::control(&mut app, &json!({"action":"save","planId":state["plan"]["id"]})).unwrap();
    closed(&mut app, &ctx);
    let packet = std::fs::read(source.with_extension("xmp")).unwrap();
    let recipe = photocraft_io::raw_session::xmp::load_native(&packet).unwrap().unwrap();
    assert_eq!(recipe.adjustments.exposure, 0.7);
    assert!(app.session.documents().is_empty());
}

#[test]
fn off_mode_returns_to_memory_after_preflight_failure_and_explicit_save() {
    let fixture = Fixture::new();
    let source = fixture.source("off-explicit-save.dng");
    let second = fixture.source("off-additional.dng");
    let mut app = fixture.app();
    app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"off"})).unwrap();
    let ctx = context();
    open(&mut app, &source);
    wait_for(&mut app, &ctx, "memory admission", |app, state| app.camera_raw.is_some() && state["busy"] == false);
    exposure(&mut app, &ctx, 0.6);
    std::fs::write(source.with_extension("xmp"), b"external-sidecar-change").unwrap();
    control(&mut app, "done");
    wait_for(&mut app, &ctx, "preflight failure", |_, state| state["busy"] == false && !state["error"].is_null());
    assert!(photocraft_io::raw_session::store::discover(&fixture.cache()).unwrap().is_empty());
    std::fs::remove_file(source.with_extension("xmp")).unwrap();
    control(&mut app, "saveEdits");
    let state = wait_for(&mut app, &ctx, "save plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    persistence_ui::control(&mut app, &json!({"action":"save","planId":state["plan"]["id"]})).unwrap();
    wait_for(&mut app, &ctx, "save remains editing", |_, state| state["busy"] == false && state["plan"].is_null() && state["error"].is_null());
    assert!(app.camera_raw.is_some());
    assert!(source.with_extension("xmp").exists());
    assert!(photocraft_io::raw_session::store::discover(&fixture.cache()).unwrap().is_empty());
    open(&mut app, &second);
    wait_for(&mut app, &ctx, "further in-memory admission", |_, state| state["busy"] == false && state["photos"].as_array().is_some_and(|p| p.len() == 2));
    assert!(photocraft_io::raw_session::store::discover(&fixture.cache()).unwrap().is_empty());
    assert_eq!(persistence_ui::describe(&app)["acknowledged"], Value::Null);
    persistence_ui::control(&mut app, &json!({"action":"discard","confirm":true})).unwrap();
    closed(&mut app, &ctx);
    let packet = std::fs::read(source.with_extension("xmp")).unwrap();
    assert_eq!(photocraft_io::raw_session::xmp::load_native(&packet).unwrap().unwrap().adjustments.exposure, 0.6);
}

#[test]
fn recovery_can_be_enabled_then_disabled_without_losing_open_edits() {
    let fixture = Fixture::new();
    let source = fixture.source("mode-toggle.dng");
    let mut app = fixture.app();
    app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"off"})).unwrap();
    let ctx = context();
    open(&mut app, &source);
    wait_for(&mut app, &ctx, "memory admission", |app, state| app.camera_raw.is_some() && state["busy"] == false);
    exposure(&mut app, &ctx, 0.9);
    app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"cache"})).unwrap();
    settled(&mut app, &ctx, 1);
    assert!(fixture.cache().exists());
    app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"off"})).unwrap();
    let state = wait_for(&mut app, &ctx, "recovery disabled", |_, state| state["busy"] == false && state["recoveryEnabled"] == false);
    assert_eq!(state["photos"][0]["recipe"]["adjustments"]["exposure"], json!(0.9f32));
    assert!(photocraft_io::raw_session::store::discover(&fixture.cache()).unwrap().is_empty());
    assert!(!source.with_extension("xmp").exists());
    persistence_ui::control(&mut app, &json!({"action":"discard","confirm":true})).unwrap();
    closed(&mut app, &ctx);
}

#[test]
fn opted_in_dialogs_offer_keep_while_done_defaults_to_xmp() {
    let fixture = Fixture::new();
    let source = fixture.source("decisions.dng");
    let mut app = fixture.app();
    let ctx = context();
    ctx.enable_accesskit();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.4);
    settled(&mut app, &ctx, 1);
    assert!(persistence_ui::request_close(&mut app));
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, vec2(1360.0, 900.0))), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| show(&mut app, ui.ctx()));
    output.textures_delta.clear();
    let nodes = &output.platform_output.accesskit_update.as_ref().unwrap().nodes;
    let labels: Vec<_> = nodes.iter().filter_map(|(_, node)| node.label()).collect();
    assert!(labels.contains(&"Discard unsaved edits and close") && labels.contains(&"Back to editing"));
    assert!(labels.iter().any(|label| label.starts_with("Keep in recovery and close")));
    assert!(!source.with_extension("xmp").exists());
    // Escape returns to the same edited review instead of discarding it.
    let input = egui::RawInput {
        screen_rect: Some(ERect::from_min_size(Pos2::ZERO, vec2(1360.0, 900.0))),
        events: vec![egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }],
        ..Default::default()
    };
    ctx.run_ui(input, |ui| show(&mut app, ui.ctx())).textures_delta.clear();
    assert!(app.camera_raw.is_some());
    assert!(!persistence_ui::blocked(&app), "Escape must dismiss only the Cancel confirmation");
    control(&mut app, "done");
    wait_for(&mut app, &ctx, "Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, vec2(1360.0, 900.0))), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| show(&mut app, ui.ctx()));
    output.textures_delta.clear();
    let labels: Vec<_> = output.platform_output.accesskit_update.as_ref().unwrap().nodes.iter().filter_map(|(_, node)| node.label()).collect();
    assert!(labels.contains(&"Save XMP and close") && labels.contains(&"Back to editing"));
    assert!(labels.iter().any(|label| label.starts_with("Keep in recovery and close")));
    let input = egui::RawInput {
        screen_rect: Some(ERect::from_min_size(Pos2::ZERO, vec2(1360.0, 900.0))),
        events: vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }],
        ..Default::default()
    };
    ctx.run_ui(input, |ui| show(&mut app, ui.ctx())).textures_delta.clear();
    closed(&mut app, &ctx);
    assert!(source.with_extension("xmp").exists(), "Enter uses the default Save XMP and close choice");
    assert!(app.session.documents().is_empty());
}

#[test]
fn focused_dialog_choices_take_precedence_over_the_enter_default() {
    fn key_frame(app: &mut PhotocraftApp, ctx: &egui::Context, key: egui::Key) -> egui::FullOutput {
        let events = [true, false]
            .into_iter()
            .map(|pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE })
            .collect();
        let mut output = ctx
            .run_ui(egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, vec2(1360.0, 900.0))), events, ..Default::default() }, |ui| {
                show(app, ui.ctx())
            });
        output.textures_delta.clear();
        output
    }
    fn focus_choice(app: &mut PhotocraftApp, ctx: &egui::Context, label: &str) {
        for _ in 0..16 {
            let output = key_frame(app, ctx, egui::Key::Tab);
            let focus = ctx.memory(|memory| memory.focused()).map(|id| id.accesskit_id());
            if output
                .platform_output
                .accesskit_update
                .as_ref()
                .is_some_and(|update| update.nodes.iter().any(|(id, node)| Some(*id) == focus && node.label() == Some(label)))
            {
                return;
            }
        }
        panic!("Could not focus dialog choice {label}");
    }
    let fixture = Fixture::new();
    let source = fixture.source("keyboard-decisions.dng");
    let mut app = fixture.app();
    let ctx = context();
    ctx.enable_accesskit();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.4);
    control(&mut app, "done");
    wait_for(&mut app, &ctx, "Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    focus_choice(&mut app, &ctx, "Back to editing");
    key_frame(&mut app, &ctx, egui::Key::Enter);
    wait_for(&mut app, &ctx, "Back from Done", |_, state| state["busy"] == false && state["plan"].is_null());
    assert!(app.camera_raw.is_some());
    assert!(!source.with_extension("xmp").exists());
    let id = runtime(&persistence_ui::describe(&app), 0);
    persistence_ui::control(&mut app, &json!({"action":"remove","photo":id.0})).unwrap();
    wait_for(&mut app, &ctx, "removal plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    focus_choice(&mut app, &ctx, "Keep in session");
    key_frame(&mut app, &ctx, egui::Key::Enter);
    wait_for(&mut app, &ctx, "Back from removal", |_, state| state["busy"] == false && state["plan"].is_null());
    assert_eq!(filmstrip_raws(&app).len(), 1);
    assert!(!source.with_extension("xmp").exists());
    assert!(persistence_ui::request_close(&mut app));
    focus_choice(&mut app, &ctx, "Discard unsaved edits and close");
    key_frame(&mut app, &ctx, egui::Key::Enter);
    closed(&mut app, &ctx);
    assert!(!source.with_extension("xmp").exists());
    assert!(app.session.documents().is_empty());
}

/// B02/B04 with recovery off (Candidate F): editing writes no cache; Open Images saves the
/// XMP first through a publication journal that is reclaimed afterwards.
#[test]
fn default_mode_open_images_saves_xmp_first_and_leaves_no_cache() {
    let fixture = Fixture::new();
    let source = fixture.source("default-open.dng");
    let mut app = fixture.app();
    app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"off"})).unwrap();
    let ctx = context();
    open(&mut app, &source);
    wait_for(&mut app, &ctx, "memory admission", |app, state| app.camera_raw.is_some() && state["busy"] == false);
    exposure(&mut app, &ctx, 0.3);
    assert!(session_dirs(&fixture).is_empty(), "editing with recovery off writes no cache");
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"commit":true}})).unwrap().unwrap();
    let plan = wait_for(&mut app, &ctx, "Open Images XMP plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    closed(&mut app, &ctx);
    assert_eq!(app.session.documents().len(), 1);
    assert!(!app.session.has_provisional());
    assert!(session_dirs(&fixture).is_empty(), "{:?}", session_dirs(&fixture));
    let packet = std::fs::read(source.with_extension("xmp")).unwrap();
    assert_eq!(photocraft_io::raw_session::xmp::load_native(&packet).unwrap().unwrap().adjustments.exposure, 0.3);
}

#[test]
#[ignore = "offscreen synthetic native-source dialog capture; requires PHOTOCRAFT_PREFERENCES_CAPTURES"]
fn revision_capture_native_decisions() {
    let dir = PathBuf::from(std::env::var_os("PHOTOCRAFT_PREFERENCES_CAPTURES").unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    for (tag, size) in [("1360x900", vec2(1360.0, 900.0)), ("900x600", vec2(900.0, 600.0))] {
        let fixture = Fixture::new();
        let source = fixture.source("Synthetic_Dialog.dng");
        let mut app = fixture.app();
        open(&mut app, &source);
        let renderer = egui_kittest::wgpu::WgpuTestRenderer::from_render_state(egui_kittest::wgpu::create_render_state(
            egui_kittest::wgpu::default_wgpu_setup(),
            eframe::egui_wgpu::RendererOptions::PREDICTABLE,
        ));
        let mut initialized = false;
        let mut harness = egui_kittest::Harness::builder().with_size(size).renderer(renderer).build_ui_state(
            |ui, app| {
                if !initialized {
                    PhotocraftApp::setup_context(ui.ctx(), crate::theme::ThemeKind::Pro);
                    initialized = true;
                    return;
                }
                show(app, ui.ctx());
            },
            app,
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            harness.run_steps(1);
            let state = persistence_ui::describe(harness.state());
            if harness.state().camera_raw.is_some() && state["busy"] == false && !state["acknowledged"].is_null() {
                break;
            }
            assert!(Instant::now() < deadline, "native-source preview capture timed out: {state}");
            std::thread::sleep(Duration::from_millis(10));
        }
        let ctx = harness.ctx.clone();
        exposure(harness.state_mut(), &ctx, 0.4);
        harness.run_steps(3);
        harness.render().unwrap().save(dir.join(format!("{tag}_Pro_recovery_hidden.png"))).unwrap();
        harness.state_mut().run("prefs.set", json!({"path":"rawDefaults.showRecoveryProgress","value":true})).unwrap();
        // Let the actual cache worker acknowledge the changed revision.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            harness.run_steps(1);
            let state = persistence_ui::describe(harness.state());
            if state["busy"] == false && state["acknowledged"] == state["revision"] {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        harness.run_steps(3);
        harness.render().unwrap().save(dir.join(format!("{tag}_Pro_recovery_visible.png"))).unwrap();
        assert!(persistence_ui::request_close(harness.state_mut()));
        harness.run_steps(3);
        harness.render().unwrap().save(dir.join(format!("{tag}_Pro_cancel.png"))).unwrap();
        harness.event(egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE });
        harness.run_steps(2);
        harness.event(egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: false, repeat: false, modifiers: egui::Modifiers::NONE });
        harness.run_steps(2);
        control(harness.state_mut(), "done");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            harness.run_steps(1);
            let state = persistence_ui::describe(harness.state());
            if state["busy"] == false && !state["plan"].is_null() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        harness.run_steps(3);
        harness.render().unwrap().save(dir.join(format!("{tag}_Pro_done.png"))).unwrap();
    }
}

// ---- Candidate F: normal exits, crash-only recovery and the two version cards ----------

fn labels_at(app: &mut PhotocraftApp, ctx: &egui::Context, size: egui::Vec2) -> Vec<(String, ERect)> {
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| show(app, ui.ctx()));
    output.textures_delta.clear();
    output
        .platform_output
        .accesskit_update
        .as_ref()
        .map(|update| {
            update
                .nodes
                .iter()
                .filter_map(|(_, node)| {
                    let bounds = node.bounds()?;
                    Some((node.label()?.to_owned(), ERect::from_min_max(pos2(bounds.x0 as f32, bounds.y0 as f32), pos2(bounds.x1 as f32, bounds.y1 as f32))))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// B07: Cancel with nothing unsaved ends the session at once: no prompt, no XMP, no cache.
#[test]
fn bridge_cancel_without_unsaved_edits_closes_without_asking() {
    let fixture = Fixture::new();
    let source = fixture.source("clean-cancel.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"cancel":true}})).unwrap().unwrap();
    // The terminal discard is asynchronous: the reply says so instead of claiming success.
    assert_eq!(reply, json!({"cancelled": false, "closing": true}));
    closed(&mut app, &ctx);
    assert!(app.session.documents().is_empty());
    assert!(!source.with_extension("xmp").exists());
    assert!(session_dirs(&fixture).is_empty());
}

/// B07/B20: with unsaved edits, Cancel reports the open confirmation truthfully; "Back to
/// editing" (control keepEditing) returns to the same review; the confirmed discard writes
/// no XMP and leaves nothing to recover.
#[test]
fn bridge_dirty_cancel_asks_and_back_to_editing_keeps_the_review() {
    let fixture = Fixture::new();
    let source = fixture.source("dirty-cancel.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.75);
    settled(&mut app, &ctx, 1);
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"cancel":true}})).unwrap().unwrap();
    assert_eq!(reply, json!({"cancelled": false, "confirmationRequired": true}));
    assert_eq!(persistence_ui::describe(&app)["cancelPrompt"], true);
    control(&mut app, "keepEditing");
    frame(&mut app, &ctx);
    assert!(app.camera_raw.is_some());
    assert_eq!(persistence_ui::describe(&app)["cancelPrompt"], false);
    assert_eq!(persistence_ui::describe(&app)["photos"][0]["recipe"]["adjustments"]["exposure"], 0.75);
    assert!(persistence_ui::control(&mut app, &json!({"action":"keepEditing"})).is_err(), "no confirmation is open");
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"cancel":true}})).unwrap().unwrap();
    control(&mut app, "discard");
    closed(&mut app, &ctx);
    assert!(!source.with_extension("xmp").exists());
    assert!(session_dirs(&fixture).is_empty());
}

/// B09/B10/B18/B20: an interrupted session is offered with two independently actionable
/// cards. "Open saved XMP edits" reopens the original from its live sidecar (not the
/// checkpoint) and leaves the recovered copy on disk; "Discard recovered copy" removes it
/// only after confirmation. No sidecar or photograph changes throughout.
#[test]
fn bridge_chooser_offers_two_cards_and_opens_saved_xmp_or_discards_the_copy() {
    let fixture = Fixture::new();
    let source = fixture.source("two-versions.dng");
    let original = std::fs::read(&source).unwrap();
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.5);
    settled(&mut app, &ctx, 1);
    save_edits(&mut app, &ctx);
    let packet = std::fs::read(source.with_extension("xmp")).unwrap();
    exposure(&mut app, &ctx, 1.0);
    let saved = settled(&mut app, &ctx, 1);
    let session = saved["session"].as_str().unwrap().to_owned();
    interrupt(app, &fixture, &session);

    let mut chooser = fixture.app();
    let ctx = context();
    ctx.enable_accesskit();
    let offered = wait_for(&mut chooser, &ctx, "interrupted session offered", |_, state| state["chooser"] == true && state["busy"] == false);
    assert_eq!(offered["discovered"][0]["unsavedPhotos"], 1);
    let inspected = wait_for(&mut chooser, &ctx, "card facts", |_, state| state["inspection"]["photos"].is_array() && state["busy"] == false);
    assert_eq!(inspected["inspection"]["photos"][0]["source"], "Available");
    assert_eq!(inspected["inspection"]["photos"][0]["sidecar"], "AsCheckpoint");
    let labels = labels_at(&mut chooser, &ctx, vec2(1360.0, 900.0));
    for label in ["Resume recovered edits", "Open saved XMP edits", "Open other files", "Cancel opening", "Discard recovered copy…"] {
        let rect = labels.iter().find(|(name, _)| name == label).unwrap_or_else(|| panic!("{label}: {labels:?}")).1;
        assert!(rect.left() >= 0.0 && rect.right() <= 1360.0, "{label} on screen: {rect:?}");
    }
    // Narrow windows stack the cards; both stay on screen and actionable.
    // A modal re-centres from the size it had last frame: measure the settled narrow layout.
    labels_at(&mut chooser, &ctx, vec2(560.0, 640.0));
    let narrow = labels_at(&mut chooser, &ctx, vec2(560.0, 640.0));
    for label in ["Resume recovered edits", "Open saved XMP edits"] {
        let rect = narrow.iter().find(|(name, _)| name == label).unwrap_or_else(|| panic!("{label} at 560 px")).1;
        assert!(rect.left() >= 0.0 && rect.right() <= 560.0, "{label} fits a 560 px window: {rect:?}");
    }
    persistence_ui::control(&mut chooser, &json!({"action":"openSavedXmp","sessionId":session})).unwrap();
    let reopened = settled(&mut chooser, &ctx, 1);
    assert_ne!(reopened["session"], session, "a new session from the files, not the checkpoint");
    assert_eq!(reopened["photos"][0]["recipe"]["adjustments"]["exposure"], 0.5, "the saved sidecar version");
    assert_eq!(reopened["photos"][0]["pendingXmp"], false);
    assert!(fixture.cache().join(&session).exists(), "the recovered copy is not destroyed by choosing the XMP version");
    control(&mut chooser, "discard");
    closed(&mut chooser, &ctx);
    drop(chooser);

    let mut again = fixture.app();
    let ctx = context();
    wait_for(&mut again, &ctx, "still offered until discarded", |_, state| state["chooser"] == true && state["busy"] == false);
    assert!(persistence_ui::control(&mut again, &json!({"action":"discardRecovered","sessionId":session})).is_err(), "needs confirmation");
    persistence_ui::control(&mut again, &json!({"action":"discardRecovered","sessionId":session,"confirm":true})).unwrap();
    wait_for(&mut again, &ctx, "recovered copy discarded", |_, state| state["busy"] == false && state["chooser"] == false);
    assert!(!fixture.cache().join(&session).exists());
    assert_eq!(std::fs::read(source.with_extension("xmp")).unwrap(), packet, "no sidecar changed");
    assert_eq!(std::fs::read(&source).unwrap(), original, "no photograph changed");
}

/// The recovery details are a modal above the Camera Raw dialog, and Escape closes them
/// without cancelling the review underneath (Candidate E native follow-up).
#[test]
fn bridge_recovery_details_are_on_top_and_escape_only_closes_them() {
    let fixture = Fixture::new();
    let source = fixture.source("details.dng");
    let mut app = fixture.app();
    let ctx = context();
    ctx.enable_accesskit();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    persistence_ui::show_details(&mut app);
    let labels = labels_at(&mut app, &ctx, vec2(1360.0, 900.0));
    let close = labels.iter().find(|(name, _)| name == "Close details").expect("details are drawn").1;
    let layer = ctx.layer_id_at(close.center()).expect("a layer under Close details");
    assert_eq!(layer.order, egui::Order::Foreground);
    assert!(persistence_ui::blocked(&app), "the workspace is blocked while details are open");
    let input = egui::RawInput {
        screen_rect: Some(ERect::from_min_size(Pos2::ZERO, vec2(1360.0, 900.0))),
        events: vec![egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }],
        ..Default::default()
    };
    ctx.run_ui(input, |ui| show(&mut app, ui.ctx())).textures_delta.clear();
    frame(&mut app, &ctx);
    assert!(app.camera_raw.is_some(), "Escape closed only the details");
    assert!(!persistence_ui::blocked(&app));
    assert_eq!(persistence_ui::describe(&app)["cancelPrompt"], false);
}

/// B13/B14/B15 (Candidate F): with JPEG handling "Open all supported files" one File > Open
/// batch of RAW and JPEG photographs (JPEGs without any XMP; .jpg, .JPG and .jpeg) forms ONE
/// Camera Raw session with every request accounted. Done saves each photo to its own
/// adjacent XMP and the originals stay byte-identical. With "Open files with Camera Raw
/// settings" the same JPEGs stay in the editor and are accounted as opened outside it.
#[test]
fn bridge_mixed_raw_and_jpeg_batch_follows_jpeg_handling() {
    for mode in ["all", "existingSettings"] {
        let fixture = Fixture::new();
        let mut paths: Vec<PathBuf> = (0..4).map(|i| fixture.source(&format!("raw_{i}.dng"))).collect();
        let jpeg = rendered_source(&fixture, photocraft_codecs::Format::Jpeg, false);
        let bytes = std::fs::read(&jpeg).unwrap();
        std::fs::remove_file(&jpeg).unwrap();
        for name in ["camera_1.jpg", "camera_2.JPG", "camera_3.jpeg"] {
            let path = fixture.root.join("inputs").join(name);
            std::fs::write(&path, &bytes).unwrap();
            paths.push(path);
        }
        let originals: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        let mut app = fixture.app();
        app.run("prefs.set", json!({"path":"rawDefaults.jpegHandling","value":mode})).unwrap();
        app.background_jobs = true;
        let ctx = context();
        let text: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(app.open_paths(&text), 7);
        let expected = if mode == "all" { 7 } else { 4 };
        let deadline = Instant::now() + Duration::from_secs(60);
        let state = loop {
            crate::jobs_ui::tick(&mut app, &ctx);
            frame(&mut app, &ctx);
            let state = persistence_ui::describe(&app);
            let accounted = state["requests"].as_array().is_some_and(|rows| rows.len() == 7 && rows.iter().all(|row| row["outcome"] != "Pending"));
            if accounted
                && state["photos"].as_array().is_some_and(|photos| photos.len() == expected)
                && state["busy"] == false
                && !state["acknowledged"].is_null()
                && state["acknowledged"] == state["revision"]
                && !app.session.has_jobs()
            {
                break state;
            }
            assert!(Instant::now() < deadline, "{mode}: the batch never settled: {state}");
            std::thread::sleep(Duration::from_millis(5));
        };
        let requests = state["requests"].as_array().unwrap();
        assert_eq!(requests.iter().filter(|row| row["outcome"] == "OutsideRaw").count(), 7 - expected, "{mode}: {requests:?}");
        assert_eq!(requests.iter().filter(|row| row["outcome"] == "Ready").count(), expected, "{mode}: {requests:?}");
        assert_eq!(filmstrip_raws(&app).len(), expected);
        if mode != "all" {
            assert_eq!(app.session.documents().len(), 7, "the JPEGs are ordinary editor tabs");
            continue;
        }
        let ids: Vec<_> = (0..expected).map(|index| runtime(&state, index)).collect();
        persistence_ui::set_metadata(&mut app, &ids, |metadata| metadata.rating = Some(4)).unwrap();
        settled(&mut app, &ctx, expected);
        control(&mut app, "done");
        let plan = wait_for(&mut app, &ctx, "mixed Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
        assert_eq!(plan["plan"]["photos"].as_array().unwrap().len(), 7);
        persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
        closed(&mut app, &ctx);
        drop(app);
        for (path, original) in paths.iter().zip(&originals) {
            assert_eq!(&std::fs::read(path).unwrap(), original, "{} is unchanged", path.display());
            let sidecar = photocraft_io::raw_session::commit::sidecar_target(path).unwrap();
            let jpeg = path.extension().is_some_and(|e| ["jpg", "jpeg"].iter().any(|j| e.eq_ignore_ascii_case(j)));
            if jpeg {
                assert!(
                    sidecar.to_string_lossy().to_ascii_lowercase().ends_with(".jpg.xmp")
                        || sidecar.to_string_lossy().to_ascii_lowercase().ends_with(".jpeg.xmp"),
                    "{}",
                    sidecar.display()
                );
            }
            let packet = std::fs::read(&sidecar).unwrap();
            let recipe = photocraft_io::raw_session::xmp::load_native(&packet).unwrap().unwrap();
            assert_eq!(recipe.metadata.rating, Some(4), "{}", path.display());
        }
        assert!(session_dirs(&fixture).is_empty());
    }
}

/// Candidate F, real files (B01/B03/B13/B15): a disposable copy of the native 22-photo
/// batch (16 NEF, 3 ARW with sidecars, 3 JPEG without), renamed to neutral role aliases, in `PHOTOCRAFT_F_MIXED_INPUTS`
/// opens through File > Open with JPEG handling "Open all supported files" as ONE session
/// of 22. Three photos change (a hidden NEF, the Sony ARW with an existing XMP, a JPEG's
/// rating and label); Done saves exactly those three sidecars, every photograph stays
/// byte-identical, untouched sidecars stay byte-identical, nothing is left to recover,
/// and a fresh session reconstructs the saved recipes from the sidecars alone.
/// It writes XMP into that disposable directory only.
#[test]
#[ignore = "needs PHOTOCRAFT_F_MIXED_INPUTS: a disposable copy of the 22-photo native batch"]
fn bridge_real_mixed_twenty_two_photo_batch_done_saves_only_changed_sidecars() {
    let inputs = PathBuf::from(std::env::var_os("PHOTOCRAFT_F_MIXED_INPUTS").expect("disposable 22-photo copy"));
    let mut photos: Vec<PathBuf> = std::fs::read_dir(&inputs)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()).is_some_and(|e| !e.eq_ignore_ascii_case("xmp")))
        .collect();
    photos.sort();
    assert_eq!(photos.len(), 22, "{photos:?}");
    let before: std::collections::BTreeMap<PathBuf, Vec<u8>> =
        std::fs::read_dir(&inputs).unwrap().flatten().map(|entry| (entry.path(), std::fs::read(entry.path()).unwrap())).collect();
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.run("prefs.set", json!({"path":"rawDefaults.jpegHandling","value":"all"})).unwrap();
    app.background_jobs = true;
    let ctx = context();
    let text: Vec<String> = photos.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    assert_eq!(app.open_paths(&text), 22);
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        crate::jobs_ui::tick(&mut app, &ctx);
        frame(&mut app, &ctx);
        let state = persistence_ui::describe(&app);
        assert!(state.get("error").is_none_or(Value::is_null), "real mixed admission: {state}");
        let accounted = state["requests"].as_array().is_some_and(|rows| rows.len() == 22 && rows.iter().all(|row| row["outcome"] == "Ready"));
        if accounted
            && state["photos"].as_array().is_some_and(|p| p.len() == 22)
            && state["busy"] == false
            && !state["acknowledged"].is_null()
            && state["acknowledged"] == state["revision"]
            && !app.session.has_jobs()
        {
            break;
        }
        assert!(Instant::now() < deadline, "22-photo admission timed out: {}", state["requests"]);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(filmstrip_raws(&app).len(), 22, "all 22 in Camera Raw, none as an editor tab");
    let by_name = |app: &PhotocraftApp, name: &str| filmstrip_raws(app).into_iter().find(|raw| raw.name.eq_ignore_ascii_case(name)).unwrap();
    // The disposable copy uses neutral role aliases (any private mapping is kept outside this
    // directory): `sample_nikon_01.NEF` is a NEF without a sidecar, and `sample_sony_01.ARW` is the
    // Sony ARW whose existing sidecar is `sample_sony_01.xmp`.
    let nef = by_name(&app, "sample_nikon_01.NEF");
    select_raw_review(&mut app, &ctx, nef.document).unwrap();
    app.raw_filmstrip.selection.select_only(nef.photo);
    exposure(&mut app, &ctx, 0.35);
    app.raw_filmstrip.manually_hidden.insert(nef.photo);
    let sony = by_name(&app, "sample_sony_01.ARW");
    select_raw_review(&mut app, &ctx, sony.document).unwrap();
    app.raw_filmstrip.selection.select_only(sony.photo);
    exposure(&mut app, &ctx, 0.07407427);
    let jpeg = filmstrip_raws(&app).into_iter().find(|raw| raw.name.to_ascii_lowercase().ends_with(".jpg")).unwrap();
    persistence_ui::set_metadata(&mut app, &[jpeg.photo], |metadata| {
        metadata.rating = Some(3);
        metadata.label = "Green".into();
    })
    .unwrap();
    settled(&mut app, &ctx, 22);
    control(&mut app, "done");
    let plan = wait_for(&mut app, &ctx, "real mixed Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    let planned: Vec<_> = plan["plan"]["photos"].as_array().unwrap().iter().map(|p| p["binding"]["target"].as_str().unwrap().to_owned()).collect();
    assert_eq!(planned.len(), 3, "only the changed photos, the hidden one included: {planned:?}");
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    closed(&mut app, &ctx);
    let receipt = app.raw_filmstrip.last_persistence_result.clone().unwrap();
    drop(app);
    let changed: Vec<PathBuf> =
        [&nef.name, &sony.name, &jpeg.name].iter().map(|name| photocraft_io::raw_session::commit::sidecar_target(&inputs.join(name)).unwrap()).collect();
    for (path, bytes) in &before {
        let now = std::fs::read(path).unwrap();
        if changed.iter().any(|c| c.file_name().map(|n| n.to_ascii_lowercase()) == path.file_name().map(|n| n.to_ascii_lowercase())) {
            assert_ne!(&now, bytes, "{} was saved", path.display());
        } else {
            assert_eq!(&now, bytes, "{} is unchanged", path.display());
        }
    }
    assert!(session_dirs(&fixture).is_empty(), "nothing left to recover");
    let mut fresh = fixture.app_at_cache(fixture.root.join("fresh-reopen-cache"));
    let ctx = context();
    for name in [&nef.name, &sony.name, &jpeg.name] {
        open(&mut fresh, &inputs.join(name));
    }
    settled(&mut fresh, &ctx, 3);
    let recipe = |app: &PhotocraftApp, name: &str| persistence_ui::recipe(app, by_name(app, name).photo).unwrap().clone();
    assert_eq!(recipe(&fresh, &nef.name).adjustments.exposure, 0.35);
    assert_eq!(recipe(&fresh, &sony.name).adjustments.exposure, 0.07407427);
    assert_eq!(recipe(&fresh, &jpeg.name).metadata.rating, Some(3));
    assert_eq!(recipe(&fresh, &jpeg.name).metadata.label, "Green");
    println!("REAL-MIXED-22 {}", json!({"status":"PASS","admitted":22,"saved":planned.len(),"receipt":receipt}));
}

/// B04 (Candidate F): with two photos selected and one edited photo hidden by the view,
/// Open Images saves every changed photo's XMP first and then opens EVERY retained photo
/// once, in admission order, whatever the selection or filter.
#[test]
fn bridge_open_images_saves_all_changed_then_opens_the_whole_batch_in_order() {
    let fixture = Fixture::new();
    let names = ["batch_1.dng", "batch_2.dng", "batch_3.dng"];
    let paths: Vec<_> = names.iter().map(|name| fixture.source(name)).collect();
    let mut app = fixture.app();
    let ctx = context();
    for path in &paths {
        open(&mut app, path);
    }
    let state = settled(&mut app, &ctx, 3);
    let ids: Vec<_> = (0..3).map(|index| runtime(&state, index)).collect();
    persistence_ui::set_metadata(&mut app, &ids, |metadata| metadata.rating = Some(2)).unwrap();
    app.raw_filmstrip.selection.select_only(ids[0]);
    app.raw_filmstrip.selection.toggle(ids[1]);
    app.raw_filmstrip.manually_hidden.insert(ids[2]);
    settled(&mut app, &ctx, 3);
    commit(&mut app).unwrap();
    let plan = wait_for(&mut app, &ctx, "Open Images plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    assert_eq!(plan["plan"]["photos"].as_array().unwrap().len(), 3, "the hidden, unselected photo is saved too");
    assert!(app.session.documents().iter().all(|st| app.session.provisional_documents().contains(&st.doc.id)), "nothing opened before the save");
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    closed(&mut app, &ctx);
    let opened: Vec<_> = app.session.documents().iter().map(|st| st.doc.name.clone()).collect();
    assert_eq!(opened, names, "every retained photo once, in admission order");
    assert!(app.session.provisional_documents().is_empty());
    for path in &paths {
        let recipe = photocraft_io::raw_session::xmp::load_native(&std::fs::read(path.with_extension("xmp")).unwrap()).unwrap().unwrap();
        assert_eq!(recipe.metadata.rating, Some(2), "{}", path.display());
    }
    assert!(session_dirs(&fixture).is_empty());
}

/// Review R-H5/R-H6 (B05): Open Images whose save is refused for one photo hands nothing to
/// the editor, leaves no exit in progress and keeps every photo in review; the successful
/// sidecar stays saved, the refused one is never written, and the photographer can still
/// Cancel (asked, because one photo is unsaved) without touching either sidecar again.
#[test]
fn bridge_open_images_publication_failure_hands_off_nothing_and_keeps_the_review() {
    let fixture = Fixture::new();
    let first = fixture.source("open_ok.dng");
    let second = fixture.source("open_denied.dng");
    let mut app = fixture.app();
    let denied = second.with_extension("xmp");
    app.services.camera_raw_sidecar_publisher = Some(std::sync::Arc::new(move |path, bytes| {
        if path.file_name() == denied.file_name() {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected denial"));
        }
        photocraft_format::atomic::atomic_write_outcome(path, bytes)
    }));
    let ctx = context();
    open(&mut app, &first);
    open(&mut app, &second);
    let state = settled(&mut app, &ctx, 2);
    let ids: Vec<_> = (0..2).map(|index| runtime(&state, index)).collect();
    persistence_ui::set_metadata(&mut app, &ids, |metadata| metadata.rating = Some(3)).unwrap();
    settled(&mut app, &ctx, 2);
    commit(&mut app).unwrap();
    let plan = wait_for(&mut app, &ctx, "Open Images plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    let failed = wait_for(&mut app, &ctx, "partial Open publication", |_, state| state["busy"] == false && state["error"].is_string());
    assert_eq!(failed["handoff"], false);
    assert_eq!(failed["openSaved"], false);
    assert!(failed["finishing"].is_null(), "no exit is still in progress: {failed}");
    assert!(failed["transition"].is_null(), "nothing was handed off, so nothing waits to be recorded");
    assert_eq!(failed["frozen"], false, "the review is editable again");
    assert!(app.camera_raw.is_some());
    assert_eq!(app.session.provisional_documents().len(), 2, "no photo reached the editor");
    let pending: Vec<_> = failed["photos"].as_array().unwrap().iter().map(|photo| photo["pendingXmp"].clone()).collect();
    assert_eq!(pending, [json!(false), json!(true)]);
    let saved = std::fs::read(first.with_extension("xmp")).unwrap();
    assert_eq!(photocraft_io::raw_session::xmp::load_native(&saved).unwrap().unwrap().metadata.rating, Some(3));
    assert!(!second.with_extension("xmp").exists());
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"cancel":true}})).unwrap().unwrap();
    assert_eq!(reply, json!({"cancelled": false, "confirmationRequired": true}));
    control(&mut app, "discard");
    closed(&mut app, &ctx);
    assert_eq!(std::fs::read(first.with_extension("xmp")).unwrap(), saved, "the saved sidecar is not rewritten");
    assert!(!second.with_extension("xmp").exists());
    assert_eq!(std::fs::read(&first).unwrap(), fixture.bytes);
    assert_eq!(std::fs::read(&second).unwrap(), fixture.bytes);
    assert!(session_dirs(&fixture).is_empty());
}

/// A control action expected to be refused: its message, or "" when it was accepted.
fn control_err(app: &mut PhotocraftApp, action: &str) -> String {
    persistence_ui::control(app, &json!({"action":action})).err().unwrap_or_default()
}

/// Review R-H4: while Open Images hands the saved batch to the editor, no second exit can
/// start; Done, Save edits and Cancel's discard are refused until the handoff finishes.
#[test]
fn bridge_no_second_exit_starts_during_the_open_images_handoff() {
    let fixture = Fixture::new();
    let paths: Vec<_> = (0..3).map(|i| fixture.source(&format!("handoff_{i}.dng"))).collect();
    let mut app = fixture.app();
    app.background_jobs = true;
    let ctx = context();
    for path in &paths {
        open(&mut app, path);
    }
    let ready = |app: &mut PhotocraftApp, ctx: &egui::Context| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            crate::jobs_ui::tick(app, ctx);
            frame(app, ctx);
            let state = persistence_ui::describe(app);
            if state["photos"].as_array().is_some_and(|p| p.len() == 3)
                && state["busy"] == false
                && state["acknowledged"] == state["revision"]
                && !app.session.has_jobs()
            {
                return state;
            }
            assert!(Instant::now() < deadline, "the batch never settled: {state}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let state = ready(&mut app, &ctx);
    let ids: Vec<_> = (0..3).map(|index| runtime(&state, index)).collect();
    persistence_ui::set_metadata(&mut app, &ids, |metadata| metadata.rating = Some(1)).unwrap();
    ready(&mut app, &ctx);
    commit(&mut app).unwrap();
    let mut refused = Vec::new();
    let mut saved = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        crate::jobs_ui::tick(&mut app, &ctx);
        frame(&mut app, &ctx);
        let state = persistence_ui::describe(&app);
        if state["available"] == false && app.camera_raw.is_none() {
            break;
        }
        if !saved && state["busy"] == false && !state["plan"].is_null() {
            persistence_ui::control(&mut app, &json!({"action":"save","planId":state["plan"]["id"]})).unwrap();
            saved = true;
        } else if state["handoff"] == true {
            for action in ["done", "saveEdits", "discard"] {
                let error = control_err(&mut app, action);
                assert!(!error.is_empty(), "{action} was accepted during the handoff: {state}");
                refused.push(action);
            }
        }
        assert!(Instant::now() < deadline, "Open Images never finished: {state}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!refused.is_empty(), "the handoff was observed and every second exit refused");
    assert_eq!(app.session.documents().len(), 3);
    assert!(app.session.provisional_documents().is_empty());
    for path in &paths {
        let recipe = photocraft_io::raw_session::xmp::load_native(&std::fs::read(path.with_extension("xmp")).unwrap()).unwrap().unwrap();
        assert_eq!(recipe.metadata.rating, Some(1), "{}", path.display());
        assert_eq!(std::fs::read(path).unwrap(), fixture.bytes);
    }
    assert!(session_dirs(&fixture).is_empty());
}

/// F priority 6 (review R-E1): with JPEG or TIFF handling Disabled, an ordinary open of a
/// rendered photo whose own sidecar cannot be used (over the binding budget) goes straight
/// to the editor without reading it; an explicit Camera Raw open still refuses that sidecar
/// before admitting it. A leftover `<stem>.xmp` is never read for it.
#[test]
fn bridge_disabled_rendered_handling_ignores_ambiguous_sidecars_until_opened_explicitly() {
    for format in [photocraft_codecs::Format::Jpeg, photocraft_codecs::Format::Tiff] {
        let fixture = Fixture::new();
        let source = rendered_source(&fixture, format, false);
        let small = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about=''/></rdf:RDF></x:xmpmeta>".to_vec();
        let mut packet = small.clone();
        packet.extend(std::iter::repeat_n(b' ', 1_100_000));
        let stem = source.with_extension("xmp");
        let full = source.with_file_name(format!("{}.xmp", source.file_name().unwrap().to_string_lossy()));
        std::fs::write(&stem, &small).unwrap();
        std::fs::write(&full, &packet).unwrap();
        let original = std::fs::read(&source).unwrap();
        let mut app = fixture.app();
        app.run("prefs.set", json!({"path":"rawDefaults.jpegHandling","value":"disabled"})).unwrap();
        app.run("prefs.set", json!({"path":"rawDefaults.tiffHandling","value":"disabled"})).unwrap();
        open(&mut app, &source);
        assert_eq!(app.session.documents().len(), 1, "{format:?}: an ordinary editor document");
        assert!(app.session.provisional_documents().is_empty());
        assert!(app.camera_raw.is_none());
        drop(app);
        let mut explicit = fixture.app_at_cache(fixture.root.join("explicit-cache"));
        explicit.run("prefs.set", json!({"path":"rawDefaults.jpegHandling","value":"disabled"})).unwrap();
        explicit.run("prefs.set", json!({"path":"rawDefaults.tiffHandling","value":"disabled"})).unwrap();
        let ctx = context();
        let refused = match explicit.open_path_camera_raw(source.to_str().unwrap()) {
            Err(error) => error,
            Ok(_) => {
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    frame(&mut explicit, &ctx);
                    let state = persistence_ui::describe(&explicit);
                    if explicit.ui.status_error || state["failedAdmissionCount"] == 1 || state["error"].is_string() {
                        break format!("{} {}", explicit.ui.status, state["error"]);
                    }
                    assert!(Instant::now() < deadline, "{format:?}: the explicit open was not refused: {state}");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        assert!(refused.to_lowercase().contains("sidecar"), "{format:?}: {refused}");
        assert!(explicit.session.provisional_documents().is_empty(), "{format:?}: nothing admitted");
        drop(explicit);
        assert_eq!(std::fs::read(&stem).unwrap(), small);
        assert_eq!(std::fs::read(&full).unwrap(), packet);
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
}

/// Review blockers R-H1/R-H2: recovered work is never discarded without asking. A resumed
/// session whose offline photo still holds unsaved recovered edits asks before the
/// loading dialog's Cancel, the window's close and Camera Raw's Cancel; "Back to editing"
/// keeps it, and interrupted again it is still offered with that unsaved photo.
#[test]
fn bridge_resumed_unsaved_work_is_never_discarded_without_asking() {
    let fixture = Fixture::new();
    let first = fixture.source("kept.dng");
    let second = fixture.source("offline.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &first);
    open(&mut app, &second);
    let state = settled(&mut app, &ctx, 2);
    let offline = runtime(&state, 1);
    persistence_ui::set_metadata(&mut app, &[offline], |metadata| metadata.rating = Some(5)).unwrap();
    let saved = settled(&mut app, &ctx, 2);
    let session = saved["session"].as_str().unwrap().to_owned();
    interrupt(app, &fixture, &session);
    let moved = fixture.root.join("offline-elsewhere.dng");
    std::fs::rename(&second, &moved).unwrap();

    let mut app = fixture.app();
    wait_for(&mut app, &ctx, "offered", |_, state| state["chooser"] == true && state["busy"] == false);
    persistence_ui::control(&mut app, &json!({"action":"resume","sessionId":session})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while persistence_ui::describe(&app)["chooser"] == true {
        frame(&mut app, &ctx);
        assert!(Instant::now() < deadline, "the resume never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The loading dialog's Cancel, while (or just after) the draft is restored.
    persistence_ui::cancel_loading(&mut app);
    assert_eq!(persistence_ui::describe(&app)["cancelPrompt"], true, "the loading Cancel asks first");
    control(&mut app, "keepEditing");
    let resumed = wait_for(&mut app, &ctx, "resumed without the offline photo", |_, state| {
        state["photos"].as_array().is_some_and(|p| p.len() == 1) && state["failedAdmissionCount"] == 1 && state["busy"] == false
    });
    assert_eq!(resumed["photos"][0]["pendingXmp"], false, "the photo on screen is clean; the unsaved one is offline");
    assert!(persistence_ui::request_close(&mut app), "the window close is held");
    assert_eq!(persistence_ui::describe(&app)["cancelPrompt"], true, "the window close asks first");
    control(&mut app, "keepEditing");
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"cancel":true}})).unwrap().unwrap();
    assert_eq!(reply, json!({"cancelled": false, "confirmationRequired": true}));
    control(&mut app, "keepEditing");
    frame(&mut app, &ctx);
    assert!(app.camera_raw.is_some());
    interrupt(app, &fixture, &session);

    let mut again = fixture.app();
    let offered = wait_for(&mut again, &ctx, "still offered", |_, state| state["chooser"] == true && state["busy"] == false);
    assert_eq!(offered["discovered"][0]["id"], session);
    assert_eq!(offered["discovered"][0]["unsavedPhotos"], 1, "the offline photo's edits survived");
    drop(again);
    assert!(!first.with_extension("xmp").exists() && !moved.with_extension("xmp").exists() && !second.with_extension("xmp").exists());
    assert_eq!(std::fs::read(&first).unwrap(), fixture.bytes);
    assert_eq!(std::fs::read(&moved).unwrap(), fixture.bytes);
}

/// Second review (test gap for R-H4): while a reviewed Done plan waits for consent, a
/// second exit is refused instead of replacing the exit in progress. Done stays the exit,
/// its plan is unchanged, and Back returns to editing with no exit pending.
#[test]
fn bridge_a_second_exit_never_replaces_a_plan_awaiting_consent() {
    let fixture = Fixture::new();
    let source = fixture.source("one-exit.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.6);
    settled(&mut app, &ctx, 1);
    control(&mut app, "done");
    let plan = wait_for(&mut app, &ctx, "Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    assert_eq!(plan["finishing"], "Done");
    for action in ["saveEdits", "done"] {
        let error = control_err(&mut app, action);
        assert!(!error.is_empty(), "{action} was accepted while the Done plan awaits consent");
        assert!(!error.contains("Unknown"), "{action}: {error}");
    }
    assert!(commit(&mut app).is_err(), "Open Images is refused too");
    frame(&mut app, &ctx);
    let state = persistence_ui::describe(&app);
    assert_eq!(state["finishing"], "Done", "the exit in progress is unchanged");
    assert_eq!(state["plan"]["id"], plan["plan"]["id"], "the reviewed plan is unchanged");
    control(&mut app, "back");
    let back = wait_for(&mut app, &ctx, "back to editing", |_, state| state["busy"] == false && state["plan"].is_null() && state["frozen"] == false);
    assert!(back["finishing"].is_null());
    assert!(!source.with_extension("xmp").exists());
}

/// Frames until `ready`, tolerating errors (unlike `wait_for`): for failure-path tests.
fn frames_until(app: &mut PhotocraftApp, context: &egui::Context, description: &str, ready: impl Fn(&PhotocraftApp, &Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        frame(app, context);
        let state = persistence_ui::describe(app);
        if ready(app, &state) {
            return state;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for {description}: {state}; status={}", app.ui.status);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Sweep S3 (pre-existing since E): an XMP save whose outcome could not be confirmed left
/// no exit but a force-kill. Done and Open Images still refuse until it is reconciled, but
/// Cancel now closes Camera Raw and KEEPS the session (nothing discarded or deleted); the
/// next start offers it, where resuming reconciles the actual sidecar bytes.
#[test]
fn bridge_an_unconfirmed_xmp_save_keeps_the_session_when_cancelled() {
    let fixture = Fixture::new();
    let source = fixture.source("unconfirmed.dng");
    let mut app = fixture.app();
    app.services.camera_raw_sidecar_publisher = Some(std::sync::Arc::new(|path, bytes| {
        photocraft_format::atomic::atomic_write_outcome(path, bytes)?;
        Ok(photocraft_format::atomic::PublicationOutcome::PublishedUnconfirmed(std::io::Error::other("injected: sync not confirmed")))
    }));
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.8);
    let state = settled(&mut app, &ctx, 1);
    let session = state["session"].as_str().unwrap().to_owned();
    control(&mut app, "done");
    let plan = wait_for(&mut app, &ctx, "Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    frames_until(&mut app, &ctx, "unconfirmed save reported", |_, state| state["busy"] == false && state["error"].is_string());
    assert!(app.camera_raw.is_some(), "Done did not close over an unconfirmed save");
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"cancel":true}})).unwrap().unwrap();
    assert_eq!(reply, json!({"cancelled": false, "confirmationRequired": true}));
    control(&mut app, "discard");
    frames_until(&mut app, &ctx, "closed and kept", |app, state| state["available"] == false && app.camera_raw.is_none());
    assert!(app.ui.status.contains("could not be confirmed"), "{}", app.ui.status);
    assert!(fixture.cache().join(&session).join("authority.json").exists(), "kept for reconciliation");
    drop(app);
    let mut again = fixture.app();
    let offered = frames_until(&mut again, &ctx, "offered next start", |_, state| state["chooser"] == true && state["busy"] == false);
    assert!(offered["discovered"].as_array().unwrap().iter().any(|row| row["id"] == session.as_str()));
    assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
}

/// Sweep S6 (pre-existing since E): quitting PhotoCraft asks; if the confirmed discard is
/// then refused, the app exit it carried is given up, so a later Done or Open Images does
/// not unexpectedly quit the application.
#[cfg(windows)]
#[test]
fn bridge_a_refused_discard_from_app_close_gives_up_the_app_exit() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let source = fixture.source("refused-exit.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.9);
    let state = settled(&mut app, &ctx, 1);
    let authority = fixture.cache().join(state["session"].as_str().unwrap()).join("authority.json");
    let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&authority).unwrap();
    assert!(persistence_ui::request_exit(&mut app));
    assert_eq!(persistence_ui::describe(&app)["exitRequested"], true);
    control(&mut app, "discard");
    let refused = frames_until(&mut app, &ctx, "refused discard", |_, state| state["busy"] == false && state["error"].is_string());
    assert_eq!(refused["exitRequested"], false, "the exit is given up with the refused discard");
    assert!(app.camera_raw.is_some());
    drop(held);
    assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
}

/// Sweep S8 (pre-existing since E): a confirmed Cancel while a resumed session is still
/// being restored keeps the restore queue until the discard is known to succeed. When the
/// discard is refused, every retained photograph is still restored, and Done saves them.
#[cfg(windows)]
#[test]
fn bridge_a_refused_discard_keeps_the_resumed_photos_it_was_restoring() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let first = fixture.source("restore_1.dng");
    let second = fixture.source("restore_2.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &first);
    open(&mut app, &second);
    let state = settled(&mut app, &ctx, 2);
    let ids: Vec<_> = (0..2).map(|index| runtime(&state, index)).collect();
    persistence_ui::set_metadata(&mut app, &ids, |metadata| metadata.rating = Some(2)).unwrap();
    let saved = settled(&mut app, &ctx, 2);
    let session = saved["session"].as_str().unwrap().to_owned();
    interrupt(app, &fixture, &session);

    let mut app = fixture.app();
    frames_until(&mut app, &ctx, "offered", |_, state| state["chooser"] == true && state["busy"] == false);
    persistence_ui::control(&mut app, &json!({"action":"resume","sessionId":session})).unwrap();
    frames_until(&mut app, &ctx, "resumed", |_, state| state["chooser"] == false);
    let authority = fixture.cache().join(&session).join("authority.json");
    let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&authority).unwrap();
    persistence_ui::cancel_loading(&mut app);
    assert_eq!(persistence_ui::describe(&app)["cancelPrompt"], true, "recovered work: Cancel asks");
    let before = persistence_ui::queued_admissions(&app);
    control(&mut app, "discard");
    let after = persistence_ui::queued_admissions(&app);
    assert_eq!(after.1, before.1, "the restore queue is kept until the discard succeeds");
    frames_until(&mut app, &ctx, "refused discard", |_, state| state["busy"] == false && state["error"].is_string());
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        frame(&mut app, &ctx);
        let state = persistence_ui::describe(&app);
        if state["photos"].as_array().is_some_and(|photos| photos.len() == 2) && state["busy"] == false && state["failedAdmissionCount"] == 0 {
            break;
        }
        if state["busy"] == false && state["failedAdmissionCount"].as_u64().is_some_and(|n| n > 0) {
            control(&mut app, "retryCache");
        }
        assert!(Instant::now() < deadline, "the resumed photos were never all restored: {state}");
        std::thread::sleep(Duration::from_millis(5));
    }
    control(&mut app, "done");
    let plan = frames_until(&mut app, &ctx, "Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    closed(&mut app, &ctx);
    for path in [&first, &second] {
        let recipe = photocraft_io::raw_session::xmp::load_native(&std::fs::read(path.with_extension("xmp")).unwrap()).unwrap().unwrap();
        assert_eq!(recipe.metadata.rating, Some(2), "{}", path.display());
    }
}

/// Sweep S9 (pre-existing since E): when the recovery folder cannot be created, the first
/// admission's failure is kept for "Retry recovery save" (not lost), turning recovery off
/// still works because the session is then created in memory, and Cancel stays an exit.
#[test]
fn bridge_an_unwritable_recovery_folder_keeps_the_admission_and_allows_turning_recovery_off() {
    let fixture = Fixture::new();
    let blocked = fixture.root.join("not-a-folder");
    std::fs::write(&blocked, b"a file where the recovery folder should be").unwrap();
    let source = fixture.source("unwritable-cache.dng");
    let mut app = fixture.app_at_cache(blocked);
    let ctx = context();
    open(&mut app, &source);
    let failed = frames_until(&mut app, &ctx, "admission kept for retry", |_, state| state["busy"] == false && state["failedAdmissionCount"] == 1);
    assert!(failed["photos"].as_array().is_none_or(Vec::is_empty));
    app.run("prefs.set", json!({"path":"rawDefaults.recoveryMode","value":"off"})).unwrap();
    frames_until(&mut app, &ctx, "recovery turned off", |_, state| state["recoveryEnabled"] == false && state["busy"] == false);
    control(&mut app, "retryCache");
    let admitted = frames_until(&mut app, &ctx, "admitted in memory", |_, state| {
        state["photos"].as_array().is_some_and(|photos| photos.len() == 1) && state["busy"] == false && state["failedAdmissionCount"] == 0
    });
    assert_eq!(admitted["photos"][0]["pendingXmp"], false);
    exposure(&mut app, &ctx, 0.3);
    frames_until(&mut app, &ctx, "edited", |_, state| state["busy"] == false && state["photos"][0]["pendingXmp"] == true);
    // Saving still journals each XMP publication privately (even with recovery off), so
    // with no writable recovery folder Done refuses before writing anything (limitation).
    control(&mut app, "done");
    let refused = frames_until(&mut app, &ctx, "Done refused", |_, state| state["busy"] == false && state["error"].is_string());
    assert!(refused["plan"].is_null() && refused["frozen"] == false, "{refused}");
    assert!(!source.with_extension("xmp").exists());
    // Cancel remains an exit and writes nothing.
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"cancel":true}})).unwrap().unwrap();
    assert_eq!(reply, json!({"cancelled": false, "confirmationRequired": true}));
    control(&mut app, "discard");
    closed(&mut app, &ctx);
    assert!(!source.with_extension("xmp").exists());
    assert_eq!(std::fs::read(&source).unwrap(), fixture.bytes);
}

/// Round-4 sweep (introduced by 7f878f1, fixed): a Cancel before any session exists (the
/// recovery folder cannot be created) ends the visit instead of trying to create a store and
/// freezing the host, so the loading dialog, Cancel and app close all keep working.
#[test]
fn bridge_cancel_before_any_session_exists_always_closes() {
    let fixture = Fixture::new();
    let blocked = fixture.root.join("not-a-folder");
    std::fs::write(&blocked, b"a file where the recovery folder should be").unwrap();
    let first = fixture.source("no-session-1.dng");
    let second = fixture.source("no-session-2.dng");
    let mut app = fixture.app_at_cache(blocked.clone());
    let ctx = context();
    open(&mut app, &first);
    open(&mut app, &second);
    frames_until(&mut app, &ctx, "the host is running", |_, state| state["available"] == true);
    persistence_ui::cancel_loading(&mut app);
    if persistence_ui::describe(&app)["cancelPrompt"] == true {
        control(&mut app, "discard");
    }
    frames_until(&mut app, &ctx, "closed without a session", |app, state| state["available"] == false && app.camera_raw.is_none());
    assert!(!persistence_ui::request_exit(&mut app), "nothing holds the application open");
    assert!(blocked.is_file(), "nothing was created under the blocked path");
    for path in [&first, &second] {
        assert!(!path.with_extension("xmp").exists());
        assert_eq!(std::fs::read(path).unwrap(), fixture.bytes);
    }
}

/// Round-4 sweep (pre-existing since E): a photograph that opened but whose admission was
/// refused (here an over-budget sidecar) left a ticket that blocked Done for the whole
/// batch. "Skip this unfinished import" now settles it, and Done saves the others; the
/// skipped photograph and its sidecar are untouched.
#[test]
fn bridge_a_refused_admission_can_be_skipped_so_the_others_save() {
    let fixture = Fixture::new();
    let first = fixture.source("admitted.dng");
    let refused = fixture.source("over-budget.dng");
    let padding = "A".repeat(1_200_000);
    let packet = format!(
        "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:p='urn:pad' p:pad='{padding}'/></rdf:RDF></x:xmpmeta>"
    );
    std::fs::write(refused.with_extension("xmp"), &packet).unwrap();
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &first);
    open(&mut app, &refused);
    let state = frames_until(&mut app, &ctx, "one admitted, one refused", |_, state| {
        state["busy"] == false && state["failedAdmissionCount"] == 1 && state["photos"].as_array().is_some_and(|photos| photos.len() == 1)
    });
    let key = state["requests"].as_array().unwrap().iter().find(|row| row["name"] == "over-budget.dng").unwrap()["key"].as_str().unwrap().to_owned();
    exposure(&mut app, &ctx, 0.45);
    frames_until(&mut app, &ctx, "edited", |_, state| state["busy"] == false && state["photos"][0]["pendingXmp"] == true);
    control(&mut app, "done");
    frames_until(&mut app, &ctx, "Done refused by the unresolved ticket", |_, state| {
        state["busy"] == false && state["error"].is_string() && state["plan"].is_null()
    });
    crate::camera_raw_admission::skip_unadmitted(&mut app, &key).unwrap();
    frames_until(&mut app, &ctx, "ticket settled", |_, state| {
        state["busy"] == false && state["failedAdmissionCount"] == 0 && state["nativeRequests"][key.as_str()] == "Cancelled"
    });
    control(&mut app, "done");
    let plan = frames_until(&mut app, &ctx, "Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    assert_eq!(plan["plan"]["photos"].as_array().unwrap().len(), 1);
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    closed(&mut app, &ctx);
    let saved = photocraft_io::raw_session::xmp::load_native(&std::fs::read(first.with_extension("xmp")).unwrap()).unwrap().unwrap();
    assert!((saved.adjustments.exposure - 0.45).abs() < 1e-6);
    assert_eq!(std::fs::read(refused.with_extension("xmp")).unwrap(), packet.as_bytes(), "the skipped sidecar is untouched");
    assert_eq!(std::fs::read(&refused).unwrap(), fixture.bytes);
    assert!(session_dirs(&fixture).is_empty());
}

/// Round-4 sweep (pre-existing since E): after a resume, a photograph whose sidecar was
/// changed elsewhere blocked Done for every photograph. "Discard this photo's changes and
/// remove" (recovery details) removes just that member, and Done saves the others.
#[test]
fn bridge_a_changed_recovered_photo_can_be_discarded_so_the_others_save() {
    let fixture = Fixture::new();
    let first = fixture.source("kept-member.dng");
    let changed = fixture.source("changed-member.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &first);
    open(&mut app, &changed);
    let state = settled(&mut app, &ctx, 2);
    let ids: Vec<_> = (0..2).map(|index| runtime(&state, index)).collect();
    persistence_ui::set_metadata(&mut app, &ids, |metadata| metadata.rating = Some(4)).unwrap();
    let saved = settled(&mut app, &ctx, 2);
    let session = saved["session"].as_str().unwrap().to_owned();
    let changed_id = saved["photos"].as_array().unwrap().iter().find(|photo| photo["photo"] == ids[1].0).unwrap()["id"].as_str().unwrap().to_owned();
    interrupt(app, &fixture, &session);
    let external = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about=''/></rdf:RDF></x:xmpmeta>";
    std::fs::write(changed.with_extension("xmp"), external).unwrap();

    let mut app = fixture.app();
    frames_until(&mut app, &ctx, "offered", |_, state| state["chooser"] == true && state["busy"] == false);
    persistence_ui::control(&mut app, &json!({"action":"resume","sessionId":session})).unwrap();
    frames_until(&mut app, &ctx, "restored", |_, state| {
        state["chooser"] == false && state["busy"] == false && state["photos"].as_array().is_some_and(|photos| photos.len() == 2)
    });
    control(&mut app, "done");
    frames_until(&mut app, &ctx, "Done refused by the changed member", |_, state| {
        state["busy"] == false && state["error"].is_string() && state["plan"].is_null()
    });
    persistence_ui::discard_recovered_member(&mut app, photocraft_io::raw_session::PhotoId(changed_id)).unwrap();
    frames_until(&mut app, &ctx, "member removed", |_, state| state["busy"] == false && state["photos"].as_array().is_some_and(|photos| photos.len() == 1));
    control(&mut app, "done");
    let plan = frames_until(&mut app, &ctx, "Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
    closed(&mut app, &ctx);
    let kept = photocraft_io::raw_session::xmp::load_native(&std::fs::read(first.with_extension("xmp")).unwrap()).unwrap().unwrap();
    assert_eq!(kept.metadata.rating, Some(4));
    assert_eq!(std::fs::read(changed.with_extension("xmp")).unwrap(), external, "the external change is kept");
    assert_eq!(std::fs::read(&changed).unwrap(), fixture.bytes);
    assert!(session_dirs(&fixture).is_empty());
}

/// Round-4 sweep (pre-existing since E): a "Save XMP and remove" whose commit is refused
/// before publishing (here an existing journal record forces reconciliation) returns to
/// editing, as Done does, instead of leaving a removal plan the worker no longer holds.
#[test]
fn bridge_a_failed_save_and_remove_returns_to_editing() {
    let fixture = Fixture::new();
    let source = fixture.source("remove-refused.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    let ready = settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 1.1);
    let edited = settled(&mut app, &ctx, 1);
    persistence_ui::control(&mut app, &json!({"action":"remove","photo":runtime(&ready, 0).0})).unwrap();
    let plan = wait_for(&mut app, &ctx, "removal plan", |_, state| state["busy"] == false && !state["plan"].is_null());
    let plan_id = plan["plan"]["id"].as_str().unwrap().to_owned();
    let blocker = fixture.cache().join(edited["session"].as_str().unwrap()).join(format!("operation-{plan_id}.json"));
    std::fs::create_dir_all(&blocker).unwrap();
    persistence_ui::control(&mut app, &json!({"action":"save","planId":plan_id})).unwrap();
    let failed = frames_until(&mut app, &ctx, "removal save refused", |_, state| state["busy"] == false && state["error"].is_string());
    assert!(failed["plan"].is_null(), "no stale plan: {failed}");
    assert_eq!(failed["frozen"], false, "back to editing");
    assert_eq!(failed["photos"].as_array().unwrap().len(), 1, "the photograph stays in the session");
    assert!(!source.with_extension("xmp").exists());
    std::fs::remove_dir(&blocker).unwrap();
}

/// Round-5 sweep (control/MCP parity): "discard" with unsaved edits needs the Cancel
/// confirmation (the "cancel" persistence action opens it) or explicit confirm: true, as the
/// GUI asks; and with no Camera Raw review open, a ui request never opens a Filter dialog.
#[test]
fn bridge_control_discard_needs_the_same_consent_as_the_gui() {
    let fixture = Fixture::new();
    let source = fixture.source("consent.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.7);
    settled(&mut app, &ctx, 1);
    let refused = control_err(&mut app, "discard");
    assert!(refused.contains("confirmation"), "{refused}");
    assert!(app.camera_raw.is_some(), "nothing was discarded");
    assert!(control_err(&mut app, "back").contains("reviewed plan"), "no Back without a plan");
    control(&mut app, "cancel");
    assert_eq!(persistence_ui::describe(&app)["cancelPrompt"], true);
    control(&mut app, "discard");
    closed(&mut app, &ctx);
    assert!(!source.with_extension("xmp").exists());
}

/// Round-5 sweep: while the recovery chooser is shown (no Camera Raw review open) a ui
/// request is refused instead of opening a Camera Raw Filter over the session.
#[test]
fn bridge_no_ui_request_opens_a_filter_over_a_raw_session() {
    let fixture = Fixture::new();
    let source = fixture.source("no-filter.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    settled(&mut app, &ctx, 1);
    exposure(&mut app, &ctx, 0.2);
    let saved = settled(&mut app, &ctx, 1);
    interrupt(app, &fixture, saved["session"].as_str().unwrap());
    let mut app = fixture.app();
    frames_until(&mut app, &ctx, "chooser", |_, state| state["chooser"] == true && state["busy"] == false);
    let refused = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"set":{"exposure":1.0}}})).unwrap().unwrap_err();
    assert!(refused.contains("No Camera Raw review is open"), "{refused}");
    assert!(app.camera_raw.is_none(), "no Filter dialog was opened");
}

/// Round-5 sweep: "Discard this photo's changes and remove" is only for a photograph whose
/// original or sidecar cannot be restored; over control it needs confirm: true.
#[test]
fn bridge_discard_member_is_only_for_an_unrestorable_photograph() {
    let fixture = Fixture::new();
    let source = fixture.source("healthy-member.dng");
    let mut app = fixture.app();
    let ctx = context();
    open(&mut app, &source);
    let state = settled(&mut app, &ctx, 1);
    let id = state["photos"][0]["id"].as_str().unwrap().to_owned();
    let refused = persistence_ui::discard_recovered_member(&mut app, photocraft_io::raw_session::PhotoId(id.clone())).unwrap_err();
    assert!(refused.contains("cannot be restored"), "{refused}");
    let unconfirmed = persistence_ui::control(&mut app, &json!({"action":"discardMember","photoId":id})).unwrap_err();
    assert!(unconfirmed.contains("confirm"), "{unconfirmed}");
    assert_eq!(persistence_ui::describe(&app)["photos"].as_array().unwrap().len(), 1);
}

/// Round-6 (regression of the persistence-only gate, fixed): the idle recovery host that
/// startup discovery leaves (nothing offered, no RAW visit) never blocks the ordinary Camera
/// Raw Filter over control/MCP.
#[test]
fn bridge_the_camera_raw_filter_still_works_beside_an_idle_recovery_host() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    let ctx = context();
    app.run("file.new", json!({"width":8,"height":6})).unwrap();
    frames_until(&mut app, &ctx, "idle host", |_, state| state["available"] == true && state["busy"] == false && state["chooser"] == false);
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"set":{"exposure":0.5}}})).unwrap();
    assert!(reply.is_ok(), "{reply:?}");
    assert!(app.camera_raw.is_some(), "the Camera Raw Filter opened");
}

/// Round-7: an editor-only open (here a PNG) beside the idle recovery host creates no Camera
/// Raw session, so the ordinary Camera Raw Filter still works over control/MCP afterwards.
#[test]
fn bridge_an_editor_only_open_beside_the_idle_host_creates_no_session() {
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, encode};
    let fixture = Fixture::new();
    let mut app = fixture.app();
    let ctx = context();
    frames_until(&mut app, &ctx, "idle host", |_, state| state["available"] == true && state["busy"] == false && state["chooser"] == false);
    let png = fixture.root.join("inputs").join("plain.png");
    let image = Image::from_u8(8, 6, ChannelLayout::Rgb, [40, 60, 80].repeat(48)).unwrap();
    std::fs::write(&png, encode(&image, Format::Png, &EncodeOptions::default()).unwrap()).unwrap();
    open(&mut app, &png);
    for _ in 0..20 {
        frame(&mut app, &ctx);
    }
    assert!(persistence_ui::describe(&app)["session"].is_null(), "no Camera Raw session for an editor-only open");
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"set":{"exposure":0.5}}})).unwrap();
    assert!(reply.is_ok(), "{reply:?}");
    assert!(app.camera_raw.is_some());
}

#[path = "camera_raw_soak_tests.rs"]
mod integrated_soak;

/// UI16 extends io/tests/raw.rs's decoder-level preview test through real opening,
/// request accounting and mixed-session Done. An embedded JPEG is not sensor RAW.
#[test]
fn bridge_unsupported_sensor_preview_is_reported_accounted_and_excluded_from_done() {
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
    for explicit in [false, true] {
        let fixture = Fixture::new();
        let image = Image::from_u8(32, 20, ChannelLayout::Rgb, vec![180; 32 * 20 * 3]).unwrap();
        let jpeg = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        let bytes = photocraft_raw::testgen::cr3_preview(&jpeg, 6);
        let preview = fixture.root.join("inputs/preview-only.cr3");
        let sidecar = preview.with_extension("xmp");
        let packet = br#"<r:RDF xmlns:r="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:u="urn:retained"><r:Description u:keep="unchanged"/></r:RDF>"#;
        std::fs::write(&preview, &bytes).unwrap();
        std::fs::write(&sidecar, packet).unwrap();
        let mut app = fixture.app();
        let ctx = context();
        let warnings = app.open_bytes_from_mode("preview-only.cr3", &bytes, Some(preview.to_str().unwrap()), explicit).unwrap();
        assert!(warnings.iter().any(|warning| warning.contains("CR3") && warning.contains("8-bit, not the raw sensor data")), "{warnings:?}");
        assert!(!warnings.iter().any(|warning| warning.contains(photocraft_io::raw::DEVELOPED_NOTE)));
        if explicit {
            assert!(warnings.iter().any(|warning| warning == photocraft_engine::jobs::RAW_PREVIEW_ONLY_NOTE));
        }
        assert!(app.camera_raw.is_none(), "preview fallback must not masquerade as a developed RAW review");
        assert!(filmstrip_raws(&app).is_empty());
        assert_eq!(app.session.documents().len(), 1);
        let preview_id = app.session.documents()[0].doc.id;
        assert_eq!((app.session.documents()[0].doc.size.width, app.session.documents()[0].doc.size.height), (20, 32));
        let requests = crate::camera_raw_admission::snapshot(&app);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].outcome, crate::camera_raw_admission::Outcome::OutsideRaw);
        assert_eq!(requests[0].document, Some(preview_id.0));
        let supported = fixture.source("supported.dng");
        open(&mut app, &supported);
        let admitted = settled(&mut app, &ctx, 1);
        assert_eq!(admitted["requests"].as_array().unwrap().len(), 2);
        assert_eq!(filmstrip_raws(&app).len(), 1);
        exposure(&mut app, &ctx, 0.4);
        control(&mut app, "done");
        let plan = wait_for(&mut app, &ctx, "supported member Done plan", |_, state| state["busy"] == false && !state["plan"].is_null());
        persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
        closed(&mut app, &ctx);
        assert_eq!(app.session.documents().len(), 1, "Done must preserve the preview editor document");
        assert_eq!(app.session.documents()[0].doc.id, preview_id);
        assert!(supported.with_extension("xmp").exists());
        assert_eq!(std::fs::read(&preview).unwrap(), bytes);
        assert_eq!(std::fs::read(&sidecar).unwrap(), packet);
        assert!(!fixture.root.join("inputs/preview-only.cr3.xmp").exists());
        let mut controller = photocraft_io::raw_session::controller::Controller::create(&fixture.root.join("direct-refusal")).unwrap();
        controller.request("unsupported".into()).unwrap();
        assert!(controller.admit("unsupported", &preview, &bytes).is_err());
        assert!(controller.session().photos.is_empty(), "no sensor recipe can be persisted for preview-only data");
        assert_eq!(std::fs::read(&preview).unwrap(), bytes);
        assert_eq!(std::fs::read(&sidecar).unwrap(), packet);
    }
}
