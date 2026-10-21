//! Camera Raw persistence domain. No pixels, runtime document IDs or filesystem effects.
//! Working, acknowledged-cache, committed-XMP and visit-entry states have separate owners.

use photocraft_algo::camera_raw::CameraRaw;
use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
pub mod commit;
#[cfg(not(target_arch = "wasm32"))]
pub mod controller;
#[cfg(not(target_arch = "wasm32"))]
pub mod scheduler;
#[cfg(not(target_arch = "wasm32"))]
pub mod store;
#[cfg(not(target_arch = "wasm32"))]
mod target_coordinator;
pub mod xmp;
use std::collections::{BTreeMap, BTreeSet};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_PHOTOS: usize = 10_000;
pub const MAX_RECIPE_BYTES: usize = 256 * 1024;

/// Host-controlled publication boundary. Native hosts inject their filesystem
/// capability checks here; read access to a photograph never grants write access
/// to its adjacent sidecar. An error means no publication occurred; an ambiguous
/// completed write must return `PublishedUnconfirmed` for reconciliation.
#[cfg(not(target_arch = "wasm32"))]
pub type SidecarPublisher = std::sync::Arc<dyn Fn(&std::path::Path, &[u8]) -> std::io::Result<photocraft_format::atomic::PublicationOutcome> + Send + Sync>;

/// Durable identities are supplied by the native service, never by a runtime DocId.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PhotoId(pub String);

/// Source and sidecar identities travel with the recovery snapshot. The native
/// binding service verifies these before reconstruction or publication.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub source: std::path::PathBuf,
    pub source_fingerprint: String,
    pub source_identity: String,
    pub target: std::path::PathBuf,
    pub original: Option<Vec<u8>>,
    pub target_identity: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// -1 is reject; zero is explicitly unrated; None means unknown.
    pub rating: Option<i8>,
    /// Keep the literal label, including labels outside the application's palette.
    pub label: String,
    pub marked: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRecipe {
    pub schema: u32,
    /// Explicit pipeline identity prevents reconstruction through different defaults.
    pub processing: Processing,
    pub adjustments: CameraRaw,
    pub metadata: Metadata,
}

/// Actual input choices outside CameraRaw. Values are captured at admission by the
/// adapter; construction deliberately has no Default implementation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Processing {
    pub version: u32,
    /// Absent in the original schema-1 payloads, which were sensor RAW only.
    #[serde(default)]
    pub source_kind: crate::camera_raw_source::SourceKind,
    pub demosaic: String,
    pub white_balance: WhiteBalance,
    pub orient: bool,
    pub camera_matrix: Option<[[f64; 3]; 3]>,
    pub camera_tone_curve: Vec<[f64; 2]>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WhiteBalance {
    AsShot,
    Auto,
    Multipliers([f64; 3]),
}

impl NativeRecipe {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA_VERSION || self.processing.version != 1 {
            return Err("Unsupported Camera Raw recipe or processing version; retain original data".into());
        }
        let rendered = matches!(self.processing.source_kind, crate::camera_raw_source::SourceKind::Jpeg | crate::camera_raw_source::SourceKind::Tiff);
        if self.processing.source_kind == crate::camera_raw_source::SourceKind::Other {
            return Err("Unsupported Camera Raw source kind".into());
        }
        if rendered
            && (self.processing.demosaic != "none"
                || self.processing.white_balance != WhiteBalance::AsShot
                || self.processing.camera_matrix.is_some()
                || !self.processing.camera_tone_curve.is_empty())
        {
            return Err("Rendered source recipe cannot contain sensor processing".into());
        }
        if !rendered && !matches!(self.processing.demosaic.as_str(), "bilinear" | "mhc" | "ahd") {
            return Err("Unsupported demosaic choice".into());
        }
        if let WhiteBalance::Multipliers(values) = self.processing.white_balance
            && values.iter().any(|v| !v.is_finite() || *v <= 0.0 || *v > 1_000.0)
        {
            return Err("Invalid RAW white balance multipliers".into());
        }
        if let Some(m) = &self.processing.camera_matrix {
            let [[a, b, c], [d, e, f], [g, h, i]] = *m;
            let determinant = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
            if m.iter().flatten().any(|v| !v.is_finite() || v.abs() >= 64.0) || determinant.abs() < 1e-12 || d + e + f <= 1e-3 {
                return Err("Invalid camera matrix".into());
            }
        }
        if self.processing.camera_tone_curve.len() > 4096 {
            return Err("Camera tone curve exceeds the supported point limit".into());
        }
        if !self.processing.camera_tone_curve.is_empty() && photocraft_raw::ToneCurve::new(&self.processing.camera_tone_curve).is_none() {
            return Err("Invalid camera tone curve".into());
        }
        if !self.processing.camera_tone_curve.is_empty() && self.processing.camera_matrix.is_none() {
            return Err("Fitted camera tone curve is missing its camera matrix".into());
        }
        if self.metadata.rating.is_some_and(|r| !(-1..=5).contains(&r)) || self.metadata.label.len() > 4096 {
            return Err("Invalid rating or excessive label length".into());
        }
        self.adjustments.validate()?;
        if !matches!(self.adjustments.vignette_style.as_str(), "highlightPriority" | "colorPriority" | "paintOverlay") {
            return Err("Invalid vignette style".into());
        }
        // serde_json represents nonfinite floats as null. Reject those rather than
        // allowing a successful write whose later deserialization cannot succeed.
        let value = serde_json::to_value(&self.adjustments).map_err(|e| e.to_string())?;
        finite_json(&value)?;
        validate_adjustment_ranges(&value)?;
        if self.adjustments.pixel_scale != 1.0 {
            return Err("A persisted recipe must use full-resolution pixel scale".into());
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_RECIPE_BYTES {
            return Err("Recipe exceeds size limit".into());
        }
        Ok(bytes)
    }

