//! Pure Camera Raw group-edit preparation. Every target is prepared and validated
//! before the workspace applies any of them, so a group has one history entry.

use photocraft_algo::camera_raw::CameraRaw;
use photocraft_io::raw_session::Metadata;
use serde_json::Value;

use super::{PhotoId, filmstrip_ui::PhotoAction};

/// Exact snapshots for coherent application and undo; no document or persistence
/// state is mutated while these changes are being prepared.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GroupChange {
    pub before: Vec<(PhotoId, CameraRaw)>,
    pub after: Vec<(PhotoId, CameraRaw)>,
}

impl GroupChange {
    pub(super) fn is_empty(&self) -> bool {
        self.before == self.after
    }
}

fn prepare_group(targets: &[(PhotoId, CameraRaw)], update: impl Fn(&CameraRaw) -> Result<CameraRaw, String>) -> Result<GroupChange, String> {
    let mut after = Vec::with_capacity(targets.len());
    for (id, params) in targets {
        if after.iter().any(|(previous, _)| previous == id) {
            return Err("A Camera Raw group contains the same photo twice".into());
        }
        let next = update(params)?;
        validate_numbers(&serde_json::to_value(&next).map_err(|e| e.to_string())?, "", 0)?;
        next.validate_changes(params)?;
        after.push((*id, next));
    }
    Ok(GroupChange { before: targets.to_vec(), after })
}

fn validate_numbers(value: &Value, path: &str, depth: u8) -> Result<(), String> {
    if depth > 4 {
        return Err("Unsupported Camera Raw parameter nesting".into());
    }
    if matches!(path, "pointCurve" | "pointCurveRed" | "pointCurveGreen" | "pointCurveBlue") {
        return Ok(());
    }
    match value {
        Value::Object(fields) => {
            for (name, child) in fields {
                let next = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
                validate_numbers(child, &next, depth + 1)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                validate_numbers(child, path, depth + 1)?;
            }
        }
        Value::Number(number) => {
            let (min, max) = match path {
                "pixelScale" => (0.01, 1.0),
                "encodingGamma" => (0.0, 10.0),
                "seed" => (0.0, f64::from(u32::MAX)),
                _ => bounds(path)?,
            };
            if !number.as_f64().is_some_and(|number| number.is_finite() && (min..=max).contains(&number)) {
                return Err(format!("Camera Raw parameter {path} is outside its valid range"));
            }
        }
        Value::String(style) if path == "vignetteStyle" && matches!(style.as_str(), "highlightPriority" | "colorPriority" | "paintOverlay") => {}
        _ => return Err(format!("Invalid Camera Raw parameter {path}")),
    }
    Ok(())
}

/// Ordinary slider semantics: preserve each target's existing difference, applying
/// only the fields changed by the active gesture. Point-curve editing and style
/// choices set that one property; they never copy the active photo's whole recipe.
pub(super) fn relative_group(before: &CameraRaw, after: &CameraRaw, targets: &[(PhotoId, CameraRaw)]) -> Result<GroupChange, String> {
    let before = serde_json::to_value(before).map_err(|e| e.to_string())?;
    let after = serde_json::to_value(after).map_err(|e| e.to_string())?;
    prepare_group(targets, |target| {
        let mut result = serde_json::to_value(target).map_err(|e| e.to_string())?;
        apply_changed_values(&before, &after, &mut result, "", 0)?;
        serde_json::from_value(result).map_err(|e| e.to_string())
    })
}

