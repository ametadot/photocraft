//! The open-time Camera Raw Filmstrip: view state, safe geometry and thumbnail cards.
//!
//! Visibility, orientation and extent are separate (`FilmstripView`): a hidden strip owns no
//! rectangle, splitter or gutter, and the width and height the user chose survive hiding,
//! orientation changes and narrow windows. Which photos exist and which is active stays in
//! `camera_raw_ui`, and which are selected stays in the selection model; this file draws and
//! reports intent.

use egui::{Align2, Color32, FontId, Key, Modifiers, Rect, Response, Sense, Stroke, TextureId, Ui, Vec2, WidgetInfo, WidgetType, pos2, vec2};
use photocraft_doc::DocId;

use super::{PhotoId, PhotoRating};

use super::selection_ui::Selection;
use crate::theme::Tokens;
use crate::widgets;

pub(super) const DEFAULT_WIDTH: f32 = 176.0;
pub(super) const MIN_WIDTH: f32 = 128.0;
/// The compact default is 176 pt; the user may widen the strip up to this (limited by the window).
pub(super) const MAX_WIDTH: f32 = 360.0;
pub(super) const DEFAULT_HEIGHT: f32 = 136.0;
pub(super) const MIN_HEIGHT: f32 = 104.0;
pub(super) const MAX_HEIGHT: f32 = 260.0;
/// The narrowest central preview the strip may leave: the preview toolbar (F3, lab cycle 4: zoom group, Filmstrip group,
/// tools) needs about 387 pt with its full English readout, plus its 16 pt margins. At the 900 pt minimum window the strip
/// is then 150 pt; a longer readout falls back to the percentage alone.
const MIN_PREVIEW_WIDTH: f32 = 420.0;
/// The shortest central preview a horizontal strip may leave (image area plus toolbar and margins).
const MIN_PREVIEW_HEIGHT: f32 = 300.0;
/// The narrowest preview column in which a horizontal strip is still useful.
const MIN_STRIP_ACROSS: f32 = 240.0;
/// Hit target across the divider. A real hand needs more than the 8 pt first used.
const SPLITTER_HIT: f32 = 12.0;
const KEY_STEP: f32 = 16.0;
/// Card anatomy (GRW-1 lab cycle 3, F1; owner addendum of 15 Oct): a small inset, the picture at its own aspect (no
/// fixed square slot), one filename row, one status row (crop, five stars, adjustments) outside the picture, a small
/// inset. Nothing else is reserved, so a card is as tall as its content.
const INSET: f32 = 4.0;
const ROW_GAP: f32 = 2.0;
const NAME_ROW: f32 = 14.0;
const STATUS_ROW: f32 = 14.0;
/// Between two cards.
const CARD_GAP: f32 = 4.0;
/// Inside the strip, around the cards.
const STRIP_PAD: f32 = 6.0;
/// Kept free along the strip's scrolling edge for the scroll bar, which widens when hovered.
const SCROLL_RESERVE: f32 = 10.0;
/// A picture is at most this tall for its width (a 4:3 box), so a portrait stays compact; a landscape keeps its aspect.
const MAX_PICTURE_ASPECT: f32 = 0.75;
/// The flattest picture for its width (very wide panoramas).
const MIN_PICTURE_ASPECT: f32 = 0.25;
/// The aspect assumed before a photo's size is known (a 3:2 landscape).
const DEFAULT_PICTURE_ASPECT: f32 = 2.0 / 3.0;
/// A vertical card's picture is at most this wide: a wider strip leaves a margin beside the cards instead of making
/// every card taller (owner priority: density).
pub(super) const MAX_PICTURE_WIDTH: f32 = 240.0;
/// One star of the rating row, and the crop and adjustments glyphs.
const STAR_PITCH: f32 = 11.0;
const STAR: f32 = 10.0;
const ICON: f32 = 12.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum Orientation {
    /// A strip left of the preview.
    #[default]
    Vertical,
    /// A strip under the preview (never under the settings panel).
    Horizontal,
}

/// Presentation preferences only: nothing here touches a photo's settings.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct FilmstripView {
    pub visible: bool,
    /// Grid shares membership, ordering and selection with the filmstrip.
    pub grid: bool,
    pub orientation: Orientation,
    /// The width the user chose; the drawn width is clamped by the window, this is not.
    pub preferred_width: f32,
    /// The height the user chose for a horizontal strip, kept apart from the width.
    pub preferred_height: f32,
    pub show_filenames: bool,
    pub show_edit_badges: bool,
    /// How the cards are ordered (F4 lab S1): the strip's visual order only.
    pub sort: SortKey,
    /// The order is reversed (Adobe's "Reverse Order").
    pub reverse: bool,
    /// Which ratings the strip shows (F4 lab F1).
    pub filter: RatingFilter,
}

/// Which ratings the Filmstrip shows (F4 lab F1). Hiding is visual only: a hidden photo stays in the review, Open Images
/// still opens it, and its selection does not change while it is hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct RatingFilter {
    pub rated: bool,
    pub unrated: bool,
    pub rejected: bool,
    pub unread: bool,
    pub stars: StarComparison,
    pub color: ColorFilter,
    pub mark: MarkFilter,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum StarComparison {
    #[default]
    Any,
    AtLeast(u8),
    AtMost(u8),
    Exactly(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum ColorFilter {
    #[default]
    Any,
    None,
    Red,
    Yellow,
    Green,
    Blue,
    Purple,
    Other,
}

impl ColorFilter {
    pub(super) fn from_label(label: &str) -> Self {
        match label {
            "" => Self::None,
            "Select" | "Red" => Self::Red,
            "Second" | "Yellow" => Self::Yellow,
            "Approved" | "Green" => Self::Green,
            "Review" | "Blue" => Self::Blue,
            "To Do" | "Purple" => Self::Purple,
            _ => Self::Other,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Any => tl!("Any colour"),
            Self::None => tl!("No Label"),
            Self::Red => tl!("Red — Select"),
            Self::Yellow => tl!("Yellow — Second"),
            Self::Green => tl!("Green — Approved"),
            Self::Blue => tl!("Blue — Review"),
            Self::Purple => tl!("Purple — To Do"),
            Self::Other => tl!("Other label"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum MarkFilter {
    #[default]
    Any,
    Marked,
    Unmarked,
}

impl Default for RatingFilter {
    fn default() -> Self {
        Self { rated: true, unrated: true, rejected: true, unread: true, stars: StarComparison::Any, color: ColorFilter::Any, mark: MarkFilter::Any }
    }
}

impl RatingFilter {
    pub(super) fn shows(self, rating: PhotoRating) -> bool {
        let category = match rating {
            PhotoRating::Stars(_) => self.rated,
            PhotoRating::Unrated => self.unrated,
            PhotoRating::Rejected => self.rejected,
            PhotoRating::Unknown => self.unread,
        };
        let count = match rating {
            PhotoRating::Stars(n) => Some(n),
            PhotoRating::Unrated => Some(0),
            PhotoRating::Rejected | PhotoRating::Unknown => None,
        };
        category
            && match self.stars {
                StarComparison::Any => true,
                StarComparison::AtLeast(n) => count.is_some_and(|v| v >= n),
                StarComparison::AtMost(n) => count.is_some_and(|v| v <= n),
                StarComparison::Exactly(n) => count == Some(n),
            }
    }

    pub(super) fn shows_metadata(self, rating: PhotoRating, label: &str, marked: bool) -> bool {
        self.shows(rating)
            && (self.color == ColorFilter::Any || self.color == ColorFilter::from_label(label))
            && match self.mark {
                MarkFilter::Any => true,
                MarkFilter::Marked => marked,
                MarkFilter::Unmarked => !marked,
            }
    }

    pub(super) fn is_default(self) -> bool {
        self == Self::default()
    }
}

/// The photos the filter hides this frame (F4 lab F1): how many, and which of them are selected.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Hidden {
    pub count: usize,
    pub selected: Vec<PhotoId>,
}

/// The count row: "3 hidden" or "3 hidden, 1 selected".
pub(super) fn hidden_text(hidden: &Hidden) -> String {
    let n = hidden.count.to_string();
    if hidden.selected.is_empty() {
        crate::i18n::fmt(tl!("{n} hidden"), &[("n", &n)])
    } else {
        crate::i18n::fmt(tl!("{n} hidden, {m} selected"), &[("n", &n), ("m", &hidden.selected.len().to_string())])
    }
}

/// What the Filmstrip orders its cards by (F4 lab S1). Visual only: photos, documents, the selection, the shown photo,
/// their settings and Open Images never depend on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum SortKey {
    /// The order the photos were queued (PhotoCraft's default; Adobe's menu has no such item).
    #[default]
    OpenOrder,
    FileName,
    StarRating,
}

impl SortKey {
    pub(super) fn code(self) -> &'static str {
        match self {
            SortKey::OpenOrder => "openOrder",
            SortKey::FileName => "fileName",
            SortKey::StarRating => "starRating",
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortKey::OpenOrder => tl!("Open Order"),
            SortKey::FileName => tl!("Name"),
            SortKey::StarRating => tl!("Star Rating"),
        }
    }
}

impl Default for FilmstripView {
    fn default() -> Self {
        Self {
            visible: true,
            grid: false,
            orientation: Orientation::Vertical,
            preferred_width: DEFAULT_WIDTH,
            preferred_height: DEFAULT_HEIGHT,
            show_filenames: true,
            show_edit_badges: true,
            sort: SortKey::OpenOrder,
            reverse: false,
            filter: RatingFilter::default(),
        }
    }
}

impl FilmstripView {
    pub fn set_width(&mut self, width: f32) -> Result<(), String> {
        if !width.is_finite() {
            return Err("Camera Raw filmstrip width must be a finite number".into());
        }
        self.preferred_width = width.clamp(MIN_WIDTH, MAX_WIDTH);
        Ok(())
    }

    pub fn set_height(&mut self, height: f32) -> Result<(), String> {
        if !height.is_finite() {
            return Err("Camera Raw filmstrip height must be a finite number".into());
        }
        self.preferred_height = height.clamp(MIN_HEIGHT, MAX_HEIGHT);
        Ok(())
    }
}

/// Validate a control-channel `filmstrip` object without touching the live view.
pub(super) fn prepare(mut next: FilmstripView, value: &serde_json::Value) -> Result<FilmstripView, String> {
    let fields = value.as_object().ok_or("Camera Raw filmstrip must be an object")?;
    for (key, v) in fields {
        let flag = || v.as_bool().ok_or(format!("Camera Raw filmstrip {key} must be true or false"));
        match key.as_str() {
            "visible" => next.visible = flag()?,
            "grid" => next.grid = flag()?,
            "showFilenames" => next.show_filenames = flag()?,
            "showEditBadges" => next.show_edit_badges = flag()?,
            "width" => next.set_width(v.as_f64().ok_or("Camera Raw filmstrip width must be a number")? as f32)?,
            "height" => next.set_height(v.as_f64().ok_or("Camera Raw filmstrip height must be a number")? as f32)?,
            "orientation" => {
                next.orientation = match v.as_str() {
                    Some("vertical") => Orientation::Vertical,
                    Some("horizontal") => Orientation::Horizontal,
                    _ => return Err("Camera Raw filmstrip orientation must be \"vertical\" or \"horizontal\"".into()),
                }
            }
            "sort" => {
                let sort = v.as_object().ok_or("Camera Raw filmstrip sort must be an object")?;
                for (part, v) in sort {
                    match part.as_str() {
                        "by" => {
                            next.sort = match v.as_str() {
                                Some("openOrder") => SortKey::OpenOrder,
                                Some("fileName") => SortKey::FileName,
                                Some("starRating") => SortKey::StarRating,
                                _ => return Err("Camera Raw filmstrip sort by must be \"openOrder\", \"fileName\" or \"starRating\"".into()),
                            }
                        }
                        "reverse" => next.reverse = v.as_bool().ok_or("Camera Raw filmstrip sort reverse must be true or false")?,
                        other => return Err(format!("unknown Camera Raw filmstrip sort setting {other}")),
                    }
                }
            }
            "filter" => {
                let filter = v.as_object().ok_or("Camera Raw filmstrip filter must be an object")?;
                for (part, v) in filter {
                    let shown = || v.as_bool().ok_or(format!("Camera Raw filmstrip filter {part} must be true or false"));
                    match part.as_str() {
                        "rated" => next.filter.rated = shown()?,
                        "unrated" => next.filter.unrated = shown()?,
                        "rejected" => next.filter.rejected = shown()?,
                        "unread" => next.filter.unread = shown()?,
                        "stars" => {
                            next.filter.stars = if v.is_null() {
                                StarComparison::Any
                            } else {
                                let stars = v.as_object().ok_or("Camera Raw star filter must be null or {comparison, value}")?;
                                if stars.len() != 2 {
                                    return Err("Camera Raw star filter needs only comparison and value".into());
                                }
                                let n = stars
                                    .get("value")
                                    .and_then(serde_json::Value::as_u64)
                                    .filter(|n| *n <= 5)
                                    .ok_or("Camera Raw star value must be an integer from 0 to 5")? as u8;
                                match stars.get("comparison").and_then(serde_json::Value::as_str) {
                                    Some(">=") => StarComparison::AtLeast(n),
                                    Some("<=") => StarComparison::AtMost(n),
                                    Some("=") => StarComparison::Exactly(n),
                                    _ => return Err("Camera Raw star comparison must be >=, <= or =".into()),
                                }
                            };
                        }
                        "color" => {
                            next.filter.color = match v.as_str() {
                                Some("any") => ColorFilter::Any,
                                Some("none") => ColorFilter::None,
                                Some("red") => ColorFilter::Red,
                                Some("yellow") => ColorFilter::Yellow,
                                Some("green") => ColorFilter::Green,
                                Some("blue") => ColorFilter::Blue,
                                Some("purple") => ColorFilter::Purple,
                                Some("other") => ColorFilter::Other,
                                _ => return Err("Camera Raw colour filter must be any, none, red, yellow, green, blue, purple or other".into()),
                            }
                        }
                        "mark" => {
                            next.filter.mark = match v.as_str() {
                                Some("any") => MarkFilter::Any,
                                Some("marked") => MarkFilter::Marked,
                                Some("unmarked") => MarkFilter::Unmarked,
                                _ => return Err("Camera Raw mark filter must be any, marked or unmarked".into()),
                            }
                        }
                        other => return Err(format!("unknown Camera Raw filmstrip filter setting {other}")),
                    }
                }
            }
            other => return Err(format!("unknown Camera Raw filmstrip setting {other}")),
        }
    }
    Ok(next)
}

/// Where the strip may sit: none when hidden, absent, or when the window cannot afford a usable one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Layout {
    pub strip: Option<Rect>,
    pub splitter: Option<Rect>,
    pub preview_body: Rect,
    pub orientation: Orientation,
}

/// What the last frame drew, kept for the control channel and tests.
#[derive(Clone, Debug)]
pub(super) struct Frame {
    pub layout: Layout,
    /// Actual grid viewport when grid replaces the preview.
    pub grid: Option<Rect>,
    pub toggle: Option<Rect>,
    pub cards: Vec<(DocId, Rect)>,
    /// What the preview toolbar actually drew; it must end inside `layout.preview_body`.
    pub toolbar: Rect,
    /// The Orientation menu entries while that menu is open.
    pub menu: Vec<(Orientation, Rect)>,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            layout: Layout { strip: None, splitter: None, preview_body: Rect::ZERO, orientation: Orientation::Vertical },
            grid: None,
            toggle: None,
            cards: Vec::new(),
            toolbar: Rect::ZERO,
            menu: Vec::new(),
        }
    }
}

