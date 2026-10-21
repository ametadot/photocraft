use super::*;

fn widgets_frame(app: &mut PhotocraftApp, ctx: &egui::Context, size: egui::Vec2) -> Vec<(String, ERect, bool)> {
    ctx.enable_accesskit();
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| show(app, ui.ctx()));
    output.textures_delta.clear();
    accessible(&output)
}

// Inspect the preferences subtree, not labels painted behind the modal.
fn preferences_widgets(app: &mut PhotocraftApp, ctx: &egui::Context) -> Vec<(String, ERect)> {
    ctx.enable_accesskit();
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| show(app, ui.ctx()));
    output.textures_delta.clear();
    let update = output.platform_output.accesskit_update.as_ref().unwrap();
    let root = egui::Id::new("camera-raw-preferences").accesskit_id();
    assert!(update.nodes.iter().any(|(id, _)| *id == root), "preferences accessibility subtree must exist");
    let mut pending = vec![root];
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    while let Some(id) = pending.pop() {
        assert!(seen.insert(id), "accessibility subtree must not contain a cycle");
        let node = &update.nodes.iter().find(|(candidate, _)| *candidate == id).expect("descendant present").1;
        pending.extend_from_slice(node.children());
        if let (Some(label), Some(bounds)) = (node.label().or_else(|| node.value()), node.bounds()) {
            result.push((label.to_owned(), ERect::from_min_max(pos2(bounds.x0 as f32, bounds.y0 as f32), pos2(bounds.x1 as f32, bounds.y1 as f32))));
        }
    }
    result
}

// This probe uses only helpers and fields present in the frozen integration. It
// can be copied unchanged into its tests module to establish the missing gear.
#[test]
fn preferences_gear_is_accessible_at_top_right() {
    let (mut app, ctx, _) = review_session();
    ctx.enable_accesskit();
    let input = egui::RawInput { screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| show(&mut app, ui.ctx()));
    output.textures_delta.clear();
    let nodes = accessible(&output);
    let (rect, enabled) = widget(&nodes, "Camera Raw Preferences");
    assert!(enabled);
    assert!(rect.left() > WINDOW.x - PANEL_W && rect.right() <= WINDOW.x);
    assert!(rect.top() >= 30.0 && rect.bottom() <= 70.0, "gear belongs above the histogram: {rect:?}");
}

/// Candidate F: the one grey gear opens a menu; its first item opens Preferences.
fn open_gear_menu(app: &mut PhotocraftApp, ctx: &egui::Context) -> Vec<(String, ERect, bool)> {
    let nodes = widgets_frame(app, ctx, WINDOW);
    click(app, ctx, widget(&nodes, "Camera Raw Preferences").0.center());
    frame(app, ctx, WINDOW, vec![]);
    assert!(egui::Popup::is_any_open(ctx), "the gear opens its menu");
    widgets_frame(app, ctx, WINDOW)
}

fn open_preferences(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let nodes = open_gear_menu(app, ctx);
    click(app, ctx, widget(&nodes, "Camera Raw Preferences…").0.center());
    frame(app, ctx, WINDOW, vec![]);
    frame(app, ctx, WINDOW, vec![]);
    assert!(app.camera_raw.as_ref().unwrap().preferences.open);
}

/// B17: one gear and no "Settings" text button. The gear menu reaches Preferences, the
/// XMP settings presets and Before/After; recovery has no shortcut outside Preferences,
/// and its page offers no "Keep in recovery and close".
#[test]
fn one_gear_menu_holds_preferences_presets_and_before_after_only() {
    let (mut app, ctx, _) = review_session();
    let closed = widgets_frame(&mut app, &ctx, WINDOW);
    assert!(!closed.iter().any(|(name, ..)| name == "Settings"), "no Settings text button: {closed:?}");
    let menu = open_gear_menu(&mut app, &ctx);
    for item in ["Camera Raw Preferences…", "Load Settings…", "Save Settings…", "Before (Y)"] {
        assert!(menu.iter().any(|(name, _, enabled)| name == item && *enabled), "{item} in the gear menu: {menu:?}");
    }
    assert!(!menu.iter().any(|(name, ..)| name.starts_with("Long-session recovery")), "no recovery shortcut: {menu:?}");
    key(&mut app, &ctx, egui::Key::Escape);
    assert!(app.camera_raw.is_some(), "Escape closes the gear menu, never the review");
    open_preferences(&mut app, &ctx);
    app.camera_raw.as_mut().unwrap().preferences.page = preferences_ui::Page::LongSessionRecovery;
    app.camera_raw.as_mut().unwrap().preferences.recovery_session = true;
    let page = widgets_frame(&mut app, &ctx, WINDOW);
    assert!(!page.iter().any(|(name, ..)| name.starts_with("Keep in recovery and close")), "{page:?}");
}

