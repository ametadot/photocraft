//! Bounded typed snapshot decoding. RAW/XMP byte arrays never become Value trees.
use super::{MAX_BOUND_XMP_BYTES, MAX_PAYLOAD_BYTES, MAX_SESSION_XMP_BYTES, StoreError};
use crate::raw_session::{Admission, NativeRecipe, Phase, Photo, PhotoId, Session, SourceBinding};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PhotoWire<'a> {
    id: PhotoId,
    #[serde(borrow)]
    working: &'a RawValue,
    #[serde(borrow)]
    committed: Option<&'a RawValue>,
    #[serde(borrow)]
    entry: &'a RawValue,
    revision: u64,
    hidden: bool,
    selected: bool,
    committed_this_visit: bool,
    unknown_operation: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionWire<'a> {
    schema: u32,
    id: String,
    epoch: u64,
    generation: u64,
    cached_generation: Option<u64>,
    phase: Phase,
    #[serde(borrow)]
    photos: Vec<PhotoWire<'a>>,
    requested: BTreeMap<String, Admission>,
    sources: BTreeMap<PhotoId, SourceBinding>,
    removed: BTreeSet<PhotoId>,
    #[serde(default, borrow)]
    view_context: Option<&'a RawValue>,
}

fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::Invalid(message.into())
}

fn recipe(raw: &RawValue) -> Result<NativeRecipe, StoreError> {
    let bytes = raw.get().as_bytes();
    if bytes.len() > crate::raw_session::MAX_RECIPE_BYTES {
        return Err(invalid("Recipe exceeds size limit"));
    }
    #[derive(Deserialize)]
    struct Version {
        schema: u32,
        processing: ProcessingVersion,
    }
    #[derive(Deserialize)]
    struct ProcessingVersion {
        version: u32,
    }
    let version: Version = serde_json::from_slice(bytes).map_err(super::invalid)?;
    if version.schema != 1 || version.processing.version != 1 {
        return Err(StoreError::Unsupported("Recipe/processing version".into()));
    }
    NativeRecipe::decode(bytes).map_err(invalid)
}

pub(in crate::raw_session) fn validate_snapshot(bytes: &[u8]) -> Result<Session, StoreError> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(StoreError::Unsupported("Session payload exceeds current 12 MiB recovery capacity; retain the acknowledged record".into()));
    }
    #[derive(Deserialize)]
    struct Version {
        schema: u32,
    }
    let version: Version = serde_json::from_slice(bytes).map_err(super::invalid)?;
    if version.schema != 1 {
        return Err(StoreError::Unsupported("Session snapshot version".into()));
    }
    let wire: SessionWire<'_> = serde_json::from_slice(bytes).map_err(super::invalid)?;
    if wire.photos.len() > crate::raw_session::MAX_PHOTOS {
        return Err(invalid("Too many photos"));
    }
    let mut photos = Vec::new();
    photos.try_reserve_exact(wire.photos.len()).map_err(|e| invalid(e.to_string()))?;
    for photo in wire.photos {
        photos.push(Photo {
            id: photo.id,
            working: recipe(photo.working)?,
            committed: photo.committed.map(recipe).transpose()?,
            entry: recipe(photo.entry)?,
            revision: photo.revision,
            hidden: photo.hidden,
            selected: photo.selected,
            committed_this_visit: photo.committed_this_visit,
            unknown_operation: photo.unknown_operation,
        });
    }
    let view_context = match wire.view_context {
        Some(raw) if raw.get().len() <= 256 * 1024 => serde_json::from_str(raw.get()).map_err(super::invalid)?,
        Some(_) => return Err(StoreError::Unsupported("Camera Raw view context exceeds current recovery capacity; retain the acknowledged record".into())),
        None => serde_json::Value::Null,
    };
    let snapshot = Session {
        schema: wire.schema,
        id: wire.id,
        epoch: wire.epoch,
        generation: wire.generation,
        cached_generation: wire.cached_generation,
        phase: wire.phase,
        photos,
        requested: wire.requested,
        sources: wire.sources,
        removed: wire.removed,
        view_context,
    };
    validate_session_bounds(&snapshot)
        .map_err(|error| StoreError::Unsupported(format!("Acknowledged snapshot exceeds current supported bounds; retain original record: {error}")))?;
    let ids: BTreeSet<_> = snapshot.photos.iter().map(|p| &p.id).collect();
    if ids.len() != snapshot.photos.len()
        || ids.iter().any(|id| snapshot.removed.contains(*id))
        || snapshot.requested.values().any(|a| match a {
            Admission::Ready(id) | Admission::Duplicate(id) => !ids.contains(id) && !snapshot.removed.contains(id),
            _ => false,
        })
    {
        return Err(invalid("Invalid photo identities or admission associations"));
    }
    Ok(snapshot)
}

/// Inspect lengths before any candidate clone or serialized allocation.
pub(in crate::raw_session) fn validate_session_bounds(snapshot: &Session) -> Result<(), StoreError> {
    let valid_id = |id: &PhotoId| !id.0.is_empty() && id.0.len() <= 128;
    if snapshot.photos.len() > crate::raw_session::MAX_PHOTOS
        || snapshot.requested.len() > crate::raw_session::MAX_PHOTOS
        || snapshot.sources.len() > crate::raw_session::MAX_PHOTOS
        || snapshot.removed.len() > crate::raw_session::MAX_PHOTOS
        || snapshot.id.is_empty()
        || snapshot.id.len() > 128
        || snapshot.photos.iter().any(|p| !valid_id(&p.id))
        || snapshot.sources.keys().chain(snapshot.removed.iter()).any(|id| !valid_id(id))
    {
        return Err(invalid("Excessive session collections or invalid identities"));
    }
    let mut text = 0usize;
    for (key, outcome) in &snapshot.requested {
        let reason = match outcome {
            Admission::Rejected(reason) | Admission::Unresolved(reason) => reason.as_str(),
            _ => "",
        };
        if key.is_empty() || key.len() > 32768 || reason.len() > 32768 {
            return Err(invalid("Admission ticket or diagnostic exceeds length budget"));
        }
        text = text.checked_add(key.len() + reason.len()).filter(|n| *n <= 1024 * 1024).ok_or_else(|| invalid("Admission text exceeds 1 MiB budget"))?;
    }
    let mut packets = 0usize;
    for binding in snapshot.sources.values() {
        let bytes = binding.original.as_ref().map_or(0, Vec::len);
        if bytes > MAX_BOUND_XMP_BYTES
            || binding.source.as_os_str().len() > 32768
            || binding.target.as_os_str().len() > 32768
            || binding.source_identity.len() > 256
            || binding.source_fingerprint.len() != 64
            || binding.target_identity.as_ref().is_some_and(|s| s.len() > 256)
        {
            return Err(invalid("Source binding exceeds recovery limits (XMP packet maximum: 1 MiB)"));
        }
        packets = packets
            .checked_add(bytes)
            .filter(|n| *n <= MAX_SESSION_XMP_BYTES)
            .ok_or_else(|| invalid("Session original XMP packets exceed 2 MiB aggregate recovery budget"))?;
    }
    for photo in &snapshot.photos {
        for recipe in [&photo.working, &photo.entry].into_iter().chain(photo.committed.as_ref()) {
            recipe.validate().map_err(invalid)?;
        }
    }
    super::serialized_size(&snapshot.view_context, 256 * 1024)?;
    super::serialized_size(snapshot, MAX_PAYLOAD_BYTES)?;
    Ok(())
}