pub(super) fn effective_width(preferred: f32, body_width: f32, panel_width: f32) -> f32 {
    let room = (body_width - panel_width - MIN_PREVIEW_WIDTH).max(0.0);
    if room < MIN_WIDTH {
        return 0.0;
    }
    let wanted = if preferred.is_finite() { preferred.clamp(MIN_WIDTH, MAX_WIDTH) } else { DEFAULT_WIDTH };
    wanted.min(room)
}

pub(super) fn effective_height(preferred: f32, body_height: f32) -> f32 {
    let room = (body_height - MIN_PREVIEW_HEIGHT).max(0.0);
    if room < MIN_HEIGHT {
        return 0.0;
    }
    let wanted = if preferred.is_finite() { preferred.clamp(MIN_HEIGHT, MAX_HEIGHT) } else { DEFAULT_HEIGHT };
    wanted.min(room)
}

pub(super) fn layout(body: Rect, panel_width: f32, view: &FilmstripView, present: bool) -> Layout {
    let shown = present && view.visible && !view.grid;
    let orientation = view.orientation;
    match orientation {
        Orientation::Vertical => {
            let width = if shown { effective_width(view.preferred_width, body.width(), panel_width) } else { 0.0 };
            let strip = (width > 0.0).then(|| Rect::from_min_max(body.min, pos2(body.left() + width, body.bottom())));
            let splitter =
                strip.map(|s| Rect::from_min_max(pos2(s.right() - SPLITTER_HIT / 2.0, body.top()), pos2(s.right() + SPLITTER_HIT / 2.0, body.bottom())));
            let right = (body.right() - panel_width).max(body.left() + width + 1.0);
            Layout { strip, splitter, preview_body: Rect::from_min_max(pos2(body.left() + width, body.top()), pos2(right, body.bottom())), orientation }
        }
        Orientation::Horizontal => {
            let right = (body.right() - panel_width).max(body.left() + 1.0);
            let height = if shown && right - body.left() >= MIN_STRIP_ACROSS { effective_height(view.preferred_height, body.height()) } else { 0.0 };
            let strip = (height > 0.0).then(|| Rect::from_min_max(pos2(body.left(), body.bottom() - height), pos2(right, body.bottom())));
            let splitter = strip.map(|s| Rect::from_min_max(pos2(s.left(), s.top() - SPLITTER_HIT / 2.0), pos2(s.right(), s.top() + SPLITTER_HIT / 2.0)));
            Layout { strip, splitter, preview_body: Rect::from_min_max(body.min, pos2(right, body.bottom() - height)), orientation }
        }
    }
}

/// A picture's height for its width: the photo's own aspect within [MIN, MAX] (DEFAULT before its size is known).
pub(super) fn picture_aspect((w, h): (u32, u32)) -> f32 {
    if w == 0 || h == 0 {
        return DEFAULT_PICTURE_ASPECT;
    }
    (h as f32 / w as f32).clamp(MIN_PICTURE_ASPECT, MAX_PICTURE_ASPECT)
}

/// What is under a picture: the filename row (when shown), the status row, their gaps and the bottom inset.
fn below_picture(filenames: bool) -> f32 {
    ROW_GAP + if filenames { NAME_ROW + ROW_GAP } else { 0.0 } + STATUS_ROW + INSET
}

/// The narrowest card: the status row (crop, five stars, adjustments) always fits.
fn min_card_width() -> f32 {
    5.0 * STAR_PITCH + 2.0 * ICON + 4.0 * INSET
}

/// A vertical strip's card width and picture width.
pub(super) fn card_widths(strip_width: f32) -> (f32, f32) {
    let room = (strip_width - 2.0 * STRIP_PAD - SCROLL_RESERVE).max(min_card_width());
    let picture = (room - 2.0 * INSET).min(MAX_PICTURE_WIDTH);
    (picture + 2.0 * INSET, picture)
}

/// A vertical card for a photo of `size` (as tall as its content), and the box its picture fits in.
pub(super) fn card_size(strip_width: f32, size: (u32, u32), filenames: bool) -> (Vec2, Vec2) {
    let (card_w, picture_w) = card_widths(strip_width);
    let picture_h = picture_w * picture_aspect(size);
    (vec2(card_w, INSET + picture_h + below_picture(filenames)), vec2(picture_w, picture_h))
}

/// A horizontal card: the strip's height sets the picture's height, the photo's aspect (2:3 to 3:2) its width.
pub(super) fn card_size_h(strip_height: f32, size: (u32, u32), filenames: bool) -> (Vec2, Vec2) {
    let card_h = (strip_height - 2.0 * STRIP_PAD - SCROLL_RESERVE).max(INSET + 24.0 + below_picture(filenames));
    let picture_h = (card_h - INSET - below_picture(filenames)).max(24.0);
    let wide = if size.0 == 0 || size.1 == 0 { 1.5 } else { (size.0 as f32 / size.1 as f32).clamp(2.0 / 3.0, 1.5) };
    let picture_w = picture_h * wide;
    (vec2((picture_w + 2.0 * INSET).max(min_card_width()), card_h), vec2(picture_w, picture_h))
}

/// Where a card's parts are: the picture, the filename row and the status row's crop glyph, stars and adjustments glyph.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CardParts {
    pub picture: Rect,
    pub name: Option<Rect>,
    pub crop: Rect,
    pub stars: Rect,
    pub adjustments: Rect,
}

pub(super) fn card_parts(card: Rect, picture_box: Vec2, size: (u32, u32), filenames: bool) -> CardParts {
    let slot = Rect::from_min_size(pos2(card.center().x - picture_box.x / 2.0, card.top() + INSET), picture_box);
    let mut y = slot.bottom() + ROW_GAP;
    let name = filenames.then(|| {
        let row = Rect::from_min_size(pos2(card.left() + INSET, y), vec2(card.width() - 2.0 * INSET, NAME_ROW));
        y += NAME_ROW + ROW_GAP;
        row
    });
    let row = Rect::from_min_size(pos2(card.left() + INSET, y), vec2(card.width() - 2.0 * INSET, STATUS_ROW));
    CardParts {
        picture: fit_rect(slot, size),
        name,
        crop: Rect::from_min_size(pos2(row.left(), row.center().y - ICON / 2.0), vec2(ICON, ICON)),
        stars: Rect::from_center_size(row.center(), vec2(5.0 * STAR_PITCH, STATUS_ROW)),
        adjustments: Rect::from_min_size(pos2(row.right() - ICON, row.center().y - ICON / 2.0), vec2(ICON, ICON)),
    }
}

/// Where the picture itself sits inside a box (the box letterboxes it).
pub(super) fn fit_rect(slot: Rect, (w, h): (u32, u32)) -> Rect {
    if w == 0 || h == 0 {
        return slot;
    }
    let scale = (slot.width() / w as f32).min(slot.height() / h as f32);
    Rect::from_center_size(slot.center(), vec2(w as f32 * scale, h as f32 * scale))
}

/// What a card's rating row says, in words (also its accessible state). Unknown is never said as unrated.
pub(super) fn rating_text(rating: PhotoRating) -> String {
    match rating {
        PhotoRating::Unknown => tl!("Rating not read yet").to_string(),
        PhotoRating::Unrated => tl!("Unrated").to_string(),
        PhotoRating::Stars(n) => crate::i18n::fmt(tl!("Rating: {n} of 5"), &[("n", &n.to_string())]),
        PhotoRating::Rejected => tl!("Rejected").to_string(),
    }
}

pub(super) fn adjustments_text(adjustments: Adjustments) -> String {
    match adjustments {
        Adjustments::Unknown => format!("{}: {}", tl!("Adjustments"), tl!("Not read yet")),
        Adjustments::None => tl!("No adjustments").to_string(),
        Adjustments::Edited => tl!("Adjustments").to_string(),
    }
}

/// The open-time review has no crop tool yet: the glyph says so instead of claiming any state.
pub(super) fn crop_text() -> String {
    format!("{}: {}", tl!("Crop"), tl!("Not available yet"))
}

/// One tooltip per card, by the part under the pointer: the crop glyph, the stars, the adjustments glyph, or the name.
pub(super) fn card_tooltip(entry: &Entry, parts: &CardParts, at: Option<egui::Pos2>, adjustments_shown: bool) -> String {
    let text = match at {
        Some(p) if parts.crop.expand(1.0).contains(p) => {
            if entry.marked {
                tl!("Marked").into()
            } else {
                crop_text()
            }
        }
        Some(p) if parts.stars.contains(p) => rating_text(entry.rating),
        Some(p) if adjustments_shown && parts.adjustments.expand(1.0).contains(p) => adjustments_text(entry.adjustments),
        _ if entry.filtered_out => format!("{}\n{}", entry.name, tl!("Shown; hidden by filter")),
        _ => entry.name.clone(),
    };
    let mut details = vec![text];
    if entry.pending_xmp {
        details.push(tl!("Changes not yet saved to XMP").into());
    }
    if entry.marked {
        details.push(tl!("Marked").into());
    }
    if !entry.label.is_empty() {
        details.push(format!("{}: {}", tl!("Label"), entry.label));
    }
    details.join("\n")
}

/// The accessible name: the file name, plus the existing translated "Adjustments" when the badge shows and the existing
/// translated "Preview" and "Active" as "Preview: Active" for the photo whose preview is shown (a state that is not colour
/// or outline alone). The key/value form avoids the English word order; a catalog whose "Active" is gendered (ru, uk, es,
/// it, el) may still not agree with its "Preview". Correct wording needs new catalog rows (not part of D2).
pub(super) fn accessible_label(name: &str, rating: PhotoRating, adjustments: Option<Adjustments>, active: bool) -> String {
    let mut label = format!("{name}, {}", rating_text(rating));
    if let Some(adjustments) = adjustments {
        label = format!("{label}, {}", adjustments_text(adjustments));
    }
    if active {
        label = format!("{label}, {}: {}", tl!("Preview"), tl!("Active"));
    }
    label
}