fn review_snapshot(app: &PhotocraftApp) -> Value {
    let dialog = app.camera_raw.as_ref().expect("RAW review remains open");
    let docs: Vec<Value> = app
        .session
        .documents()
        .iter()
        .map(|st| {
            json!({
                "id": st.doc.id.0,
                "layer": st.active_layer.map(|id| id.0),
                "history": st.history.past_len(),
                "pixels": st.active_layer.and_then(|id| st.doc.layer(id)).and_then(|layer| layer.surface()).map(crate::surface_fingerprint),
            })
        })
        .collect();
    let raws: Vec<Value> = filmstrip_raws(app)
        .iter()
        .map(|raw| json!({"document":raw.document.0,"photo":raw.photo.0,"name":raw.name,"path":raw.path,"rating":format!("{:?}",raw.rating)}))
        .collect();
    json!({
        "documents":docs,"raws":raws,"active":active_document(app).0,
        "params":dialog.params,"original":dialog.original,"before":dialog.show_before,
        "navigation":app.ui.camera_raw_preview,
        "scope":[app.ui.camera_raw_scope.shadows,app.ui.camera_raw_scope.highlights,app.ui.camera_raw_scope.sampler_tool],
        "samplers":app.ui.camera_raw_scope.samplers,"floatingScope":app.ui.camera_raw_scope.floating,
        "selection":filmstrip_raws(app).iter().filter(|raw|app.raw_filmstrip.selection.contains(raw.photo)).map(|raw|raw.photo.0).collect::<Vec<_>>(),
        "selectionAnchor":app.raw_filmstrip.selection.anchor().map(|id|id.0),
        "view":format!("{:?}",app.raw_filmstrip.view),
        "prefs":app.session.prefs.get().to_json(),"prefsRevision":app.session.prefs.rev(),
        "provisional":app.session.provisional_documents().iter().map(|id|id.0).collect::<Vec<_>>(),
        "batch":app.raw_filmstrip.batch_open_all,"redevelop":app.raw_redevelop.is_some(),
    })
}

#[test]
fn preferences_all_seven_pages_are_selectable_and_saving_information_is_truthful() {
    let (mut app, ctx, _) = review_session();
    open_preferences(&mut app, &ctx);
    let before = review_snapshot(&app);
    for page in preferences_ui::Page::ALL {
        let rect = app.camera_raw.as_ref().unwrap().preferences.frame.pages.iter().find(|(p, _)| *p == page).unwrap().1;
        click(&mut app, &ctx, rect.center());
        let nodes = preferences_widgets(&mut app, &ctx);
        let state = &app.camera_raw.as_ref().unwrap().preferences;
        assert_eq!(state.page, page);
        let body = state.frame.body.unwrap();
        let body_labels: Vec<_> = nodes.iter().filter(|(_, rect)| body.contains(rect.center())).map(|(label, _)| label.as_str()).collect();
        assert!(body_labels.contains(&page.label()), "the current page is identified");
        if page == preferences_ui::Page::LongSessionRecovery {
            assert!(body_labels.contains(&"PhotoCraft default — recovery off"));
            assert!(body_labels.contains(&"Long-session recovery — cache saving"));
            assert!(body_labels.contains(&preferences_ui::CACHE_SAVING_COPY));
            assert!(body_labels.contains(&"Live saving — planned"));
            let all = widgets_frame(&mut app, &ctx, WINDOW);
            assert!(!widget(&all, "Live saving — planned").1);
        } else {
            assert!(!body_labels.contains(&preferences_ui::CACHE_SAVING_COPY), "recovery controls belong only on their dedicated page");
        }
        assert_eq!(review_snapshot(&app), before, "tab navigation must not edit the review");
    }
}

