//! Captured-photo JPEG export. The worker owns its recipe and source binding, so
//! navigation cannot change the image being rendered. Export never acknowledges
//! recovery, saves sidecars, changes baselines, or closes Camera Raw.

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use photocraft_io::raw_session::{NativeRecipe, SourceBinding, controller};

use super::{PhotoId, persistence_ui};
use crate::{FileDialogAnswer, FileDialogRequest, PhotocraftApp};

#[derive(Clone)]
struct Capture {
    token: u64,
    photo: PhotoId,
    name: String,
    binding: SourceBinding,
    recipe: NativeRecipe,
    protected: Vec<SourceBinding>,
    recovery_root: Option<PathBuf>,
}

struct Dialog {
    capture: Capture,
    filename: String,
    folder: String,
    quality: u8,
    error: Option<String>,
    overwrite: Option<(PathBuf, controller::DestinationReceipt)>,
}

enum ResultMessage {
    NeedsOverwrite(PathBuf, controller::DestinationReceipt),
    Saved { path: PathBuf, warnings: Vec<String>, confirmed: bool },
    Failed(String),
    Cancelled,
}

struct Worker {
    result: mpsc::Receiver<ResultMessage>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub(super) struct JpegState {
    dialog: Option<Dialog>,
    worker: Option<Worker>,
    /// Last actual export outcome, separate from recovery/XMP status.
    pub message: Option<String>,
}

pub(super) fn is_modal_open(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.jpeg.dialog.is_some() && app.raw_filmstrip.jpeg.worker.is_none()
}

pub(super) fn is_running(app: &PhotocraftApp) -> bool {
    app.raw_filmstrip.jpeg.worker.is_some()
}

/// Supply the export modal's fields in a headless integration harness. Production
/// capture, destination validation, worker and publication remain the normal path.
#[cfg(test)]
pub(super) fn fixture_export_destination(app: &mut PhotocraftApp, ctx: &egui::Context, destination: &std::path::Path) -> Result<NativeRecipe, String> {
    let dialog = app.raw_filmstrip.jpeg.dialog.as_mut().ok_or("Choose the export photograph first")?;
    dialog.filename = destination.file_name().and_then(|s| s.to_str()).ok_or("Invalid fixture filename")?.into();
    dialog.folder = destination.parent().and_then(|s| s.to_str()).ok_or("Invalid fixture folder")?.into();
    let recipe = dialog.capture.recipe.clone();
    start(app, ctx, None)?;
    Ok(recipe)
}

pub(super) fn request(app: &mut PhotocraftApp, photo: PhotoId) -> Result<(), String> {
    if app.raw_filmstrip.jpeg.dialog.is_some() || app.raw_filmstrip.jpeg.worker.is_some() {
        return Err("Finish or cancel the current JPEG export first".into());
    }
    let binding = persistence_ui::binding(app, photo).ok_or("Wait for this photograph's source admission before saving JPEG")?;
    let mut recipe = persistence_ui::recipe(app, photo).cloned().ok_or("This photograph's native recipe is unavailable")?;
    if let Some(dialog) = app.camera_raw.as_ref().filter(|d| d.raw_open.as_ref().is_some_and(|raw| raw.photo == photo)) {
        recipe.adjustments = dialog.params.clone();
        recipe.adjustments.pixel_scale = 1.0;
    } else if let Some(parked) = app.raw_filmstrip.parked.iter().find(|item| item.raw.photo == photo) {
        recipe.adjustments = parked.params.clone();
        recipe.adjustments.pixel_scale = 1.0;
    }
    recipe.validate()?;
    let protected = persistence_ui::bindings(app);
    for raw in super::filmstrip_raws(app) {
        if persistence_ui::binding(app, raw.photo).is_none() {
            return Err("Wait for every session photograph to finish source admission before saving JPEG".into());
        }
    }
    let name = binding.source.file_name().map(|name| name.to_string_lossy().into_owned()).ok_or("The source has no filename")?;
    let stem = binding.source.file_stem().map(|stem| stem.to_string_lossy().into_owned()).ok_or("The source has no filename stem")?;
    let folder = binding.source.parent().map(|parent| parent.to_string_lossy().into_owned()).ok_or("The source has no folder")?;
    static NEXT_EXPORT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let token = NEXT_EXPORT.fetch_add(1, Ordering::Relaxed);
    app.raw_filmstrip.jpeg.dialog = Some(Dialog {
        capture: Capture { token, photo, name, binding, recipe, protected, recovery_root: app.services.camera_raw_recovery_root.clone() },
        filename: format!("{stem}-edited.jpg"),
        folder,
        quality: 90,
        error: None,
        overwrite: None,
    });
    app.raw_filmstrip.jpeg.message = None;
    Ok(())
}

fn output_path(folder: &str, filename: &str) -> Result<PathBuf, String> {
    if filename.is_empty() || filename.trim() != filename || filename.ends_with('.') || filename.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)) {
        return Err("Enter a filename without folders, reserved characters or trailing dots/spaces".into());
    }
    let name = std::path::Path::new(filename);
    let stem = name.file_stem().and_then(|name| name.to_str()).ok_or("Enter a valid JPEG filename")?;
    let reserved = stem.split('.').next().unwrap_or(stem).to_ascii_uppercase();
    if matches!(reserved.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"]
            .iter()
            .any(|prefix| reserved.strip_prefix(prefix).is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes().first(), Some(b'1'..=b'9'))))
    {
        return Err("This filename is reserved by Windows".into());
    }
    let name = match name.extension().and_then(|extension| extension.to_str()) {
        None => name.with_extension("jpg"),
        Some(extension) if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") => name.to_path_buf(),
        _ => return Err("JPEG output needs a .jpg or .jpeg filename".into()),
    };
    let folder = PathBuf::from(folder);
    if !folder.is_absolute() {
        return Err("Enter an absolute destination folder".into());
    }
    Ok(folder.join(name))
}