/// The longest form of `name` (shortened as [`fit_name`] does) whose drawn width, by `width_of`, is at most `max_width`.
fn fit_name_to_width(name: &str, max_width: f32, width_of: impl Fn(&str) -> f32) -> String {
    let count = name.chars().count();
    if width_of(name) <= max_width {
        return name.to_string();
    }
    // A larger character budget never draws narrower: search for the largest budget that fits.
    let (mut fits, mut too_wide) = (1usize, count);
    while too_wide > fits + 1 {
        let mid = (fits + too_wide) / 2;
        if width_of(&fit_name(name, mid)) <= max_width {
            fits = mid;
        } else {
            too_wide = mid;
        }
    }
    fit_name(name, fits)
}

/// Shorten `name` to `max_chars`, keeping a short extension so the file type stays visible.
fn fit_name(name: &str, max_chars: usize) -> String {
    let count = name.chars().count();
    if count <= max_chars || max_chars < 4 {
        return name.chars().take(max_chars.max(1)).collect();
    }
    let ext_len = name.rfind('.').map_or(0, |at| name.get(at..).map_or(0, |e| e.chars().count())).min(6);
    let head: String = name.chars().take(max_chars.saturating_sub(ext_len + 1)).collect();
    let tail: String = name.chars().skip(count - ext_len).collect();
    format!("{head}…{tail}")
}

/// Whether a photo carries develop adjustments, as far as PhotoCraft knows: a photo not yet shown is `Unknown`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Adjustments {
    #[default]
    Unknown,
    None,
    Edited,
}

pub(super) struct Entry {
    pub document: DocId,
    /// The photograph's stable identity: selection, activation and keyboard focus follow it, so a re-develop that gives the
    /// document a new id (A1 -> A2) never changes which photo is selected or focused.
    pub photo: PhotoId,
    pub name: String,
    pub texture: Option<TextureId>,
    /// The part of `texture` that is the picture: the 64 px layer thumbnail is square and letterboxed, a thumbnail made
    /// for the card is the picture alone (F2).
    pub uv: Rect,
    /// The document's pixel size: the picture's aspect.
    pub size: (u32, u32),
    /// The photo whose preview and settings are shown.
    pub active: bool,
    /// In the selection set; independent of `active`.
    pub selected: bool,
    /// The active, ready photograph whose settings can be copied by an explicit Sync Settings action.
    pub settings_source: Option<(PhotoId, String)>,
    pub can_undo: bool,
    /// Whether it carries effective develop adjustments; an unvisited photo is `Unknown`, never claimed either way.
    pub adjustments: Adjustments,
    /// Its current star rating; `Unknown` when not read.
    pub rating: PhotoRating,
    /// Preserve the original literal label until the user explicitly changes it.
    pub label: String,
    /// A separate user mark, independent of ratings, labels and rejection.
    pub marked: bool,
    /// Current recipe or metadata differs from the acknowledged XMP baseline.
    pub pending_xmp: bool,
    /// The photo shown, kept in the strip although the filter would hide it (F4 lab F1).
    pub filtered_out: bool,
}

/// A thumbnail reports intent only. The session controller performs metadata edits,
/// export and guarded removal through the same operations used by keyboard actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PhotoAction {
    SaveJpeg(PhotoId),
    SyncSettings { source: PhotoId, targets: Vec<PhotoId> },
    ResetSettings(PhotoId),
    SetValue { targets: Vec<PhotoId> },
    UndoGroup,
    SetRating { targets: Vec<PhotoId>, rating: u8 },
    SetLabel { targets: Vec<PhotoId>, label: String },
    SetMark { targets: Vec<PhotoId>, marked: bool },
    Hide { targets: Vec<PhotoId> },
    ShowHidden,
    Remove { targets: Vec<PhotoId> },
}

/// Call only after focused fields and tools have had first refusal. This pure mapping
/// is also used by headless tests; it never consumes keyboard input or mutates state.
pub(super) fn metadata_shortcut(key: Key, modifiers: Modifiers, targets: Vec<PhotoId>) -> Option<PhotoAction> {
    if targets.is_empty() {
        return None;
    }
    if key == Key::Num0 && modifiers.ctrl && modifiers.shift && !modifiers.alt && !modifiers.mac_cmd {
        return Some(PhotoAction::SetLabel { targets, label: "To Do".into() });
    }
    if modifiers.any() {
        return None;
    }
    let rating = match key {
        Key::Num0 => Some(0),
        Key::Num1 => Some(1),
        Key::Num2 => Some(2),
        Key::Num3 => Some(3),
        Key::Num4 => Some(4),
        Key::Num5 => Some(5),
        _ => None,
    };
    if let Some(rating) = rating {
        return Some(PhotoAction::SetRating { targets, rating });
    }
    let label = match key {
        Key::Num6 => "Select",
        Key::Num7 => "Second",
        Key::Num8 => "Approved",
        Key::Num9 => "Review",
        _ => return None,
    };
    Some(PhotoAction::SetLabel { targets, label: label.into() })
}

#[derive(Default)]
pub(super) struct Drawn {
    /// The photo a click or Enter asked to show; selection changes are applied in place.
    pub activate: Option<PhotoId>,
    pub cards: Vec<(DocId, Rect)>,
    pub actions: Vec<PhotoAction>,
    ids: Vec<egui::Id>,
    photos: Vec<PhotoId>,
}

/// Arrow keys move keyboard focus between thumbnails along the strip (Up/Down vertical,
/// Left/Right horizontal; Home/End either way); Enter or Space then activates the focused one like
/// a click. Focus alone never changes the active photo or the selection, and only a focused
/// thumbnail claims these keys, so a slider or text field that owns focus keeps its arrows.
fn move_focus(ui: &mut Ui, ids: &[egui::Id], orientation: Orientation) {
    let Some(at) = ids.iter().position(|id| ui.memory(|m| m.has_focus(*id))) else { return };
    let last = ids.len().saturating_sub(1);
    let (back, forward) = match orientation {
        Orientation::Vertical => (Key::ArrowUp, Key::ArrowDown),
        Orientation::Horizontal => (Key::ArrowLeft, Key::ArrowRight),
    };
    for (key, to) in [(back, at.saturating_sub(1)), (forward, (at + 1).min(last)), (Key::Home, 0), (Key::End, last)] {
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, key))
            && let Some(id) = ids.get(to)
        {
            ui.memory_mut(|m| m.request_focus(*id));
        }
    }
}

/// With several photos selected and a thumbnail focused, the arrows along the strip step the *shown* photo through the
/// selected photos only (skipping unselected ones, stopping at the ends) and leave the selection untouched; focus follows.
/// Returns whether it handled the keys. With one photo or none selected the arrows keep their old job (`move_focus`),
/// and a slider or text field that owns focus never reaches here because no thumbnail has focus.
fn step_selected_group(ui: &mut Ui, drawn: &mut Drawn, selection: &Selection, order: &[PhotoId], entries: &[Entry], orientation: Orientation) -> bool {
    // Shift/Ctrl/Alt with an arrow is not plain stepping (Adobe's Shift+arrow extends the selection: not implemented).
    // A group to step through needs two selected photos the strip shows (one hidden by the filter does not count).
    let visible_selected = order.iter().filter(|photo| selection.contains(**photo)).count();
    if visible_selected < 2 || ui.input(|i| i.modifiers.any()) || !drawn.ids.iter().any(|id| ui.memory(|m| m.has_focus(*id))) {
        return false;
    }
    let Some(active) = entries.iter().find(|e| e.active).map(|e| e.photo) else { return false };
    let (back, forward) = match orientation {
        Orientation::Vertical => (Key::ArrowUp, Key::ArrowDown),
        Orientation::Horizontal => (Key::ArrowLeft, Key::ArrowRight),
    };
    let mut handled = false;
    for (key, step_forward) in [(back, false), (forward, true)] {
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, key)) {
            handled = true;
            if let Some(next) = selection.step_from(active, order, step_forward) {
                drawn.activate = Some(next);
                if let Some(id) = drawn.photos.iter().position(|d| *d == next).and_then(|at| drawn.ids.get(at)) {
                    ui.memory_mut(|m| m.request_focus(*id));
                }
            }
        }
    }
    handled
}

pub(super) fn draw(ui: &mut Ui, strip: Rect, t: &Tokens, view: &mut FilmstripView, selection: &mut Selection, entries: &[Entry], hidden: &Hidden) -> Drawn {
    let mut drawn = Drawn::default();
    let order: Vec<PhotoId> = entries.iter().map(|e| e.photo).collect();
    let orientation = view.orientation;
    ui.painter().rect_filled(strip, 0.0, t.dock);
    let edge = if orientation == Orientation::Vertical { [strip.right_top(), strip.right_bottom()] } else { [strip.left_top(), strip.right_top()] };
    ui.painter().line_segment(edge, Stroke::new(1.0, t.separator));
    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(strip.shrink(STRIP_PAD)));
    if orientation == Orientation::Vertical {
        egui::ScrollArea::vertical().id_salt("camera-raw-filmstrip").auto_shrink([false, false]).show(&mut inner, |ui| {
            ui.spacing_mut().item_spacing.y = CARD_GAP;
            // Cards are centred: past MAX_PICTURE_WIDTH a wider strip adds a margin, not taller cards.
            let width = (strip.width() - 2.0 * STRIP_PAD - SCROLL_RESERVE).max(0.0);
            ui.allocate_ui_with_layout(vec2(width, 0.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                hidden_row(ui, t, hidden, &mut drawn.actions);
                empty_result_row(ui, view, entries);
                cards(ui, t, view, selection, &order, entries, strip.width(), &mut drawn);
            });
        });
    } else {
        egui::ScrollArea::horizontal().id_salt("camera-raw-filmstrip-h").auto_shrink([false, false]).show(&mut inner, |ui| {
            ui.spacing_mut().item_spacing.x = CARD_GAP;
            ui.horizontal_top(|ui| {
                hidden_row(ui, t, hidden, &mut drawn.actions);
                empty_result_row(ui, view, entries);
                cards(ui, t, view, selection, &order, entries, strip.height(), &mut drawn)
            });
        });
    }
    finish_draw(ui, &mut drawn, selection, &order, entries, hidden, orientation);
    drawn
}

/// The same filtered entries and selection, laid out in the preview area. Its scroll
/// position is independent of either strip orientation and survives a mode round trip.
pub(super) fn draw_grid(ui: &mut Ui, rect: Rect, t: &Tokens, view: &mut FilmstripView, selection: &mut Selection, entries: &[Entry], hidden: &Hidden) -> Drawn {
    let mut drawn = Drawn::default();
    ui.painter().rect_filled(rect, 0.0, t.canvas);
    let order: Vec<PhotoId> = entries.iter().map(|entry| entry.photo).collect();
    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(STRIP_PAD)));
    inner.set_clip_rect(rect.intersect(ui.clip_rect()));
    let extent = view.preferred_width.clamp(MIN_WIDTH, MAX_WIDTH);
    let card_width = card_widths(extent).0;
    let columns = ((inner.available_width() + CARD_GAP) / (card_width + CARD_GAP)).floor().max(1.0) as usize;
    egui::ScrollArea::vertical().id_salt("camera-raw-grid").auto_shrink([false, false]).show(&mut inner, |ui| {
        hidden_row(ui, t, hidden, &mut drawn.actions);
        empty_result_row(ui, view, entries);
        egui::Grid::new("camera-raw-grid-cards").num_columns(columns).spacing(vec2(CARD_GAP, CARD_GAP)).show(ui, |ui| {
            for (at, entry) in entries.iter().enumerate() {
                ui.push_id(("cr-filmstrip-card", entry.photo.0), |ui| card(ui, t, view, selection, &order, entry, extent, &mut drawn));
                if (at + 1) % columns == 0 {
                    ui.end_row();
                }
            }
        });
    });
    // Grid arrows move focus without changing the editing group.
    if !egui::Popup::is_any_open(ui.ctx())
        && let Some(at) = drawn.ids.iter().position(|id| ui.memory(|m| m.has_focus(*id)))
    {
        let last = drawn.ids.len().saturating_sub(1);
        for (key, to) in [
            (Key::ArrowLeft, at.saturating_sub(1)),
            (Key::ArrowRight, at.saturating_add(1).min(last)),
            (Key::ArrowUp, at.saturating_sub(columns)),
            (Key::ArrowDown, at.saturating_add(columns).min(last)),
        ] {
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, key))
                && let Some(id) = drawn.ids.get(to)
            {
                ui.memory_mut(|m| m.request_focus(*id));
            }
        }
    }
    finish_draw(ui, &mut drawn, selection, &order, entries, hidden, Orientation::Vertical);
    drawn
}

