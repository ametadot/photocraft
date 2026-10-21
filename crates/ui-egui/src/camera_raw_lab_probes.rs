//! GRW-1 lab cycle 1 (P-0) probes: the same bytes run on pristine main 0.9.0 (RED) and on the
//! safety-ported lab (GREEN). They assert the SAFE behaviour, so on main a failure is the defect
//! (or a missing R2b policy, labelled as such) and on the port a failure is a regression.
//!
//! Only crate APIs present on both trees are used. A failing "CONTROL" or "PROBE INVALID"
//! assertion means the set-up itself is wrong, not the behaviour under test. Synthetic in-memory
//! DNGs and documents only: no photograph, sidecar or other file is read or written.

use super::*;
use photocraft_engine::jobs::JobId;

/// The real importer opening a synthetic DNG (the same as the module's own `raw_app`).
fn lab_app() -> (PhotocraftApp, Vec<u8>) {
    use photocraft_raw::testgen::{DngSpec, mosaic, scene};
    let services = crate::Services {
        import: Some(Box::new(|name: &str, b: &[u8], depth: usize| {
            photocraft_io::import_with_svg_group_depth(name, b, depth).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
        })),
        ..Default::default()
    };
    let (w, h) = (48, 32);
    let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
    spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
    (PhotocraftApp::new(photocraft_engine::Session::new(), services), spec.build())
}

fn ids(app: &PhotocraftApp) -> Vec<photocraft_doc::DocId> {
    app.session.documents().iter().map(|st| st.doc.id).collect()
}

fn names(app: &PhotocraftApp) -> Vec<String> {
    app.session.documents().iter().map(|st| st.doc.name.clone()).collect()
}

fn control(app: &mut PhotocraftApp, ctx: &egui::Context, method: &str, params: Value) -> Value {
    let (request, _reply) = crate::control::ControlRequest::new(method, params);
    match crate::control::handle(app, ctx, &request) {
        crate::control::Outcome::Done(value) => value,
        _ => json!({"ok": false, "error": "the request did not complete synchronously"}),
    }
}

/// P0-A1, production path (adapted from R2b `stage2a_save_close_and_open`). A has unsaved work and its close prompt is
/// answered Save, so A is written in the background (gated). The RAW is under review; B, to its right, is an accepted
/// document with unsaved work. Open with Exposure starts the re-develop. With `same_poll` the save and the re-develop are
/// applied in one poll, save first, and both events are handled in one tick.
/// Returns (B still has unsaved work, sanity: A closed and the RAW replaced, documents left).
fn save_close_and_open(same_poll: bool) -> (bool, bool, usize) {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (mut app, dng) = lab_app();
    let ctx = egui::Context::default();
    app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
    app.run("layer.new.layer", json!({})).unwrap();
    let a = app.session.documents()[0].doc.id;
    app.open_bytes("held.dng", &dng).unwrap();
    let old = app.pending_raw_open.as_ref().map(|raw| raw.document).expect("PROBE INVALID: the RAW waits for review");
    app.run("file.new", json!({"width": 12, "height": 12})).unwrap();
    app.session.execute("layer.new.layer", json!({})).unwrap();
    let b = app.session.documents()[2].doc.id;
    assert!(app.session.set_active(0), "PROBE INVALID: A is tab 0");
    assert!(crate::discard_ui::intercept(&mut app, "file.close", &json!({"document": 0})), "PROBE INVALID: closing A asks first");
    let gate = std::sync::Arc::new(AtomicBool::new(false));
    let worker_gate = gate.clone();
    app.services.save_file = Some(std::sync::Arc::new(move |_, _, _, ctl| {
        while !worker_gate.load(Ordering::SeqCst) {
            ctl.check().map_err(|e| e.to_string())?;
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(Vec::new())
    }));
    let save = app.services.save_file.clone().expect("PROBE INVALID: the save function was just set");
    let started = crate::jobs_ui::start_save(&mut app, "/lab/a.psd".to_string(), crate::ExportSettings::default(), false, save, false).unwrap();
    assert!(started.is_none(), "PROBE INVALID: the save runs in the background");
    let save_job = app.jobs.saves[0].job;
    let raw = app.pending_raw_open.take().expect("PROBE INVALID: the review opens");
    open_raw(&mut app, &ctx, raw).unwrap();
    app.background_jobs = true;
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": 1.0}}})).unwrap().unwrap();
    if !same_poll {
        gate.store(true, Ordering::SeqCst);
        app.session.wait_job(save_job).unwrap();
        crate::jobs_ui::tick(&mut app, &ctx);
    }
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"commit": true}})).unwrap().unwrap();
    assert_eq!(reply["pending"], true, "PROBE INVALID: Open runs the re-develop as a job: {reply}");
    let commit = app.raw_redevelop.as_ref().map(|c| c.job).expect("PROBE INVALID: the commit job is held");
    if same_poll {
        gate.store(true, Ordering::SeqCst);
        app.session.wait_job(save_job).unwrap();
    }
    app.session.wait_job(commit).unwrap();
    crate::jobs_ui::tick(&mut app, &ctx);
    let docs = app.session.documents();
    let sane = docs.len() == 2 && docs.iter().all(|st| st.doc.id != a && st.doc.id != old);
    let b_dirty = docs.iter().find(|st| st.doc.id == b).is_some_and(|st| st.is_dirty());
    (b_dirty, sane, docs.len())
}

