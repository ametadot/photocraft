//! Content-based Camera Raw admission for sensor RAW and rendered photographs.
//! Inspection never rewrites the source. Rendered files retain their encoded
//! pixels, depth and profile; only the RGB stage applies their adjustments.

use crate::raw_session::{Metadata, NativeRecipe, Processing, WhiteBalance};
use crate::{ImportResult, IoError};
use photocraft_codecs::{ChannelLayout, Format};
use serde::{Deserialize, Serialize};

pub const RENDERED_NOTE: &str = "rendered photograph staged for Camera Raw";
const MAX_PACKET: usize = crate::raw_xmp::MAX_XMP_BYTES;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    /// Schema-1 recipes predating rendered admission were all sensor RAW.
    #[default]
    Raw,
    Jpeg,
    Tiff,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspection {
    pub kind: SourceKind,
    pub supported: bool,
    pub layered_tiff: bool,
    pub has_settings: bool,
    pub embedded_xmp: Option<Vec<u8>>,
}

fn unsupported(message: impl Into<String>) -> IoError {
    IoError::Unsupported(message.into())
}

/// Actual content wins over extension. TIFF sensor files are recognized before
/// rendered TIFF; all TIFF directories are checked for Photoshop layer data.
pub fn inspect(bytes: &[u8]) -> Result<Inspection, IoError> {
    let format = photocraft_codecs::detect(bytes);
    let kind = if crate::raw::is_raw(bytes) {
        SourceKind::Raw
    } else {
        match format {
            Some(Format::Jpeg) => SourceKind::Jpeg,
            Some(Format::Tiff) => SourceKind::Tiff,
            _ => SourceKind::Other,
        }
    };
    let (layered_tiff, embedded_xmp, rendered_supported) = match format {
        Some(Format::Jpeg) => {
            let (xmp, components) = jpeg_xmp(bytes)?;
            (false, xmp, matches!(components, Some(1 | 3)))
        }
        Some(Format::Tiff) => tiff_metadata(bytes)?,
        _ => (false, None, false),
    };
    let has_settings = embedded_xmp.as_deref().map(has_settings).transpose()?.unwrap_or(false);
    let supported = kind == SourceKind::Raw || (rendered_supported && !layered_tiff);
    Ok(Inspection { kind, supported, layered_tiff, has_settings, embedded_xmp })
}

/// A caller which has read authority for an adjacent packet can supply it. This
/// module itself does no ambient filesystem reads or permission escalation.
pub fn inspect_with_sidecar(bytes: &[u8], sidecar: Option<&[u8]>) -> Result<Inspection, IoError> {
    let mut inspection = inspect(bytes)?;
    if let Some(packet) = sidecar {
        inspection.has_settings = has_settings(packet)?;
    }
    Ok(inspection)
}

/// Native recipes and active Adobe CRS properties qualify; namespace text in an
/// unrelated description, a rating alone and HasSettings=False do not.
pub fn has_settings(bytes: &[u8]) -> Result<bool, IoError> {
    let packet = photocraft_xmp::XmpPacket::parse(bytes).map_err(|e| unsupported(e.to_string()))?;
    if !matches!(packet.property(crate::raw_session::xmp::NATIVE_NS, "Recipe"), photocraft_xmp::ValueState::Absent) {
        return Ok(true);
    }
    match packet.camera_raw("HasSettings") {
        photocraft_xmp::ValueState::Text(value) if value.eq_ignore_ascii_case("false") => return Ok(false),
        photocraft_xmp::ValueState::Text(value) if value.eq_ignore_ascii_case("true") => return Ok(true),
        photocraft_xmp::ValueState::Absent => {}
        _ => return Err(unsupported("Invalid embedded Camera Raw HasSettings flag")),
    }
    Ok(packet
        .active_properties()
        .keys()
        .any(|key| key.namespace_uri == photocraft_xmp::CAMERA_RAW_NS && !matches!(key.local_name.as_str(), "Version" | "ProcessVersion" | "HasSettings")))
}