#[allow(clippy::too_many_arguments)]
fn finish_draw(ui: &mut Ui, drawn: &mut Drawn, selection: &mut Selection, order: &[PhotoId], entries: &[Entry], hidden: &Hidden, orientation: Orientation) {
    ui.ctx().data_mut(|data| data.insert_temp(egui::Id::new("camera-raw-thumbnail-focus-ids"), drawn.ids.clone()));
    // While a menu or popup is open (a thumbnail's own menu keeps its focus) the keys belong to it, never to the strip
    // underneath (F3, lab cycle 4, LF-12).
    let menu_open = egui::Popup::is_any_open(ui.ctx());
    if !menu_open && !step_selected_group(ui, drawn, selection, order, entries, orientation) {
        move_focus(ui, &drawn.ids, orientation);
    }
    let focused = drawn.ids.iter().position(|id| ui.memory(|m| m.has_focus(*id)));
    selection.set_focus(focused.and_then(|i| drawn.photos.get(i).copied()));
    // Select All only while a thumbnail has focus, so a text field keeps its own Ctrl+A.
    if focused.is_some() && !menu_open && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
        selection.select_all(order);
    }
    // A photo hidden by the filter keeps its selection whatever is done to the visible ones (F4 lab F1).
    for id in &hidden.selected {
        selection.include(*id);
    }
}

/// Text fields and DragValues keep their input; only a currently drawn thumbnail
/// opts into the Camera Raw rating/label shortcuts while keyboard focus is held.
pub(super) fn owns_keyboard_focus(ctx: &egui::Context) -> bool {
    let Some(focused) = ctx.memory(|memory| memory.focused()) else { return false };
    ctx.data(|data| data.get_temp::<Vec<egui::Id>>(egui::Id::new("camera-raw-thumbnail-focus-ids"))).is_some_and(|ids| ids.contains(&focused))
}

fn empty_result_row(ui: &mut Ui, view: &mut FilmstripView, entries: &[Entry]) {
    if entries.iter().all(|entry| entry.filtered_out) {
        ui.label(tl!("No photos match"));
        if ui.button(tl!("Clear filters")).clicked() {
            view.filter = RatingFilter::default();
        }
    }
}

/// "3 hidden, 1 selected" above (or before) the cards while the filter hides anything.
fn hidden_row(ui: &mut Ui, t: &Tokens, hidden: &Hidden, actions: &mut Vec<PhotoAction>) {
    if hidden.count > 0 {
        ui.label(egui::RichText::new(hidden_text(hidden)).size(11.0).color(t.text_faint));
        if ui.small_button(tl!("Show hidden photos")).clicked() {
            actions.push(PhotoAction::ShowHidden);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cards(ui: &mut Ui, t: &Tokens, view: &mut FilmstripView, selection: &mut Selection, order: &[PhotoId], entries: &[Entry], extent: f32, drawn: &mut Drawn) {
    for entry in entries {
        // Stable widget identity follows the photograph (not the list index, and not its document id, which a
        // re-develop replaces), so keyboard focus survives A1 -> A2.
        ui.push_id(("cr-filmstrip-card", entry.photo.0), |ui| card(ui, t, view, selection, order, entry, extent, drawn));
    }
}

/// The Orientation choice shared by the show/hide control's menu and the cards' menu. A radio dot
/// marks the current choice; selecting one only changes the layout.
fn orientation_menu(ui: &mut Ui, view: &mut FilmstripView) -> Vec<(Orientation, Rect)> {
    let mut items = Vec::new();
    ui.label(tl!("Orientation"));
    for (choice, label) in [(Orientation::Horizontal, tl!("Horizontal")), (Orientation::Vertical, tl!("Vertical"))] {
        let response = ui.radio(view.orientation == choice, label);
        items.push((choice, response.rect));
        if response.clicked() {
            view.orientation = choice;
            ui.close();
        }
    }
    items
}

/// F2 (lab cycle 5): the longest side, in physical pixels, a thumbnail needs for the strip the last frame drew, in 64 px
/// steps (so a splitter drag does not remake every thumbnail at each point) from 64 to 512; `None` without a strip.
pub(super) fn thumbnail_side(frame: &Frame, pixels_per_point: f32) -> Option<u32> {
    let longest = if frame.grid.is_some() {
        frame.cards.iter().map(|(_, card)| card.width() - 2.0 * INSET).reduce(f32::max)?
    } else {
        let strip = frame.layout.strip?;
        // A vertical picture is never taller than wide; a horizontal one is widest at 3:2.
        match frame.layout.orientation {
            Orientation::Vertical => card_widths(strip.width()).1,
            Orientation::Horizontal => card_size_h(strip.height(), (3, 2), true).1.x,
        }
    };
    let pixels = (longest * pixels_per_point).ceil().max(1.0) as u32;
    Some(pixels.div_ceil(64).saturating_mul(64).clamp(64, 512))
}

/// The documents whose cards the last frame drew at least partly inside the strip.
pub(super) fn documents_in_view(frame: &Frame) -> Vec<DocId> {
    let Some(strip) = frame.grid.or(frame.layout.strip) else { return Vec::new() };
    frame.cards.iter().filter(|(_, card)| card.intersects(strip)).map(|(document, _)| *document).collect()
}

/// The strip's visual order of `entries` (given in open order), F4 lab S1. The sort is stable and only the key comparison is
/// reversed, so photos that compare equal keep their open order in either direction; a rating not read yet is never treated
/// as a value and always sorts last.
pub(super) fn sort_entries(entries: &mut [Entry], key: SortKey, reverse: bool) {
    let directed = |o: std::cmp::Ordering| if reverse { o.reverse() } else { o };
    match key {
        SortKey::OpenOrder if reverse => entries.reverse(),
        SortKey::OpenOrder => {}
        SortKey::FileName => entries.sort_by(|a, b| directed(natural_cmp(&a.name, &b.name))),
        SortKey::StarRating => entries.sort_by(|a, b| match (rating_rank(a.rating), rating_rank(b.rating)) {
            (Some(a), Some(b)) => directed(a.cmp(&b)),
            (a, b) => a.is_none().cmp(&b.is_none()),
        }),
    }
}

/// Rejected, then unrated, then one to five stars; a rating not read yet has no rank.
fn rating_rank(rating: PhotoRating) -> Option<u8> {
    match rating {
        PhotoRating::Unknown => None,
        PhotoRating::Rejected => Some(0),
        PhotoRating::Unrated => Some(1),
        PhotoRating::Stars(n) => Some(1 + n),
    }
}

/// Case-insensitive natural order: runs of digits compare by value, so IMG_2 comes before IMG_10.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let digits = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut run = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        run.push(c);
                        it.next();
                    }
                    run.trim_start_matches('0').to_string()
                };
                let (x, y) = (digits(&mut a), digits(&mut b));
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(&y));
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// What the Sort control says when the order is not the default: "Sort: Star Rating, Reverse".
pub(super) fn sort_summary(view: &FilmstripView) -> String {
    let mut text = format!("{}: {}", tl!("Sort"), view.sort.label());
    if view.reverse {
        text = format!("{text}, {}", tl!("Reverse"));
    }
    text
}

/// The Sort control (F4 lab S1): Adobe's menu order, Capture Date and Color Label visibly unavailable, Reverse as a check
/// item. It is lit while the order is not the default. It changes only the view.
pub(super) fn sort_control(ui: &mut Ui, t: &Tokens, rect: Rect, view: &mut FilmstripView) {
    let active = view.sort != SortKey::OpenOrder || view.reverse;
    let name = if active { sort_summary(view) } else { tl!("Sort").to_string() };
    let response = ui.interact(rect, ui.id().with("cr-sort"), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 3.0, t.accent_soft);
    } else if response.hovered() {
        painter.rect_filled(rect, 3.0, t.hover);
    }
    paint_sort_glyph(painter, rect, Stroke::new(1.5, if active { t.accent_text } else { t.icon }));
    let response = response.on_hover_text(&name);
    egui::Popup::menu(&response).show(|ui| {
        ui.label(egui::RichText::new(tl!("Sort By:")).color(t.text_faint));
        let mut item = |ui: &mut Ui, key: SortKey| {
            if ui.add(egui::Button::selectable(view.sort == key, key.label())).clicked() {
                view.sort = key;
                ui.close();
            }
        };
        item(ui, SortKey::OpenOrder);
        ui.add_enabled(false, egui::Button::new(tl!("Capture Date"))).on_disabled_hover_text(unavailable_text(tl!("Capture Date")));
        item(ui, SortKey::FileName);
        item(ui, SortKey::StarRating);
        ui.add_enabled(false, egui::Button::new(tl!("Color Label"))).on_disabled_hover_text(unavailable_text(tl!("Color Label")));
        ui.separator();
        if ui.add(egui::Button::selectable(view.reverse, tl!("Reverse"))).clicked() {
            view.reverse = !view.reverse;
            ui.close();
        }
    });
}

/// The Filter control (F4 lab F1): Show: Rated, Unrated, Rejected, Rating not read yet (each a check item), then Show All.
/// It is lit while anything is hidden and says how many. It changes only the view.
pub(super) fn filter_control(ui: &mut Ui, t: &Tokens, rect: Rect, view: &mut FilmstripView, hidden: &Hidden) {
    let active = !view.filter.is_default();
    let name = if active { format!("{}: {}", tl!("Filter"), hidden_text(hidden)) } else { tl!("Filter").to_string() };
    let response = ui.interact(rect, ui.id().with("cr-filter"), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &name));
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 3.0, t.accent_soft);
    } else if response.hovered() {
        painter.rect_filled(rect, 3.0, t.hover);
    }
    paint_filter_glyph(painter, rect, Stroke::new(1.5, if active { t.accent_text } else { t.icon }));
    let response = response.on_hover_text(format!("{name}\n{}", filter_summary(view.filter)));
    // Several boxes can be set in one visit: the menu closes on a click outside it (or Esc), not on each toggle.
    egui::Popup::menu(&response).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        ui.label(egui::RichText::new(tl!("Show:")).color(t.text_faint));
        let filter = &mut view.filter;
        for (shown, label) in [
            (&mut filter.rated, tl!("Rated")),
            (&mut filter.unrated, tl!("Unrated")),
            (&mut filter.rejected, tl!("Rejected")),
            (&mut filter.unread, tl!("Rating not read yet")),
        ] {
            if ui.add(egui::Button::selectable(*shown, label)).clicked() {
                *shown = !*shown;
            }
        }
        ui.separator();
        ui.label(tl!("Stars"));
        let stars = filter.stars;
        let mut comparison = match stars {
            StarComparison::Any => 0,
            StarComparison::AtLeast(_) => 1,
            StarComparison::AtMost(_) => 2,
            StarComparison::Exactly(_) => 3,
        };
        let mut value = match stars {
            StarComparison::Any => 0,
            StarComparison::AtLeast(n) | StarComparison::AtMost(n) | StarComparison::Exactly(n) => n,
        };
        ui.horizontal(|ui| {
            // A nested ComboBox replaces this memory-backed popup. Keep the
            // choices in the Filter menu so several criteria can be set together.
            for (choice, label) in [(0, tl!("Any")), (1, "≥"), (2, "≤"), (3, "=")] {
                ui.selectable_value(&mut comparison, choice, label);
            }
            ui.add_enabled(comparison != 0, egui::DragValue::new(&mut value).range(0..=5).suffix(" ★"));
        });
        filter.stars = match comparison {
            1 => StarComparison::AtLeast(value),
            2 => StarComparison::AtMost(value),
            3 => StarComparison::Exactly(value),
            _ => StarComparison::Any,
        };
        ui.horizontal_wrapped(|ui| {
            for choice in [
                ColorFilter::Any,
                ColorFilter::None,
                ColorFilter::Red,
                ColorFilter::Yellow,
                ColorFilter::Green,
                ColorFilter::Blue,
                ColorFilter::Purple,
                ColorFilter::Other,
            ] {
                ui.selectable_value(&mut filter.color, choice, choice.label());
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut filter.mark, MarkFilter::Any, tl!("Marked and unmarked"));
            ui.selectable_value(&mut filter.mark, MarkFilter::Marked, tl!("Marked"));
            ui.selectable_value(&mut filter.mark, MarkFilter::Unmarked, tl!("Unmarked"));
        });
        ui.separator();
        if ui.add_enabled(!filter.is_default(), egui::Button::new(tl!("Clear filters"))).clicked() {
            *filter = RatingFilter::default();
            ui.close();
        }
        if ui.add_enabled(active, egui::Button::new(tl!("Show All"))).clicked() {
            view.filter = RatingFilter::default();
            ui.close();
        }
    });
}

