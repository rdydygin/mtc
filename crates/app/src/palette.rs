//! mtc's surface palette, layered onto GPUI Kit's light and dark themes.
//!
//! The stock themes paint every surface pure white or pure black. mtc uses a
//! faintly cool, layered set instead: window chrome (`title_bar`, also the
//! backdrop behind the panels) sits a step behind the content cards
//! (`background`), and fields, hovers, and borders step evenly from there.
//! In dark mode cards are *lighter* than the backdrop, the way depth reads
//! in the dark. This is the theme layer, so it is the one place raw colors
//! belong; everything else reads these roles from `cx.theme()`.

use gpui_kit::component::Theme;
use gpui_kit::{App, Hsla, hsla};

/// `hsl(230°, s%, l%)`: one cool hue keeps every surface in the same family.
fn cool(saturation: f32, lightness: f32) -> Hsla {
    hsla(230. / 360., saturation / 100., lightness / 100., 1.)
}

pub fn apply(cx: &mut App) {
    Theme::update(cx, |theme| {
        if theme.is_dark() {
            theme.title_bar = cool(9., 7.);
            theme.background = cool(8., 11.);
            theme.sidebar = cool(8., 11.);
            theme.popover = cool(8., 14.);
            theme.secondary = cool(7., 16.);
            theme.muted = cool(7., 17.);
            theme.accent = cool(7., 18.);
            theme.border = cool(7., 20.);
            theme.input = cool(7., 23.);
        } else {
            theme.title_bar = cool(16., 95.);
            theme.background = cool(30., 99.5);
            theme.sidebar = cool(30., 99.5);
            theme.popover = cool(0., 100.);
            theme.secondary = cool(16., 95.5);
            theme.muted = cool(14., 94.);
            theme.accent = cool(16., 93.);
            theme.border = cool(14., 89.);
            theme.input = cool(14., 86.);
        }
    });
}