/// LP-A1 (main defect class A-1): a re-develop result resolved by apply-time tab index marks another tab saved.
#[test]
fn lab_p0_a1_same_poll_save_close_and_open_never_marks_another_tab_saved() {
    let (b_dirty, sane, left) = save_close_and_open(false);
    assert!(sane, "PROBE INVALID (control): A must be closed by its save and the RAW replaced ({left} documents left)");
    assert!(b_dirty, "CONTROL: A closed in an earlier poll, so B keeps its unsaved work");
    let (b_dirty, sane, left) = save_close_and_open(true);
    assert!(sane, "PROBE INVALID: A must be closed by its save and the RAW replaced ({left} documents left)");
    assert!(b_dirty, "LP-A1: A's save-close and the RAW's re-develop landed in one poll; the re-develop's tab index named B and marked its unsaved work saved");
}

/// LP-UP3 (main defect, hazard UP-3 / P0-TAB): while a file waits behind a profile prompt, a finished open is moved into
/// the waiting queue by `app.session.close(index)` with the job's apply-time tab index. After an ordinary tab move in
/// between, that index names another, accepted document with unsaved work.
#[test]
fn lab_p0_up3_a_waiting_open_never_takes_another_tab_into_the_queue() {
    for reorder in [false, true] {
        let label = if reorder { "LP-UP3 (C moved behind the new tab before the open's event)" } else { "CONTROL (no tab moved)" };
        let (mut app, dng) = lab_app();
        let ctx = egui::Context::default();
        app.run("prefs.set", json!({"path": "rawDefaults.openInCameraRaw", "value": false})).unwrap();
        app.run("file.new", json!({"width": 10, "height": 10})).unwrap();
        app.run("file.new", json!({"width": 12, "height": 12})).unwrap();
        app.session.execute("layer.new.layer", json!({})).unwrap();
        let c = app.session.documents()[1].doc.id;
        assert!(app.session.documents()[1].is_dirty(), "PROBE INVALID: C holds unsaved work");
        app.background_jobs = true;
        app.open_bytes("n.dng", &dng).unwrap();
        let job = app.jobs.opens.last().map(|o| o.job).expect("PROBE INVALID: the open runs in the background");
        app.session.wait_job(job).unwrap();
        assert_eq!(app.session.documents().len(), 3, "PROBE INVALID: the open's apply appended N at tab 2");
        let n = app.session.documents()[2].doc.id;
        if reorder {
            app.run("document.move", json!({"document": 1, "to": 2})).unwrap();
            assert_eq!(ids(&app)[1], n, "PROBE INVALID: tabs are now [A, N, C]");
        }
        // An earlier file is waiting on its prompt (only the queue's presence matters to the branch under test).
        app.held_opens.push_back(crate::open_prompt::Held {
            doc: photocraft_doc::Document::new(
                "waiting.psd",
                photocraft_geom::Size::new(4, 4),
                photocraft_doc::ColorMode::Rgb,
                photocraft_color::SampleType::U8,
            ),
            read_only: false,
            name: "waiting.psd".into(),
            path: None,
            warnings: Vec::new(),
            bytes: None,
            slot: None,
            question: Some(json!({"working": "sRGB", "missing": true, "action": "preserve"})),
            policy_applied: false,
        });
        crate::jobs_ui::tick(&mut app, &ctx);
        let c_open = ids(&app).contains(&c);
        let n_held = app.held_opens.iter().any(|h| h.doc.id == n);
        assert!(
            c_open && n_held && !ids(&app).contains(&n),
            "{label}: C (accepted, unsaved) open={c_open}; N queued behind the prompt={n_held}; tabs now {:?}",
            names(&app)
        );
    }
}