pub(super) fn filter_summary(filter: RatingFilter) -> String {
    let mut criteria = Vec::new();
    match filter.stars {
        StarComparison::Any => {}
        StarComparison::AtLeast(n) => criteria.push(format!("Stars ≥ {n}")),
        StarComparison::AtMost(n) => criteria.push(format!("Stars ≤ {n}")),
        StarComparison::Exactly(n) => criteria.push(format!("Stars = {n}")),
    }
    if filter.color != ColorFilter::Any {
        criteria.push(filter.color.label().into());
    }
    match filter.mark {
        MarkFilter::Any => {}
        MarkFilter::Marked => criteria.push(tl!("Marked").into()),
        MarkFilter::Unmarked => criteria.push(tl!("Unmarked").into()),
    }
    for (enabled, label) in
        [(filter.rated, tl!("Rated")), (filter.unrated, tl!("Unrated")), (filter.rejected, tl!("Rejected")), (filter.unread, tl!("Rating not read yet"))]
    {
        if !enabled {
            criteria.push(format!("{label}: {}", tl!("hidden")));
        }
    }
    if criteria.is_empty() { tl!("All photos").into() } else { criteria.join(" · ") }
}

/// A funnel (the Filter glyph).
fn paint_filter_glyph(painter: &egui::Painter, rect: Rect, stroke: Stroke) {
    let c = rect.center();
    let points = [(-6.5, -5.5), (6.5, -5.5), (1.5, 0.5), (1.5, 5.5), (-1.5, 4.0), (-1.5, 0.5)].map(|(x, y)| pos2(c.x + x, c.y + y));
    painter.add(egui::Shape::closed_line(points.to_vec(), stroke));
}

/// Up and down arrows side by side (the Sort glyph).
fn paint_sort_glyph(painter: &egui::Painter, rect: Rect, stroke: Stroke) {
    let c = rect.center();
    for (x, from, to) in [(c.x - 3.5, c.y + 6.0, c.y - 6.0), (c.x + 3.5, c.y - 6.0, c.y + 6.0)] {
        let head = if to < from { 3.0 } else { -3.0 };
        painter.line_segment([pos2(x, from), pos2(x, to)], stroke);
        painter.line_segment([pos2(x - 3.0, to + head), pos2(x, to)], stroke);
        painter.line_segment([pos2(x + 3.0, to + head), pos2(x, to)], stroke);
    }
}

/// Whether a card is the shown photo within a group of two or more selected photos (the dashed frame, D-48).
pub(super) fn shown_in_group(active: bool, selected: bool, selected_count: usize) -> bool {
    active && selected && selected_count >= 2
}

fn dashed_outline(painter: &egui::Painter, rect: Rect, stroke: Stroke) {
    for [a, b] in [
        [rect.left_top(), rect.right_top()],
        [rect.right_top(), rect.right_bottom()],
        [rect.right_bottom(), rect.left_bottom()],
        [rect.left_bottom(), rect.left_top()],
    ] {
        painter.extend(egui::Shape::dashed_line(&[a, b], stroke, 5.0, 3.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn card(ui: &mut Ui, t: &Tokens, view: &mut FilmstripView, selection: &mut Selection, order: &[PhotoId], entry: &Entry, extent: f32, drawn: &mut Drawn) {
    let (size, picture_box) = match if view.grid { Orientation::Vertical } else { view.orientation } {
        Orientation::Vertical => card_size(extent, entry.size, view.show_filenames),
        Orientation::Horizontal => card_size_h(extent, entry.size, view.show_filenames),
    };
    // One click target per card: the rating row and the glyphs are painted, never separate widgets, so they cannot
    // swallow the selecting click.
    // The card's id is the photograph's (F4 lab S1), not its place in the strip: keyboard focus stays on the photo when the
    // strip is sorted, filtered or turned to the other orientation.
    let (_, rect) = ui.allocate_space(size);
    let parts = card_parts(rect, picture_box, entry.size, view.show_filenames);
    let [save_rect, menu_rect] = action_rects(rect);
    let response = ui.interact(rect, egui::Id::new(("cr-filmstrip-card", entry.photo.0)), Sense::click());
    let on_action = response.interact_pointer_pos().is_some_and(|at| save_rect.contains(at) || menu_rect.contains(at));
    // Enter or Space on a focused thumbnail behind an open menu belongs to the menu, like the arrows (C5-R1).
    if response.clicked() && !on_action && (response.interact_pointer_pos().is_some() || !egui::Popup::is_any_open(ui.ctx())) {
        // A click does not focus a button on its own; the arrow keys continue from here.
        response.request_focus();
        // A press with the pointer edits the selection by its modifiers; Enter or Space (no
        // pointer position) only steps the preview and leaves the selection alone.
        let by_pointer = response.interact_pointer_pos().is_some();
        let modifiers = ui.input(|i| i.modifiers);
        if by_pointer && modifiers.shift {
            selection.range_to(entry.photo, order);
        } else if by_pointer && modifiers.command {
            let was_active_selected = entry.active && selection.contains(entry.photo);
            selection.toggle(entry.photo);
            if was_active_selected
                && !selection.contains(entry.photo)
                && let Some(next) = selection.fallback_after_removal(entry.photo, order)
            {
                // The shown photo left the selection: show the nearest selected one (below first, then above).
                drawn.activate = Some(next);
            }
        } else {
            if by_pointer {
                selection.select_only(entry.photo);
            }
            if !entry.active {
                drawn.activate = Some(entry.photo);
            }
        }
    }
    let painter = ui.painter().clone();
    // Three readable states: selected and active (accent border on the canvas colour), active but
    // not selected (dashed accent outline), selected but inactive (tinted fill and a firm border).
    let fill = match (entry.active, entry.selected) {
        (true, true) => t.canvas,
        (false, true) => t.hover,
        _ => t.dock,
    };
    painter.rect_filled(rect, 3.0, fill);
    match (entry.active, entry.selected) {
        (true, true) => {
            painter.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
            // The shown photo among a group: a dashed frame around its picture in the text colour, so it differs from
            // the selected-inactive firm border and from the keyboard-focus ring by shape as well as colour, and never
            // crosses the filename or the status row (F1). A photo selected alone is not a group (D-48).
            if shown_in_group(entry.active, entry.selected, selection.len()) {
                dashed_outline(&painter, parts.picture.expand(0.75), Stroke::new(1.0, t.text));
            }
        }
        (true, false) => dashed_outline(&painter, rect.shrink(1.5), Stroke::new(1.5, t.accent)),
        (false, true) => {
            painter.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(1.5, t.text), egui::StrokeKind::Inside);
        }
        (false, false) if response.hovered() => {
            painter.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
        }
        _ => {}
    }
    if response.has_focus() {
        painter.rect_stroke(rect.expand(1.0), 3.0, Stroke::new(1.5, t.text), egui::StrokeKind::Inside);
        // Claim the arrows along the strip so egui's own focus search does not move focus first.
        let filter = if view.grid {
            egui::EventFilter { vertical_arrows: true, horizontal_arrows: true, ..Default::default() }
        } else {
            match view.orientation {
                Orientation::Vertical => egui::EventFilter { vertical_arrows: true, ..Default::default() },
                Orientation::Horizontal => egui::EventFilter { horizontal_arrows: true, ..Default::default() },
            }
        };
        ui.memory_mut(|m| m.set_focus_lock_filter(response.id, filter));
    }
    if response.gained_focus() {
        response.scroll_to_me(None);
        ui.ctx().request_repaint();
    }
    drawn.ids.push(response.id);
    drawn.photos.push(entry.photo);
    match entry.texture {
        Some(texture) => {
            painter.image(texture, parts.picture, entry.uv, Color32::WHITE);
        }
        None => {
            painter.rect_filled(parts.picture, 2.0, t.canvas);
        }
    }
    if let Some(row) = parts.name {
        // Fitted by its drawn width (not a character count) and clipped to its own row, so a wide name never leaves its
        // card or covers a neighbour (F4 lab L1); the tooltip keeps the whole name.
        let font = FontId::proportional(11.0);
        let label = fit_name_to_width(&entry.name, row.width(), |text| painter.layout_no_wrap(text.to_owned(), font.clone(), t.text).size().x);
        painter.with_clip_rect(row.intersect(painter.clip_rect())).text(row.center(), Align2::CENTER_CENTER, label, font, t.text);
    }
    let adjustments_shown = view.show_edit_badges;
    paint_status(ui, &painter, t, &parts, entry, adjustments_shown);
    let adjustments = adjustments_shown.then_some(entry.adjustments);
    response.widget_info(|| {
        let label = accessible_label(&entry.name, entry.rating, adjustments, entry.active);
        let label = if entry.filtered_out { format!("{label}, {}", tl!("Shown; hidden by filter")) } else { label };
        let label = if entry.pending_xmp { format!("{label}, {}", tl!("Changes not yet saved to XMP")) } else { label };
        let label = if entry.marked { format!("{label}, {}", tl!("Marked")) } else { label };
        let label = if entry.label.is_empty() { label } else { format!("{label}, {}: {}", tl!("Label"), entry.label) };
        WidgetInfo::selected(WidgetType::SelectableLabel, true, entry.selected, label)
    });
    response.clone().on_hover_text(card_tooltip(entry, &parts, response.hover_pos(), adjustments_shown));
    // Freeze identities when opening the context menu: later selection or preview changes must not retarget an action.
    let context_id = response.id.with("settings-target");
    if response.secondary_clicked() {
        ui.ctx().data_mut(|data| data.insert_temp(context_id, SettingsTarget::capture(selection, entry)));
    }
    response.context_menu(|ui| {
        let target = ui.ctx().data_mut(|data| data.get_temp_mut_or_insert_with(context_id, || SettingsTarget::capture(selection, entry)).clone());
        photo_menu(ui, view, selection, entry, Some(&target), &mut drawn.actions);
    });
    let save = ui.interact(save_rect, ui.id().with("jpeg-save"), Sense::click());
    let more = ui.interact(menu_rect, ui.id().with("photo-actions"), Sense::click());
    save.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("{} {}", tl!("Save JPEG"), entry.name)));
    more.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("{} {}", tl!("Photo actions"), entry.name)));
    if save.clicked() {
        drawn.actions.push(PhotoAction::SaveJpeg(entry.photo));
    }
    egui::Popup::menu(&more).show(|ui| photo_menu(ui, view, selection, entry, None, &mut drawn.actions));
    let show_actions = response.hovered()
        || response.has_focus()
        || save.hovered()
        || save.has_focus()
        || more.hovered()
        || more.has_focus()
        || egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&more));
    if show_actions {
        for (r, hovered, focused) in [(save_rect, save.hovered(), save.has_focus()), (menu_rect, more.hovered(), more.has_focus())] {
            painter.rect_filled(r, t.radius_sm, if hovered { t.hover } else { t.chrome });
            painter.rect_stroke(r, t.radius_sm, Stroke::new(1.0, if focused { t.accent } else { t.card_border }), egui::StrokeKind::Inside);
        }
        painter.text(save_rect.center(), Align2::CENTER_CENTER, tl!("Save"), FontId::proportional(11.0), t.text);
        for dx in [-4.0, 0.0, 4.0] {
            painter.circle_filled(menu_rect.center() + vec2(dx, 0.0), 1.2, t.text);
        }
    }
    save.on_hover_text(tl!("Save this photo as JPEG"));
    more.on_hover_text(tl!("Photo actions"));
    drawn.cards.push((entry.document, rect));
}

fn action_rects(card: Rect) -> [Rect; 2] {
    let menu = Rect::from_min_size(pos2(card.right() - INSET - 24.0, card.top() + INSET), vec2(24.0, 24.0));
    let save = Rect::from_min_size(pos2(menu.left() - 43.0, menu.top()), vec2(41.0, 24.0));
    [save, menu]
}

#[derive(Clone)]
struct SettingsTarget {
    photo: PhotoId,
    targets: Vec<PhotoId>,
    source: Option<(PhotoId, String)>,
    show_sync: bool,
    can_undo: bool,
}

impl SettingsTarget {
    fn capture(selection: &Selection, entry: &Entry) -> Self {
        Self {
            photo: entry.photo,
            targets: selection.targets_for(entry.photo),
            source: entry.settings_source.clone(),
            show_sync: selection.contains(entry.photo) && selection.len() >= 2,
            can_undo: entry.can_undo,
        }
    }
}

