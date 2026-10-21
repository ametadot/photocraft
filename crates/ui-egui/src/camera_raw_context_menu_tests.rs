// Offscreen interaction regressions for the owner's 22 October context-menu request.
// Included inside camera_raw_ui::tests to reuse synthetic RAW fixtures and pointer helpers.
use super::*;

fn context_widgets(app: &mut PhotocraftApp, ctx: &egui::Context) -> Vec<(String, ERect, bool)> {
    ctx.enable_accesskit();
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| show(app, ui.ctx()));
    out.textures_delta.clear();
    accessible(&out)
}

fn choose_context(app: &mut PhotocraftApp, ctx: &egui::Context, label: &str) {
    let widgets = context_widgets(app, ctx);
    let (rect, enabled) = widget(&widgets, label);
    assert!(enabled, "{label} must be available: {:?}", names(&widgets));
    // The deliberately minimal synthetic DNG reports its missing colour matrix
    // when admitted. Judge this click's outcome separately from that known import
    // warning, while refusing to hide any other pre-existing error.
    if app.ui.status_error {
        assert!(app.ui.status.starts_with("Opened ") && app.ui.status.contains("the DNG has no colour matrix; colours are approximate"), "{}", app.ui.status);
        app.ui.status.clear();
        app.ui.status_error = false;
    }
    click(app, ctx, rect.center());
    frame(app, ctx, WINDOW, vec![]);
    assert!(!app.ui.status_error, "{}", app.ui.status);
}

fn photo_id(app: &PhotocraftApp, document: photocraft_doc::DocId) -> PhotoId {
    filmstrip_raws(app).iter().find(|raw| raw.document == document).unwrap().photo
}

fn settings(app: &PhotocraftApp, document: photocraft_doc::DocId) -> CameraRaw {
    if active_document(app) == document {
        app.camera_raw.as_ref().unwrap().params.clone()
    } else {
        app.raw_filmstrip.parked.iter().find(|item| item.raw.document == document).unwrap().params.clone()
    }
}

fn all_settings(app: &PhotocraftApp, documents: &[photocraft_doc::DocId]) -> Vec<CameraRaw> {
    documents.iter().map(|document| settings(app, *document)).collect()
}

/// Distinct recipes make copying from the clicked card, touching an unrelated photo,
/// resetting a selection, or restoring only part of an undo observable.
fn edited_session() -> (PhotocraftApp, egui::Context, Vec<photocraft_doc::DocId>) {
    let (mut app, ctx, ids) = review_session_n(4);
    for (i, id) in ids.iter().enumerate() {
        click_card(&mut app, &ctx, *id);
        let exposure = [0.5, -0.75, 1.25, -1.0][i];
        let contrast = [11, -22, 33, -44][i];
        menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui": {"set": {"exposure": exposure, "contrast": contrast}}})).unwrap().unwrap();
        frame(&mut app, &ctx, WINDOW, vec![]);
    }
    select_group(&mut app, &ctx, &ids[..3], ids[0]);
    app.raw_filmstrip.group_undo.clear();
    (app, ctx, ids)
}

fn layout(app: &mut PhotocraftApp, ctx: &egui::Context, name: &str) {
    match name {
        "grid" => set_view(app, ctx, json!({"grid": true})),
        orientation => set_view(app, ctx, json!({"orientation": orientation, "grid": false})),
    }
    frame(app, ctx, WINDOW, vec![]);
}

#[test]
fn context_menu_settings_actions_are_absent_from_the_adjustment_panel() {
    let (mut app, ctx, ids) = edited_session();
    for selection in [&ids[..1], &ids[..3]] {
        select_group(&mut app, &ctx, selection, ids[0]);
        let widgets = context_widgets(&mut app, &ctx);
        for removed in ["Set value…", "Set value...", "Reset selected", "Synchronise selected", "Undo group edit", "Sync Settings", "Reset Settings"] {
            assert!(!widgets.iter().any(|(label, ..)| label == removed), "{removed} must not remain a permanently visible panel control");
        }
        assert!(widgets.iter().any(|(label, ..)| label == "Exposure"), "ordinary adjustment controls remain available");
    }
}