fn bounds(path: &str) -> Result<(f64, f64), String> {
    let top = path.split('.').next().ok_or("Missing Camera Raw parameter")?;
    if matches!(top, "gradeShadows" | "gradeMidtones" | "gradeHighlights" | "gradeGlobal") {
        return match path.rsplit('.').next() {
            Some("hue") => Ok((0.0, 360.0)),
            Some("sat") => Ok((0.0, 100.0)),
            Some("lum") => Ok((-100.0, 100.0)),
            _ => Err(format!("Unknown colour-grading parameter {path}")),
        };
    }
    match top {
        "exposure" => Ok((-5.0, 5.0)),
        "sharpenRadius" => Ok((0.5, 3.0)),
        "sharpenAmount" => Ok((0.0, 150.0)),
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
        | "vignetteHighlights" => Ok((0.0, 100.0)),
        "temperature" | "tint" | "contrast" | "highlights" | "shadows" | "whites" | "blacks" | "texture" | "clarity" | "dehaze" | "vibrance" | "saturation"
        | "curveHighlights" | "curveLights" | "curveDarks" | "curveShadows" | "hslHue" | "hslSat" | "hslLum" | "gradeBalance" | "vignetteAmount"
        | "vignetteRoundness" => Ok((-100.0, 100.0)),
        _ => Err(format!("Camera Raw parameter {path} has no supported slider range")),
    }
}

fn apply_changed_values(before: &Value, after: &Value, target: &mut Value, path: &str, depth: u8) -> Result<(), String> {
    if depth > 4 {
        return Err("Camera Raw parameter nesting is unsupported".into());
    }
    if before == after || matches!(path, "pixelScale" | "encodingGamma" | "seed") {
        return Ok(());
    }
    if matches!(path, "pointCurve" | "pointCurveRed" | "pointCurveGreen" | "pointCurveBlue") {
        *target = after.clone();
        return Ok(());
    }
    match (before, after, target) {
        (Value::Object(from), Value::Object(to), Value::Object(output)) => {
            if from.len() != to.len() || from.len() != output.len() {
                return Err("Camera Raw parameter shapes differ".into());
            }
            for (key, after) in to {
                let before = from.get(key).ok_or_else(|| format!("Missing Camera Raw parameter {key}"))?;
                let output = output.get_mut(key).ok_or_else(|| format!("Missing group target parameter {key}"))?;
                let next = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                apply_changed_values(before, after, output, &next, depth + 1)?;
            }
        }
        (Value::Array(from), Value::Array(to), Value::Array(output)) => {
            if from.len() != to.len() || from.len() != output.len() {
                return Err(format!("Camera Raw array shape differs at {path}"));
            }
            for ((before, after), output) in from.iter().zip(to).zip(output.iter_mut()) {
                apply_changed_values(before, after, output, path, depth + 1)?;
            }
        }
        (Value::Number(from), Value::Number(to), output @ Value::Number(_)) => {
            let from = from.as_f64().filter(|v| v.is_finite()).ok_or("Invalid starting adjustment")?;
            let to = to.as_f64().filter(|v| v.is_finite()).ok_or("Invalid ending adjustment")?;
            let original = output.as_f64().filter(|v| v.is_finite()).ok_or("Invalid target adjustment")?;
            let (min, max) = bounds(path)?;
            if !(min..=max).contains(&from) || !(min..=max).contains(&to) || !(min..=max).contains(&original) {
                return Err(format!("Camera Raw parameter {path} is outside its valid range"));
            }
            *output = Value::from((original + to - from).clamp(min, max));
        }
        (Value::String(_), Value::String(value), output) if path == "vignetteStyle" => {
            if !matches!(value.as_str(), "highlightPriority" | "colorPriority" | "paintOverlay") {
                return Err("Unsupported Camera Raw vignette style".into());
            }
            *output = Value::String(value.clone());
        }
        _ => return Err(format!("Unsupported Camera Raw parameter change at {path}")),
    }
    Ok(())
}

/// Explicit Set value uses the same absolute value even when the active value was
/// already equal. Paths are typed JSON field names, with `.index` for HSL arrays.
pub(super) fn set_value_group(path: &str, value: f32, targets: &[(PhotoId, CameraRaw)]) -> Result<GroupChange, String> {
    let (min, max) = bounds(path)?;
    if !value.is_finite() || !(min..=max).contains(&f64::from(value)) {
        return Err(format!("Set value for {path} must be finite and within {min}..={max}"));
    }
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() > 2 {
        return Err("Unsupported Camera Raw parameter path".into());
    }
    prepare_group(targets, |target| {
        let mut output = serde_json::to_value(target).map_err(|e| e.to_string())?;
        let mut field = &mut output;
        for part in &parts {
            field = match field {
                Value::Object(values) => values.get_mut(*part),
                Value::Array(values) => part.parse::<usize>().ok().and_then(|index| values.get_mut(index)),
                _ => None,
            }
            .ok_or_else(|| format!("Unknown Camera Raw parameter {path}"))?;
        }
        if !field.is_number() {
            return Err("Set value requires an individual numeric Camera Raw parameter".into());
        }
        *field = Value::from(value);
        serde_json::from_value(output).map_err(|e| e.to_string())
    })
}

