//! ACR-S2 Segment C and the S2-P0 safety correction: what the control channel may do to, and can see
//! of, an unaccepted RAW review. Synthetic in-memory DNGs only: no photograph, sidecar or other file
//! is read, and every write goes to an in-memory log. These tests use only the public control API.
//!
//! A refusal is never judged by its error alone: each one compares every document (the `Arc` of its
//! content, id, path, revisions, history depth), the active tab, the dialogs and the write log before
//! and after.
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use egui::{Pos2, RawInput, Rect, vec2};
use egui_kittest::Harness;
use photocraft_doc::{DocId, Document};
use photocraft_raw::testgen::{DngSpec, mosaic, scene};
use photocraft_ui_egui::{PhotocraftApp, Services, camera_raw_ui, control, theme::ThemeKind};
use serde_json::{Value, json};

type Writes = Rc<RefCell<Vec<String>>>;

/// Import plus in-memory writers: the user's writer (`file.save` and friends) and the automation
/// writer (`app.save`) both record into `writes` instead of touching a disk.
fn services(writes: &Writes) -> Services {
    let (automation, user) = (writes.clone(), writes.clone());
    Services {
        import: Some(Box::new(|name: &str, bytes: &[u8], depth: usize| {
            photocraft_io::import_with_svg_group_depth(name, bytes, depth).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
        })),
        export: Some(Box::new(|_doc, _path, _settings| Ok((vec![1, 2, 3, 4], vec![])))),
        automation_write: Some(Box::new(move |path: &str, bytes: &[u8]| {
            automation.borrow_mut().push(format!("automation {path} {}", bytes.len()));
            Ok(())
        })),
        write: Some(Box::new(move |path: &str, bytes: &[u8]| {
            user.borrow_mut().push(format!("user {path} {}", bytes.len()));
            Ok(())
        })),
        ..Default::default()
    }
}

fn dng() -> Vec<u8> {
    let (w, h) = (48, 32);
    let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
    spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
    spec.build()
}

/// Three RAWs open in Camera Raw. The synthetic files carry a profile that differs from the working
/// space, so each queues an Embedded Profile Mismatch prompt; `clear_prompts` answers them so the
/// gate under test is Camera Raw's own, not theirs.
fn review_app(writes: &Writes, clear_prompts: bool) -> (PhotocraftApp, egui::Context) {
    review_named(writes, ["one.dng", "two.dng", "three.dng"], clear_prompts)
}

fn review_named(writes: &Writes, names: [&str; 3], clear_prompts: bool) -> (PhotocraftApp, egui::Context) {
    let (mut app, ctx) = new_app(writes);
    open_review(&mut app, &ctx, names, clear_prompts);
    (app, ctx)
}

fn new_app(writes: &Writes) -> (PhotocraftApp, egui::Context) {
    new_app_with(writes, |_| {})
}

/// [`new_app`] with the services adjusted first (a hook, a reader, a save worker).
fn new_app_with(writes: &Writes, tweak: impl FnOnce(&mut Services)) -> (PhotocraftApp, egui::Context) {
    let mut services = services(writes);
    tweak(&mut services);
    let app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
    let ctx = egui::Context::default();
    PhotocraftApp::setup_context(&ctx, ThemeKind::Pro);
    (app, ctx)
}

/// Open three RAWs (Camera Raw opens its review) in an app that may already hold other documents.
fn open_review(app: &mut PhotocraftApp, ctx: &egui::Context, names: [&str; 3], clear_prompts: bool) {
    let dng = dng();
    for name in names {
        app.open_bytes(name, &dng).unwrap();
    }
    frames(app, ctx, 3);
    if clear_prompts {
        cancel_prompts(app, ctx);
        assert_eq!(control::inspect(app, ctx)["dialogs"].as_array().map(Vec::len), Some(0), "only Camera Raw is open");
    }
}
fn review_of_three() -> (PhotocraftApp, egui::Context) {
    review_app(&Writes::default(), true)
}

fn cancel_prompts(app: &mut PhotocraftApp, ctx: &egui::Context) {
    for _ in 0..6 {
        let open = control::inspect(app, ctx)["dialogs"].as_array().cloned().unwrap_or_default();
        for dialog in open {
            call(app, ctx, "ui.dialog.cancel", json!({"dialog": dialog["id"]}));
        }
        frames(app, ctx, 1);
    }
}

fn frames(app: &mut PhotocraftApp, ctx: &egui::Context, n: usize) {
    for _ in 0..n {
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1360.0, 900.0))), ..Default::default() };
        ctx.run_ui(input, |ui| camera_raw_ui::show(app, ui.ctx())).textures_delta.clear();
    }
}

fn call(app: &mut PhotocraftApp, ctx: &egui::Context, method: &str, params: Value) -> Value {
    let (request, _reply) = control::ControlRequest::new(method, params);
    match control::handle(app, ctx, &request) {
        control::Outcome::Done(value) => value,
        _ => json!({"ok": false, "error": "the request did not complete synchronously"}),
    }
}

fn engine(app: &mut PhotocraftApp, ctx: &egui::Context, command: &str, params: Value) -> Value {
    call(app, ctx, "engine.execute", json!({"command": command, "params": params}))
}

/// The pixel the provisional RAW (the active document during review) currently has at (0, 0).
fn provisional_pixel(app: &PhotocraftApp) -> [f32; 4] {
    let st = app.session.active().unwrap();
    st.doc.layer(st.active_layer.unwrap()).unwrap().surface().unwrap().rgba(0, 0)
}

struct DocPrint {
    id: DocId,
    doc: Arc<Document>,
    path: Option<String>,
    revision: u64,
    saved_revision: u64,
    past: usize,
}

struct Print {
    docs: Vec<DocPrint>,
    active: Option<usize>,
    writes: Vec<String>,
    dialogs: usize,
}

fn print(app: &PhotocraftApp, ctx: &egui::Context, writes: &Writes) -> Print {
    Print {
        docs: app
            .session
            .documents()
            .iter()
            .map(|st| DocPrint {
                id: st.doc.id,
                doc: st.doc.clone(),
                path: st.path.clone(),
                revision: st.revision,
                saved_revision: st.saved_revision,
                past: st.history.past_len(),
            })
            .collect(),
        active: app.session.active_index(),
        writes: writes.borrow().clone(),
        dialogs: control::inspect(app, ctx)["dialogs"].as_array().map_or(0, Vec::len),
    }
}

thread_local! {
    /// Every unmet expectation of the running test. A refusal test goes through all of its routes
    /// and reports each one that was reachable, instead of stopping at the first.
    static VIOLATIONS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn violation(message: String) {
    VIOLATIONS.with(|v| v.borrow_mut().push(message));
}

fn violation_count() -> usize {
    VIOLATIONS.with(|v| v.borrow().len())
}

fn finish() {
    let all = VIOLATIONS.with(|v| std::mem::take(&mut *v.borrow_mut()));
    assert!(all.is_empty(), "{} violation(s):\n  {}", all.len(), all.join("\n  "));
}

fn assert_doc_same(before: &DocPrint, after: &DocPrint, what: &str) {
    if before.id != after.id {
        violation(format!("{what}: identity {:?} became {:?}", before.id, after.id));
    }
    if !Arc::ptr_eq(&before.doc, &after.doc) {
        violation(format!("{what}: the content of document {:?} was replaced", before.id));
    }
    let (b, a) = ((&before.path, before.revision, before.saved_revision, before.past), (&after.path, after.revision, after.saved_revision, after.past));
    if b != a {
        violation(format!("{what}: path, revisions or history of document {:?} changed: {b:?} -> {a:?}", before.id));
    }
}

/// Nothing at all changed: every document, the active tab, the dialogs and the writes.
fn assert_same(before: &Print, after: &Print, what: &str) {
    if before.docs.len() != after.docs.len() {
        violation(format!("{what}: document count {} -> {}", before.docs.len(), after.docs.len()));
    }
    for (b, a) in before.docs.iter().zip(&after.docs) {
        assert_doc_same(b, a, what);
    }
    if before.active != after.active {
        violation(format!("{what}: active tab {:?} -> {:?}", before.active, after.active));
    }
    if before.writes != after.writes {
        violation(format!("{what}: file writes {:?} -> {:?}", before.writes, after.writes));
    }
    if before.dialogs != after.dialogs {
        violation(format!("{what}: open dialogs {} -> {}", before.dialogs, after.dialogs));
    }
}

fn is_refusal(reply: &Value) -> bool {
    reply["ok"] == false && reply["error"].as_str().is_some_and(|error| error.contains("unaccepted RAW"))
}

fn assert_refused(reply: &Value, what: &str) {
    if !is_refusal(reply) {
        violation(format!("{what} was not refused for the unaccepted RAW: {reply}"));
    }
}

fn assert_not_refused(reply: &Value, what: &str) {
    if is_refusal(reply) {
        violation(format!("{what} must not be refused: {reply}"));
    }
}
/// An accepted document (not a RAW review) with one layer, made through the control channel.
fn add_accepted_document(app: &mut PhotocraftApp, ctx: &egui::Context) -> DocId {
    assert_eq!(engine(app, ctx, "file.new", json!({"width": 16, "height": 16}))["ok"], true);
    assert_eq!(engine(app, ctx, "layer.new.layer", json!({}))["ok"], true);
    app.session.active().unwrap().doc.id
}

/// T04 / T05 (P0-05, menu half): a menu command sent through the control channel meets the same
/// modal gate as the native menu, so it cannot edit a RAW that has not been accepted.
#[test]
fn ui_menu_invoke_cannot_edit_an_unaccepted_raw_while_camera_raw_is_open() {
    let (mut app, ctx) = review_of_three();
    let before = provisional_pixel(&app);
    let reply = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "edit.fill", "params": {"color": "#ff0000"}}));
    assert_eq!(reply["ok"], false, "{reply}");
    assert!(reply["error"].as_str().unwrap_or_default().contains("modal"), "{reply}");
    assert_eq!(provisional_pixel(&app), before, "the provisional document is untouched");
    assert_eq!(app.session.documents().len(), 3);
}

/// T05 / P0-05 (resource half), engine.execute on the implicit target: whichever of the three
/// unaccepted photographs is the active tab, nothing reaches it.
#[test]
fn engine_execute_cannot_edit_any_unaccepted_raw_whichever_tab_is_active() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    for index in 0..3 {
        assert!(app.session.set_active(index));
        let before = print(&app, &ctx, &writes);
        for (command, params) in [("edit.fill", json!({"color": "#ff0000"})), ("layer.new.layer", json!({})), ("edit.undo", json!({}))] {
            let reply = engine(&mut app, &ctx, command, params);
            assert_refused(&reply, &format!("engine.execute {command} on photograph {index}"));
            assert_same(&before, &print(&app, &ctx, &writes), &format!("{command} on photograph {index}"));
        }
    }
    finish();
}

/// Photographs from different folders often share a file name. Protection is by `DocId`, so each of
/// them is held independently, and a name selector that matches any of them is refused.
#[test]
fn photographs_with_the_same_file_name_are_each_protected() {
    let writes = Writes::default();
    let (mut app, ctx) = review_named(&writes, ["IMG_0001.dng", "IMG_0001.dng", "IMG_0001.dng"], true);
    assert_eq!(app.session.documents().len(), 3);
    for index in 0..3 {
        assert!(app.session.set_active(index));
        let before = print(&app, &ctx, &writes);
        assert_refused(&engine(&mut app, &ctx, "edit.fill", json!({"color": "#ff0000"})), &format!("edit.fill on duplicate-named photograph {index}"));
        assert_refused(&call(&mut app, &ctx, "app.save", json!({"path": "IMG_0001.psd"})), &format!("app.save on duplicate-named photograph {index}"));
        assert_same(&before, &print(&app, &ctx, &writes), &format!("duplicate-named photograph {index}"));
    }
    let accepted = add_accepted_document(&mut app, &ctx);
    let by_name = engine(&mut app, &ctx, "image.adjustments.matchColor", json!({"source": "IMG_0001.dng"}));
    assert_refused(&by_name, "a name selector that matches an unaccepted photograph");
    assert_eq!(app.session.active().unwrap().doc.id, accepted);
    finish();
}

/// A canvas context menu opened while an accepted document was active survives the opening of RAWs;
/// choosing a row afterwards would run it on the unaccepted RAW that is now active.
#[test]
fn a_canvas_menu_opened_earlier_cannot_be_chosen_on_an_unaccepted_raw() {
    use photocraft_ui_egui::{Tool, canvas_tool_menu};
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    assert!(canvas_tool_menu::open(&mut app, &ctx, Tool::RectMarquee, [5.0, 5.0]), "the Marquee tool offers a context menu");
    let before = print(&app, &ctx, &writes);
    let reply = call(&mut app, &ctx, "ui.context.choose", json!({"id": "select.all"}));
    assert_refused(&reply, "ui.context.choose select.all on an unaccepted RAW");
    assert_same(&before, &print(&app, &ctx, &writes), "ui.context.choose on an unaccepted RAW");
    // The same menu row on an accepted active document is an ordinary action.
    add_accepted_document(&mut app, &ctx);
    assert!(canvas_tool_menu::open(&mut app, &ctx, Tool::RectMarquee, [5.0, 5.0]));
    let chosen = call(&mut app, &ctx, "ui.context.choose", json!({"id": "select.all"}));
    assert_eq!(chosen["ok"], true, "{chosen}");
    assert!(app.session.active().unwrap().doc.selection.is_some(), "Select All reached the accepted document");
    finish();
}

/// `app.save` is a second route to the same document: it wrote the unaccepted RAW and, for a layered
/// extension, rebound the document to the new file.
#[test]
fn app_save_cannot_write_or_rebind_an_unaccepted_raw() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    for index in 0..3 {
        assert!(app.session.set_active(index));
        for path in ["unaccepted_copy.png", "unaccepted_layered.psd", "unaccepted.pcraft"] {
            let before = print(&app, &ctx, &writes);
            let reply = call(&mut app, &ctx, "app.save", json!({"path": path}));
            assert_refused(&reply, &format!("app.save {path} on photograph {index}"));
            let after = print(&app, &ctx, &writes);
            assert_same(&before, &after, &format!("app.save {path} on photograph {index}"));
            assert!(after.writes.is_empty(), "nothing was written: {:?}", after.writes);
            assert!(after.docs.iter().all(|d| d.path.is_none()), "no document was bound to a file");
        }
    }
    finish();
}

/// A UI-level command id is not an engine command, so `engine.execute` used to hand it to the menu
/// code without any modal gate: Save, Save As and Free Transform all reached the unaccepted RAW.
#[test]
fn the_ui_command_fallback_of_engine_execute_cannot_save_or_transform_an_unaccepted_raw() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    for (command, params) in [
        ("file.save", json!({"path": "fallback_save.psd"})),
        ("file.saveAs", json!({"path": "fallback_save_as.psd"})),
        ("file.export.quickExportAsPng", json!({})),
        ("edit.freeTransform", json!({})),
    ] {
        let before = print(&app, &ctx, &writes);
        let reply = engine(&mut app, &ctx, command, params);
        assert_refused(&reply, command);
        let after = print(&app, &ctx, &writes);
        assert_same(&before, &after, command);
        assert!(after.writes.is_empty() && after.docs.iter().all(|d| d.path.is_none()), "{command} wrote or bound a file: {:?}", after.writes);
    }
    finish();
}

/// A selector can point at an unaccepted document while an accepted one is active. Selectors are
/// tab indexes, `DocId`s or names; each is resolved to the stable identity before anything runs.
#[test]
fn explicit_selectors_cannot_reach_an_unaccepted_raw_while_an_accepted_document_is_active() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let raw_id = app.session.documents().first().map(|st| st.doc.id).unwrap();
    let accepted = add_accepted_document(&mut app, &ctx);
    assert_eq!(app.session.active().unwrap().doc.id, accepted);
    for (command, params) in [
        ("file.close", json!({"document": 0})),
        ("file.revealInFinder", json!({"document": 1, "dryRun": true})),
        ("document.activate", json!({"document": 1})),
        ("document.move", json!({"document": 0, "to": 3})),
        ("layer.copyToDocument", json!({"document": 2})),
        ("image.adjustments.matchColor", json!({"source": "one.dng"})),
        ("image.applyImage", json!({"source": {"document": 1}})),
        ("channel.duplicate", json!({"document": 2})),
        ("select.saveSelection", json!({"document": 0, "name": "x"})),
    ] {
        let before = print(&app, &ctx, &writes);
        let reply = engine(&mut app, &ctx, command, params.clone());
        assert_refused(&reply, &format!("{command} {params}"));
        assert_same(&before, &print(&app, &ctx, &writes), &format!("{command} {params}"));
    }
    // A DocId parameter (the stale-dialog check of a few commands) is compared with the active document and never acted on.
    let before = print(&app, &ctx, &writes);
    let stale = engine(&mut app, &ctx, "layer.fill.solidColor.set", json!({"document": raw_id.0}));
    assert_eq!(stale["ok"], false, "{stale}");
    assert_same(&before, &print(&app, &ctx, &writes), "a stale DocId parameter");
    assert_eq!(app.session.active().unwrap().doc.id, accepted, "the accepted document is still the active tab");
    finish();
}

