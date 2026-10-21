//! Real decode/apply routing tests. These do not start a desktop or read user files.

// Test-only fixture helpers should fail the test immediately on setup errors.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::prefs::RenderedRawHandling::{All, Disabled, ExistingSettings};

fn packet(settings: bool) -> String {
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:HasSettings="{settings}" crs:Exposure2012="1.25"/></rdf:RDF></x:xmpmeta>"#
    )
}

fn photograph(extension: &str, settings: Option<bool>, layered: bool) -> Vec<u8> {
    let mut session = Session::new();
    session.execute("file.new", json!({"width":8,"height":6})).unwrap();
    if layered {
        session.execute("layer.new.layer", json!({"name":"Retained top layer"})).unwrap();
    }
    let mut doc = (*session.active().unwrap().doc).clone();
    doc.metadata.xmp = settings.map(packet);
    let options = photocraft_io::ExportOptions { tiff_layers: layered, ..Default::default() };
    photocraft_io::export(&doc, &format!("source.{extension}"), &options).unwrap().bytes
}

fn open(session: &mut Session, name: &str, source: OpenSource, admission: Admission) -> Result<Value> {
    match session.start_open_with(name, source, admission)? {
        Started::Job(id) => session.wait_job(id),
        Started::Done(value) => Ok(value),
    }
}

#[test]
fn rendered_opening_modes_use_actual_content_and_embedded_settings() {
    for extension in ["jpg", "tif"] {
        for embedded in [None, Some(false), Some(true)] {
            let bytes = photograph(extension, embedded, false);
            for mode in [Disabled, ExistingSettings, All] {
                for explicit in [false, true] {
                    let mut session = Session::new();
                    session.edit_prefs(|prefs| {
                        prefs.raw_defaults.jpeg_handling = if extension == "jpg" { mode } else { Disabled };
                        prefs.raw_defaults.tiff_handling = if extension == "tif" { mode } else { Disabled };
                    });
                    // The suffix is deliberately sensor RAW-looking. Content controls routing.
                    let admission = if explicit { Admission::ReviewInCameraRaw } else { Admission::ReviewIfRaw };
                    let value = open(&mut session, "renamed.NEF", OpenSource::Bytes(Arc::new(bytes.clone())), admission).unwrap();
                    let expected = explicit || mode == All || mode == ExistingSettings && embedded == Some(true);
                    assert_eq!(session.has_provisional(), expected, "{extension} {embedded:?} {mode:?} explicit={explicit}");
                    assert_eq!(value["warnings"].as_array().unwrap().iter().any(|warning| warning == CAMERA_RAW_REVIEW_NOTE), expected);
                    assert_eq!(session.active().unwrap().source_read_only, expected);
                }
            }
        }
    }
}

#[test]
fn layered_tiff_stays_in_editor_with_both_layers_and_names() {
    let bytes = photograph("tif", Some(true), true);
    assert!(photocraft_io::camera_raw_source::inspect(&bytes).unwrap().layered_tiff);
    for admission in [Admission::ReviewIfRaw, Admission::ReviewInCameraRaw] {
        let mut session = Session::new();
        session.edit_prefs(|prefs| prefs.raw_defaults.tiff_handling = All);
        open(&mut session, "layers.jpg", OpenSource::Bytes(Arc::new(bytes.clone())), admission).unwrap();
        assert!(!session.has_provisional());
        let doc = &session.active().unwrap().doc;
        assert_eq!(doc.layers.len(), 2);
        assert!(doc.layers.iter().any(|layer| layer.name == "Retained top layer"));
    }
}