#[test]
fn context_menu_sync_uses_active_source_including_hidden_selection_and_undo_is_exact_in_every_layout() {
    for mode in ["vertical", "horizontal", "grid"] {
        let (mut app, ctx, ids) = edited_session();
        layout(&mut app, &ctx, mode);
        // Mark B only, then hide rated photos. B stays selected and must still be synchronised.
        app.raw_filmstrip.parked.iter_mut().find(|item| item.raw.document == ids[1]).unwrap().raw.rating = PhotoRating::Stars(2);
        set_view(&mut app, &ctx, json!({"filter": {"rated": false}}));
        assert!(!card_order(&app).contains(&ids[1]), "{mode}: selected B is hidden");
        let before = all_settings(&app, &ids);
        let selection_before = selected(&app);
        let clicked = card(&app, ids[2]).center();
        secondary_click(&mut app, &ctx, clicked);
        assert_eq!((active_document(&app), selected(&app)), (ids[0], selection_before.clone()), "right-click does not select or activate C");
        choose_context(&mut app, &ctx, "Sync Settings");
        let after = all_settings(&app, &ids);
        for target in 0..3 {
            let mut expected = before[0].clone();
            expected.encoding_gamma = before[target].encoding_gamma;
            expected.pixel_scale = before[target].pixel_scale;
            assert_eq!(after[target], expected, "{mode}: selected target {target} copies active A, never clicked C");
        }
        assert_eq!(after[3], before[3], "{mode}: unselected D is unchanged");
        assert_eq!((active_document(&app), selected(&app)), (ids[0], selection_before.clone()));
        assert_eq!(app.raw_filmstrip.group_undo.len(), 1, "one coherent undo entry");
        let clicked = card(&app, ids[2]).center();
        secondary_click(&mut app, &ctx, clicked);
        choose_context(&mut app, &ctx, "Undo group edit");
        assert_eq!(all_settings(&app, &ids), before, "{mode}: undo restores every setting exactly");
        assert_eq!((active_document(&app), selected(&app)), (ids[0], selection_before));
        assert!(app.raw_filmstrip.group_undo.is_empty());
    }
}

#[test]
fn context_menu_thumbnail_reset_changes_only_the_clicked_photo_in_every_layout() {
    for mode in ["vertical", "horizontal", "grid"] {
        let (mut app, ctx, ids) = edited_session();
        layout(&mut app, &ctx, mode);
        let before = all_settings(&app, &ids);
        let selection_before = selected(&app);
        let clicked = card(&app, ids[2]).center();
        secondary_click(&mut app, &ctx, clicked);
        choose_context(&mut app, &ctx, "Reset Settings");
        let after = all_settings(&app, &ids);
        let expected = CameraRaw { encoding_gamma: before[2].encoding_gamma, pixel_scale: before[2].pixel_scale, ..Default::default() };
        assert_eq!(after[2], expected, "{mode}: only clicked C resets to defaults");
        for untouched in [0, 1, 3] {
            assert_eq!(after[untouched], before[untouched], "{mode}: photo {untouched} remains unchanged");
        }
        assert_eq!((active_document(&app), selected(&app)), (ids[0], selection_before));
        assert_eq!(app.raw_filmstrip.group_undo.len(), 1);
        let clicked = card(&app, ids[2]).center();
        secondary_click(&mut app, &ctx, clicked);
        choose_context(&mut app, &ctx, "Undo group edit");
        assert_eq!(all_settings(&app, &ids), before, "{mode}: resetting one photo is undoable");
    }
}

