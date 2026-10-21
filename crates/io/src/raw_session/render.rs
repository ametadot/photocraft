//! Recipe reconstruction and guarded derived JPEG publication for worker jobs.

use super::super::{NativeRecipe, SourceBinding, commit, store::StoreError};
use photocraft_algo::camera_raw::CameraRaw;
use photocraft_format::atomic::{PublicationOutcome, atomic_write_outcome};
use std::path::{Path, PathBuf};

fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::Invalid(message.into())
}

/// Verify identity before and after reading; a same-byte replacement does not
/// inherit a retained recipe. Sidecar conflicts do not prohibit previewing an
/// otherwise unchanged source using the saved working recipe.
pub fn read_verified_source(binding: &SourceBinding) -> Result<Vec<u8>, StoreError> {
    let expected = (binding.source_fingerprint.clone(), binding.source_identity.clone());
    if commit::source_fingerprint(&binding.source)? != expected {
        return Err(invalid("Retained source identity or contents changed"));
    }
    let bytes = std::fs::read(&binding.source)?;
    if blake3::hash(&bytes).to_hex().as_str() != binding.source_fingerprint || commit::source_fingerprint(&binding.source)? != expected {
        return Err(invalid("Retained source changed while it was being read"));
    }
    Ok(bytes)
}

/// Mirrors the production RAW/RGB split. Raw-stage choices must not be applied
/// a second time when reconstructing exports or main-editor handoffs.
pub fn remaining_rgb(recipe: &NativeRecipe, wb_applied: bool) -> CameraRaw {
    let mut remaining = recipe.adjustments.clone();
    if recipe.processing.source_kind != crate::camera_raw_source::SourceKind::Raw {
        remaining.pixel_scale = 1.0;
        return remaining;
    }
    remaining.exposure = 0.0;
    remaining.shadows = 0.0;
    remaining.highlights = 0.0;
    remaining.clarity = 0.0;
    remaining.texture = 0.0;
    remaining.dehaze = 0.0;
    if wb_applied {
        remaining.temperature = 0.0;
        remaining.tint = 0.0;
    }
    // RAW development's documented output is gamma-1.8 ProPhoto-compatible RGB.
    // Zero in an older native recipe requests automatic encoding resolution.
    if remaining.encoding_gamma == 0.0 {
        remaining.encoding_gamma = 1.8;
    }
    remaining.pixel_scale = 1.0;
    remaining
}