/// Tab indexes and `DocId`s are both small integers. A number in a request is only ever a tab index, so
/// an accepted document sitting at the tab whose number equals an unaccepted photograph's `DocId` can
/// still be closed by index (an earlier version read the number both ways and refused it).
#[test]
fn a_tab_index_that_equals_an_unaccepted_documents_id_is_still_only_a_tab_index() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let held_id = app.session.documents().get(2).map(|st| st.doc.id.0).unwrap();
    let index = usize::try_from(held_id).unwrap();
    assert!((3..3000).contains(&index), "the third photograph's id {held_id} is a plausible tab index");
    while app.session.documents().len() <= index {
        assert_eq!(engine(&mut app, &ctx, "file.new", json!({"width": 8, "height": 8}))["ok"], true);
    }
    let victim = app.session.documents().get(index).map(|st| st.doc.id).unwrap();
    assert!(app.session.documents().iter().take(3).all(|st| st.doc.id.0 != victim.0), "the document at that tab is an accepted one");
    assert!(app.session.set_active(0));
    let closed = engine(&mut app, &ctx, "file.close", json!({"document": index}));
    assert_eq!(closed["ok"], true, "{closed}");
    assert!(app.session.documents().iter().all(|st| st.doc.id != victim), "the accepted document at tab {index} was closed");
    assert_eq!(app.session.documents().iter().take(3).count(), 3, "the three photographs are still there");
    finish();
}
/// Commands whose target is not one `DocId` (every open document, the next tab, a recorded step, a
/// plug-in) are refused while a review is open, even when the active tab is an accepted document.
#[test]
fn commands_that_choose_their_own_targets_or_affect_every_document_are_refused() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    add_accepted_document(&mut app, &ctx);
    // A recorded action that fills whatever is active.
    app.run("actions.record", json!({"name": "Red"})).unwrap();
    app.run("edit.fill", json!({"color": "#ff0000"})).unwrap();
    app.run("actions.stop", json!({})).unwrap();
    for active in [0, 3] {
        assert!(app.session.set_active(active));
        let before = print(&app, &ctx, &writes);
        for (command, params) in [
            ("actions.play", json!({"action": "Red"})),
            ("file.closeAll", json!({})),
            ("file.closeOthers", json!({"document": 3})),
            ("file.automate.batch", json!({"steps": [["edit.fill", {"color": "#00ff00"}]], "input": []})),
            ("file.scripts.deleteAllEmptyLayers", json!({})),
            ("plugin.run", json!({"id": "any"})),
            ("channel.merge", json!({})),
            ("window.nextDocument", json!({})),
            ("window.previousDocument", json!({})),
        ] {
            let reply = engine(&mut app, &ctx, command, params);
            assert_refused(&reply, &format!("{command} with tab {active} active"));
            assert_same(&before, &print(&app, &ctx, &writes), &format!("{command} with tab {active} active"));
        }
    }
    assert_eq!(app.session.documents().len(), 4);
    finish();
}

/// The other control routes that drive the active document: the pointer tool channel, extra windows,
/// and dialogs that bind to the active layer.
#[test]
fn pointer_window_and_dialog_routes_cannot_reach_an_unaccepted_raw() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let stroke = json!({"tool": "brush", "events": [{"kind": "down", "x": 8, "y": 8}, {"kind": "move", "x": 30, "y": 20}, {"kind": "up", "x": 30, "y": 20}]});
    for (method, params) in [
        ("ui.pointer", stroke),
        ("ui.window.open", json!({"document": 1})),
        ("ui.window.open", json!({})),
        ("ui.dialog.open", json!({"kind": "layerStyle"})),
        ("ui.dialog.open", json!({"kind": "command", "command": "edit.fill"})),
    ] {
        let before = print(&app, &ctx, &writes);
        let reply = call(&mut app, &ctx, method, params.clone());
        assert_refused(&reply, &format!("{method} {params}"));
        assert_same(&before, &print(&app, &ctx, &writes), &format!("{method} {params}"));
    }
    // A New Document dialog touches no existing document: it opens, and cancelling closes it.
    let opened = call(&mut app, &ctx, "ui.dialog.open", json!({"kind": "newDocument"}));
    assert_eq!(opened["ok"], true, "{opened}");
    let cancelled = call(&mut app, &ctx, "ui.dialog.cancel", json!({"dialog": opened["result"]["dialog"]}));
    assert_eq!(cancelled["ok"], true, "{cancelled}");
    finish();
}

/// Lab port (main 0.9.0, D-16): Ask When Opening asks before a file opens and never asks for a developed RAW, so a RAW
/// waiting in Camera Raw has no Embedded Profile Mismatch prompt through which `ui.dialog.*` could convert it. (R2b's
/// version answered such a prompt; on main the prompt does not exist.)
#[test]
fn a_waiting_raw_raises_no_profile_prompt_that_could_convert_it() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, false);
    let before = print(&app, &ctx, &writes);
    let prompts: Vec<Value> = control::inspect(&app, &ctx)["dialogs"].as_array().cloned().unwrap_or_default();
    assert!(prompts.iter().all(|d| d["fields"]["__prefsui"] != "mismatch"), "a waiting RAW raises no profile prompt: {prompts:?}");
    frames(&mut app, &ctx, 2);
    assert_same(&before, &print(&app, &ctx, &writes), "a waiting review with no prompt");
    finish();
}

/// Protection follows the document, not what is on screen: the same refusals hold with the
/// Filmstrip hidden and turned sideways and with profile prompts covering the dialog.
#[test]
fn protection_holds_with_the_filmstrip_hidden_and_a_prompt_in_front() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, false);
    cancel_one_prompt_only(&mut app, &ctx);
    for view in [json!({"visible": false}), json!({"orientation": "horizontal", "height": 140})] {
        // Prompts are modal, so the Filmstrip is driven once they are answered; ask for it, and
        // accept a refusal of the continuation itself: the refusals below are what matters.
        let _ = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"filmstrip": view}}}));
        frames(&mut app, &ctx, 2);
        for index in 0..3 {
            assert!(app.session.set_active(index));
            let before = print(&app, &ctx, &writes);
            assert_refused(&engine(&mut app, &ctx, "edit.fill", json!({"color": "#ff0000"})), "edit.fill");
            assert_refused(&call(&mut app, &ctx, "app.save", json!({"path": "hidden.psd"})), "app.save");
            assert_same(&before, &print(&app, &ctx, &writes), "with the Filmstrip changed and a prompt open");
        }
    }
    finish();
}

fn cancel_one_prompt_only(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let open = control::inspect(app, ctx)["dialogs"].as_array().cloned().unwrap_or_default();
    if let Some(first) = open.first() {
        call(app, ctx, "ui.dialog.cancel", json!({"dialog": first["id"]}));
    }
}

/// What must keep working while Camera Raw stays open: editing, saving and closing an accepted
/// document, with the unaccepted photographs untouched throughout.
#[test]
fn accepted_documents_stay_editable_while_camera_raw_stays_open() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let before = print(&app, &ctx, &writes);
    let accepted = add_accepted_document(&mut app, &ctx);
    let fill = engine(&mut app, &ctx, "edit.fill", json!({"color": "#ff0000"}));
    assert_eq!(fill["ok"], true, "{fill}");
    let red = provisional_pixel(&app);
    assert!(red[0] > 0.9 && red[1] < 0.1, "the accepted document took the fill: {red:?}");
    // The pointer channel, a command dialog and an extra window all act on the accepted document too.
    let drawn_on = app.session.active().unwrap().doc.clone();
    let stroke = call(
        &mut app,
        &ctx,
        "ui.pointer",
        json!({"tool": "brush", "events": [{"kind": "down", "x": 3, "y": 3}, {"kind": "move", "x": 10, "y": 10}, {"kind": "up", "x": 10, "y": 10}]}),
    );
    assert_eq!(stroke["ok"], true, "{stroke}");
    assert!(!Arc::ptr_eq(&drawn_on, &app.session.active().unwrap().doc), "the stroke landed on the accepted document");
    let dialog = call(&mut app, &ctx, "ui.dialog.open", json!({"kind": "command", "command": "edit.fill"}));
    assert_eq!(dialog["ok"], true, "{dialog}");
    assert_eq!(call(&mut app, &ctx, "ui.dialog.cancel", json!({"dialog": dialog["result"]["dialog"]}))["ok"], true);
    let window = call(&mut app, &ctx, "ui.window.open", json!({}));
    assert_eq!(window["ok"], true, "{window}");
    assert_eq!(call(&mut app, &ctx, "ui.window.close", json!({"window": window["result"]["window"]}))["ok"], true);
    for path in ["accepted.png", "accepted.psd"] {
        let saved = call(&mut app, &ctx, "app.save", json!({"path": path}));
        assert_eq!(saved["ok"], true, "{saved}");
    }
    assert_eq!(writes.borrow().len(), 2, "both saves of the accepted document were written");
    let st = app.session.active().unwrap();
    assert_eq!((st.doc.id, st.path.as_deref()), (accepted, Some("accepted.psd")), "the accepted document is bound to its own file");
    // The unaccepted photographs are exactly as they were.
    let now = print(&app, &ctx, &writes);
    for (b, a) in before.docs.iter().zip(&now.docs) {
        assert_doc_same(b, a, "while an accepted document was edited and saved");
    }
    // Explicit selectors naming only the accepted document work even with a RAW as the active tab,
    // and a UI-level command on the accepted document is not mistaken for the RAW.
    assert!(app.session.set_active(0));
    let closed = engine(&mut app, &ctx, "file.close", json!({"document": 3}));
    assert_eq!(closed["ok"], true, "{closed}");
    assert_eq!(app.session.documents().len(), 3);
    assert_eq!(app.session.active_index(), Some(0), "closing the accepted tab leaves the review where it was");
    for (b, a) in before.docs.iter().zip(&print(&app, &ctx, &writes).docs) {
        assert_doc_same(b, a, "after the accepted document was closed");
    }
    frames(&mut app, &ctx, 2);
    assert!(!control::inspect(&app, &ctx)["cameraRaw"].is_null(), "Camera Raw is still open");
    finish();
}

/// Queries and view state stay available during a review, and a test proves each one changes nothing.
#[test]
fn read_only_requests_and_view_state_stay_available_and_change_nothing() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let before = print(&app, &ctx, &writes);
    for command in control::READ_ONLY_COMMANDS {
        let reply = engine(&mut app, &ctx, command, json!({}));
        assert_not_refused(&reply, command);
        assert_same(&before, &print(&app, &ctx, &writes), command);
    }
    // The same queries with parameters that make them do their work (the loop above sends none).
    for (command, params, answers) in [
        ("document.pixel", json!({"x": 1, "y": 1}), true),
        ("document.sampleColor", json!({"x": 1.0, "y": 1.0, "size": 3}), true),
        ("type.hitTest", json!({"x": 1.0, "y": 1.0}), false),
        ("document.inspect", json!({"document": 0}), true),
        ("document.inspect", json!({"document": 2}), true),
        ("channel.list", json!({}), true),
        ("brush.get", json!({}), false),
    ] {
        let reply = engine(&mut app, &ctx, command, params.clone());
        assert_not_refused(&reply, command);
        if answers && reply["ok"] != true {
            violation(format!("{command} {params} should answer while a review is open: {reply}"));
        }
        assert_same(&before, &print(&app, &ctx, &writes), command);
    }
    for command in [
        "view.zoomIn",
        "view.zoomOut",
        "view.fitOnScreen",
        "view.actualPixels",
        "view.rulers",
        "window.toggle.layers",
        "window.toggle.history",
        "window.theme.pro",
    ] {
        let reply = engine(&mut app, &ctx, command, json!({}));
        assert_not_refused(&reply, command);
        assert_same(&before, &print(&app, &ctx, &writes), command);
    }
    for (method, params) in [
        ("engine.commands", json!({})),
        ("jobs.list", json!({})),
        ("ui.inspect", json!({})),
        ("ui.menu.list", json!({})),
        (
            "ui.set",
            json!({
                "tool": "brush", "zoom": 2.0, "center": [10.0, 10.0], "rotation": 10.0, "fit": false, "brushSize": 12, "selectionMode": 1,
                "maskTarget": false, "vectorMaskTarget": false, "panels": {"layers": true}, "cropOverlayShow": "auto", "eyedropperRing": true
            }),
        ),
    ] {
        let reply = call(&mut app, &ctx, method, params);
        assert_eq!(reply["ok"], true, "{method}: {reply}");
        assert_same(&before, &print(&app, &ctx, &writes), method);
    }
    finish();
}

/// Cancel releases every protection (nothing is left to protect), and the guard follows acceptance:
/// a committed photograph becomes editable while the ones still waiting stay protected.
#[test]
fn the_guard_follows_acceptance_and_is_released_by_cancel() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let state = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {}}}))["result"].clone();
    let waiting: Vec<u64> = state["filmstrip"].as_array().unwrap().iter().filter_map(|p| p["document"].as_u64()).collect();
    assert_eq!(waiting.len(), 3);
    let committed = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"commit": true}}}));
    assert_eq!(committed["ok"], true, "{committed}");
    frames(&mut app, &ctx, 3);
    cancel_prompts(&mut app, &ctx);
    let state = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {}}}));
    let still_waiting: Vec<u64> =
        state["result"]["filmstrip"].as_array().map(|f| f.iter().filter_map(|p| p["document"].as_u64()).collect()).unwrap_or_default();
    let accepted: Vec<usize> =
        app.session.documents().iter().enumerate().filter(|(_, st)| !still_waiting.contains(&st.doc.id.0)).map(|(index, _)| index).collect();
    assert!(!accepted.is_empty() && still_waiting.len() < 3, "Open accepted a photograph: waiting {still_waiting:?}, accepted tabs {accepted:?}");
    // The accepted photograph can be activated and edited; the ones still waiting cannot.
    let activated = engine(&mut app, &ctx, "document.activate", json!({"document": accepted[0]}));
    assert_eq!(activated["ok"], true, "{activated}");
    let fill = engine(&mut app, &ctx, "edit.fill", json!({"color": "#ff0000"}));
    assert_eq!(fill["ok"], true, "an accepted RAW is an ordinary document: {fill}");
    if let Some(waiting_index) = app.session.documents().iter().position(|st| still_waiting.contains(&st.doc.id.0)) {
        assert!(app.session.set_active(waiting_index));
        assert_refused(&engine(&mut app, &ctx, "edit.fill", json!({"color": "#00ff00"})), "a photograph still waiting");
    }
    // Cancel discards what is waiting and releases the guard.
    let cancelled = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"cancel": true}}}));
    assert_eq!(cancelled["ok"], true, "{cancelled}");
    frames(&mut app, &ctx, 3);
    assert!(app.session.documents().iter().all(|st| !still_waiting.contains(&st.doc.id.0)), "Cancel discarded every waiting photograph");
    // Nothing is left to protect: a new document is edited and saved like any other, and a stale
    // identity of a cancelled photograph does not strand the guard.
    let writes_before = writes.borrow().len();
    add_accepted_document(&mut app, &ctx);
    assert_eq!(engine(&mut app, &ctx, "edit.fill", json!({"color": "#00ff00"}))["ok"], true);
    assert_eq!(call(&mut app, &ctx, "app.save", json!({"path": "after_cancel.psd"}))["ok"], true);
    assert_eq!(writes.borrow().len(), writes_before + 1);
    assert_not_refused(&engine(&mut app, &ctx, "layer.fill.solidColor.set", json!({"document": waiting[0]})), "a stale identity");
    finish();
}

/// MCP-24-ish: the Filmstrip state an agent needs is observable and drivable through the existing
/// `filter.cameraRaw` `ui` continuation, with no new command. **KNOWN GAP:** `ui.inspect` itself
/// carries no Filmstrip or selection block (adding one needs `control.rs`); recorded here so a fix
/// must update this test.
#[test]
fn the_existing_continuation_reports_and_drives_selection_and_layout_but_ui_inspect_does_not() {
    let (mut app, ctx) = review_of_three();
    assert!(control::inspect(&app, &ctx)["cameraRaw"].get("filmstrip").is_none(), "known gap: ui.inspect has no Filmstrip block");
    let reply = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {}}}));
    assert_eq!(reply["ok"], true, "{reply}");
    let state = reply["result"].clone();
    assert_eq!(state["filmstrip"].as_array().map(Vec::len), Some(3), "{state}");
    assert_eq!((state["selection"]["count"].clone(), state["filmstripView"]["orientation"].clone()), (json!(1), json!("vertical")));
    let active = state["selection"]["active"].clone();
    assert_eq!(state["selection"]["selected"], json!([active]), "the first selection is only the active photo");
    let reply = call(
        &mut app,
        &ctx,
        "ui.menu.invoke",
        json!({"id": "filter.cameraRaw", "params": {"ui": {"filmstrip": {"orientation": "horizontal", "height": 180}}}}),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    frames(&mut app, &ctx, 2);
    let state = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {}}}))["result"].clone();
    assert_eq!((state["filmstripView"]["orientation"].clone(), state["filmstripView"]["effectiveHeight"].clone()), (json!("horizontal"), json!(180.0)));
    assert_eq!(state["selection"]["active"], active, "changing the layout never changes the photo");
    let bad = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"filmstrip": {"orientation": "sideways"}}}}));
    assert_eq!(bad["ok"], false, "an invalid layout is rejected, not half applied: {bad}");
    let after = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {}}}))["result"].clone();
    assert_eq!(after["filmstripView"]["orientation"], "horizontal");
}

/// T22: the `filter.cameraRaw` continuation drives an open dialog; it is not a way to start a RAW
/// session, and after Cancel there is nothing left to drive.
#[test]
fn the_camera_raw_continuation_does_not_start_or_revive_a_raw_session() {
    let (mut app, ctx) = review_of_three();
    let cancelled = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"cancel": true}}}));
    assert_eq!(cancelled["ok"], true, "{cancelled}");
    frames(&mut app, &ctx, 2);
    assert_eq!(app.session.documents().len(), 0, "Cancel discards every unaccepted photo");
    let again = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"set": {"exposure": 1.0}}}}));
    assert_ne!(again["ok"], json!(true), "nothing is open to continue: {again}");
    frames(&mut app, &ctx, 2);
    assert_eq!(app.session.documents().len(), 0, "and no session or document appears");
    assert!(control::inspect(&app, &ctx)["cameraRaw"].is_null() || control::inspect(&app, &ctx)["cameraRaw"].get("filmstrip").is_none());
}

/// The continuation is the one control request that may pass Camera Raw's own gate, and it must not
/// be a way to edit the document: engine.execute of the same id is an ordinary engine command and is
/// refused like any other edit.
#[test]
fn the_camera_raw_filter_itself_cannot_be_applied_to_an_unaccepted_raw_through_engine_execute() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let before = print(&app, &ctx, &writes);
    for params in [json!({"exposure": 1.0}), json!({"ui": {"set": {"exposure": 1.0}}}), json!({"ui": {"commit": true}})] {
        let reply = engine(&mut app, &ctx, "filter.cameraRaw", params.clone());
        assert_refused(&reply, &format!("engine.execute filter.cameraRaw {params}"));
    }
    assert_same(&before, &print(&app, &ctx, &writes), "filter.cameraRaw through engine.execute");
    // The documented continuation still works.
    let reply = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"set": {"exposure": 0.5}}}}));
    assert_eq!(reply["ok"], true, "{reply}");
    finish();
}

fn call_h(h: &mut Harness<'_, PhotocraftApp>, method: &str, params: Value) -> Value {
    let ctx = h.ctx.clone();
    let (request, _reply) = control::ControlRequest::new(method, params);
    match control::handle(h.state_mut(), &ctx, &request) {
        control::Outcome::Done(value) => value,
        control::Outcome::AfterInput => {
            // The harness doesn't call `raw_input_hook`: feed the queued input one press/release step per frame.
            loop {
                let step = h.state_mut().take_synthetic_step();
                if step.is_empty() {
                    break;
                }
                for event in step {
                    h.event(event);
                }
                h.step();
            }
            h.run_steps(2);
            json!({"ok": true})
        }
        _ => json!({"ok": false, "error": "the request did not complete synchronously"}),
    }
}