fn start(app: &mut PhotocraftApp, ctx: &egui::Context, receipt: Option<controller::DestinationReceipt>) -> Result<(), String> {
    let Some(dialog) = app.raw_filmstrip.jpeg.dialog.as_ref() else { return Err("No JPEG export is open".into()) };
    if app.raw_filmstrip.jpeg.worker.is_some() {
        return Err("JPEG export is already running".into());
    }
    let path = output_path(&dialog.folder, &dialog.filename)?;
    let mut capture = dialog.capture.clone();
    // Retain the opening set too: removed photos may still be needed by recovery.
    for binding in persistence_ui::bindings(app) {
        if !capture.protected.contains(&binding) {
            capture.protected.push(binding);
        }
    }
    let quality = dialog.quality;
    let (tx, result) = mpsc::sync_channel(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let repaint = ctx.clone();
    std::thread::Builder::new()
        .name("camera-raw-jpeg-export".into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_export(capture, path, quality, receipt, &worker_cancel)))
                .unwrap_or_else(|_| ResultMessage::Failed("JPEG export worker failed; the recovery session remains open".into()));
            let _ = tx.send(outcome);
            repaint.request_repaint();
        })
        .map_err(|error| error.to_string())?;
    app.raw_filmstrip.jpeg.worker = Some(Worker { result, cancel });
    app.raw_filmstrip.jpeg.message = Some("Saving JPEG…".into());
    Ok(())
}

fn refresh_protected(capture: &mut Capture) -> Result<(), String> {
    if let Some(root) = &capture.recovery_root {
        for binding in photocraft_io::raw_session::store::protected_sources(root).map_err(|error| error.to_string())? {
            if !capture.protected.contains(&binding) {
                capture.protected.push(binding);
            }
        }
    }
    Ok(())
}

