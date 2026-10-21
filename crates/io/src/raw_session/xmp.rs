//! Pure, strict XMP planning. Existing unrelated XML stays byte-for-byte intact.
//! The bounded production XMP parser remains the validity and ambiguity gate;
//! roxmltree supplies byte ranges only after that gate succeeds.

use super::NativeRecipe;
use photocraft_xmp::{CAMERA_RAW_NS, ExpandedName, RDF_NS, ValueState, XMP_NS, XmpPacket};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const NATIVE_NS: &str = "https://photocraft.app/ns/camera-raw/1.0/";
const NATIVE_FIELD: &str = "Recipe";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativePacket {
    version: u32,
    recipe: NativeRecipe,
    /// The exact standard values present when the native recipe was emitted.
    projection: BTreeMap<String, Option<String>>,
    /// Rendered-source import understands more CRS fields than RAW projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rendered_import: Option<BTreeMap<String, Option<String>>>,
}

fn rendered_import(packet: &XmpPacket, bytes: &[u8]) -> Result<BTreeMap<String, Option<String>>, String> {
    crate::camera_raw_xmp::imported_fields()
        .map(|name| {
            let key = ExpandedName::new(CAMERA_RAW_NS, name);
            let signature = match packet.active_properties().get(&key) {
                None => None,
                Some(photocraft_xmp::XmpValue::Text(value)) => Some(format!("text:{value}")),
                Some(photocraft_xmp::XmpValue::Resource(value)) => Some(format!("resource:{value}")),
                Some(photocraft_xmp::XmpValue::Opaque { xml_span }) => {
                    let xml = bytes.get(xml_span.clone()).ok_or("Invalid rendered property span")?;
                    Some(format!("xml:{}", blake3::hash(xml)))
                }
            };
            Ok((name.to_owned(), signature))
        })
        .collect()
}

fn projection(packet: &XmpPacket) -> Result<BTreeMap<String, Option<String>>, String> {
    match packet.camera_raw("HasSettings") {
        ValueState::Absent => {}
        ValueState::Text(value) if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") => {}
        _ => return Err("Invalid or structured Camera Raw HasSettings flag".into()),
    }
    let mut result = BTreeMap::new();
    for (namespace, name) in fields() {
        let value = match packet.property(namespace, name) {
            ValueState::Absent => None,
            ValueState::Text(text) => Some(text.to_owned()),
            _ => return Err(format!("Structured standard property {name} cannot be projected safely")),
        };
        result.insert(name.to_owned(), value);
    }
    Ok(result)
}

fn fields() -> [(&'static str, &'static str); 9] {
    [
        (CAMERA_RAW_NS, "Exposure2012"),
        (CAMERA_RAW_NS, "Contrast2012"),
        (CAMERA_RAW_NS, "Highlights2012"),
        (CAMERA_RAW_NS, "Shadows2012"),
        (CAMERA_RAW_NS, "Whites2012"),
        (CAMERA_RAW_NS, "Blacks2012"),
        (XMP_NS, "Rating"),
        (XMP_NS, "Label"),
        (CAMERA_RAW_NS, "HasSettings"),
    ]
}

fn standard(recipe: &NativeRecipe) -> BTreeMap<ExpandedName, String> {
    let p = &recipe.adjustments;
    let values = [p.exposure, p.contrast, p.highlights, p.shadows, p.whites, p.blacks];
    let mut result = BTreeMap::new();
    for ((namespace, name), value) in fields().into_iter().take(6).zip(values) {
        result.insert(ExpandedName::new(namespace, name), value.to_string());
    }
    if let Some(rating) = recipe.metadata.rating {
        result.insert(ExpandedName::new(XMP_NS, "Rating"), rating.to_string());
    }
    result.insert(ExpandedName::new(XMP_NS, "Label"), recipe.metadata.label.clone());
    result
}