/// A request that fills, writes or otherwise acts on whatever is active must not reach an unaccepted
/// RAW even when it arrives dressed as something harmless: a dialog opened for a read-only command and
/// then re-marked as a Fill dialog with `ui.dialog.set` runs the Fill on confirm.
#[test]
fn a_dialog_cannot_be_retargeted_to_edit_an_unaccepted_raw() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let opened = call(&mut app, &ctx, "ui.dialog.open", json!({"kind": "command", "command": "prefs.get"}));
    assert_eq!(opened["ok"], true, "{opened}");
    let dialog = opened["result"]["dialog"].clone();
    let before = print(&app, &ctx, &writes);
    let retargeted = call(&mut app, &ctx, "ui.dialog.set", json!({"dialog": dialog, "field": "__fill", "value": true}));
    assert_refused(&retargeted, "re-marking a dialog with an internal field");
    let confirmed = call(&mut app, &ctx, "ui.dialog.confirm", json!({"dialog": dialog}));
    assert_refused(&confirmed, "confirming a dialog that is not about preferences");
    assert_same(&before, &print(&app, &ctx, &writes), "retargeting a dialog");
    finish();
}

/// The same with the disguise applied before the review began, when nothing was held yet: a Fill
/// dialog marked as a Preferences dialog is still judged by what its confirm would run.
#[test]
fn a_dialog_disguised_as_preferences_before_the_review_is_still_judged_by_what_it_would_run() {
    let writes = Writes::default();
    let (mut app, ctx) = new_app(&writes);
    add_accepted_document(&mut app, &ctx);
    let fill = call(&mut app, &ctx, "ui.dialog.open", json!({"kind": "command", "command": "edit.fill"}));
    assert_eq!(fill["ok"], true, "{fill}");
    let dialog = fill["result"]["dialog"].clone();
    let disguised = call(&mut app, &ctx, "ui.dialog.set", json!({"dialog": dialog, "field": "__prefsui", "value": "prefs"}));
    assert_eq!(disguised["ok"], true, "nothing is held yet, so the dialog can still be edited: {disguised}");
    let dng = dng();
    for name in ["one.dng", "two.dng", "three.dng"] {
        app.open_bytes(name, &dng).unwrap();
    }
    frames(&mut app, &ctx, 3);
    // Answer the profile prompts only; the Fill dialog stays open.
    for _ in 0..6 {
        let prompts: Vec<Value> = control::inspect(&app, &ctx)["dialogs"].as_array().cloned().unwrap_or_default();
        for prompt in prompts.iter().filter(|p| p["fields"]["__prefsui"] == "mismatch") {
            call(&mut app, &ctx, "ui.dialog.cancel", json!({"dialog": prompt["id"]}));
        }
        frames(&mut app, &ctx, 1);
    }
    let held_active = app.session.active().unwrap().doc.id;
    assert!(app.session.documents().iter().skip(1).any(|st| st.doc.id == held_active), "a RAW photograph is the active tab");
    let before = print(&app, &ctx, &writes);
    let confirmed = call(&mut app, &ctx, "ui.dialog.confirm", json!({"dialog": dialog}));
    assert_refused(&confirmed, "confirming a Fill dialog marked as preferences");
    assert_same(&before, &print(&app, &ctx, &writes), "confirming a disguised dialog");
    finish();
}

/// `document` is read the way the engine reads it: anything that is not a tab index makes `file.close`
/// act on the active tab, which during review is a photograph waiting for Open or Cancel.
#[test]
fn a_document_selector_that_is_not_a_tab_index_cannot_close_the_active_unaccepted_raw() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    add_accepted_document(&mut app, &ctx);
    for selector in [json!("other.psd"), json!([3]), json!(null), json!(1.5), json!(-1), json!(99), json!({"index": 3})] {
        assert!(app.session.set_active(0));
        let before = print(&app, &ctx, &writes);
        let reply = engine(&mut app, &ctx, "file.close", json!({"document": selector}));
        assert_refused(&reply, &format!("file.close with document {selector}"));
        assert_same(&before, &print(&app, &ctx, &writes), &format!("file.close with document {selector}"));
    }
    finish();
}

/// Session-level commands and copies between accepted documents touch no photograph: they are allowed
/// even while a photograph is the active tab. Purging every document's history is not.
#[test]
fn commands_that_touch_no_photograph_are_allowed_and_purging_every_history_is_not() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let first = add_accepted_document(&mut app, &ctx);
    let second = add_accepted_document(&mut app, &ctx);
    assert_ne!(first, second);
    let before = print(&app, &ctx, &writes);
    assert!(app.session.set_active(0));
    for (command, params) in [
        ("tools.defaultColors", json!({})),
        ("tools.swapColors", json!({})),
        ("tools.setBrush", json!({"brush": {"size": 9}})),
        ("tool.presets.reset", json!({})),
    ] {
        let reply = engine(&mut app, &ctx, command, params);
        assert_not_refused(&reply, command);
    }
    let mid = print(&app, &ctx, &writes);
    for (b, a) in before.docs.iter().zip(&mid.docs) {
        assert_doc_same(b, a, "session-level commands");
    }
    // A copy from one accepted document to another does not involve the active photograph.
    let copied = engine(&mut app, &ctx, "layer.copyToDocument", json!({"source": 3, "document": 4}));
    assert_eq!(copied["ok"], true, "{copied}");
    let now = print(&app, &ctx, &writes);
    for (b, a) in before.docs.iter().zip(&now.docs).take(4) {
        assert_doc_same(b, a, "a copy between accepted documents (photographs and the source)");
    }
    assert!(!Arc::ptr_eq(&before.docs[4].doc, &now.docs[4].doc), "the destination accepted document received the layer");
    // Purges clear every document's history, the waiting photographs included.
    assert!(app.session.set_active(4));
    let before = print(&app, &ctx, &writes);
    for command in ["edit.purge.histories", "edit.purge.undo", "edit.purge.all"] {
        assert_refused(&engine(&mut app, &ctx, command, json!({})), command);
    }
    assert_same(&before, &print(&app, &ctx, &writes), "purging histories");
    finish();
}

/// The real app (frame loop, dialogs, palette, shortcuts, canvas), with the profile prompts of three
/// freshly opened RAWs answered and every other dialog left open. `before_raws` runs first.
fn app_with_review(writes: &Writes, before_raws: impl FnOnce(&mut Harness<'static, PhotocraftApp>)) -> Harness<'static, PhotocraftApp> {
    app_with_review_using(writes, |_| {}, before_raws)
}

fn app_with_review_using(
    writes: &Writes,
    tweak: impl FnOnce(&mut Services) + 'static,
    before_raws: impl FnOnce(&mut Harness<'static, PhotocraftApp>),
) -> Harness<'static, PhotocraftApp> {
    let for_app = writes.clone();
    let mut h = Harness::builder().with_step_dt(1.0 / 60.0).with_size(vec2(1360.0, 900.0)).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
        let mut services = services(&for_app);
        tweak(&mut services);
        PhotocraftApp::new(photocraft_engine::Session::new(), services)
    });
    h.run_steps(3);
    before_raws(&mut h);
    let dng = dng();
    for name in ["one.dng", "two.dng", "three.dng"] {
        h.state_mut().open_bytes(name, &dng).unwrap();
    }
    h.run_steps(6);
    for _ in 0..6 {
        let prompts: Vec<Value> = control::inspect(h.state(), &h.ctx)["dialogs"].as_array().cloned().unwrap_or_default();
        for prompt in prompts.iter().filter(|p| p["fields"]["__prefsui"] == "mismatch") {
            call_h(&mut h, "ui.dialog.cancel", json!({"dialog": prompt["id"]}));
        }
        h.run_steps(1);
    }
    assert!(!control::inspect(h.state(), &h.ctx)["cameraRaw"].is_null(), "Camera Raw is open");
    h
}

/// Synthetic pointer, key and text input is delivered to whatever UI surface is under it, and several
/// surfaces (dialogs, the command palette, floating panels) are not disabled while Camera Raw is open
/// and act on the active document. While a photograph waits for Open or Cancel the control channel
/// refuses that input; the `filter.cameraRaw` `ui` requests and `ui.dialog.*` are the way to drive
/// Camera Raw and its prompts.
#[test]
fn synthetic_keys_text_and_clicks_are_refused_while_a_photograph_waits_and_change_nothing() {
    let writes = Writes::default();
    let mut h = app_with_review(&writes, |_| {});
    assert_eq!(h.state().session.documents().len(), 3);
    let before = print(h.state(), &h.ctx, &writes);
    for (key, command) in
        [("Delete", false), ("Backspace", false), ("Z", true), ("A", true), ("D", true), ("I", true), ("S", true), ("N", true), ("J", true), ("T", true)]
    {
        assert_refused(&call_h(&mut h, "ui.key", json!({"key": key, "command": command})), &format!("ui.key {key}"));
    }
    for text in ["b", "e", "g", "x", "[", "]", "d", "/"] {
        assert_refused(&call_h(&mut h, "ui.type", json!({"text": text})), &format!("ui.type {text}"));
    }
    for (x, y) in [(40.0, 300.0), (400.0, 300.0), (680.0, 450.0), (1000.0, 600.0), (1300.0, 300.0), (680.0, 880.0)] {
        assert_refused(&call_h(&mut h, "ui.click", json!({"x": x, "y": y})), &format!("ui.click {x},{y}"));
        assert_refused(&call_h(&mut h, "ui.click", json!({"x": x, "y": y, "button": "right"})), &format!("ui.click right {x},{y}"));
        assert_refused(&call_h(&mut h, "ui.move", json!({"x": x, "y": y})), &format!("ui.move {x},{y}"));
    }
    h.run_steps(4);
    assert_same(&before, &print(h.state(), &h.ctx, &writes), "synthetic input while a photograph waits");
    finish();
}

/// A dialog that was already open when the RAWs arrived stays open; confirming it with Enter used to
/// run its command (here a Fill) on the photograph that is now the active tab.
#[test]
fn a_dialog_opened_before_the_review_cannot_be_confirmed_with_a_synthetic_key() {
    let writes = Writes::default();
    let mut h = app_with_review(&writes, |h| {
        assert_eq!(call_h(h, "engine.execute", json!({"command": "file.new", "params": {"width": 16, "height": 16}}))["ok"], true);
        assert_eq!(call_h(h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
        let fill = call_h(h, "ui.dialog.open", json!({"kind": "command", "command": "edit.fill"}));
        assert_eq!(fill["ok"], true, "{fill}");
        h.run_steps(3);
    });
    let accepted = h.state().session.documents().first().map(|st| st.doc.id).unwrap();
    assert_ne!(h.state().session.active().unwrap().doc.id, accepted, "a photograph is the active tab");
    assert!(control::inspect(h.state(), &h.ctx)["dialogs"].as_array().is_some_and(|d| !d.is_empty()), "the Fill dialog is still open");
    let before = print(h.state(), &h.ctx, &writes);
    assert_refused(&call_h(&mut h, "ui.key", json!({"key": "Enter"})), "ui.key Enter with a Fill dialog open");
    h.run_steps(3);
    assert_same(&before, &print(h.state(), &h.ctx, &writes), "Enter with a Fill dialog open over a photograph");
    finish();
}

/// `edit.search` is a shell command that opens the command palette, which runs the chosen command on
/// whatever is active without passing the menu gate.
#[test]
fn the_command_palette_cannot_be_opened_and_driven_to_edit_an_unaccepted_raw() {
    let writes = Writes::default();
    let mut h = app_with_review(&writes, |_| {});
    let before = print(h.state(), &h.ctx, &writes);
    assert_refused(&call_h(&mut h, "engine.execute", json!({"command": "edit.search", "params": {}})), "engine.execute edit.search");
    h.run_steps(3);
    assert_refused(&call_h(&mut h, "ui.type", json!({"text": "invert"})), "ui.type into the palette");
    assert_refused(&call_h(&mut h, "ui.key", json!({"key": "Enter"})), "ui.key Enter in the palette");
    h.run_steps(3);
    assert_same(&before, &print(h.state(), &h.ctx, &writes), "the command palette");
    finish();
}

/// `file.exit` is a UI-level command: with unsaved changes it raises a prompt that only the screen
/// can answer, switches to the dirty tab, and while it is up the Camera Raw `ui` requests are refused,
/// so the review could no longer be driven or cancelled by the control channel.
#[test]
fn file_exit_cannot_park_an_unsaved_changes_prompt_over_the_review() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    add_accepted_document(&mut app, &ctx);
    assert_eq!(engine(&mut app, &ctx, "edit.fill", json!({"color": "#ff0000"}))["ok"], true, "an accepted document with unsaved changes");
    assert!(app.session.set_active(0));
    let before = print(&app, &ctx, &writes);
    assert_refused(&engine(&mut app, &ctx, "file.exit", json!({})), "engine.execute file.exit");
    assert_same(&before, &print(&app, &ctx, &writes), "file.exit with unsaved changes");
    let state = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {}}}));
    if state["ok"] != true {
        violation(format!("the review can no longer be driven after file.exit: {state}"));
    }
    finish();
}

/// A control method added later without a decision about it is refused while a photograph waits.
#[test]
fn a_method_nobody_classified_is_refused_while_a_photograph_waits() {
    let (mut app, ctx) = review_of_three();
    assert_refused(&call(&mut app, &ctx, "ui.somethingNew", json!({})), "an unclassified method");
    let (mut idle, idle_ctx) = new_app(&Writes::default());
    let reply = call(&mut idle, &idle_ctx, "ui.somethingNew", json!({}));
    assert!(reply["error"].as_str().unwrap_or_default().contains("unknown method"), "with nothing waiting the dispatcher answers as before: {reply}");
    finish();
}

/// The session-level allow-list is by id. A command added under one of its prefixes fails this test until
/// someone decides whether it may run while a photograph waits.
#[test]
fn every_command_under_the_session_level_prefixes_is_classified_on_purpose() {
    // Left out on purpose: they take file paths.
    let held_back = ["swatches.import", "swatches.export"];
    for spec in photocraft_engine::commands::command_specs() {
        if ["tools.", "swatches.", "tool.presets."].iter().any(|prefix| spec.id.starts_with(prefix)) {
            let known = control::SESSION_LEVEL_COMMANDS.contains(&spec.id) || control::READ_ONLY_COMMANDS.contains(&spec.id) || held_back.contains(&spec.id);
            assert!(known, "`{}` is new: decide whether it may run while a photograph waits and list it in control.rs", spec.id);
        }
    }
    for id in control::SESSION_LEVEL_COMMANDS.iter().chain(control::READ_ONLY_COMMANDS) {
        assert!(photocraft_engine::commands::find(id).is_some(), "`{id}` is listed in control.rs but is not a registered command");
    }
}

/// The waiting photographs, by identity, as they were: none may be closed, replaced, re-bound or edited.
fn assert_waiting_unchanged(app: &PhotocraftApp, before: &[DocPrint], what: &str) {
    for b in before {
        match app.session.documents().iter().find(|st| st.doc.id == b.id) {
            None => violation(format!("{what}: waiting document {:?} is gone", b.id)),
            Some(st) => {
                let now = DocPrint {
                    id: st.doc.id,
                    doc: st.doc.clone(),
                    path: st.path.clone(),
                    revision: st.revision,
                    saved_revision: st.saved_revision,
                    past: st.history.past_len(),
                };
                assert_doc_same(b, &now, what);
            }
        }
    }
}

/// Every command in the registry, sent with default parameters while three photographs wait, once with a
/// photograph as the active tab and once with an accepted document. The registry's `journal` flag
/// (107 commands are "query-like") is not trusted: each command is either refused or leaves every
/// waiting photograph exactly as it was.
#[test]
fn every_registered_command_with_default_parameters_leaves_waiting_photographs_untouched() {
    // Not run: they open the platform file manager, enumerate scanners or print.
    const NOT_RUN: &[&str] = &["file.revealInFinder", "file.import.wiaSupport", "file.print", "file.printOneCopy"];
    let writes = Writes::default();
    let fresh = || {
        let (app, ctx) = review_app(&writes, true);
        let waiting: Vec<DocPrint> = print(&app, &ctx, &writes).docs;
        let ids: Vec<DocId> = waiting.iter().map(|d| d.id).collect();
        (app, ctx, waiting, ids)
    };
    let mut totals = Vec::new();
    let mut reached = Vec::new();
    for accepted_is_active in [false, true] {
        let (mut app, mut ctx, mut waiting, mut ids) = fresh();
        let (mut refused, mut allowed) = (0, 0);
        for spec in photocraft_engine::commands::command_specs() {
            if NOT_RUN.contains(&spec.id) {
                continue;
            }
            // An accepted document with a layer is always there to be the target.
            if !app.session.documents().iter().any(|st| !ids.contains(&st.doc.id)) {
                app.run("file.new", json!({"width": 8, "height": 8})).unwrap();
                app.run("layer.new.layer", json!({})).unwrap();
            }
            let accepted = app.session.documents().iter().position(|st| !ids.contains(&st.doc.id)).unwrap();
            let waiting_tab = app.session.documents().iter().position(|st| ids.contains(&st.doc.id)).unwrap_or(0);
            assert!(app.session.set_active(if accepted_is_active { accepted } else { waiting_tab }));
            eprintln!("sweep active={} {}", if accepted_is_active { "accepted" } else { "photograph" }, spec.id);
            let reply = engine(&mut app, &ctx, spec.id, json!({}));
            if is_refusal(&reply) {
                refused += 1;
            } else {
                allowed += 1;
            }
            let before = violation_count();
            assert_waiting_unchanged(
                &app,
                &waiting,
                &format!("`{}` with default parameters (active: {})", spec.id, if accepted_is_active { "accepted document" } else { "photograph" }),
            );
            if violation_count() != before {
                // One command reached a photograph: say so once, then start the next one from a fresh review so
                // the damage is not blamed on every command after it.
                reached.push((spec.id, accepted_is_active));
                (app, ctx, waiting, ids) = fresh();
            }
        }
        totals.push((accepted_is_active, refused, allowed));
    }
    eprintln!("sweep totals (accepted active, refused, allowed): {totals:?}");
    if !reached.is_empty() {
        violation(format!("{} command(s) changed a waiting photograph: {reached:?}", reached.len()));
    }
    finish();
}

