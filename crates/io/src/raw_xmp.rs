//! Read-only Adobe Camera Raw `.xmp` sidecar settings for the real RAW opener.
//!
//! PR02-A3 consumes the independently bounded `photocraft-xmp` parser at runtime;
//! this module ONLY maps the six supported active scalar properties into the
//! existing PhotoCraft Camera Raw parameters. There is no second RDF/XML parser.
//! Snapshots, masks, crops, absolute Kelvin, profiles and tone curves are never
//! interpreted as these six sliders. This is metadata import, NOT Adobe pixel
//! equivalence, and no XMP or RAW file is written here.

use photocraft_algo::camera_raw::CameraRaw;
use photocraft_xmp::{CAMERA_RAW_NS, ValueState, XMP_NS, XmpError, XmpPacket};

#[cfg(test)]
const CRS_NS: &str = CAMERA_RAW_NS;

/// Keep the native bounded file reader and the parser on the same limit.
pub const MAX_XMP_BYTES: usize = photocraft_xmp::MAX_XMP_BYTES;

/// The six explicitly supported Adobe Camera Raw properties, in display order.
const FIELDS: [(&str, f32, f32); 6] = [
    ("Exposure2012", -5.0, 5.0),
    ("Contrast2012", -100.0, 100.0),
    ("Highlights2012", -100.0, 100.0),
    ("Shadows2012", -100.0, 100.0),
    ("Whites2012", -100.0, 100.0),
    ("Blacks2012", -100.0, 100.0),
];

/// Settings recognised from the current edit. All other XMP fields are not applied.
#[derive(Clone, Debug, PartialEq)]
pub struct XmpImport {
    pub params: CameraRaw,
    /// Exact Adobe property names applied to `params`.
    pub applied: Vec<&'static str>,
    /// Number of unmodelled Camera Raw properties in active RDF descriptions.
    pub unsupported_count: usize,
}