fn packet(bytes: &[u8]) -> Result<Vec<u8>, IoError> {
    if bytes.len() > MAX_PACKET {
        return Err(unsupported("Embedded XMP exceeds its bounded packet limit"));
    }
    Ok(bytes.to_vec())
}

fn jpeg_xmp(bytes: &[u8]) -> Result<(Option<Vec<u8>>, Option<u8>), IoError> {
    const STANDARD: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    const EXTENDED: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
    let mut offset = 2usize;
    let mut found = None;
    let mut components = None;
    while offset < bytes.len() {
        if bytes.get(offset) != Some(&0xff) {
            return Err(unsupported("Malformed JPEG marker before image data"));
        }
        while bytes.get(offset) == Some(&0xff) {
            offset = offset.saturating_add(1);
        }
        let marker = *bytes.get(offset).ok_or_else(|| unsupported("Truncated JPEG marker"))?;
        offset = offset.saturating_add(1);
        if matches!(marker, 0xda | 0xd9) {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        let len: [u8; 2] =
            bytes.get(offset..offset.saturating_add(2)).and_then(|b| b.try_into().ok()).ok_or_else(|| unsupported("Truncated JPEG segment length"))?;
        let len = usize::from(u16::from_be_bytes(len));
        if len < 2 {
            return Err(unsupported("Invalid JPEG segment length"));
        }
        let end = offset.checked_add(len).ok_or_else(|| unsupported("JPEG segment overflow"))?;
        let payload = bytes.get(offset.saturating_add(2)..end).ok_or_else(|| unsupported("Truncated JPEG metadata segment"))?;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            components = payload.get(5).copied();
        }
        if marker == 0xe1 {
            if payload.starts_with(EXTENDED) {
                return Err(unsupported("Extended embedded JPEG XMP requires a metadata-preserving reader; open in the editor"));
            }
            if let Some(xmp) = payload.strip_prefix(STANDARD) {
                if found.is_some() {
                    return Err(unsupported("Multiple embedded JPEG XMP packets require a metadata decision"));
                }
                found = Some(packet(xmp)?);
            }
        }
        offset = end;
    }
    Ok((found, components))
}

fn tiff_metadata(bytes: &[u8]) -> Result<(bool, Option<Vec<u8>>, bool), IoError> {
    let info = photocraft_codecs::tiff_info(bytes)?;
    if !info.complete {
        return Err(unsupported("Incomplete TIFF directory inspection; Camera Raw cannot establish layer safety"));
    }
    let chosen = info.default_page();
    let width = if info.big_tiff { 8usize } else { 4 };
    let read = |offset: usize, size: usize| -> Result<u64, IoError> {
        let slice = bytes.get(offset..offset.saturating_add(size)).ok_or_else(|| unsupported("Truncated TIFF directory"))?;
        let mut value = 0u64;
        for index in 0..size {
            let index = if info.little_endian { size - 1 - index } else { index };
            value = (value << 8) | u64::from(*slice.get(index).ok_or_else(|| unsupported("Truncated TIFF value"))?);
        }
        Ok(value)
    };
    let mut layered = false;
    let mut xmp = None;
    let mut total_entries = 0u64;
    for (page_index, page) in info.pages.iter().enumerate() {
        let at = usize::try_from(page.ifd_offset).map_err(|_| unsupported("TIFF offset exceeds address space"))?;
        let count_size = if info.big_tiff { 8usize } else { 2 };
        let count = read(at, count_size)?;
        total_entries = total_entries.saturating_add(count);
        if total_entries > 100_000 {
            return Err(unsupported("TIFF metadata directory budget exceeded"));
        }
        let entry_size = if info.big_tiff { 20usize } else { 12 };
        let first = at.checked_add(count_size).ok_or_else(|| unsupported("TIFF directory overflow"))?;
        for index in 0..count {
            let offset = usize::try_from(index)
                .ok()
                .and_then(|i| i.checked_mul(entry_size))
                .and_then(|i| first.checked_add(i))
                .ok_or_else(|| unsupported("TIFF entry overflow"))?;
            let tag = read(offset, 2)?;
            if tag == 37724 {
                // Presence is enough, even if the layer block itself is damaged.
                layered = true;
            }
            if tag != 700 || Some(page_index) != chosen {
                continue;
            }
            if xmp.is_some() {
                return Err(unsupported("Multiple TIFF XMP tags require a metadata decision"));
            }
            let data_type = read(offset.saturating_add(2), 2)?;
            if !matches!(data_type, 1 | 2 | 7) {
                return Err(unsupported("Unsupported TIFF XMP tag storage type"));
            }
            let length = read(offset.saturating_add(4), width)?;
            if length > MAX_PACKET as u64 {
                return Err(unsupported("Embedded TIFF XMP exceeds packet limit"));
            }
            let location = offset.checked_add(4 + width).ok_or_else(|| unsupported("TIFF payload overflow"))?;
            let start = if length <= width as u64 { location as u64 } else { read(location, width)? };
            let start = usize::try_from(start).map_err(|_| unsupported("TIFF XMP offset exceeds address space"))?;
            let length = usize::try_from(length).map_err(|_| unsupported("TIFF XMP size exceeds address space"))?;
            let payload = bytes.get(start..start.saturating_add(length)).ok_or_else(|| unsupported("Truncated TIFF XMP packet"))?;
            let payload = payload.strip_suffix(&[0]).unwrap_or(payload);
            xmp = Some(packet(payload)?);
        }
    }
    let supported = info.page_count() == 1
        && chosen.and_then(|i| info.pages.get(i)).is_some_and(|page| matches!(page.photometric, Some(0..=3)) && matches!(page.sample_format, 1 | 3));
    Ok((layered, xmp, supported))
}