fn settings_menu(ui: &mut Ui, target: &SettingsTarget, actions: &mut Vec<PhotoAction>) {
    if target.show_sync {
        let source = target.source.as_ref().filter(|(id, _)| target.targets.contains(id));
        let response = ui.add_enabled(source.is_some(), egui::Button::new(tl!("Sync Settings")));
        let response = match source {
            Some((_, name)) => response.on_hover_text(tl!("Copy settings from {photo} to the selected photos.").replace("{photo}", name)),
            None => response.on_disabled_hover_text(tl!("Select the active photograph to sync its settings.")),
        };
        if response.clicked()
            && let Some((source, _)) = source
        {
            actions.push(PhotoAction::SyncSettings { source: *source, targets: target.targets.clone() });
            ui.close();
        }
    }
    if ui.button(tl!("Reset Settings")).clicked() {
        actions.push(PhotoAction::ResetSettings(target.photo));
        ui.close();
    }
    if ui.button(tl!("Set value…")).clicked() {
        actions.push(PhotoAction::SetValue { targets: target.targets.clone() });
        ui.close();
    }
    if ui.add_enabled(target.can_undo, egui::Button::new(tl!("Undo group edit"))).clicked() {
        actions.push(PhotoAction::UndoGroup);
        ui.close();
    }
    ui.separator();
}

fn photo_menu(ui: &mut Ui, view: &mut FilmstripView, selection: &Selection, entry: &Entry, settings: Option<&SettingsTarget>, actions: &mut Vec<PhotoAction>) {
    let targets = settings.map_or_else(|| selection.targets_for(entry.photo), |target| target.targets.clone());
    let scope = if targets.len() == 1 { entry.name.clone() } else { format!("{} {}", targets.len(), tl!("selected photos")) };
    ui.label(egui::RichText::new(scope).strong());
    if let Some(settings) = settings {
        settings_menu(ui, settings, actions);
    }
    if ui.button(format!("{} {}", tl!("Save JPEG:"), entry.name)).clicked() {
        actions.push(PhotoAction::SaveJpeg(entry.photo));
        ui.close();
    }
    ui.separator();
    ui.menu_button(tl!("Rating"), |ui| {
        for rating in 0..=5 {
            let label = if rating == 0 { tl!("Unrated (0)").into() } else { format!("{rating} ★ ({rating})") };
            if ui.button(label).clicked() {
                actions.push(PhotoAction::SetRating { targets: targets.clone(), rating });
                ui.close();
            }
        }
    });
    ui.menu_button(tl!("Colour label"), |ui| {
        for (label, title) in [
            ("", tl!("No Label")),
            ("Select", tl!("Red — Select (6)")),
            ("Second", tl!("Yellow — Second (7)")),
            ("Approved", tl!("Green — Approved (8)")),
            ("Review", tl!("Blue — Review (9)")),
            ("To Do", tl!("Purple — To Do (Ctrl+Shift+0)")),
        ] {
            if ui.button(title).clicked() {
                actions.push(PhotoAction::SetLabel { targets: targets.clone(), label: label.into() });
                ui.close();
            }
        }
    });
    for (marked, label) in [(true, tl!("Mark")), (false, tl!("Unmark"))] {
        if ui.button(label).clicked() {
            actions.push(PhotoAction::SetMark { targets: targets.clone(), marked });
            ui.close();
        }
    }
    if ui.button(tl!("Hide")).clicked() {
        actions.push(PhotoAction::Hide { targets: targets.clone() });
        ui.close();
    }
    if ui.button(tl!("Show hidden photos")).clicked() {
        actions.push(PhotoAction::ShowHidden);
        ui.close();
    }
    if ui.button(tl!("Remove…")).clicked() {
        actions.push(PhotoAction::Remove { targets });
        ui.close();
    }
    ui.separator();
    ui.label(tl!("Show:"));
    widgets::checkbox(ui, &mut view.show_filenames, tl!("Name"));
    widgets::checkbox(ui, &mut view.show_edit_badges, tl!("Adjustments"));
    ui.separator();
    orientation_menu(ui, view);
}

/// The status row: the crop glyph (left), the five-star rating (centre) and the adjustments glyph (right), all outside the
/// picture. Every state reads differently: a known rating fills its stars; unrated shows five hollow stars; a rating not
/// read yet shows five faint dots, never stars; rejected shows the "ban" glyph. The crop glyph is always faint: the
/// open-time review has no crop yet. Adjustments: solid when edited, faint when none, fainter still when unknown.
fn paint_status(ui: &Ui, painter: &egui::Painter, t: &Tokens, parts: &CardParts, entry: &Entry, adjustments_shown: bool) {
    let faint = t.text_faint;
    if entry.marked {
        crate::icons::paint(ui, parts.crop, "check", ICON, t.accent);
    } else {
        crate::icons::paint(ui, parts.crop, "crop", ICON, faint.gamma_multiply(0.7));
    }
    let label_color = match ColorFilter::from_label(&entry.label) {
        ColorFilter::Red => photocraft_doc::LabelColor::Red,
        ColorFilter::Yellow => photocraft_doc::LabelColor::Yellow,
        ColorFilter::Green => photocraft_doc::LabelColor::Green,
        ColorFilter::Blue => photocraft_doc::LabelColor::Blue,
        ColorFilter::Purple => photocraft_doc::LabelColor::Violet,
        ColorFilter::Other => photocraft_doc::LabelColor::Gray,
        ColorFilter::Any | ColorFilter::None => photocraft_doc::LabelColor::None,
    };
    if let Some((color, _)) = t.layer_label_colors(label_color) {
        painter.rect_filled(Rect::from_min_size(parts.picture.left_bottom() - vec2(0.0, 3.0), vec2(parts.picture.width(), 3.0)), 0.0, color);
    }
    if entry.pending_xmp {
        painter.circle_filled(parts.adjustments.right_bottom() - vec2(1.0, 1.0), 2.5, t.warning);
    }
    let star_at = |i: usize| Rect::from_center_size(pos2(parts.stars.left() + STAR_PITCH * (i as f32 + 0.5), parts.stars.center().y), vec2(STAR, STAR));
    match entry.rating {
        PhotoRating::Unknown => {
            for i in 0..5 {
                painter.circle_filled(star_at(i).center(), 1.5, faint.gamma_multiply(0.7));
            }
        }
        PhotoRating::Rejected => crate::icons::paint(ui, Rect::from_center_size(parts.stars.center(), vec2(ICON, ICON)), "ban", ICON, faint),
        PhotoRating::Unrated | PhotoRating::Stars(_) => {
            let filled = if let PhotoRating::Stars(n) = entry.rating { usize::from(n.min(5)) } else { 0 };
            for i in 0..5 {
                let (name, tint) = if i < filled { ("star-filled", t.text) } else { ("star", t.icon) };
                crate::icons::paint(ui, star_at(i), name, STAR, tint);
            }
        }
    }
    if adjustments_shown {
        let tint = match entry.adjustments {
            Adjustments::Edited => t.text,
            Adjustments::None => faint,
            Adjustments::Unknown => faint.gamma_multiply(0.5),
        };
        crate::icons::paint(ui, parts.adjustments, "sliders-horizontal", ICON, tint);
    }
}

/// Splitter input, read from last frame's rectangle *before* the layout so a drag reflows the
/// preview in the same frame. `previous` is that rectangle with the orientation it was laid out
/// for; a stale one (the orientation just changed) is ignored. Returns whether it is hovered,
/// dragged or focused.
pub(super) fn splitter_input(ui: &mut Ui, previous: Option<(Rect, Orientation)>, body: Rect, view: &mut FilmstripView) -> bool {
    let Some((rect, laid_out_for)) = previous else { return false };
    if laid_out_for != view.orientation {
        return false;
    }
    let vertical = laid_out_for == Orientation::Vertical;
    let response = ui.interact(rect, ui.id().with("cr-filmstrip-splitter"), Sense::click_and_drag());
    if response.drag_started() || response.clicked() {
        response.request_focus();
    }
    if response.gained_focus() {
        // egui honours the focus-lock filter below only from the second focused frame, so
        // paint once more before the first key can arrive.
        ui.ctx().request_repaint();
    }
    let hot = response.hovered() || response.dragged() || response.has_focus();
    // The cursor follows the pointer only; keyboard focus alone must not pin it.
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(if vertical { egui::CursorIcon::ResizeHorizontal } else { egui::CursorIcon::ResizeVertical });
    }
    if let Some(pointer) = response.interact_pointer_pos().filter(|_| response.dragged()) {
        let _ = if vertical { view.set_width(pointer.x - body.left()) } else { view.set_height(body.bottom() - pointer.y) };
    }
    if response.has_focus() {
        let filter = if vertical {
            egui::EventFilter { horizontal_arrows: true, ..Default::default() }
        } else {
            egui::EventFilter { vertical_arrows: true, ..Default::default() }
        };
        ui.memory_mut(|m| m.set_focus_lock_filter(response.id, filter));
        // Left/Right narrow and widen a vertical strip; Up/Down grow and shrink a horizontal one.
        let keys = if vertical { [(Key::ArrowLeft, -KEY_STEP), (Key::ArrowRight, KEY_STEP)] } else { [(Key::ArrowDown, -KEY_STEP), (Key::ArrowUp, KEY_STEP)] };
        for (key, step) in keys {
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, key)) {
                let _ = if vertical { view.set_width(view.preferred_width + step) } else { view.set_height(view.preferred_height + step) };
            }
        }
    }
    let extent = if vertical { rect.center().x - body.left() } else { body.bottom() - rect.center().y };
    response.widget_info(|| WidgetInfo::slider(true, f64::from(extent), tl!("Resize panels")));
    hot
}

pub(super) fn paint_splitter(ui: &Ui, rect: Rect, t: &Tokens, hot: bool, orientation: Orientation) {
    let (width, color) = if hot { (2.0, t.accent) } else { (1.0, t.separator) };
    let line = if orientation == Orientation::Vertical { [rect.center_top(), rect.center_bottom()] } else { [rect.left_center(), rect.right_center()] };
    ui.painter().line_segment(line, Stroke::new(width, color));
}

/// The show/hide control. Its place never depends on the strip, so it can be read before the
/// layout. A primary click toggles visibility; a secondary click opens the Orientation menu and
/// never toggles. Returns the response and the menu entries while the menu is open.
pub(super) fn toggle_input(ui: &mut Ui, rect: Rect, view: &mut FilmstripView) -> (Response, Vec<(Orientation, Rect)>) {
    let response = ui.interact(rect, ui.id().with("cr-filmstrip-toggle"), Sense::click());
    if response.clicked() {
        view.visible = !view.visible;
    }
    let name = if view.visible { tl!("Collapse") } else { tl!("Expand") };
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, name));
    response.clone().on_hover_text(name);
    let mut menu = Vec::new();
    response.context_menu(|ui| menu = orientation_menu(ui, view));
    (response, menu)
}

/// What a control that is not available yet says: "Grid: Not available yet".
pub(super) fn unavailable_text(name: &str) -> String {
    format!("{name}: {}", tl!("Not available yet"))
}

/// A four-square view toggle. Orientation and thumbnail dimensions are retained.
pub(super) fn grid_control(ui: &mut Ui, t: &Tokens, grid: Rect, view: &mut FilmstripView) {
    let name = tl!("Grid");
    let response = ui.interact(grid, ui.id().with("cr-grid"), Sense::click());
    if response.clicked() {
        view.grid = !view.grid;
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, view.grid, name));
    if view.grid || response.hovered() {
        ui.painter().rect_filled(grid, t.radius_sm, if view.grid { t.accent_soft } else { t.hover });
    }
    crate::icons::paint(ui, grid, "grid-2x2", 14.0, if view.grid { t.accent_text } else { t.icon });
    if response.has_focus() {
        ui.painter().rect_stroke(grid, t.radius_sm, Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
    }
    response.on_hover_text(if view.grid { tl!("Return to photo preview") } else { tl!("Show grid") });
}

