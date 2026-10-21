//! Camera Raw navigation in source-pixel coordinates. No document edits.
use egui::{Key, Modifiers, PointerButton, Pos2, Rect, Response, Sense, Ui, Vec2, pos2, vec2};
use serde_json::Value;

use crate::{state::CameraRawPreviewState, widgets};

const MIN_ZOOM: f32 = 0.0001;
const MAX_ZOOM: f32 = 16.0;

impl CameraRawPreviewState {
    pub(crate) fn scale(&self, viewport: Rect, size: Vec2) -> f32 {
        self.zoom
            .filter(|z| z.is_finite() && *z > 0.0)
            .map(|z| z.clamp(MIN_ZOOM, MAX_ZOOM))
            .unwrap_or_else(|| (viewport.width().max(1.0) / size.x.max(f32::EPSILON)).min(viewport.height().max(1.0) / size.y.max(f32::EPSILON)).min(MAX_ZOOM))
    }

    pub(crate) fn image_rect(&mut self, viewport: Rect, size: Vec2) -> Rect {
        let extent = size.max(Vec2::splat(f32::EPSILON)) * self.scale(viewport, size);
        for (center, (image, visible)) in self.center.iter_mut().zip([(extent.x, viewport.width()), (extent.y, viewport.height())]) {
            let margin = (visible.max(1.0) / image.max(1.0) * 0.5).min(0.5);
            *center = if center.is_finite() { center.clamp(margin, 1.0 - margin) } else { 0.5 };
        }
        Rect::from_min_size(viewport.center() - vec2(self.center[0] * extent.x, self.center[1] * extent.y), extent)
    }

    pub(crate) fn zoom_at(&mut self, zoom: f32, at: Pos2, viewport: Rect, size: Vec2) {
        if !zoom.is_finite() || !at.x.is_finite() || !at.y.is_finite() {
            return;
        }
        let old = self.image_rect(viewport, size);
        let point = (at - old.min) / old.size();
        self.zoom = Some(zoom.clamp(MIN_ZOOM, MAX_ZOOM));
        let extent = size.max(Vec2::splat(f32::EPSILON)) * self.scale(viewport, size);
        let center = point - (at - viewport.center()) / extent;
        self.center = [center.x, center.y];
        self.image_rect(viewport, size);
    }

    pub(crate) fn pan(&mut self, delta: Vec2, viewport: Rect, size: Vec2) {
        if !delta.x.is_finite() || !delta.y.is_finite() {
            return;
        }
        let rect = self.image_rect(viewport, size);
        // Leaving fit mode on a hand drag would make later window resizes stop fitting.
        self.center[0] -= delta.x / rect.width();
        self.center[1] -= delta.y / rect.height();
        self.image_rect(viewport, size);
    }

    pub(crate) fn fit(&mut self) {
        self.zoom = None;
        self.center = [0.5, 0.5];
    }
}

/// Validate with the rest of the Camera Raw request before changing anything.
pub(crate) fn prepare(current: CameraRawPreviewState, value: &Value) -> Result<CameraRawPreviewState, String> {
    let obj = value.as_object().ok_or("Camera Raw view must be an object")?;
    let mut next = current;
    for (key, value) in obj {
        match key.as_str() {
            "zoom" if value.as_str() == Some("fit") => next.fit(),
            "zoom" => {
                let z = value
                    .as_f64()
                    .filter(|z| z.is_finite() && (MIN_ZOOM as f64..=MAX_ZOOM as f64).contains(z))
                    .ok_or("Camera Raw zoom must be 'fit' or 0.0001–16 (1 = 100%)")?;
                next.zoom = Some(z as f32);
            }
            "center" => {
                let a = value.as_array().filter(|a| a.len() == 2).ok_or("Camera Raw center must be [u,v]")?;
                let coordinate = |i| {
                    a.get(i)
                        .and_then(Value::as_f64)
                        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                        .map(|v| v as f32)
                        .ok_or("Camera Raw center must be finite coordinates in 0–1")
                };
                next.center = [coordinate(0)?, coordinate(1)?];
            }
            "hand" => next.hand = value.as_bool().ok_or("Camera Raw hand must be a boolean")?,
            _ => return Err(format!("unknown Camera Raw view property {key}")),
        }
    }
    if obj.get("zoom").and_then(Value::as_str) == Some("fit") {
        next.fit();
    }
    Ok(next)
}