/// LP-A3 (R2b batch-cancel policy, A-3; main closes only the photograph shown): Cancel while a renamed RAW of the same
/// review is still decoding. Nothing of the cancelled review lands. `flip`: the open-in-Camera-Raw preference is
/// switched off after Cancel, before the decode lands (the A-3 case proper).
fn renamed_raw_after_cancel(flip: bool) -> (Vec<String>, bool, bool) {
    let (mut app, dng) = lab_app();
    let ctx = egui::Context::default();
    app.open_bytes("review.dng", &dng).unwrap();
    let raw = app.pending_raw_open.take().expect("PROBE INVALID: the review opens");
    open_raw(&mut app, &ctx, raw).unwrap();
    app.background_jobs = true;
    app.open_bytes("scan.dat", &dng).unwrap();
    let job: JobId = app.jobs.opens[0].job;
    cancel(&mut app);
    if flip {
        app.run("prefs.set", json!({"path": "rawDefaults.openInCameraRaw", "value": false})).unwrap();
    }
    let _ = app.session.wait_job(job);
    crate::jobs_ui::tick(&mut app, &ctx);
    (names(&app), app.pending_raw_open.is_some(), app.camera_raw.is_some())
}

#[test]
fn lab_p0_a3_a_renamed_raw_still_decoding_at_cancel_never_lands() {
    let (left, queued, shown) = renamed_raw_after_cancel(false);
    assert!(
        left.is_empty() && !queued && !shown,
        "LP-A3 (preference unchanged): a photograph of the cancelled review survived Cancel: {left:?}, Camera Raw queued {queued}"
    );
}

#[test]
fn lab_p0_a3b_a_renamed_raw_never_lands_after_review_on_open_is_switched_off() {
    let (left, queued, shown) = renamed_raw_after_cancel(true);
    assert!(
        left.is_empty() && !queued && !shown,
        "LP-A3b (preference switched off after Cancel): a photograph of the cancelled review survived Cancel: {left:?}"
    );
}

/// LP-S07 (R2b batch-cancel policy): a renamed RAW of a cancelled review that lands while the next review is open does not
/// join or revive anything.
#[test]
fn lab_p0_s07_a_late_renamed_raw_of_a_cancelled_review_does_not_join_the_next_review() {
    let (mut app, dng) = lab_app();
    let ctx = egui::Context::default();
    app.open_bytes("first.dng", &dng).unwrap();
    let raw = app.pending_raw_open.take().expect("PROBE INVALID: the review opens");
    open_raw(&mut app, &ctx, raw).unwrap();
    app.background_jobs = true;
    app.open_bytes("scan.dat", &dng).unwrap();
    let late: JobId = app.jobs.opens[0].job;
    cancel(&mut app);
    app.background_jobs = false;
    app.open_bytes("second.dng", &dng).unwrap();
    let second = app.pending_raw_open.take().expect("PROBE INVALID: the new review is queued");
    let second_id = second.document;
    open_raw(&mut app, &ctx, second).unwrap();
    let _ = app.session.wait_job(late);
    crate::jobs_ui::tick(&mut app, &ctx);
    assert_eq!(ids(&app), vec![second_id], "LP-S07: only the new review's photograph exists: {:?}", names(&app));
    assert!(app.pending_raw_open.is_none(), "LP-S07: nothing queued behind the new review");
}