/// Control-channel Set applies only the explicitly named fields to every target.
/// It is absolute even when the active photo already has the requested value.
pub(super) fn set_fields_group(fields: &Value, targets: &[(PhotoId, CameraRaw)]) -> Result<GroupChange, String> {
    let fields = fields.as_object().ok_or("Camera Raw Set needs a settings object")?;
    prepare_group(targets, |target| {
        let mut value = serde_json::to_value(target).map_err(|e| e.to_string())?;
        let output = value.as_object_mut().ok_or("Camera Raw settings are not an object")?;
        for (key, value) in fields {
            if matches!(key.as_str(), "pixelScale" | "encodingGamma") {
                return Err(format!("{key} is photograph processing context, not a group adjustment"));
            }
            *output.get_mut(key).ok_or_else(|| format!("Unknown Camera Raw setting {key}"))? = value.clone();
        }
        serde_json::from_value(value).map_err(|e| e.to_string())
    })
}

/// A preset overlays each photo independently. Unspecified adjustments keep their
/// own values, including when a preset value already equals the active photo.
pub(super) fn load_settings_group(bytes: &[u8], targets: &[(PhotoId, CameraRaw)]) -> Result<GroupChange, String> {
    prepare_group(targets, |target| {
        let mut next = target.clone();
        photocraft_io::camera_raw_xmp::load(&mut next, bytes)?;
        Ok(next)
    })
}

/// Explicit Synchronise/preset semantics copy adjustment settings while retaining
/// the target image's transfer encoding and preview scale.
pub(super) fn synchronise_group(source: &CameraRaw, targets: &[(PhotoId, CameraRaw)]) -> Result<GroupChange, String> {
    source.validate()?;
    prepare_group(targets, |target| {
        let mut next = source.clone();
        next.encoding_gamma = target.encoding_gamma;
        next.pixel_scale = target.pixel_scale;
        Ok(next)
    })
}

pub(super) fn reset_group(targets: &[(PhotoId, CameraRaw)]) -> Result<GroupChange, String> {
    synchronise_group(&CameraRaw::default(), targets)
}

/// Keyboard and context-menu actions use this same metadata transformation. The
/// controller checks the action's target IDs before calling it for each member.
pub(super) fn metadata_change(current: &Metadata, action: &PhotoAction) -> Result<Metadata, String> {
    let mut next = current.clone();
    match action {
        PhotoAction::SetRating { rating, .. } if *rating <= 5 => next.rating = Some(*rating as i8),
        PhotoAction::SetRating { .. } => return Err("A star rating must be between 0 and 5".into()),
        PhotoAction::SetLabel { label, .. } if label.len() <= 4096 => next.label = label.clone(),
        PhotoAction::SetLabel { .. } => return Err("The colour label is too long".into()),
        PhotoAction::SetMark { marked, .. } => next.marked = *marked,
        _ => return Err("This photo action does not edit metadata".into()),
    }
    Ok(next)
}