fn rendered_image(bytes: &[u8], inspection: &Inspection, orient: bool) -> Result<photocraft_codecs::Image, IoError> {
    if !matches!(inspection.kind, SourceKind::Jpeg | SourceKind::Tiff) || !inspection.supported {
        return Err(unsupported("Camera Raw rendered admission requires a flattened JPEG or TIFF; layered TIFF stays in the editor"));
    }
    let image = photocraft_codecs::decode_with(bytes, &photocraft_codecs::DecodeOptions { keep_orientation: !orient, ..Default::default() })?;
    if matches!(image.layout(), ChannelLayout::Cmyk | ChannelLayout::CmykA) {
        return Err(unsupported("Camera Raw currently supports RGB and grayscale rendered photographs; retain CMYK in the editor"));
    }
    if let Some(profile) = image.icc.as_deref() {
        let profile = photocraft_cms::Profile::parse(profile).map_err(|e| unsupported(format!("Invalid rendered source ICC profile: {e}")))?;
        let expected = match image.layout() {
            ChannelLayout::Gray | ChannelLayout::GrayA => photocraft_cms::ColorSpace::Gray,
            _ => photocraft_cms::ColorSpace::Rgb,
        };
        if profile.color_space != expected {
            return Err(unsupported("Rendered source ICC profile does not describe its pixel colour model"));
        }
    }
    Ok(image)
}

pub fn capture_processing(bytes: &[u8]) -> Result<Processing, IoError> {
    let inspection = inspect(bytes)?;
    if inspection.kind == SourceKind::Raw {
        return crate::raw::capture_processing(bytes);
    }
    let _ = rendered_image(bytes, &inspection, true)?;
    Ok(rendered_processing(inspection.kind))
}

fn rendered_processing(kind: SourceKind) -> Processing {
    Processing {
        version: 1,
        source_kind: kind,
        demosaic: "none".into(),
        white_balance: WhiteBalance::AsShot,
        orient: true,
        camera_matrix: None,
        camera_tone_curve: Vec::new(),
    }
}