/// LP-ENGINE (P0-ENGINE, main lacks the provisional guard) and LP-POSITIVE: while a RAW waits in Camera Raw, a control
/// `engine.execute` that would edit it is refused and leaves it untouched; an accepted document stays editable.
#[test]
fn lab_p0_engine_a_waiting_raw_is_refused_and_an_accepted_document_stays_editable() {
    let (mut app, dng) = lab_app();
    let ctx = egui::Context::default();
    app.run("file.new", json!({"width": 16, "height": 16})).unwrap();
    let accepted = app.session.documents()[0].doc.id;
    app.open_bytes("held.dng", &dng).unwrap();
    let raw = app.pending_raw_open.take().expect("PROBE INVALID: the review opens");
    let held = raw.document;
    open_raw(&mut app, &ctx, raw).unwrap();
    let held_index = app.session.documents().iter().position(|st| st.doc.id == held).expect("PROBE INVALID: the RAW is open");
    let before = app.session.documents()[held_index].revision;
    let reply = control(&mut app, &ctx, "engine.execute", json!({"command": "edit.fill", "params": {"document": held_index, "color": "#ff0000"}}));
    let after = app.session.documents().iter().find(|st| st.doc.id == held).map(|st| st.revision);
    // The positive control acts on the accepted document as the active tab (as a person or agent switching to it would):
    // `edit.fill` acts on the active document whatever index is passed.
    let accepted_index = app.session.documents().iter().position(|st| st.doc.id == accepted).expect("PROBE INVALID: the accepted document is open");
    assert!(app.session.set_active(accepted_index), "PROBE INVALID: the accepted document can be made active");
    let accepted_before = app.session.documents()[accepted_index].revision;
    let positive = control(&mut app, &ctx, "engine.execute", json!({"command": "edit.fill", "params": {"color": "#00ff00"}}));
    let accepted_after = app.session.documents().iter().find(|st| st.doc.id == accepted).map(|st| st.revision);
    assert!(
        positive.get("ok").and_then(Value::as_bool) == Some(true) && accepted_after > Some(accepted_before),
        "LP-POSITIVE: an accepted document must stay editable while a RAW waits: {positive}, revision {accepted_before} -> {accepted_after:?}"
    );
    assert!(
        reply.get("ok").and_then(Value::as_bool) == Some(false) && after == Some(before),
        "LP-ENGINE: a waiting RAW was edited through engine.execute: reply {reply}, revision {before} -> {after:?}"
    );
}

/// LP-HELD (port hazard H-MAIN-1; R2b batch-cancel policy, which main does not have): a RAW whose open finishes while an
/// earlier file waits on its profile prompt waits behind it. When the review is cancelled meanwhile, the RAW never opens
/// once the prompt is answered. CONTROL: without the Cancel, the same RAW reaches Camera Raw.
#[test]
fn lab_p0_held_a_raw_waiting_behind_a_prompt_never_opens_after_its_review_is_cancelled() {
    for cancel_review in [false, true] {
        let label = if cancel_review { "LP-HELD (review cancelled while the RAW waited)" } else { "CONTROL (no Cancel)" };
        let (mut app, dng) = lab_app();
        let ctx = egui::Context::default();
        app.open_bytes("first.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().expect("PROBE INVALID: the review opens");
        open_raw(&mut app, &ctx, raw).unwrap();
        // An earlier file is waiting on its profile prompt.
        app.held_opens.push_back(crate::open_prompt::Held {
            doc: photocraft_doc::Document::new(
                "waiting.psd",
                photocraft_geom::Size::new(4, 4),
                photocraft_doc::ColorMode::Rgb,
                photocraft_color::SampleType::U8,
            ),
            read_only: false,
            name: "waiting.psd".into(),
            path: None,
            warnings: Vec::new(),
            bytes: None,
            slot: None,
            question: Some(json!({"working": "sRGB", "missing": true, "action": "preserve"})),
            policy_applied: false,
        });
        app.background_jobs = true;
        app.open_bytes("second.dng", &dng).unwrap();
        let job: JobId = app.jobs.opens.last().map(|o| o.job).expect("PROBE INVALID: the second RAW opens in the background");
        app.session.wait_job(job).unwrap();
        crate::jobs_ui::tick(&mut app, &ctx);
        assert!(app.held_opens.iter().any(|h| h.name == "second.dng"), "PROBE INVALID: second.dng waits behind the prompt");
        if cancel_review {
            cancel(&mut app);
        }
        crate::open_prompt::answer(&mut app, Some("preserve")).unwrap();
        let second_open = names(&app).iter().any(|n| n == "second.dng");
        let second_queued = app.pending_raw_open.as_ref().is_some_and(|r| r.name == "second.dng")
            || app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).is_some_and(|r| r.name == "second.dng");
        if cancel_review {
            assert!(
                !second_open && !second_queued,
                "{label}: second.dng joined after Cancel: open {second_open}, Camera Raw queued {second_queued}; tabs {:?}",
                names(&app)
            );
        } else {
            assert!(second_open, "{label}: second.dng must open once the prompt is answered; tabs {:?}", names(&app));
        }
    }
}

