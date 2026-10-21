//! Private, explicit-path corpus check. Run with --features private-fixture.
//! No photo is packaged, renamed, or modified by this test.
#![forbid(unsafe_code)]

#[cfg(feature = "private-fixture")]
#[test]
fn supplied_nikon_sidecar_from_a_third_party_editor() -> Result<(), Box<dyn std::error::Error>> {
    use photocraft_xmp::{ValueState, XmpPacket};
    use std::fs;
    use std::path::Path;

    // Paths are explicit and controlled by the local test runner, not inferred
    // from a username, untrusted RAW path or automation command.
    let raw = std::env::var("PHOTOCRAFT_PRIVATE_NEF_PATH")?;
    let xmp = std::env::var("PHOTOCRAFT_PRIVATE_XMP_PATH")?;
    if !Path::new(&raw).is_file() {
        return Err("required private NEF fixture is unavailable".into());
    }
    let xmp_path = Path::new(&xmp);
    let before = fs::read(xmp_path)?;
    let packet = XmpPacket::parse(&before)?;
    assert_eq!(packet.original_bytes(), before.as_slice());
    // The expected values describe the private sidecar itself, so they are supplied with its
    // path rather than committed: `Name=Value` pairs separated by `;`, and the snapshot-name
    // prefixes in order, separated by `|`.
    let expected = std::env::var("PHOTOCRAFT_PRIVATE_XMP_EXPECT")?;
    for pair in expected.split(';').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').ok_or("PHOTOCRAFT_PRIVATE_XMP_EXPECT needs Name=Value pairs")?;
        assert_eq!(packet.camera_raw(name), ValueState::Text(value), "{name}");
    }
    let prefixes = std::env::var("PHOTOCRAFT_PRIVATE_XMP_SNAPSHOTS")?;
    let prefixes: Vec<&str> = prefixes.split('|').collect();
    assert_eq!(packet.snapshots().len(), prefixes.len());
    for (snapshot, prefix) in packet.snapshots().iter().zip(&prefixes) {
        let name = snapshot.name.as_deref().unwrap_or("");
        assert!(name.starts_with(prefix), "{name:?} should start with {prefix:?}");
    }
    assert_eq!(fs::read(xmp_path)?, before);
    Ok(())
}