/// Script Events bind actions to New, Open, Save and Close and run them on whatever is active afterwards.
/// A control request runs with them suspended, so the hook cannot reach a waiting photograph: the case that
/// matters is closing the accepted tab, which leaves a photograph as the active one.
#[test]
fn script_event_hooks_never_run_on_a_waiting_photograph_for_a_control_request() {
    use photocraft_engine::automate_cmds::ScriptBinding;
    let writes = Writes::default();
    let bytes = dng();
    let (mut app, ctx) = new_app_with(&writes, |services| {
        services.automation_read = Some(Box::new(move |path: &str| Ok((path.to_string(), bytes.clone()))));
    });
    let accepted = add_accepted_document(&mut app, &ctx);
    app.session.edit_prefs(|prefs| {
        prefs.script_events.enabled = true;
        prefs.script_events.bindings = vec![ScriptBinding {
            event: "everything".into(),
            script: None,
            steps: Some(json!([["layer.new.layer", {"name": "From the hook"}], ["edit.fill", {"color": "#ff0000"}]])),
            name: "Hook".into(),
        }];
    });
    // The hook is live while nothing waits: fired the way the app shell fires it, it changes the active (accepted) document.
    let layers = app.session.active().unwrap().doc.layer_count();
    assert_eq!(photocraft_engine::automate_cmds::fire_event(&mut app.session, "closeDocument"), 1);
    assert_eq!(app.session.active().unwrap().doc.layer_count(), layers + 1, "the binding ran on the active document");
    open_review(&mut app, &ctx, ["one.dng", "two.dng", "three.dng"], true);
    // Stage 1: with photographs waiting, Script Events are paused for every document, an accepted one included.
    assert!(app.session.set_active(0));
    let layers = app.session.active().unwrap().doc.layer_count();
    assert_eq!(photocraft_engine::automate_cmds::fire_event(&mut app.session, "closeDocument"), 0, "Script Events are paused while photographs wait");
    assert_eq!(app.session.active().unwrap().doc.layer_count(), layers, "the paused binding changed nothing");
    let waiting: Vec<DocPrint> = print(&app, &ctx, &writes).docs.into_iter().skip(1).collect();
    assert_eq!(waiting.len(), 3);
    // Control requests that fire those events in the app shell or the engine, with a photograph active.
    for step in 0..4 {
        let waiting_ids: Vec<DocId> = waiting.iter().map(|w| w.id).collect();
        let waiting_tab = app.session.documents().iter().position(|st| waiting_ids.contains(&st.doc.id)).unwrap();
        let accepted_tab = app.session.documents().iter().position(|st| !waiting_ids.contains(&st.doc.id));
        assert!(app.session.set_active(waiting_tab));
        // Close an accepted tab (the first and the last step), add one, and open one from a file.
        let (method, params) = match step {
            1 => ("engine.execute", json!({"command": "file.new", "params": {"width": 8, "height": 8}})),
            2 => ("app.open", json!({"path": "opened.dng"})),
            _ => ("engine.execute", json!({"command": "file.close", "params": {"document": accepted_tab.unwrap()}})),
        };
        let reply = call(&mut app, &ctx, method, params.clone());
        assert_not_refused(&reply, &format!("{method} {params}"));
        assert_waiting_unchanged(&app, &waiting, &format!("{method} {params} with Script Events enabled"));
    }
    assert!(app.session.documents().iter().all(|st| st.doc.id != accepted), "the accepted document was closed through the control channel");
    // An accepted document is saved with the hook enabled, again with a photograph as the previously active tab.
    add_accepted_document(&mut app, &ctx);
    assert_eq!(call(&mut app, &ctx, "app.save", json!({"path": "hooked.psd"}))["ok"], true);
    assert_waiting_unchanged(&app, &waiting, "app.save of an accepted document with Script Events enabled");
    finish();
}

/// The authorization hook (`automation_command`) is consulted for requests that go ahead, never for a refused
/// one: nothing happens to a waiting photograph, not even a policy check or a log line, before a refusal.
#[test]
fn a_refused_request_never_reaches_the_authorization_hook() {
    let writes = Writes::default();
    let calls = Rc::new(std::cell::Cell::new(0usize));
    let counter = calls.clone();
    let (mut app, ctx) = new_app_with(&writes, move |services| {
        services.automation_command = Some(Box::new(move |_id, _params| {
            counter.set(counter.get() + 1);
            Ok(())
        }));
    });
    open_review(&mut app, &ctx, ["one.dng", "two.dng", "three.dng"], true);
    for (command, params) in [("edit.fill", json!({"color": "#ff0000"})), ("file.save", json!({"path": "x.psd"})), ("file.closeAll", json!({}))] {
        assert_refused(&engine(&mut app, &ctx, command, params), command);
    }
    if calls.get() != 0 {
        violation(format!("the authorization hook ran {} time(s) for refused requests", calls.get()));
    }
    assert_eq!(engine(&mut app, &ctx, "file.new", json!({"width": 8, "height": 8}))["ok"], true);
    assert!(calls.get() >= 1, "the hook is wired: an allowed request reaches it");
    finish();
}

/// A confirmation prompt is a deferred command. One raised for an accepted document keeps that document: it
/// cannot be confirmed onto the photograph that has since become the active tab, and it still works for its
/// own document afterwards.
#[test]
fn a_confirmation_prompt_raised_for_an_accepted_document_stays_with_that_document() {
    let writes = Writes::default();
    let (mut app, ctx) = new_app(&writes);
    let accepted = add_accepted_document(&mut app, &ctx);
    let layers = app.session.active().unwrap().doc.layer_count();
    let raised = engine(&mut app, &ctx, "layer.delete", json!({"__trash": true}));
    assert_eq!(raised["result"]["confirmationPending"], true, "{raised}");
    let prompt = raised["result"]["dialog"].clone();
    open_review(&mut app, &ctx, ["one.dng", "two.dng", "three.dng"], false);
    for _ in 0..6 {
        let prompts: Vec<Value> = control::inspect(&app, &ctx)["dialogs"].as_array().cloned().unwrap_or_default();
        for p in prompts.iter().filter(|p| p["fields"]["__prefsui"] == "mismatch") {
            call(&mut app, &ctx, "ui.dialog.cancel", json!({"dialog": p["id"]}));
        }
        frames(&mut app, &ctx, 1);
    }
    assert_ne!(app.session.active().unwrap().doc.id, accepted, "a photograph is the active tab");
    let waiting: Vec<DocPrint> = print(&app, &ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    let before = print(&app, &ctx, &writes);
    assert_refused(&call(&mut app, &ctx, "ui.dialog.confirm", json!({"dialog": prompt})), "confirming a delete prompt onto a photograph");
    assert_same(&before, &print(&app, &ctx, &writes), "the delete prompt");
    // Back on its own document the prompt still does what it was raised for, and only that.
    let own = app.session.documents().iter().position(|st| st.doc.id == accepted).unwrap();
    assert!(app.session.set_active(own));
    let confirmed = call(&mut app, &ctx, "ui.dialog.confirm", json!({"dialog": prompt}));
    assert_eq!(confirmed["ok"], true, "{confirmed}");
    assert_eq!(app.session.active().unwrap().doc.layer_count(), layers - 1, "the layer left the document the prompt was raised for");
    assert_waiting_unchanged(&app, &waiting, "the delete prompt confirmed on its own document");
    finish();
}

/// A background save belongs to the document it started on, however the tabs change before it finishes.
fn late_save_job(cancel_the_review: bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as A, Mutex};
    let writes = Writes::default();
    let gate = A::new(AtomicBool::new(false));
    let saved: A<Mutex<Vec<String>>> = Default::default();
    let (worker_gate, worker_log) = (gate.clone(), saved.clone());
    let mut h = app_with_review_using(
        &writes,
        move |services| {
            services.automation_save = Some(A::new(move |_doc, path: &str, _settings, _ctl| {
                while !worker_gate.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                worker_log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(path.to_string());
                Ok(Vec::new())
            }));
        },
        |_| {},
    );
    assert_eq!(call_h(&mut h, "engine.execute", json!({"command": "file.new", "params": {"width": 8, "height": 8}}))["ok"], true);
    assert_eq!(call_h(&mut h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
    let accepted = h.state().session.active().unwrap().doc.id;
    let waiting: Vec<DocPrint> = print(h.state(), &h.ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    assert_eq!(waiting.len(), 3);
    h.state_mut().background_jobs = true;
    let started = call_h(&mut h, "app.save", json!({"path": "late.psd", "wait": false}));
    assert_eq!(started["result"]["pending"], true, "the save runs in the background: {started}");
    assert!(h.state().session.has_jobs());
    if cancel_the_review {
        // Cancel closes the photographs, so the accepted document's tab index changes while the save is in flight.
        let cancelled = call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"cancel": true}}}));
        assert_eq!(cancelled["ok"], true, "{cancelled}");
        h.run_steps(3);
        assert_eq!(h.state().session.documents().len(), 1, "Cancel discarded the three photographs");
    } else {
        assert!(h.state_mut().session.set_active(0), "a waiting photograph is the active tab while the save finishes");
    }
    gate.store(true, Ordering::SeqCst);
    for _ in 0..400 {
        h.run_steps(1);
        if !h.state().session.has_jobs() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(!h.state().session.has_jobs(), "the save finished");
    assert_eq!(*saved.lock().unwrap_or_else(std::sync::PoisonError::into_inner), vec!["late.psd".to_string()]);
    let owner = h.state().session.documents().iter().find(|st| st.doc.id == accepted).map(|st| st.path.clone());
    assert_eq!(owner, Some(Some("late.psd".to_string())), "the late save was recorded on the document it started on");
    if !cancel_the_review {
        assert_waiting_unchanged(h.state(), &waiting, "a late save finishing while a photograph is the active tab");
    }
    finish();
}

#[test]
fn a_background_save_finishing_while_a_photograph_is_the_active_tab_stays_with_its_own_document() {
    late_save_job(false);
}

#[test]
fn a_background_save_finishing_after_cancel_stays_with_its_own_document() {
    late_save_job(true);
}

/// Removes a scratch directory of synthetic files when the test ends, however it ends.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The full app with no documents open and nothing configured.
fn plain_harness(writes: &Writes) -> Harness<'static, PhotocraftApp> {
    let for_app = writes.clone();
    let mut h = Harness::builder().with_step_dt(1.0 / 60.0).with_size(vec2(1360.0, 900.0)).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
        PhotocraftApp::new(photocraft_engine::Session::new(), services(&for_app))
    });
    h.run_steps(3);
    h
}

/// A control `file.open` of a RAW is a background open when background jobs are on (the desktop app): it
/// returns at once, `run_waiting` puts Script Events back, and when the decode lands the shell queues the RAW
/// for Camera Raw and then fires "Open Document", whose steps run on the RAW before Open or Cancel. Control
/// requests are meant to run with Script Events off, so such an open is refused while one is bound.
#[test]
fn a_control_started_background_open_cannot_run_an_open_document_hook_on_a_raw() {
    use photocraft_engine::automate_cmds::ScriptBinding;
    let scratch = Scratch(std::env::temp_dir().join("opencode").join(format!("p0-synthetic-{}", std::process::id())));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let path = scratch.0.join("hooked.dng");
    std::fs::write(&path, dng()).unwrap();
    let writes = Writes::default();
    let mut h = plain_harness(&writes);
    h.state_mut().session.edit_prefs(|prefs| {
        prefs.script_events.enabled = true;
        prefs.script_events.bindings = vec![ScriptBinding {
            event: "openDocument".into(),
            script: None,
            steps: Some(json!([["layer.new.layer", {"name": "From the hook"}], ["edit.fill", {"color": "#ff0000"}]])),
            name: "Hook".into(),
        }];
    });
    h.state_mut().background_jobs = true;
    let reply = call_h(&mut h, "engine.execute", json!({"command": "file.open", "params": {"path": path.to_string_lossy()}}));
    if !(reply["ok"] == false && reply["error"].as_str().is_some_and(|e| e.contains("Script Event"))) {
        violation(format!("a background open with an Open Document hook bound was not refused: {reply}"));
    }
    // Whatever happened, let any job land, then look at what the hook did.
    for _ in 0..300 {
        h.run_steps(1);
        if !h.state().session.has_jobs() && !h.state().session.documents().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.run_steps(3);
    for st in h.state().session.documents() {
        if st.revision != 1 || st.history.past_len() != 0 {
            violation(format!("the Open Document hook edited a RAW before Camera Raw decided it: revision {}, history {}", st.revision, st.history.past_len()));
        }
    }
    // The documented way to open a file from a controller keeps Script Events off and is unaffected.
    finish();
}

/// A Camera Raw Filter dialog opened on an accepted document must stop taking the controller's `commit` once a
/// RAW has become the active tab: the commit would run on the active tab. (It used to fail only because layer ids
/// are unique across documents.)
#[test]
fn a_camera_raw_filter_dialog_cannot_be_committed_once_a_raw_has_become_the_active_tab() {
    let writes = Writes::default();
    let (mut app, ctx) = new_app(&writes);
    let accepted = add_accepted_document(&mut app, &ctx);
    assert_eq!(engine(&mut app, &ctx, "edit.fill", json!({"color": "#808080"}))["ok"], true);
    camera_raw_ui::open(&mut app, &ctx).unwrap();
    frames(&mut app, &ctx, 2);
    let dng = dng();
    app.open_bytes("late.dng", &dng).unwrap();
    frames(&mut app, &ctx, 3);
    cancel_prompts(&mut app, &ctx);
    let active = app.session.active().unwrap().doc.id;
    assert_ne!(active, accepted, "the RAW that landed is the active tab");
    let before = print(&app, &ctx, &writes);
    let reply = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"commit": true}}}));
    if !(reply["ok"] == false && reply["error"].as_str().is_some_and(|e| e.contains("modal"))) {
        violation(format!("the Filter dialog took a commit while a RAW is the active tab: {reply}"));
    }
    assert_same(&before, &print(&app, &ctx, &writes), "a stale Camera Raw Filter commit");
    finish();
}

/// `source` is a pair of coordinates for the clone and healing tools and a tab index for `layer.copyToDocument`.
/// Only a number or a name (or a nested `document`) is a document reference; coordinates and deep parameters on an
/// accepted document are not refused.
#[test]
fn coordinates_and_deep_parameters_on_an_accepted_document_are_not_taken_for_document_references() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    add_accepted_document(&mut app, &ctx);
    let mut deep = json!(1);
    for level in 0..14 {
        deep = json!({ format!("level{level}"): deep });
    }
    for (command, params) in [
        ("paint.cloneStamp", json!({"points": [[6, 3]], "source": [0, 5]})),
        ("paint.healingBrush", json!({"points": [[6, 3]], "source": [-5, 10]})),
        ("cloneSource.set", json!({"source": [8, 8]})),
        ("edit.fill", json!({"color": "#00ff00", "extra": deep})),
    ] {
        let reply = engine(&mut app, &ctx, command, params.clone());
        assert_not_refused(&reply, &format!("{command} {params}"));
    }
    finish();
}

/// Input queued before a RAW lands is delivered one step per frame, after the guard judged it when nothing
/// waited. A palette opened and typed into, then a RAW arriving: the queued Enter used to run the top hit on it.
#[test]
fn input_queued_before_a_raw_lands_is_dropped_when_the_raw_is_queued_for_review() {
    let writes = Writes::default();
    let mut h = plain_harness(&writes);
    assert_eq!(call_h(&mut h, "engine.execute", json!({"command": "edit.search", "params": {}}))["ok"], true);
    h.run_steps(3);
    let ctx = h.ctx.clone();
    for (method, params) in [("ui.type", json!({"text": "Invert"})), ("ui.key", json!({"key": "Enter"}))] {
        let (request, _reply) = control::ControlRequest::new(method, params);
        assert!(matches!(control::handle(h.state_mut(), &ctx, &request), control::Outcome::AfterInput), "{method} was queued");
    }
    let dng = dng();
    h.state_mut().open_bytes("queued.dng", &dng).unwrap();
    loop {
        let step = h.state_mut().take_synthetic_step();
        if step.is_empty() {
            break;
        }
        for event in step {
            h.event(event);
        }
        h.step();
    }
    h.run_steps(3);
    for st in h.state().session.documents() {
        if st.revision != 1 || st.history.past_len() != 0 {
            violation(format!("queued input edited the waiting RAW: revision {}, history {}", st.revision, st.history.past_len()));
        }
    }
    finish();
}