/// LP-DOCID (main defect, A-a1; specialist A probe P1): an open job's result names its document by stable id as well as
/// by apply-time tab index, for a developed RAW and for an ordinary document alike.
#[test]
fn lab_p0_docid_an_open_result_names_its_document_by_id() {
    let (_, dng) = lab_app();
    let mut session = photocraft_engine::Session::new();
    session.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    let started = session.start_open("a.dng", photocraft_engine::jobs::OpenSource::Bytes(std::sync::Arc::new(dng))).unwrap();
    let v = match started {
        photocraft_engine::jobs::Started::Done(v) => v,
        photocraft_engine::jobs::Started::Job(job) => session.wait_job(job).unwrap(),
    };
    let index = v.get("document").and_then(Value::as_u64).expect("PROBE INVALID: the open returns its tab index") as usize;
    let at = session.documents().get(index).map(|st| st.doc.id.0);
    assert_eq!(v.get("docId").and_then(Value::as_u64), at, "LP-DOCID: the open result carries no stable document id: {v}");
}

/// LP-PENDING (main defect A-a5; specialist A probe P5): three RAWs opened together are all held for review; a second
/// RAW never overwrites the first's place in the queue, so Cancel leaves no unreviewed RAW behind as an accepted tab.
#[test]
fn lab_p0_pending_three_raws_opened_together_are_all_cancelled_with_their_review() {
    let (mut app, dng) = lab_app();
    let ctx = egui::Context::default();
    PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
    for name in ["one.dng", "two.dng", "three.dng"] {
        app.open_bytes(name, &dng).unwrap();
    }
    assert_eq!(app.session.documents().len(), 3, "PROBE INVALID: three RAWs decoded");
    for _ in 0..3 {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1360.0, 900.0))), ..Default::default() };
        ctx.run_ui(input, |ui| show(&mut app, ui.ctx())).textures_delta.clear();
    }
    assert!(app.camera_raw.is_some(), "PROBE INVALID: Camera Raw is showing a photograph");
    let reply = menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"cancel": true}}));
    assert!(reply.as_ref().is_some_and(Result::is_ok), "PROBE INVALID: Cancel is accepted: {reply:?}");
    assert!(names(&app).is_empty(), "LP-PENDING: RAWs that were never reviewed survived Cancel as ordinary tabs: {:?}", names(&app));
}

/// A file held back before it opens, with the Embedded Profile Mismatch question main asks (Ask When Opening).
fn held_psd(name: &str) -> crate::open_prompt::Held {
    crate::open_prompt::Held {
        doc: photocraft_doc::Document::new(name, photocraft_geom::Size::new(4, 4), photocraft_doc::ColorMode::Rgb, photocraft_color::SampleType::U8),
        read_only: false,
        name: name.into(),
        path: None,
        warnings: Vec::new(),
        bytes: None,
        slot: None,
        question: Some(json!({"working": "sRGB", "missing": true, "action": "preserve"})),
        policy_applied: false,
    }
}

