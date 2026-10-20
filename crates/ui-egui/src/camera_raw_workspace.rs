//! Retained Camera Raw review workspace, independently selectable from persistence.
//! Adapted from PhotoCraft candidate-source-02's selection/parked-review design.
//! This port deliberately owns no XMP writer or recovery cache.
use super::*;
use photocraft_doc::DocId;
use std::collections::VecDeque;

#[derive(Default)]
pub struct Workspace {
    pub(super) queued: VecDeque<RawOpen>,
    parked: Vec<CameraRawDialog>,
    pub(super) selected: Vec<DocId>,
}

pub(super) fn entries(app: &PhotocraftApp) -> Vec<(DocId, String, bool, Option<TextureHandle>)> {
    let mut out = Vec::new();
    for d in app.camera_raw.iter().chain(app.raw_workspace.parked.iter()) {
        if let Some(raw) = &d.raw_open {
            out.push((raw.document, raw.name.clone(), app.raw_workspace.selected.contains(&raw.document), d.tex.clone()));
        }
    }
    for raw in app.pending_raw_open.iter().chain(app.raw_workspace.queued.iter()) {
        out.push((raw.document, raw.name.clone(), app.raw_workspace.selected.contains(&raw.document), None));
    }
    // DocIds are allocated at admission and stay stable while reviewing. Changing the
    // active preview must not reorder the strip.
    out.sort_by_key(|entry| entry.0.0);
    out
}

pub(super) fn advance(app: &mut PhotocraftApp) {
    if app.camera_raw.is_some() || app.raw_redevelop.is_some() {
        return;
    }
    if let Some(dialog) = app.raw_workspace.parked.pop() {
        if let Some(index) = dialog.raw_open.as_ref().and_then(|r| document_index(app, r.document)) {
            app.session.set_active(index);
            app.camera_raw = Some(dialog);
        } else {
            app.raw_workspace.parked.push(dialog);
        }
    } else if app.pending_raw_open.is_none() {
        app.pending_raw_open = app.raw_workspace.queued.pop_front();
    }
}

pub(super) fn activate(app: &mut PhotocraftApp, ctx: &egui::Context, id: DocId, toggle: bool) -> Result<(), String> {
    if app.camera_raw.as_ref().is_some_and(|d| d.target != Target::OpenRaw) {
        return Err("Finish Camera Raw Filter before activating a review photograph".into());
    }
    if app.raw_redevelop.is_some() {
        return Err("Wait for the current photograph to finish opening".into());
    }
    let index = document_index(app, id).ok_or("The review photograph is no longer open")?;
    if !entries(app).iter().any(|e| e.0 == id) {
        return Err("The photograph is not in this review".into());
    }
    if app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.document) != Some(id) {
        if let Some(position) = app.raw_workspace.parked.iter().position(|d| d.raw_open.as_ref().is_some_and(|r| r.document == id)) {
            let next = app.raw_workspace.parked.remove(position);
            if let Some(previous) = app.camera_raw.replace(next) {
                app.raw_workspace.parked.push(previous);
            }
            app.session.set_active(index);
        } else {
            let raw =
                app.pending_raw_open.iter().chain(app.raw_workspace.queued.iter()).find(|r| r.document == id).cloned().ok_or("The photograph is not queued")?;
            let previous = app.camera_raw.take();
            let old_active = previous.as_ref().and_then(|d| d.raw_open.as_ref()).and_then(|r| document_index(app, r.document));
            if let Err(error) = open_raw(app, ctx, raw) {
                app.camera_raw = previous;
                if let Some(old) = old_active {
                    app.session.set_active(old);
                }
                return Err(error);
            }
            if let Some(previous) = previous {
                app.raw_workspace.parked.push(previous);
            }
            if app.pending_raw_open.as_ref().is_some_and(|r| r.document == id) {
                app.pending_raw_open = None;
            }
            app.raw_workspace.queued.retain(|r| r.document != id);
        }
    }
    if toggle {
        if app.raw_workspace.selected.contains(&id) {
            app.raw_workspace.selected.retain(|v| *v != id);
        } else {
            app.raw_workspace.selected.push(id);
        }
    } else {
        app.raw_workspace.selected = vec![id];
    }
    Ok(())
}

pub(super) fn discard_retained(app: &mut PhotocraftApp) {
    // Never include unrelated editor documents. Resolve each identity immediately
    // before closing because removing earlier tabs shifts indices.
    let ids: Vec<_> = app
        .raw_workspace
        .parked
        .iter()
        .filter_map(|d| d.raw_open.as_ref().map(|r| r.document))
        .chain(app.pending_raw_open.iter().map(|r| r.document))
        .chain(app.raw_workspace.queued.iter().map(|r| r.document))
        .collect();
    for id in ids {
        if let Some(index) = document_index(app, id) {
            app.session.close(index);
        }
    }
    app.pending_raw_open = None;
    app.raw_workspace = Workspace::default();
}

pub(super) fn control(app: &mut PhotocraftApp, ctx: &egui::Context, request: &Value) -> Result<Value, String> {
    let object = request.as_object().ok_or("workspace must be an object")?;
    if object.keys().any(|k| k != "activate" && k != "toggle") {
        return Err("Unknown workspace property".into());
    }
    if let Some(value) = object.get("toggle")
        && !value.is_boolean()
    {
        return Err("toggle must be a boolean".into());
    }
    if let Some(value) = object.get("activate") {
        let id = value.as_u64().ok_or("activate must be a document id")?;
        activate(app, ctx, DocId(id), request.get("toggle").and_then(Value::as_bool).unwrap_or(false))?;
    }
    Ok(snapshot(app))
}

/// Current retained sources and recipes; queued photos still have default adjustments.
/// This is a read-only seam for a separately reviewed saving adapter.
pub(super) fn retained_reviews(app: &PhotocraftApp) -> Vec<(RawOpen, CameraRaw)> {
    let mut reviews: Vec<_> =
        app.camera_raw.iter().chain(app.raw_workspace.parked.iter()).filter_map(|d| d.raw_open.as_ref().map(|r| (r.clone(), d.params.clone()))).collect();
    reviews.extend(app.pending_raw_open.iter().chain(app.raw_workspace.queued.iter()).map(|r| (r.clone(), CameraRaw::default())));
    if let Some(pending) = &app.raw_redevelop {
        reviews.push((pending.raw.clone(), pending.params.clone()));
    }
    reviews.sort_by_key(|(r, _)| r.document.0);
    reviews.dedup_by_key(|(r, _)| r.document.0);
    reviews
}

pub(super) fn snapshot(app: &PhotocraftApp) -> Value {
    json!({"photos": retained_reviews(app).iter().map(|(raw, params)| json!({
        "document": raw.document.0, "name": raw.name, "path": raw.path,
        "hasSourceBytes": raw.bytes.is_some(), "params": params,
        "selected": app.raw_workspace.selected.contains(&raw.document)
    })).collect::<Vec<_>>(),
        "active": app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.document.0),
        "pendingImports": app.jobs.opens.len(), "redeveloping": app.raw_redevelop.is_some(),
        "ready": app.jobs.opens.is_empty() && app.raw_redevelop.is_none()
    })
}