#[derive(Clone, Copy)]
struct ZoomCapture {
    start: Pos2,
    image: Rect,
    box_zoom: bool,
}

/// One capture shared with the colour sampler. Space/middle drag always pans; S takes
/// precedence over the persistent Zoom/Hand tool, so a sampler click never zooms.
pub(crate) fn interact(
    ui: &mut Ui,
    state: &mut CameraRawPreviewState,
    viewport: Rect,
    size: Vec2,
    sampler: &mut bool,
    reset_settings: &mut bool,
) -> (Rect, Response, bool, Option<Rect>) {
    let sampling = *sampler;
    let response = ui.interact(viewport, ui.id().with("cr-sample-preview"), Sense::click_and_drag());
    let modifiers = ui.input(|i| i.modifiers);
    let temporary_zoom = modifiers.command && (state.hand || sampling);
    let hand = !temporary_zoom && (state.hand && !sampling || ui.input(|i| i.key_down(Key::Space)));
    let zoom_tool = temporary_zoom || !hand && !sampling;
    let panning = hand || response.dragged_by(PointerButton::Middle);
    let at = response.hover_pos().unwrap_or(viewport.center());
    let rect = state.image_rect(viewport, size);
    // Reuse the canvas wheel policy: retain the modifiers of the wheel event while egui
    // smooths it, even if Alt is released before the final part of the notch arrives.
    let cr_hovered = response.hovered();
    let wheel = crate::wheel_nav::read(ui.ctx(), false, true, ui.id().with("cr-wheel"), cr_hovered);
    if cr_hovered {
        ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        match wheel {
            Some(crate::wheel_nav::Wheel::Zoom(factor)) => state.zoom_at(state.scale(viewport, size) * factor, at, viewport, size),
            Some(crate::wheel_nav::Wheel::SnapZoom(n)) => {
                let dir = if n > 0 { 1 } else { -1 };
                let mut nz = state.scale(viewport, size);
                for _ in 0..n.unsigned_abs() {
                    nz = crate::zoom_levels::step_in_range(nz, dir, MIN_ZOOM, MAX_ZOOM);
                }
                if nz != state.scale(viewport, size) {
                    state.zoom_at(nz, at, viewport, size);
                }
            }
            Some(crate::wheel_nav::Wheel::Pan(delta)) => state.pan(delta, viewport, size),
            None => {}
        }
        response.clone().on_hover_cursor(if hand {
            crate::tool_cursor::hand(ui.ctx(), false)
        } else if !zoom_tool {
            egui::CursorIcon::Crosshair
        } else {
            crate::tool_cursor::zoom(ui.ctx(), modifiers.alt)
        });
    }
    let capture_id = response.id.with("zoom-capture");
    if response.drag_started_by(PointerButton::Primary) && zoom_tool {
        let start = ui.input(|i| i.pointer.press_origin()).unwrap_or(at);
        if rect.contains(start) {
            ui.ctx().data_mut(|data| data.insert_temp(capture_id, ZoomCapture { start, image: rect, box_zoom: modifiers.command && !temporary_zoom }));
        }
    }
    let capture = ui.ctx().data(|data| data.get_temp::<ZoomCapture>(capture_id));
    let mut zoom_box = None;
    if panning && (response.dragged_by(PointerButton::Primary) || response.dragged_by(PointerButton::Middle)) {
        state.pan(response.drag_delta(), viewport, size);
        ui.ctx().set_cursor_icon(crate::tool_cursor::hand(ui.ctx(), true));
    } else if response.dragged_by(PointerButton::Primary)
        && let Some(capture) = capture
    {
        if capture.box_zoom {
            if let Some(end) = response.interact_pointer_pos() {
                let region = Rect::from_two_pos(capture.start, end).intersect(viewport);
                zoom_box = Some(region);
            }
        } else {
            // The anchor is the initial press, not the moving pointer; reversing a scrub
            // follows the same detail instead of drifting across the image.
            state.zoom_at(state.scale(viewport, size) * (response.drag_delta().x * 0.01).clamp(-1.0, 1.0).exp(), capture.start, viewport, size);
        }
    } else if response.double_clicked() && hand {
        state.fit();
    } else if response.clicked() && zoom_tool && rect.contains(at) {
        if modifiers.command && modifiers.shift {
            state.zoom_at(1.0, at, viewport, size);
        } else if modifiers.alt {
            state.zoom_at(state.scale(viewport, size) / 1.25, at, viewport, size);
        } else if state.zoom.is_none() {
            state.zoom_at(1.0, at, viewport, size);
        } else {
            state.fit();
        }
    }
    if response.drag_stopped_by(PointerButton::Primary) {
        if let Some(capture) = capture.filter(|c| c.box_zoom)
            && let Some(end) = response.interact_pointer_pos()
        {
            let region = Rect::from_two_pos(capture.start, end).intersect(viewport);
            if region.width() >= 4.0 && region.height() >= 4.0 {
                let point = (region.center() - capture.image.min) / capture.image.size();
                state.zoom = Some(
                    (capture.image.width() / size.x * (viewport.width() / region.width()).min(viewport.height() / region.height())).clamp(MIN_ZOOM, MAX_ZOOM),
                );
                state.center = [point.x, point.y];
            }
        }
        ui.ctx().data_mut(|data| data.remove::<ZoomCapture>(capture_id));
    }
    let reset_target_id = response.id.with("reset-photo");
    if response.secondary_clicked() {
        let on_photo = response.interact_pointer_pos().is_some_and(|at| rect.contains(at));
        ui.ctx().data_mut(|data| data.insert_temp(reset_target_id, on_photo));
    }
    response.context_menu(|ui| {
        if ui.ctx().data(|data| data.get_temp::<bool>(reset_target_id)).unwrap_or(false) {
            if ui.button(tl!("Reset Settings")).clicked() {
                *reset_settings = true;
                ui.close();
            }
            ui.separator();
        }
        if ui.button(tl!("Fit in View")).clicked() {
            state.fit();
            ui.close();
        }
        if ui.button(tl!("100%")).clicked() {
            state.zoom_at(1.0, at, viewport, size);
            ui.close();
        }
        ui.menu_button(tl!("Zoom Tool"), |ui| {
            for percent in [25.0, 50.0, 100.0, 200.0, 400.0, 800.0, 1600.0] {
                if ui.button(format!("{percent:.0}%")).clicked() {
                    state.zoom_at(percent / 100.0, at, viewport, size);
                    ui.close();
                }
            }
        });
    });
    if !ui.ctx().text_edit_focused() {
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::COMMAND | Modifiers::ALT, Key::Num0) {
                state.zoom_at(1.0, viewport.center(), viewport, size);
            } else if i.consume_key(Modifiers::COMMAND, Key::Num0) {
                state.fit();
            }
            if i.consume_key(Modifiers::COMMAND, Key::Plus) || i.consume_key(Modifiers::COMMAND, Key::Equals) {
                state.zoom_at(state.scale(viewport, size) * 1.25, viewport.center(), viewport, size);
            }
            if i.consume_key(Modifiers::COMMAND, Key::Minus) {
                state.zoom_at(state.scale(viewport, size) / 1.25, viewport.center(), viewport, size);
            }
            if i.consume_key(Modifiers::NONE, Key::H) {
                state.hand = true;
                *sampler = false;
            }
            if i.consume_key(Modifiers::NONE, Key::Z) {
                state.hand = false;
                *sampler = false;
            }
        });
    }
    (state.image_rect(viewport, size), response, panning || temporary_zoom, zoom_box)
}