#[test]
fn context_menu_preview_reset_changes_only_active_photo_with_multiple_selected() {
    let (mut app, ctx, ids) = edited_session();
    let before = all_settings(&app, &ids);
    let selection_before = selected(&app);
    let photo = app.camera_raw.as_ref().unwrap().scope.preview_rect.unwrap();
    secondary_click(&mut app, &ctx, photo.center());
    let widgets = context_widgets(&mut app, &ctx);
    assert!(!widgets.iter().any(|(label, ..)| label == "Sync Settings"), "sync belongs to the selected Filmstrip context");
    choose_context(&mut app, &ctx, "Reset Settings");
    let after = all_settings(&app, &ids);
    let expected = CameraRaw { encoding_gamma: before[0].encoding_gamma, pixel_scale: before[0].pixel_scale, ..Default::default() };
    assert_eq!(after[0], expected);
    assert_eq!(&after[1..], &before[1..], "other selected photos keep their settings");
    assert_eq!((active_document(&app), selected(&app)), (ids[0], selection_before));
}

#[test]
fn context_menu_preview_reset_keeps_the_photo_captured_when_the_menu_opened() {
    let (mut app, ctx, ids) = edited_session();
    let before = all_settings(&app, &ids);
    let selection_before = selected(&app);
    let photo = app.camera_raw.as_ref().unwrap().scope.preview_rect.unwrap();
    secondary_click(&mut app, &ctx, photo.center());
    assert!(egui::Popup::is_any_open(&ctx));
    // Simulate an independent controller changing the active preview while this
    // already-open menu still names A. The click must never reset newly shown B.
    select_raw_review(&mut app, &ctx, ids[1]).unwrap();
    assert!(egui::Popup::is_any_open(&ctx));
    choose_context(&mut app, &ctx, "Reset Settings");
    let after = all_settings(&app, &ids);
    let expected = CameraRaw { encoding_gamma: before[0].encoding_gamma, pixel_scale: before[0].pixel_scale, ..Default::default() };
    assert_eq!(after[0], expected, "Reset retains the photo named by the original right-click");
    assert_eq!(&after[1..], &before[1..], "the new active photo and other photos remain unchanged");
    assert_eq!((active_document(&app), selected(&app)), (ids[1], selection_before));
    assert_eq!(app.raw_filmstrip.group_undo.len(), 1);
}

#[test]
fn context_menu_set_value_keeps_captured_targets_when_selection_changes_before_apply() {
    let (mut app, ctx, ids) = edited_session();
    let before = all_settings(&app, &ids);
    let captured: Vec<_> = ids[..3].iter().map(|id| photo_id(&app, *id)).collect();
    let at = card(&app, ids[2]).center();
    secondary_click(&mut app, &ctx, at);
    choose_context(&mut app, &ctx, "Set value…");
    assert_eq!(app.raw_filmstrip.group_set.as_ref().unwrap().2, captured);
    // An external selection update is deliberate adversarial test setup. The
    // modal was opened for A/B/C; its Apply button must not start editing D.
    let newly_selected = photo_id(&app, ids[3]);
    app.raw_filmstrip.selection.select_only(newly_selected);
    app.raw_filmstrip.group_set.as_mut().unwrap().1 = 1.75;
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| show(&mut app, ui.ctx()));
    output.textures_delta.clear();
    let buttons: Vec<_> = output
        .platform_output
        .accesskit_update
        .as_ref()
        .unwrap()
        .nodes
        .iter()
        .filter_map(|(_, node)| {
            if node.role() != egui::accesskit::Role::Button || node.label() != Some("Set value") {
                return None;
            }
            let bounds = node.bounds()?;
            Some(ERect::from_min_max(pos2(bounds.x0 as f32, bounds.y0 as f32), pos2(bounds.x1 as f32, bounds.y1 as f32)))
        })
        .collect();
    assert_eq!(buttons.len(), 1, "one modal Apply button, distinct from the heading");
    click(&mut app, &ctx, buttons[0].center());
    frame(&mut app, &ctx, WINDOW, vec![]);
    assert!(!app.ui.status_error, "{}", app.ui.status);
    assert!(app.raw_filmstrip.group_set.is_none(), "the successful modal closes");
    let after = all_settings(&app, &ids);
    for target in 0..3 {
        let mut expected = before[target].clone();
        expected.exposure = 1.75;
        assert_eq!(after[target], expected, "only exposure changes for original target {target}");
    }
    assert_eq!(after[3], before[3], "D receives neither the absolute edit nor an accidental relative slider delta");
    assert_eq!((active_document(&app), selected(&app)), (ids[0], vec![ids[3]]));
    assert_eq!(app.raw_filmstrip.group_undo.len(), 1, "modal apply produces exactly one group edit");
}

