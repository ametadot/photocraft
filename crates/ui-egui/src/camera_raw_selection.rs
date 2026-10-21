//! The Camera Raw Filmstrip's selection model.
//!
//! Which photo is *active* (its preview, sliders and histogram) lives in the dialog and is not
//! stored here. This model only tracks which photos are *selected*, plus the range anchor and the
//! keyboard focus, so stepping the preview never changes the selection and selecting never changes
//! the preview. Identity is the photo's stable `PhotoId` (GRW-1 lab cycle 2, B0): never a list index, a file name or
//! the document id, which changes when Open re-develops a photograph (A1 -> A2) while the photo stays the same.
//!
//! Provisional policy (pending the Adobe reference): the first selection is just the active photo.
//! Open Images and Cancel do not read this model; they still act on every pending photo.

use super::PhotoId;

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Selection {
    selected: Vec<PhotoId>,
    anchor: Option<PhotoId>,
    focus: Option<PhotoId>,
    started: bool,
}

impl Selection {
    pub fn contains(&self, id: PhotoId) -> bool {
        self.selected.contains(&id)
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    /// The complete group, including members hidden by filters. Context actions must
    /// not silently narrow a selected group to the currently visible thumbnails.
    pub fn targets_for(&self, clicked: PhotoId) -> Vec<PhotoId> {
        if self.contains(clicked) { self.selected.clone() } else { vec![clicked] }
    }

    pub fn selected_ids(&self) -> &[PhotoId] {
        &self.selected
    }

    /// A resumed session already has a selection, including a deliberately empty
    /// one. Disable first-draw initialization before asynchronous rows arrive.
    pub fn restore_ids(&mut self, ids: &[PhotoId], anchor: Option<PhotoId>, focus: Option<PhotoId>) {
        self.selected.clear();
        for id in ids {
            if !self.selected.contains(id) {
                self.selected.push(*id);
            }
        }
        self.anchor = anchor.filter(|id| self.selected.contains(id));
        self.focus = focus;
        self.started = true;
    }

    pub fn anchor(&self) -> Option<PhotoId> {
        self.anchor
    }

    pub fn focus(&self) -> Option<PhotoId> {
        self.focus
    }

    /// The selected photos in display order (never in click order). The shell names photographs by their current
    /// document instead (`camera_raw_ui`), so this is the model's own test view.
    #[cfg(test)]
    pub fn members(&self, order: &[PhotoId]) -> Vec<PhotoId> {
        order.iter().copied().filter(|id| self.contains(*id)).collect()
    }

    /// The first time a session is drawn only the active photo is selected.
    pub fn start_with(&mut self, active: Option<PhotoId>) {
        if !self.started {
            self.started = true;
            if let Some(id) = active {
                self.selected = vec![id];
                self.anchor = Some(id);
            }
        }
    }

    pub fn select_only(&mut self, id: PhotoId) {
        self.selected = vec![id];
        self.anchor = Some(id);
    }

    /// Ctrl/Cmd-click: flip one member and leave the rest alone.
    pub fn toggle(&mut self, id: PhotoId) {
        match self.selected.iter().position(|s| *s == id) {
            Some(at) => {
                self.selected.remove(at);
            }
            None => self.selected.push(id),
        }
        self.anchor = Some(id);
    }

    /// Shift-click: everything from the anchor to `id`, inclusive, in display order. The anchor stays.
    pub fn range_to(&mut self, id: PhotoId, order: &[PhotoId]) {
        let Some(to) = order.iter().position(|o| *o == id) else { return };
        let Some(from) = self.anchor.and_then(|a| order.iter().position(|o| *o == a)) else {
            self.select_only(id);
            return;
        };
        let (lo, hi) = (from.min(to), from.max(to));
        self.selected = order.get(lo..=hi).map(<[PhotoId]>::to_vec).unwrap_or_default();
    }

    /// The next selected photo after (`forward`) or before `from` in display order; `None` at the end of the
    /// selection (no wrap) or when `from` is not in the strip. Unselected photos are skipped, never returned.
    pub fn step_from(&self, from: PhotoId, order: &[PhotoId], forward: bool) -> Option<PhotoId> {
        let at = order.iter().position(|id| *id == from)?;
        if forward {
            order.iter().skip(at + 1).find(|id| self.contains(**id)).copied()
        } else {
            order.iter().take(at).rev().find(|id| self.contains(**id)).copied()
        }
    }

    /// The photo to show once the active photo `removed` has left the selection: the nearest selected one below it in
    /// display order, otherwise the nearest selected one above; `None` when nothing is left selected.
    pub fn fallback_after_removal(&self, removed: PhotoId, order: &[PhotoId]) -> Option<PhotoId> {
        self.step_from(removed, order, true).or_else(|| self.step_from(removed, order, false))
    }

    /// Add `id` to the selection if it is not there, leaving the anchor alone (a photo hidden by the Filmstrip filter keeps
    /// its selection, F4 lab F1).
    pub fn include(&mut self, id: PhotoId) {
        if !self.selected.contains(&id) {
            self.selected.push(id);
        }
    }

    pub fn select_all(&mut self, order: &[PhotoId]) {
        self.selected = order.to_vec();
        self.anchor = self.anchor.filter(|a| order.contains(a)).or_else(|| order.first().copied());
    }

    pub fn set_focus(&mut self, id: Option<PhotoId>) {
        self.focus = id;
    }

    /// Forget photos that left the session (Open, Cancel, closed documents).
    pub fn retain_present(&mut self, order: &[PhotoId]) {
        self.selected.retain(|id| order.contains(id));
        self.anchor = self.anchor.filter(|a| order.contains(a));
        self.focus = self.focus.filter(|f| order.contains(f));
    }

    /// A new session starts from scratch.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: u64) -> Vec<PhotoId> {
        (1..=n).map(PhotoId).collect()
    }