/// Full-resolution reconstruction through the production RAW decoder followed
/// by Camera Raw's remaining RGB adjustments, retaining its 16-bit RGB profile.
pub fn render_recipe(name: &str, bytes: &[u8], recipe: &NativeRecipe) -> Result<crate::ImportResult, StoreError> {
    let (mut imported, wb_applied) = crate::raw::import_raw_recipe(name, bytes, recipe).map_err(|error| invalid(error.to_string()))?;
    let params = remaining_rgb(recipe, wb_applied);
    if params.is_identity() {
        return Ok(imported);
    }
    let width = imported.document.size.width as usize;
    let height = imported.document.size.height as usize;
    let count = width.checked_mul(height).filter(|count| *count <= 300_000_000).ok_or_else(|| invalid("RAW reconstruction dimensions exceed limits"))?;
    let surface = imported.document.layers.first_mut().and_then(|layer| layer.surface_mut()).ok_or_else(|| invalid("RAW decoder produced no raster layer"))?;
    let format = surface.format();
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(count).map_err(|error| invalid(error.to_string()))?;
    for y in 0..height {
        for x in 0..width {
            let mut pixel = [0.0; 4];
            surface.read_pixel(x as i32, y as i32, &mut pixel);
            pixels.push(photocraft_raster::to_rgba(&format, &pixel));
        }
    }
    photocraft_algo::camera_raw::develop(&mut pixels, width, height, &params, format.sample == photocraft_color::SampleType::F32);
    for (index, pixel) in pixels.iter().enumerate() {
        let x = (index % width) as i32;
        let y = (index / width) as i32;
        let mut converted = [0.0; 4];
        photocraft_raster::from_rgba_into(&format, *pixel, &mut converted);
        // Match the Camera Raw filter's alpha contract at every source depth.
        if format.alpha
            && let Some(alpha) = converted.get_mut(format.channels().saturating_sub(1))
        {
            *alpha = surface.sample_channel(x, y, format.channels().saturating_sub(1));
        }
        surface.write_pixel(x, y, &converted);
    }
    Ok(imported)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationReceipt {
    pub path: PathBuf,
    pub existed: bool,
    fingerprint_and_identity: Option<(String, String)>,
}

fn resolved_destination(path: &Path) -> Result<PathBuf, StoreError> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| invalid("JPEG destination has no parent"))?;
            let name = path.file_name().ok_or_else(|| invalid("JPEG destination has no filename"))?;
            Ok(std::fs::canonicalize(parent)?.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

fn same_destination(a: &Path, b: &Path) -> Result<bool, StoreError> {
    if a == b {
        return Ok(true);
    }
    if a.try_exists()? && b.try_exists()? {
        return same_file::is_same_file(a, b).map_err(StoreError::Io);
    }
    let a = resolved_destination(a)?;
    let b = resolved_destination(b)?;
    #[cfg(windows)]
    {
        let left = a.to_str().ok_or_else(|| invalid("JPEG destination is not losslessly representable"))?;
        let right = b.to_str().ok_or_else(|| invalid("Retained path is not losslessly representable"))?;
        Ok(left.to_lowercase() == right.to_lowercase())
    }
    #[cfg(not(windows))]
    {
        Ok(a == b)
    }
}

/// Protect every retained source and chosen sidecar, including physical aliases
/// and sidecars which did not exist when the draft was admitted.
pub fn validate_jpeg_destination(path: &Path, protected: &[SourceBinding]) -> Result<(), StoreError> {
    let destination = resolved_destination(path)?;
    let extension = destination.extension().and_then(|ext| ext.to_str()).ok_or_else(|| invalid("JPEG filename requires .jpg or .jpeg"))?;
    if !extension.eq_ignore_ascii_case("jpg") && !extension.eq_ignore_ascii_case("jpeg") {
        return Err(invalid("JPEG filename requires .jpg or .jpeg"));
    }
    let current_identity = match commit::source_fingerprint(&destination) {
        Ok((_, identity)) => Some(identity),
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    for binding in protected {
        if current_identity.as_ref().is_some_and(|identity| identity == &binding.source_identity || binding.target_identity.as_ref() == Some(identity)) {
            return Err(invalid("Choose another JPEG filename: this is a renamed retained source or sidecar"));
        }
        if same_destination(&destination, &binding.source)? || same_destination(&destination, &binding.target)? {
            return Err(invalid("Choose another JPEG filename: this destination is a retained source or sidecar"));
        }
    }
    Ok(())
}

pub fn prepare_jpeg_destination(path: &Path, protected: &[SourceBinding]) -> Result<DestinationReceipt, StoreError> {
    validate_jpeg_destination(path, protected)?;
    let path = resolved_destination(path)?;
    let fingerprint_and_identity = match commit::source_fingerprint(&path) {
        Ok(value) => Some(value),
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok(DestinationReceipt { path, existed: fingerprint_and_identity.is_some(), fingerprint_and_identity })
}

/// The caller has approved overwriting the exact existing receipt, if any.
/// Validation runs again after encoding and immediately before staged replace.
pub fn publish_jpeg(path: &Path, bytes: &[u8], protected: &[SourceBinding], receipt: &DestinationReceipt) -> Result<PublicationOutcome, StoreError> {
    if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err(invalid("JPEG encoder did not produce a JPEG packet"));
    }
    let current = prepare_jpeg_destination(path, protected)?;
    if current != *receipt {
        return Err(invalid("JPEG output changed since its overwrite decision; prepare a new destination"));
    }
    atomic_write_outcome(&current.path, bytes).map_err(StoreError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_recipe_reconstruction_matches_production_raw_rgb_split() {
        let bytes = photocraft_raw::testgen::tiff_ep("NIKON CORPORATION", 16, 16, &[1024; 256], [0, 1, 1, 2], 12, vec![]);
        let mut recipe = super::super::super::tests::recipe(0.75);
        recipe.processing = crate::raw::capture_processing(&bytes).unwrap();
        recipe.adjustments.contrast = 23.0;
        recipe.adjustments.temperature = 12.0;
        let (raw, wb) = crate::raw::import_raw_recipe("test.NEF", &bytes, &recipe).unwrap();
        let mut expected = photocraft_compose::flatten(&raw.document);
        photocraft_algo::camera_raw::develop(&mut expected.px, 16, 16, &remaining_rgb(&recipe, wb), false);
        let actual = render_recipe("test.NEF", &bytes, &recipe).unwrap();
        let actual = photocraft_compose::flatten(&actual.document);
        for (a, b) in actual.px.iter().zip(&expected.px) {
            assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1.1 / 65535.0));
        }
    }

    #[test]
    fn jpeg_destination_protects_aliases_and_exact_overwrite_receipt() {
        let root = std::env::temp_dir().join(super::super::identity("jpeg-test").unwrap());
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.NEF");
        std::fs::write(&source, b"retained source").unwrap();
        std::fs::write(source.with_extension("xmp"), b"retained sidecar").unwrap();
        let binding = commit::bind(&source).unwrap();
        let alias = root.join("alias.jpg");
        std::fs::hard_link(&binding.target, &alias).unwrap();
        assert!(validate_jpeg_destination(&alias, std::slice::from_ref(&binding)).is_err());
        let output = root.join("derived.jpg");
        let receipt = prepare_jpeg_destination(&output, std::slice::from_ref(&binding)).unwrap();
        assert!(!receipt.existed);
        assert!(matches!(publish_jpeg(&output, &[0xff, 0xd8, 0xff, 0xd9], std::slice::from_ref(&binding), &receipt).unwrap(), PublicationOutcome::Published));
        let overwrite = prepare_jpeg_destination(&output, std::slice::from_ref(&binding)).unwrap();
        assert!(overwrite.existed);
        std::fs::rename(&output, root.join("preserved.jpg")).unwrap();
        std::fs::write(&output, [0xff, 0xd8, 0xff, 0xd9]).unwrap();
        assert!(publish_jpeg(&output, &[0xff, 0xd8, 0xff, 0xd9], std::slice::from_ref(&binding), &overwrite).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"retained source");
        let moved = root.join("renamed-source.jpg");
        std::fs::rename(&source, &moved).unwrap();
        assert!(validate_jpeg_destination(&moved, std::slice::from_ref(&binding)).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