/// Import errors are non-fatal to opening the RAW; the UI shows a skip notice.
#[derive(Debug, thiserror::Error)]
pub enum XmpImportError {
    #[error("XMP packet exceeds the allowed size limit")]
    Oversized,
    #[error("XMP DTD and entity declarations are not supported")]
    ForbiddenMarkup,
    #[error("XMP is not UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("invalid XMP XML: {0}")]
    Xml(String),
    #[error("invalid or out-of-range Adobe XMP value for {field}")]
    InvalidValue { field: &'static str },
    #[error("conflicting active Adobe XMP values for {0}")]
    ConflictingValue(&'static str),
    #[error("structured or resource-valued Adobe XMP property cannot be mapped to {field}")]
    StructuredValue { field: &'static str },
    #[error("invalid or structured Adobe XMP HasSettings flag")]
    InvalidSettingsFlag,
    #[error("XMP packet has no description of the current resource")]
    NoCurrentDescription,
}

/// Strict, bounded read-only conversion of supported active CRS properties.
///
/// The same `XmpPacket` parse used by the XMP inspection CLI is now used at
/// runtime by `photocraft-ui-egui` through this function. Both XML attributes
/// and simple child elements are accepted. Structured values fail closed,
/// rather than silently becoming zeros. Missing/disabled properties retain
/// the PhotoCraft defaults; explicit zero remains an explicitly applied value.
///
/// This function performs no filesystem I/O and never persists the result.
pub fn import_adobe_raw_settings(bytes: &[u8]) -> Result<XmpImport, XmpImportError> {
    if bytes.len() > MAX_XMP_BYTES {
        return Err(XmpImportError::Oversized);
    }
    // Preserve the existing concrete UTF-8 error variant for callers/tests.
    std::str::from_utf8(bytes)?;
    let packet = XmpPacket::parse(bytes).map_err(map_xmp_error)?;

    // Always validate supported values, including when HasSettings=False.
    // No malformed scalar can be silently suppressed by a disabled flag.
    let mut values = [None; 6];
    for (index, (name, min, max)) in FIELDS.iter().copied().enumerate() {
        match packet.camera_raw(name) {
            ValueState::Absent => {}
            ValueState::Text(raw) => {
                let value = raw.trim().parse::<f32>().map_err(|_| XmpImportError::InvalidValue { field: name })?;
                if !value.is_finite() || value < min || value > max {
                    return Err(XmpImportError::InvalidValue { field: name });
                }
                values[index] = Some(value);
            }
            ValueState::Resource(_) | ValueState::Opaque => {
                return Err(XmpImportError::StructuredValue { field: name });
            }
        }
    }

    let enabled = match packet.camera_raw("HasSettings") {
        ValueState::Absent => true,
        ValueState::Text(raw) if raw.eq_ignore_ascii_case("true") => true,
        ValueState::Text(raw) if raw.eq_ignore_ascii_case("false") => false,
        ValueState::Text(_) | ValueState::Resource(_) | ValueState::Opaque => {
            return Err(XmpImportError::InvalidSettingsFlag);
        }
    };

    let unsupported_count = packet
        .active_properties()
        .keys()
        .filter(|name| {
            name.namespace_uri == CAMERA_RAW_NS && name.local_name != "HasSettings" && !FIELDS.iter().any(|(known, _, _)| *known == name.local_name.as_str())
        })
        .count();

    let mut params = CameraRaw::default();
    let mut applied = Vec::new();
    if enabled {
        for (index, value) in values.iter().enumerate() {
            if let Some(value) = value {
                match index {
                    0 => params.exposure = *value,
                    1 => params.contrast = *value,
                    2 => params.highlights = *value,
                    3 => params.shadows = *value,
                    4 => params.whites = *value,
                    5 => params.blacks = *value,
                    _ => continue,
                }
                applied.push(FIELDS[index].0);
            }
        }
    }
    Ok(XmpImport { params, applied, unsupported_count })
}

/// The photograph's star rating recorded in a sidecar (`xmp:Rating` of the active description), read and never written
/// (GRW-1 lab, Filmstrip addendum of 15 Oct: display only). `Ok(None)`: the packet has no rating, so the photograph is
/// unrated. `Ok(Some(-1))`: rejected. `Ok(Some(1..=5))`: stars (`0` is unrated, `None`). Anything else (an unreadable packet,
/// a non-integer, out-of-range or structured value) is an error, which the caller shows as "rating unknown", never as 0.
pub fn read_rating(bytes: &[u8]) -> Result<Option<i8>, XmpImportError> {
    const FIELD: &str = "Rating";
    if bytes.len() > MAX_XMP_BYTES {
        return Err(XmpImportError::Oversized);
    }
    std::str::from_utf8(bytes)?;
    let packet = XmpPacket::parse(bytes).map_err(map_xmp_error)?;
    // Only a description of the current resource (`rdf:about=""`) speaks for the photograph: without one the rating is
    // not known, never "unrated".
    if packet.active_description_count() == 0 {
        return Err(XmpImportError::NoCurrentDescription);
    }
    match packet.property(XMP_NS, FIELD) {
        ValueState::Absent => Ok(None),
        ValueState::Text(raw) => {
            let value: f32 = raw.trim().parse().map_err(|_| XmpImportError::InvalidValue { field: FIELD })?;
            if !value.is_finite() || value.fract() != 0.0 || !(-1.0..=5.0).contains(&value) {
                return Err(XmpImportError::InvalidValue { field: FIELD });
            }
            // Exact: an integral value in -1..=5.
            let stars = value as i8;
            Ok((stars != 0).then_some(stars))
        }
        ValueState::Resource(_) | ValueState::Opaque => Err(XmpImportError::StructuredValue { field: FIELD }),
    }
}

/// Preserve the original public error categories where a failure can be
/// characterised precisely. All other parser errors remain fail-closed.
fn map_xmp_error(error: XmpError) -> XmpImportError {
    match &error {
        XmpError::DuplicateActiveProperty { namespace_uri, local_name } if namespace_uri == CAMERA_RAW_NS => {
            if let Some((name, _, _)) = FIELDS.iter().copied().find(|(name, _, _)| *name == local_name.as_str()) {
                return XmpImportError::ConflictingValue(name);
            }
        }
        XmpError::Unsupported(message) if message.contains("DOCTYPE") => {
            return XmpImportError::ForbiddenMarkup;
        }
        _ => {}
    }
    XmpImportError::Xml(error.to_string())
}

/// Read the current resource's literal label for the in-memory workspace.
/// The complete session service takes ownership of metadata in the finish feature.
pub fn read_workspace_label(bytes: &[u8]) -> Option<String> {
    let packet = XmpPacket::parse(bytes).ok()?;
    match packet.property(XMP_NS, "Label") {
        ValueState::Text(label) => Some(label.to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:RDF>"#;
    const FOOTER: &str = "</rdf:RDF></x:xmpmeta>";

    fn packet(active: &str, nested: &str) -> String {
        format!("{HEADER}<rdf:Description rdf:about=\"\" xmlns:crs=\"{CRS_NS}\" {active}>{nested}</rdf:Description>{FOOTER}")
    }

    #[test]
    fn imports_six_active_fields_not_nested_import_snapshot() {
        let nested = "<crss:SavedSettings xmlns:crss=\"http://ns.adobe.com/camera-raw-saved-settings/1.0/\">\
                <rdf:Bag><rdf:li><rdf:Description crs:Exposure2012=\"0\" crs:Contrast2012=\"0\"/></rdf:li></rdf:Bag>\
             </crss:SavedSettings>";
        let xml = packet(
            "crs:Exposure2012=\"0.75\" crs:Contrast2012=\"18\" crs:Highlights2012=\"-35\" \
             crs:Shadows2012=\"12\" crs:Whites2012=\"5\" crs:Blacks2012=\"-8\" crs:Temperature=\"6100\"",
            nested,
        );
        let got = import_adobe_raw_settings(xml.as_bytes()).unwrap();
        assert!((got.params.exposure - 0.75).abs() < 0.00001);
        assert_eq!((got.params.contrast, got.params.highlights, got.params.shadows, got.params.whites, got.params.blacks), (18.0, -35.0, 12.0, 5.0, -8.0));
        assert_eq!(got.applied.len(), 6);
        assert_eq!(got.unsupported_count, 1);
        assert_eq!((got.params.temperature, got.params.tint), (0.0, 0.0));
    }

    #[test]
    fn a_rating_is_read_from_the_active_description_only_and_never_invented() {
        let rated = |attrs: &str| format!("{HEADER}<rdf:Description rdf:about=\"\" xmlns:xmp=\"{XMP_NS}\" {attrs}/>{FOOTER}");
        assert_eq!(read_rating(rated("xmp:Rating=\"3\"").as_bytes()).unwrap(), Some(3));
        assert_eq!(read_rating(rated("xmp:Rating=\"5.0\"").as_bytes()).unwrap(), Some(5));
        assert_eq!(read_rating(rated("xmp:Rating=\"-1\"").as_bytes()).unwrap(), Some(-1), "rejected");
        assert_eq!(read_rating(rated("xmp:Rating=\"0\"").as_bytes()).unwrap(), None, "0 is unrated");
        assert_eq!(read_rating(rated("").as_bytes()).unwrap(), None, "no rating: unrated");
        for bad in ["6", "-2", "2.5", "three", "NaN"] {
            assert!(read_rating(rated(&format!("xmp:Rating=\"{bad}\"")).as_bytes()).is_err(), "{bad} is not a rating");
        }
        // A rating in a saved snapshot (nested) is not the photograph's current rating.
        let nested = format!(
            "{HEADER}<rdf:Description rdf:about=\"\" xmlns:xmp=\"{XMP_NS}\" xmlns:crss=\"http://ns.adobe.com/camera-raw-saved-settings/1.0/\">\
             <crss:SavedSettings><rdf:Bag><rdf:li><rdf:Description xmp:Rating=\"4\"/></rdf:li></rdf:Bag></crss:SavedSettings></rdf:Description>{FOOTER}"
        );
        assert_eq!(read_rating(nested.as_bytes()).unwrap(), None);
        assert!(read_rating(b"<not xml").is_err());
        assert!(read_rating(&vec![b' '; MAX_XMP_BYTES + 1]).is_err(), "oversized");
    }

    /// C5-R1 (final cross-examination R1): a packet whose only Description is about something else (the older
    /// `rdf:about="uuid:…"` form) or that has none says nothing about this photograph's rating: not read, never unrated.
    #[test]
    fn a_packet_without_a_current_description_is_not_read_as_unrated() {
        let about =
            format!("{HEADER}<rdf:Description rdf:about=\"uuid:faf5bdd5-ba3d-11da-ad31-d33d75182f1b\" xmlns:xmp=\"{XMP_NS}\" xmp:Rating=\"4\"/>{FOOTER}");
        assert!(read_rating(about.as_bytes()).is_err(), "a description about another resource");
        assert!(read_rating(format!("{HEADER}{FOOTER}").as_bytes()).is_err(), "no description at all");
        let current = format!("{HEADER}<rdf:Description rdf:about=\"\" xmlns:xmp=\"{XMP_NS}\"/>{FOOTER}");
        assert_eq!(read_rating(current.as_bytes()).unwrap(), None, "a current description without a rating is unrated");
    }

    #[test]
    fn namespace_uri_matters_not_prefix() {
        let xml = format!("{HEADER}<rdf:Description xmlns:edit=\"{CRS_NS}\" edit:Exposure2012=\"1.25\"/>{FOOTER}");
        let got = import_adobe_raw_settings(xml.as_bytes()).unwrap();
        assert_eq!(got.params.exposure, 1.25);
        assert_eq!(got.applied, vec!["Exposure2012"]);
    }

    #[test]
    fn has_settings_false_and_missing_fields_leave_defaults() {
        let xml = packet("crs:HasSettings=\"False\" crs:Exposure2012=\"3\"", "");
        let got = import_adobe_raw_settings(xml.as_bytes()).unwrap();
        assert!(got.applied.is_empty());
        assert_eq!(got.params, CameraRaw::default());
        let missing = import_adobe_raw_settings(packet("", "").as_bytes()).unwrap();
        assert!(missing.applied.is_empty());
    }

    #[test]
    fn conflicting_top_level_values_are_rejected() {
        let xml = format!(
            "{HEADER}<rdf:Description xmlns:a=\"{CRS_NS}\" a:Exposure2012=\"1\"/>\
             <rdf:Description xmlns:b=\"{CRS_NS}\" b:Exposure2012=\"2\"/>{FOOTER}"
        );
        assert!(matches!(import_adobe_raw_settings(xml.as_bytes()), Err(XmpImportError::ConflictingValue("Exposure2012"))));
    }

    #[test]
    fn rejects_malformed_untrusted_and_out_of_range_packets() {
        let samples = [
            "<rdf:RDF>".to_owned(),
            "<!DOCTYPE foo [<!ENTITY x \"hi\">]><x:xmpmeta/>".to_owned(),
            packet("crs:Exposure2012=\"NaN\"", ""),
            packet("crs:Exposure2012=\"6\"", ""),
            packet("crs:Highlights2012=\"Infinity\"", ""),
            packet("crs:Blacks2012=\"-101\"", ""),
            packet("crs:Contrast2012=\"oops\"", ""),
        ];
        for xml in samples {
            assert!(import_adobe_raw_settings(xml.as_bytes()).is_err(), "accepted {xml}");
        }
        assert!(matches!(import_adobe_raw_settings(&[0xFF, 0xFF]), Err(XmpImportError::Utf8(_))));
        assert!(matches!(import_adobe_raw_settings(&vec![b'x'; MAX_XMP_BYTES + 1]), Err(XmpImportError::Oversized)));
    }

    #[test]
    fn historical_snapshots_cannot_become_current_edits() {
        let xml = packet("", "<rdf:Bag><rdf:li><rdf:Description crs:Exposure2012=\"4\"/></rdf:li></rdf:Bag>");
        let got = import_adobe_raw_settings(xml.as_bytes()).unwrap();
        assert!(got.applied.is_empty());
        assert_eq!(got.params.exposure, 0.0);
    }

    #[test]
    fn simple_child_properties_use_the_same_runtime_parser() {
        let xml = format!(
            "{HEADER}<rdf:Description xmlns:crs=\"{CRS_NS}\">\
             <crs:Exposure2012>0.625</crs:Exposure2012>\
             <crs:Contrast2012> 12 </crs:Contrast2012>\
             </rdf:Description>{FOOTER}"
        );
        let got = import_adobe_raw_settings(xml.as_bytes()).unwrap();
        assert_eq!(got.params.exposure, 0.625);
        assert_eq!(got.params.contrast, 12.0);
        assert_eq!(got.applied, vec!["Exposure2012", "Contrast2012"]);
    }

    #[test]
    fn opaque_or_resource_tonal_values_fail_closed() {
        let resource = format!(
            "{HEADER}<rdf:Description xmlns:crs=\"{CRS_NS}\">\
             <crs:Exposure2012 rdf:resource=\"urn:raw:nonnumeric\"/>\
             </rdf:Description>{FOOTER}"
        );
        assert!(matches!(import_adobe_raw_settings(resource.as_bytes()), Err(XmpImportError::StructuredValue { field: "Exposure2012" })));

        let curve = format!(
            "{HEADER}<rdf:Description xmlns:crs=\"{CRS_NS}\">\
             <crs:Contrast2012><rdf:Seq><rdf:li>10</rdf:li></rdf:Seq></crs:Contrast2012>\
             </rdf:Description>{FOOTER}"
        );
        assert!(matches!(import_adobe_raw_settings(curve.as_bytes()), Err(XmpImportError::StructuredValue { field: "Contrast2012" })));
    }

    #[test]
    fn unknown_complex_fields_are_not_mistaken_for_sliders() {
        let xml =
            packet("crs:Exposure2012=\"0\" crs:Temperature=\"6100\"", "<crs:ToneCurvePV2012><rdf:Seq><rdf:li>0,0</rdf:li></rdf:Seq></crs:ToneCurvePV2012>");
        let got = import_adobe_raw_settings(xml.as_bytes()).unwrap();
        assert_eq!(got.applied, vec!["Exposure2012"]);
        assert_eq!(got.params.exposure, 0.0);
        assert_eq!(got.unsupported_count, 2);
    }

    #[test]
    fn malformed_settings_flag_and_disabled_invalid_numbers_are_rejected() {
        assert!(matches!(import_adobe_raw_settings(packet("crs:HasSettings=\"Maybe\"", "").as_bytes()), Err(XmpImportError::InvalidSettingsFlag)));
        assert!(matches!(
            import_adobe_raw_settings(packet("crs:HasSettings=\"False\" crs:Exposure2012=\"8\"", "").as_bytes()),
            Err(XmpImportError::InvalidValue { field: "Exposure2012" })
        ));
        let structured = format!(
            "{HEADER}<rdf:Description xmlns:crs=\"{CRS_NS}\">\
             <crs:HasSettings rdf:resource=\"urn:unknown\"/>\
             </rdf:Description>{FOOTER}"
        );
        assert!(matches!(import_adobe_raw_settings(structured.as_bytes()), Err(XmpImportError::InvalidSettingsFlag)));
    }

    #[test]
    fn conflicting_non_tonal_active_metadata_is_not_silently_chosen() {
        let xml = format!(
            "{HEADER}<rdf:Description xmlns:crs=\"{CRS_NS}\" crs:ProcessVersion=\"9.0\"/>\
             <rdf:Description xmlns:crs=\"{CRS_NS}\" crs:ProcessVersion=\"11.0\"/>{FOOTER}"
        );
        assert!(matches!(import_adobe_raw_settings(xml.as_bytes()), Err(XmpImportError::Xml(_))));
    }
}
