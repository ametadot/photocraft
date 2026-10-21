//! PhotoCraft XMP read-only core. This crate never opens or writes photo files.
//!
//! An intentionally bounded RDF/XML subset: active root descriptions, simple
//! properties, opaque structured properties, and saved snapshot discovery.
//! It is NOT a general-purpose semantic RDF equivalence engine and NEVER
//! silently interprets complex property trees as scalars.
//!
//! Source: public XMP / RDF/XML specifications and independently supplied
//! XMP fixtures. No Adobe product code, SDK code, or third-party parser code
//! is copied into this implementation.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

pub const CAMERA_RAW_NS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
pub const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
pub const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub const CRSS_NS: &str = "http://ns.adobe.com/camera-raw-saved-settings/1.0/";
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Non-negotiable limits against hostile or accidental oversized metadata.
pub const MAX_XMP_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_EVENTS: usize = 200_000;
const MAX_ATTRIBUTES_PER_TAG: usize = 2_048;
const MAX_ACTIVE_PROPERTIES: usize = 20_000;
const MAX_SCALAR_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XmpError {
    TooLarge { length: usize, limit: usize },
    InvalidUtf8,
    InvalidXml(String),
    Unsupported(String),
    DuplicateActiveProperty { namespace_uri: String, local_name: String },
}

impl fmt::Display for XmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { length, limit } => write!(f, "XMP resource too large: {length} (limit {limit})"),
            Self::InvalidUtf8 => write!(f, "XMP is not valid UTF-8"),
            Self::InvalidXml(detail) => write!(f, "invalid XMP XML: {detail}"),
            Self::Unsupported(detail) => write!(f, "unsupported XMP construct: {detail}"),
            Self::DuplicateActiveProperty { namespace_uri, local_name } => {
                write!(f, "conflicting active XMP property: {{{namespace_uri}}}{local_name}")
            }
        }
    }
}

impl std::error::Error for XmpError {}

/// Namespace URI (not prefix) and local name uniquely identify an XMP property.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExpandedName {
    pub namespace_uri: String,
    pub local_name: String,
}

