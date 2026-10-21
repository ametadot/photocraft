//! PR02-A3 production bridge contract: `photocraft-io` maps active scalar
//! properties through the same bounded `photocraft-xmp` parser used for
//! inspection. Synthetic reference values are pinned explicitly.
//!
//! These are integration/invariant tests, NOT independent differential tests
//! now that both readers share a parser. Genuine Adobe reference comparisons,
//! rendering parity and Adobe write/reopen parity remain separate gates.

use photocraft_io::raw_xmp::{MAX_XMP_BYTES, XmpImport, XmpImportError, import_adobe_raw_settings};
use photocraft_xmp::{ValueState, XmpPacket};

const A: &[u8] = include_bytes!("fixtures/xmp_compat/sample_a.xmp");
const B: &[u8] = include_bytes!("fixtures/xmp_compat/sample_b.xmp");
const ZERO: &[u8] = include_bytes!("fixtures/xmp_compat/explicit_zero.xmp");
const MISSING: &[u8] = include_bytes!("fixtures/xmp_compat/missing_active.xmp");
const DISABLED: &[u8] = include_bytes!("fixtures/xmp_compat/disabled_settings.xmp");
const CONFLICT: &[u8] = include_bytes!("fixtures/xmp_compat/conflicting_active.xmp");

/// Each scalar is still an *approximate* PhotoCraft rendering control.
/// This test asserts exact metadata interpretation, not pixel equivalence.
fn assert_tonal_agreement(source: &[u8], packet: &XmpPacket, imported: &XmpImport) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(packet.original_bytes(), source, "parser must retain input bytes");
    let enabled = !matches!(packet.camera_raw("HasSettings"), ValueState::Text(v) if v.eq_ignore_ascii_case("false"));
    let fields = [
        ("Exposure2012", imported.params.exposure),
        ("Contrast2012", imported.params.contrast),
        ("Highlights2012", imported.params.highlights),
        ("Shadows2012", imported.params.shadows),
        ("Whites2012", imported.params.whites),
        ("Blacks2012", imported.params.blacks),
    ];
    for (name, actual) in fields {
        match packet.camera_raw(name) {
            ValueState::Absent => {
                assert!(!imported.applied.contains(&name), "missing {name} incorrectly marked applied");
                assert_eq!(actual, 0.0, "missing {name} should use default tonal value");
            }
            ValueState::Text(raw) if !enabled => {
                assert!(raw.parse::<f32>().is_ok());
                assert!(!imported.applied.contains(&name), "disabled {name} incorrectly applied");
                assert_eq!(actual, 0.0, "disabled {name} should use default tonal value");
            }
            ValueState::Text(raw) => {
                let expected: f32 = raw.parse()?;
                assert!(imported.applied.contains(&name), "present {name} not marked applied");
                assert_eq!(actual, expected, "v0.3 and live importer disagree on {name}");
            }
            ValueState::Resource(_) | ValueState::Opaque => {
                panic!("structured {name} requires an explicit unsupported mapping")
            }
        }
    }
    Ok(())
}

#[test]
fn synthetic_batch_each_photo_has_its_own_active_values() -> Result<(), Box<dyn std::error::Error>> {
    for (bytes, exposure, contrast, highlights, shadows) in [(A, 0.99_f32, 37.0, -87.0, 8.0), (B, 0.85_f32, 33.0, -81.0, -5.0)] {
        let parsed = XmpPacket::parse(bytes)?;
        let mapped = import_adobe_raw_settings(bytes)?;
        assert_tonal_agreement(bytes, &parsed, &mapped)?;
        assert_eq!(mapped.params.exposure, exposure);
        assert_eq!(mapped.params.contrast, contrast);
        assert_eq!(mapped.params.highlights, highlights);
        assert_eq!(mapped.params.shadows, shadows);
        assert_eq!(mapped.params.blacks, -40.0);
        assert_eq!(mapped.applied.len(), 6);
    }
    Ok(())
}

#[test]
fn active_values_are_not_historical_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    let packet = XmpPacket::parse(A)?;
    let imported = import_adobe_raw_settings(A)?;
    assert_eq!(packet.snapshots().len(), 1);
    assert_eq!(packet.snapshots()[0].name.as_deref(), Some("Before"));
    assert_eq!(packet.camera_raw("Exposure2012"), ValueState::Text("0.99"));
    assert_eq!(imported.params.exposure, 0.99);
    assert_tonal_agreement(A, &packet, &imported)?;
    Ok(())
}

