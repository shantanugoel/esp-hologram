//! The holographic scene: a compact weather strip and a legible floating clock, composed onto the
//! 128x64 monochrome buffer.
//!
//! Design constraints (ESP32-C3 has no FPU, I2C bandwidth is precious):
//!
//! * **No floats.** The 3D depth cues use fixed lookup tables and integer-only geometry.
//! * **Vector digits.** Numbers are drawn as scalable 7-segment shapes rather than font bitmaps,
//!   so they zoom smoothly and cost almost nothing in flash.
//! * **Stepped motion.** The time stays readable; only the ring phase and a small minute-change
//!   pulse animate, which keeps the I2C bus quiet.
//! * **Dithered depth.** The close offset time pass is masked with a checkerboard so it reads as
//!   a rear extrusion on a display that has no grey.

use embassy_time::Instant;
use embedded_graphics::{
    Pixel,
    mono_font::{MonoTextStyle, ascii::FONT_6X10},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Circle, Line, Polyline, PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};

use crate::weather::WeatherState;
use crate::{TimeSync, Update};

const DISPLAY_W: i32 = 128;
const DISPLAY_H: i32 = 64;

// Base (full-scale) digit metrics in pixels; everything else scales off these.
const DW: i32 = 11; // digit width
const DH: i32 = 18; // digit height
const CW: i32 = 6; // colon column width
const G: i32 = 3; // gap between elements
const TH: i32 = 3; // segment thickness

/// Below this scale a token is drawn as a dithered blob instead of legible digits.
const BLOB_SCALE: i32 = 96;
/// At or above this scale a token is drawn solid; in between it is dithered for depth.
const SOLID_SCALE: i32 = 210;

const PULSE_LEN: u8 = 5;

/// 24 points around a shallow ellipse, clockwise from the left edge. The ring is intentionally
/// wider than the time so it reads as the horizontal "floor" reflection in the cube.
const RING: [Point; 24] = [
    Point::new(28, 35),
    Point::new(31, 31),
    Point::new(39, 27),
    Point::new(50, 24),
    Point::new(63, 22),
    Point::new(77, 21),
    Point::new(91, 22),
    Point::new(104, 24),
    Point::new(115, 27),
    Point::new(123, 31),
    Point::new(126, 35),
    Point::new(123, 49),
    Point::new(115, 54),
    Point::new(104, 57),
    Point::new(91, 59),
    Point::new(77, 60),
    Point::new(63, 59),
    Point::new(50, 57),
    Point::new(39, 54),
    Point::new(31, 49),
    Point::new(28, 35),
    Point::new(31, 31),
    Point::new(39, 27),
    Point::new(50, 24),
];

/// Segment masks for digits 0-9. Bit 0..=6 map to segments a,b,c,d,e,f,g.
const SEG: [u8; 10] = [
    0b0111111, // 0
    0b0000110, // 1
    0b1011011, // 2
    0b1001111, // 3
    0b1100110, // 4
    0b1101101, // 5
    0b1111101, // 6
    0b0000111, // 7
    0b1111111, // 8
    0b1101111, // 9
];

/// Draw into the OLED buffer vertically flipped so the cube reflection reads normally.
struct CubeView<'a, D> {
    target: &'a mut D,
}

impl<'a, D> CubeView<'a, D> {
    fn new(target: &'a mut D) -> Self {
        Self { target }
    }
}

impl<D> DrawTarget for CubeView<'_, D>
where
    D: DrawTarget<Color = BinaryColor>,
{
    type Color = BinaryColor;
    type Error = D::Error;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        self.target
            .draw_iter(pixels.into_iter().filter_map(|Pixel(p, color)| {
                if (0..DISPLAY_W).contains(&p.x) && (0..DISPLAY_H).contains(&p.y) {
                    Some(Pixel(Point::new(p.x, DISPLAY_H - 1 - p.y), color))
                } else {
                    None
                }
            }))
    }
}

impl<D> OriginDimensions for CubeView<'_, D>
where
    D: DrawTarget<Color = BinaryColor>,
{
    fn size(&self) -> Size {
        Size::new(DISPLAY_W as u32, DISPLAY_H as u32)
    }
}