/// Returns true for an action handled here. JPEG and guarded Remove are routed to
/// their own workflows by the parent; Hide only changes presentation.
pub(super) fn route_photo_action(app: &mut crate::PhotocraftApp, action: &PhotoAction) -> Result<bool, String> {
    match action {
        PhotoAction::Hide { targets } => {
            app.raw_filmstrip.manually_hidden.extend(targets.iter().copied());
            Ok(true)
        }
        PhotoAction::ShowHidden => {
            app.raw_filmstrip.manually_hidden.clear();
            app.raw_filmstrip.view.filter = super::filmstrip_ui::RatingFilter::default();
            Ok(true)
        }
        PhotoAction::SetRating { .. } | PhotoAction::SetLabel { .. } | PhotoAction::SetMark { .. } => {
            let targets = match action {
                PhotoAction::SetRating { targets, .. } | PhotoAction::SetLabel { targets, .. } | PhotoAction::SetMark { targets, .. } => targets,
                _ => return Err("Not a metadata action".into()),
            };
            let present = super::filmstrip_raws(app);
            let mut staged = std::collections::BTreeMap::new();
            for id in targets {
                if staged.contains_key(id) || !present.iter().any(|raw| raw.photo == *id) {
                    return Err("Metadata target is missing or duplicated".into());
                }
                let before = app.raw_filmstrip.metadata.get(id).ok_or("Metadata target is no longer retained")?;
                staged.insert(*id, metadata_change(before, action)?);
            }
            app.raw_filmstrip.metadata.extend(staged);
            Ok(true)
        }
        PhotoAction::SyncSettings { .. } | PhotoAction::ResetSettings(_) | PhotoAction::SetValue { .. } | PhotoAction::UndoGroup => Ok(false),
    }
}

/// Snapshot selected targets, including hidden and not-yet-previewed admitted
/// members. A visible active photo outside the group is not silently added to it.
fn queued_baseline(raw: &super::RawOpen) -> CameraRaw {
    super::adjacent_raw_xmp(raw.path.as_deref()).ok().flatten().map(|imported| imported.params).unwrap_or_default()
}

pub(super) fn slider_group(app: &crate::PhotocraftApp, before: &CameraRaw, after: &CameraRaw) -> Result<GroupChange, String> {
    let active = app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|raw| raw.photo).ok_or("No Camera Raw photograph is active")?;
    let selected = app.raw_filmstrip.selection.selected_ids();
    let ids = if selected.is_empty() { vec![active] } else { selected.to_vec() };
    let targets = settings_targets(app, &ids, before)?;
    relative_group(before, after, &targets)
}

/// Resolve the menu's captured photograph identities, never the current selection.
/// All targets are checked before an edit is applied; missing or duplicate members
/// must not turn a one-photo Reset into a selected-group operation.
pub(super) fn settings_targets(app: &crate::PhotocraftApp, ids: &[PhotoId], active_params: &CameraRaw) -> Result<Vec<(PhotoId, CameraRaw)>, String> {
    let active = app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|raw| raw.photo).ok_or("No Camera Raw photograph is active")?;
    if ids.is_empty() {
        return Err("Choose at least one Camera Raw photograph".into());
    }
    let mut targets = Vec::with_capacity(ids.len());
    for &id in ids {
        if targets.iter().any(|(photo, _)| *photo == id) {
            return Err("A photograph appears more than once in the settings targets".into());
        }
        let params = if id == active {
            active_params.clone()
        } else if let Some(parked) = app.raw_filmstrip.parked.iter().find(|item| item.raw.photo == id) {
            parked.params.clone()
        } else {
            app.pending_raw_open
                .iter()
                .chain(app.queued_raw_opens.iter())
                .find(|raw| raw.photo == id)
                .map(queued_baseline)
                .ok_or("Selected photograph is no longer available")?
        };
        targets.push((id, params));
    }
    Ok(targets)
}