#[test]
fn preferences_ok_cancel_escape_and_reopening_preserve_review() {
    let (mut app, ctx, _) = review_session();
    app.camera_raw.as_mut().unwrap().params.exposure = 1.25;
    for dismissal in ["OK", "Cancel", "Escape"] {
        open_preferences(&mut app, &ctx);
        let before = review_snapshot(&app);
        if dismissal == "Escape" {
            key(&mut app, &ctx, egui::Key::Escape);
        } else {
            let nodes = widgets_frame(&mut app, &ctx, WINDOW);
            let rect = nodes.iter().find(|(name, _, enabled)| name == dismissal && *enabled).unwrap().1;
            click(&mut app, &ctx, rect.center());
        }
        assert!(!app.camera_raw.as_ref().unwrap().preferences.open);
        assert_eq!(review_snapshot(&app), before, "{dismissal} only dismisses preferences");
        frame(&mut app, &ctx, WINDOW, vec![]);
        frame(&mut app, &ctx, WINDOW, vec![]);
    }
    // The icon is a real keyboard-focusable control, including after dismissal.
    let nodes = widgets_frame(&mut app, &ctx, WINDOW);
    let gear = widget(&nodes, "Camera Raw Preferences").0;
    let mut focused = false;
    for _ in 0..64 {
        key(&mut app, &ctx, egui::Key::Tab);
        focused = ctx.memory(|memory| memory.focused()).and_then(|id| ctx.read_response(id)).is_some_and(|response| response.rect == gear);
        if focused {
            break;
        }
    }
    assert!(focused, "Tab must reach the Camera Raw preferences gear");
    key(&mut app, &ctx, egui::Key::Enter);
    assert!(egui::Popup::is_any_open(&ctx), "Enter on the gear opens its menu");
    let nodes = widgets_frame(&mut app, &ctx, WINDOW);
    click(&mut app, &ctx, widget(&nodes, "Camera Raw Preferences…").0.center());
    frame(&mut app, &ctx, WINDOW, vec![]);
    assert!(app.camera_raw.as_ref().unwrap().preferences.open, "the gear menu reopens preferences");
}

#[test]
fn preferences_block_background_shortcuts_and_dismiss_frame_keys() {
    let (mut app, ctx, ids) = review_session();
    click_card(&mut app, &ctx, ids[0]);
    open_preferences(&mut app, &ctx);
    let before = review_snapshot(&app);
    for shortcut in
        [egui::Key::Y, egui::Key::H, egui::Key::Z, egui::Key::S, egui::Key::U, egui::Key::O, egui::Key::ArrowDown, egui::Key::Space, egui::Key::Enter]
    {
        key(&mut app, &ctx, shortcut);
        assert_eq!(review_snapshot(&app), before, "shortcut leaked: {shortcut:?}");
        if !app.camera_raw.as_ref().unwrap().preferences.open {
            open_preferences(&mut app, &ctx);
        }
    }
    let press = |key| egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
    frame(&mut app, &ctx, WINDOW, vec![press(egui::Key::Escape), press(egui::Key::Y), press(egui::Key::Enter)]);
    assert!(!app.camera_raw.as_ref().unwrap().preferences.open);
    assert_eq!(review_snapshot(&app), before, "simultaneous dismissal keys belong to preferences");
    let release = |key| egui::Event::Key { key, physical_key: Some(key), pressed: false, repeat: false, modifiers: egui::Modifiers::NONE };
    frame(&mut app, &ctx, WINDOW, vec![release(egui::Key::Escape), release(egui::Key::Y), release(egui::Key::Enter)]);
    open_preferences(&mut app, &ctx);
    frame(&mut app, &ctx, WINDOW, vec![press(egui::Key::H)]);
    frame(&mut app, &ctx, WINDOW, vec![press(egui::Key::Escape)]);
    frame(&mut app, &ctx, WINDOW, vec![release(egui::Key::H), release(egui::Key::Escape)]);
    assert_eq!(review_snapshot(&app), before, "a key held across dismissal must not be replayed");
}