#[test]
fn native_rendered_sidecar_controls_existing_settings_but_bytes_never_read_ambient_paths() {
    let dir = tempfile::tempdir().unwrap();
    for extension in ["jpg", "tif"] {
        let bytes = photograph(extension, Some(true), false);
        let source = dir.path().join(format!("photo.{extension}"));
        let path = source.to_string_lossy().into_owned();
        std::fs::write(&source, &bytes).unwrap();
        let sidecar = dir.path().join(format!("photo.{extension}.xmp"));
        // The native sidecar is the higher-priority packet, matching reconstruction.
        std::fs::write(&sidecar, packet(false)).unwrap();
        let mut native = Session::new();
        let value = open(&mut native, &path, OpenSource::Path(path.clone()), Admission::ReviewIfRaw).unwrap();
        assert!(
            !value["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| { warning.as_str().is_some_and(|message| message.contains("Camera Raw opening unavailable")) }),
            "the owned sidecar must be read, without falling back after a refusal"
        );
        assert!(!native.has_provisional(), "a bound HasSettings=False packet overrides embedded settings");
        std::fs::write(&sidecar, packet(true)).unwrap();
        let mut native = Session::new();
        open(&mut native, &path, OpenSource::Path(path.clone()), Admission::ReviewIfRaw).unwrap();
        assert!(native.has_provisional());
        let plain_bytes = photograph(extension, None, false);
        let mut supplied = Session::new();
        open(&mut supplied, &path, OpenSource::Bytes(Arc::new(plain_bytes)), Admission::ReviewIfRaw).unwrap();
        assert!(!supplied.has_provisional(), "a display name must never authorize reading its adjacent sidecar");
        assert_eq!(std::fs::read(&source).unwrap(), bytes, "routing never rewrites the photograph");
        std::fs::remove_file(&sidecar).unwrap();
    }
}

/// Candidate F (stable binding): a JPEG's own `<name>.jpg.xmp` is its sidecar whenever it
/// exists, so a leftover `<stem>.xmp` (for example a deleted RAW's) is never ambiguous with
/// it and never read: here only the stem packet carries settings, and the JPEG opens as an
/// ordinary editor document. Two same-stem RAW sidecars stay ambiguous (io tests).
#[test]
fn a_jpegs_own_full_name_sidecar_wins_over_a_stem_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("photo.jpg");
    std::fs::write(&source, photograph("jpg", None, false)).unwrap();
    std::fs::write(source.with_extension("xmp"), packet(true)).unwrap();
    std::fs::write(dir.path().join("photo.jpg.xmp"), packet(false)).unwrap();
    let mut session = Session::new();
    open(&mut session, "photo.jpg", OpenSource::Path(source.to_string_lossy().into_owned()), Admission::ReviewIfRaw).unwrap();
    assert_eq!(session.documents().len(), 1);
    assert!(!session.has_provisional(), "routed by its own sidecar (no settings), not by the stem packet");
}

/// Candidate F: with JPEG handling Disabled the photo is never routed automatically, so its
/// sidecars are not read and an unreadable sidecar cannot block the ordinary editor open.
/// An explicit Camera Raw open still refuses that sidecar before admitting anything.
#[test]
fn disabled_rendered_handling_ignores_sidecars_but_explicit_review_still_refuses_ambiguity() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("photo.jpg");
    let bytes = photograph("jpg", None, false);
    std::fs::write(&source, &bytes).unwrap();
    // Its own sidecar is over the 1 MiB binding budget: explicit admission must refuse it.
    std::fs::write(dir.path().join("photo.jpg.xmp"), format!("{}{}", packet(true), " ".repeat(1_100_000))).unwrap();
    let path = source.to_string_lossy().into_owned();
    let mut session = Session::new();
    session.edit_prefs(|prefs| prefs.raw_defaults.jpeg_handling = Disabled);
    open(&mut session, &path, OpenSource::Path(path.clone()), Admission::ReviewIfRaw).unwrap();
    assert_eq!(session.documents().len(), 1, "an ordinary editor document");
    assert!(!session.has_provisional());
    let mut explicit = Session::new();
    explicit.edit_prefs(|prefs| prefs.raw_defaults.jpeg_handling = Disabled);
    assert!(open(&mut explicit, &path, OpenSource::Path(path.clone()), Admission::ReviewInCameraRaw).is_err());
    assert!(explicit.documents().is_empty());
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
}

#[test]
fn cancelled_rendered_review_cannot_reappear_after_the_preference_is_disabled() {
    for extension in ["jpg", "tif"] {
        let mut session = Session::new();
        let bytes = photograph(extension, Some(true), false);
        let Started::Job(job) = session.start_open_with("renamed.dat", OpenSource::Bytes(Arc::new(bytes)), Admission::ReviewIfRaw).unwrap() else {
            panic!("native job should be deferred");
        };
        session.next_review_generation();
        session.edit_prefs(|prefs| {
            prefs.raw_defaults.jpeg_handling = Disabled;
            prefs.raw_defaults.tiff_handling = Disabled;
        });
        assert!(matches!(session.wait_job(job), Err(EngineError::Cancelled)));
        assert!(session.documents().is_empty());
    }
}

