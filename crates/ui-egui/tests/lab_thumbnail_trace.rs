//! GRW-1 lab measurement (not a product test): traces the open-time Camera Raw Filmstrip thumbnail from the developed
//! document to the drawn card, and compares it with an area-averaged reduction of the same document at the card's own
//! physical size. Synthetic resolution-chart DNGs only (written to the measurement folder); nothing is read from or
//! written to any photograph or sidecar. Ignored by default (GPU rendering); run with
//! `PHOTOCRAFT_LAB_THUMBS=<dir> cargo test -p photocraft-ui-egui --test lab_thumbnail_trace -- --ignored --test-threads=1`.
use egui::vec2;
use egui_kittest::Harness;
use photocraft_raw::testgen::{DngSpec, mosaic};
use photocraft_ui_egui::{PhotocraftApp, Services, control, theme::ThemeKind};
use serde_json::{Value, json};
use std::time::Instant;

/// A resolution chart: vertical stripes whose period falls from 64 to 2 pixels across the left half, a zone plate on the
/// right half, and four colour patches along the bottom.
fn chart(w: usize, h: usize) -> Vec<[f32; 3]> {
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            if y > h * 7 / 8 {
                return [[0.8, 0.2, 0.2], [0.2, 0.7, 0.25], [0.2, 0.3, 0.8], [0.75, 0.7, 0.2]][x * 4 / w];
            }
            if x < w / 2 {
                let t = x as f32 / (w / 2) as f32;
                let period = 64.0 * (2.0f32 / 64.0).powf(t);
                let on = (x as f32 / period).fract() < 0.5;
                if on { [0.85; 3] } else { [0.08; 3] }
            } else {
                let (cx, cy) = ((x - w / 2) as f32 - w as f32 / 4.0, y as f32 - h as f32 * 7.0 / 16.0);
                let r2 = (cx * cx + cy * cy) / (w as f32 * 0.9);
                let v = 0.5 + 0.4 * r2.cos();
                [v; 3]
            }
        })
        .collect()
}

fn dng(w: usize, h: usize) -> Vec<u8> {
    let mut spec = DngSpec::cfa(w, h, mosaic(&chart(w, h), w, [0, 1, 1, 2], 0, 30000));
    spec.as_shot_neutral = Some([1.0, 1.0, 1.0]);
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

/// Mean absolute Laplacian over the luma of an RGBA8 image (a sharpness figure: higher is more detail).
fn detail(px: &[[u8; 4]], w: usize, h: usize) -> f64 {
    let luma = |x: usize, y: usize| {
        let p = px[y * w + x];
        0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64
    };
    let (mut sum, mut n) = (0.0, 0.0);
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            sum += (4.0 * luma(x, y) - luma(x - 1, y) - luma(x + 1, y) - luma(x, y - 1) - luma(x, y + 1)).abs();
            n += 1.0;
        }
    }
    if n > 0.0 { sum / n } else { 0.0 }
}

/// Peak signal-to-noise ratio (dB) between two RGB images of one size.
fn psnr(a: &[[u8; 4]], b: &[[u8; 4]]) -> f64 {
    let mse = a.iter().zip(b).map(|(p, q)| (0..3).map(|c| (p[c] as f64 - q[c] as f64).powi(2)).sum::<f64>()).sum::<f64>() / (a.len() as f64 * 3.0);
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

fn mean(px: &[[u8; 4]]) -> [f64; 3] {
    let n = px.len().max(1) as f64;
    [0, 1, 2].map(|c| px.iter().map(|p| p[c] as f64).sum::<f64>() / n)
}

/// `w`×`h` pixels from (`x0`, `y0`) of an image read through `pixel`.
fn crop(pixel: &dyn Fn(u32, u32) -> [u8; 4], x0: u32, y0: u32, w: u32, h: u32) -> Vec<[u8; 4]> {
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            out.push(pixel(x, y));
        }
    }
    out
}

/// A binary PPM (no image crate needed here).
fn save_ppm(path: &std::path::Path, w: usize, h: usize, rgba: &[u8]) {
    let mut bytes = format!(
        "P6
{w} {h}
255
"
    )
    .into_bytes();
    for p in rgba.chunks(4) {
        bytes.extend_from_slice(&p[..3]);
    }
    std::fs::write(path, bytes).unwrap();
}

