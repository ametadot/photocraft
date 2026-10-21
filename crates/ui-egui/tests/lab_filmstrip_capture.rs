//! GRW-1 lab capture (not a product test): renders the open-time Camera Raw window with seven synthetic RAWs (five 3:2
//! landscapes, two 2:3 portraits) at the owner's review sizes and UI scales, and writes the PNG plus the Filmstrip and
//! toolbar geometry that `ui.inspect` reports. Synthetic in-memory DNGs only; nothing is read from or written to any
//! photograph or sidecar. Ignored by default (GPU rendering); run with
//! `PHOTOCRAFT_LAB_CAPTURE=<dir> cargo test -p photocraft-ui-egui --test lab_filmstrip_capture -- --ignored --test-threads=1`.
use egui::vec2;
use egui_kittest::Harness;
use photocraft_raw::testgen::{DngSpec, mosaic, scene};
use photocraft_ui_egui::{PhotocraftApp, Services, control, theme::ThemeKind};
use serde_json::{Value, json};

fn dng(w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut spec = DngSpec::cfa(w, h, mosaic(&scene(w, h), w, [0, 1, 1, 2], 0, 30000));
    spec.as_shot_neutral = Some([0.5, 1.0, 0.7]);
    spec.build()
}

fn services() -> Services {
    Services {
        import: Some(Box::new(|name: &str, bytes: &[u8], depth: usize| {
            photocraft_io::import_with_svg_group_depth(name, bytes, depth).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
        })),
        ..Default::default()
    }
}

fn capture(dir: &std::path::Path, tag: &str, size: egui::Vec2, ppp: f32, strip: Value) -> Value {
    // Synthetic fixtures written to the capture's own folder (never a photograph): seven DNGs, five with a sidecar
    // carrying the rating states the Filmstrip must tell apart.
    let fixtures = dir.join("fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    let packet = |attrs: &str| {
        format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:RDF><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" {attrs}/></rdf:RDF></x:xmpmeta>"
        )
    };
    let ratings = [Some("xmp:Rating=\"3\""), Some(""), None, Some("xmp:Rating=\"5\""), Some("xmp:Rating=\"-1\""), Some("xmp:Rating=\"1\""), None];
    let mut paths = Vec::new();
    for (i, (portrait_photo, rating)) in [false, false, true, false, false, true, false].into_iter().zip(ratings).enumerate() {
        let path = fixtures.join(format!("sample_{:04}.dng", 1690 + i));
        std::fs::write(&path, if portrait_photo { dng(128, 192) } else { dng(192, 128) }).unwrap();
        if let Some(attrs) = rating {
            std::fs::write(path.with_extension("xmp"), packet(attrs)).unwrap();
        }
        paths.push(path.to_string_lossy().into_owned());
    }
    let mut h = Harness::builder().with_size(size).with_pixels_per_point(ppp).with_step_dt(1.0 / 60.0).wgpu().build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services());
        for path in &paths {
            app.open_path(path).unwrap();
        }
        app
    });
    h.run_steps(8);
    let (request, _reply) = control::ControlRequest::new("ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"filmstrip": strip}}}));
    let ctx = h.ctx.clone();
    let _ = control::handle(h.state_mut(), &ctx, &request);
    h.run_steps(8);
    let image = h.render().expect("render");
    let png = dir.join(format!("{tag}.png"));
    image.save(&png).unwrap();
    // The Camera Raw continuation's reply describes the review, including the Filmstrip's drawn geometry.
    let (request, _reply) = control::ControlRequest::new("ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"filmstrip": {}}}}));
    let reply = match control::handle(h.state_mut(), &ctx, &request) {
        control::Outcome::Done(value) => value,
        _ => Value::Null,
    };
    let raw = &reply["result"];
    let geometry = json!({
        "tag": tag, "window": [size.x, size.y], "pixelsPerPoint": ppp,
        "filmstripView": raw["filmstripView"], "toolbarRect": raw["filmstripView"]["toolbarRect"], "zoom": raw["zoom"],
        "png": png.file_name().map(|n| n.to_string_lossy().into_owned()),
    });
    std::fs::write(dir.join(format!("{tag}.json")), serde_json::to_string_pretty(&geometry).unwrap()).unwrap();
    geometry
}

#[test]
#[ignore = "GRW-1 lab capture: GPU rendering, run explicitly with PHOTOCRAFT_LAB_CAPTURE"]
fn lab_capture_filmstrip_and_toolbar() {
    let Ok(dir) = std::env::var("PHOTOCRAFT_LAB_CAPTURE") else { return };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut summary = Vec::new();
    for (tag, size, ppp, strip) in [
        ("w1360x900_s100_default", vec2(1360.0, 900.0), 1.0, json!({"visible": true, "orientation": "vertical", "width": 192})),
        ("w1360x900_s100_wide360", vec2(1360.0, 900.0), 1.0, json!({"visible": true, "orientation": "vertical", "width": 360})),
        ("w900x600_s100", vec2(900.0, 600.0), 1.0, json!({"visible": true, "orientation": "vertical", "width": 192})),
        ("w1280x800_s150", vec2(1280.0, 800.0), 1.5, json!({"visible": true, "orientation": "vertical", "width": 192})),
        ("w1920x1080_s100", vec2(1920.0, 1080.0), 1.0, json!({"visible": true, "orientation": "vertical", "width": 192})),
        ("w1360x900_s100_horizontal", vec2(1360.0, 900.0), 1.0, json!({"visible": true, "orientation": "horizontal", "height": 136})),
        // F4 lab S1: sorted by star rating, reversed (5 stars first; unread last).
        (
            "w1360x900_s100_sort_rating_reverse",
            vec2(1360.0, 900.0),
            1.0,
            json!({"visible": true, "orientation": "vertical", "width": 192, "sort": {"by": "starRating", "reverse": true}}),
        ),
        // F4 lab F1: rated photos hidden (the shown one stays, marked); the count row above the cards.
        (
            "w1360x900_s100_filter_hide_rated",
            vec2(1360.0, 900.0),
            1.0,
            json!({"visible": true, "orientation": "vertical", "width": 192, "sort": {"by": "openOrder", "reverse": false}, "filter": {"rated": false}}),
        ),
    ] {
        summary.push(capture(&dir, tag, size, ppp, strip));
    }
    std::fs::write(dir.join("SUMMARY.json"), serde_json::to_string_pretty(&summary).unwrap()).unwrap();
}