    /// CameraRaw's preset deserializer intentionally fills defaults. Persistence
    /// must instead reject missing fields, including nested colour wheel members.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_RECIPE_BYTES {
            return Err("Recipe exceeds size limit".into());
        }
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let adjustments = value.get("adjustments").ok_or("Missing native adjustments")?;
        let shape = serde_json::to_value(CameraRaw::default()).map_err(|e| e.to_string())?;
        required_shape(&shape, adjustments)?;
        let recipe: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        recipe.validate()?;
        Ok(recipe)
    }
}

fn finite_json(value: &serde_json::Value) -> Result<(), String> {
    match value {
        serde_json::Value::Null => Err("Nonfinite or uninitialized native adjustment".into()),
        serde_json::Value::Array(items) => items.iter().try_for_each(finite_json),
        serde_json::Value::Object(items) => items.values().try_for_each(finite_json),
        _ => Ok(()),
    }
}

fn validate_adjustment_ranges(value: &serde_json::Value) -> Result<(), String> {
    let fields = value.as_object().ok_or("Adjustment object required")?;
    for (name, value) in fields {
        if matches!(name.as_str(), "vignetteStyle" | "pointCurve" | "pointCurveRed" | "pointCurveGreen" | "pointCurveBlue") {
            continue;
        }
        if name.starts_with("grade") && value.is_object() {
            for (key, min, max) in [("hue", 0.0, 360.0), ("sat", 0.0, 100.0), ("lum", -100.0, 100.0)] {
                let number = value.get(key).and_then(|v| v.as_f64()).ok_or("Invalid colour wheel")?;
                if !(min..=max).contains(&number) {
                    return Err(format!("Invalid {name}.{key}"));
                }
            }
            continue;
        }
        let (min, max) = match name.as_str() {
            "exposure" => (-5.0, 5.0),
            "seed" => (0.0, u32::MAX as f64),
            "encodingGamma" => (0.0, 10.0),
            "pixelScale" => (1.0, 1.0),
            "sharpenRadius" => (0.5, 3.0),
            "sharpenAmount" => (0.0, 150.0),
            "curveSplits"
            | "gradeBlending"
            | "sharpenDetail"
            | "sharpenMasking"
            | "noiseLuminance"
            | "noiseLuminanceDetail"
            | "noiseColor"
            | "noiseColorDetail"
            | "grainAmount"
            | "grainSize"
            | "grainRoughness"
            | "vignetteMidpoint"
            | "vignetteFeather"
            | "vignetteHighlights" => (0.0, 100.0),
            "temperature" | "tint" | "contrast" | "highlights" | "shadows" | "whites" | "blacks" | "texture" | "clarity" | "dehaze" | "vibrance"
            | "saturation" | "curveHighlights" | "curveLights" | "curveDarks" | "curveShadows" | "hslHue" | "hslSat" | "hslLum" | "gradeBalance"
            | "vignetteAmount" | "vignetteRoundness" => (-100.0, 100.0),
            _ => return Err(format!("No persistence validation for adjustment {name}")),
        };
        let valid = |v: &serde_json::Value| v.as_f64().is_some_and(|n| (min..=max).contains(&n));
        if !value.as_array().map_or_else(|| valid(value), |items| items.iter().all(valid)) {
            return Err(format!("Native adjustment {name} is outside its supported range"));
        }
    }
    Ok(())
}