/// LP-HELD-INLINE (port hazard H-MAIN-1b, raised by the Q4 cross-examiner): the LP-HELD case on the inline open path
/// (`background_jobs` off: the web build, or `PHOTOCRAFT_INLINE_JOBS`). A RAW decoded inline while an earlier file waits
/// on its profile prompt waits behind it; when the review is cancelled meanwhile it never opens. CONTROL: without Cancel it does.
#[test]
fn lab_p0_held_inline_a_raw_waiting_behind_a_prompt_never_opens_after_its_review_is_cancelled() {
    for cancel_review in [false, true] {
        let label = if cancel_review { "LP-HELD-INLINE (review cancelled while the RAW waited)" } else { "CONTROL (no Cancel)" };
        let (mut app, dng) = lab_app();
        let ctx = egui::Context::default();
        app.open_bytes("first.dng", &dng).unwrap();
        let raw = app.pending_raw_open.take().expect("PROBE INVALID: the review opens");
        open_raw(&mut app, &ctx, raw).unwrap();
        app.held_opens.push_back(held_psd("waiting.psd"));
        assert!(!app.background_jobs, "PROBE INVALID: the inline open path");
        app.open_bytes("second.dng", &dng).unwrap();
        assert!(app.held_opens.iter().any(|h| h.name == "second.dng"), "PROBE INVALID: second.dng waits behind the prompt");
        if cancel_review {
            cancel(&mut app);
        }
        crate::open_prompt::answer(&mut app, Some("preserve")).unwrap();
        let second_open = names(&app).iter().any(|n| n == "second.dng");
        let second_queued = app.pending_raw_open.as_ref().is_some_and(|r| r.name == "second.dng")
            || app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).is_some_and(|r| r.name == "second.dng");
        if cancel_review {
            assert!(
                !second_open && !second_queued,
                "{label}: second.dng joined after Cancel: open {second_open}, queued {second_queued}; tabs {:?}",
                names(&app)
            );
        } else {
            assert!(second_open, "{label}: second.dng must open once the prompt is answered; tabs {:?}", names(&app));
        }
    }
}

/// LP-PROMPT (liveness, raised by the Q4 cross-examiner): main's real Ask-When-Opening prompt for an unrelated file
/// can still be answered over the control channel while a RAW waits in Camera Raw (its answer applies to that held file,
/// never to a document in the session). The file then opens as an ordinary editable document; the RAW is untouched.
#[test]
fn lab_p0_prompt_an_unrelated_files_profile_prompt_can_be_answered_while_a_raw_waits() {
    let (mut app, dng) = lab_app();
    let ctx = egui::Context::default();
    app.open_bytes("first.dng", &dng).unwrap();
    let raw = app.pending_raw_open.take().expect("PROBE INVALID: the review opens");
    let held = raw.document;
    open_raw(&mut app, &ctx, raw).unwrap();
    let held_revision = app.session.documents().iter().find(|st| st.doc.id == held).map(|st| st.revision);
    crate::open_prompt::arrive(&mut app, held_psd("other.psd"));
    let prompt = app.ui.dialogs.iter().find(|d| crate::open_prompt::owns(&d.fields)).map(|d| d.id).expect("PROBE INVALID: the profile prompt is shown");
    assert_eq!(app.session.active().map(|st| st.doc.id), Some(held), "PROBE INVALID: the RAW is the active tab");
    let reply = control(&mut app, &ctx, "ui.dialog.confirm", json!({"dialog": prompt}));
    assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(true), "LP-PROMPT: answering an unrelated file's profile prompt was refused: {reply}");
    assert!(names(&app).iter().any(|n| n == "other.psd"), "LP-PROMPT: other.psd opened: {:?}", names(&app));
    assert_eq!(app.session.documents().iter().find(|st| st.doc.id == held).map(|st| st.revision), held_revision, "the RAW is untouched");
}