/// The other order of the Open Document hazard: the open is started first (no hook bound, so it is allowed), and a
/// request binds the hook while the decode is still running.
#[test]
fn a_script_event_cannot_be_bound_while_a_background_open_is_still_decoding() {
    let scratch = Scratch(std::env::temp_dir().join("opencode").join(format!("p0-order-{}", std::process::id())));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let path = scratch.0.join("late_hook.dng");
    std::fs::write(&path, dng()).unwrap();
    let writes = Writes::default();
    let mut h = plain_harness(&writes);
    h.state_mut().background_jobs = true;
    let opened = call_h(&mut h, "engine.execute", json!({"command": "file.open", "params": {"path": path.to_string_lossy()}}));
    assert_eq!(opened["ok"], true, "no hook is bound yet, so the open is allowed: {opened}");
    let bind = json!({"enabled": true, "add": {"event": "openDocument", "steps": [["edit.fill", {"color": "#ff0000"}]]}});
    for (command, params) in [("file.scripts.scriptEventsManager", bind), ("prefs.set", json!({"path": "scriptEvents.enabled", "value": true}))] {
        let reply = call_h(&mut h, "engine.execute", json!({"command": command, "params": params}));
        if !(reply["ok"] == false && reply["error"].as_str().is_some_and(|e| e.contains("Script Event"))) {
            violation(format!("{command} could bind a hook while the open was decoding: {reply}"));
        }
    }
    for _ in 0..300 {
        h.run_steps(1);
        if !h.state().session.has_jobs() && !h.state().session.documents().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.run_steps(3);
    assert_eq!(h.state().session.documents().len(), 1, "the RAW opened for review");
    for st in h.state().session.documents() {
        if st.revision != 1 || st.history.past_len() != 0 {
            violation(format!("a hook bound during the open edited the RAW: revision {}, history {}", st.revision, st.history.past_len()));
        }
    }
    assert!(h.state().session.prefs().script_events.bindings.is_empty(), "no binding was added");
    // Once the open has landed, nothing is decoding any more and the same request is judged by the usual rules.
    finish();
}

// =============================================================================================================
// S2-P0 Stage 1: execution boundary. A provisional RAW must not be edited by anything that runs later or from
// another origin than a control request: Script Event steps, the command palette, a dialog opened earlier, a
// floating window. Each test states what it needs to reproduce; a chain that does not reproduce on the frozen
// source is reported as such and kept as a guard, never forced to fail.
// =============================================================================================================

fn key_enter() -> egui::Event {
    egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
}

/// A Script Event binding made straight on the preferences (the harness's setup, not a route under test).
fn bind_hook(app: &mut PhotocraftApp, event: &str, steps: Value) {
    use photocraft_engine::automate_cmds::ScriptBinding;
    app.session.edit_prefs(|prefs| {
        prefs.script_events.enabled = true;
        prefs.script_events.bindings = vec![ScriptBinding { event: event.into(), script: None, steps: Some(steps), name: "Hook".into() }];
    });
}

fn scratch_dng(tag: &str) -> (Scratch, std::path::PathBuf) {
    let scratch = Scratch(std::env::temp_dir().join("opencode").join(format!("p0-stage1-{tag}-{}", std::process::id())));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let path = scratch.0.join(format!("{tag}.dng"));
    std::fs::write(&path, dng()).unwrap();
    (scratch, path)
}

/// Let background jobs land (a decode, a save) while frames run.
fn settle(h: &mut Harness<'_, PhotocraftApp>) {
    for _ in 0..400 {
        h.run_steps(1);
        if !h.state().session.has_jobs() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.run_steps(3);
}

/// Every document untouched: used where every open document is a RAW.
fn assert_all_documents_untouched(app: &PhotocraftApp, what: &str) {
    for st in app.session.documents() {
        if st.revision != 1 || st.history.past_len() != 0 {
            violation(format!("{what}: document {:?} was edited: revision {}, history {}", st.doc.id, st.revision, st.history.past_len()));
        }
    }
}

/// R5-1. A generic Command dialog (not Preferences) is confirmed while a background open is still decoding. The
/// control refusal knows Preferences dialogs and the two command routes, so the binding goes through; when the RAW
/// lands the shell queues it for Camera Raw and then fires Open Document on it.
#[test]
fn stage1_r5_1_a_generic_command_dialog_cannot_bind_a_hook_that_edits_a_raw_landing_later() {
    let (_scratch, path) = scratch_dng("r51");
    let writes = Writes::default();
    let mut h = plain_harness(&writes);
    h.state_mut().background_jobs = true;
    let opened = call_h(&mut h, "engine.execute", json!({"command": "file.open", "params": {"path": path.to_string_lossy()}}));
    assert_eq!(opened["ok"], true, "no hook is bound yet, so the open is allowed: {opened}");
    let dialog = call_h(&mut h, "ui.dialog.open", json!({"kind": "command", "command": "file.scripts.scriptEventsManager"}));
    let id = dialog["result"]["dialog"].as_u64().unwrap_or_else(|| panic!("no dialog id: {dialog}"));
    call_h(&mut h, "ui.dialog.set", json!({"dialog": id, "field": "enabled", "value": true}));
    call_h(&mut h, "ui.dialog.set", json!({"dialog": id, "field": "add", "value": {"event": "openDocument", "steps": [["edit.fill", {"color": "#ff0000"}]]}}));
    let confirm = call_h(&mut h, "ui.dialog.confirm", json!({"dialog": id}));
    eprintln!("R5-1 generic command dialog confirm: {confirm}");
    eprintln!("R5-1 binding present after the confirm: {}", !h.state().session.prefs().script_events.bindings.is_empty());
    settle(&mut h);
    assert_eq!(h.state().session.documents().len(), 1, "the RAW opened for review");
    assert_all_documents_untouched(h.state(), "a hook bound through a generic Command dialog");
    finish();
}

/// R5-2. A user save that runs in the background finishes after the request that started it returned, with Script
/// Events back on. The `saveDocument` hook runs on the saved document A; a step that activates a RAW makes the next
/// step edit it.
#[test]
fn stage1_r5_2_a_background_save_hook_cannot_retarget_a_waiting_raw() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as A, Mutex};
    let writes = Writes::default();
    let gate = A::new(AtomicBool::new(false));
    let saved: A<Mutex<Vec<String>>> = Default::default();
    let (worker_gate, worker_log) = (gate.clone(), saved.clone());
    let mut h = app_with_review_using(
        &writes,
        move |services| {
            services.save_file = Some(A::new(move |_doc, path: &str, _settings, _ctl| {
                while !worker_gate.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                worker_log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(path.to_string());
                Ok(Vec::new())
            }));
        },
        |h| {
            assert_eq!(call_h(h, "engine.execute", json!({"command": "file.new", "params": {"width": 8, "height": 8}}))["ok"], true);
            assert_eq!(call_h(h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
        },
    );
    let accepted = h.state().session.documents().first().map(|st| st.doc.id).unwrap();
    let waiting: Vec<DocPrint> = print(h.state(), &h.ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    assert_eq!(waiting.len(), 3);
    h.state_mut().background_jobs = true;
    assert!(h.state_mut().session.set_active(0), "the accepted document is the active tab");
    bind_hook(h.state_mut(), "saveDocument", json!([["document.activate", {"document": 1}], ["edit.fill", {"color": "#ff0000"}]]));
    let started = call_h(&mut h, "engine.execute", json!({"command": "file.saveAs", "params": {"path": "kept.psd"}, "wait": false}));
    eprintln!("R5-2 save request: {started}");
    assert!(h.state().session.has_jobs(), "the user save runs in the background: {started}");
    gate.store(true, Ordering::SeqCst);
    for _ in 0..400 {
        h.run_steps(1);
        if !h.state().session.has_jobs() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.run_steps(3);
    assert_eq!(*saved.lock().unwrap_or_else(std::sync::PoisonError::into_inner), vec!["kept.psd".to_string()], "the accepted document was saved");
    assert_waiting_unchanged(h.state(), &waiting, "a saveDocument hook that retargets a waiting RAW");
    finish();
}

/// Open the palette, type a command that changes the document, and deliver Enter in the very frame in which a
/// background decode lands. `same_step` puts the text in that frame too.
fn palette_enter_on_the_decode_frame(same_step: bool) {
    let (_scratch, path) = scratch_dng(if same_step { "r53b" } else { "r53a" });
    let writes = Writes::default();
    let mut h = plain_harness(&writes);
    h.state_mut().background_jobs = true;
    assert_eq!(call_h(&mut h, "engine.execute", json!({"command": "edit.search", "params": {}}))["ok"], true);
    h.run_steps(3);
    assert!(h.state().ui.palette_open, "the palette is open and focused");
    if !same_step {
        h.event(egui::Event::Text("flip canvas horizontal".into()));
        h.run_steps(2);
    }
    let opened = call_h(&mut h, "engine.execute", json!({"command": "file.open", "params": {"path": path.to_string_lossy()}}));
    assert_eq!(opened["ok"], true, "{opened}");
    // The worker has finished; the result is only taken when the next frame's `logic` polls the jobs.
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(h.state().session.documents().is_empty(), "the decode has not landed yet");
    if same_step {
        h.event(egui::Event::Text("flip canvas horizontal".into()));
    }
    h.event(key_enter());
    h.step();
    assert_eq!(h.state().session.documents().len(), 1, "the decode landed in the frame that carried the key (else this test proves nothing)");
    h.run_steps(3);
    settle(&mut h);
    assert_eq!(h.state().session.documents().len(), 1, "the RAW opened for review");
    assert_all_documents_untouched(h.state(), if same_step { "Text and Enter in the decode frame" } else { "Enter in the decode frame" });
    finish();
}

/// R5-3. The step already merged into the frame cannot be cleared by `queue_open_dialog`, and the palette draws
/// before Camera Raw's own panel and ignores `blocks_editor`.
#[test]
fn stage1_r5_3_enter_in_the_decode_frame_cannot_run_the_palette_hit_on_the_raw() {
    palette_enter_on_the_decode_frame(false);
}

#[test]
fn stage1_r5_3_text_and_enter_in_the_decode_frame_cannot_run_the_palette_hit_on_the_raw() {
    palette_enter_on_the_decode_frame(true);
}

/// Native route, no control channel: a Fill dialog that was already open when the RAWs arrived, then Enter.
#[test]
fn stage1_native_enter_on_a_dialog_opened_before_the_review_cannot_fill_the_raw() {
    let writes = Writes::default();
    let mut h = app_with_review(&writes, |h| {
        assert_eq!(call_h(h, "engine.execute", json!({"command": "file.new", "params": {"width": 16, "height": 16}}))["ok"], true);
        assert_eq!(call_h(h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
        assert_eq!(call_h(h, "ui.dialog.open", json!({"kind": "command", "command": "edit.fill"}))["ok"], true);
        h.run_steps(3);
    });
    assert!(control::inspect(h.state(), &h.ctx)["dialogs"].as_array().is_some_and(|d| !d.is_empty()), "the Fill dialog is still open");
    let before = print(h.state(), &h.ctx, &writes);
    h.event(key_enter());
    h.run_steps(4);
    // Enter consumes the dialog (it is closed before its command runs, and the command is refused); nothing else moves.
    let mut after = print(h.state(), &h.ctx, &writes);
    assert_eq!((before.dialogs, after.dialogs), (1, 0), "the Fill dialog is consumed by the refused confirm");
    after.dialogs = before.dialogs;
    assert_same(&before, &after, "a native Enter on a dialog opened before the review");
    assert!(h.state().ui.status.contains("unaccepted RAW"), "the refusal is shown to the person: {:?}", h.state().ui.status);
    finish();
}

/// Native route: a palette opened and typed into before the RAWs arrived stays open over the review.
#[test]
fn stage1_native_enter_in_a_palette_opened_before_the_review_cannot_edit_the_raw() {
    let writes = Writes::default();
    let mut h = app_with_review(&writes, |h| {
        assert_eq!(call_h(h, "engine.execute", json!({"command": "edit.search", "params": {}}))["ok"], true);
        h.run_steps(3);
        h.event(egui::Event::Text("flip canvas horizontal".into()));
        h.run_steps(2);
    });
    assert!(!h.state().ui.palette_open, "the palette is closed when the RAWs land: it runs shell commands the command guard does not see");
    let before = print(h.state(), &h.ctx, &writes);
    h.event(key_enter());
    h.run_steps(4);
    assert_same(&before, &print(h.state(), &h.ctx, &writes), "a native Enter in a palette opened before the review");
    finish();
}

/// Native origin sweep: every registered command, default parameters, called through `PhotocraftApp::run` (what the
/// palette, dialogs, floating windows and tools call) while three photographs wait. The control sweep goes through
/// the control guard; this one does not.
#[test]
fn stage1_every_registered_command_called_natively_leaves_waiting_photographs_untouched() {
    const NOT_RUN: &[&str] = &["file.revealInFinder", "file.import.wiaSupport", "file.print", "file.printOneCopy"];
    let writes = Writes::default();
    let fresh = || {
        let (app, ctx) = review_app(&writes, true);
        let waiting: Vec<DocPrint> = print(&app, &ctx, &writes).docs;
        let ids: Vec<DocId> = waiting.iter().map(|d| d.id).collect();
        (app, ctx, waiting, ids)
    };
    let mut totals = Vec::new();
    let mut reached = Vec::new();
    for accepted_is_active in [false, true] {
        let (mut app, mut _ctx, mut waiting, mut ids) = fresh();
        let (mut refused, mut allowed) = (0, 0);
        for spec in photocraft_engine::commands::command_specs() {
            if NOT_RUN.contains(&spec.id) {
                continue;
            }
            if !app.session.documents().iter().any(|st| !ids.contains(&st.doc.id)) {
                app.run("file.new", json!({"width": 8, "height": 8})).unwrap();
                app.run("layer.new.layer", json!({})).unwrap();
            }
            let accepted = app.session.documents().iter().position(|st| !ids.contains(&st.doc.id)).unwrap();
            let waiting_tab = app.session.documents().iter().position(|st| ids.contains(&st.doc.id)).unwrap_or(0);
            assert!(app.session.set_active(if accepted_is_active { accepted } else { waiting_tab }));
            match app.run(spec.id, json!({})) {
                Err(error) if error.contains("unaccepted RAW") => refused += 1,
                _ => allowed += 1,
            }
            let before = violation_count();
            assert_waiting_unchanged(
                &app,
                &waiting,
                &format!("`{}` called natively (active: {})", spec.id, if accepted_is_active { "accepted" } else { "photograph" }),
            );
            if violation_count() != before {
                reached.push((spec.id, accepted_is_active));
                (app, _ctx, waiting, ids) = fresh();
            }
        }
        totals.push((accepted_is_active, refused, allowed));
    }
    eprintln!("native sweep totals (accepted active, refused, allowed): {totals:?}");
    if !reached.is_empty() {
        violation(format!("{} command(s) changed a waiting photograph when called natively: {reached:?}", reached.len()));
    }
    finish();
}

/// Closing an accepted tab natively (Script Events on, no control request around it) while a RAW waits: the engine's
/// `closeDocument` hook runs on whichever tab is active afterwards.
#[test]
fn stage1_a_close_hook_cannot_edit_a_waiting_raw_after_an_accepted_tab_closes() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let accepted = add_accepted_document(&mut app, &ctx);
    let waiting: Vec<DocPrint> = print(&app, &ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    assert_eq!(waiting.len(), 3);
    bind_hook(&mut app, "closeDocument", json!([["edit.fill", {"color": "#ff0000"}]]));
    let tab = app.session.documents().iter().position(|st| st.doc.id == accepted).unwrap();
    let closed = app.run("file.close", json!({"document": tab}));
    eprintln!("close hook: file.close -> {closed:?}; active tab afterwards {:?}", app.session.active_index());
    assert_waiting_unchanged(&app, &waiting, "a closeDocument hook after the accepted tab closed");
    finish();
}

/// A single-RAW review whose Open starts a background re-develop. Returns the job id of the commit.
fn committing_raw_harness(writes: &Writes) -> (Harness<'static, PhotocraftApp>, u64) {
    committing_raw_harness_with(writes, json!({"exposure": 1.0}))
}

fn committing_raw_harness_with(writes: &Writes, settings: Value) -> (Harness<'static, PhotocraftApp>, u64) {
    let for_app = writes.clone();
    let mut h = Harness::builder().with_step_dt(1.0 / 60.0).with_size(vec2(1360.0, 900.0)).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
        PhotocraftApp::new(photocraft_engine::Session::new(), services(&for_app))
    });
    h.run_steps(3);
    h.state_mut().open_bytes("only.dng", &dng()).unwrap();
    h.run_steps(6);
    for _ in 0..6 {
        let prompts: Vec<Value> = control::inspect(h.state(), &h.ctx)["dialogs"].as_array().cloned().unwrap_or_default();
        for prompt in prompts {
            call_h(&mut h, "ui.dialog.cancel", json!({"dialog": prompt["id"]}));
        }
        h.run_steps(1);
    }
    assert!(!control::inspect(h.state(), &h.ctx)["cameraRaw"].is_null(), "Camera Raw is open");
    h.state_mut().background_jobs = true;
    let set = call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"set": settings}}}));
    assert_eq!(set["ok"], true, "{set}");
    let commit = call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"commit": true}}, "wait": false}));
    let job = commit["result"]["job"].as_u64().unwrap_or_else(|| panic!("the commit did not start a job: {commit}"));
    (h, job)
}

/// jobs.cancel, control side: the commit job belongs to Camera Raw. Cancelling it leaves the RAW in the session,
/// developed with the defaults, with the raw-stage choices lost.
#[test]
fn stage1_jobs_cancel_from_the_control_channel_cannot_cancel_a_raw_commit() {
    let writes = Writes::default();
    let (mut h, job) = committing_raw_harness(&writes);
    let reply = call_h(&mut h, "jobs.cancel", json!({"job": job}));
    eprintln!("jobs.cancel {{job}} during a commit: {reply}");
    let all = call_h(&mut h, "jobs.cancel", json!({}));
    eprintln!("jobs.cancel {{}} during a commit: {all}");
    settle(&mut h);
    let cancelled = h.state().ui.status.contains("Open Images stopped");
    if cancelled {
        violation(format!("the control channel cancelled a Camera Raw commit: {}", h.state().ui.status));
    }
    finish();
}

/// What a cancelled commit must leave: every RAW still unaccepted (declared provisional, refused by the guards, not
/// saved), the cancelled one back in Camera Raw with its adjustments, and no other photograph opened for it.
fn assert_returned_to_review(h: &mut Harness<'_, PhotocraftApp>, what: &str, raws: usize, exposure: f64) {
    let open: Vec<DocId> = h.state().session.documents().iter().map(|st| st.doc.id).collect();
    assert_eq!(open.len(), raws, "{what}: no document appeared or went away");
    let mut declared = h.state().session.provisional_documents();
    declared.sort_by_key(|id| id.0);
    let mut expected = open.clone();
    expected.sort_by_key(|id| id.0);
    if declared != expected {
        violation(format!("{what}: the photographs are not all provisional: declared {declared:?}, open {expected:?}"));
    }
    for st in h.state().session.documents() {
        if st.history.past_len() != 0 {
            violation(format!("{what}: {:?} was edited (history {})", st.doc.id, st.history.past_len()));
        }
    }
    let edit = h.state_mut().run("edit.fill", json!({"color": "#ff0000"}));
    if !is_unaccepted_refusal(&edit) {
        violation(format!("{what}: a native edit was not refused: {edit:?}"));
    }
    let save = h.state_mut().save_automation_with(Some("leak.psd".into()), Default::default());
    if !is_unaccepted_refusal(&save) {
        violation(format!("{what}: a save was not refused: {save:?}"));
    }
    let dialog = control::inspect(h.state(), &h.ctx)["cameraRaw"].clone();
    if dialog.is_null() {
        violation(format!("{what}: Camera Raw is not back for the photographer to choose Open or Cancel"));
    } else if (dialog["params"]["exposure"].as_f64().unwrap_or(0.0) - exposure).abs() > 1e-6 {
        violation(format!("{what}: the adjustments were lost: {}", dialog["params"]));
    }
    if h.state().session.has_jobs() {
        violation(format!("{what}: another job was started on its own"));
    }
}

/// Esc (the native cancel) during Open's re-develop must not choose Open or Cancel for the photographer.
#[test]
fn stage1_native_cancel_of_the_redevelop_returns_the_photograph_to_review() {
    let writes = Writes::default();
    let (mut h, job) = committing_raw_harness(&writes);
    let id = h.state().session.documents()[0].doc.id;
    photocraft_ui_egui::jobs_ui::cancel(h.state_mut(), photocraft_engine::jobs::JobId(job));
    settle(&mut h);
    assert_eq!(h.state().session.documents()[0].doc.id, id, "the provisional document keeps its identity");
    assert_returned_to_review(&mut h, "Esc during the re-develop", 1, 1.0);
    // Open can be chosen again and then accepts it normally.
    let again = call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"commit": true}}, "wait": false}));
    assert_eq!(again["ok"], true, "{again}");
    settle(&mut h);
    assert!(h.state().session.provisional_documents().is_empty(), "Open accepted it");
    finish();
}