/// Fill a rectangle, optionally through a checkerboard mask (the "depth" dither).
fn fill<D>(target: &mut D, x: i32, y: i32, w: i32, h: i32, dither: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    if w <= 0 || h <= 0 {
        return;
    }
    if !dither {
        let _ = Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
            .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
            .draw(target);
        return;
    }
    for yy in 0..h {
        for xx in 0..w {
            if (x + xx + y + yy) & 1 == 0 {
                let _ = Pixel(Point::new(x + xx, y + yy), BinaryColor::On).draw(target);
            }
        }
    }
}

/// Geometry of one glyph cell: top-left corner, size, and segment thickness.
#[derive(Clone, Copy)]
struct Cell {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    t: i32,
}

/// Draw a single 7-segment digit inside the cell.
fn draw_digit<D>(target: &mut D, c: Cell, value: u8, dither: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let mask = SEG[(value % 10) as usize];
    let Cell { x, y, w, h, t } = c;
    let half = h / 2;
    let lower = h - half;
    let mid_y = y + half - t / 2;

    if mask & 0b0000001 != 0 {
        fill(target, x, y, w, t, dither); // a (top)
    }
    if mask & 0b0000010 != 0 {
        fill(target, x + w - t, y, t, half, dither); // b (top-right)
    }
    if mask & 0b0000100 != 0 {
        fill(target, x + w - t, y + half, t, lower, dither); // c (bottom-right)
    }
    if mask & 0b0001000 != 0 {
        fill(target, x, y + h - t, w, t, dither); // d (bottom)
    }
    if mask & 0b0010000 != 0 {
        fill(target, x, y + half, t, lower, dither); // e (bottom-left)
    }
    if mask & 0b0100000 != 0 {
        fill(target, x, y, t, half, dither); // f (top-left)
    }
    if mask & 0b1000000 != 0 {
        fill(target, x, mid_y, w, t, dither); // g (middle)
    }
}

fn draw_colon<D>(target: &mut D, c: Cell, dither: bool, on: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    if !on {
        return;
    }
    let dot = (c.t + 1).max(2);
    let dx = c.x + (c.w - dot) / 2;
    fill(target, dx, c.y + c.h / 3 - dot / 2, dot, dot, dither);
    fill(target, dx, c.y + 2 * c.h / 3 - dot / 2, dot, dot, dither);
}

#[inline]
fn ring_point(i: usize) -> Point {
    RING[i % 20]
}

fn draw_ring_segment<D>(target: &mut D, i: usize, bright: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let stroke = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let a = ring_point(i);
    let b = ring_point(i + 1);
    let _ = Line::new(a, b).into_styled(stroke).draw(target);

    if bright {
        let _ = Line::new(Point::new(a.x, a.y + 1), Point::new(b.x, b.y + 1))
            .into_styled(stroke)
            .draw(target);
    }
}

fn draw_ring_runner<D>(target: &mut D, phase: usize, front: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    for i in phase..phase + 3 {
        let in_front = (10..20).contains(&(i % 20));
        if in_front == front {
            draw_ring_segment(target, i, front);
        }
    }
}

fn draw_ring_back<D>(target: &mut D, phase: usize)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let stroke = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let _ = Polyline::new(&RING[0..=10])
        .into_styled(stroke)
        .draw(target);
    draw_ring_runner(target, phase, false);
}

fn draw_ring_front<D>(target: &mut D, phase: usize)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let stroke = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let _ = Polyline::new(&RING[10..=20])
        .into_styled(stroke)
        .draw(target);
    draw_ring_runner(target, phase, true);
}

fn weather_label(weather: WeatherState) -> &'static str {
    match weather {
        WeatherState::Clear => "CLEAR",
        WeatherState::Clouds => "CLOUD",
        WeatherState::Rain => "RAIN",
        WeatherState::Thunderstorm => "STORM",
        WeatherState::Mist => "MIST",
    }
}