#[test]
fn context_menu_unselected_thumbnail_and_single_selection_do_not_offer_sync() {
    let (mut app, ctx, ids) = edited_session();
    let before = all_settings(&app, &ids);
    let selected_before = selected(&app);
    let at = card(&app, ids[3]).center();
    secondary_click(&mut app, &ctx, at);
    let widgets = context_widgets(&mut app, &ctx);
    assert!(!widgets.iter().any(|(label, ..)| label == "Sync Settings"));
    assert!(widgets.iter().any(|(label, ..)| label == "Reset Settings"));
    assert_eq!((active_document(&app), selected(&app)), (ids[0], selected_before));
    assert_eq!(all_settings(&app, &ids), before);
    key(&mut app, &ctx, egui::Key::Escape);
    select_group(&mut app, &ctx, &ids[..1], ids[0]);
    let at = card(&app, ids[0]).center();
    secondary_click(&mut app, &ctx, at);
    assert!(!context_widgets(&mut app, &ctx).iter().any(|(label, ..)| label == "Sync Settings"));
}

#[test]
fn context_menu_stale_unknown_and_duplicate_targets_refuse_atomically() {
    let (mut app, _ctx, ids) = edited_session();
    let photos: Vec<_> = ids.iter().map(|id| photo_id(&app, *id)).collect();
    let before = all_settings(&app, &ids);
    let selection_before = selected(&app);
    let missing = PhotoId::fresh();
    let invalid = [
        filmstrip_ui::PhotoAction::SyncSettings { source: photos[0], targets: vec![photos[0], missing] },
        filmstrip_ui::PhotoAction::SyncSettings { source: photos[0], targets: vec![photos[0], photos[1], photos[1]] },
        filmstrip_ui::PhotoAction::SyncSettings { source: photos[1], targets: photos[..3].to_vec() },
        filmstrip_ui::PhotoAction::SyncSettings { source: photos[0], targets: vec![photos[1], photos[2]] },
        filmstrip_ui::PhotoAction::ResetSettings(missing),
    ];
    for action in invalid {
        assert!(settings_photo_action(&mut app, &action).is_err(), "invalid context must refuse: {action:?}");
        assert_eq!(all_settings(&app, &ids), before, "an error cannot partially edit any photo");
        assert!(app.raw_filmstrip.group_undo.is_empty());
        assert_eq!((active_document(&app), selected(&app)), (ids[0], selection_before.clone()));
    }
}