#[test]
fn preferences_block_pointer_scroll_drag_and_underlying_accept_cancel() {
    let (mut app, ctx, ids) = review_session();
    app.ui.camera_raw_scope.floating = true;
    frame(&mut app, &ctx, WINDOW, vec![]);
    frame(&mut app, &ctx, WINDOW, vec![]);
    let footer_nodes = widgets_frame(&mut app, &ctx, WINDOW);
    let accept = widget(&footer_nodes, "Open Images").0.center();
    let cancel = widget(&footer_nodes, "Cancel").0.center();
    let target = card(&app, ids[1]).center();
    let preview = app.camera_raw.as_ref().unwrap().viewport.unwrap().left_center() + vec2(8.0, 0.0);
    open_preferences(&mut app, &ctx);
    let before = review_snapshot(&app);
    for at in [accept, cancel, target, preview] {
        click(&mut app, &ctx, at);
        assert!(app.camera_raw.as_ref().unwrap().preferences.open, "outside click must not dismiss preferences");
        assert_eq!(review_snapshot(&app), before, "click leaked at {at:?}");
    }
    drag(&mut app, &ctx, target, target + vec2(100.0, 150.0));
    let space =
        egui::Event::Key { key: egui::Key::Space, physical_key: Some(egui::Key::Space), pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
    frame_mod(
        &mut app,
        &ctx,
        WINDOW,
        vec![
            space,
            egui::Event::PointerMoved(preview),
            button(preview, true),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0.0, 120.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::CTRL,
            },
        ],
        egui::Modifiers::CTRL,
    );
    frame_mod(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(preview + vec2(70.0, 70.0))], egui::Modifiers::CTRL);
    frame(&mut app, &ctx, WINDOW, vec![button(preview + vec2(70.0, 70.0), false)]);
    assert_eq!(review_snapshot(&app), before, "held modifiers/Space, dragging and wheel must not alter background");
}

#[test]
fn preferences_opening_frame_owns_simultaneous_shortcuts() {
    let (mut app, ctx, _) = review_session();
    let nodes = open_gear_menu(&mut app, &ctx);
    let at = widget(&nodes, "Camera Raw Preferences…").0.center();
    frame(&mut app, &ctx, WINDOW, vec![egui::Event::PointerMoved(at)]);
    frame(&mut app, &ctx, WINDOW, vec![button(at, true)]);
    let before = review_snapshot(&app);
    frame(
        &mut app,
        &ctx,
        WINDOW,
        vec![
            button(at, false),
            egui::Event::Key { key: egui::Key::Y, physical_key: Some(egui::Key::Y), pressed: true, repeat: false, modifiers: egui::Modifiers::NONE },
        ],
    );
    assert!(app.camera_raw.as_ref().unwrap().preferences.open);
    assert_eq!(review_snapshot(&app), before);
}

#[test]
fn preferences_native_parent_close_only_dismisses_preferences() {
    let (mut app, ctx, _) = review_session();
    open_preferences(&mut app, &ctx);
    // The synthetic backend has no separate native input surface. Establish the
    // visible child in embedded mode, then exercise show()'s native-close route.
    ctx.set_embed_viewports(false);
    let before = review_snapshot(&app);
    let mut input = egui::RawInput {
        screen_rect: Some(ERect::from_min_size(Pos2::ZERO, WINDOW)),
        events: vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: Some(egui::Key::Escape),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
        ..Default::default()
    };
    input.viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    let mut output = ctx.run_ui(input, |ui| show(&mut app, ui.ctx()));
    output.textures_delta.clear();
    assert!(output.viewport_output.values().any(|viewport| viewport.commands.contains(&egui::ViewportCommand::CancelClose)));
    assert!(!app.camera_raw.as_ref().unwrap().preferences.open);
    assert_eq!(review_snapshot(&app), before);
    let mut input = egui::RawInput::default();
    input.viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    ctx.run_ui(input, |ui| show(&mut app, ui.ctx())).textures_delta.clear();
    assert!(app.camera_raw.is_none(), "a later parent close retains ordinary review cancellation");
}

