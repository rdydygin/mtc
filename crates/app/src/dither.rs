//! Shared dither backdrop for sign-in and idle conversations. Only mounted views request frames.
use std::time::Instant;

use gpui_kit::component::{
    ActiveTheme as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

const BAYER: [[f32; 4]; 4] = [
    [0., 8., 2., 10.],
    [12., 4., 14., 6.],
    [3., 11., 1., 9.],
    [15., 7., 13., 5.],
];
const LIFETIME: f32 = 3.;

#[derive(Clone, Copy)]
struct Ripple {
    position: Point<Pixels>,
    born: Instant,
}

pub struct DitherView {
    started: Instant,
    ripples: [Option<Ripple>; 8],
    next: usize,
    conversation_hint: bool,
}

impl DitherView {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            ripples: [None; 8],
            next: 0,
            conversation_hint: false,
        }
    }

    pub fn conversation_hint(mut self) -> Self {
        self.conversation_hint = true;
        self
    }
}

fn wave(distance: f32, age: f32) -> f32 {
    let front = distance - age * 130.;
    (-front * front / 1800.).exp() * (front * 0.08).cos() * (1. - age / LIFETIME).max(0.)
}

// Interpolate around the short arc, including across the hue seam.
fn mix_hue(a: f32, b: f32, amount: f32) -> f32 {
    (a + ((b - a + 0.5).rem_euclid(1.) - 0.5) * amount).rem_euclid(1.)
}

fn mix_color(mut a: Hsla, b: Hsla, amount: f32) -> Hsla {
    a.h = mix_hue(a.h, b.h, amount);
    a.s += (b.s - a.s) * amount;
    a.l += (b.l - a.l) * amount;
    a.a += (b.a - a.a) * amount;
    a
}

impl Render for DitherView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reduced = cx.reduce_motion();
        if !reduced {
            window.request_animation_frame();
        }
        let now = Instant::now();
        let time = if reduced {
            0.
        } else {
            now.duration_since(self.started).as_secs_f32()
        };
        let ripples = self.ripples;
        let theme = cx.theme();
        // Theme palette roles carry this decorative gradient; status colors retain their meaning.
        let stops = [theme.cyan, theme.chart_3, theme.magenta];
        let palette: [Hsla; 9] = std::array::from_fn(|ix| {
            let position = ix as f32 / 4.;
            let segment = (position as usize).min(1);
            let mut color = mix_color(
                stops[segment],
                stops[segment + 1],
                position - segment as f32,
            );
            color.s *= 0.7;
            color.a = 0.32;
            color
        });
        let foreground = theme.foreground;
        let background = cx.theme().background;
        let muted = cx.theme().muted_foreground;
        // Canvas coordinates are resolved pixels; derive the grid from the current rem size.
        let cell = window.rem_size().as_f32() * 0.5;
        div()
            .id("dither-backdrop")
            .relative()
            .size_full()
            .bg(background)
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if !cx.reduce_motion() {
                    this.ripples[this.next] = Some(Ripple {
                        position: event.position(),
                        born: Instant::now(),
                    });
                    this.next = (this.next + 1) % this.ripples.len();
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let cell = cell
                            .max(bounds.size.width.as_f32() / 200.)
                            .max(bounds.size.height.as_f32() / 160.);
                        let columns = (bounds.size.width.as_f32() / cell).ceil() as usize;
                        let rows = (bounds.size.height.as_f32() / cell).ceil() as usize;
                        for y in 0..rows {
                            for x in 0..columns {
                                let fx = x as f32 * cell;
                                let fy = y as f32 * cell;
                                let mut shock = 0.;
                                let mut field = 0.36
                                    + 0.16 * (fx * 0.007 + time * 0.15).sin()
                                    + 0.13 * (fy * 0.009 - time * 0.12 + (fx * 0.003).sin()).cos();
                                for ripple in ripples.iter().flatten() {
                                    let age = now.duration_since(ripple.born).as_secs_f32();
                                    if age < LIFETIME {
                                        let dx = bounds.origin.x.as_f32() + fx
                                            - ripple.position.x.as_f32();
                                        let dy = bounds.origin.y.as_f32() + fy
                                            - ripple.position.y.as_f32();
                                        shock += wave(dx.hypot(dy), age);
                                    }
                                }
                                field += 0.5 * shock;
                                let threshold = (BAYER[y % 4][x % 4] + 0.5) / 16.;
                                // Bayer rounding chooses adjacent colors along a smooth, drifting gradient.
                                let u = fx / bounds.size.width.as_f32().max(1.);
                                let v = fy / bounds.size.height.as_f32().max(1.);
                                let gradient = (0.5
                                    + 0.5
                                        * (u * 2.5
                                            + v * 1.8
                                            + time * 0.09
                                            + 0.4 * (v * 3. - time * 0.07).sin()
                                            + shock * 0.7)
                                            .sin())
                                    * 8.;
                                let color_ix = (gradient.floor() as usize
                                    + usize::from(gradient.fract() > threshold))
                                .min(8);
                                let mut ink = palette[color_ix];
                                let energy = shock.abs().min(1.);
                                ink.l += (foreground.l - ink.l) * energy * 0.3;
                                ink.a += energy * 0.4;
                                if field > threshold {
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            bounds.origin + point(px(fx), px(fy)),
                                            size(px(cell * 0.45), px(cell * 0.45)),
                                        ),
                                        ink,
                                    ));
                                }
                            }
                        }
                    },
                )
                .size_full(),
            )
            .when(self.conversation_hint, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            v_flex()
                                .items_center()
                                .gap_3()
                                .p_6()
                                .rounded(cx.theme().radius_lg)
                                .bg(background.opacity(0.94))
                                .child(div().text_sm().font_medium().child("Select a conversation"))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .text_xs()
                                        .text_color(muted)
                                        .child("Pick a channel, or")
                                        .child(
                                            Button::new("idle-jump")
                                                .ghost()
                                                .label("Jump to…  ⌘K")
                                                .on_click(|_, window, cx| {
                                                    window.dispatch_action(
                                                        Box::new(crate::actions::QuickSwitch),
                                                        cx,
                                                    )
                                                }),
                                        ),
                                ),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{LIFETIME, mix_hue, wave};
    #[test]
    fn hue_blending_crosses_the_seam_by_the_short_arc() {
        let middle = mix_hue(0.98, 0.02, 0.5);
        assert!(!(0.001..=0.999).contains(&middle));
        assert!((mix_hue(0.2, 0.4, 0.5) - 0.3).abs() < 0.001);
    }

    #[test]
    fn shockwave_travels_and_expires() {
        assert!(wave(130., 1.) > wave(0., 1.));
        assert_eq!(wave(390., LIFETIME), 0.);
        assert_eq!(wave(520., 4.), 0.);
    }
}
