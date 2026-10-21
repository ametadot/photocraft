//! Read-only source and sidecar identity binding shared by Camera Raw admission.
pub use super::SourceBinding as Binding;
use super::store::StoreError;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::Invalid(message.into())
}

pub(super) fn source_fingerprint(path: &Path) -> Result<(String, String), StoreError> {
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("Source is not a regular file"));
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).ok_or_else(|| invalid("Invalid source read length"))?);
    }
    Ok((hasher.finalize().to_hex().to_string(), physical_identity(&file)?))
}

pub(super) fn physical_identity(file: &File) -> Result<String, StoreError> {
    #[cfg(windows)]
    {
        let information = winapi_util::file::information(file)?;
        Ok(format!("windows:{}:{}:{:?}", information.volume_serial_number(), information.file_index(), information.creation_time()))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = file;
        Err(invalid("Native physical identity unavailable on this platform"))
    }
}

pub(super) fn sidecar_bytes(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(invalid("Sidecar is not a regular file"));
    }
    let limit = super::store::MAX_BOUND_XMP_BYTES;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid("Sidecar exceeds the 1 MiB per-packet recovery budget; source packet retained"));
    }
    Ok(Some(bytes))
}

const RAW_SIDECAR_OWNERS: &[&str] = &[
    "3fr", "ari", "arw", "bay", "braw", "cap", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "drf", "eip", "erf", "fff", "gpr", "iiq", "k25", "kdc", "mdc", "mef",
    "mos", "mrw", "nef", "nrw", "orf", "pef", "ptx", "pxn", "r3d", "raf", "raw", "rw2", "rwl", "rwz", "sr2", "srf", "srw", "x3f",
];

fn camera_raw_source_name(name: &str) -> bool {
    raw_name(name)
        || Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ["jpg", "jpeg", "jpe", "jfif", "jif", "tif", "tiff"].iter().any(|known| e.eq_ignore_ascii_case(known)))
}

fn raw_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| photocraft_raw::EXTENSIONS.iter().chain(RAW_SIDECAR_OWNERS).any(|raw| e.eq_ignore_ascii_case(raw)))
}

pub fn sidecar_target(source: &Path) -> Result<PathBuf, StoreError> {
    resolve_target(&std::fs::canonicalize(source)?)
}

fn resolve_target(source: &Path) -> Result<PathBuf, StoreError> {
    let parent = source.parent().ok_or_else(|| invalid("Source has no parent"))?;
    let conventional = source.with_extension("xmp");
    let own = source.file_name().and_then(|s| s.to_str()).ok_or_else(|| invalid("Source filename cannot be serialized losslessly"))?;
    let own_stem = source.file_stem().and_then(|s| s.to_str()).ok_or_else(|| invalid("Source filename cannot be serialized losslessly"))?;
    let stem = conventional.file_name().and_then(|s| s.to_str()).ok_or_else(|| invalid("Sidecar filename cannot be serialized losslessly"))?;
    let full = format!("{own}.xmp");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            names.push((name.to_owned(), entry.path()));
        }
    }
    let siblings: Vec<&str> = names
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| !name.eq_ignore_ascii_case(own) && camera_raw_source_name(name))
        .filter(|name| Path::new(name).file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(own_stem)))
        .collect();
    let stem_path = names.iter().find(|(name, _)| name.eq_ignore_ascii_case(stem)).map(|(_, path)| path.clone());
    let has_full = names.iter().any(|(name, _)| name.eq_ignore_ascii_case(&full));
    // Every non-RAW source (whatever its extension: admission routes by content) follows
    // the rendered rule.
    let rendered = !raw_name(own);
    if raw_name(own)
        && let Some(saved) = stem_path.as_deref().and_then(saved_kind)
        && saved != crate::camera_raw_source::SourceKind::Raw
    {
        return Err(invalid(format!(
            "{stem} holds PhotoCraft settings saved for a JPEG or TIFF, not for {own}; rename it to that file's own <name>.<ext>.xmp or move it. Nothing was changed"
        )));
    }
    // Classify stem packets that the legacy resolver would adopt. For rendered photos,
    // neither matching CONTENT kind nor absence of a RAW sibling proves photo ownership:
    // refuse those packets below before discovery or admission. Reading content kind may
    // fail, which remains an error (retry), never permission to change the binding.
    let owns_stem = !rendered
        || match stem_path.as_deref().and_then(saved_kind) {
            Some(saved) => content_kind(source)? == saved,
            None => !has_full && !siblings.iter().any(|name| raw_name(name)),
        };
    if rendered && let Some(stem_path) = stem_path.as_deref() {
        let alias = names
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&full))
            .map(|(_, path)| same_file::is_same_file(stem_path, path))
            .transpose()?
            .unwrap_or(false);
        if owns_stem || alias {
            return Err(invalid(format!(
                "Sidecar ownership is unproven: {stem} is not explicitly bound to {own}. Camera Raw discovery and admission refuse this stem packet; original files and metadata were not changed"
            )));
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for (name, path) in &names {
        if !name.eq_ignore_ascii_case(&full) && !(owns_stem && name.eq_ignore_ascii_case(stem)) {
            continue;
        }
        let path = std::fs::canonicalize(path)?;
        let mut alias = false;
        for old in &candidates {
            alias |= same_file::is_same_file(old, &path)?;
        }
        if !alias {
            candidates.push(path);
        }
    }
    if candidates.len() > 1 {
        let names: Vec<String> = candidates.iter().filter_map(|path| path.file_name().map(|name| name.to_string_lossy().into_owned())).collect();
        return Err(invalid(format!(
            "Multiple distinct sidecars require a binding decision: {} all belong to {own}; rename or move the one that does not belong to it. Nothing was changed",
            names.join(" and ")
        )));
    }
    // A new sidecar for a JPEG/TIFF is always its own <name>.<ext>.xmp.
    let target = candidates.pop().unwrap_or_else(|| if rendered { parent.join(&full) } else { conventional });
    Ok(target)
}

fn content_kind(source: &Path) -> Result<crate::camera_raw_source::SourceKind, StoreError> {
    let bytes = photocraft_format::read_file(source)?;
    crate::camera_raw_source::inspect(&bytes)
        .map(|inspection| inspection.kind)
        .map_err(|error| invalid(format!("Cannot confirm which file owns its sidecar: {error}")))
}

fn saved_kind(path: &Path) -> Option<crate::camera_raw_source::SourceKind> {
    let bytes = sidecar_bytes(path).ok().flatten()?;
    super::xmp::load_native(&bytes).ok().flatten().map(|recipe| recipe.processing.source_kind)
}

pub fn bind(source: &Path) -> Result<Binding, StoreError> {
    let source = std::fs::canonicalize(source)?;
    let (source_fingerprint, source_identity) = source_fingerprint(&source)?;
    let target = resolve_target(&source)?;
    if target.try_exists()? && same_file::is_same_file(&source, &target)? {
        return Err(invalid("Sidecar aliases the retained source"));
    }
    let original = sidecar_bytes(&target)?;
    let target_identity = if original.is_some() { Some(physical_identity(&File::open(&target)?)?) } else { None };
    Ok(Binding { source, source_fingerprint, source_identity, target, original, target_identity })
}