fn run_export(mut capture: Capture, path: PathBuf, quality: u8, receipt: Option<controller::DestinationReceipt>, cancel: &AtomicBool) -> ResultMessage {
    log::debug!("Camera Raw JPEG export uses captured photo {}", capture.photo.0);
    let result = (|| -> Result<ResultMessage, String> {
        if cancel.load(Ordering::Acquire) {
            return Ok(ResultMessage::Cancelled);
        }
        refresh_protected(&mut capture)?;
        let receipt = match receipt {
            Some(receipt) => receipt,
            None => {
                let receipt = controller::prepare_jpeg_destination(&path, &capture.protected).map_err(|error| error.to_string())?;
                if receipt.existed {
                    return Ok(ResultMessage::NeedsOverwrite(path, receipt));
                }
                receipt
            }
        };
        let bytes = controller::read_verified_source(&capture.binding).map_err(|error| error.to_string())?;
        if cancel.load(Ordering::Acquire) {
            return Ok(ResultMessage::Cancelled);
        }
        let rendered = controller::render_recipe(&capture.name, &bytes, &capture.recipe).map_err(|error| error.to_string())?;
        if cancel.load(Ordering::Acquire) {
            return Ok(ResultMessage::Cancelled);
        }
        let mut options = photocraft_io::ExportOptions::default();
        options.encode.jpeg_quality = quality;
        let encoded = photocraft_io::export(&rendered.document, "jpg", &options).map_err(|error| error.to_string())?;
        if cancel.load(Ordering::Acquire) {
            return Ok(ResultMessage::Cancelled);
        }
        refresh_protected(&mut capture)?;
        if cancel.load(Ordering::Acquire) {
            return Ok(ResultMessage::Cancelled);
        }
        let published = controller::publish_jpeg(&path, &encoded.bytes, &capture.protected, &receipt).map_err(|error| error.to_string())?;
        let confirmed = matches!(published, photocraft_format::atomic::PublicationOutcome::Published);
        let mut warnings = rendered.warnings;
        warnings.extend(encoded.warnings);
        if let photocraft_format::atomic::PublicationOutcome::PublishedUnconfirmed(error) = published {
            warnings.push(error.to_string());
        }
        Ok(ResultMessage::Saved { path, warnings, confirmed })
    })();
    result.unwrap_or_else(ResultMessage::Failed)
}

fn poll(app: &mut PhotocraftApp) {
    let Some(worker) = app.raw_filmstrip.jpeg.worker.as_ref() else { return };
    let mut result = match worker.result.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return,
        Err(mpsc::TryRecvError::Disconnected) => ResultMessage::Failed("JPEG export worker disconnected; inspect the output before retrying".into()),
    };
    if worker.cancel.load(Ordering::Acquire) && matches!(result, ResultMessage::NeedsOverwrite(..)) {
        result = ResultMessage::Cancelled;
    }
    app.raw_filmstrip.jpeg.worker = None;
    match result {
        ResultMessage::NeedsOverwrite(path, receipt) => {
            if let Some(dialog) = app.raw_filmstrip.jpeg.dialog.as_mut() {
                dialog.overwrite = Some((path, receipt));
            }
            app.raw_filmstrip.jpeg.message = None;
        }
        ResultMessage::Saved { path, warnings, confirmed } => {
            let mut message = if confirmed {
                format!("JPEG saved: {}", path.display())
            } else {
                format!("JPEG was published but final durability is unconfirmed: {}", path.display())
            };
            if !warnings.is_empty() {
                message.push_str(&format!("\n{}", warnings.join("\n")));
            }
            app.raw_filmstrip.jpeg.message = Some(message);
            app.raw_filmstrip.jpeg.dialog = None;
        }
        ResultMessage::Failed(error) => {
            if let Some(dialog) = app.raw_filmstrip.jpeg.dialog.as_mut() {
                dialog.error = Some(error.clone());
                dialog.overwrite = None;
            }
            app.raw_filmstrip.jpeg.message = Some(format!("JPEG save failed: {error}"));
        }
        ResultMessage::Cancelled => {
            app.raw_filmstrip.jpeg.message = Some("JPEG save cancelled before publication".into());
            app.raw_filmstrip.jpeg.dialog = None;
        }
    }
}