pub(super) fn paint_toggle(ui: &Ui, rect: Rect, t: &Tokens, view: &FilmstripView, response: &Response) {
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, 3.0, t.hover);
    }
    let glyph = Rect::from_center_size(rect.center(), vec2(18.0, 14.0));
    // The accent block sits on the side the strip occupies.
    let block = match view.orientation {
        Orientation::Vertical => Rect::from_min_size(glyph.min, vec2(6.0, glyph.height())),
        Orientation::Horizontal => Rect::from_min_size(pos2(glyph.left(), glyph.bottom() - 5.0), vec2(glyph.width(), 5.0)),
    };
    if view.visible {
        painter.rect_filled(block, 2.0, t.accent);
    }
    painter.rect_stroke(glyph, 2.0, Stroke::new(1.5, t.icon), egui::StrokeKind::Inside);
    let divider = match view.orientation {
        Orientation::Vertical => [block.right_top(), block.right_bottom()],
        Orientation::Horizontal => [block.left_top(), block.right_top()],
    };
    painter.line_segment(divider, Stroke::new(1.5, t.icon));
    if response.has_focus() {
        painter.rect_stroke(rect, 3.0, Stroke::new(1.5, t.text), egui::StrokeKind::Inside);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(w: f32) -> Rect {
        Rect::from_min_size(pos2(0.0, 30.0), vec2(w, 800.0))
    }

    fn horizontal() -> FilmstripView {
        FilmstripView { orientation: Orientation::Horizontal, ..Default::default() }
    }

    #[test]
    fn hidden_or_absent_strip_owns_no_rectangle_splitter_or_gutter() {
        for orientation in [Orientation::Vertical, Orientation::Horizontal] {
            for (visible, present) in [(false, true), (true, false), (false, false)] {
                let view = FilmstripView { visible, orientation, ..Default::default() };
                let l = layout(body(1360.0), 330.0, &view, present);
                assert!(l.strip.is_none() && l.splitter.is_none(), "{orientation:?} {visible} {present}");
                assert_eq!((l.preview_body.left(), l.preview_body.right(), l.preview_body.bottom()), (0.0, 1030.0, 830.0));
            }
        }
        let shown = layout(body(1360.0), 330.0, &FilmstripView::default(), true);
        assert_eq!(shown.strip.map(|s| s.width()), Some(DEFAULT_WIDTH));
        assert_eq!(shown.preview_body.left(), DEFAULT_WIDTH);
    }

    #[test]
    fn width_clamps_to_limits_and_to_the_room_the_window_leaves() {
        assert_eq!(effective_width(10.0, 1360.0, 330.0), MIN_WIDTH);
        assert_eq!(effective_width(900.0, 1360.0, 330.0), MAX_WIDTH);
        // A 900 wide body leaves 900 - 330 - MIN_PREVIEW_WIDTH of room, so a wide preference is drawn smaller.
        assert_eq!(effective_width(300.0, 900.0, 330.0), 900.0 - 330.0 - MIN_PREVIEW_WIDTH);
        assert_eq!(effective_width(f32::NAN, 1360.0, 330.0), DEFAULT_WIDTH);
        for w in [-5.0, 0.0, 1.0, 100.0, 449.0, f32::NAN] {
            let l = layout(body(w), 330.0, &FilmstripView::default(), true);
            assert!(l.strip.is_none(), "no usable strip fits a {w} wide body");
            assert!(l.preview_body.width() >= 1.0 && l.preview_body.width().is_finite(), "{w}");
        }
    }

    #[test]
    fn shrinking_the_window_never_erases_the_preferred_width() {
        let mut view = FilmstripView::default();
        view.set_width(280.0).unwrap();
        assert!(layout(body(900.0), 330.0, &view, true).strip.is_some_and(|s| s.width() == 900.0 - 330.0 - MIN_PREVIEW_WIDTH));
        assert_eq!(view.preferred_width, 280.0);
        assert!(layout(body(1360.0), 330.0, &view, true).strip.is_some_and(|s| s.width() == 280.0));
        assert!(view.set_width(f32::NAN).is_err() && view.set_width(f32::INFINITY).is_err());
        assert_eq!(view.preferred_width, 280.0);
    }

    #[test]
    fn splitter_straddles_the_strip_edge_with_a_generous_hit_target() {
        let l = layout(body(1360.0), 330.0, &FilmstripView::default(), true);
        let (strip, splitter) = (l.strip.unwrap(), l.splitter.unwrap());
        assert!(splitter.width() >= 12.0 && splitter.contains(strip.right_center()));
        assert_eq!((splitter.top(), splitter.bottom()), (strip.top(), strip.bottom()));
    }

    #[test]
    fn a_horizontal_strip_spans_the_preview_column_only_never_the_settings_panel() {
        let l = layout(body(1360.0), 330.0, &horizontal(), true);
        let (strip, splitter) = (l.strip.unwrap(), l.splitter.unwrap());
        assert_eq!(
            (strip.left(), strip.right(), strip.bottom(), strip.height()),
            (0.0, 1030.0, 830.0, DEFAULT_HEIGHT),
            "it ends where the settings panel begins"
        );
        assert_eq!((l.preview_body.left(), l.preview_body.right(), l.preview_body.top(), l.preview_body.bottom()), (0.0, 1030.0, 30.0, strip.top()));
        assert!(splitter.height() >= 12.0 && splitter.contains(strip.center_top()) && (splitter.left(), splitter.right()) == (strip.left(), strip.right()));
    }

    #[test]
    fn height_clamps_to_limits_to_the_room_and_the_preferred_height_is_never_erased() {
        assert_eq!(effective_height(10.0, 800.0), MIN_HEIGHT);
        assert_eq!(effective_height(900.0, 800.0), MAX_HEIGHT);
        assert_eq!(effective_height(240.0, 500.0), 500.0 - MIN_PREVIEW_HEIGHT);
        assert_eq!(effective_height(240.0, 380.0), 0.0, "no room left for a usable strip");
        assert_eq!(effective_height(f32::NAN, 800.0), DEFAULT_HEIGHT);
        let mut view = horizontal();
        view.set_height(200.0).unwrap();
        assert!(layout(Rect::from_min_size(pos2(0.0, 30.0), vec2(1360.0, 480.0)), 330.0, &view, true).strip.is_some_and(|s| s.height() == 180.0));
        assert_eq!(view.preferred_height, 200.0);
        assert!(view.set_height(f32::NAN).is_err());
        assert_eq!(
            layout(Rect::from_min_size(pos2(0.0, 30.0), vec2(500.0, 800.0)), 330.0, &horizontal(), true).strip,
            None,
            "too narrow a preview column for a horizontal strip"
        );
    }

    #[test]
    fn width_and_height_are_independent_and_survive_an_orientation_round_trip() {
        let mut view = FilmstripView::default();
        view.set_width(300.0).unwrap();
        view.set_height(220.0).unwrap();
        view.orientation = Orientation::Horizontal;
        view.visible = false;
        view.orientation = Orientation::Vertical;
        assert_eq!((view.preferred_width, view.preferred_height, view.visible), (300.0, 220.0, false));
    }

    /// F1 (lab cycle 3): a card is as tall as its content: no square slot, no phantom rows.
    #[test]
    fn cards_are_as_tall_as_their_content_and_denser_than_the_square_slot() {
        let landscape = (6048, 4024);
        let (named, picture) = card_size(DEFAULT_WIDTH, landscape, true);
        let (bare, _) = card_size(DEFAULT_WIDTH, landscape, false);
        assert_eq!(named.y - bare.y, NAME_ROW + ROW_GAP, "no filename row is reserved when names are hidden");
        assert!((picture.y - picture.x * 4024.0 / 6048.0).abs() < 0.01, "the picture keeps its own aspect");
        assert_eq!(named.y, INSET + picture.y + ROW_GAP + NAME_ROW + ROW_GAP + STATUS_ROW + INSET);
        // The old square slot at the default width: 156 + 16 + 18 = 190 pt per card (plus a 6 pt gap); now well below.
        assert!(named.y + CARD_GAP < 160.0, "{named:?}");
        // A portrait is held to a 4:3 box, an unknown size to 3:2.
        assert_eq!(card_size(DEFAULT_WIDTH, (4000, 6000), true).1.y, picture.x * MAX_PICTURE_ASPECT);
        assert_eq!(card_size(DEFAULT_WIDTH, (0, 0), true).1.y, picture.x * DEFAULT_PICTURE_ASPECT);
        // A wide strip does not grow the picture past MAX_PICTURE_WIDTH; a narrow one still fits the status row.
        assert_eq!(card_widths(MAX_WIDTH).1, MAX_PICTURE_WIDTH);
        assert!(card_widths(MIN_WIDTH).0 >= min_card_width());
        // Horizontal: the strip's height sets the card's; every card is that tall.
        let (h1, _) = card_size_h(DEFAULT_HEIGHT, landscape, true);
        let (h2, _) = card_size_h(DEFAULT_HEIGHT, (4000, 6000), true);
        assert!(h1.y == h2.y && h1.y <= DEFAULT_HEIGHT - 2.0 * STRIP_PAD, "{h1:?} {h2:?}");
        assert!(h1.x > h2.x && h2.x >= min_card_width());
    }

    /// F1: the status row sits outside the picture; crop left, stars centred, adjustments right, all inside the card and
    /// clear of each other at the narrowest card.
    #[test]
    fn the_status_row_is_outside_the_picture_and_its_parts_never_overlap() {
        for (width, size) in [(MIN_WIDTH, (6048, 4024)), (DEFAULT_WIDTH, (4000, 6000)), (MAX_WIDTH, (6000, 2000))] {
            let (card, picture_box) = card_size(width, size, true);
            let rect = Rect::from_min_size(pos2(0.0, 0.0), card);
            let p = card_parts(rect, picture_box, size, true);
            assert!(rect.contains_rect(p.picture) && rect.contains_rect(p.crop) && rect.contains_rect(p.stars) && rect.contains_rect(p.adjustments), "{width}");
            assert!(p.crop.top() >= p.picture.bottom() && p.stars.top() >= p.picture.bottom(), "{width}: below the picture");
            assert!(p.crop.right() <= p.stars.left() && p.stars.right() <= p.adjustments.left(), "{width}: no overlap");
            assert!(p.name.is_some_and(|n| n.top() >= p.picture.bottom() && n.bottom() <= p.stars.top()), "{width}: the name row between");
        }
        assert_eq!(fit_rect(Rect::from_min_size(pos2(0.0, 0.0), vec2(150.0, 100.0)), (0, 10)).width(), 150.0, "an unknown size fills the box");
    }

    /// F1: unknown is never said as unrated; every state has its own words, and the accessible name carries them.
    #[test]
    fn ratings_and_adjustments_are_named_truthfully() {
        let texts = [PhotoRating::Unknown, PhotoRating::Unrated, PhotoRating::Stars(3), PhotoRating::Rejected].map(rating_text);
        for (i, a) in texts.iter().enumerate() {
            for b in texts.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        assert_eq!(rating_text(PhotoRating::Stars(3)), "Rating: 3 of 5");
        assert_ne!(adjustments_text(Adjustments::Unknown), adjustments_text(Adjustments::None));
        assert_eq!(accessible_label("a.dng", PhotoRating::Unrated, None, false), "a.dng, Unrated");
        assert_eq!(accessible_label("a.dng", PhotoRating::Stars(5), Some(Adjustments::Edited), true), "a.dng, Rating: 5 of 5, Adjustments, Preview: Active");
    }

    /// D-48 (F1 follow-up, lab cycle 4): the dashed frame marks the shown photo only among two or more selected photos;
    /// a photo selected alone is simply the selected, shown one.
    #[test]
    fn the_group_frame_marks_the_shown_photo_only_within_a_group() {
        assert!(!shown_in_group(true, true, 1), "alone is not a group");
        assert!(shown_in_group(true, true, 2) && shown_in_group(true, true, 30));
        assert!(!shown_in_group(false, true, 3) && !shown_in_group(true, false, 3));
        assert_eq!(unavailable_text("Grid"), "Grid: Not available yet");
    }

    /// L1 (F4 lab): names are fitted by drawn width: a fitting name is kept whole; a wide one becomes the longest
    /// shortened form that fits, keeping its extension; one wide glyph per character is accounted for.
    #[test]
    fn names_are_fitted_by_drawn_width_not_by_character_count() {
        let narrow = |s: &str| s.chars().count() as f32 * 5.0;
        let wide = |s: &str| s.chars().map(|c| if c == 'W' || !c.is_ascii() { 11.0 } else { 5.0 }).sum::<f32>();
        assert_eq!(fit_name_to_width("IMG_0001.dng", 100.0, narrow), "IMG_0001.dng");
        for name in ["WWWWWWWWWWWWWWWWWWWWWWWWWWWWWW_WIDE.dng", "日本語の長いファイル名のテスト写真.dng"] {
            let fitted = fit_name_to_width(name, 100.0, wide);
            assert!(wide(&fitted) <= 100.0 && fitted.ends_with(".dng") && fitted.contains('…'), "{fitted}");
            let one_more = fit_name(name, fitted.chars().count() + 1);
            assert!(wide(&one_more) > 100.0, "{fitted} is the longest that fits");
        }
    }

    fn entry(name: &str, rating: PhotoRating) -> Entry {
        Entry {
            document: DocId(0),
            photo: PhotoId::fresh(),
            name: name.into(),
            texture: None,
            uv: Rect::ZERO,
            size: (3, 2),
            active: false,
            selected: false,
            settings_source: None,
            can_undo: false,
            adjustments: Adjustments::Unknown,
            rating,
            label: String::new(),
            marked: false,
            pending_xmp: false,
            filtered_out: false,
        }
    }

    #[test]
    fn settings_menu_snapshots_selected_targets_and_active_source_without_retargeting_reset() {
        let mut clicked = entry("clicked.nef", PhotoRating::Unrated);
        let source = PhotoId::fresh();
        let hidden = PhotoId::fresh();
        let mut selection = Selection::default();
        selection.select_all(&[source, clicked.photo, hidden]);
        clicked.settings_source = Some((source, "active.nef".into()));
        clicked.can_undo = true;
        let target = SettingsTarget::capture(&selection, &clicked);
        assert!(target.show_sync && target.can_undo);
        assert_eq!(target.targets, vec![source, clicked.photo, hidden], "hidden selected members remain targets");
        assert_eq!(target.photo, clicked.photo, "Reset Settings targets the clicked photo only");
        assert_eq!(target.source, Some((source, "active.nef".into())), "right-click does not select its own sync source");
        selection.select_only(PhotoId::fresh());
        clicked.settings_source = None;
        assert_eq!(target.targets, vec![source, clicked.photo, hidden], "an open menu cannot retarget after selection changes");
        assert_eq!(target.source, Some((source, "active.nef".into())));
        let outside = SettingsTarget::capture(&selection, &clicked);
        assert!(!outside.show_sync);
        assert_eq!(outside.targets, vec![clicked.photo]);
        selection.select_only(clicked.photo);
        assert!(!SettingsTarget::capture(&selection, &clicked).show_sync, "a one-photo selection has nothing to synchronize");
    }

    /// S1 (F4 lab): natural, case-insensitive name order; rating order rejected → unrated → 1…5 stars, reversed by
    /// Reverse; a rating not read yet always last; ties keep the open order in both directions; open order reverses.
    #[test]
    fn sorting_is_natural_stable_and_never_ranks_an_unread_rating() {
        let names = |e: &[Entry]| e.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        let mut e = vec![entry("img_10.dng", PhotoRating::Unknown), entry("IMG_2.dng", PhotoRating::Stars(3)), entry("img_1.dng", PhotoRating::Stars(3))];
        sort_entries(&mut e, SortKey::FileName, false);
        assert_eq!(names(&e), ["img_1.dng", "IMG_2.dng", "img_10.dng"]);
        sort_entries(&mut e, SortKey::FileName, true);
        assert_eq!(names(&e), ["img_10.dng", "IMG_2.dng", "img_1.dng"]);
        let open = || {
            vec![
                entry("a", PhotoRating::Stars(3)),
                entry("b", PhotoRating::Unknown),
                entry("c", PhotoRating::Rejected),
                entry("d", PhotoRating::Stars(5)),
                entry("e", PhotoRating::Unrated),
                entry("f", PhotoRating::Stars(3)),
            ]
        };
        let mut e = open();
        sort_entries(&mut e, SortKey::StarRating, false);
        assert_eq!(names(&e), ["c", "e", "a", "f", "d", "b"]);
        let mut e = open();
        sort_entries(&mut e, SortKey::StarRating, true);
        assert_eq!(names(&e), ["d", "a", "f", "e", "c", "b"], "ties keep their open order; unread stays last");
        let mut e = open();
        sort_entries(&mut e, SortKey::OpenOrder, true);
        assert_eq!(names(&e), ["f", "e", "d", "c", "b", "a"]);
        let mut e = open();
        sort_entries(&mut e, SortKey::OpenOrder, false);
        assert_eq!(names(&e), ["a", "b", "c", "d", "e", "f"]);
    }

    /// F1-R1 (review R4): the photo shown although the filter would hide it says so in its tooltip as well as its name.
    #[test]
    fn the_shown_photo_kept_by_the_filter_says_so_in_its_tooltip() {
        let mut kept = entry("a.dng", PhotoRating::Stars(2));
        kept.filtered_out = true;
        let parts = card_parts(Rect::from_min_size(pos2(0.0, 0.0), vec2(170.0, 148.0)), vec2(162.0, 108.0), (3, 2), true);
        let tip = card_tooltip(&kept, &parts, None, true);
        assert!(tip.starts_with("a.dng") && tip.contains("Shown; hidden by filter"), "{tip}");
    }

    /// F1-R1 (review R7): fitting never underflows or loops, even for an empty name in a row with no room.
    #[test]
    fn fitting_an_empty_name_into_no_room_terminates() {
        assert_eq!(fit_name_to_width("", -1.0, |_| 0.0), "");
        assert!(fit_name_to_width("abc.dng", 0.0, |s| s.chars().count() as f32).chars().count() <= 1);
    }

    #[test]
    fn names_are_shortened_without_losing_the_extension() {
        assert_eq!(fit_name("a.dng", 16), "a.dng");
        let long = fit_name("a-very-long-example-photo-name.NEF", 14);
        assert!(long.chars().count() <= 14 && long.ends_with(".NEF") && long.contains('…'), "{long}");
        assert_eq!(fit_name("日本語の長いファイル名のテスト.nef", 8).chars().count(), 8);
        assert!(fit_name("", 8).is_empty() && !fit_name("abc", 1).is_empty());
    }

    #[test]
    fn control_requests_validate_every_field_before_changing_anything() {
        let view = FilmstripView::default();
        let ok = prepare(
            view,
            &serde_json::json!({"visible": false, "width": 250, "height": 180, "orientation": "horizontal", "showFilenames": false, "showEditBadges": false}),
        )
        .unwrap();
        assert_eq!(
            (ok.visible, ok.preferred_width, ok.preferred_height, ok.orientation, ok.show_filenames, ok.show_edit_badges),
            (false, 250.0, 180.0, Orientation::Horizontal, false, false)
        );
        for bad in [
            serde_json::json!({"visible": "no"}),
            serde_json::json!({"width": "wide"}),
            serde_json::json!({"width": 1e300}),
            serde_json::json!({"height": "tall"}),
            serde_json::json!({"orientation": "diagonal"}),
            serde_json::json!({"orientation": 1}),
            serde_json::json!({"nope": 1}),
            serde_json::json!([1]),
        ] {
            assert!(prepare(view, &bad).is_err(), "{bad}");
        }
        assert_eq!(prepare(view, &serde_json::json!({"width": 5})).unwrap().preferred_width, MIN_WIDTH);
        assert_eq!(prepare(view, &serde_json::json!({"height": 5})).unwrap().preferred_height, MIN_HEIGHT);
        assert_eq!(view.orientation, Orientation::Vertical, "a rejected or partial request leaves the live view alone");
    }

    #[test]
    fn filters_combine_stars_literal_colour_and_independent_mark() {
        let mut filter = RatingFilter { stars: StarComparison::AtLeast(3), color: ColorFilter::Yellow, mark: MarkFilter::Marked, ..Default::default() };
        assert!(filter.shows_metadata(PhotoRating::Stars(3), "Second", true));
        assert!(filter.shows_metadata(PhotoRating::Stars(5), "Yellow", true), "legacy Yellow is recognised without rewriting it");
        assert!(!filter.shows_metadata(PhotoRating::Stars(2), "Second", true));
        assert!(!filter.shows_metadata(PhotoRating::Stars(4), "Review", true));
        assert!(!filter.shows_metadata(PhotoRating::Stars(4), "Second", false));
        filter.stars = StarComparison::AtMost(2);
        filter.mark = MarkFilter::Unmarked;
        assert!(filter.shows_metadata(PhotoRating::Unrated, "Yellow", false));
        assert!(!filter.shows_metadata(PhotoRating::Unknown, "Yellow", false));
        assert!(!filter.shows_metadata(PhotoRating::Rejected, "Yellow", false));
        assert!(filter_summary(filter).contains("≤ 2"));
    }

    #[test]
    fn view_roundtrip_preserves_grid_preferences_and_validated_filter_controls() {
        let original = FilmstripView::default();
        let next = prepare(
            original,
            &serde_json::json!({"grid": true, "orientation": "horizontal", "filter": {
                "stars": {"comparison": ">=", "value": 2}, "color": "blue", "mark": "marked"
            }}),
        )
        .unwrap();
        assert!(next.grid);
        assert_eq!(next.orientation, Orientation::Horizontal);
        assert!(next.filter.shows_metadata(PhotoRating::Stars(3), "Review", true));
        let encoded = serde_json::to_vec(&next).unwrap();
        assert_eq!(serde_json::from_slice::<FilmstripView>(&encoded).unwrap(), next);
        assert_eq!(serde_json::from_str::<FilmstripView>("{}").unwrap(), original);
        for value in [
            serde_json::json!({"filter": {"stars": {"comparison": ">=", "value": 6}}}),
            serde_json::json!({"filter": {"stars": {"comparison": "<", "value": 2}}}),
            serde_json::json!({"filter": {"color": "cyan"}}),
            serde_json::json!({"filter": {"mark": true}}),
            serde_json::json!({"grid": "yes"}),
        ] {
            assert!(prepare(original, &value).is_err(), "{value}");
        }
        let grid_layout = layout(body(1360.0), 320.0, &next, true);
        assert!(grid_layout.strip.is_none() && grid_layout.splitter.is_none());
        assert_eq!(grid_layout.preview_body.width(), 1040.0);
    }

    #[test]
    fn metadata_shortcuts_match_product_labels_and_do_not_repurpose_zero() {
        let targets = vec![PhotoId(1), PhotoId(8)];
        assert_eq!(metadata_shortcut(Key::Num0, Modifiers::NONE, targets.clone()), Some(PhotoAction::SetRating { targets: targets.clone(), rating: 0 }));
        for (key, label) in [(Key::Num6, "Select"), (Key::Num7, "Second"), (Key::Num8, "Approved"), (Key::Num9, "Review")] {
            assert_eq!(metadata_shortcut(key, Modifiers::NONE, targets.clone()), Some(PhotoAction::SetLabel { targets: targets.clone(), label: label.into() }));
        }
        assert_eq!(
            metadata_shortcut(Key::Num0, Modifiers { ctrl: true, shift: true, ..Modifiers::NONE }, targets.clone()),
            Some(PhotoAction::SetLabel { targets: targets.clone(), label: "To Do".into() })
        );
        assert!(metadata_shortcut(Key::Num7, Modifiers::SHIFT, targets.clone()).is_none());
        assert!(metadata_shortcut(Key::Num7, Modifiers::COMMAND, targets).is_none());
        assert!(metadata_shortcut(Key::Num1, Modifiers::NONE, Vec::new()).is_none());
    }

    fn grid_frame(ctx: &egui::Context, view: &mut FilmstripView, selection: &mut Selection, entries: &[Entry], events: Vec<egui::Event>) -> Drawn {
        let rect = Rect::from_min_size(egui::Pos2::ZERO, vec2(900.0, 600.0));
        let input = egui::RawInput { screen_rect: Some(rect), events, ..Default::default() };
        let mut result = Drawn::default();
        let mut output = ctx.run_ui(input, |ui| {
            let t = Tokens::get(ui.ctx());
            result = draw_grid(ui, rect, &t, view, selection, entries, &Hidden::default());
        });
        output.textures_delta.clear();
        result
    }

    #[test]
    fn grid_and_hover_save_preserve_group_and_capture_clicked_identity() {
        let ctx = egui::Context::default();
        let mut view = FilmstripView { grid: true, orientation: Orientation::Horizontal, ..Default::default() };
        let entries = vec![entry("a.nef", PhotoRating::Stars(3)), entry("b.nef", PhotoRating::Stars(1)), entry("c.nef", PhotoRating::Unrated)];
        let mut selection = Selection::default();
        selection.select_all(&entries.iter().map(|e| e.photo).collect::<Vec<_>>());
        let before = selection.selected_ids().to_vec();
        let first = grid_frame(&ctx, &mut view, &mut selection, &entries, Vec::new());
        assert_eq!(first.cards.len(), 3);
        assert!(first.actions.is_empty());
        assert!(first.cards[1].1.left() > first.cards[0].1.right());
        let save = action_rects(first.cards[1].1)[0].center();
        grid_frame(&ctx, &mut view, &mut selection, &entries, vec![egui::Event::PointerMoved(save)]);
        let click = |pressed| egui::Event::PointerButton { pos: save, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        grid_frame(&ctx, &mut view, &mut selection, &entries, vec![click(true)]);
        let saved = grid_frame(&ctx, &mut view, &mut selection, &entries, vec![click(false)]);
        assert_eq!(saved.actions, vec![PhotoAction::SaveJpeg(entries[1].photo)]);
        assert_eq!(selection.selected_ids(), before);
        assert_eq!(view.orientation, Orientation::Horizontal);
        assert!(saved.activate.is_none());
    }
}