/// Returns no output for exact semantic no-op, including edit then exact undo.
/// `existing=None` means verified absence, not an unreadable or empty sidecar.
pub fn plan(existing: Option<&[u8]>, baseline: &NativeRecipe, working: &NativeRecipe) -> Result<Option<Vec<u8>>, String> {
    baseline.validate()?;
    working.validate()?;
    if baseline == working {
        return Ok(None);
    }
    let blank = format!("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"{RDF_NS}\"><rdf:Description rdf:about=\"\"/></rdf:RDF></x:xmpmeta>");
    let input = existing.unwrap_or(blank.as_bytes());
    let packet = XmpPacket::parse(input).map_err(|e| e.to_string())?;
    if packet.active_description_count() == 0 {
        return Err("No current-resource XMP description".into());
    }
    // Refuse to overwrite future or stale native state, even if the intended edit
    // touches another field. The caller must resolve it with a fresh baseline.
    let _ = load_native(input)?;
    let before = standard(baseline);
    let after = standard(working);
    let mut changes: BTreeMap<_, _> = after.into_iter().filter(|(key, value)| before.get(key) != Some(value)).collect();
    if baseline.adjustments != working.adjustments {
        // An inactive Adobe packet may still contain old supported values. When
        // enabling it, project all supported tones from the actual recipe so an
        // unchanged default does not reactivate an ignored historical exposure.
        if matches!(packet.camera_raw("HasSettings"), ValueState::Text(value) if value.eq_ignore_ascii_case("false")) {
            changes.extend(standard(working).into_iter().filter(|(key, _)| key.namespace_uri == CAMERA_RAW_NS));
        }
        changes.insert(ExpandedName::new(CAMERA_RAW_NS, "HasSettings"), "True".into());
    }
    let projected = patch(input, &changes)?;
    let packet = XmpPacket::parse(&projected).map_err(|e| e.to_string())?;
    let imported = if working.processing.source_kind == crate::camera_raw_source::SourceKind::Raw { None } else { Some(rendered_import(&packet, &projected)?) };
    let native = NativePacket { version: 1, recipe: working.clone(), projection: projection(&packet)?, rendered_import: imported };
    let payload = serde_json::to_string(&native).map_err(|e| e.to_string())?;
    if payload.len() > super::MAX_RECIPE_BYTES {
        return Err("Native XMP payload exceeds limit".into());
    }
    let output = patch(&projected, &BTreeMap::from([(ExpandedName::new(NATIVE_NS, NATIVE_FIELD), payload)]))?;
    // Refuse before consent: a reopened sidecar must reconstruct this exact recipe.
    if load_native(&output)?.as_ref() != Some(working) {
        return Err("Planned XMP would not reconstruct the exact recipe; no sidecar was changed".into());
    }
    Ok(Some(output))
}