/// Call after the Camera Raw content each frame. While the editor modal is open,
/// the parent blocks background input; a running export allows photo navigation.
pub(super) fn show(app: &mut PhotocraftApp, ctx: &egui::Context) {
    poll(app);
    if let Some(worker) = app.raw_filmstrip.jpeg.worker.as_ref() {
        egui::Window::new(tl!("Saving JPEG")).id(egui::Id::new("camera-raw-jpeg-progress")).collapsible(false).resizable(false).show(ctx, |ui| {
            if let Some(dialog) = &app.raw_filmstrip.jpeg.dialog {
                ui.label(&dialog.capture.name);
            }
            ui.spinner();
            if worker.cancel.load(Ordering::Acquire) {
                ui.label(tl!("Cancelling before publication…"));
            } else if ui.button(tl!("Cancel")).clicked() {
                worker.cancel.store(true, Ordering::Release);
            }
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
        return;
    }
    let Some(mut dialog) = app.raw_filmstrip.jpeg.dialog.take() else { return };
    let mut cancel = false;
    let mut save = false;
    let mut browse = false;
    let mut approved = None;
    let response = egui::Modal::new(egui::Id::new("camera-raw-jpeg-save")).show(ctx, |ui| {
        ui.set_width(440.0_f32.min(ctx.content_rect().width() - 48.0).max(200.0));
        ui.heading(tl!("Save JPEG"));
        ui.label(format!("{}: {}", tl!("Photograph"), dialog.capture.name));
        ui.label(tl!("Uses the settings captured when Save was chosen."));
        ui.separator();
        if let Some((path, receipt)) = &dialog.overwrite {
            ui.label(tl!("Replace existing JPEG?"));
            ui.label(path.to_string_lossy());
            ui.horizontal(|ui| {
                if ui.button(tl!("Replace")).clicked() {
                    approved = Some(receipt.clone());
                }
                if ui.button(tl!("Back")).clicked() {
                    cancel = true;
                }
            });
        } else {
            ui.label(tl!("Filename"));
            ui.text_edit_singleline(&mut dialog.filename);
            ui.label(tl!("Destination folder"));
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut dialog.folder);
                browse = ui.button(tl!("Browse…")).clicked();
            });
            ui.add(egui::Slider::new(&mut dialog.quality, 1..=100).text(tl!("JPEG quality")));
            if let Some(error) = &dialog.error {
                ui.colored_label(crate::theme::Tokens::get(ui.ctx()).danger, error);
            }
            ui.horizontal(|ui| {
                save = ui.button(tl!("Save JPEG")).clicked();
                cancel = ui.button(tl!("Cancel")).clicked();
            });
        }
    });
    let escape = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    if cancel || escape || response.should_close() {
        if dialog.overwrite.take().is_some() {
            app.raw_filmstrip.jpeg.dialog = Some(dialog);
        }
        return;
    }
    let token = dialog.capture.token;
    app.raw_filmstrip.jpeg.dialog = Some(dialog);
    if browse {
        let initial_dir = app.raw_filmstrip.jpeg.dialog.as_ref().map(|dialog| dialog.folder.clone());
        if let Err(error) = app.ask_file(FileDialogRequest::Folder { initial_dir }, move |app, answer| {
            let FileDialogAnswer::FolderTo(folder) = answer else { return Err("The folder picker returned an unexpected answer".into()) };
            if let Some(dialog) = app.raw_filmstrip.jpeg.dialog.as_mut().filter(|dialog| dialog.capture.token == token) {
                dialog.folder = folder;
            }
            Ok(serde_json::json!({"jpegDestinationChosen": true}))
        }) && let Some(dialog) = app.raw_filmstrip.jpeg.dialog.as_mut()
        {
            dialog.error = Some(error);
        }
    }
    if (save || approved.is_some())
        && let Err(error) = start(app, ctx, approved)
        && let Some(dialog) = app.raw_filmstrip.jpeg.dialog.as_mut()
    {
        dialog.error = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_filename_cannot_escape_folder_use_ads_or_change_output_type() {
        let folder = std::env::temp_dir().to_string_lossy().into_owned();
        assert_eq!(output_path(&folder, "edited").unwrap(), std::env::temp_dir().join("edited.jpg"));
        assert!(output_path(&folder, "edited.JPEG").is_ok());
        for name in ["", "..", "../raw.nef", "a\\raw.nef", "raw.nef:stream.jpg", "output.png", "CON.jpg", "LPT1.jpg", "trailing .", "space.jpg "] {
            assert!(output_path(&folder, name).is_err(), "{name}");
        }
        assert!(output_path("relative", "edited.jpg").is_err());
    }

    #[test]
    fn captured_photo_exports_without_touching_source_and_overwrite_needs_current_consent() {
        let directory = std::env::temp_dir().join(format!("photocraft-jpeg-{}-{}", std::process::id(), PhotoId::fresh().0));
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join("captured.NEF");
        let bytes = photocraft_raw::testgen::tiff_ep("NIKON CORPORATION", 16, 16, &[1024; 256], [0, 1, 1, 2], 12, vec![]);
        std::fs::write(&source, &bytes).unwrap();
        let binding = photocraft_io::raw_session::commit::bind(&source).unwrap();
        let recipe = NativeRecipe {
            schema: photocraft_io::raw_session::SCHEMA_VERSION,
            processing: photocraft_io::raw::capture_processing(&bytes).unwrap(),
            adjustments: photocraft_algo::camera_raw::CameraRaw { exposure: 0.7, ..Default::default() },
            metadata: photocraft_io::raw_session::Metadata { rating: Some(4), label: "Review".into(), marked: true },
        };
        let capture = Capture {
            token: 1,
            photo: PhotoId(17),
            name: "captured.NEF".into(),
            binding: binding.clone(),
            recipe,
            protected: vec![binding],
            recovery_root: None,
        };
        let output = directory.join("output.jpg");
        assert!(matches!(run_export(capture.clone(), output.clone(), 90, None, &AtomicBool::new(false)), ResultMessage::Saved { confirmed: true, .. }));
        let encoded = std::fs::read(&output).unwrap();
        assert_eq!(encoded.get(..2), Some(&[0xff, 0xd8][..]));
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert!(!source.with_extension("xmp").exists());
        let ResultMessage::NeedsOverwrite(_, receipt) = run_export(capture.clone(), output.clone(), 90, None, &AtomicBool::new(false)) else {
            panic!("an existing derived JPEG needs explicit review");
        };
        assert!(matches!(
            run_export(capture.clone(), output.clone(), 90, Some(receipt), &AtomicBool::new(false)),
            ResultMessage::Saved { confirmed: true, .. }
        ));
        let ResultMessage::NeedsOverwrite(_, receipt) = run_export(capture.clone(), output.clone(), 90, None, &AtomicBool::new(false)) else {
            panic!("a later overwrite needs its own current receipt");
        };
        std::fs::write(&output, b"external change after confirmation").unwrap();
        assert!(matches!(run_export(capture.clone(), output.clone(), 90, Some(receipt), &AtomicBool::new(false)), ResultMessage::Failed(_)));
        assert_eq!(std::fs::read(&output).unwrap(), b"external change after confirmation");
        let cancelled = directory.join("cancelled.jpg");
        assert!(matches!(run_export(capture, cancelled.clone(), 90, None, &AtomicBool::new(true)), ResultMessage::Cancelled));
        assert!(!cancelled.exists());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn loaded_jpeg_export_refuses_own_source_other_member_alias_and_bound_sidecar() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
        let directory = std::env::temp_dir().join(format!("photocraft-jpeg-protected-{}-{}", std::process::id(), PhotoId::fresh().0));
        std::fs::create_dir(&directory).unwrap();
        let image = Image::from_normalized(2, 2, ChannelLayout::Rgb, SampleType::U8, &[0.25; 12]).unwrap();
        let bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        let source = directory.join("loaded.jpg");
        let other = directory.join("retained.jpg");
        std::fs::write(&source, &bytes).unwrap();
        std::fs::write(&other, &bytes).unwrap();
        let sidecar = directory.join("loaded.jpg.xmp");
        let packet = b"<r:RDF xmlns:r='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><r:Description/></r:RDF>";
        std::fs::write(&sidecar, packet).unwrap();
        let alias = directory.join("physical-alias.jpg");
        std::fs::hard_link(&source, &alias).unwrap();
        let binding = photocraft_io::raw_session::commit::bind(&source).unwrap();
        let recipe = photocraft_io::camera_raw_source::initial_recipe(&bytes, Some(packet)).unwrap();
        assert_eq!(recipe.processing.source_kind, photocraft_io::camera_raw_source::SourceKind::Jpeg);
        let capture = Capture {
            token: 1,
            photo: PhotoId(18),
            name: "loaded.jpg".into(),
            binding: binding.clone(),
            recipe,
            protected: vec![binding, photocraft_io::raw_session::commit::bind(&other).unwrap()],
            recovery_root: None,
        };
        for destination in [&source, &other, &alias, &sidecar] {
            assert!(
                matches!(run_export(capture.clone(), destination.clone(), 90, None, &AtomicBool::new(false)), ResultMessage::Failed(_)),
                "must protect {}",
                destination.display()
            );
            assert_eq!(std::fs::read(&source).unwrap(), bytes);
            assert_eq!(std::fs::read(&other).unwrap(), bytes);
            assert_eq!(std::fs::read(&alias).unwrap(), bytes);
            assert_eq!(std::fs::read(&sidecar).unwrap(), packet);
        }
        assert_eq!(capture.recipe, photocraft_io::camera_raw_source::initial_recipe(&bytes, Some(packet)).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn admitted_jpeg_request_and_worker_preserve_recipe_and_recovery_records() {
        use super::super::persistence_tests::{Fixture, context, open, settled};
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
        fn records(root: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
            let mut result = Vec::new();
            for entry in std::fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    result.extend(records(&path));
                } else if path.file_name().is_some_and(|name| name == "ownership.lock") {
                    // Windows denies byte reads through the live ownership lock.
                    // It is an empty synchronization file, not a recovery record.
                    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
                    result.push((path, Vec::new()));
                } else {
                    result.push((path.clone(), std::fs::read(path).unwrap()));
                }
            }
            result.sort_by(|left, right| left.0.cmp(&right.0));
            result
        }
        let fixture = Fixture::new();
        let source = fixture.source("loaded.jpg");
        let other = fixture.source("other.jpg");
        let image = Image::from_normalized(8, 6, ChannelLayout::Rgb, SampleType::U8, &[0.25; 144]).unwrap();
        let bytes = photocraft_codecs::encode(&image, Format::Jpeg, &EncodeOptions::default()).unwrap();
        std::fs::write(&source, &bytes).unwrap();
        std::fs::write(&other, &bytes).unwrap();
        let alias = source.with_file_name("alias.jpg");
        std::fs::hard_link(&source, &alias).unwrap();
        let mut app = fixture.app();
        app.session.edit_prefs(|prefs| prefs.raw_defaults.jpeg_handling = photocraft_engine::prefs::RenderedRawHandling::All);
        let ctx = context();
        open(&mut app, &source);
        open(&mut app, &other);
        let state = settled(&mut app, &ctx, 2);
        let photo = PhotoId(state["photos"][0]["photo"].as_u64().unwrap());
        let recipe = persistence_ui::recipe(&app, photo).unwrap().clone();
        let before = records(&fixture.cache());
        for destination in [&source, &other, &alias] {
            request(&mut app, photo).unwrap();
            let dialog = app.raw_filmstrip.jpeg.dialog.as_mut().unwrap();
            assert_eq!(dialog.capture.recovery_root, Some(fixture.cache()));
            assert_eq!(dialog.capture.recipe, recipe);
            assert_eq!(dialog.capture.protected.len(), 2);
            dialog.filename = destination.file_name().unwrap().to_str().unwrap().into();
            start(&mut app, &ctx, None).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while is_running(&app) {
                poll(&mut app);
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(app.raw_filmstrip.jpeg.dialog.as_ref().unwrap().error.is_some());
            assert_eq!(persistence_ui::recipe(&app, photo), Some(&recipe));
            assert_eq!(persistence_ui::describe(&app), state);
            assert_eq!(records(&fixture.cache()), before);
            assert_eq!(std::fs::read(&source).unwrap(), bytes);
            assert_eq!(std::fs::read(&other).unwrap(), bytes);
            assert!(!source.with_extension("xmp").exists());
            app.raw_filmstrip.jpeg.dialog = None;
        }
    }

    #[test]
    #[ignore = "real NEF rendering; requires verified fixture manifest and disposable JPEG artifact directory"]
    fn real_nef_captured_recipe_exports_full_resolution_jpeg() {
        let manifest = std::env::var_os("PHOTOCRAFT_RAW_UI_FIXTURE_MANIFEST").expect("explicit verified fixture manifest required");
        let output_root = PathBuf::from(std::env::var_os("PHOTOCRAFT_JPEG_ARTIFACTS").expect("explicit disposable artifact directory required"));
        let records: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let source_record = records
            .iter()
            .filter(|record| {
                std::path::Path::new(record["copy"].as_str().unwrap()).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("nef"))
            })
            .min_by_key(|record| record["bytes"].as_u64().unwrap())
            .expect("NEF fixture");
        assert_eq!(source_record["source_unchanged"], true);
        let pristine = PathBuf::from(source_record["copy"].as_str().unwrap());
        let source_bytes = std::fs::read(&pristine).unwrap();
        assert_eq!(source_bytes.len() as u64, source_record["bytes"].as_u64().unwrap());
        let directory = output_root.join(format!("real-nef-jpeg-{}-{}", std::process::id(), PhotoId::fresh().0));
        std::fs::create_dir_all(&output_root).unwrap();
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join(pristine.file_name().unwrap());
        std::fs::write(&source, &source_bytes).unwrap();
        let sidecar = source.with_extension("xmp");
        let original_xmp = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:RDF><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3" xmp:Label="Second"/></rdf:RDF></x:xmpmeta>"#;
        std::fs::write(&sidecar, original_xmp).unwrap();
        let binding = photocraft_io::raw_session::commit::bind(&source).unwrap();
        let recipe = NativeRecipe {
            schema: photocraft_io::raw_session::SCHEMA_VERSION,
            processing: photocraft_io::raw::capture_processing(&source_bytes).unwrap(),
            adjustments: photocraft_algo::camera_raw::CameraRaw { exposure: 0.3, contrast: 7.0, ..Default::default() },
            metadata: photocraft_io::raw_session::Metadata { rating: Some(5), label: "Review".into(), marked: true },
        };
        let sensor = photocraft_raw::decode(&source_bytes, &photocraft_raw::Limits::default()).unwrap();
        let expected_size = if recipe.processing.orient && (5..=8).contains(&sensor.orientation) {
            (sensor.crop.height as u32, sensor.crop.width as u32)
        } else {
            (sensor.crop.width as u32, sensor.crop.height as u32)
        };
        drop(sensor);
        let output = directory.join("captured-edited.jpg");
        let capture = Capture {
            token: 1,
            photo: PhotoId(17),
            name: source.file_name().unwrap().to_string_lossy().into_owned(),
            binding: binding.clone(),
            recipe: recipe.clone(),
            protected: vec![binding.clone()],
            recovery_root: None,
        };
        let warnings = match run_export(capture, output.clone(), 90, None, &AtomicBool::new(false)) {
            ResultMessage::Saved { confirmed: true, warnings, .. } => warnings,
            ResultMessage::Failed(error) => panic!("real JPEG export: {error}"),
            _ => panic!("real JPEG export did not report confirmed publication"),
        };
        let jpeg = std::fs::read(&output).unwrap();
        assert_eq!(jpeg.get(..2), Some(&[0xff, 0xd8][..]));
        let decoded = photocraft_io::import_with_svg_group_depth("captured-edited.jpg", &jpeg, 0).unwrap();
        let size = decoded.document.size;
        assert_eq!((size.width, size.height), expected_size, "JPEG must match the oriented RAW crop dimensions");
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&pristine).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&sidecar).unwrap(), original_xmp);
        let receipt = serde_json::json!({
            "scope": "Real NEF captured-recipe JPEG worker; no live desktop or picker",
            "source_fixture": pristine, "source_manifest_sha256": source_record["sha256"],
            "source_copy": source, "source_blake3": binding.source_fingerprint,
            "jpeg": output, "jpeg_bytes": jpeg.len(), "width": size.width, "height": size.height,
            "recipe": recipe, "source_and_existing_xmp_unchanged": true,
            "publication_confirmed": true, "warnings": warnings,
        });
        std::fs::write(directory.join("receipt.json"), serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        println!("Real NEF JPEG artifact: {}", directory.display());
    }
}