fn draw_weather_icon<D>(target: &mut D, weather: WeatherState, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let on = BinaryColor::On;
    let stroke = PrimitiveStyle::with_stroke(on, 1);
    let fill = PrimitiveStyle::with_fill(on);

    match weather {
        WeatherState::Clear => {
            let center = Point::new(x + 7, y + 6);
            let _ = Circle::with_center(center, 5)
                .into_styled(stroke)
                .draw(target);
            for (dx, dy) in [(0, -7), (0, 7), (-7, 0), (7, 0)] {
                let _ = Line::new(
                    Point::new(center.x + dx * 4 / 7, center.y + dy * 4 / 7),
                    Point::new(center.x + dx, center.y + dy),
                )
                .into_styled(stroke)
                .draw(target);
            }
        }
        WeatherState::Clouds => {
            draw_cloud_icon(target, x, y, stroke);
        }
        WeatherState::Rain => {
            draw_cloud_icon(target, x, y, stroke);
            for dx in [3, 8, 13] {
                let _ = Line::new(Point::new(x + dx, y + 11), Point::new(x + dx - 2, y + 14))
                    .into_styled(stroke)
                    .draw(target);
            }
        }
        WeatherState::Thunderstorm => {
            draw_cloud_icon(target, x, y, stroke);
            let pts = [
                Point::new(x + 8, y + 9),
                Point::new(x + 5, y + 14),
                Point::new(x + 10, y + 13),
                Point::new(x + 7, y + 18),
            ];
            let _ = Polyline::new(&pts).into_styled(stroke).draw(target);
        }
        WeatherState::Mist => {
            for yy in [3, 7, 11] {
                let _ = Rectangle::new(Point::new(x, y + yy), Size::new(16, 1))
                    .into_styled(fill)
                    .draw(target);
            }
        }
    }
}

fn draw_cloud_icon<D>(target: &mut D, x: i32, y: i32, stroke: PrimitiveStyle<BinaryColor>)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ = Circle::with_center(Point::new(x + 5, y + 7), 5)
        .into_styled(stroke)
        .draw(target);
    let _ = Circle::with_center(Point::new(x + 10, y + 5), 7)
        .into_styled(stroke)
        .draw(target);
    let _ = Circle::with_center(Point::new(x + 15, y + 7), 5)
        .into_styled(stroke)
        .draw(target);
    let _ = Line::new(Point::new(x + 1, y + 10), Point::new(x + 19, y + 10))
        .into_styled(stroke)
        .draw(target);
}

fn draw_weather_strip<D>(target: &mut D, weather: WeatherState)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let on = BinaryColor::On;
    let stroke = PrimitiveStyle::with_stroke(on, 1);
    draw_weather_icon(target, weather, 4, 0);

    let text = MonoTextStyle::new(&FONT_6X10, on);
    let _ = Text::with_baseline(
        weather_label(weather),
        Point::new(30, 2),
        text,
        Baseline::Top,
    )
    .draw(target);

    // Thin divider gives the clock a clean stage without boxing it in.
    let _ = Line::new(Point::new(0, 14), Point::new(127, 14))
        .into_styled(stroke)
        .draw(target);
}

/// Draw a HH:MM token centred at `(cx, cy)` at the given Q8 `scale`.
fn draw_token<D>(target: &mut D, cx: i32, cy: i32, scale: i32, hh: u8, mm: u8, colon_on: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    draw_token_inner(target, cx, cy, scale, hh, mm, colon_on, false);
}

