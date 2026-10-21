//! Camera Raw preferences. Saving-mode information describes the session controller;
//! opening or changing pages never mutates a recipe or writes a sidecar.

use egui::{Align, Layout, Rect, Stroke, pos2, vec2};

use crate::{theme::Tokens, widgets};

pub(super) const CACHE_SAVING_COPY: &str =
    "When cache saving is enabled, your Camera Raw changes are saved for recovery in the background. Done and Open Images save adjustments to XMP sidecars.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Page {
    #[default]
    General,
    FileHandling,
    Performance,
    Defaults,
    Workflow,
    GenerativeAi,
    LongSessionRecovery,
}

impl Page {
    pub(super) const ALL: [Self; 7] =
        [Self::General, Self::FileHandling, Self::Performance, Self::Defaults, Self::Workflow, Self::GenerativeAi, Self::LongSessionRecovery];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::General => tl!("General"),
            Self::FileHandling => tl!("File Handling"),
            Self::Performance => tl!("Performance"),
            Self::Defaults => tl!("Defaults"),
            Self::Workflow => tl!("Workflow"),
            Self::GenerativeAi => tl!("Generative AI"),
            Self::LongSessionRecovery => tl!("Long-session recovery"),
        }
    }
}

#[derive(Default)]
pub(super) struct Preferences {
    pub open: bool,
    pub page: Page,
    pub gear: Option<Rect>,
    pub block_frame: bool,
    pub recovery_action: Option<&'static str>,
    pub recovery_session: bool,
    draft: Option<photocraft_engine::prefs::RawDefaults>,
    #[cfg(test)]
    pub frame: Frame,
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct Frame {
    pub window: Option<Rect>,
    pub body: Option<Rect>,
    pub pages: Vec<(Page, Rect)>,
}

impl Preferences {
    /// Closing Camera Raw's native parent first dismisses this child modal. The
    /// closing frame still belongs to preferences, including simultaneous keys.
    pub(super) fn handle_native_close(&mut self, ctx: &egui::Context) -> bool {
        if !self.open {
            return false;
        }
        self.open = false;
        self.draft = None;
        self.block_frame = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        true
    }
}

pub(super) struct BackgroundInput {
    ctx: egui::Context,
    saved: Option<egui::InputState>,
}

impl BackgroundInput {
    pub(super) fn block(ctx: &egui::Context) -> Self {
        Self::suspend(ctx, false)
    }

    /// A persistence modal is drawn after the workspace. Hold its input aside
    /// while drawing the workspace, then restore it for the modal itself.
    pub(super) fn before_overlay(ctx: &egui::Context) -> Self {
        Self::suspend(ctx, true)
    }