fn required_shape(expected: &serde_json::Value, actual: &serde_json::Value) -> Result<(), String> {
    if let serde_json::Value::Object(fields) = expected {
        let found = actual.as_object().ok_or("Native adjustment object required")?;
        if fields.len() != found.len() {
            return Err("Incomplete or unsupported native adjustment fields".into());
        }
        for (key, shape) in fields {
            required_shape(shape, found.get(key).ok_or_else(|| format!("Missing native field {key}"))?)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Admission {
    Pending,
    Ready(PhotoId),
    Rejected(String),
    Cancelled,
    Duplicate(PhotoId),
    Unresolved(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Photo {
    pub id: PhotoId,
    pub working: NativeRecipe,
    /// None means an unresolved baseline, never an implicitly clean photograph.
    pub committed: Option<NativeRecipe>,
    pub entry: NativeRecipe,
    pub revision: u64,
    pub hidden: bool,
    pub selected: bool,
    pub committed_this_visit: bool,
    pub unknown_operation: bool,
}

impl Photo {
    pub fn pending(&self) -> Result<bool, String> {
        self.working.validate()?;
        let baseline = self.committed.as_ref().ok_or("Unresolved XMP baseline")?;
        baseline.validate()?;
        Ok(self.working != *baseline)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Editing,
    Settling,
    Frozen,
    Committing,
    Paused,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub schema: u32,
    pub id: String,
    pub epoch: u64,
    pub generation: u64,
    pub cached_generation: Option<u64>,
    pub phase: Phase,
    /// Vector order is admission order; filters never change it.
    pub photos: Vec<Photo>,
    pub requested: BTreeMap<String, Admission>,
    pub sources: BTreeMap<PhotoId, SourceBinding>,
    pub removed: BTreeSet<PhotoId>,
    /// Versioned host view context; never participates in native recipe equality.
    #[serde(default)]
    pub view_context: serde_json::Value,
}

impl Session {
    pub fn begin_finish(&mut self) -> Result<(), String> {
        if self.phase != Phase::Editing {
            return Err("Session is already finishing".into());
        }
        self.phase = Phase::Settling;
        Ok(())
    }

    pub fn freeze(&mut self) -> Result<Vec<PhotoId>, String> {
        if self.phase != Phase::Settling {
            return Err("Admission must settle before freezing".into());
        }
        if self.requested.values().any(|a| matches!(a, Admission::Pending | Admission::Unresolved(_))) {
            return Err("Requested photographs have not settled".into());
        }
        if self.photos.len() > MAX_PHOTOS {
            return Err("Too many session photographs".into());
        }
        let mut pending = Vec::new();
        for photo in &self.photos {
            if !self.removed.contains(&photo.id) && photo.pending()? {
                pending.push(photo.id.clone());
            }
        }
        // Phase is persisted state. A previously acknowledged Editing snapshot
        // must not satisfy the frozen finish barrier under the same generation.
        self.generation = self.generation.checked_add(1).ok_or("Generation exhausted")?;
        self.phase = Phase::Frozen;
        Ok(pending)
    }

    pub fn acknowledge_cache(&mut self, session: &str, epoch: u64, generation: u64) -> bool {
        if session != self.id
            || epoch != self.epoch
            || generation > self.generation
            || matches!(self.phase, Phase::Completed | Phase::Paused)
            || self.cached_generation.is_some_and(|g| generation <= g)
        {
            return false;
        }
        self.cached_generation = Some(generation);
        true
    }

    pub fn current_cached(&self) -> bool {
        self.cached_generation == Some(self.generation)
    }

    /// All targets are validated before any mutation: a group action is coherent.
    pub fn apply(&mut self, changes: Vec<(PhotoId, NativeRecipe)>) -> Result<(), String> {
        if self.phase != Phase::Editing {
            return Err("Edits are frozen while finishing".into());
        }
        let mut ids = BTreeSet::new();
        for (id, recipe) in &changes {
            recipe.validate()?;
            if !ids.insert(id) || self.removed.contains(id) || !self.photos.iter().any(|p| &p.id == id) {
                return Err("Invalid, duplicate or removed edit target".into());
            }
        }
        let changed = changes.iter().any(|(id, recipe)| self.photos.iter().any(|p| &p.id == id && &p.working != recipe));
        if !changed {
            return Ok(());
        }
        let generation = self.generation.checked_add(1).ok_or("Generation exhausted")?;
        if self.photos.iter().any(|p| changes.iter().any(|(id, r)| *id == p.id && *r != p.working) && p.revision == u64::MAX) {
            return Err("Photo revision exhausted".into());
        }
        for (id, recipe) in changes {
            if let Some(photo) = self.photos.iter_mut().find(|p| p.id == id)
                && photo.working != recipe
            {
                photo.working = recipe;
                photo.revision += 1;
            }
        }
        self.generation = generation;
        Ok(())
    }

    /// The controller must durably publish this result before dropping live state.
    /// Unknown operations block discard until reconciled; successes are not undone.
    pub fn discarded_visit(&self) -> Result<Self, String> {
        if self.photos.iter().any(|p| p.unknown_operation) {
            return Err("Reconcile uncertain XMP operations before discard".into());
        }
        let mut restored = self.clone();
        restored.photos.retain(|p| !restored.removed.contains(&p.id));
        for photo in &mut restored.photos {
            photo.working = if photo.committed_this_visit { photo.committed.clone().ok_or("Successful commit has no baseline")? } else { photo.entry.clone() };
        }
        restored.generation = self.generation.checked_add(1).ok_or("Generation exhausted")?;
        restored.phase = Phase::Paused;
        Ok(restored)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn recipe(exposure: f32) -> NativeRecipe {
        NativeRecipe {
            schema: 1,
            processing: Processing {
                version: 1,
                source_kind: crate::camera_raw_source::SourceKind::Raw,
                demosaic: "ahd".into(),
                white_balance: WhiteBalance::AsShot,
                orient: true,
                camera_matrix: None,
                camera_tone_curve: vec![],
            },
            adjustments: CameraRaw { exposure, ..Default::default() },
            metadata: Metadata { rating: Some(0), label: "Yellow".into(), marked: false },
        }
    }
    pub(super) fn session() -> Session {
        let id = PhotoId("photo-one".into());
        Session {
            schema: 1,
            id: "session-one".into(),
            epoch: 1,
            generation: 1,
            cached_generation: None,
            phase: Phase::Editing,
            photos: vec![Photo {
                id: id.clone(),
                working: recipe(1.0),
                committed: Some(recipe(0.0)),
                entry: recipe(1.0),
                revision: 1,
                hidden: true,
                selected: false,
                committed_this_visit: false,
                unknown_operation: false,
            }],
            requested: BTreeMap::from([("input".into(), Admission::Ready(id))]),
            sources: BTreeMap::new(),
            removed: BTreeSet::new(),
            view_context: serde_json::Value::Null,
        }
    }
    #[test]
    fn complete_recipe_roundtrip_and_missing_fields_rejected() {
        let mut r = recipe(0.7853281);
        r.adjustments.point_curve = vec![[0., 0.], [64., 93.], [255., 255.]];
        r.metadata.marked = true;
        r.processing.white_balance = WhiteBalance::Multipliers([1.23456789, 1., 2.3456789]);
        assert_eq!(NativeRecipe::decode(&r.encode().unwrap()).unwrap(), r);
        let mut value = serde_json::to_value(&r).unwrap();
        value["adjustments"].as_object_mut().unwrap().remove("grainSize");
        assert!(NativeRecipe::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn every_supported_adjustment_survives_native_xmp_and_missing_fields_fail_closed() {
        use photocraft_algo::camera_raw::Wheel;
        let baseline = recipe(0.0);
        let mut complete = baseline.clone();
        complete.adjustments = CameraRaw {
            temperature: -100.0,
            tint: 100.0,
            exposure: 4.875,
            contrast: -91.0,
            highlights: -72.0,
            shadows: 83.0,
            whites: 64.0,
            blacks: -55.0,
            texture: 36.0,
            clarity: -27.0,
            dehaze: 18.0,
            vibrance: -19.0,
            saturation: 10.0,
            curve_highlights: 81.0,
            curve_lights: -62.0,
            curve_darks: 43.0,
            curve_shadows: -24.0,
            curve_splits: [20.0, 45.0, 80.0],
            point_curve: vec![[0.0, 2.0], [100.0, 124.0], [255.0, 252.0]],
            point_curve_red: vec![[0.0, 3.0], [255.0, 254.0]],
            point_curve_green: vec![[0.0, 4.0], [255.0, 253.0]],
            point_curve_blue: vec![[0.0, 5.0], [255.0, 251.0]],
            hsl_hue: [-100.0, -75.0, -50.0, -25.0, 25.0, 50.0, 75.0, 100.0],
            hsl_sat: [100.0, 75.0, 50.0, 25.0, -25.0, -50.0, -75.0, -100.0],
            hsl_lum: [-80.0, -60.0, -40.0, -20.0, 20.0, 40.0, 60.0, 80.0],
            grade_shadows: Wheel { hue: 359.0, sat: 99.0, lum: -88.0 },
            grade_midtones: Wheel { hue: 90.0, sat: 62.0, lum: 48.0 },
            grade_highlights: Wheel { hue: 181.0, sat: 31.0, lum: -27.0 },
            grade_global: Wheel { hue: 274.0, sat: 18.0, lum: 12.0 },
            grade_blending: 73.0,
            grade_balance: -42.0,
            sharpen_amount: 150.0,
            sharpen_radius: 3.0,
            sharpen_detail: 100.0,
            sharpen_masking: 87.0,
            noise_luminance: 63.0,
            noise_luminance_detail: 91.0,
            noise_color: 42.0,
            noise_color_detail: 79.0,
            grain_amount: 27.0,
            grain_size: 39.0,
            grain_roughness: 83.0,
            vignette_amount: -74.0,
            vignette_midpoint: 61.0,
            vignette_roundness: -33.0,
            vignette_feather: 82.0,
            vignette_highlights: 47.0,
            vignette_style: "colorPriority".into(),
            seed: u32::MAX,
            pixel_scale: 1.0,
            encoding_gamma: 1.8,
        };
        complete.processing.demosaic = "mhc".into();
        complete.processing.white_balance = WhiteBalance::Multipliers([1.23456789, 1.0, 2.3456789]);
        complete.processing.orient = false;
        complete.processing.camera_matrix = Some([[1.0, 0.1, 0.0], [0.0, 0.9, 0.1], [0.1, 0.0, 1.1]]);
        complete.processing.camera_tone_curve = vec![[0.0, 0.0], [0.25, 0.3], [1.0, 1.0]];
        complete.metadata = Metadata { rating: Some(5), label: "Yellow & literal <review>".into(), marked: true };
        let packet = super::xmp::plan(None, &baseline, &complete).unwrap().unwrap();
        assert_eq!(super::xmp::load_native(&packet).unwrap(), Some(complete.clone()));
        let value = serde_json::to_value(&complete).unwrap();
        let baseline_value = serde_json::to_value(&baseline).unwrap();
        for field in value["adjustments"].as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing["adjustments"].as_object_mut().unwrap().remove(field);
            assert!(NativeRecipe::decode(&serde_json::to_vec(&missing).unwrap()).is_err(), "missing {field} must never default");
            let mut single = baseline_value.clone();
            if field == "pixelScale" {
                // Preview scale is a fixed full-resolution persistence invariant,
                // not an editable photographic adjustment.
                single["adjustments"][field] = serde_json::json!(0.5);
                assert!(NativeRecipe::decode(&serde_json::to_vec(&single).unwrap()).is_err());
            } else {
                assert_ne!(value["adjustments"][field], baseline_value["adjustments"][field], "fixture must change {field}");
                single["adjustments"][field] = value["adjustments"][field].clone();
                assert_single_field_dirty_and_reset(&baseline, &NativeRecipe::decode(&serde_json::to_vec(&single).unwrap()).unwrap(), field);
            }
        }
        for field in ["rating", "label", "marked"] {
            let mut single = baseline_value.clone();
            single["metadata"][field] = value["metadata"][field].clone();
            assert_single_field_dirty_and_reset(&baseline, &NativeRecipe::decode(&serde_json::to_vec(&single).unwrap()).unwrap(), field);
        }
        for field in ["demosaic", "white_balance", "orient", "camera_matrix", "camera_tone_curve"] {
            let mut single = baseline_value.clone();
            single["processing"][field] = value["processing"][field].clone();
            let mut prior = baseline.clone();
            if field == "camera_tone_curve" {
                // A fitted tone curve requires its camera matrix; both sides use
                // that same matrix so only the curve contributes to dirtiness.
                prior.processing.camera_matrix = complete.processing.camera_matrix;
                single["processing"]["camera_matrix"] = value["processing"]["camera_matrix"].clone();
            }
            assert_single_field_dirty_and_reset(&prior, &NativeRecipe::decode(&serde_json::to_vec(&single).unwrap()).unwrap(), field);
        }
        for wheel in ["gradeShadows", "gradeMidtones", "gradeHighlights", "gradeGlobal"] {
            for field in ["hue", "sat", "lum"] {
                let mut missing = value.clone();
                missing["adjustments"][wheel].as_object_mut().unwrap().remove(field);
                assert!(NativeRecipe::decode(&serde_json::to_vec(&missing).unwrap()).is_err());
            }
        }
        assert!(super::xmp::plan(Some(&packet), &complete, &complete).unwrap().is_none());
        complete.adjustments.vignette_style = "x".repeat(MAX_RECIPE_BYTES + 1);
        assert!(complete.validate().is_err());
    }

    fn assert_single_field_dirty_and_reset(baseline: &NativeRecipe, changed: &NativeRecipe, field: &str) {
        assert_ne!(baseline, changed, "fixture must change {field}");
        let mut s = session();
        s.photos[0].working = baseline.clone();
        s.photos[0].entry = baseline.clone();
        s.photos[0].committed = Some(baseline.clone());
        let id = s.photos[0].id.clone();
        assert!(!s.photos[0].pending().unwrap());
        s.apply(vec![(id.clone(), changed.clone())]).unwrap();
        assert!(s.photos[0].pending().unwrap(), "{field} alone must make the hidden unselected photograph pending");
        s.begin_finish().unwrap();
        assert_eq!(s.freeze().unwrap(), vec![id]);
        let packet = super::xmp::plan(None, baseline, changed).unwrap().expect("one field must produce an XMP plan");
        assert_eq!(super::xmp::load_native(&packet).unwrap().as_ref(), Some(changed), "{field} roundtrip");
        assert!(super::xmp::plan(Some(&packet), changed, changed).unwrap().is_none(), "{field} no-op preserves bytes");
        let reset = super::xmp::plan(Some(&packet), changed, baseline).unwrap().expect("reset is a meaningful saved change");
        assert_eq!(super::xmp::load_native(&reset).unwrap().as_ref(), Some(baseline), "{field} reset roundtrip");
    }
    #[test]
    fn nonfinite_and_future_recipe_rejected() {
        let mut r = recipe(f32::NAN);
        assert!(r.encode().is_err());
        r.adjustments.exposure = 0.;
        r.schema = 2;
        assert!(r.encode().is_err());
    }
    #[test]
    fn hidden_unselected_recovered_edits_are_pending_and_imports_block_freeze() {
        let mut s = session();
        s.requested.insert("delayed".into(), Admission::Pending);
        s.begin_finish().unwrap();
        assert!(s.freeze().is_err());
        s.requested.insert("delayed".into(), Admission::Cancelled);
        assert_eq!(s.freeze().unwrap(), vec![PhotoId("photo-one".into())]);
        assert!(s.apply(vec![(PhotoId("photo-one".into()), recipe(2.0))]).is_err());
    }
    #[test]
    fn old_ack_does_not_mark_new_generation_saved() {
        let mut s = session();
        s.apply(vec![(PhotoId("photo-one".into()), recipe(2.0))]).unwrap();
        assert!(s.acknowledge_cache("session-one", 1, 1));
        assert!(!s.current_cached());
        assert!(!s.acknowledge_cache("session-one", 2, 2));
        assert!(s.acknowledge_cache("session-one", 1, 2));
        assert!(s.current_cached());
    }
    #[test]
    fn cancel_preserves_entry_successes_and_uncertain_work() {
        let mut s = session();
        s.photos[0].working = recipe(2.);
        assert_eq!(s.discarded_visit().unwrap().photos[0].working, recipe(1.));
        s.photos[0].committed_this_visit = true;
        s.photos[0].committed = Some(recipe(1.5));
        assert_eq!(s.discarded_visit().unwrap().photos[0].working, recipe(1.5));
        s.photos[0].unknown_operation = true;
        assert!(s.discarded_visit().is_err());
    }
    #[test]
    fn exact_undo_is_clean_and_invalid_group_action_is_atomic() {
        let mut s = session();
        let before = s.photos[0].working.clone();
        assert!(s.apply(vec![(PhotoId("photo-one".into()), recipe(2.)), (PhotoId("missing".into()), recipe(3.))]).is_err());
        assert_eq!(s.photos[0].working, before);
        s.apply(vec![(PhotoId("photo-one".into()), recipe(0.))]).unwrap();
        assert!(!s.photos[0].pending().unwrap());
        s.photos[0].committed = None;
        assert!(s.photos[0].pending().is_err());
    }
}