fn trace(dir: &std::path::Path, tag: &str, window: egui::Vec2, ppp: f32, (pw, ph): (usize, usize)) -> Value {
    let fixtures = dir.join("fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    let paths: Vec<String> = (0..2)
        .map(|i| {
            let path = fixtures.join(format!("CHART_{pw}x{ph}_{i}.dng"));
            std::fs::write(&path, dng(pw, ph)).unwrap();
            path.to_string_lossy().into_owned()
        })
        .collect();
    let opened = paths.clone();
    let mut h = Harness::builder().with_size(window).with_pixels_per_point(ppp).with_step_dt(1.0 / 60.0).wgpu().build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services());
        for path in &opened {
            app.open_path(path).unwrap();
        }
        app
    });
    h.run_steps(10);
    let ctx = h.ctx.clone();
    let (request, _) = control::ControlRequest::new(
        "ui.menu.invoke",
        json!({"id": "filter.cameraRaw", "params": {"ui": {"filmstrip": {"visible": true, "orientation": "vertical", "width": 192}}}}),
    );
    let _ = control::handle(h.state_mut(), &ctx, &request);
    // Frames for about 1.5 s, so work the Filmstrip does off the frame (if any) can arrive; on a build without such work
    // the extra frames change nothing.
    for _ in 0..150 {
        h.run_steps(1);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let image = h.render().expect("render");
    image.save(dir.join(format!("{tag}.png"))).unwrap();
    let pixel = |x: u32, y: u32| image.get_pixel(x, y).0;
    let (request, _) = control::ControlRequest::new("ui.menu.invoke", json!({"id": "filter.cameraRaw", "params": {"ui": {"filmstrip": {}}}}));
    let reply = match control::handle(h.state_mut(), &ctx, &request) {
        control::Outcome::Done(value) => value,
        _ => Value::Null,
    };
    let raw = &reply["result"];
    let inspected = control::inspect(h.state(), &ctx)["cameraRaw"].clone();
    let card = raw["filmstripView"]["cards"][0]["rect"].as_array().cloned().unwrap_or_default();
    let card: Vec<f32> = card.iter().map(|v| v.as_f64().unwrap() as f32).collect();
    // The card's picture: inset 4 pt, the card's width less 8 pt, the photo's own aspect (F1).
    let (top, card_w) = (card[1] + 4.0, card[2] - card[0] - 8.0);
    let pic_w_pt = card_w.min(240.0);
    let pic_h_pt = pic_w_pt * ph as f32 / pw as f32;
    let left = card[0] + (card[2] - card[0] - pic_w_pt) / 2.0;
    let (x0, y0, w, hgt) =
        ((left * ppp).ceil() as u32 + 1, (top * ppp).ceil() as u32 + 1, (pic_w_pt * ppp).floor() as u32 - 2, (pic_h_pt * ppp).floor() as u32 - 2);
    let shown = crop(&pixel, x0, y0, w, hgt);
    // The same document, area-averaged to the card's physical size (the main compositor's own reduction).
    let app = h.state();
    let doc_id = raw["filmstrip"][0]["document"].as_u64();
    let state = app.session.documents().iter().find(|st| Some(st.doc.id.0) == doc_id).or_else(|| app.session.documents().first()).unwrap();
    let doc = state.doc.clone();
    let layer = doc.layer(state.active_layer.unwrap()).unwrap();
    let format = layer.surface().map(|s| format!("{:?}", s.format()));
    let started = Instant::now();
    let reduced = photocraft_compose::thumbnail_buffer(&doc, (pic_w_pt * ppp).ceil() as u32);
    let reference = reduced.to_rgba8();
    let reference_ms = started.elapsed().as_secs_f64() * 1000.0;
    // The same reduction converted for the display as the preview is (the managed ideal).
    let display = app.session.color.canvas_display(&doc).ok();
    let managed = display.as_ref().map(|d| d.to_rgba8(&reduced));
    // Reference cropped to the shown area's size (it may be a pixel larger after rounding).
    let (rw, rh) = (reference.width as usize, reference.height as usize);
    let ref_px: Vec<[u8; 4]> = (0..hgt as usize)
        .flat_map(|y| (0..w as usize).map(move |x| (x, y)))
        .map(|(x, y)| {
            let (sx, sy) = ((x + 1).min(rw - 1), (y + 1).min(rh - 1));
            let i = (sy * rw + sx) * 4;
            [reference.pixels[i], reference.pixels[i + 1], reference.pixels[i + 2], 255]
        })
        .collect();
    let managed_px: Option<Vec<[u8; 4]>> = managed.as_ref().map(|m| {
        let (mw, mh) = (m.width as usize, m.height as usize);
        (0..hgt as usize)
            .flat_map(|y| (0..w as usize).map(move |x| (x, y)))
            .map(|(x, y)| {
                let i = (((y + 1).min(mh - 1)) * mw + (x + 1).min(mw - 1)) * 4;
                [m.pixels[i], m.pixels[i + 1], m.pixels[i + 2], 255]
            })
            .collect()
    });
    // The whole fitted preview of the same photo in the same render (the colour-managed display path), for a tone
    // comparison over the same picture.
    let view = inspected["previewRect"].as_array().cloned().unwrap_or_default();
    let preview_mean = if view.len() == 4 {
        let v: Vec<f32> = view.iter().map(|v| v.as_f64().unwrap() as f32 * ppp).collect();
        let (vx, vy) = (v[0].ceil() as u32 + 1, v[1].ceil() as u32 + 1);
        mean(&crop(&pixel, vx, vy, (v[2] - v[0]) as u32 - 3, (v[3] - v[1]) as u32 - 3))
    } else {
        [f64::NAN; 3]
    };
    let started = Instant::now();
    for _ in 0..3 {
        let _ = photocraft_compose::thumbnail_buffer(&doc, 64);
    }
    let compose64_ms = started.elapsed().as_secs_f64() * 1000.0 / 3.0;
    let result = json!({
        "tag": tag, "window": [window.x, window.y], "pixelsPerPoint": ppp,
        "document": {"size": [doc.size.width, doc.size.height], "layerFormat": format},
        // Constants describing main's 64 px layer thumbnail (the BEFORE path), not what this build draws.
        "mainLayerThumbnail": {"texture": [64, 64], "usedForPhoto": [64, (64.0 * ph as f32 / pw as f32).round()], "sampling": "point (one source pixel per thumbnail pixel)"},
        "card": card, "picturePoints": [pic_w_pt, pic_h_pt], "picturePhysical": [w + 2, hgt + 2],
        "upscaleOfMainLayerThumbnail": (pic_w_pt * ppp) / 64.0,
        "detail": {"shown": detail(&shown, w as usize, hgt as usize), "areaAveragedReference": detail(&ref_px, w as usize, hgt as usize)},
        "psnrShownVsReferenceDb": psnr(&shown, &ref_px),
        "psnrShownVsManagedReferenceDb": managed_px.as_ref().map(|m| psnr(&shown, m)),
        "detailManagedReference": managed_px.as_ref().map(|m| detail(m, w as usize, hgt as usize)),
        "meanManagedReference": managed_px.as_ref().map(|m| mean(m)),
        "meanRgb": {"shownThumbnail": mean(&shown), "reference": mean(&ref_px), "previewWhole": preview_mean},
        "timingDebugBuildMs": {"composeAtCardSize": reference_ms, "composeAt64": compose64_ms},
    });
    save_ppm(&dir.join(format!("{tag}_reference.ppm")), rw, rh, &reference.pixels);
    if let Some(m) = &managed {
        save_ppm(&dir.join(format!("{tag}_reference_managed.ppm")), m.width as usize, m.height as usize, &m.pixels);
    }
    std::fs::write(dir.join(format!("{tag}.json")), serde_json::to_string_pretty(&result).unwrap()).unwrap();
    result
}

#[test]
#[ignore = "GRW-1 lab measurement: GPU rendering, run explicitly with PHOTOCRAFT_LAB_THUMBS"]
fn lab_trace_filmstrip_thumbnails() {
    let Ok(dir) = std::env::var("PHOTOCRAFT_LAB_THUMBS") else { return };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut summary = Vec::new();
    for (tag, window, ppp, size) in [
        ("chart_1536x1024_s100", vec2(1360.0, 900.0), 1.0, (1536, 1024)),
        ("chart_1536x1024_s150", vec2(1280.0, 800.0), 1.5, (1536, 1024)),
        ("chart_3072x2048_s100", vec2(1360.0, 900.0), 1.0, (3072, 2048)),
    ] {
        summary.push(trace(&dir, tag, window, ppp, size));
    }
    std::fs::write(dir.join("SUMMARY.json"), serde_json::to_string_pretty(&summary).unwrap()).unwrap();
}