/// Optional genuine egui/wgpu renders. Synthetic DNG fixture bytes are generated
/// by raw_app; no native window, personal photograph, or desktop capture is used.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn context_menu_offscreen_snapshots_when_requested() {
    use egui_kittest::kittest::{NodeT, Queryable};

    let Some(destination) = std::env::var_os("PHOTOCRAFT_CONTEXT_SNAPSHOTS") else {
        return;
    };
    let destination = std::path::PathBuf::from(destination);
    std::fs::create_dir_all(&destination).unwrap();
    let mut harness = egui_kittest::Harness::builder().with_size(WINDOW).with_max_steps(64).wgpu().build_eframe(|cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, crate::theme::ThemeKind::Pro);
        let (mut app, dng) = raw_app();
        for name in ["Generated A.dng", "Generated B.dng", "Generated C.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        app
    });
    harness.run_steps(12);
    harness.render().unwrap().save(destination.join("camera-raw-clean-adjustment-panel.png")).unwrap();
    let click_harness = |harness: &mut egui_kittest::Harness<'_, PhotocraftApp>, at: Pos2, button: egui::PointerButton, modifiers| {
        harness.event(egui::Event::ModifiersChanged(modifiers));
        harness.event(egui::Event::PointerMoved(at));
        harness.step();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton { pos: at, button, pressed, modifiers });
            harness.step();
        }
        harness.run_steps(4);
    };
    let cards = harness.state().raw_filmstrip.frame.cards.clone();
    for (_, rect) in cards.iter().skip(1) {
        click_harness(&mut harness, rect.center(), egui::PointerButton::Primary, egui::Modifiers::COMMAND);
    }
    let at = harness.state().raw_filmstrip.frame.cards[1].1.center();
    click_harness(&mut harness, at, egui::PointerButton::Secondary, egui::Modifiers::NONE);
    harness.render().unwrap().save(destination.join("camera-raw-filmstrip-context-menu.png")).unwrap();
    for pressed in [true, false] {
        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: Some(egui::Key::Escape),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
    }
    harness.run_steps(4);
    let at = harness.state().camera_raw.as_ref().unwrap().scope.preview_rect.unwrap().center();
    click_harness(&mut harness, at, egui::PointerButton::Secondary, egui::Modifiers::NONE);
    harness.render().unwrap().save(destination.join("camera-raw-photo-context-menu.png")).unwrap();
    for pressed in [true, false] {
        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: Some(egui::Key::Escape),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
    }
    harness.run_steps(4);
    let filter_buttons: Vec<_> = harness.query_all_by_label("Filter").filter(|node| !node.accesskit_node().is_disabled()).collect();
    assert_eq!(filter_buttons.len(), 1, "the enabled Camera Raw control is distinct from the disabled main Filter menu");
    filter_buttons[0].click();
    harness.run_steps(4);
    harness.get_by_label("≥").click();
    harness.run_steps(4);
    assert!(harness.get_by_label("Rated").rect().width() > 0.0, "the parent remains after choosing a comparison");
    harness.render().unwrap().save(destination.join("camera-raw-filter-menu.png")).unwrap();
}

// The pinned upstream adds dialog-local undo. Retained shoots must keep using
// one group history entry so their previously different adjustments survive.
#[test]
fn context_menu_integration_raw_undo_restores_all_captured_group_values() {
    let (mut app, ctx, ids) = edited_session();
    let before = all_settings(&app, &ids);
    let current = app.camera_raw.as_ref().unwrap().params.clone();
    let mut next = current.clone();
    next.exposure += 0.25;
    let change = edit_actions::slider_group(&app, &current, &next).unwrap();
    apply_group(&mut app, change).unwrap();
    let after = all_settings(&app, &ids);
    assert_ne!(after, before);
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"undo":true}})).unwrap().unwrap();
    assert_eq!(all_settings(&app, &ids), before);
    assert!(app.raw_filmstrip.group_undo.is_empty());
    assert!(app.camera_raw.as_ref().unwrap().undo.is_empty(), "RAW history is not split between active and group stacks");
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"redo":true}})).unwrap().unwrap();
    assert_eq!(all_settings(&app, &ids), after, "redo restores each photograph's distinct values");
    key_mod(&mut app, &ctx, egui::Key::Z, egui::Modifiers::COMMAND);
    assert_eq!(all_settings(&app, &ids), before);
    key_mod(&mut app, &ctx, egui::Key::Z, egui::Modifiers::COMMAND | egui::Modifiers::SHIFT);
    assert_eq!(all_settings(&app, &ids), after);
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"undo":true}})).unwrap().unwrap();
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"set":{"contrast":7}}})).unwrap().unwrap();
    let new_edit = all_settings(&app, &ids);
    assert!(app.raw_filmstrip.group_redo.is_empty(), "a successful new edit invalidates redo");
    assert!(menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"redo":true}})).unwrap().is_err());
    assert_eq!(all_settings(&app, &ids), new_edit);
}

