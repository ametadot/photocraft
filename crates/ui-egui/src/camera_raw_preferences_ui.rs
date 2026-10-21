//! Input suspension shared by the Camera Raw workspace modal.

pub(super) struct BackgroundInput {
    ctx: egui::Context,
    saved: Option<egui::InputState>,
}

impl BackgroundInput {
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