    #[test]
    fn a_session_starts_with_only_the_active_photo_selected_and_only_once() {
        let order = ids(3);
        let mut s = Selection::default();
        s.start_with(Some(order[1]));
        assert_eq!((s.members(&order), s.anchor()), (vec![order[1]], Some(order[1])));
        s.toggle(order[2]);
        s.start_with(Some(order[0]));
        assert_eq!(s.members(&order), vec![order[1], order[2]], "a later call never resets the user's selection");
    }

    #[test]
    fn ctrl_click_flips_one_member_and_keeps_the_others() {
        let order = ids(4);
        let mut s = Selection::default();
        s.select_only(order[0]);
        s.toggle(order[2]);
        assert_eq!(s.members(&order), vec![order[0], order[2]]);
        s.toggle(order[2]);
        assert_eq!(s.members(&order), vec![order[0]]);
        s.toggle(order[0]);
        assert_eq!(s.len(), 0, "deselecting the last member is allowed");
    }

    #[test]
    fn shift_click_selects_the_inclusive_range_in_display_order_from_the_anchor() {
        let order = ids(4);
        let mut s = Selection::default();
        s.select_only(order[0]);
        s.range_to(order[2], &order);
        assert_eq!(s.members(&order), vec![order[0], order[1], order[2]]);
        assert_eq!(s.anchor(), Some(order[0]), "the anchor stays put");
        s.range_to(order[1], &order);
        assert_eq!(s.members(&order), vec![order[0], order[1]], "a shorter range replaces the longer one");
        let reversed: Vec<PhotoId> = order.iter().rev().copied().collect();
        s.select_only(order[3]);
        s.range_to(order[1], &reversed);
        assert_eq!(s.members(&reversed), vec![order[3], order[2], order[1]], "ranges follow the order they are given");
    }

    #[test]
    fn a_missing_anchor_selects_just_that_photo_and_an_unknown_target_changes_nothing() {
        let order = ids(3);
        let mut s = Selection::default();
        s.range_to(order[1], &order);
        assert_eq!(s.members(&order), vec![order[1]]);
        s.range_to(PhotoId(99), &order);
        assert_eq!((s.members(&order), s.len()), (vec![order[1]], 1), "a photo that is not in the strip is ignored");
    }