impl ExpandedName {
    pub fn new(namespace_uri: impl Into<String>, local_name: impl Into<String>) -> Self {
        Self { namespace_uri: namespace_uri.into(), local_name: local_name.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XmpValue {
    /// Entity-decoded but otherwise unrounded value. "0" differs from absence.
    Text(String),
    /// URI-style RDF resource; not assumed to be a Camera Raw slider.
    Resource(String),
    /// Complex RDF/XML retained solely as bytes at the provided source range.
    /// The source packet is retained intact. Not a semantic RDF parse.
    Opaque { xml_span: Range<usize> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueState<'a> {
    Absent,
    Text(&'a str),
    Resource(&'a str),
    Opaque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct XmpPacket {
    /// Exact input bytes, including xpacket processing instructions and padding.
    original: Vec<u8>,
    active: BTreeMap<ExpandedName, XmpValue>,
    snapshots: Vec<Snapshot>,
    active_description_count: usize,
}

impl XmpPacket {
    /// Parse read-only. Never mutates the input bytes, filesystem or RAW image.
    /// The result can re-emit the source bytes verbatim, but has NO writer API.
    pub fn parse(input: &[u8]) -> Result<Self, XmpError> {
        if input.len() > MAX_XMP_BYTES {
            return Err(XmpError::TooLarge { length: input.len(), limit: MAX_XMP_BYTES });
        }
        let source_text = std::str::from_utf8(input).map_err(|_| XmpError::InvalidUtf8)?;
        if source_text.chars().any(|ch| !valid_xml_char(ch)) {
            return Err(XmpError::InvalidXml("prohibited XML control codepoint".into()));
        }
        let bom_len = if input.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
        let mut xml = Reader::from_reader(&input[bom_len..]);
        xml.config_mut().trim_text(false);
        let mut stack: Vec<Frame> = Vec::new();
        let mut result = Self { original: input.to_vec(), active: BTreeMap::new(), snapshots: Vec::new(), active_description_count: 0 };
        let mut prev_pos = bom_len;
        let mut saw_rdf = false;
        let mut roots = 0usize;
        let mut events = 0usize;

        loop {
            events += 1;
            if events > MAX_EVENTS {
                return Err(XmpError::TooLarge { length: events, limit: MAX_EVENTS });
            }
            let event = xml.read_event().map_err(|e| XmpError::InvalidXml(e.to_string()))?;
            let after = (xml.buffer_position() as usize).saturating_add(bom_len);
            match event {
                Event::Start(start) => {
                    parse_start(&mut result, &mut stack, &mut saw_rdf, &mut roots, &start, prev_pos, after)?;
                }
                Event::Empty(start) => {
                    parse_start(&mut result, &mut stack, &mut saw_rdf, &mut roots, &start, prev_pos, after)?;
                    finish_element(&mut result, &mut stack, after, None)?;
                }
                Event::End(end) => {
                    let qname = str_from_bytes(end.name().as_ref())?.to_owned();
                    finish_element(&mut result, &mut stack, after, Some(&qname))?;
                }
                Event::Text(text) => {
                    let decoded = decode_entities(str_from_bytes(text.as_ref())?)?;
                    append_text(&mut stack, &decoded)?;
                    if stack.is_empty() && !decoded.trim().is_empty() {
                        return Err(XmpError::InvalidXml("non-whitespace text outside the document element".into()));
                    }
                }
                Event::CData(text) => {
                    let decoded = str_from_bytes(text.as_ref())?;
                    append_text(&mut stack, decoded)?;
                    if stack.is_empty() && !decoded.trim().is_empty() {
                        return Err(XmpError::InvalidXml("CDATA outside document element".into()));
                    }
                }
                Event::GeneralRef(reference) => {
                    // quick-xml 0.38+ emits XML text entities as distinct events.
                    // Never resolve DTD-defined or external entities.
                    let entity = str_from_bytes(reference.as_ref())?;
                    let decoded = decode_entities(&format!("&{entity};"))?;
                    append_text(&mut stack, &decoded)?;
                    if stack.is_empty() && !decoded.trim().is_empty() {
                        return Err(XmpError::InvalidXml("entity reference outside document element".into()));
                    }
                }
                Event::DocType(_) => return Err(XmpError::Unsupported("DOCTYPE and external entities are prohibited".into())),
                Event::Eof => break,
                Event::Decl(decl) => {
                    if let Some(declared) = decl.encoding() {
                        let label = declared.map_err(|e| XmpError::InvalidXml(e.to_string()))?;
                        if !label.as_ref().eq_ignore_ascii_case(b"utf-8") {
                            return Err(XmpError::Unsupported("only UTF-8 XML declarations are supported".into()));
                        }
                    }
                }
                Event::PI(_) | Event::Comment(_) => {}
            }
            prev_pos = after;
        }
        if !stack.is_empty() {
            return Err(XmpError::InvalidXml("unclosed XML elements".into()));
        }
        if roots != 1 || !saw_rdf {
            return Err(XmpError::InvalidXml("expected one XML root containing rdf:RDF".into()));
        }
        Ok(result)
    }

    pub fn original_bytes(&self) -> &[u8] {
        &self.original
    }
    pub fn active_properties(&self) -> &BTreeMap<ExpandedName, XmpValue> {
        &self.active
    }
    pub fn snapshots(&self) -> &[Snapshot] {
        &self.snapshots
    }
    pub fn active_description_count(&self) -> usize {
        self.active_description_count
    }

    /// Returns Absence independently of an explicitly stored string "0".
    pub fn property(&self, namespace_uri: &str, local_name: &str) -> ValueState<'_> {
        match self.active.get(&ExpandedName::new(namespace_uri, local_name)) {
            None => ValueState::Absent,
            Some(XmpValue::Text(value)) => ValueState::Text(value),
            Some(XmpValue::Resource(value)) => ValueState::Resource(value),
            Some(XmpValue::Opaque { .. }) => ValueState::Opaque,
        }
    }

    pub fn camera_raw(&self, local_name: &str) -> ValueState<'_> {
        self.property(CAMERA_RAW_NS, local_name)
    }
}

#[derive(Debug)]
struct PendingProperty {
    text: String,
    resource: Option<String>,
    complex: bool,
    xml_start: usize,
}

#[derive(Debug)]
struct Frame {
    qname: String,
    expanded: ExpandedName,
    namespaces: BTreeMap<String, String>,
    is_active_description: bool,
    within_saved_settings: bool,
    pending: Option<PendingProperty>,
}

fn parse_start(
    packet: &mut XmpPacket,
    stack: &mut Vec<Frame>,
    saw_rdf: &mut bool,
    roots: &mut usize,
    start: &BytesStart<'_>,
    raw_start: usize,
    raw_end: usize,
) -> Result<(), XmpError> {
    if stack.len() >= MAX_DEPTH {
        return Err(XmpError::TooLarge { length: stack.len() + 1, limit: MAX_DEPTH });
    }
    let qname = str_from_bytes(start.name().as_ref())?.to_owned();
    let attributes = parse_attributes(start)?;
    let mut namespaces = stack.last().map(|p| p.namespaces.clone()).unwrap_or_else(|| {
        let mut initial = BTreeMap::new();
        initial.insert("xml".into(), XML_NS.into());
        initial
    });
    for (key, value) in &attributes {
        if key == "xmlns" {
            namespaces.insert(String::new(), value.clone());
        } else if let Some(prefix) = key.strip_prefix("xmlns:") {
            if prefix.is_empty() || prefix == "xmlns" || (prefix == "xml" && value != XML_NS) {
                return Err(XmpError::InvalidXml("reserved or empty namespace declaration".into()));
            }
            namespaces.insert(prefix.to_owned(), value.clone());
        }
    }
    let expanded = expand_name(&qname, &namespaces, false)?;
    if stack.is_empty() {
        *roots += 1;
    }
    if expanded.namespace_uri == RDF_NS && expanded.local_name == "RDF" {
        *saw_rdf = true;
    }
    if let Some(parent) = stack.last_mut()
        && let Some(ref mut pending) = parent.pending
    {
        pending.complex = true;
    }
    let attrs = expanded_attributes(&attributes, &namespaces)?;
    let is_root_description = expanded.namespace_uri == RDF_NS
        && expanded.local_name == "Description"
        && stack.last().is_some_and(|f| f.expanded.namespace_uri == RDF_NS && f.expanded.local_name == "RDF");
    let about = attrs.iter().find(|(name, _)| name.namespace_uri == RDF_NS && name.local_name == "about").map(|(_, value)| value.as_str());
    let active_description = is_root_description && about.is_none_or(str::is_empty);
    if active_description {
        packet.active_description_count += 1;
        for (name, value) in &attrs {
            if name.namespace_uri.is_empty() || name.namespace_uri == RDF_NS || name.namespace_uri == XML_NS || name.namespace_uri == XMLNS_NS {
                continue;
            }
            store_active(packet, name.clone(), XmpValue::Text(value.clone()))?;
        }
    }
    let within_saved_settings =
        (expanded.namespace_uri == CRSS_NS && expanded.local_name == "SavedSettings") || stack.last().is_some_and(|f| f.within_saved_settings);
    if within_saved_settings && expanded.namespace_uri == RDF_NS && expanded.local_name == "Description" {
        let snapshot_type = attrs.iter().find(|(name, _)| name.namespace_uri == CRSS_NS && name.local_name == "Type").map(|(_, value)| value.as_str());
        if snapshot_type == Some("Snapshot") {
            let name = attrs.iter().find(|(name, _)| name.namespace_uri == CRSS_NS && name.local_name == "Name").map(|(_, value)| value.clone());
            packet.snapshots.push(Snapshot { name });
        }
    }
    let pending = if stack.last().is_some_and(|frame| frame.is_active_description) {
        let rdf_resource = attrs.iter().find(|(name, _)| name.namespace_uri == RDF_NS && name.local_name == "resource").map(|(_, value)| value.clone());
        let other_attrs = attrs.iter().any(|(name, _)| name.namespace_uri != RDF_NS || (name.local_name != "resource" && name.local_name != "datatype"));
        Some(PendingProperty { text: String::new(), resource: rdf_resource, complex: other_attrs, xml_start: raw_start })
    } else {
        None
    };
    stack.push(Frame { qname, expanded, namespaces, is_active_description: active_description, within_saved_settings, pending });
    if raw_end > packet.original.len() {
        return Err(XmpError::InvalidXml("XML event exceeds input bounds".into()));
    }
    Ok(())
}

fn finish_element(packet: &mut XmpPacket, stack: &mut Vec<Frame>, raw_end: usize, end_qname: Option<&str>) -> Result<(), XmpError> {
    let frame = stack.pop().ok_or_else(|| XmpError::InvalidXml("unexpected closing element".into()))?;
    if end_qname.is_some_and(|name| name != frame.qname.as_str()) {
        return Err(XmpError::InvalidXml(format!("mismatched closing element: {}", frame.qname)));
    }
    if let Some(pending) = frame.pending {
        let value = if pending.complex || (pending.resource.is_some() && !pending.text.trim().is_empty()) {
            XmpValue::Opaque { xml_span: pending.xml_start..raw_end }
        } else if let Some(resource) = pending.resource {
            XmpValue::Resource(resource)
        } else {
            XmpValue::Text(pending.text)
        };
        store_active(packet, frame.expanded, value)?;
    }
    Ok(())
}

fn store_active(packet: &mut XmpPacket, name: ExpandedName, value: XmpValue) -> Result<(), XmpError> {
    if packet.active.len() >= MAX_ACTIVE_PROPERTIES {
        return Err(XmpError::TooLarge { length: packet.active.len() + 1, limit: MAX_ACTIVE_PROPERTIES });
    }
    if let Some(existing) = packet.active.get(&name) {
        if existing == &value {
            return Ok(());
        }
        return Err(XmpError::DuplicateActiveProperty { namespace_uri: name.namespace_uri, local_name: name.local_name });
    }
    packet.active.insert(name, value);
    Ok(())
}

fn append_text(stack: &mut [Frame], text: &str) -> Result<(), XmpError> {
    if let Some(frame) = stack.last_mut()
        && let Some(pending) = frame.pending.as_mut()
    {
        if text.len() > MAX_SCALAR_BYTES.saturating_sub(pending.text.len()) {
            return Err(XmpError::TooLarge { length: pending.text.len().saturating_add(text.len()), limit: MAX_SCALAR_BYTES });
        }
        pending.text.push_str(text);
    }
    Ok(())
}

fn parse_attributes(start: &BytesStart<'_>) -> Result<Vec<(String, String)>, XmpError> {
    let mut result = Vec::new();
    for attr in start.attributes().with_checks(true) {
        if result.len() >= MAX_ATTRIBUTES_PER_TAG {
            return Err(XmpError::TooLarge { length: result.len() + 1, limit: MAX_ATTRIBUTES_PER_TAG });
        }
        let attr = attr.map_err(|err| XmpError::InvalidXml(err.to_string()))?;
        let key = str_from_bytes(attr.key.as_ref())?.to_owned();
        let value = decode_entities(str_from_bytes(attr.value.as_ref())?)?;
        result.push((key, value));
    }
    Ok(result)
}

fn expanded_attributes(attributes: &[(String, String)], namespaces: &BTreeMap<String, String>) -> Result<Vec<(ExpandedName, String)>, XmpError> {
    let mut result = Vec::with_capacity(attributes.len());
    let mut expanded_seen = BTreeSet::new();
    for (name, value) in attributes {
        if name == "xmlns" || name.starts_with("xmlns:") {
            continue;
        }
        let expanded = expand_name(name, namespaces, true)?;
        // XML prohibits attributes that have the same expanded name, even if
        // different prefixes conceal the duplicate and values are identical.
        if !expanded_seen.insert(expanded.clone()) {
            return Err(XmpError::InvalidXml(format!("duplicate expanded XML attribute: {{{}}}{}", expanded.namespace_uri, expanded.local_name,)));
        }
        result.push((expanded, value.clone()));
    }
    Ok(result)
}

fn expand_name(raw: &str, namespaces: &BTreeMap<String, String>, is_attribute: bool) -> Result<ExpandedName, XmpError> {
    if raw.is_empty() {
        return Err(XmpError::InvalidXml("empty XML name".into()));
    }
    if let Some((prefix, local)) = raw.split_once(':') {
        if prefix.is_empty() || local.is_empty() || local.contains(':') {
            return Err(XmpError::InvalidXml("invalid qualified XML name".into()));
        }
        let uri = namespaces.get(prefix).ok_or_else(|| XmpError::InvalidXml(format!("undeclared XML prefix: {prefix}")))?;
        if uri.is_empty() {
            return Err(XmpError::InvalidXml(format!("XML prefix bound to empty URI: {prefix}")));
        }
        return Ok(ExpandedName::new(uri.clone(), local));
    }
    let uri = if is_attribute { "" } else { namespaces.get("").map(String::as_str).unwrap_or("") };
    Ok(ExpandedName::new(uri, raw))
}

fn str_from_bytes(bytes: &[u8]) -> Result<&str, XmpError> {
    std::str::from_utf8(bytes).map_err(|_| XmpError::InvalidUtf8)
}

fn valid_xml_char(ch: char) -> bool {
    matches!(ch as u32, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

/// Deliberately no DTD expansion. Only predefined and numeric XML entities.
fn decode_entities(input: &str) -> Result<String, XmpError> {
    let mut out = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(amp) = remaining.find('&') {
        out.push_str(&remaining[..amp]);
        remaining = &remaining[amp + 1..];
        let semi = remaining.find(';').ok_or_else(|| XmpError::InvalidXml("unterminated XML entity".into()))?;
        let reference = &remaining[..semi];
        let decoded = match reference {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ if reference.starts_with("#x") => {
                let num = u32::from_str_radix(&reference[2..], 16).map_err(|_| XmpError::InvalidXml("invalid hex XML entity".into()))?;
                numeric_entity(num)?
            }
            _ if reference.starts_with('#') => {
                let num = reference[1..].parse::<u32>().map_err(|_| XmpError::InvalidXml("invalid decimal XML entity".into()))?;
                numeric_entity(num)?
            }
            _ => return Err(XmpError::Unsupported(format!("external or undefined XML entity: &{reference};"))),
        };
        out.push(decoded);
        remaining = &remaining[semi + 1..];
    }
    out.push_str(remaining);
    Ok(out)
}

fn numeric_entity(num: u32) -> Result<char, XmpError> {
    let ch = char::from_u32(num).ok_or_else(|| XmpError::InvalidXml("invalid Unicode XML character reference".into()))?;
    if !valid_xml_char(ch) {
        return Err(XmpError::InvalidXml("forbidden XML character reference".into()));
    }
    Ok(ch)
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const SIMPLE: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
      <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
       <rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
          crs:Exposure2012="0.7853281" crs:Contrast2012="32" crs:Blacks2012="-40"/>
      </rdf:RDF></x:xmpmeta>"#;

    #[test]
    fn exposure_and_absence_are_exact() -> TestResult {
        let parsed = XmpPacket::parse(SIMPLE.as_bytes())?;
        assert_eq!(parsed.camera_raw("Exposure2012"), ValueState::Text("0.7853281"));
        assert_eq!(parsed.camera_raw("Blacks2012"), ValueState::Text("-40"));
        assert_eq!(parsed.camera_raw("Highlights2012"), ValueState::Absent);
        assert_eq!(parsed.original_bytes(), SIMPLE.as_bytes());
        Ok(())
    }

    #[test]
    fn namespace_uri_not_prefix() -> TestResult {
        let xml = format!(r#"<rdf:RDF xmlns:rdf="{RDF_NS}"><rdf:Description rdf:about="" xmlns:c="{CAMERA_RAW_NS}" c:Exposure2012="0"/></rdf:RDF>"#);
        let parsed = XmpPacket::parse(xml.as_bytes())?;
        assert_eq!(parsed.camera_raw("Exposure2012"), ValueState::Text("0"));
        Ok(())
    }

    #[test]
    fn snapshots_never_override_active_values() -> TestResult {
        let xml = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}" xmlns:crss="{CRSS_NS}">
          <rdf:Description rdf:about="" crs:Exposure2012="0.7853281">
            <crss:SavedSettings><rdf:Bag><rdf:li><rdf:Description crss:Type="Snapshot" crss:Name="Import">
              <crss:Parameters><rdf:Description crs:Exposure2012="0" /></crss:Parameters>
            </rdf:Description></rdf:li></rdf:Bag></crss:SavedSettings>
          </rdf:Description></rdf:RDF>"#
        );
        let parsed = XmpPacket::parse(xml.as_bytes())?;
        assert_eq!(parsed.camera_raw("Exposure2012"), ValueState::Text("0.7853281"));
        assert_eq!(parsed.snapshots().len(), 1);
        assert_eq!(parsed.snapshots()[0].name.as_deref(), Some("Import"));
        assert_eq!(parsed.property(CRSS_NS, "SavedSettings"), ValueState::Opaque);
        Ok(())
    }

    #[test]
    fn child_property_text_and_entities() -> TestResult {
        let xml = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}"><rdf:Description>
          <crs:Exposure2012>0.5</crs:Exposure2012><crs:WhiteBalance>As &amp; Shot</crs:WhiteBalance>
          </rdf:Description></rdf:RDF>"#
        );
        let packet = XmpPacket::parse(xml.as_bytes())?;
        assert_eq!(packet.camera_raw("Exposure2012"), ValueState::Text("0.5"));
        assert_eq!(packet.camera_raw("WhiteBalance"), ValueState::Text("As & Shot"));
        Ok(())
    }

    #[test]
    fn curve_and_unknown_are_retained_verbatim() -> TestResult {
        let xml = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}" xmlns:Tp="http://ns.example.com/third-party/1.0/">
          <rdf:Description crs:Highlights2012="-84" Tp:TpCropAngle="-0.55"><crs:ToneCurvePV2012>
          <rdf:Bag><rdf:li>0, 0</rdf:li><rdf:li>255, 255</rdf:li></rdf:Bag>
          </crs:ToneCurvePV2012></rdf:Description></rdf:RDF>"#
        );
        let packet = XmpPacket::parse(xml.as_bytes())?;
        assert_eq!(packet.original_bytes(), xml.as_bytes());
        assert_eq!(packet.property("http://ns.example.com/third-party/1.0/", "TpCropAngle"), ValueState::Text("-0.55"));
        assert_eq!(packet.camera_raw("ToneCurvePV2012"), ValueState::Opaque);
        Ok(())
    }

    #[test]
    fn conflicts_cannot_be_silently_merged() {
        let xml = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}">
          <rdf:Description crs:Exposure2012="1"/><rdf:Description crs:Exposure2012="2"/></rdf:RDF>"#
        );
        assert!(matches!(XmpPacket::parse(xml.as_bytes()), Err(XmpError::DuplicateActiveProperty { .. })));
    }