/// Open All of three photographs, the first one cancelled natively while it develops: nothing is accepted and the
/// other two are not opened on their own.
#[test]
fn stage1_native_cancel_during_open_all_keeps_every_unfinished_photograph_unaccepted() {
    let writes = Writes::default();
    let mut h = app_with_review(&writes, |_| {});
    h.state_mut().background_jobs = true;
    assert_eq!(call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"set": {"exposure": 1.0}}}}))["ok"], true);
    let commit = call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"commit": true}}, "wait": false}));
    let job = commit["result"]["first"]["job"].as_u64().unwrap_or_else(|| panic!("the commit did not start a job: {commit}"));
    photocraft_ui_egui::jobs_ui::cancel(h.state_mut(), photocraft_engine::jobs::JobId(job));
    settle(&mut h);
    assert_returned_to_review(&mut h, "Esc during Open All", 3, 1.0);
    finish();
}

/// The second commit phase (the Camera Raw step on the re-developed document) is synchronous today: `filter.cameraRaw`
/// never answers with a job, even with background jobs on. This pins that premise; the recovery for a cancelled step is
/// covered by the unit test `a_cancelled_camera_raw_step_returns_the_replacement_to_review`, and this test fails the day
/// the command becomes a job without the end-to-end test being written.
#[test]
fn stage1_the_camera_raw_filter_step_is_not_a_background_job_today() {
    let writes = Writes::default();
    let (mut h, _job) = committing_raw_harness_with(&writes, json!({"exposure": 1.0, "contrast": 25.0}));
    let mut seen = Vec::new();
    for _ in 0..300 {
        h.run_steps(1);
        seen.extend(h.state().session.jobs().into_iter().map(|j| j.command));
        if !h.state().session.has_jobs() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.run_steps(3);
    assert!(!seen.iter().any(|c| c == "filter.cameraRaw"), "filter.cameraRaw became a background job: write the end-to-end cancel test ({seen:?})");
    assert!(h.state().session.provisional_documents().is_empty(), "both phases finished: the photograph is accepted");
    finish();
}
/// Tab indexes of the documents that are not in `accepted`.
fn tabs_other_than(app: &PhotocraftApp, accepted: &[DocId]) -> Vec<usize> {
    app.session.documents().iter().enumerate().filter(|(_, st)| !accepted.contains(&st.doc.id)).map(|(index, _)| index).collect()
}

fn is_unaccepted_refusal<T: std::fmt::Debug>(result: &Result<T, String>) -> bool {
    matches!(result, Err(error) if error.contains("unaccepted RAW"))
}

/// Explicit document targets, native origin: a command that names a waiting RAW (by tab index, by file name, or as
/// a source or destination) is refused while another document is the active tab, for the reviewed, queued and
/// parked photographs alike. The accepted document stays addressable.
#[test]
fn stage1_native_commands_that_name_a_waiting_raw_are_refused_for_reviewed_queued_and_parked_photographs() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    // One photograph is parked: it was under review and another was selected in the Filmstrip.
    let state = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {}}}))["result"].clone();
    let others: Vec<u64> = state["filmstrip"].as_array().unwrap().iter().filter(|p| p["active"] == false).filter_map(|p| p["document"].as_u64()).collect();
    let selected = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"selectRaw": others[0]}}}));
    assert_eq!(selected["ok"], true, "{selected}");
    frames(&mut app, &ctx, 2);
    let accepted = add_accepted_document(&mut app, &ctx);
    let ids: Vec<DocId> = app.session.documents().iter().map(|st| st.doc.id).filter(|id| *id != accepted).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(app.session.provisional_documents(), ids, "all three are provisional: reviewed, queued and parked");
    let before = print(&app, &ctx, &writes);
    let waiting: Vec<DocPrint> = print(&app, &ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    for index in tabs_other_than(&app, &[accepted]) {
        let name = app.session.documents()[index].doc.name.clone();
        for (command, params) in [
            ("document.activate", json!({"document": index})),
            ("document.activate", json!({"document": name})),
            ("file.close", json!({"document": index})),
            ("document.move", json!({"document": index, "to": 0})),
            ("layer.copyToDocument", json!({"source": index})),
            ("layer.copyToDocument", json!({"destination": index})),
            ("layer.copyToDocument", json!({"destination": {"document": index}})),
            ("edit.fill", json!({"document": index, "color": "#ff0000"})),
        ] {
            let result = app.run(command, params.clone());
            if !is_unaccepted_refusal(&result) {
                violation(format!("native {command} {params} was not refused for the waiting RAW {index}: {result:?}"));
            }
        }
    }
    assert_same(&before, &print(&app, &ctx, &writes), "native commands that name a waiting RAW");
    // The accepted document is still addressable and editable natively.
    let tab = app.session.documents().iter().position(|st| st.doc.id == accepted).unwrap();
    assert!(app.run("document.activate", json!({"document": tab})).is_ok());
    assert!(app.run("edit.fill", json!({"color": "#00ff00"})).is_ok(), "the accepted document is edited natively");
    assert_waiting_unchanged(&app, &waiting, "after the accepted document was edited");
    finish();
}

/// Nested actions and scripts: `actions.play` is refused natively while a RAW waits, and a hook whose steps call it
/// (or retarget, or name a RAW) does not run at all.
#[test]
fn stage1_nested_actions_and_scripts_cannot_reach_a_waiting_raw() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let accepted = add_accepted_document(&mut app, &ctx);
    let waiting: Vec<DocPrint> = print(&app, &ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    for (command, params) in [
        ("actions.play", json!({})),
        ("actions.play", json!({"name": "Default"})),
        ("file.scripts.browse", json!({"steps": [["edit.fill", {"color": "#ff0000"}]]})),
        ("plugin.run", json!({})),
    ] {
        let result = app.run(command, params.clone());
        if !is_unaccepted_refusal(&result) {
            violation(format!("native {command} {params} was not refused while a RAW waits: {result:?}"));
        }
    }
    // A hook that calls an action, activates a RAW and edits it, or closes it by name: none of it runs.
    for event in ["openDocument", "saveDocument", "closeDocument", "newDocument", "everything"] {
        bind_hook(
            &mut app,
            event,
            json!([["actions.play", {}], ["document.activate", {"document": 1}], ["edit.fill", {"color": "#ff0000"}], ["file.close", {"document": 2}]]),
        );
        assert!(app.session.set_active(app.session.documents().iter().position(|st| st.doc.id == accepted).unwrap()));
        let ran = photocraft_engine::automate_cmds::fire_event(&mut app.session, event);
        assert_eq!(ran, 0, "the {event} hook ran while a RAW waits");
    }
    assert_waiting_unchanged(&app, &waiting, "hooks and nested actions");
    assert_eq!(app.session.documents().len(), 4, "no document was closed");
    finish();
}

/// Lab port (main 0.9.0, D-16): a RAW opened natively goes straight to Camera Raw with no profile prompt in front, so
/// Enter cannot answer one on its behalf; the waiting photograph is unchanged and stays held.
#[test]
fn stage1_a_waiting_raw_opened_natively_raises_no_profile_prompt() {
    let writes = Writes::default();
    let mut h = plain_harness(&writes);
    h.state_mut().open_bytes("prompted.dng", &dng()).unwrap();
    h.run_steps(6);
    let dialogs = control::inspect(h.state(), &h.ctx)["dialogs"].as_array().cloned().unwrap_or_default();
    assert!(dialogs.iter().all(|d| d["fields"]["__prefsui"] != "mismatch"), "no profile prompt over the review: {dialogs:?}");
    assert!(!control::inspect(h.state(), &h.ctx)["cameraRaw"].is_null(), "the review is showing");
    let before = print(h.state(), &h.ctx, &writes);
    h.event(key_enter());
    h.run_steps(4);
    let after = print(h.state(), &h.ctx, &writes);
    assert_eq!(after.docs.len(), before.docs.len());
    for (b, a) in before.docs.iter().zip(&after.docs) {
        assert_doc_same(b, a, "Enter over a review with no prompt");
    }
    finish();
}

/// The provisional set's lifetime, from queueing to Open All to a second review and Cancel: it holds exactly the
/// photographs that wait, never an accepted document, and is empty once nothing waits.
#[test]
fn stage1_the_provisional_set_follows_a_review_from_queue_to_open_all_and_cancel() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let ids: Vec<DocId> = app.session.documents().iter().map(|st| st.doc.id).collect();
    assert_eq!(app.session.provisional_documents(), ids, "queued: every photograph is provisional");
    let accepted = add_accepted_document(&mut app, &ctx);
    assert!(!app.session.provisional_documents().contains(&accepted), "an accepted document is never provisional");
    // Open All: the photographs are accepted one after another; the set only shrinks and ends empty.
    let committed = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"commit": true}}}));
    assert_eq!(committed["ok"], true, "{committed}");
    let mut size = app.session.provisional_documents().len();
    for _ in 0..40 {
        frames(&mut app, &ctx, 1);
        cancel_prompts(&mut app, &ctx);
        let now = app.session.provisional_documents();
        assert!(now.len() <= size, "the set never grows while photographs are accepted: {size} -> {}", now.len());
        assert!(now.iter().all(|id| app.session.documents().iter().any(|st| st.doc.id == *id)), "only open documents are reported");
        size = now.len();
        if size == 0 && !app.session.has_jobs() {
            break;
        }
    }
    assert_eq!(app.session.provisional_documents(), Vec::<DocId>::new(), "every photograph was accepted: nothing is provisional");
    // A second review and Cancel: the set follows the review and is released by Cancel in the same request.
    let dng = dng();
    for name in ["again-one.dng", "again-two.dng"] {
        app.open_bytes(name, &dng).unwrap();
    }
    frames(&mut app, &ctx, 3);
    cancel_prompts(&mut app, &ctx);
    assert_eq!(app.session.provisional_documents().len(), 2);
    let cancelled = call(&mut app, &ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"cancel": true}}}));
    assert_eq!(cancelled["ok"], true, "{cancelled}");
    assert!(app.session.provisional_documents().is_empty(), "Cancel releases the set at once, before any frame");
    // Script Events work again, and the accepted document is edited and saved like any other.
    bind_hook(&mut app, "closeDocument", json!([["layer.new.layer", {}]]));
    assert!(app.session.set_active(app.session.documents().iter().position(|st| st.doc.id == accepted).unwrap()));
    let layers = app.session.active().unwrap().doc.layer_count();
    assert_eq!(photocraft_engine::automate_cmds::fire_event(&mut app.session, "closeDocument"), 1);
    assert_eq!(app.session.active().unwrap().doc.layer_count(), layers + 1);
    assert_eq!(call(&mut app, &ctx, "app.save", json!({"path": "after_cancel.psd"}))["ok"], true);
    finish();
}

/// Old-to-new DocId replacement: Open with a changed exposure closes the default-developed document and opens a
/// re-developed one under a new id. At every frame the document a job is working on is provisional; once the commit has
/// landed the new document is accepted and the set is empty.
#[test]
fn stage1_the_provisional_set_follows_the_old_to_new_doc_id_replacement_of_a_commit() {
    let writes = Writes::default();
    let (mut h, job) = committing_raw_harness(&writes);
    let old = h.state().session.documents()[0].doc.id;
    assert_eq!(h.state().session.provisional_documents(), vec![old], "the committing document is provisional while its job runs ({job})");
    let mut ids_seen = vec![old];
    for _ in 0..600 {
        h.run_steps(1);
        let held = h.state().session.provisional_documents();
        for running in h.state().session.jobs() {
            if let Some(document) = running.document {
                assert!(held.contains(&document), "a document that a commit job is working on must be provisional: {document:?} not in {held:?}");
            }
        }
        if let Some(st) = h.state().session.documents().first()
            && !ids_seen.contains(&st.doc.id)
        {
            ids_seen.push(st.doc.id);
        }
        if !h.state().session.has_jobs() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    h.run_steps(3);
    let documents = h.state().session.documents();
    assert_eq!(documents.len(), 1);
    assert_ne!(documents[0].doc.id, old, "the document was replaced by the re-developed one");
    assert!(ids_seen.len() >= 2, "the new id was observed: {ids_seen:?}");
    assert!(h.state().session.provisional_documents().is_empty(), "the committed document is accepted: nothing is provisional");
    assert!(!h.state().ui.status.contains("stopped"), "{}", h.state().ui.status);
    finish();
}

/// jobs.cancel from the control channel: Camera Raw's commit is protected (and `{}` skips it and says so); a job of an
/// accepted document is still cancelled.
#[test]
fn stage1_jobs_cancel_skips_a_raw_commit_and_still_cancels_other_jobs() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as A, Mutex};
    let writes = Writes::default();
    let gate = A::new(AtomicBool::new(false));
    let saved: A<Mutex<Vec<String>>> = Default::default();
    let (worker_gate, worker_log) = (gate.clone(), saved.clone());
    let for_app = writes.clone();
    let mut h = Harness::builder().with_step_dt(1.0 / 60.0).with_size(vec2(1360.0, 900.0)).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
        let mut services = services(&for_app);
        services.automation_save = Some(A::new(move |_doc, path: &str, _settings, ctl| {
            while !worker_gate.load(Ordering::SeqCst) {
                if ctl.check().is_err() {
                    return Err("cancelled".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            worker_log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(path.to_string());
            Ok(Vec::new())
        }));
        PhotocraftApp::new(photocraft_engine::Session::new(), services)
    });
    h.run_steps(3);
    assert_eq!(call_h(&mut h, "engine.execute", json!({"command": "file.new", "params": {"width": 8, "height": 8}}))["ok"], true);
    assert_eq!(call_h(&mut h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
    let accepted = h.state().session.active().unwrap().doc.id;
    // A RAW opens (read in the request, so the file is kept and Open can re-develop it as a job).
    h.state_mut().open_bytes("only.dng", &dng()).unwrap();
    h.run_steps(6);
    for _ in 0..6 {
        let prompts: Vec<Value> = control::inspect(h.state(), &h.ctx)["dialogs"].as_array().cloned().unwrap_or_default();
        for prompt in prompts {
            call_h(&mut h, "ui.dialog.cancel", json!({"dialog": prompt["id"]}));
        }
        h.run_steps(1);
    }
    // A background save of the accepted document, started while the RAW waits, and then the RAW's commit.
    h.state_mut().background_jobs = true;
    assert!(h.state_mut().session.set_active(0));
    let started = call_h(&mut h, "app.save", json!({"path": "slow.psd", "wait": false}));
    let save_job = started["result"]["job"].as_u64().unwrap_or_else(|| panic!("the save did not start a job: {started}"));
    assert_eq!(call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"set": {"exposure": 1.0}}}}))["ok"], true);
    let commit = call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"commit": true}}, "wait": false}));
    let commit_job = commit["result"]["job"].as_u64().unwrap_or_else(|| panic!("the commit did not start a job: {commit}"));
    let refused = call_h(&mut h, "jobs.cancel", json!({"job": commit_job}));
    assert!(refused["ok"] == false && refused["error"].as_str().is_some_and(|e| e.contains("Camera Raw's own commit")), "{refused}");
    // The engine command of the same name is another way to the same job: with the accepted document as the active tab
    // the control guard lets it through, so the commit must be protected where the command runs.
    assert!(h.state_mut().session.set_active(0));
    for params in [json!({"job": commit_job}), json!({})] {
        let by_command = call_h(&mut h, "engine.execute", json!({"command": "jobs.cancel", "params": params}));
        if by_command["ok"] != false {
            violation(format!("engine.execute jobs.cancel {params} cancelled or skipped a Camera Raw commit: {by_command}"));
        }
        if h.state().ui.status.contains("Open Images stopped") {
            violation(format!("engine.execute jobs.cancel {params} stopped the commit: {}", h.state().ui.status));
        }
    }
    assert!(h.state().session.job(photocraft_engine::jobs::JobId(commit_job)).is_some(), "the commit job is still running");
    let all = call_h(&mut h, "jobs.cancel", json!({}));
    assert_eq!(all["result"]["skipped"], 1, "the commit was skipped and reported: {all}");
    assert_eq!(all["result"]["cancelled"], 1, "the accepted document's save was cancelled: {all}");
    assert!(h.state().session.job(photocraft_engine::jobs::JobId(commit_job)).is_some() || !h.state().ui.status.contains("stopped"), "the commit goes on");
    let missing = call_h(&mut h, "jobs.cancel", json!({"job": save_job + 1000}));
    assert_eq!(missing["ok"], false, "{missing}");
    gate.store(true, Ordering::SeqCst);
    settle(&mut h);
    assert!(!h.state().ui.status.contains("Open Images stopped"), "the commit was not cancelled: {}", h.state().ui.status);
    let raw = h.state().session.documents().iter().find(|st| st.doc.id != accepted).expect("the re-developed RAW is open");
    assert_eq!(raw.saved_revision, raw.revision, "Open finished: the document is accepted and unmodified");
    assert!(h.state().session.provisional_documents().is_empty());
    finish();
}