    #[test]
    fn select_all_and_retain_present_follow_the_session() {
        let order = ids(3);
        let mut s = Selection::default();
        s.select_all(&order);
        s.set_focus(Some(order[2]));
        assert_eq!((s.len(), s.anchor(), s.focus()), (3, Some(order[0]), Some(order[2])));
        let after_open: Vec<PhotoId> = vec![order[0]];
        s.retain_present(&after_open);
        assert_eq!((s.members(&after_open), s.anchor(), s.focus()), (vec![order[0]], Some(order[0]), None));
        s.clear();
        assert_eq!(s, Selection::default(), "clear is a new session");
    }

    #[test]
    fn removing_the_active_photo_falls_back_below_then_above_skipping_gaps() {
        let order = ids(5);
        let mut s = Selection::default();
        // [A, B(active), C] -> C
        s.select_all(&order[..3]);
        s.toggle(order[1]);
        assert_eq!(s.fallback_after_removal(order[1], &order), Some(order[2]));
        // [A, B, C(active)] -> B
        let mut s = Selection::default();
        s.select_all(&order[..3]);
        s.toggle(order[2]);
        assert_eq!(s.fallback_after_removal(order[2], &order), Some(order[1]));
        // [A, B(active), C unselected, D] -> D, never C
        let mut s = Selection::default();
        for id in [order[0], order[1], order[3]] {
            s.toggle(id);
        }
        s.toggle(order[1]);
        assert_eq!(s.fallback_after_removal(order[1], &order), Some(order[3]));
        // [A(active), B, C unselected, D] with only A and D: -> D
        let mut s = Selection::default();
        s.toggle(order[0]);
        s.toggle(order[3]);
        s.toggle(order[0]);
        assert_eq!(s.fallback_after_removal(order[0], &order), Some(order[3]));
        // nothing left: no fallback
        s.toggle(order[3]);
        assert_eq!(s.fallback_after_removal(order[3], &order), None);
    }

    #[test]
    fn stepping_through_the_selection_stops_at_the_ends_and_never_wraps() {
        let order = ids(5);
        let mut s = Selection::default();
        for id in [order[0], order[2], order[4]] {
            s.toggle(id);
        }
        assert_eq!(s.step_from(order[2], &order, true), Some(order[4]));
        assert_eq!(s.step_from(order[4], &order, true), None);
        assert_eq!(s.step_from(order[2], &order, false), Some(order[0]));
        assert_eq!(s.step_from(order[0], &order, false), None);
        assert_eq!(s.step_from(order[1], &order, true), Some(order[2]), "an unselected start still finds the next selected one");
        assert_eq!(s.step_from(PhotoId(99), &order, true), None);
        assert_eq!(s.len(), 3, "stepping never changes the selection");
    }

    #[test]
    fn duplicate_display_names_cannot_collide_because_identity_is_the_id() {
        let (a, b) = (PhotoId(7), PhotoId(8));
        let mut s = Selection::default();
        s.select_only(a);
        assert!(s.contains(a) && !s.contains(b));
    }

    #[test]
    fn context_scope_preserves_hidden_selected_members_without_selecting_an_unselected_target() {
        let mut selection = Selection::default();
        selection.select_only(PhotoId(2));
        selection.include(PhotoId(9));
        assert_eq!(selection.targets_for(PhotoId(2)), vec![PhotoId(2), PhotoId(9)]);
        assert_eq!(selection.targets_for(PhotoId(4)), vec![PhotoId(4)]);
        assert_eq!(selection.selected_ids(), [PhotoId(2), PhotoId(9)]);
    }

    #[test]
    fn resumed_empty_selection_is_not_replaced_while_photos_arrive() {
        let mut selection = Selection::default();
        selection.restore_ids(&[], None, None);
        selection.start_with(Some(PhotoId(1)));
        assert_eq!(selection.len(), 0);
        selection.include(PhotoId(3));
        selection.start_with(Some(PhotoId(1)));
        assert_eq!(selection.selected_ids(), [PhotoId(3)]);
    }
}