#[test]
fn unsupported_embedded_packet_falls_back_only_for_automatic_opening() {
    let mut bytes = photograph("jpg", None, false);
    // A structurally valid extension segment reaches the Camera Raw routing
    // fallback; a truncated segment is correctly rejected by the JPEG decoder.
    let packet = packet(true);
    let mut payload = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
    payload.extend_from_slice(b"0123456789ABCDEF0123456789ABCDEF");
    payload.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(packet.as_bytes());
    let mut segment = vec![0xff, 0xe1];
    segment.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
    segment.extend_from_slice(&payload);
    bytes.splice(2..2, segment);
    let mut automatic = Session::new();
    let result = open(&mut automatic, "extended.jpg", OpenSource::Bytes(Arc::new(bytes.clone())), Admission::ReviewIfRaw).unwrap();
    assert!(!automatic.has_provisional());
    assert!(result["warnings"].as_array().unwrap().iter().any(|warning| warning.as_str().is_some_and(|text| text.contains("Camera Raw opening unavailable"))));
    let mut explicit = Session::new();
    assert!(open(&mut explicit, "extended.jpg", OpenSource::Bytes(Arc::new(bytes)), Admission::ReviewInCameraRaw).is_err());
    assert!(explicit.documents().is_empty());
}

/// Round-5 sweep (pre-existing since E): a RAW whose sensor data is not decoded yet (CR3,
/// compressed ORF, X-Trans, sRAW) opens only as its embedded preview and is never routed into
/// Camera Raw, where it could never be admitted; a developed RAW and a rendered photo are.
#[test]
fn only_a_developed_raw_routes_into_camera_raw() {
    use photocraft_io::camera_raw_source::{Inspection, SourceKind};
    let defaults = crate::prefs::RawDefaults::default();
    let raw = Inspection { kind: SourceKind::Raw, supported: true, layered_tiff: false, has_settings: false, embedded_xmp: None };
    let developed = vec![format!("photo.CR2: {} (ahd demosaic, as-shot white balance)", photocraft_io::raw::DEVELOPED_NOTE)];
    let preview = vec!["photo.CR3: Canon CR3 is not decoded yet; opened its embedded preview".to_string()];
    assert!(routes_imported(&defaults, &raw, true, &developed));
    assert!(!routes_imported(&defaults, &raw, true, &preview), "explicit, but there is nothing Camera Raw can develop");
    assert!(!routes_imported(&defaults, &raw, false, &preview));
    let jpeg = Inspection { kind: SourceKind::Jpeg, supported: true, layered_tiff: false, has_settings: true, embedded_xmp: None };
    assert!(routes_imported(&defaults, &jpeg, true, &[]), "a rendered photo needs no developed note");
}

/// Round-7: an ordinary open of a JPEG whose own sidecar cannot be used (here over the 1 MiB
/// binding budget) opens in the editor with a warning instead of failing; an explicit Camera
/// Raw open still refuses before admitting anything.
#[test]
fn an_unusable_sidecar_never_blocks_an_ordinary_open() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("photo.jpg");
    let bytes = photograph("jpg", None, false);
    std::fs::write(&source, &bytes).unwrap();
    std::fs::write(dir.path().join("photo.jpg.xmp"), format!("{}{}", packet(true), " ".repeat(1_100_000))).unwrap();
    let path = source.to_string_lossy().into_owned();
    let mut session = Session::new();
    let opened = open(&mut session, &path, OpenSource::Path(path.clone()), Admission::ReviewIfRaw).unwrap();
    assert_eq!(session.documents().len(), 1, "an ordinary editor document");
    assert!(!session.has_provisional());
    assert!(opened["warnings"].as_array().unwrap().iter().any(|w| w.as_str().is_some_and(|w| w.contains("Camera Raw opening unavailable"))), "{opened}");
    let mut explicit = Session::new();
    assert!(open(&mut explicit, &path, OpenSource::Path(path.clone()), Admission::ReviewInCameraRaw).is_err());
    assert!(explicit.documents().is_empty());
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
}