#[test]
fn preferences_shell_fits_compact_viewports_and_themes() {
    for theme in crate::theme::ThemeKind::ALL {
        let (mut app, ctx, _) = review_session();
        PhotocraftApp::setup_context(&ctx, theme);
        app.camera_raw.as_mut().unwrap().preferences.open = true;
        let size = vec2(900.0, 600.0);
        for _ in 0..3 {
            frame(&mut app, &ctx, size, vec![]);
        }
        let nodes = widgets_frame(&mut app, &ctx, size);
        let state = &app.camera_raw.as_ref().unwrap().preferences;
        let screen = ERect::from_min_size(Pos2::ZERO, size);
        let window = state.frame.window.unwrap();
        assert!(screen.contains_rect(window), "{theme:?}: {window:?}");
        assert!(state.frame.body.unwrap().width() > 400.0);
        for (_, rect) in &state.frame.pages {
            assert!(window.contains_rect(*rect), "tab clipped in {theme:?}");
        }
        assert_eq!(state.frame.pages.len(), 7);
        for name in ["OK", "Cancel"] {
            let rect = nodes.iter().find(|(label, _, enabled)| label == name && *enabled).unwrap().1;
            assert!(window.contains_rect(rect), "button clipped in {theme:?}: {rect:?}");
        }
    }
}