/// GRW-1 lab cycle 4 (F3): the preview toolbar in the owner's order of 15 Oct,
/// `[actual zoom] [100%] [zoom presets ▾] | [Filmstrip] [Grid] [Sort] [Filter]`, then the Zoom and Hand tools (not part
/// of that order) at the row's right end. Every control has a slot fixed before anything is drawn, so the Filmstrip
/// control can be hit-tested before the layout it changes and a click reclaims the width in its own frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ToolbarSlots {
    pub readout: Rect,
    /// The readout has room for "Fit (x%)"; otherwise it shows the percentage alone (its accessible name and tooltip keep
    /// the whole text).
    pub readout_full: bool,
    pub quick: Rect,
    pub presets: Rect,
    /// With a Filmstrip: the divider's x, then the Filmstrip control, Grid, Sort and Filter.
    pub filmstrip: Option<(f32, [Rect; 4])>,
    /// Between the groups and the tools: room for the detail spinner or notice (may be empty).
    pub status: Rect,
    pub zoom_tool: Rect,
    pub hand_tool: Rect,
}

impl ToolbarSlots {
    /// Everything the toolbar occupies.
    pub(crate) fn extent(&self) -> Rect {
        self.readout.union(self.zoom_tool).union(self.hand_tool)
    }
}