/// Positive controls: while RAW photographs wait, the accepted document is edited and saved through the supported
/// automation, and natively, and the photographs stay exactly as they were.
#[test]
fn stage1_an_accepted_document_is_edited_and_saved_while_raws_wait() {
    let writes = Writes::default();
    let (mut app, ctx) = review_app(&writes, true);
    let accepted = add_accepted_document(&mut app, &ctx);
    let waiting: Vec<DocPrint> = print(&app, &ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    let revision = |app: &PhotocraftApp| app.session.documents().iter().find(|st| st.doc.id == accepted).map(|st| st.revision).unwrap();
    let start = revision(&app);
    assert_eq!(engine(&mut app, &ctx, "edit.fill", json!({"color": "#336699"}))["ok"], true, "engine.execute edits the accepted document");
    assert!(revision(&app) > start);
    let after_control = revision(&app);
    assert!(app.run("edit.fill", json!({"color": "#996633"})).is_ok(), "the native route edits it too");
    assert!(revision(&app) > after_control);
    let writes_before = writes.borrow().len();
    assert_eq!(call(&mut app, &ctx, "app.save", json!({"path": "accepted.psd"}))["ok"], true, "app.save writes the accepted document");
    assert_eq!(writes.borrow().len(), writes_before + 1);
    assert!(writes.borrow().last().is_some_and(|w| w.contains("accepted.psd")), "{:?}", writes.borrow());
    let saved = app.session.documents().iter().find(|st| st.doc.id == accepted).and_then(|st| st.path.clone());
    assert_eq!(saved.as_deref(), Some("accepted.psd"), "the accepted document is bound to its file");
    assert_waiting_unchanged(&app, &waiting, "editing and saving the accepted document");
    finish();
}
/// The deferred job-application boundary (`Session::apply_job`), tested directly. An edit job started on the accepted
/// document A is still running when a RAW becomes the active tab (or the review is cancelled and A's tab moves): the
/// result lands on A, found by its `DocId`, and a photograph is not touched. This describes what `apply_job` does; the
/// Stage 1 guards neither change nor extend it.
fn late_edit_job(cancel_the_review: bool) {
    let writes = Writes::default();
    let mut h = app_with_review(&writes, |h| {
        assert_eq!(call_h(h, "engine.execute", json!({"command": "file.new", "params": {"width": 16, "height": 16}}))["ok"], true);
        assert_eq!(call_h(h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
        assert_eq!(call_h(h, "engine.execute", json!({"command": "edit.fill", "params": {"color": "#336699"}}))["ok"], true);
    });
    let accepted = h.state().session.documents().first().map(|st| st.doc.id).unwrap();
    let waiting: Vec<DocPrint> = print(h.state(), &h.ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    assert_eq!(waiting.len(), 3);
    let revision = |h: &Harness<'_, PhotocraftApp>| {
        h.state().session.documents().iter().find(|st| st.doc.id == accepted).map(|st| (st.revision, st.history.past_len())).unwrap()
    };
    let start = revision(&h);
    h.state_mut().background_jobs = true;
    assert!(h.state_mut().session.set_active(0), "the accepted document is the active tab when the job starts");
    let started = call_h(&mut h, "engine.execute", json!({"command": "filter.blur.gaussianBlur", "params": {"radius": 3}, "wait": false}));
    assert_eq!(started["result"]["pending"], true, "the blur runs as a background job: {started}");
    if cancel_the_review {
        let cancelled = call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"cancel": true}}}));
        assert_eq!(cancelled["ok"], true, "{cancelled}");
    } else {
        assert!(h.state_mut().session.set_active(1), "a photograph is the active tab when the job lands");
    }
    settle(&mut h);
    let end = revision(&h);
    assert!(end.0 > start.0 && end.1 == start.1 + 1, "the job landed on the accepted document: {start:?} -> {end:?}");
    if cancel_the_review {
        assert_eq!(h.state().session.documents().len(), 1, "Cancel discarded the photographs");
    } else {
        assert_waiting_unchanged(h.state(), &waiting, "an edit job that landed while a photograph was the active tab");
    }
    finish();
}

#[test]
fn a_background_edit_of_the_accepted_document_lands_on_it_while_a_photograph_is_the_active_tab() {
    late_edit_job(false);
}

#[test]
fn a_background_edit_of_the_accepted_document_lands_on_it_after_cancel_moved_its_tab() {
    late_edit_job(true);
}

/// Native route, a dialog that writes without going through a command: the Export As dialog opened on the accepted
/// document before the RAWs arrived exports whichever tab is active when it is confirmed. The file dialog answers at
/// once (the real app has one; without it the test would stop at "cancelled" and prove nothing).
#[test]
fn stage1_native_enter_on_an_export_dialog_opened_before_the_review_cannot_export_the_raw() {
    let writes = Writes::default();
    let mut h = app_with_review_using(
        &writes,
        |services| {
            services.file_dialog = Some(Box::new(|_request, _parent, reply| {
                reply.send(Some(photocraft_ui_egui::file_dialog::FileDialogAnswer::SaveTo("exported.png".into())));
            }));
        },
        |h| {
            assert_eq!(call_h(h, "engine.execute", json!({"command": "file.new", "params": {"width": 16, "height": 16}}))["ok"], true);
            assert_eq!(call_h(h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
            let opened = call_h(h, "ui.menu.invoke", json!({"id": "file.export.exportAs", "params": {}}));
            assert_eq!(opened["ok"], true, "{opened}");
            h.run_steps(3);
        },
    );
    let open_dialogs = |h: &Harness<'_, PhotocraftApp>| {
        control::inspect(h.state(), &h.ctx)["dialogs"].as_array().map_or(0, |d| d.iter().filter(|d| d["fields"].get("__export").is_some()).count())
    };
    eprintln!("Export As dialogs still open over the review: {}", open_dialogs(&h));
    let before = print(h.state(), &h.ctx, &writes);
    h.event(key_enter());
    h.run_steps(6);
    let mut after = print(h.state(), &h.ctx, &writes);
    after.dialogs = before.dialogs;
    eprintln!("writes {:?} -> {:?}", before.writes, after.writes);
    assert_same(&before, &after, "a native Enter on an Export As dialog opened before the review");
    finish();
}
/// Native route through a UI-level command: the palette opened before the RAWs arrived runs "Save As" on the active
/// tab, and a file dialog that answers at once saves the waiting photograph under that name and binds it to the file.
/// `file.saveAs` is a shell command, so the command guard in `PhotocraftApp::run` never sees it.
#[test]
fn stage1_native_palette_save_as_cannot_save_or_bind_the_waiting_raw() {
    let writes = Writes::default();
    let mut h = app_with_review_using(
        &writes,
        |services| {
            services.file_dialog = Some(Box::new(|_request, _parent, reply| {
                reply.send(Some(photocraft_ui_egui::file_dialog::FileDialogAnswer::SaveTo("from-the-palette.png".into())));
            }));
        },
        |h| {
            assert_eq!(call_h(h, "engine.execute", json!({"command": "edit.search", "params": {}}))["ok"], true);
            h.run_steps(3);
            h.event(egui::Event::Text("save as".into()));
            h.run_steps(2);
        },
    );
    assert!(!h.state().ui.palette_open, "the palette is closed when the RAWs land");
    let before = print(h.state(), &h.ctx, &writes);
    h.event(key_enter());
    h.run_steps(6);
    let after = print(h.state(), &h.ctx, &writes);
    eprintln!("palette Save As: writes {:?} -> {:?}; status {:?}", before.writes, after.writes, h.state().ui.status);
    let mut compared = after;
    compared.dialogs = before.dialogs;
    assert_same(&before, &compared, "a native palette Save As over the review");
    finish();
}
/// The app-level writers, called directly (what Save As, a file dialog answered later and a recorded save reach): the
/// waiting photograph is refused before anything is encoded or written; the accepted document is saved normally. And a
/// Save As that was started on the accepted document and answered after a RAW landed still saves that document.
#[test]
fn stage1_the_active_waiting_raw_is_never_written_and_a_late_save_dialog_keeps_its_document() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let writes = Writes::default();
    let reply: Rc<RefCell<Option<photocraft_ui_egui::file_dialog::FileDialogReply>>> = Rc::default();
    let for_dialog = reply.clone();
    let mut h = app_with_review_using(
        &writes,
        move |services| {
            services.file_dialog = Some(Box::new(move |_request, _parent, answer| {
                *for_dialog.borrow_mut() = Some(answer);
            }));
        },
        |h| {
            assert_eq!(call_h(h, "engine.execute", json!({"command": "file.new", "params": {"width": 16, "height": 16}}))["ok"], true);
            assert_eq!(call_h(h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
        },
    );
    let accepted = h.state().session.documents().first().map(|st| st.doc.id).unwrap();
    let waiting: Vec<DocPrint> = print(h.state(), &h.ctx, &writes).docs.into_iter().filter(|d| d.id != accepted).collect();
    assert_eq!(waiting.len(), 3);
    // A photograph is the active tab: neither writer touches it, and nothing is encoded or written.
    assert!(!h.state().session.active().is_some_and(|st| st.doc.id == accepted));
    let before = print(h.state(), &h.ctx, &writes);
    for result in [
        h.state_mut().save_as_with(Some("direct.png".into()), Default::default()),
        h.state_mut().save_automation_with(Some("direct.psd".into()), Default::default()),
    ] {
        assert!(is_unaccepted_refusal(&result), "{result:?}");
    }
    assert_same(&before, &print(h.state(), &h.ctx, &writes), "the app-level writers called on a waiting RAW");
    // The accepted document is the active tab: both writers work.
    assert!(h.state_mut().session.set_active(0));
    assert!(h.state_mut().save_as_with(Some("kept.psd".into()), Default::default()).is_ok(), "the accepted document is saved");
    // A Save As asked on the accepted document, answered after a photograph has become the active tab.
    let asked = h.state_mut().save_as_with(None, Default::default());
    assert!(asked.is_ok(), "the Save As dialog opens for the accepted document: {asked:?}");
    h.run_steps(2);
    assert!(h.state_mut().session.set_active(1), "a photograph is the active tab when the answer arrives");
    reply.borrow_mut().take().expect("the dialog was asked").send(Some(photocraft_ui_egui::file_dialog::FileDialogAnswer::SaveTo("late.psd".into())));
    h.run_steps(4);
    let owner = h.state().session.documents().iter().find(|st| st.doc.id == accepted).map(|st| st.path.clone());
    assert_eq!(owner, Some(Some("late.psd".to_string())), "the late answer saved the document the dialog was asked for");
    assert!(writes.borrow().iter().any(|w| w.contains("late.psd")), "{:?}", writes.borrow());
    assert_waiting_unchanged(h.state(), &waiting, "a Save As answered while a photograph was the active tab");
    finish();
}

/// A user save that runs in the background, with a gate on its worker, so the test decides when it finishes.
fn harness_with_a_gated_user_save(
    writes: &Writes,
) -> (Harness<'static, PhotocraftApp>, std::sync::Arc<std::sync::atomic::AtomicBool>, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as A, Mutex};
    let gate = A::new(AtomicBool::new(false));
    let saved: A<Mutex<Vec<String>>> = Default::default();
    let (worker_gate, worker_log) = (gate.clone(), saved.clone());
    let for_app = writes.clone();
    let mut h = Harness::builder().with_step_dt(1.0 / 60.0).with_size(vec2(1360.0, 900.0)).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
        let mut services = services(&for_app);
        services.save_file = Some(A::new(move |_doc, path: &str, _settings, _ctl| {
            while !worker_gate.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            worker_log.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(path.to_string());
            Ok(Vec::new())
        }));
        PhotocraftApp::new(photocraft_engine::Session::new(), services)
    });
    h.run_steps(3);
    assert_eq!(call_h(&mut h, "engine.execute", json!({"command": "file.new", "params": {"width": 8, "height": 8}}))["ok"], true);
    assert_eq!(call_h(&mut h, "engine.execute", json!({"command": "layer.new.layer", "params": {}}))["ok"], true);
    (h, gate, saved)
}

/// A hook on the save of an accepted document that activates the second tab and edits it.
fn retargeting_save_hook() -> Value {
    json!([["document.activate", {"document": 1}], ["edit.fill", {"color": "#ff0000"}]])
}

/// Two jobs land in one `poll_jobs`, the save first. Every job is applied before any event is handled, so while the
/// save's handler runs the hook, the RAW that the second job opened is already a document and not yet declared. The
/// decode in flight is what keeps Script Events paused.
#[test]
fn stage1_a_save_hook_in_the_same_poll_as_a_raw_decode_cannot_edit_the_raw() {
    let (_scratch, path) = scratch_dng("samepoll");
    let writes = Writes::default();
    let (mut h, gate, saved) = harness_with_a_gated_user_save(&writes);
    h.state_mut().background_jobs = true;
    bind_hook(h.state_mut(), "saveDocument", retargeting_save_hook());
    let save = call_h(&mut h, "engine.execute", json!({"command": "file.saveAs", "params": {"path": "a.psd"}, "wait": false}));
    assert_eq!(save["result"]["pending"], true, "{save}");
    let open = call_h(&mut h, "engine.execute", json!({"command": "file.open", "params": {"path": path.to_string_lossy()}}));
    assert_eq!(open["ok"], true, "{open}");
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(700));
    h.step();
    assert_eq!(*saved.lock().unwrap_or_else(std::sync::PoisonError::into_inner), vec!["a.psd".to_string()], "the save finished");
    assert_eq!(h.state().session.documents().len(), 2, "both jobs landed in the one poll (else this test proves nothing)");
    settle(&mut h);
    let raw = &h.state().session.documents()[1];
    if raw.revision != 1 || raw.history.past_len() != 0 {
        violation(format!("the save hook edited the RAW that landed in the same poll: revision {}, history {}", raw.revision, raw.history.past_len()));
    }
    finish();
}

/// S2-2: the importer decides by content, not by name. A RAW whose file name does not look like one decodes as a
/// provisional photograph all the same, so a save hook handled in the same poll must not edit it.
#[test]
fn p0_s2_2_a_raw_with_a_non_raw_name_is_protected_in_the_same_poll() {
    let scratch = Scratch(std::env::temp_dir().join("opencode").join(format!("p0b-s22-{}", std::process::id())));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let path = scratch.0.join("scan.dat");
    std::fs::write(&path, dng()).unwrap();
    let writes = Writes::default();
    let (mut h, gate, saved) = harness_with_a_gated_user_save(&writes);
    h.state_mut().background_jobs = true;
    bind_hook(h.state_mut(), "saveDocument", retargeting_save_hook());
    let save = call_h(&mut h, "engine.execute", json!({"command": "file.saveAs", "params": {"path": "a.psd"}, "wait": false}));
    assert_eq!(save["result"]["pending"], true, "{save}");
    let open = call_h(&mut h, "engine.execute", json!({"command": "file.open", "params": {"path": path.to_string_lossy()}}));
    assert_eq!(open["ok"], true, "{open}");
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(700));
    h.step();
    assert_eq!(*saved.lock().unwrap_or_else(std::sync::PoisonError::into_inner), vec!["a.psd".to_string()], "the save finished");
    assert_eq!(h.state().session.documents().len(), 2, "both jobs landed in the one poll (else this test proves nothing)");
    settle(&mut h);
    let raw = &h.state().session.documents()[1];
    if raw.revision != 1 || raw.history.past_len() != 0 {
        violation(format!("the save hook edited a RAW that decoded under a non-RAW name: revision {}, history {}", raw.revision, raw.history.past_len()));
    }
    finish();
}
/// An app whose automation reader serves `files` (name, bytes) for any path ending in that name.
fn app_reading(writes: &Writes, files: &[(&str, Vec<u8>)]) -> (PhotocraftApp, egui::Context) {
    let files: Vec<(String, Vec<u8>)> = files.iter().map(|(name, bytes)| (name.to_string(), bytes.clone())).collect();
    new_app_with(writes, move |services| {
        services.automation_read = Some(Box::new(move |path: &str| {
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            files.iter().find(|(n, _)| *n == name).map(|(n, b)| (n.clone(), b.clone())).ok_or_else(|| format!("no such file: {path}"))
        }));
    })
}

/// A small flat document encoded as `name` (a PSD or a JPEG).
fn flat_file(name: &str) -> Vec<u8> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 12, "height": 9})).unwrap();
    let doc = (*s.active().unwrap().doc).clone();
    photocraft_io::export(&doc, name, &Default::default()).unwrap().bytes
}

/// The photographs waiting in Camera Raw, by document id, as an agent sees them.
fn waiting_ids(app: &mut PhotocraftApp, ctx: &egui::Context) -> Vec<u64> {
    let state = call(app, ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {}}}));
    let mut ids: Vec<u64> = state["result"]["filmstrip"].as_array().map(|f| f.iter().filter_map(|p| p["document"].as_u64()).collect()).unwrap_or_default();
    ids.sort_unstable();
    ids
}

fn cancel_review_via_control(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let cancelled = call(app, ctx, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"cancel": true}}}));
    assert_eq!(cancelled["ok"], true, "{cancelled}");
    frames(app, ctx, 3);
}

/// P0D: a desktop `app.open` of a RAW enters Camera Raw like File > Open: provisional at once, in the same call.
#[test]
fn p0d_app_open_of_a_raw_enters_provisional_review() {
    let writes = Writes::default();
    let (mut app, ctx) = app_reading(&writes, &[("shot.dng", dng())]);
    let reply = call(&mut app, &ctx, "app.open", json!({"path": "shot.dng"}));
    assert_eq!(reply["ok"], true, "{reply}");
    let id = app.session.active().unwrap().doc.id;
    assert!(app.session.provisional_documents().contains(&id), "held before any other request can see it");
    frames(&mut app, &ctx, 3);
    assert_eq!(waiting_ids(&mut app, &ctx), vec![id.0], "its review is open");
}

/// P0D: nothing edits, saves or admits an `app.open`ed RAW before Open.
#[test]
fn p0d_an_app_opened_raw_cannot_be_edited_or_saved() {
    let writes = Writes::default();
    let (mut app, ctx) = app_reading(&writes, &[("shot.dng", dng())]);
    assert_eq!(call(&mut app, &ctx, "app.open", json!({"path": "shot.dng"}))["ok"], true);
    frames(&mut app, &ctx, 3);
    let (revision, past) = {
        let st = app.session.active().unwrap();
        (st.revision, st.history.past_len())
    };
    let writes_before = writes.borrow().len();
    for (what, reply) in [
        ("edit.fill", engine(&mut app, &ctx, "edit.fill", json!({"color": "#ff0000"}))),
        ("layer.new.layer", engine(&mut app, &ctx, "layer.new.layer", json!({}))),
        ("app.save", call(&mut app, &ctx, "app.save", json!({"path": "out.psd"}))),
    ] {
        assert_eq!(reply["ok"], false, "{what} must be refused for an unaccepted RAW: {reply}");
    }
    let st = app.session.active().unwrap();
    assert_eq!((st.revision, st.history.past_len(), writes.borrow().len()), (revision, past, writes_before), "nothing changed, nothing was written");
}

/// P0D: Cancel removes exactly the unaccepted `app.open`ed RAWs; an accepted tab is untouched.
#[test]
fn p0d_cancel_discards_exactly_the_app_opened_raws() {
    let writes = Writes::default();
    let (mut app, ctx) = app_reading(&writes, &[("a.dng", dng()), ("b.dng", dng())]);
    let accepted = add_accepted_document(&mut app, &ctx);
    for name in ["a.dng", "b.dng"] {
        assert_eq!(call(&mut app, &ctx, "app.open", json!({"path": name}))["ok"], true);
    }
    frames(&mut app, &ctx, 3);
    assert_eq!(waiting_ids(&mut app, &ctx).len(), 2, "both are waiting");
    cancel_review_via_control(&mut app, &ctx);
    let left: Vec<DocId> = app.session.documents().iter().map(|st| st.doc.id).collect();
    assert_eq!(left, vec![accepted], "only the accepted document remains");
    assert!(!app.session.has_provisional());
}