fn draw_token_inner<D>(
    target: &mut D,
    cx: i32,
    cy: i32,
    scale: i32,
    hh: u8,
    mm: u8,
    colon_on: bool,
    force_dither: bool,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    if scale < 8 {
        return;
    }
    let dw = (DW * scale) >> 8;
    let dh = ((DH * scale) >> 8).max(2);
    let cw = (CW * scale) >> 8;
    let gap = (G * scale) >> 8;
    let th = ((TH * scale) >> 8).max(1);

    let total = 4 * dw + cw + 4 * gap;
    let x0 = cx - total / 2;
    let y0 = cy - dh / 2;

    // Distant tokens are unreadable anyway: draw a dithered blob to suggest a far-off time.
    if scale < BLOB_SCALE {
        fill(target, x0, y0, total.max(2), dh, true);
        return;
    }

    let dither = force_dither || scale < SOLID_SCALE;
    let cell = |x: i32, w: i32| Cell {
        x,
        y: y0,
        w,
        h: dh,
        t: th,
    };
    let mut x = x0;
    draw_digit(target, cell(x, dw), hh / 10, dither);
    x += dw + gap;
    draw_digit(target, cell(x, dw), hh % 10, dither);
    x += dw + gap;
    draw_colon(target, cell(x, cw), dither, colon_on);
    x += cw + gap;
    draw_digit(target, cell(x, dw), mm / 10, dither);
    x += dw + gap;
    draw_digit(target, cell(x, dw), mm % 10, dither);
}

/// The floating clock: tracks wall-clock time and drives the ring phase.
pub struct Clock {
    anchor: Option<TimeSync>,
    /// Minute-of-day currently shown by the foreground time.
    current_min: i32,
    /// Small frame countdown used to pulse the foreground time when the minute changes.
    pulse: u8,
    colon_on: bool,
    frame: u32,
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            anchor: None,
            current_min: 0,
            pulse: 0,
            colon_on: true,
            frame: 0,
        }
    }

    /// Adopt a fresh wall-clock anchor from the network.
    pub fn sync(&mut self, ts: TimeSync) {
        self.anchor = Some(ts);
    }

    fn now_secs(&self) -> u32 {
        match self.anchor {
            Some(a) => {
                let elapsed = Instant::now().duration_since(a.at).as_secs();
                ((a.secs_of_day as u64 + elapsed) % 86_400) as u32
            }
            // Before the first sync, run off the monotonic clock so the display still ticks.
            None => (Instant::now().as_secs() % 86_400) as u32,
        }
    }

    /// Advance one frame: update the colon blink, minute pulse, and ring phase.
    pub fn tick(&mut self) {
        let secs = self.now_secs();
        let cur_min = ((secs / 60) % 1440) as i32;
        self.colon_on = secs.is_multiple_of(2);

        if cur_min != self.current_min {
            self.current_min = cur_min;
            self.pulse = PULSE_LEN;
        } else if self.pulse > 0 {
            self.pulse -= 1;
        }

        self.frame = self.frame.wrapping_add(1);
    }

    /// Draw one readable foreground time with a rotating horizontal ring.
    pub fn draw<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let phase = ((self.frame / 2) % 20) as usize;
        let hh = (self.current_min / 60) as u8;
        let mm = (self.current_min % 60) as u8;
        let scale = if self.pulse > 0 { 268 } else { 256 };

        draw_ring_back(target, phase);
        draw_token_inner(target, 82, 47, scale, hh, mm, self.colon_on, true);
        draw_token(target, 79, 44, scale, hh, mm, self.colon_on);
        draw_ring_front(target, phase);
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// The full animated scene: weather strip + floating clock. Owns all per-frame state so the
/// render loop stays a thin driver.
pub struct Scene {
    clock: Clock,
    weather: WeatherState,
}

impl Scene {
    pub fn new(_seed: u32) -> Self {
        Scene {
            clock: Clock::new(),
            weather: WeatherState::Clear,
        }
    }

    /// Apply a network update: switch weather and re-anchor the clock.
    pub fn apply(&mut self, update: Update) {
        self.weather = update.weather;
        self.clock.sync(update.time);
    }

    /// Advance the simulation by one frame (no drawing).
    pub fn tick(&mut self) {
        self.clock.tick();
    }

    /// Render the current frame into `target`'s back buffer. Drawing order gives the depth
    /// illusion: weather context stays in the top strip, then the ring clock fills the stage.
    pub fn draw<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let _ = target.clear(BinaryColor::Off);
        let mut cube_view = CubeView::new(target);
        draw_weather_strip(&mut cube_view, self.weather);
        self.clock.draw(&mut cube_view);
    }
}