/// A text button's height, and an icon control's square.
const SLOT_H: f32 = 28.0;
pub(crate) const TOOL: f32 = 26.0;
/// Between two groups, and between two icons of a group.
const GROUP_GAP: f32 = 4.0;
const ICON_GAP: f32 = 2.0;
/// The divider before the Filmstrip group, including its margins.
const DIVIDER: f32 = 9.0;
const PRESETS_W: f32 = 22.0;
/// The zoom presets, after Fit in View (the readout's own click also fits).
const PRESETS: [(f32, &str); 12] = [
    (0.0625, "6.25%"),
    (0.125, "12.5%"),
    (0.25, "25%"),
    (1.0 / 3.0, "33.3%"),
    (0.5, "50%"),
    (2.0 / 3.0, "66.7%"),
    (1.0, "100%"),
    (2.0, "200%"),
    (3.0, "300%"),
    (4.0, "400%"),
    (8.0, "800%"),
    (16.0, "1600%"),
];

/// Where each control goes in `row`; `readout` is the readout's (full, short) width and `quick` the 100% button's. The
/// readout falls back to its short form only when the row cannot hold the full one.
pub(crate) fn toolbar_slots(row: Rect, readout: (f32, f32), quick: f32, filmstrip: bool) -> ToolbarSlots {
    let slot = |x: f32, w: f32, h: f32| Rect::from_min_size(pos2(x, row.center().y - h / 2.0), vec2(w, h));
    let group = if filmstrip { DIVIDER + 4.0 * TOOL + 3.0 * ICON_GAP } else { 0.0 };
    let tools = 2.0 * TOOL + ICON_GAP;
    let rest = GROUP_GAP + quick + GROUP_GAP + PRESETS_W + group + 2.0 * GROUP_GAP + tools;
    let readout_full = readout.0 + rest <= row.width();
    let readout = slot(row.left(), if readout_full { readout.0 } else { readout.1 }, SLOT_H);
    let quick = slot(readout.right() + GROUP_GAP, quick, SLOT_H);
    let presets = slot(quick.right() + GROUP_GAP, PRESETS_W, TOOL);
    let mut x = presets.right();
    let filmstrip = filmstrip.then(|| {
        let divider = x + DIVIDER / 2.0;
        let rects = [0.0, 1.0, 2.0, 3.0].map(|i| slot(x + DIVIDER + i * (TOOL + ICON_GAP), TOOL, TOOL));
        x = rects[3].right();
        (divider, rects)
    });
    // The tools keep to the right end, but never overlap the groups in a row too short for both.
    let zoom_tool = slot((row.right() - tools).max(x + 2.0 * GROUP_GAP), TOOL, TOOL);
    let hand_tool = slot(zoom_tool.right() + ICON_GAP, TOOL, TOOL);
    let status = Rect::from_min_max(pos2(x + GROUP_GAP, row.top()), pos2((zoom_tool.left() - GROUP_GAP).max(x + GROUP_GAP), row.bottom()));
    ToolbarSlots { readout, readout_full, quick, presets, filmstrip, status, zoom_tool, hand_tool }
}

/// The readout's (full, short) widths and the 100% button's, in the current language and fonts. The readout is sized for
/// its widest value, so it never jitters as the zoom changes.
pub(crate) fn toolbar_widths(ui: &Ui) -> ((f32, f32), f32) {
    let widest = "1888.8%";
    let full = widgets::button_width(ui, &[&crate::i18n::fmt(tl!("Fit ({zoom})"), &[("zoom", widest)])]);
    ((full, widgets::button_width(ui, &[widest])), widgets::button_width(ui, &[tl!("100%")]).max(52.0))
}

