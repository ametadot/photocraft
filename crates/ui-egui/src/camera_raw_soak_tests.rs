//! Explicit, headless full-workflow workload. An external owner starts a fresh test
//! process for each phase and interrupts only the acknowledged phase-2 child.
//! No eframe window, native dialog, live transport or real profile is used.
use super::*;
use std::collections::BTreeMap;
use std::io::Write;

fn receipt(path: &Path, value: &Value) {
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(&serde_json::to_vec_pretty(value).unwrap()).unwrap();
    file.sync_all().unwrap();
}

fn recipes(app: &PhotocraftApp) -> BTreeMap<String, photocraft_io::raw_session::NativeRecipe> {
    filmstrip_raws(app).into_iter().map(|raw| (raw.name, persistence_ui::recipe(app, raw.photo).unwrap().clone())).collect()
}

fn workspace(app: &PhotocraftApp) -> Value {
    let raws = filmstrip_raws(app);
    let mut selected: Vec<_> = raws.iter().filter(|r| app.raw_filmstrip.selection.contains(r.photo)).map(|r| r.name.clone()).collect();
    selected.sort();
    let active = app.camera_raw.as_ref().and_then(|d| d.raw_open.as_ref()).map(|r| r.name.clone());
    let mut per_photo = live_view_receipt(app);
    // An unvisited queued photograph has default view state. Resume may already
    // have parked it explicitly; compare the same complete set in either form.
    for raw in &raws {
        per_photo["views"].as_object_mut().unwrap().entry(raw.name.clone()).or_insert_with(|| {
            json!({"scope":crate::state::CameraRawScopeState::default(),
                "navigation":crate::state::CameraRawPreviewState::default(),"before":false})
        });
    }
    json!({"filmstrip":app.raw_filmstrip.view,"selected":selected,"active":active,
        "scope":app.ui.camera_raw_scope,"navigation":app.ui.camera_raw_preview,"per_photo":per_photo})
}

