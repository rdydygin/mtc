//! Light/dark appearance and interface zoom.
//!
//! Zoom is the theme's base font size: GPUI Kit derives the window's `rem`
//! from it, so type, spacing, controls, and icons scale together.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Window, px};

use crate::persist::{self, Appearance};

pub const DEFAULT_FONT_SIZE: f32 = 14.;
pub const MIN_FONT_SIZE: f32 = 11.;
pub const MAX_FONT_SIZE: f32 = 22.;

/// Apply the saved appearance and zoom at startup.
pub fn restore(cx: &mut App) {
    let state = persist::load();
    apply(state.appearance, None, cx);
    let size = state.font_size.unwrap_or(DEFAULT_FONT_SIZE);
    Theme::update(cx, |theme| {
        theme.font_size = px(size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE))
    });
}

/// The appearance the user chose.
pub fn current() -> Appearance {
    persist::read_with(|state| state.appearance)
}

pub fn set(appearance: Appearance, cx: &mut App) {
    persist::update(|state| state.appearance = appearance);
    apply(appearance, None, cx);
}

/// Follow a system appearance change when the user chose "System".
pub fn system_appearance_changed(window: &mut Window, cx: &mut App) {
    if persist::load().appearance == Appearance::System {
        apply(Appearance::System, Some(window), cx);
    }
}

fn apply(appearance: Appearance, window: Option<&mut Window>, cx: &mut App) {
    // Changing the theme resets the font size; carry the zoom across.
    let font_size = cx
        .has_global::<Theme>()
        .then(|| Theme::global(cx).font_size);
    match appearance {
        Appearance::System => Theme::sync_system_appearance(window, cx),
        Appearance::Light => Theme::change(ThemeMode::Light, window, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
    if let Some(font_size) = font_size {
        Theme::update(cx, |theme| theme.font_size = font_size);
    }
    crate::palette::apply(cx);
}

pub fn zoom(step: f32, cx: &mut App) {
    let current = Theme::global(cx).font_size.as_f32();
    let next = (current + step).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
    Theme::update(cx, |theme| theme.font_size = px(next));
    persist::update(|state| state.font_size = Some(next));
}

/// The interface zoom, as the base font size in pixels.
pub fn font_size(cx: &App) -> f32 {
    Theme::global(cx).font_size.as_f32()
}

/// Zoom to a base font size, within the supported range.
pub fn set_font_size(size: f32, cx: &mut App) {
    if size.round() == DEFAULT_FONT_SIZE {
        reset_zoom(cx);
        return;
    }
    let size = size.round().clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
    Theme::update(cx, |theme| theme.font_size = px(size));
    persist::update(|state| state.font_size = Some(size));
}

pub fn reset_zoom(cx: &mut App) {
    Theme::update(cx, |theme| theme.font_size = px(DEFAULT_FONT_SIZE));
    persist::update(|state| state.font_size = None);
}