#[test]
fn context_menu_integration_filter_keeps_upstream_local_undo_redo() {
    let (mut app, ctx) = worker_app();
    let before = app.camera_raw.as_ref().unwrap().params.clone();
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"set":{"exposure":0.75}}})).unwrap().unwrap();
    let after = app.camera_raw.as_ref().unwrap().params.clone();
    assert_ne!(after, before);
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"undo":true}})).unwrap().unwrap();
    assert_eq!(app.camera_raw.as_ref().unwrap().params, before);
    menu(&mut app, &ctx, "filter.cameraRaw", &json!({"ui":{"redo":true}})).unwrap().unwrap();
    assert_eq!(app.camera_raw.as_ref().unwrap().params, after);
}

#[test]
fn context_menu_filter_criteria_stay_open_and_preserve_the_retained_shoot() {
    use filmstrip_ui::{ColorFilter, MarkFilter, RatingFilter, StarComparison};

    for mode in ["vertical", "horizontal", "grid"] {
        let (mut app, ctx, ids) = edited_session();
        layout(&mut app, &ctx, mode);
        for (id, stars) in ids.iter().zip([1, 2, 4, 5]) {
            rate(&mut app, *id, PhotoRating::Stars(stars));
        }
        set_view(&mut app, &ctx, json!({"filter":{"stars":{"comparison":">=","value":3}}}));
        let state = |app: &PhotocraftApp| {
            let retained: Vec<_> = filmstrip_raws(app).into_iter().map(|r| (r.photo, r.document, r.rating)).collect();
            (active_document(app), selected(app), all_settings(app, &ids), retained)
        };
        let before = state(&app);
        let row = toolbar_row(&mut app, &ctx, WINDOW);
        let at = row.iter().find(|(name, ..)| name.starts_with("Filter")).unwrap().1.center();
        click(&mut app, &ctx, at);

        // Never reopen the menu between choices: that would hide the nested
        // popup regression, where selecting a criterion displaced its parent.
        let choose_filter = |app: &mut PhotocraftApp, label: &str| {
            choose_context(app, &ctx, label);
            let visible = context_widgets(app, &ctx);
            assert!(egui::Popup::is_any_open(&ctx) && visible.iter().any(|(name, ..)| name == "Rated"), "{mode}: Filter closed after {label}");
            assert_eq!(state(app), before, "{mode}: filtering must preserve the complete shoot, selection and edits");
        };
        for (label, expected) in [("≥", StarComparison::AtLeast(3)), ("≤", StarComparison::AtMost(3)), ("=", StarComparison::Exactly(3))] {
            choose_filter(&mut app, label);
            assert_eq!(app.raw_filmstrip.view.filter.stars, expected, "comparison changes keep the numeric threshold");
        }
        for (label, expected) in [
            ("Red — Select", ColorFilter::Red),
            ("Yellow — Second", ColorFilter::Yellow),
            ("Green — Approved", ColorFilter::Green),
            ("Blue — Review", ColorFilter::Blue),
            ("Purple — To Do", ColorFilter::Purple),
            ("Other label", ColorFilter::Other),
            ("No Label", ColorFilter::None),
            ("Any colour", ColorFilter::Any),
        ] {
            choose_filter(&mut app, label);
            assert_eq!(app.raw_filmstrip.view.filter.color, expected);
        }
        for (label, expected) in [("Marked", MarkFilter::Marked), ("Unmarked", MarkFilter::Unmarked), ("Marked and unmarked", MarkFilter::Any)] {
            choose_filter(&mut app, label);
            assert_eq!(app.raw_filmstrip.view.filter.mark, expected);
        }
        choose_filter(&mut app, "Any");
        assert_eq!(app.raw_filmstrip.view.filter.stars, StarComparison::Any);
        choose_filter(&mut app, "Rated");
        assert!(!app.raw_filmstrip.view.filter.rated);
        choose_context(&mut app, &ctx, "Clear filters");
        assert_eq!(app.raw_filmstrip.view.filter, RatingFilter::default());
        assert!(!egui::Popup::is_any_open(&ctx), "Clear filters closes the menu");
        assert_eq!(state(&app), before);
    }
}