/// The actual display scale in words: "Fit (x%)" while fitted (when `full`), else the percentage. A whole percentage has no
/// decimals, an exact two-decimal one (the 6.25% preset) keeps both, and anything else keeps one, so 66.7% is never 67%.
pub(crate) fn readout_text(state: &CameraRawPreviewState, scale: f32, full: bool) -> String {
    let percent = scale * 100.0;
    let near = |v: f32| (v - v.round()).abs() < 0.01;
    let text = if (percent - percent.round()).abs() < 0.05 {
        format!("{percent:.0}%")
    } else if near(percent * 100.0) && !near(percent * 10.0) {
        format!("{percent:.2}%")
    } else {
        format!("{percent:.1}%")
    };
    if state.zoom.is_none() && full { crate::i18n::fmt(tl!("Fit ({zoom})"), &[("zoom", &text)]) } else { text }
}

/// A child for one slot. Every slot has its own id, so no two controls share widget ids.
fn in_slot<R>(ui: &mut Ui, rect: Rect, salt: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(salt).max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
    add(&mut child)
}

/// The zoom group and the tools in their slots (the Filmstrip group is drawn by the Filmstrip). Nothing here edits the
/// photo: every control changes the preview's navigation only.
/// `notice` is a status the row had no room to show (it joins the readout's tooltip).
pub(crate) fn toolbar(
    ui: &mut Ui,
    slots: &ToolbarSlots,
    state: &mut CameraRawPreviewState,
    viewport: Rect,
    size: Vec2,
    sampler: &mut bool,
    notice: Option<&str>,
) {
    let scale = state.scale(viewport, size);
    // The actual zoom: always the display scale now, never a stale preset; a click fits the photo.
    let whole = readout_text(state, scale, true);
    let name = format!("{}: {whole}", tl!("Zoom"));
    let shown = readout_text(state, scale, slots.readout_full);
    let readout = in_slot(ui, slots.readout, "cr-zoom-readout", |ui| widgets::secondary_button(ui, &shown, slots.readout.width()));
    readout.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), &name));
    let tip = match notice {
        Some(notice) => format!("{whole}\n{}\n\n{notice}", tl!("Fit in View")),
        None => format!("{whole}\n{}", tl!("Fit in View")),
    };
    if readout.on_hover_text(tip).clicked() {
        state.fit();
    }
    // One image pixel per physical pixel.
    if in_slot(ui, slots.quick, "cr-zoom-100", |ui| widgets::secondary_button(ui, tl!("100%"), slots.quick.width())).clicked() {
        state.zoom_at(1.0, viewport.center(), viewport, size);
    }
    let presets =
        in_slot(ui, slots.presets, "cr-zoom-presets", |ui| crate::icons::button_with_icon_size(ui, "chevron-down", PRESETS_W, 14.0, false, tl!("Zoom")));
    egui::Popup::menu(&presets).show(|ui| {
        if ui.add(egui::Button::selectable(state.zoom.is_none(), tl!("Fit in View"))).clicked() {
            state.fit();
            ui.close();
        }
        for (zoom, label) in PRESETS {
            let checked = state.zoom.is_some_and(|z| (z - zoom).abs() < 1e-4);
            if ui.add(egui::Button::selectable(checked, label)).clicked() {
                state.zoom_at(zoom, viewport.center(), viewport, size);
                ui.close();
            }
        }
    });
    let zoom_tool = in_slot(ui, slots.zoom_tool, "cr-zoom-tool", |ui| crate::icons::button(ui, "zoom-in", TOOL, !state.hand && !*sampler, tl!("Zoom Tool")));
    zoom_tool.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tl!("Zoom Tool")));
    if zoom_tool.double_clicked() {
        state.fit();
    }
    if zoom_tool.clicked() {
        state.hand = false;
        *sampler = false;
    }
    let hand_tool = in_slot(ui, slots.hand_tool, "cr-hand-tool", |ui| crate::icons::button(ui, "hand", TOOL, state.hand && !*sampler, tl!("Hand Tool")));
    hand_tool.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tl!("Hand Tool")));
    if hand_tool.double_clicked() {
        state.fit();
    }
    if hand_tool.clicked() {
        state.hand = true;
        *sampler = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;
    #[test]
    fn zoom_preserves_the_point_under_the_pointer_and_pan_clamps() {
        let viewport = Rect::from_min_size(pos2(10.0, 20.0), vec2(800.0, 600.0));
        let size = vec2(1600.0, 1200.0);
        let mut view = CameraRawPreviewState::default();
        let at = pos2(310.0, 270.0);
        let initial = view.image_rect(viewport, size);
        let point = (at - initial.min) / initial.size();
        view.zoom_at(1.0, at, viewport, size);
        let enlarged = view.image_rect(viewport, size);
        assert!(((at - enlarged.min) / enlarged.size() - point).length() < 1e-5);
        view.pan(vec2(1e9, -1e9), viewport, size);
        let panned = view.image_rect(viewport, size);
        assert!(panned.contains_rect(viewport));
        view.fit();
        assert_eq!(view.image_rect(viewport, size), initial);
    }
    #[test]
    fn shift_alt_wheel_snaps_zoom_in_camera_raw() {
        let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let size = vec2(1600.0, 1200.0);
        let at = pos2(400.0, 300.0);
        let mut state = CameraRawPreviewState { zoom: Some(1.0), center: [0.5, 0.5], hand: false };
        let scale_before = state.scale(viewport, size);
        let n = 1i32;
        let dir = if n > 0 { 1 } else { -1 };
        let mut nz = state.scale(viewport, size);
        for _ in 0..n.unsigned_abs() {
            nz = crate::zoom_levels::step_in_range(nz, dir, MIN_ZOOM, MAX_ZOOM);
        }
        assert_ne!(nz, scale_before, "zoom-in steps to a new level");
        state.zoom_at(nz, at, viewport, size);
        assert_eq!(state.scale(viewport, size), nz, "zoom applied");
        assert!(nz > scale_before, "zoomed in");
        let scale_after_in = state.scale(viewport, size);
        let mut nz = state.scale(viewport, size);
        nz = crate::zoom_levels::step_in_range(nz, -1, MIN_ZOOM, MAX_ZOOM);
        state.zoom_at(nz, at, viewport, size);
        assert!(state.scale(viewport, size) < scale_after_in, "zoomed out");
    }

    #[test]
    fn shift_alt_snap_at_limits_does_not_move() {
        let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let size = vec2(1600.0, 1200.0);
        let at = viewport.center();
        let state = CameraRawPreviewState { zoom: Some(MAX_ZOOM), center: [0.5, 0.5], hand: false };
        let nz = crate::zoom_levels::step_in_range(state.scale(viewport, size), 1, MIN_ZOOM, MAX_ZOOM);
        assert_eq!(nz, state.scale(viewport, size), "at max, step up stays put");
        let state_lo = CameraRawPreviewState { zoom: Some(MIN_ZOOM), center: [0.5, 0.5], hand: false };
        let nz = crate::zoom_levels::step_in_range(state_lo.scale(viewport, size), -1, MIN_ZOOM, MAX_ZOOM);
        assert_eq!(nz, state_lo.scale(viewport, size), "at min, step down stays put");
        let _ = at;
    }

    #[test]
    fn snap_zoom_from_fit_enters_explicit_zoom() {
        let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let size = vec2(1600.0, 1200.0);
        let at = viewport.center();
        let mut state = CameraRawPreviewState::default();
        assert!(state.zoom.is_none(), "starts in fit mode");
        let fit_scale = state.scale(viewport, size);
        let nz = crate::zoom_levels::step_in_range(fit_scale, 1, MIN_ZOOM, MAX_ZOOM);
        if nz != fit_scale {
            state.zoom_at(nz, at, viewport, size);
        }
        assert!(state.zoom.is_some(), "snap zoom leaves fit mode");
    }

    #[test]
    fn snap_zoom_preserves_point_under_pointer() {
        let viewport = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let size = vec2(1600.0, 1200.0);
        let at = pos2(300.0, 200.0);
        let mut state = CameraRawPreviewState { zoom: Some(1.0), center: [0.5, 0.5], hand: false };
        let rect_before = state.image_rect(viewport, size);
        let point_before = (at - rect_before.min) / rect_before.size();
        let nz = crate::zoom_levels::step_in_range(1.0, 1, MIN_ZOOM, MAX_ZOOM);
        state.zoom_at(nz, at, viewport, size);
        let rect_after = state.image_rect(viewport, size);
        let point_after = (at - rect_after.min) / rect_after.size();
        assert!((point_before - point_after).length() < 0.01, "point preserved: {point_before:?} vs {point_after:?}");
    }

    #[test]
    fn tiny_images_nonfinite_state_and_resize_stay_centered_and_finite() {
        let viewport = Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0));
        let size = vec2(40.0, 20.0);
        let mut view = CameraRawPreviewState { zoom: Some(f32::NAN), center: [f32::INFINITY, f32::NAN], hand: true };
        assert!(view.image_rect(viewport, size).is_finite());
        view.zoom_at(1.0, pos2(100.0, 100.0), viewport, size);
        assert_eq!(view.image_rect(viewport, size).center(), viewport.center());
        let before = view;
        view.zoom_at(f32::INFINITY, viewport.center(), viewport, size);
        view.pan(vec2(f32::NAN, 1.0), viewport, size);
        assert_eq!(view, before);
    }

    /// F3 (lab cycle 4): the slots keep the owner's order and never overlap, the tools keep to the right end, and only the
    /// readout compacts, and only when the row cannot hold its full text.
    #[test]
    fn toolbar_slots_keep_the_owner_order_and_compact_only_the_readout() {
        let row = Rect::from_min_size(pos2(10.0, 0.0), vec2(600.0, 36.0));
        let s = toolbar_slots(row, (110.0, 70.0), 56.0, true);
        let (divider, [toggle, grid, sort, filter]) = s.filmstrip.unwrap();
        let order = [s.readout, s.quick, s.presets, toggle, grid, sort, filter, s.zoom_tool, s.hand_tool];
        assert!(order.windows(2).all(|w| w[0].right() <= w[1].left()), "{order:?}");
        assert!(order.iter().all(|r| row.contains_rect(*r)), "{order:?}");
        assert!(s.presets.right() < divider && divider < toggle.left());
        assert!(s.readout_full && s.readout.width() == 110.0);
        assert_eq!(s.hand_tool.right(), row.right());
        assert!(s.status.left() >= filter.right() && s.status.right() <= s.zoom_tool.left());
        // The narrowest row that holds the full readout keeps it; one point less shows the short form, in order.
        let need = s.readout.width() + (s.hand_tool.right() - s.quick.left()) - (s.zoom_tool.left() - filter.right()) + 4.0 + 2.0 * 4.0;
        let tight = |w: f32| toolbar_slots(Rect::from_min_size(row.min, vec2(w, 36.0)), (110.0, 70.0), 56.0, true);
        assert!(tight(need).readout_full, "{need}");
        let short = tight(need - 1.0);
        assert!(!short.readout_full && short.readout.width() == 70.0);
        let (_, [_, _, _, filter]) = short.filmstrip.unwrap();
        assert!(filter.right() <= short.zoom_tool.left() && short.hand_tool.right() <= row.left() + need - 1.0);
        // Without a Filmstrip there is no Filmstrip group.
        assert!(toolbar_slots(row, (110.0, 70.0), 56.0, false).filmstrip.is_none());
    }

    /// F3: the readout tells the actual scale; "Fit (x%)" only while fitted; a fraction is never rounded to a whole.
    #[test]
    fn the_readout_says_fit_only_while_fitted_and_keeps_fractions() {
        let mut state = CameraRawPreviewState::default();
        assert_eq!(readout_text(&state, 0.157, true), "Fit (15.7%)");
        assert_eq!(readout_text(&state, 0.157, false), "15.7%");
        assert_eq!(readout_text(&state, 16.0, true), "Fit (1600%)");
        state.zoom = Some(1.0);
        assert_eq!(readout_text(&state, 1.0, true), "100%");
        assert_eq!(readout_text(&state, 2.0 / 3.0, true), "66.7%");
        assert_eq!(readout_text(&state, 0.0625, true), "6.25%");
        assert_eq!(readout_text(&state, 0.125, true), "12.5%");
    }
}