/// Complete native sidecar state wins over embedded settings. Without a native
/// recipe, the immutable embedded packet supplies the entry settings and metadata;
/// a bound sidecar, when present, is the explicit higher-priority packet.
pub fn initial_recipe(bytes: &[u8], sidecar: Option<&[u8]>) -> Result<NativeRecipe, IoError> {
    let inspection = inspect(bytes)?;
    let input = sidecar.or(inspection.embedded_xmp.as_deref());
    if let Some(packet) = input
        && let Some(recipe) = crate::raw_session::xmp::load_native(packet).map_err(unsupported)?
    {
        if recipe.processing.source_kind != inspection.kind || inspection.layered_tiff {
            return Err(unsupported("Saved recipe source kind differs from the actual source"));
        }
        // Decode through the saved choices now, before acknowledging admission.
        let _ = crate::raw::import_raw_recipe("Camera Raw source", bytes, &recipe)?;
        return Ok(recipe);
    }
    let (processing, encoding_gamma) = if inspection.kind == SourceKind::Raw {
        (crate::raw::capture_processing(bytes)?, 0.0)
    } else {
        let image = rendered_image(bytes, &inspection, true)?;
        (rendered_processing(inspection.kind), rendered_encoding_gamma(&image)?)
    };
    let mut recipe = NativeRecipe {
        schema: crate::raw_session::SCHEMA_VERSION,
        processing,
        adjustments: Default::default(),
        metadata: Metadata { rating: None, label: String::new(), marked: false },
    };
    if let Some(packet) = input {
        recipe.adjustments = crate::raw_xmp::import_adobe_raw_settings(packet).map_err(|e| unsupported(e.to_string()))?.params;
        if inspection.kind != SourceKind::Raw && has_settings(packet)? {
            crate::camera_raw_xmp::load_current(&mut recipe.adjustments, packet).map_err(unsupported)?;
        }
        recipe.metadata.rating = crate::raw_xmp::read_rating(packet).map_err(|e| unsupported(e.to_string()))?;
        let parsed = photocraft_xmp::XmpPacket::parse(packet).map_err(|e| unsupported(e.to_string()))?;
        recipe.metadata.label = match parsed.property(photocraft_xmp::XMP_NS, "Label") {
            photocraft_xmp::ValueState::Absent => String::new(),
            photocraft_xmp::ValueState::Text(value) => value.to_owned(),
            _ => return Err(unsupported("Structured XMP label requires a metadata decision")),
        };
    }
    if inspection.kind != SourceKind::Raw {
        // These internal processing values describe the actual source encoding,
        // independently of any defaults in an imported adjustment packet.
        recipe.adjustments.encoding_gamma = encoding_gamma;
    }
    recipe.validate().map_err(unsupported)?;
    Ok(recipe)
}

fn rendered_encoding_gamma(image: &photocraft_codecs::Image) -> Result<f32, IoError> {
    use photocraft_cms::{Builtin, Curve, Profile};
    let Some(bytes) = image.icc.as_deref() else { return Ok(0.0) };
    let profile = Profile::parse(bytes).map_err(|e| unsupported(e.to_string()))?;
    if let Some(curve) = &profile.gray_trc {
        return Ok(match curve {
            Curve::Identity => 1.0,
            Curve::Gamma(gamma) => *gamma as f32,
            Curve::Parametric { kind: 0, p } => p.first().copied().unwrap_or(1.0) as f32,
            _ => 0.0,
        });
    }
    Ok([(Builtin::ProPhotoCompat, 1.8), (Builtin::AdobeRgbCompat, 563.0 / 256.0), (Builtin::LinearSrgb, 1.0)]
        .into_iter()
        .find(|(builtin, _)| profile.same_colors(builtin.profile()))
        .map_or(0.0, |(_, gamma)| gamma))
}

#[cfg(test)]
mod tests;

pub fn import_rendered_recipe(name: &str, bytes: &[u8], recipe: &NativeRecipe) -> Result<ImportResult, IoError> {
    recipe.validate().map_err(unsupported)?;
    let inspection = inspect(bytes)?;
    if inspection.kind != recipe.processing.source_kind {
        return Err(unsupported("Rendered source content differs from the saved recipe kind"));
    }
    let image = rendered_image(bytes, &inspection, recipe.processing.orient)?;
    crate::flat::image_to_document(name, &image)
}