#[test]
fn absent_zero_and_disabled_are_not_interchangeable() -> Result<(), Box<dyn std::error::Error>> {
    let zero = XmpPacket::parse(ZERO)?;
    let missing = XmpPacket::parse(MISSING)?;
    let disabled = XmpPacket::parse(DISABLED)?;
    assert_eq!(zero.camera_raw("Exposure2012"), ValueState::Text("0"));
    assert_eq!(missing.camera_raw("Exposure2012"), ValueState::Absent);
    assert_eq!(disabled.camera_raw("HasSettings"), ValueState::Text("False"));
    for source in [ZERO, MISSING, DISABLED] {
        let packet = XmpPacket::parse(source)?;
        let mapped = import_adobe_raw_settings(source)?;
        assert_tonal_agreement(source, &packet, &mapped)?;
    }
    let explicitly_zero = import_adobe_raw_settings(ZERO)?;
    assert_eq!(explicitly_zero.applied, vec!["Exposure2012", "Contrast2012"]);
    assert!(import_adobe_raw_settings(MISSING)?.applied.is_empty());
    assert!(import_adobe_raw_settings(DISABLED)?.applied.is_empty());
    Ok(())
}

#[test]
fn unknown_metadata_is_retained_without_becoming_a_slider() -> Result<(), Box<dyn std::error::Error>> {
    let packet = XmpPacket::parse(A)?;
    let values = packet.active_properties();
    assert_eq!(packet.original_bytes(), A);
    assert!(values.keys().any(|k| k.namespace_uri == "urn:photocraft:test:unknown" && k.local_name == "VendorData"));
    assert_eq!(packet.camera_raw("ProcessVersion"), ValueState::Text("11.0"));
    let mapped = import_adobe_raw_settings(A)?;
    assert_eq!(mapped.applied.len(), 6);
    assert_eq!(mapped.unsupported_count, 1); // ProcessVersion; structured settings are separately preserved by v0.3.
    Ok(())
}

#[test]
fn hostile_xml_and_conflicting_active_values_fail_closed() {
    assert!(XmpPacket::parse(CONFLICT).is_err());
    assert!(matches!(import_adobe_raw_settings(CONFLICT), Err(XmpImportError::ConflictingValue("Exposure2012"))));
    let malicious = b"<!DOCTYPE rdf:RDF [<!ENTITY secret SYSTEM 'file:///windows/win.ini'>]><rdf:RDF/>";
    assert!(XmpPacket::parse(malicious).is_err());
    assert!(import_adobe_raw_settings(malicious).is_err());
    let oversized = vec![b'x'; MAX_XMP_BYTES + 1];
    assert!(XmpPacket::parse(&oversized).is_err());
    assert!(import_adobe_raw_settings(&oversized).is_err());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn candidate_paths_and_identity_conflicts_are_explicit() -> Result<(), Box<dyn std::error::Error>> {
    use photocraft_xmp::sidecar::{AuthorizedCandidate, CompanionKind, SidecarError, plan_companions, select_authorized};
    use std::path::Path;
    let plan = plan_companions(Path::new("C:/synthetic/sample_a.NEF"))?;
    let xmp_candidates: Vec<_> = plan.iter().filter(|c| c.kind == CompanionKind::Xmp).collect();
    assert_eq!(xmp_candidates.len(), 4);
    let stem = xmp_candidates[0].clone();
    let full = xmp_candidates[1].clone();
    let distinct = [
        AuthorizedCandidate { candidate: stem.clone(), resolved_identity: "file-A".into() },
        AuthorizedCandidate { candidate: full.clone(), resolved_identity: "file-B".into() },
    ];
    assert!(matches!(select_authorized(&plan, &distinct, CompanionKind::Xmp), Err(SidecarError::Ambiguous { .. })));
    let aliases = [
        AuthorizedCandidate { candidate: stem.clone(), resolved_identity: "file-A".into() },
        AuthorizedCandidate { candidate: full.clone(), resolved_identity: "file-A".into() },
    ];
    assert!(select_authorized(&plan, &aliases, CompanionKind::Xmp)?.is_some());
    Ok(())
}

/// Opt-in private-fixture differential gate, never run by the default test suite.
/// Set PHOTOCRAFT_XMP_REFERENCE to the absolute path of a *copy* of the XMP.
/// No private bytes, paths or hashes are written to stdout, the repository or reports.
#[test]
#[ignore = "requires PHOTOCRAFT_XMP_REFERENCE; do not publish private fixture paths or bytes"]
fn privately_supplied_adobe_xmp_matches_both_readers() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    let path = std::env::var_os("PHOTOCRAFT_XMP_REFERENCE").ok_or("PHOTOCRAFT_XMP_REFERENCE not set; point to a local XMP test COPY")?;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_XMP_BYTES as u64) + 1).read_to_end(&mut bytes)?;
    let packet = XmpPacket::parse(&bytes)?;
    let imported = import_adobe_raw_settings(&bytes)?;
    assert_tonal_agreement(&bytes, &packet, &imported)?;
    assert!(!imported.applied.is_empty(), "reference XMP did not contain supported active tonal settings");
    Ok(())
}