fn preview_cache_probe(app: &mut PhotocraftApp, ctx: &egui::Context, root: &Path, phase: u64, samples: &mut Vec<f64>) {
    let raws = filmstrip_raws(app);
    // The Sony ARW with an existing sidecar, by its neutral role alias in the disposable inputs.
    let index = raws.iter().position(|r| r.name == "sample_sony_01.ARW").unwrap();
    let prior = recipes(app);
    activate(app, ctx, index);
    pump(app, ctx, samples);
    let original_exposure = app.camera_raw.as_ref().unwrap().params.exposure;
    let mut rows = Vec::new();
    for mode in ["first_visit", "warm_repeat", "after_navigation_eviction"] {
        if mode == "after_navigation_eviction" {
            let previous_texture = app.camera_raw.as_ref().unwrap().tex.as_ref().unwrap().id();
            activate(app, ctx, (index + 1) % raws.len());
            pump(app, ctx, samples);
            activate(app, ctx, index);
            pump(app, ctx, samples);
            // select_raw_review drops the old dialog, retaining only its settings.
            // The revisited dialog has a newly allocated preview texture/proxy.
            assert_ne!(app.camera_raw.as_ref().unwrap().tex.as_ref().unwrap().id(), previous_texture);
        }
        let mut timings = Vec::new();
        for value in [0.125, -0.125, original_exposure] {
            let started = Instant::now();
            exposure(app, ctx, value);
            pump(app, ctx, samples);
            timings.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        rows.push(json!({"mode":mode,"exposure_action_and_frame_ms":timings}));
    }
    assert_eq!(recipes(app), prior, "cache probe restores all photograph recipes");
    await_ack(app, ctx, samples, raws.len());
    receipt(
        &root.join(format!("phase-{phase}-cache-probe.json")),
        &json!({"photo":"sample_sony_01.ARW","rows":rows,
        "scope":"Same source and exposure sequence; actual active-dialog preview/proxy recreation through navigation. Decoded documents, CMS and OS caches remain retained. No whole-process leak or cold-disk claim."}),
    );
}

fn pump(app: &mut PhotocraftApp, ctx: &egui::Context, samples: &mut Vec<f64>) {
    let start = Instant::now();
    crate::jobs_ui::tick(app, ctx);
    frame(app, ctx);
    samples.push(start.elapsed().as_secs_f64() * 1000.0);
    let state = persistence_ui::describe(app);
    assert!(state.get("error").is_none_or(Value::is_null), "soak error: {state}; {}", app.ui.status);
}

fn await_ack(app: &mut PhotocraftApp, ctx: &egui::Context, samples: &mut Vec<f64>, count: usize) -> Value {
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        pump(app, ctx, samples);
        let state = persistence_ui::describe(app);
        if state["photos"].as_array().is_some_and(|p| p.len() == count)
            && state["busy"] == false
            && !state["acknowledged"].is_null()
            && state["acknowledged"] == state["revision"]
            && !app.session.has_jobs()
        {
            return state;
        }
        assert!(Instant::now() < deadline, "soak acknowledgement timed out: {state}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn await_closed(app: &mut PhotocraftApp, ctx: &egui::Context, root: &Path, phase: u64, samples: &mut Vec<f64>) {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(900);
    loop {
        // Real-file Open Images develops on the background jobs queue. Pump the
        // same job completion path as the app, as during admission and export.
        pump(app, ctx, samples);
        let state = persistence_ui::describe(app);
        if state["available"] == false && app.camera_raw.is_none() {
            receipt(
                &root.join(format!("phase-{phase}-closure.json")),
                &json!({
                    "elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
                    "editor_documents":app.session.documents().len(),
                    "provisional_documents":app.session.provisional_documents().len(),
                    "jobs_pending":app.session.has_jobs()
                }),
            );
            return;
        }
        if Instant::now() >= deadline {
            receipt(&root.join(format!("phase-{phase}-closure-timeout.json")), &state);
            panic!("workflow closure exceeded 900 seconds; inspect closure-timeout.json");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stats(values: &[f64]) -> Value {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let at = |percent: usize| values.get(values.len().saturating_sub(1) * percent / 100).copied();
    json!({"count":values.len(),"p50_ms":at(50),"p95_ms":at(95),"max_ms":values.last()})
}

fn storage(root: &Path) -> (u64, usize, usize) {
    let (mut bytes, mut generations, mut operations) = (0u64, 0usize, 0usize);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    stack.push(entry.path());
                } else if kind.is_file() {
                    bytes += entry.metadata().unwrap().len();
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    generations += usize::from(name.starts_with("generation-"));
                    operations += usize::from(name.starts_with("operation-"));
                }
            }
        }
    }
    (bytes, generations, operations)
}

fn activate(app: &mut PhotocraftApp, ctx: &egui::Context, index: usize) -> PhotoId {
    let raw = filmstrip_raws(app).into_iter().nth(index).unwrap();
    menu(app, ctx, "filter.cameraRaw", &json!({"ui":{"selectRaw":raw.document.0}})).unwrap().unwrap();
    app.raw_filmstrip.selection.select_only(raw.photo);
    raw.photo
}

fn full_export(app: &mut PhotocraftApp, ctx: &egui::Context, root: &Path, phase: u64, samples: &mut Vec<f64>) -> f64 {
    let raws = filmstrip_raws(app);
    let raw = raws.iter().find(|r| r.name.to_ascii_lowercase().ends_with(".jpg")).unwrap();
    let target = raw.photo;
    let name = raw.name.clone();
    let expected = persistence_ui::recipe(app, target).unwrap().clone();
    let before = recipes(app);
    jpeg_ui::request(app, target).unwrap();
    let destination = root.join("exports").join(format!("phase-{phase}-frozen.jpg"));
    let captured = jpeg_ui::fixture_export_destination(app, ctx, &destination).unwrap();
    assert_eq!(captured, expected);
    // Navigation is permitted after the worker starts, and must not retarget it.
    let other = raws.iter().find(|r| r.photo != target).unwrap();
    menu(app, ctx, "filter.cameraRaw", &json!({"ui":{"selectRaw":other.document.0}})).unwrap().unwrap();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(300);
    while jpeg_ui::is_running(app) {
        pump(app, ctx, samples);
        assert!(Instant::now() < deadline, "JPEG worker timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(app.raw_filmstrip.jpeg.message.as_deref().is_some_and(|m| m.starts_with("JPEG saved:")), "{:?}", app.raw_filmstrip.jpeg.message);
    assert_eq!(recipes(app), before, "export cannot clear or retarget edits");
    let output = std::fs::read(&destination).unwrap();
    // Independent render/encode from the captured source and full recipe is the
    // byte oracle; checking merely that a JPEG exists would miss wrong-photo export.
    let source = std::fs::read(root.join("inputs").join(&name)).unwrap();
    let rendered = photocraft_io::raw_session::controller::render_recipe(&name, &source, &captured).unwrap();
    let mut options = photocraft_io::ExportOptions::default();
    options.encode.jpeg_quality = 90;
    assert_eq!(output, photocraft_io::export(&rendered.document, "jpg", &options).unwrap().bytes);
    started.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore = "explicit owned subprocess workload; requires isolated real Mixed22 and elapsed-time protocol"]
fn measured_integrated_workflow_child() {
    let root = PathBuf::from(std::env::var_os("PHOTOCRAFT_SOAK_ROOT").expect("explicit isolated soak root"));
    let phase: u64 = std::env::var("PHOTOCRAFT_SOAK_PHASE").unwrap().parse().unwrap();
    let duration: u64 = std::env::var("PHOTOCRAFT_SOAK_SECONDS").unwrap().parse().unwrap();
    assert!(root.is_absolute() && duration <= 7200 && (1..=4).contains(&phase));
    let fixture = std::mem::ManuallyDrop::new(Fixture { root: root.clone(), bytes: Vec::new() });
    let mut app = if phase == 4 { fixture.app_at_cache(root.join("fresh-sidecar-cache")) } else { fixture.app() };
    let ctx = context();
    app.background_jobs = true;
    app.run("prefs.set", json!({"path":"rawDefaults.jpegHandling","value":"all"})).unwrap();
    let mut frame_samples = Vec::new();
    let mut ack_samples = Vec::new();
    let mut action_samples = Vec::new();
    let admission_start = Instant::now();
    if phase == 1 || phase == 4 {
        let mut paths: Vec<_> = std::fs::read_dir(root.join("inputs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| ["nef", "arw", "jpg", "jpeg"].iter().any(|x| e.eq_ignore_ascii_case(x))))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        assert_eq!(paths.len(), 22);
        assert_eq!(app.open_paths(&paths), 22);
    } else {
        let prior: Value = serde_json::from_slice(&std::fs::read(root.join(format!("phase-{}-ack.json", phase - 1))).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            pump(&mut app, &ctx, &mut frame_samples);
            let state = persistence_ui::describe(&app);
            if state["chooser"] == true && state["busy"] == false {
                break;
            }
            assert!(Instant::now() < deadline, "recovery chooser did not arrive: {state}");
            std::thread::sleep(Duration::from_millis(20));
        }
        persistence_ui::control(&mut app, &json!({"action":"resume","sessionId":prior["session"]})).unwrap();
    }
    let state = await_ack(&mut app, &ctx, &mut frame_samples, 22);
    let admission_ms = admission_start.elapsed().as_secs_f64() * 1000.0;
    if phase > 1 {
        let expected: BTreeMap<String, photocraft_io::raw_session::NativeRecipe> =
            serde_json::from_slice(&std::fs::read(root.join(format!("phase-{}-recipes.json", if phase == 4 { 3 } else { phase - 1 }))).unwrap()).unwrap();
        assert_eq!(recipes(&app), expected, "independent process must recover every native field");
        if phase < 4 {
            let previous: Value = serde_json::from_slice(&std::fs::read(root.join(format!("phase-{}-workspace.json", phase - 1))).unwrap()).unwrap();
            let actual = workspace(&app);
            receipt(&root.join(format!("phase-{phase}-workspace-restored.json")), &actual);
            assert!(actual == previous, "recovered workspace differs; inspect workspace-restored.json against prior workspace.json");
        }
    }
    assert_eq!(filmstrip_raws(&app).len(), 22);
    assert_eq!(app.session.provisional_documents().len(), 22);
    if phase == 4 {
        assert!(state["photos"].as_array().unwrap().iter().all(|p| p["pendingXmp"] == false));
        let id = activate(&mut app, &ctx, 0);
        persistence_ui::set_metadata(&mut app, &[id], |m| m.marked = !m.marked).unwrap();
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"filmstrip":{"filter":{"stars":{"comparison":">=","value":4}}}}})).unwrap().unwrap();
        await_ack(&mut app, &ctx, &mut frame_samples, 22);
        let expected = recipes(&app);
        commit(&mut app).unwrap();
        let plan = wait_for(&mut app, &ctx, "final save-then-open plan", |_, s| s["busy"] == false && !s["plan"].is_null());
        persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
        await_closed(&mut app, &ctx, &root, phase, &mut frame_samples);
        assert_eq!(app.session.documents().len(), 22);
        assert!(app.session.provisional_documents().is_empty());
        for (name, recipe) in expected {
            let target = photocraft_io::raw_session::commit::sidecar_target(&root.join("inputs").join(name)).unwrap();
            assert_eq!(photocraft_io::raw_session::xmp::load_native(&std::fs::read(target).unwrap()).unwrap(), Some(recipe));
        }
        receipt(
            &root.join("phase-4-pass.json"),
            &json!({"pid":std::process::id(),"status":"PASS","independent_xmp_reopen":22,"opened_once":22,"cache":root.join("fresh-sidecar-cache"),"admission_ms":admission_ms}),
        );
        return;
    }

    preview_cache_probe(&mut app, &ctx, &root, phase, &mut frame_samples);
    if phase == 1 {
        // Ensure the short harness validation also covers a real write/reopen for
        // every member; the measured run later revisits all 22 through group edits.
        for raw in filmstrip_raws(&app) {
            persistence_ui::set_metadata(&mut app, &[raw.photo], |m| m.marked = !m.marked).unwrap();
        }
        await_ack(&mut app, &ctx, &mut frame_samples, 22);
    }

    let start = Instant::now();
    let mut cycle = 0usize;
    let mut exports = 0usize;
    let mut export_ms = None;
    let mut telemetry = std::fs::File::create(root.join(format!("phase-{phase}-telemetry.jsonl"))).unwrap();
    while start.elapsed() < Duration::from_secs(duration) || cycle == 0 {
        let action_start = Instant::now();
        let first = activate(&mut app, &ctx, cycle % 22);
        let second_index = (cycle + 1) % 22;
        exposure(&mut app, &ctx, 0.5);
        let second = activate(&mut app, &ctx, second_index);
        exposure(&mut app, &ctx, -0.5);
        activate(&mut app, &ctx, cycle % 22);
        app.raw_filmstrip.selection.toggle(second);
        let group_before = recipes(&app);
        let old = app.camera_raw.as_ref().unwrap().params.clone();
        let mut next = old.clone();
        next.exposure = 0.8;
        let change = edit_actions::slider_group(&app, &old, &next).unwrap();
        apply_group(&mut app, change.clone()).unwrap();
        group_control(&mut app, &json!({"action":"undo"})).unwrap();
        assert_eq!(recipes(&app), group_before, "group undo is one coherent operation");
        apply_group(&mut app, change).unwrap();
        let current = recipes(&app);
        let raws = filmstrip_raws(&app);
        let name1 = &raws.iter().find(|r| r.photo == first).unwrap().name;
        let name2 = &raws.iter().find(|r| r.photo == second).unwrap().name;
        assert!((current[name1].adjustments.exposure - 0.8).abs() < f32::EPSILON);
        assert!((current[name2].adjustments.exposure + 0.2).abs() < f32::EPSILON);
        persistence_ui::set_metadata(&mut app, &[first, second], |m| {
            m.rating = Some(3);
            m.label = "Review".into();
            m.marked = cycle.is_multiple_of(2);
        })
        .unwrap();
        let orientation = if cycle.is_multiple_of(2) { "horizontal" } else { "vertical" };
        for grid in [true, false] {
            menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"filmstrip":{"orientation":orientation,"grid":grid,"sort":{"by":"starRating","reverse":cycle.is_multiple_of(2)},"filter":{"stars":{"comparison":">=","value":4}}}}})).unwrap().unwrap();
            pump(&mut app, &ctx, &mut frame_samples);
        }
        assert_eq!(filmstrip_raws(&app).len(), 22, "filtered members remain retained");
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"filmstrip":{"filter":{"stars":null}},"before":true}})).unwrap().unwrap();
        pump(&mut app, &ctx, &mut frame_samples);
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"before":false}})).unwrap().unwrap();
        action_samples.push(action_start.elapsed().as_secs_f64() * 1000.0);
        let ack_start = Instant::now();
        let state = await_ack(&mut app, &ctx, &mut frame_samples, 22);
        ack_samples.push(ack_start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(state["acknowledged"], state["revision"]);
        if cycle == 0 {
            export_ms = Some(full_export(&mut app, &ctx, &root, phase, &mut frame_samples));
            exports += 1;
        }
        let (bytes, generations, operations) = storage(&fixture.cache());
        writeln!(telemetry, "{}", json!({"elapsed_seconds":start.elapsed().as_secs_f64(),"cycle":cycle,
            "process_cache_state":if cycle == 0 {"first workload after fresh process and admission"} else {"revisited workload in same process"},
            "action_ms":action_samples.last(),"revision":state["revision"],"acknowledged":state["acknowledged"],"pending":state["pendingXmp"],"cache_bytes":bytes,"generation_files":generations,"operation_files":operations,"busy":state["busy"],"checkpoint_ack_wait_ms":ack_samples.last()})).unwrap();
        telemetry.flush().unwrap();
        cycle += 1;
        let idle_until = (start + Duration::from_secs((cycle as u64) * 15)).min(start + Duration::from_secs(duration));
        while Instant::now() < idle_until {
            pump(&mut app, &ctx, &mut frame_samples);
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    // Carry a real hidden-pending case through both Keep and interruption. One
    // member remains visible; other edits still belong to the retained shoot.
    let visible = filmstrip_raws(&app).first().unwrap().photo;
    persistence_ui::set_metadata(&mut app, &[visible], |metadata| metadata.rating = Some(5)).unwrap();
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"filmstrip":{"filter":{"stars":{"comparison":">=","value":4}}}}})).unwrap().unwrap();
    let final_state = await_ack(&mut app, &ctx, &mut frame_samples, 22);
    assert!(
        final_state["photos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|photo| { photo["recipe"]["metadata"]["rating"].as_i64().is_some_and(|rating| rating < 4) && photo["pendingXmp"] == true }),
        "the >= 4 filter must hide at least one changed retained member"
    );
    receipt(&root.join(format!("phase-{phase}-workspace.json")), &workspace(&app));
    receipt(&root.join(format!("phase-{phase}-recipes.json")), &serde_json::to_value(recipes(&app)).unwrap());
    receipt(
        &root.join(format!("phase-{phase}-ack.json")),
        &json!({"pid":std::process::id(),"session":final_state["session"],"revision":final_state["revision"],"acknowledged":final_state["acknowledged"],"elapsed_seconds":start.elapsed().as_secs_f64(),"cycles":cycle,"exports":exports,"export_ms":export_ms,"admission_ms":admission_ms,"frame":stats(&frame_samples),"action":stats(&action_samples),"ack_wait":stats(&ack_samples),"gpu":"NOT RUN: embedded CPU-only context","queue_depth":"NOT RUN: no public depth counter"}),
    );
    if phase == 1 {
        persistence_ui::control(&mut app, &json!({"action":"keep"})).unwrap();
        await_closed(&mut app, &ctx, &root, phase, &mut frame_samples);
        receipt(&root.join("phase-1-pass.json"), &json!({"status":"PASS","keep_closed":true,"pid":std::process::id()}));
    } else if phase == 2 {
        // The parent terminates this exact owned child only after reading its ack.
        loop {
            std::thread::sleep(Duration::from_millis(100));
        }
    } else {
        let expected = recipes(&app);
        control(&mut app, "done");
        let plan = wait_for(&mut app, &ctx, "soak Done plan", |_, s| s["busy"] == false && !s["plan"].is_null());
        assert!(plan["plan"]["photos"].as_array().unwrap().iter().any(|photo| photo["hidden"] == true), "Done must include hidden pending members");
        persistence_ui::control(&mut app, &json!({"action":"save","planId":plan["plan"]["id"]})).unwrap();
        await_closed(&mut app, &ctx, &root, phase, &mut frame_samples);
        for (name, recipe) in expected {
            let target = photocraft_io::raw_session::commit::sidecar_target(&root.join("inputs").join(name)).unwrap();
            assert_eq!(photocraft_io::raw_session::xmp::load_native(&std::fs::read(target).unwrap()).unwrap(), Some(recipe));
        }
        assert!(session_dirs(&fixture).is_empty(), "completed draft must retire");
        receipt(&root.join("phase-3-pass.json"), &json!({"status":"PASS","done_closed":true,"pid":std::process::id()}));
    }
}