    #[test]
    fn duplicate_expanded_attributes_are_rejected_even_if_equal() {
        let xml = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:a="{CAMERA_RAW_NS}" xmlns:b="{CAMERA_RAW_NS}">
          <rdf:Description rdf:about="" a:Exposure2012="0" b:Exposure2012="0"/>
          </rdf:RDF>"#
        );
        assert!(matches!(XmpPacket::parse(xml.as_bytes()), Err(XmpError::InvalidXml(_))));
    }

    #[test]
    fn duplicate_same_value_in_separate_descriptions_is_semantically_compatible() -> TestResult {
        let xml = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}">
          <rdf:Description crs:Exposure2012="0"/><rdf:Description crs:Exposure2012="0"/>
          </rdf:RDF>"#
        );
        assert_eq!(XmpPacket::parse(xml.as_bytes())?.camera_raw("Exposure2012"), ValueState::Text("0"));
        Ok(())
    }

    #[test]
    fn missing_and_zero_white_balance_metadata_remain_distinct() -> TestResult {
        let xml = format!(r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}"><rdf:Description crs:Temperature="0"/></rdf:RDF>"#);
        let packet = XmpPacket::parse(xml.as_bytes())?;
        assert_eq!(packet.camera_raw("Temperature"), ValueState::Text("0"));
        assert_eq!(packet.camera_raw("CustomTemperature"), ValueState::Absent);
        Ok(())
    }

    #[test]
    fn doctype_and_external_entities_refused() {
        let xml = format!(r#"<!DOCTYPE rdf:RDF [<!ENTITY x SYSTEM "file:///etc/passwd">]><rdf:RDF xmlns:rdf="{RDF_NS}"/>"#);
        assert!(matches!(XmpPacket::parse(xml.as_bytes()), Err(XmpError::Unsupported(_))));
    }

    #[test]
    fn malformed_and_undeclared_prefix_refused() {
        let malformed = format!(r#"<rdf:RDF xmlns:rdf="{RDF_NS}"><rdf:Description></rdf:RDF>"#);
        assert!(XmpPacket::parse(malformed.as_bytes()).is_err());
        let unbound = format!(r#"<rdf:RDF xmlns:rdf="{RDF_NS}"><rdf:Description crs:Exposure2012="0"/></rdf:RDF>"#);
        assert!(XmpPacket::parse(unbound.as_bytes()).is_err());
    }

    #[test]
    fn declared_non_utf8_encoding_is_rejected() {
        let xml = format!(r#"<?xml version="1.0" encoding="ISO-8859-1"?><rdf:RDF xmlns:rdf="{RDF_NS}"/>"#);
        assert!(matches!(XmpPacket::parse(xml.as_bytes()), Err(XmpError::Unsupported(_))));
    }

    #[test]
    fn huge_input_rejected_before_allocation() {
        let xml = vec![b'X'; MAX_XMP_BYTES + 1];
        assert!(matches!(XmpPacket::parse(&xml), Err(XmpError::TooLarge { .. })));
    }

    #[test]
    fn deep_nesting_is_rejected() {
        let xml = format!("<rdf:RDF xmlns:rdf=\"{RDF_NS}\">{}{}</rdf:RDF>", "<a>".repeat(MAX_DEPTH + 1), "</a>".repeat(MAX_DEPTH + 1));
        assert!(matches!(XmpPacket::parse(xml.as_bytes()), Err(XmpError::TooLarge { .. })));
    }

    #[test]
    fn zero_is_distinct_from_missing() -> TestResult {
        let xml = format!(r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}"><rdf:Description crs:Exposure2012="0"/></rdf:RDF>"#);
        let packet = XmpPacket::parse(xml.as_bytes())?;
        assert_eq!(packet.camera_raw("Exposure2012"), ValueState::Text("0"));
        assert_eq!(packet.camera_raw("Contrast2012"), ValueState::Absent);
        Ok(())
    }

    #[test]
    fn xml_character_references_are_decoded() -> TestResult {
        let xml = format!(r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:crs="{CAMERA_RAW_NS}"><rdf:Description crs:WhiteBalance="A&#x20;B"/></rdf:RDF>"#);
        let packet = XmpPacket::parse(xml.as_bytes())?;
        assert_eq!(packet.camera_raw("WhiteBalance"), ValueState::Text("A B"));
        Ok(())
    }
}

/// Native host adapter: finds matching XMP sidecars without opening the RAW.
/// Kept outside the core XML parser; not compiled for wasm32 targets.
#[cfg(not(target_arch = "wasm32"))]
pub mod sidecar;

pub mod inspection;

#[cfg(test)]
mod fixture_tests {
    use super::*;

    #[test]
    fn synthetic_adobe_style_sidecar_is_read_only() -> Result<(), Box<dyn std::error::Error>> {
        let source = include_bytes!("../fixtures/synthetic.xmp");
        let packet = XmpPacket::parse(source)?;
        assert_eq!(packet.original_bytes(), source);
        assert_eq!(packet.camera_raw("Exposure2012"), ValueState::Text("0.7853281"));
        assert_eq!(packet.camera_raw("Contrast2012"), ValueState::Text("32"));
        assert_eq!(packet.camera_raw("Highlights2012"), ValueState::Text("-84"));
        assert_eq!(packet.camera_raw("Blacks2012"), ValueState::Text("-40"));
        assert_eq!(packet.camera_raw("ToneCurvePV2012"), ValueState::Opaque);
        assert_eq!(packet.snapshots().len(), 2);
        assert_eq!(packet.snapshots()[0].name.as_deref(), Some("Import"));
        assert_eq!(packet.snapshots()[1].name.as_deref(), Some("Synthetic profile"));
        Ok(())
    }
}