#[test]
#[ignore = "GPU visual capture; run explicitly with PHOTOCRAFT_PREFERENCES_CAPTURES set to a disposable output folder"]
fn preferences_capture_synthetic_pages() {
    let dir = std::path::PathBuf::from(std::env::var_os("PHOTOCRAFT_PREFERENCES_CAPTURES").expect("explicit capture directory required"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SCOPE.txt"), "Offscreen egui/wgpu rendering of synthetic DNG test data. No desktop application, live PhotoCraft instance, window picker, keyboard, mouse or desktop screenshot was used. These images demonstrate rendered controls and layout; real-file persistence and application interaction remain separate evidence.\n").unwrap();
    for (size_tag, size) in [("1360x900", WINDOW), ("900x600", vec2(900.0, 600.0))] {
        let (mut app, dng) = raw_app();
        for name in ["Synthetic_01.dng", "Synthetic_02.dng", "Synthetic_03.dng"] {
            app.open_bytes(name, &dng).unwrap();
        }
        let render_state = egui_kittest::wgpu::create_render_state(egui_kittest::wgpu::default_wgpu_setup(), eframe::egui_wgpu::RendererOptions::PREDICTABLE);
        let adapter = render_state.adapter.get_info();
        std::fs::write(dir.join(format!("{size_tag}_renderer.txt")), format!("{adapter:#?}\n")).unwrap();
        let renderer = egui_kittest::wgpu::WgpuTestRenderer::from_render_state(render_state);
        let mut initialized = false;
        let mut harness = egui_kittest::Harness::builder().with_size(size).renderer(renderer).build_ui_state(
            |ui, app| {
                if !initialized {
                    PhotocraftApp::setup_context(ui.ctx(), crate::theme::ThemeKind::Pro);
                    initialized = true;
                    return; // Font definitions take effect on the next frame.
                }
                show(app, ui.ctx());
            },
            app,
        );
        for theme in crate::theme::ThemeKind::ALL {
            PhotocraftApp::setup_context(&harness.ctx, theme);
            harness.run_steps(4);
            harness.state_mut().camera_raw.as_mut().unwrap().preferences.open = false;
            harness.run_steps(3);
            harness.render().expect("synthetic gear capture").save(dir.join(format!("{size_tag}_{theme:?}_gear.png"))).unwrap();
            let ids = filmstrip_raws(harness.state()).iter().map(|raw| raw.photo).collect::<Vec<_>>();
            harness.state_mut().raw_filmstrip.selection.select_all(&ids);
            harness.state_mut().jobs.opens.push(crate::jobs_ui::OpenTab { job: photocraft_engine::jobs::JobId(999), name: "Synthetic_pending.dng".into(), path: None, slot: None, order: None });
            harness.run_steps(3);
            harness.render().expect("loading dialog capture").save(dir.join(format!("{size_tag}_{theme:?}_loading.png"))).unwrap();
            harness.state_mut().jobs.opens.clear();
            for (state_tag, orientation, grid) in [
                ("vertical_group", filmstrip_ui::Orientation::Vertical, false),
                ("horizontal_group", filmstrip_ui::Orientation::Horizontal, false),
                ("grid_group", filmstrip_ui::Orientation::Vertical, true),
            ] {
                harness.state_mut().raw_filmstrip.view.orientation = orientation;
                harness.state_mut().raw_filmstrip.view.grid = grid;
                harness.run_steps(3);
                let at = harness.state().raw_filmstrip.frame.cards[0].1.center();
                harness.event(egui::Event::PointerMoved(at));
                harness.run_steps(2);
                harness.render().expect("synthetic group layout capture").save(dir.join(format!("{size_tag}_{theme:?}_{state_tag}.png"))).unwrap();
            }
            harness.state_mut().raw_filmstrip.view.filter.stars = filmstrip_ui::StarComparison::Exactly(5);
            harness.run_steps(3);
            harness.render().expect("synthetic empty-filter capture").save(dir.join(format!("{size_tag}_{theme:?}_no_matching_photos.png"))).unwrap();
            harness.state_mut().raw_filmstrip.view.filter = Default::default();
            harness.state_mut().raw_filmstrip.view.grid = false;
            harness.state_mut().raw_filmstrip.view.orientation = filmstrip_ui::Orientation::Vertical;
            harness.run_steps(3);
            let gear = harness.state().camera_raw.as_ref().unwrap().preferences.gear.unwrap().center();
            for event in [egui::Event::PointerMoved(gear), button(gear, true), button(gear, false)] {
                harness.event(event);
                harness.run_steps(1);
            }
            harness.run_steps(2);
            harness.render().expect("synthetic gear menu capture").save(dir.join(format!("{size_tag}_{theme:?}_gear_menu.png"))).unwrap();
            egui::Popup::close_all(&harness.ctx);
            harness.state_mut().camera_raw.as_mut().unwrap().preferences.open = true;
            for page in [preferences_ui::Page::General, preferences_ui::Page::FileHandling, preferences_ui::Page::LongSessionRecovery] {
                harness.state_mut().camera_raw.as_mut().unwrap().preferences.page = page;
                harness.run_steps(3);
                assert!(harness.state().camera_raw.as_ref().unwrap().preferences.open);
                harness.render().expect("synthetic preferences capture").save(dir.join(format!("{size_tag}_{theme:?}_{page:?}.png"))).unwrap();
            }
        }
    }
}

#[test]
fn normal_footer_has_only_three_equal_secondary_actions_and_no_recovery_status() {
    let (mut app, ctx, _) = review_session();
    let nodes = widgets_frame(&mut app, &ctx, WINDOW);
    let cancel = widget(&nodes, "Cancel").0;
    let done = widget(&nodes, "Done").0;
    assert_eq!(cancel.size(), done.size());
    let footer: Vec<_> = nodes.iter().filter(|(_, r, _)| r.center().y >= WINDOW.y - 48.0).map(|(s, _, _)| s.as_str()).collect();
    assert!(footer.contains(&"Open Images") && footer.contains(&"Cancel") && footer.contains(&"Done"));
    for removed in ["Save edits", "Recovery details", "Load Settings…", "Save Settings…", "Recovery cache unavailable"] {
        assert!(!footer.contains(&removed), "{removed} should not be in the footer");
    }
}

#[test]
fn opening_batch_only_shows_centred_loading_dialog_until_ready() {
    let (mut app, dng) = raw_app();
    let ctx = egui::Context::default();
    PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::Pro);
    app.open_bytes("first.dng", &dng).unwrap();
    app.jobs.opens.push(crate::jobs_ui::OpenTab { job: photocraft_engine::jobs::JobId(123), name: "pending.dng".into(), path: None, slot: None, order: None });
    for _ in 0..3 { widgets_frame(&mut app, &ctx, WINDOW); }
    let nodes = widgets_frame(&mut app, &ctx, WINDOW);
    assert!(nodes.iter().any(|(s,_,_)| s == "Opening photographs"));
    assert!(hides_unaccepted_raw(&app), "main document tabs remain hidden");
    assert!(app.camera_raw.is_none(), "settings must not appear during loading");
    for removed in ["Camera Raw session", "Done", "Open Images", "Recovery details"] {
        assert!(!nodes.iter().any(|(s,_,_)| s == removed));
    }
    let cancel = widget(&nodes, "Cancel").0;
    assert!((cancel.center().x - WINDOW.x/2.0).abs() < 180.0);
    assert!((cancel.center().y - WINDOW.y/2.0).abs() < 120.0);
    app.jobs.opens.clear();
    widgets_frame(&mut app, &ctx, WINDOW);
    assert!(app.camera_raw.is_some(), "ready batch enters the workspace");
}