    fn suspend(ctx: &egui::Context, restore_events: bool) -> Self {
        let saved = ctx.input_mut(|input| {
            let mut saved = input.clone();
            if !restore_events {
                saved.events.clear();
            }
            input.events.clear();
            input.keys_down.clear();
            input.modifiers = egui::Modifiers::NONE;
            input.pointer = Default::default();
            input.smooth_scroll_delta = egui::Vec2::ZERO;
            saved
        });
        Self { ctx: ctx.clone(), saved: Some(saved) }
    }
}

impl Drop for BackgroundInput {
    fn drop(&mut self) {
        // Preferences consume their dismissal events; a later persistence
        // overlay receives the original events after the workspace is drawn.
        if let Some(saved) = self.saved.take() {
            self.ctx.input_mut(|input| *input = saved);
        }
    }
}

pub(super) fn show(
    ctx: &egui::Context,
    state: &mut Preferences,
    current: &photocraft_engine::prefs::RawDefaults,
) -> Option<photocraft_engine::prefs::RawDefaults> {
    let draft = state.draft.get_or_insert_with(|| current.clone());
    let t = Tokens::get(ctx);
    // Keep the complete shell inside the viewport, including smaller embedded windows.
    let size = (ctx.content_rect().size() - vec2(48.0, 48.0)).min(vec2(920.0, 680.0)).max(vec2(1.0, 1.0));
    let mut dismiss = false;
    let mut accept = false;
    #[cfg(test)]
    {
        state.frame = Frame::default();
    }
    let response = egui::Modal::new(egui::Id::new("camera-raw-preferences"))
        .backdrop_color(t.scrim)
        .frame(egui::Frame::NONE.fill(t.dock).stroke(Stroke::new(1.0, t.card_border)).corner_radius(t.radius_lg).inner_margin(12.0))
        .show(ctx, |ui| {
            ui.set_min_size((size - vec2(24.0, 24.0)).max(vec2(1.0, 1.0)));
            ui.set_max_size((size - vec2(24.0, 24.0)).max(vec2(1.0, 1.0)));
            ui.label(egui::RichText::new(tl!("Camera Raw Preferences")).font(crate::theme::semibold(14.0)).color(t.text));
            ui.add_space(8.0);
            widgets::hairline(ui);
            ui.add_space(12.0);
            let available = ui.available_rect_before_wrap();
            let body_height = (available.height() - 44.0).max(1.0);
            let sidebar_width = 176.0_f32.min(available.width() * 0.45);
            let sidebar = Rect::from_min_size(available.min, vec2(sidebar_width, body_height));
            let body = Rect::from_min_max(pos2(sidebar.right() + 16.0, available.top()), pos2(available.right(), sidebar.bottom()));
            ui.painter().rect_filled(sidebar, t.radius_sm, t.chrome);
            ui.painter().rect_stroke(sidebar, t.radius_sm, Stroke::new(1.0, t.card_border), egui::StrokeKind::Inside);
            let mut tabs = ui.new_child(egui::UiBuilder::new().max_rect(sidebar.shrink(5.0)).layout(Layout::top_down(Align::Min)));
            tabs.set_clip_rect(sidebar.intersect(ui.clip_rect()));
            egui::ScrollArea::vertical().id_salt("camera-raw-preferences-pages").show(&mut tabs, |ui| {
                for page in Page::ALL {
                    let label = page.label();
                    let selected = state.page == page;
                    let mut text = egui::RichText::new(label);
                    if selected && t.bevel {
                        // Classic's global black text override otherwise hides the
                        // selected label against its navy selection background.
                        text = text.color(t.accent_text);
                    }
                    let row = ui.add_sized(vec2(ui.available_width(), 28.0), egui::Button::selectable(selected, text).wrap());
                    if row.clicked() {
                        state.page = page;
                    }
                    #[cfg(test)]
                    state.frame.pages.push((page, row.rect));
                }
            });
            let mut content = ui.new_child(egui::UiBuilder::new().max_rect(body).layout(Layout::top_down(Align::Min)));
            content.set_clip_rect(body.intersect(ui.clip_rect()));
            egui::ScrollArea::vertical().id_salt("camera-raw-preferences-content").show(&mut content, |ui| {
                ui.label(egui::RichText::new(state.page.label()).strong());
                ui.add_space(12.0);
                match state.page {
                    Page::LongSessionRecovery => {
                        use photocraft_engine::prefs::RawRecoveryMode;
                        ui.radio_value(&mut draft.recovery_mode, RawRecoveryMode::Off, tl!("PhotoCraft default — recovery off"));
                        ui.radio_value(&mut draft.recovery_mode, RawRecoveryMode::Cache, tl!("Long-session recovery — cache saving"));
                        ui.add_enabled(false, egui::RadioButton::new(false, tl!("Live saving — planned")))
                            .on_disabled_hover_text(tl!("Live save is planned for a later release."));
                        ui.add_space(12.0);
                        ui.label(tl!("Recovery is off by default. Turn on cache saving to protect a long editing session in the background."));
                        ui.label(tl!(
                            "Turning recovery off stops automatic checkpoints. Earlier recovered work remains until you finish it or discard its recovered copy."
                        ));
                        ui.label(tl!(CACHE_SAVING_COPY));
                        ui.label(tl!(
                            "Recovery is offered after an interruption or when you choose Keep for recovery. Done and Open Images retire completed sessions. Discarding changes to a resumed session preserves its earlier recovered work."
                        ));
                        ui.add_enabled_ui(draft.recovery_mode == RawRecoveryMode::Cache, |ui| {
                            ui.checkbox(&mut draft.show_recovery_progress, tl!("Show recovery progress in Camera Raw"));
                        });
                        ui.add_space(12.0);
                        ui.label(tl!("Done and Open Images save supported edits to XMP sidecars first. Original photographs remain unchanged."));
                        if state.recovery_session
                            && current.recovery_mode == RawRecoveryMode::Cache
                            && draft.recovery_mode == RawRecoveryMode::Cache
                            && ui.button(tl!("Recovery details…")).clicked()
                        {
                            state.recovery_action = Some("recovery-details");
                            accept = true;
                            dismiss = true;
                        }
                    }
                    Page::FileHandling => {
                        ui.label(egui::RichText::new(tl!("JPEG and TIFF Handling")).strong());
                        handling(ui, tl!("JPEG handling"), &mut draft.jpeg_handling);
                        handling(ui, tl!("TIFF handling"), &mut draft.tiff_handling);
                        ui.label(tl!("Layered TIFF files open in the editor with their layers preserved."));
                        ui.label(tl!("Camera Raw saves XMP sidecars for JPEG and TIFF; original image files remain unchanged."));
                    }
                    Page::General => {
                        ui.label(tl!("Change the filmstrip size, orientation and grid view in Camera Raw. These display choices do not edit photographs."));
                    }
                    _ => {}
                }
            });
            #[cfg(test)]
            {
                state.frame.body = Some(body);
            }
            let footer = Rect::from_min_max(pos2(available.left(), sidebar.bottom() + 12.0), available.max);
            let mut buttons = ui.new_child(egui::UiBuilder::new().max_rect(footer).layout(Layout::right_to_left(Align::Center)));
            let chosen = widgets::dialog_buttons(
                &mut buttons,
                &[
                    widgets::DialogButton::new(widgets::ButtonRole::Default, tl!("OK"), 90.0),
                    widgets::DialogButton::new(widgets::ButtonRole::Cancel, tl!("Cancel"), 90.0),
                ],
            );
            accept |= chosen == Some(widgets::ButtonRole::Default);
            dismiss |= chosen.is_some();
        });
    #[cfg(test)]
    {
        state.frame.window = Some(response.response.rect);
    }
    // Outside clicks leave preferences open. Escape belongs exclusively to this
    // modal; Enter may activate a focused tab/button but never accepts a RAW.
    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) || dismiss {
        state.open = false;
        response.response.ctx.request_repaint();
        let draft = state.draft.take();
        return if accept { draft.filter(|draft| draft != current) } else { None };
    }
    None
}

fn handling(ui: &mut egui::Ui, label: &str, value: &mut photocraft_engine::prefs::RenderedRawHandling) {
    use photocraft_engine::prefs::RenderedRawHandling as Mode;
    let text = |mode| match mode {
        Mode::Disabled => tl!("Disable Camera Raw support"),
        Mode::ExistingSettings => tl!("Open files with Camera Raw settings"),
        Mode::All => tl!("Open all supported files"),
    };
    ui.label(label);
    egui::ComboBox::from_id_salt(label).selected_text(text(*value)).width(ui.available_width().min(370.0)).show_ui(ui, |ui| {
        for mode in [Mode::Disabled, Mode::ExistingSettings, Mode::All] {
            ui.selectable_value(value, mode, text(mode));
        }
    });
    ui.add_space(8.0);
}