/// Native reconstruction has no cache dependency. Recognized external standard
/// changes are explicit conflicts, never silently hidden by stale private state.
pub fn load_native(bytes: &[u8]) -> Result<Option<NativeRecipe>, String> {
    let packet = XmpPacket::parse(bytes).map_err(|e| e.to_string())?;
    let xml = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let tree = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    if tree.descendants().any(|node| node.attribute((photocraft_xmp::XML_NS, "base")).is_some()) {
        return Err("XMP xml:base subject resolution is unsupported; preserve original packet".into());
    }
    let payload = match packet.property(NATIVE_NS, NATIVE_FIELD) {
        ValueState::Absent => return Ok(None),
        ValueState::Text(text) => text,
        _ => return Err("Unsupported structured PhotoCraft recipe".into()),
    };
    if payload.len() > super::MAX_RECIPE_BYTES {
        return Err("Native XMP payload exceeds limit".into());
    }
    let value: serde_json::Value = serde_json::from_str(payload).map_err(|e| e.to_string())?;
    let recipe_bytes = serde_json::to_vec(value.get("recipe").ok_or("Missing native recipe")?).map_err(|e| e.to_string())?;
    let recipe = NativeRecipe::decode(&recipe_bytes)?;
    let mut native: NativePacket = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if native.version != 1 {
        return Err("Unsupported native XMP schema; preserve packet".into());
    }
    // Original schema-1 packets did not track the enable flag. An absent flag is
    // backward compatible; an existing untracked flag requires reconciliation.
    if !native.projection.contains_key("HasSettings") && matches!(packet.camera_raw("HasSettings"), ValueState::Absent) {
        native.projection.insert("HasSettings".into(), None);
    }
    if native.projection != projection(&packet)? {
        return Err("Recognized XMP properties changed after the native recipe; resolve external edits".into());
    }
    if recipe.processing.source_kind != crate::camera_raw_source::SourceKind::Raw && native.rendered_import.as_ref() != Some(&rendered_import(&packet, bytes)?)
    {
        return Err("Rendered XMP properties changed after the native recipe or lack provenance; resolve external edits".into());
    }
    Ok(Some(recipe))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn patch(input: &[u8], changes: &BTreeMap<ExpandedName, String>) -> Result<Vec<u8>, String> {
    let original = XmpPacket::parse(input).map_err(|e| e.to_string())?;
    if changes.is_empty() {
        return Ok(input.to_vec());
    }
    let text = std::str::from_utf8(input).map_err(|e| e.to_string())?;
    let tree = roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    if tree.descendants().any(|node| node.attribute((photocraft_xmp::XML_NS, "base")).is_some()) {
        return Err("XMP xml:base subject resolution is unsupported; preserve original packet".into());
    }
    let mut rdfs = tree.descendants().filter(|n| n.has_tag_name((RDF_NS, "RDF")));
    let rdf = rdfs.next().ok_or("Missing RDF root")?;
    if rdfs.next().is_some() {
        return Err("Multiple RDF roots require explicit resolution".into());
    }
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for desc in rdf.children().filter(|n| n.has_tag_name((RDF_NS, "Description")) && n.attribute((RDF_NS, "about")).is_none_or(str::is_empty)) {
        // Nonempty nodeID/ID changes subject semantics; do not treat it as current.
        if desc.attribute((RDF_NS, "nodeID")).is_some() || desc.attribute((RDF_NS, "ID")).is_some() {
            return Err("Named RDF subject cannot be safely edited as the current resource".into());
        }
        for attr in desc.attributes() {
            if changes.contains_key(&ExpandedName::new(attr.namespace().unwrap_or(""), attr.name())) {
                edits.push((attr.range(), String::new()));
            }
        }
        for node in desc.children().filter(|n| n.is_element()) {
            let name = node.tag_name();
            if changes.contains_key(&ExpandedName::new(name.namespace().unwrap_or(""), name.name())) {
                edits.push((node.range(), String::new()));
            }
        }
    }
    let rdf_text = text.get(rdf.range()).ok_or("Invalid RDF source range")?;
    let closing = rdf_text.rfind("</").ok_or("Empty RDF root cannot be patched safely")? + rdf.range().start;
    let mut addition = format!("<pc_rdf:Description xmlns:pc_rdf=\"{RDF_NS}\" pc_rdf:about=\"\"");
    for (index, (name, value)) in changes.iter().enumerate() {
        if !matches!(name.namespace_uri.as_str(), NATIVE_NS | CAMERA_RAW_NS | XMP_NS) {
            return Err("Attempt to modify unowned XMP namespace".into());
        }
        addition.push_str(&format!(" xmlns:pc{index}=\"{}\" pc{index}:{}=\"{}\"", escape(&name.namespace_uri), name.local_name, escape(value)));
    }
    addition.push_str("/>");
    edits.push((closing..closing, addition));
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut output = text.to_owned();
    let mut previous = output.len();
    for (range, replacement) in edits {
        if range.end > previous || output.get(range.clone()).is_none() {
            return Err("Overlapping XMP update ranges".into());
        }
        previous = range.start;
        output.replace_range(range, &replacement);
    }
    let checked = XmpPacket::parse(output.as_bytes()).map_err(|e| e.to_string())?;
    for (name, value) in original.active_properties() {
        if !changes.contains_key(name) {
            // Opaque values use byte spans, which move after edits. Verify their raw
            // XML separately; text/resource properties compare their decoded values.
            match value {
                photocraft_xmp::XmpValue::Opaque { xml_span } => {
                    let preserved = text.get(xml_span.clone()).ok_or("Invalid original XML span")?;
                    if !output.contains(preserved) {
                        return Err("Unrelated structured metadata changed".into());
                    }
                }
                _ if checked.active_properties().get(name) != Some(value) => return Err("Unrelated XMP metadata changed".into()),
                _ => {}
            }
        }
    }
    for (name, value) in changes {
        if checked.property(&name.namespace_uri, &name.local_name) != ValueState::Text(value) {
            return Err("Generated XMP did not retain intended value".into());
        }
    }
    Ok(output.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_roundtrip_keeps_unknown_arrays_literal_labels_and_precise_values() {
        let baseline = super::super::tests::recipe(0.);
        let mut working = baseline.clone();
        working.adjustments.exposure = 0.7853281;
        working.metadata.marked = true;
        let xml = format!(
            "<r:RDF xmlns:r=\"{RDF_NS}\" xmlns:c=\"{CAMERA_RAW_NS}\" xmlns:x=\"{XMP_NS}\" xmlns:u=\"urn:unknown\"><r:Description c:Exposure2012=\"0\" x:Label=\"Yellow\"><u:values><r:Bag><r:li>keep &amp; retain</r:li></r:Bag></u:values></r:Description></r:RDF>"
        );
        let bytes = plan(Some(xml.as_bytes()), &baseline, &working).unwrap().unwrap();
        assert_eq!(load_native(&bytes).unwrap(), Some(working));
        let output = std::str::from_utf8(&bytes).unwrap();
        assert!(output.contains("<u:values><r:Bag><r:li>keep &amp; retain</r:li></r:Bag></u:values>"));
        assert!(output.contains("x:Label=\"Yellow\""));
    }
    #[test]
    fn external_standard_change_cannot_be_hidden_by_native_payload() {
        let baseline = super::super::tests::recipe(0.);
        let working = super::super::tests::recipe(1.);
        let saved = plan(None, &baseline, &working).unwrap().unwrap();
        let changed = patch(&saved, &BTreeMap::from([(ExpandedName::new(CAMERA_RAW_NS, "Exposure2012"), "2".into())])).unwrap();
        assert!(load_native(&changed).unwrap_err().contains("changed after"));
    }
    #[test]
    fn rendered_native_provenance_detects_external_scalar_and_curve_edits() {
        let mut baseline = super::super::tests::recipe(0.);
        baseline.processing.source_kind = crate::camera_raw_source::SourceKind::Jpeg;
        baseline.processing.demosaic = "none".into();
        let mut working = baseline.clone();
        working.metadata.marked = true;
        let input = format!(
            "<r:RDF xmlns:r='{RDF_NS}' xmlns:c='{CAMERA_RAW_NS}'><r:Description c:Clarity2012='15'><c:ToneCurvePV2012><r:Seq><r:li>0, 0</r:li><r:li>255, 255</r:li></r:Seq></c:ToneCurvePV2012></r:Description></r:RDF>"
        );
        let saved = plan(Some(input.as_bytes()), &baseline, &working).unwrap().unwrap();
        assert_eq!(load_native(&saved).unwrap(), Some(working.clone()));
        let text = String::from_utf8(saved).unwrap();
        for changed in [text.replace("c:Clarity2012='15'", "c:Clarity2012='25'"), text.replace("255, 255", "255, 240")] {
            assert!(load_native(changed.as_bytes()).unwrap_err().contains("Rendered XMP properties changed"));
            assert!(plan(Some(changed.as_bytes()), &working, &baseline).is_err());
        }
    }

    #[test]
    fn external_has_settings_disable_cannot_reactivate_stale_native_adjustments() {
        let baseline = super::super::tests::recipe(0.);
        let working = super::super::tests::recipe(1.);
        let saved = plan(None, &baseline, &working).unwrap().unwrap();
        assert_eq!(XmpPacket::parse(&saved).unwrap().camera_raw("HasSettings"), ValueState::Text("True"));
        let changed = patch(&saved, &BTreeMap::from([(ExpandedName::new(CAMERA_RAW_NS, "HasSettings"), "False".into())])).unwrap();
        assert!(load_native(&changed).unwrap_err().contains("changed after"));
        assert!(plan(Some(&changed), &working, &baseline).is_err());
    }

    #[test]
    fn enabling_an_inactive_packet_does_not_reactivate_ignored_tonal_values() {
        let baseline = super::super::tests::recipe(0.);
        let mut working = baseline.clone();
        working.adjustments.clarity = 15.;
        let input = format!("<r:RDF xmlns:r=\"{RDF_NS}\" xmlns:c=\"{CAMERA_RAW_NS}\"><r:Description c:HasSettings=\"False\" c:Exposure2012=\"2\"/></r:RDF>");
        let saved = plan(Some(input.as_bytes()), &baseline, &working).unwrap().unwrap();
        let parsed = XmpPacket::parse(&saved).unwrap();
        assert_eq!(parsed.camera_raw("HasSettings"), ValueState::Text("True"));
        assert_eq!(parsed.camera_raw("Exposure2012"), ValueState::Text("0"));
        assert_eq!(load_native(&saved).unwrap(), Some(working));
    }

    #[test]
    fn original_schema_without_enable_provenance_only_accepts_an_absent_flag() {
        let baseline = super::super::tests::recipe(0.);
        let mut working = baseline.clone();
        working.metadata.marked = true;
        let saved = plan(None, &baseline, &working).unwrap().unwrap();
        let parsed = XmpPacket::parse(&saved).unwrap();
        let ValueState::Text(payload) = parsed.property(NATIVE_NS, NATIVE_FIELD) else { panic!("Missing recipe") };
        let mut legacy: serde_json::Value = serde_json::from_str(payload).unwrap();
        legacy["projection"].as_object_mut().unwrap().remove("HasSettings");
        let legacy = patch(&saved, &BTreeMap::from([(ExpandedName::new(NATIVE_NS, NATIVE_FIELD), legacy.to_string())])).unwrap();
        assert_eq!(load_native(&legacy).unwrap(), Some(working));
        let changed = patch(&legacy, &BTreeMap::from([(ExpandedName::new(CAMERA_RAW_NS, "HasSettings"), "False".into())])).unwrap();
        assert!(load_native(&changed).is_err());
    }
    #[test]
    fn no_op_and_unsafe_packets_never_produce_output() {
        let baseline = super::super::tests::recipe(0.);
        assert!(plan(None, &baseline, &baseline).unwrap().is_none());
        let working = super::super::tests::recipe(1.);
        for bytes in [b"".as_slice(), b"\xff", b"<!DOCTYPE x><x/>"] {
            assert!(plan(Some(bytes), &baseline, &working).is_err());
        }
    }
}