/// Update every captured display copy after the entire group has been validated.
/// Queued members are parked with their own original and edited settings.
pub(super) fn apply_display_group(app: &mut crate::PhotocraftApp, changes: &[(PhotoId, CameraRaw)]) {
    for (id, params) in changes {
        if let Some(dialog) = app.camera_raw.as_mut().filter(|d| d.raw_open.as_ref().is_some_and(|raw| raw.photo == *id)) {
            if dialog.params.point_curve != params.point_curve {
                dialog.curve_state = Default::default();
            }
            dialog.dirty |= dialog.params != *params;
            dialog.params = params.clone();
            continue;
        }
        if let Some(parked) = app.raw_filmstrip.parked.iter_mut().find(|item| item.raw.photo == *id) {
            parked.params = params.clone();
            continue;
        }
        {
            let raw = if app.pending_raw_open.as_ref().is_some_and(|raw| raw.photo == *id) {
                app.pending_raw_open.take()
            } else {
                app.queued_raw_opens.iter().position(|raw| raw.photo == *id).and_then(|index| app.queued_raw_opens.remove(index))
            };
            if let Some(raw) = raw {
                let original = queued_baseline(&raw);
                app.raw_filmstrip.parked.push_back(super::ParkedRawReview {
                    raw,
                    params: params.clone(),
                    original,
                    scope: Default::default(),
                    navigation: Default::default(),
                    show_before: false,
                    recover: false,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> Vec<(PhotoId, CameraRaw)> {
        vec![
            (PhotoId(1), CameraRaw { exposure: 0.5, contrast: 15.0, ..Default::default() }),
            (PhotoId(2), CameraRaw { exposure: -0.5, contrast: -20.0, ..Default::default() }),
        ]
    }

    #[test]
    fn relative_group_preserves_differences_unrelated_values_and_undo_snapshot() {
        let targets = targets();
        let before = targets[0].1.clone();
        let after = CameraRaw { exposure: 0.8, ..before.clone() };
        let change = relative_group(&before, &after, &targets).unwrap();
        assert!(!change.is_empty());
        assert!((change.after[0].1.exposure - 0.8).abs() < 1e-6);
        assert!((change.after[1].1.exposure + 0.2).abs() < 1e-6);
        assert_eq!(change.after[1].1.contrast, -20.0);
        assert_eq!(change.before, targets);
        assert_eq!(targets[1].1.exposure, -0.5, "preparing is side-effect free");
        assert_eq!(change.after.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [PhotoId(1), PhotoId(2)]);
    }

    #[test]
    fn relative_arrays_wheels_and_bounds_do_not_copy_whole_recipes() {
        let mut targets = targets();
        targets[0].1.hsl_hue[3] = 20.0;
        targets[1].1.hsl_hue[3] = 90.0;
        targets[1].1.grade_global.sat = 30.0;
        let before = targets[0].1.clone();
        let mut after = before.clone();
        after.hsl_hue[3] = 40.0;
        after.grade_global.sat = 5.0;
        let change = relative_group(&before, &after, &targets).unwrap();
        assert_eq!(change.after[1].1.hsl_hue[3], 100.0);
        assert_eq!(change.after[1].1.grade_global.sat, 35.0);
        assert_eq!(change.after[1].1.exposure, -0.5);
    }

    #[test]
    fn set_value_and_reset_have_absolute_semantics_even_without_active_change() {
        let targets = targets();
        let set = set_value_group("exposure", 0.5, &targets).unwrap();
        assert_eq!(set.after[0].1.exposure, 0.5);
        assert_eq!(set.after[1].1.exposure, 0.5);
        let reset = reset_group(&targets).unwrap();
        assert_eq!(reset.after[0].1.exposure, 0.0);
        assert_eq!(reset.after[1].1.contrast, 0.0);
        assert_eq!(reset.after[0].1.sharpen_radius, 1.0);
        assert!(set_value_group("sharpenRadius", 0.0, &targets).is_err());
        assert!(set_value_group("exposure", f32::NAN, &targets).is_err());
        assert!(set_value_group("hslHue.8", 0.0, &targets).is_err());
        assert_eq!(set_value_group("hslHue.3", 11.0, &targets).unwrap().after[1].1.hsl_hue[3], 11.0);
    }

    #[test]
    fn explicit_field_set_and_sparse_preset_apply_same_value_without_copying_unrelated_fields() {
        let targets = targets();
        let fields = set_fields_group(&serde_json::json!({"exposure": 0.5}), &targets).unwrap();
        assert_eq!(fields.after[0].1.exposure, 0.5);
        assert_eq!(fields.after[1].1.exposure, 0.5);
        assert_eq!(fields.after[1].1.contrast, -20.0);
        assert!(set_fields_group(&serde_json::json!({"exposure": 0.5, "contrast": 101}), &targets).is_err());
        assert!(set_fields_group(&serde_json::json!({"encodingGamma": 2.2}), &targets).is_err());
        let preset = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:RDF><rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:Exposure2012="+0.5"/></rdf:RDF></x:xmpmeta>"#;
        let loaded = load_settings_group(preset, &targets).unwrap();
        assert_eq!(loaded.after, fields.after);
        assert_eq!(loaded.before, targets);
    }

    #[test]
    fn failed_group_is_atomic_and_curves_only_replace_the_changed_curve() {
        let targets = targets();
        let before = targets[0].1.clone();
        let mut after = before.clone();
        after.point_curve = vec![[0.0, 3.0], [255.0, 250.0]];
        let changed = relative_group(&before, &after, &targets).unwrap();
        assert_eq!(changed.after[1].1.point_curve, after.point_curve);
        assert_eq!(changed.after[1].1.exposure, -0.5);
        after.point_curve = vec![[0.0, 0.0]];
        assert!(relative_group(&before, &after, &targets).is_err());
        assert!(targets[1].1.point_curve.is_empty());
        let duplicate = vec![targets[0].clone(), targets[0].clone()];
        assert!(reset_group(&duplicate).is_err());
    }

    #[test]
    fn synchronize_preserves_processing_context_and_metadata_ops_are_separate() {
        let mut targets = targets();
        targets[1].1.encoding_gamma = 1.8;
        targets[1].1.pixel_scale = 0.5;
        let change = synchronise_group(&targets[0].1, &targets).unwrap();
        assert_eq!(change.after[1].1.exposure, 0.5);
        assert_eq!(change.after[1].1.encoding_gamma, 1.8);
        assert_eq!(change.after[1].1.pixel_scale, 0.5);
        let old = Metadata { rating: Some(3), label: "Yellow".into(), marked: true };
        let clear_stars = PhotoAction::SetRating { targets: vec![PhotoId(1)], rating: 0 };
        let next = metadata_change(&old, &clear_stars).unwrap();
        assert_eq!(next, Metadata { rating: Some(0), ..old.clone() });
        let key = super::super::filmstrip_ui::metadata_shortcut(egui::Key::Num9, egui::Modifiers::NONE, vec![PhotoId(1)]).unwrap();
        let menu = PhotoAction::SetLabel { targets: vec![PhotoId(1)], label: "Review".into() };
        assert_eq!(metadata_change(&old, &key).unwrap(), metadata_change(&old, &menu).unwrap());
    }

    #[test]
    fn byte_only_group_changes_survive_first_preview_of_queued_photos() {
        let mut app = crate::PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        let ctx = egui::Context::default();
        for name in ["active.dng", "queued.dng", "hidden.dng"] {
            app.run("file.new", serde_json::json!({"width": 8, "height": 8})).unwrap();
            let document = app.session.active().unwrap().doc.id;
            super::super::queue_open_dialog(&mut app, document, name, None, None).unwrap();
        }
        let first = app.pending_raw_open.take().unwrap();
        super::super::open_raw(&mut app, &ctx, first).unwrap();
        let raws = super::super::filmstrip_raws(&app);
        let ids: Vec<_> = raws.iter().map(|raw| raw.photo).collect();
        app.raw_filmstrip.selection.select_all(&ids);
        app.raw_filmstrip.manually_hidden.insert(ids[2]);
        let before = app.camera_raw.as_ref().unwrap().params.clone();
        let after = CameraRaw { exposure: 0.3, ..before.clone() };
        let changed = slider_group(&app, &before, &after).unwrap();
        apply_display_group(&mut app, &changed.after);
        assert_eq!(app.raw_filmstrip.parked.len(), 2);
        assert!(app.queued_raw_opens.is_empty());
        assert!(app.raw_filmstrip.parked.iter().all(|parked| parked.original.exposure == 0.0 && parked.params.exposure == 0.3));
        super::super::select_raw_review(&mut app, &ctx, raws[2].document).unwrap();
        assert_eq!(app.camera_raw.as_ref().unwrap().params.exposure, 0.3);
        assert_eq!(app.raw_filmstrip.selection.selected_ids(), ids);
        assert_eq!(super::super::filmstrip_raws(&app).iter().map(|raw| raw.photo).collect::<Vec<_>>(), ids);
    }
}