/// P0D: a RAW whose name does not look like one is held the same way (the importer decides).
#[test]
fn p0d_app_open_of_a_renamed_raw_is_held() {
    let writes = Writes::default();
    let (mut app, ctx) = app_reading(&writes, &[("scan.dat", dng())]);
    assert_eq!(call(&mut app, &ctx, "app.open", json!({"path": "scan.dat"}))["ok"], true);
    let id = app.session.active().unwrap().doc.id;
    assert!(app.session.provisional_documents().contains(&id), "scan.dat is a RAW to the importer");
}

/// P0D: PSD and JPEG through `app.open` stay ordinary while a review is open, and survive its Cancel.
#[test]
fn p0d_app_open_of_a_non_raw_stays_ordinary() {
    let writes = Writes::default();
    let (mut app, ctx) = app_reading(&writes, &[("review.dng", dng()), ("doc.psd", flat_file("x.psd")), ("photo.jpg", flat_file("x.jpg"))]);
    assert_eq!(call(&mut app, &ctx, "app.open", json!({"path": "review.dng"}))["ok"], true);
    frames(&mut app, &ctx, 3);
    for name in ["doc.psd", "photo.jpg"] {
        assert_eq!(call(&mut app, &ctx, "app.open", json!({"path": name}))["ok"], true);
    }
    let ordinary: Vec<DocId> = app.session.documents().iter().filter(|st| st.doc.name != "review.dng").map(|st| st.doc.id).collect();
    assert_eq!(ordinary.len(), 2);
    assert!(ordinary.iter().all(|id| !app.session.provisional_documents().contains(id)), "a PSD and a JPEG are never held");
    cancel_review_via_control(&mut app, &ctx);
    let left: Vec<DocId> = app.session.documents().iter().map(|st| st.doc.id).collect();
    assert_eq!(left, ordinary, "Cancel removed only the RAW");
}

/// P0D: with Camera Raw off for opens, `app.open` of a RAW is an ordinary document, as File > Open.
#[test]
fn p0d_app_open_with_camera_raw_off_is_ordinary() {
    let writes = Writes::default();
    let (mut app, ctx) = app_reading(&writes, &[("shot.dng", dng())]);
    app.session.edit_prefs(|prefs| prefs.raw_defaults.open_in_camera_raw = false);
    assert_eq!(call(&mut app, &ctx, "app.open", json!({"path": "shot.dng"}))["ok"], true);
    assert!(!app.session.has_provisional());
    assert_eq!(engine(&mut app, &ctx, "layer.new.layer", json!({}))["ok"], true, "an ordinary document");
}

fn raw_on_disk(dir: &Scratch, name: &str, bytes: &[u8]) -> String {
    let path = dir.0.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
}

fn scratch(tag: &str) -> Scratch {
    let dir = Scratch(std::env::temp_dir().join("opencode").join(format!("p0d-{tag}-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    dir
}

/// P0D: `file.openAs {path}` through `engine.execute` and `ui.menu.invoke` opens a RAW through Camera Raw.
#[test]
fn p0d_file_open_as_with_a_path_enters_review_by_every_desktop_route() {
    let dir = scratch("openas");
    let path = raw_on_disk(&dir, "shot.dng", &dng());
    for (method, params) in [
        ("engine.execute", json!({"command": "file.openAs", "params": {"path": path}})),
        ("ui.menu.invoke", json!({"id": "file.openAs", "params": {"path": path}})),
    ] {
        let writes = Writes::default();
        let (mut app, ctx) = new_app(&writes);
        let reply = call(&mut app, &ctx, method, params);
        assert_eq!(reply["ok"], true, "{method}: {reply}");
        frames(&mut app, &ctx, 3);
        cancel_prompts(&mut app, &ctx);
        let id = app.session.documents().last().map(|st| st.doc.id).expect("a document opened");
        assert!(app.session.provisional_documents().contains(&id), "{method}: the RAW is held");
        assert_eq!(waiting_ids(&mut app, &ctx), vec![id.0], "{method}: its review is open");
        assert_eq!(engine(&mut app, &ctx, "edit.fill", json!({"color": "#ff0000"}))["ok"], false, "{method}: not editable before Open");
    }
}

/// P0D: the same with background jobs on (the desktop app): the decode lands later and is still reviewed.
#[test]
fn p0d_file_open_as_with_a_path_enters_review_in_the_background() {
    let dir = scratch("openas-bg");
    let path = raw_on_disk(&dir, "scan.dat", &dng());
    let writes = Writes::default();
    let mut h = plain_harness(&writes);
    h.state_mut().background_jobs = true;
    let reply = call_h(&mut h, "engine.execute", json!({"command": "file.openAs", "params": {"path": path}}));
    assert_eq!(reply["ok"], true, "{reply}");
    // Within the request itself, before any frame or job runs: the RAW exists and is already held.
    let id = h.state().session.documents().last().map(|st| st.doc.id).expect("opened within the request");
    assert!(h.state().session.provisional_documents().contains(&id), "held when the reply is delivered");
    settle(&mut h);
    assert!(h.state().session.provisional_documents().contains(&id), "a RAW named scan.dat stays held");
}

/// P0D: a decoder override cannot admit a developed RAW: the open is refused and nothing is added.
#[test]
fn p0d_file_open_as_with_an_override_refuses_a_developed_raw() {
    let dir = scratch("openas-as");
    let path = raw_on_disk(&dir, "shot.dng", &dng());
    for (method, params) in [
        ("engine.execute", json!({"command": "file.openAs", "params": {"path": path, "as": "dng"}})),
        ("ui.menu.invoke", json!({"id": "file.openAs", "params": {"path": path, "as": "dng"}})),
    ] {
        let writes = Writes::default();
        let (mut app, ctx) = new_app(&writes);
        let reply = call(&mut app, &ctx, method, params);
        assert_eq!(reply["ok"], false, "{method}: {reply}");
        assert!(app.session.documents().is_empty(), "{method}: no document was admitted");
        assert!(!app.session.has_provisional());
    }
}

/// P0D: non-RAW formats open as before, with or without an override, and with Camera Raw off a RAW is ordinary.
#[test]
fn p0d_file_open_as_keeps_ordinary_formats_and_the_preference() {
    let dir = scratch("openas-plain");
    let jpg = raw_on_disk(&dir, "photo.jpg", &flat_file("x.jpg"));
    let psd = raw_on_disk(&dir, "doc.psd", &flat_file("x.psd"));
    let raw = raw_on_disk(&dir, "shot.dng", &dng());
    let writes = Writes::default();
    let (mut app, ctx) = new_app(&writes);
    for params in [json!({"path": jpg}), json!({"path": psd}), json!({"path": jpg, "as": "jpg"})] {
        let reply = engine(&mut app, &ctx, "file.openAs", params.clone());
        assert_eq!(reply["ok"], true, "{params}: {reply}");
    }
    frames(&mut app, &ctx, 3);
    assert_eq!(app.session.documents().len(), 3);
    assert!(!app.session.has_provisional(), "ordinary formats are never held");
    app.session.edit_prefs(|prefs| prefs.raw_defaults.open_in_camera_raw = false);
    assert_eq!(engine(&mut app, &ctx, "file.openAs", json!({"path": raw}))["ok"], true);
    assert!(!app.session.has_provisional(), "Camera Raw is off for opens: an ordinary document");
}

/// P0D: an Open Document hook cannot run on a RAW opened by `file.openAs {path}`: the open happens within the request
/// (also when background jobs are on), the RAW is held at once, and the hook leaves it untouched.
#[test]
fn p0d_file_open_as_cannot_run_an_open_document_hook_on_a_raw() {
    let dir = scratch("openas-hook-bound");
    let path = raw_on_disk(&dir, "hooked.dng", &dng());
    let steps = json!([["layer.new.layer", {"name": "From the hook"}], ["edit.fill", {"color": "#ff0000"}]]);
    for background in [true, false] {
        let writes = Writes::default();
        let mut h = plain_harness(&writes);
        bind_hook(h.state_mut(), "openDocument", steps.clone());
        h.state_mut().background_jobs = background;
        let reply = call_h(&mut h, "engine.execute", json!({"command": "file.openAs", "params": {"path": path}}));
        assert_eq!(reply["ok"], true, "background {background}: {reply}");
        settle(&mut h);
        assert_eq!(h.state().session.documents().len(), 1, "background {background}");
        for st in h.state().session.documents() {
            assert!(st.revision == 1 && st.history.past_len() == 0, "background {background}: the hook edited a RAW before Camera Raw decided it");
            assert!(h.state().session.provisional_documents().contains(&st.doc.id), "background {background}: the RAW is held");
        }
    }
}
fn result_of(reply: &Value) -> &Value {
    &reply["result"]
}

/// P0E R01: for an ordinary file `file.openAs {path}` keeps the engine's reply (`document`, `color`, `warnings`) and adds
/// the stable `docId`; nothing says it is in review.
#[test]
fn p0e_file_open_as_keeps_the_engine_reply_for_ordinary_files() {
    let dir = scratch("p0e-plain");
    let jpg = raw_on_disk(&dir, "photo.jpg", &flat_file("x.jpg"));
    let psd = raw_on_disk(&dir, "doc.psd", &flat_file("x.psd"));
    for (method, key) in [("engine.execute", "command"), ("ui.menu.invoke", "id")] {
        for path in [&jpg, &psd] {
            let writes = Writes::default();
            let (mut app, ctx) = new_app(&writes);
            let reply = call(&mut app, &ctx, method, json!({key: "file.openAs", "params": {"path": path}}));
            assert_eq!(reply["ok"], true, "{method}: {reply}");
            let result = result_of(&reply);
            let index = result["document"].as_u64().unwrap_or_else(|| panic!("{method}: no `document`: {reply}")) as usize;
            assert!(result["color"].is_object() && result["warnings"].is_array(), "{method}: `color` and `warnings` kept: {reply}");
            let doc = app.session.documents().get(index).map(|st| st.doc.id.0);
            assert_eq!(result["docId"].as_u64(), doc, "{method}: `docId` names the document at `document`: {reply}");
            assert!(result.get("review").is_none(), "{method}: an ordinary file is not in review: {reply}");
            assert!(!app.session.has_provisional());
        }
    }
}

/// P0E R01: a RAW answers with the same fields plus `review: "pending"` and its stable `docId`, and is held when the
/// reply is delivered: not editable, not accepted.
#[test]
fn p0e_file_open_as_reports_a_raw_as_pending_review() {
    let dir = scratch("p0e-raw");
    let path = raw_on_disk(&dir, "scan.dat", &dng());
    for (method, key) in [("engine.execute", "command"), ("ui.menu.invoke", "id")] {
        let writes = Writes::default();
        let (mut app, ctx) = new_app(&writes);
        let reply = call(&mut app, &ctx, method, json!({key: "file.openAs", "params": {"path": path}}));
        assert_eq!(reply["ok"], true, "{method}: {reply}");
        let result = result_of(&reply);
        assert_eq!(result["review"], "pending", "{method}: {reply}");
        let id = result["docId"].as_u64().unwrap_or_else(|| panic!("{method}: no docId: {reply}"));
        assert!(result["color"].is_object() && result["warnings"].is_array() && result["document"].is_u64(), "{method}: {reply}");
        assert!(app.session.provisional_documents().iter().any(|held| held.0 == id), "{method}: held before the reply returns");
        frames(&mut app, &ctx, 3);
        cancel_prompts(&mut app, &ctx);
        assert_eq!(waiting_ids(&mut app, &ctx), vec![id], "{method}: its review is open");
        assert_eq!(engine(&mut app, &ctx, "edit.fill", json!({"color": "#ff0000"}))["ok"], false, "{method}: not editable before Open");
        cancel_review_via_control(&mut app, &ctx);
        assert!(app.session.documents().is_empty() && !app.session.has_provisional(), "{method}: Cancel removes it");
    }
}

/// P0E R01: ownership is atomic. Every observer an open could reach (an Open Document Script Event, the job beep) is
/// bound, and the RAW is still held and untouched when the call returns.
#[test]
fn p0e_a_raw_opened_by_file_open_as_is_held_before_any_observer_can_run() {
    let dir = scratch("p0e-observer");
    let path = raw_on_disk(&dir, "hooked.dng", &dng());
    for background in [false, true] {
        let writes = Writes::default();
        let mut h = plain_harness(&writes);
        bind_hook(h.state_mut(), "openDocument", json!([["layer.new.layer", {"name": "From the hook"}], ["edit.fill", {"color": "#ff0000"}]]));
        h.state_mut().session.edit_prefs(|prefs| prefs.general.beep_when_done = true);
        h.state_mut().background_jobs = background;
        let reply = call_h(&mut h, "engine.execute", json!({"command": "file.openAs", "params": {"path": path}}));
        assert_eq!(reply["ok"], true, "{reply}");
        let st = h.state().session.documents().first().expect("opened within the call");
        assert!(h.state().session.provisional_documents().contains(&st.doc.id), "background {background}: held at once");
        assert!(st.revision == 1 && st.history.past_len() == 0, "background {background}: untouched at return");
        settle(&mut h);
        let st = &h.state().session.documents()[0];
        assert!(st.revision == 1 && st.history.past_len() == 0 && st.doc.layer_count() == 1, "background {background}: no hook ran on the RAW");
    }
}

/// P0E R02 (characterisation): while a RAW is held, scripting and automation commands are refused on both the control
/// and the native route, nothing changes, and the accepted document keeps working.
#[test]
fn p0e_scripts_and_actions_are_refused_while_a_raw_is_held() {
    let dir = scratch("p0e-scripts");
    let raw = raw_on_disk(&dir, "shot.dng", &dng());
    let writes = Writes::default();
    let (mut app, ctx) = new_app(&writes);
    let accepted = add_accepted_document(&mut app, &ctx);
    open_review(&mut app, &ctx, ["one.dng", "two.dng", "three.dng"], true);
    // The accepted document is the active tab, so only the scripting rule (not "acts on the active RAW") can refuse these.
    let accepted_tab = app.session.documents().iter().position(|st| st.doc.id == accepted).unwrap();
    assert!(app.session.set_active(accepted_tab));
    let before: Vec<(DocId, u64, usize)> = app.session.documents().iter().map(|st| (st.doc.id, st.revision, st.history.past_len())).collect();
    let commands = [
        ("actions.play", json!({"action": "Anything"})),
        ("plugin.run", json!({})),
        ("file.scripts.loadFilesIntoStack", json!({"paths": [raw]})),
        ("file.scripts.imageProcessor", json!({})),
        ("file.automate.batch", json!({})),
        ("file.automate.fitImage", json!({})),
        ("file.automate.createDroplet", json!({})),
    ];
    for (id, params) in commands {
        for (method, key) in [("engine.execute", "command"), ("ui.menu.invoke", "id")] {
            let reply = call(&mut app, &ctx, method, json!({key: id, "params": params}));
            assert_eq!(reply["ok"], false, "{method} {id} must be refused while a RAW is held: {reply}");
        }
        let native = app.run(id, params.clone());
        assert!(native.as_ref().is_err_and(|e| e.contains("refused")), "native {id} must be refused: {native:?}");
    }
    let after: Vec<(DocId, u64, usize)> = app.session.documents().iter().map(|st| (st.doc.id, st.revision, st.history.past_len())).collect();
    assert_eq!(before, after, "no document was added or changed");
    // The accepted document is still edited and saved, and a new one is made, as policy allows.
    assert_eq!(engine(&mut app, &ctx, "edit.fill", json!({"color": "#00ff00"}))["ok"], true, "an accepted document stays editable");
    assert_eq!(engine(&mut app, &ctx, "file.new", json!({"width": 8, "height": 8}))["ok"], true);
}
/// P0D: the authorization hook still judges `file.openAs` before anything opens.
#[test]
fn p0d_file_open_as_still_meets_the_authorization_hook() {
    let dir = scratch("openas-hook");
    let path = raw_on_disk(&dir, "shot.dng", &dng());
    let writes = Writes::default();
    let (mut app, ctx) = new_app_with(&writes, |services| {
        services.automation_command = Some(Box::new(|_id, _params| Err("outside the granted roots".into())));
    });
    let reply = engine(&mut app, &ctx, "file.openAs", json!({"path": path}));
    assert_eq!(reply["ok"], false, "{reply}");
    assert!(app.session.documents().is_empty());
}
/// The same window for the Open commit: its job replaces the RAW under a new id inside `poll_jobs`, and the save
/// event that precedes it in the poll would have wiped the declaration of the new id.
#[test]
fn stage1_a_save_hook_in_the_same_poll_as_a_raw_commit_cannot_edit_the_developed_raw() {
    let writes = Writes::default();
    let (mut h, gate, saved) = harness_with_a_gated_user_save(&writes);
    h.state_mut().open_bytes("only.dng", &dng()).unwrap();
    h.run_steps(6);
    for _ in 0..6 {
        let prompts: Vec<Value> = control::inspect(h.state(), &h.ctx)["dialogs"].as_array().cloned().unwrap_or_default();
        for prompt in prompts {
            call_h(&mut h, "ui.dialog.cancel", json!({"dialog": prompt["id"]}));
        }
        h.run_steps(1);
    }
    h.state_mut().background_jobs = true;
    bind_hook(h.state_mut(), "saveDocument", retargeting_save_hook());
    assert!(h.state_mut().session.set_active(0), "the accepted document is the active tab");
    let save = call_h(&mut h, "engine.execute", json!({"command": "file.saveAs", "params": {"path": "a.psd"}, "wait": false}));
    assert_eq!(save["result"]["pending"], true, "{save}");
    assert_eq!(call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"set": {"exposure": 1.0}}}}))["ok"], true);
    let commit = call_h(&mut h, "ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"commit": true}}, "wait": false}));
    assert!(commit["result"]["job"].is_u64(), "the commit started a job: {commit}");
    let old = h.state().session.documents()[1].doc.id;
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(900));
    h.step();
    assert_eq!(*saved.lock().unwrap_or_else(std::sync::PoisonError::into_inner), vec!["a.psd".to_string()], "the save finished");
    assert_ne!(h.state().session.documents()[1].doc.id, old, "the commit replaced the RAW in the same poll (else this test proves nothing)");
    settle(&mut h);
    let raw = &h.state().session.documents()[1];
    if raw.history.past_len() != 0 {
        violation(format!("the save hook edited the RAW that the commit replaced in the same poll: history {}", raw.history.past_len()));
    }
    finish();
}
