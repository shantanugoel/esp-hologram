//! The holographic scene: a compact weather strip and a legible floating clock, composed onto the
//! 128x64 monochrome buffer.
//!
//! Design constraints (ESP32-C3 has no FPU, I2C bandwidth is precious):
//!
//! * **No floats.** The 3D depth cues use fixed lookup tables and integer-only geometry.
//! * **Vector digits.** Numbers are drawn as scalable 7-segment shapes rather than font bitmaps,
//!   so they zoom smoothly and cost almost nothing in flash.
//! * **Stepped motion.** The time stays readable; only the weather strip and a small minute-change
//!   depth pulse animate, which keeps the I2C bus quiet at the 12.5fps render rate.
//! * **Dithered depth.** The time's stacked offset passes are masked with a checkerboard so the
//!   stack reads as an extruded 3D block on a display that has no grey.

use embassy_time::Instant;
use embedded_graphics::{
    Pixel,
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Circle, Line, Polyline, PrimitiveStyle, Rectangle},
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

/// The clock is one big centred HH:MM. `BIG_SCALE` is the Q8 zoom (256 = 1:1) chosen so the time
/// nearly fills the width, `DEPTH` is how many dithered passes are stacked behind the solid front
/// face to fake an extruded 3D block, and `(CLOCK_CX, CLOCK_CY)` centres that front face.
const BIG_SCALE: i32 = 500;
const DEPTH: i32 = 4;
const CLOCK_CX: i32 = 64;
const CLOCK_CY: i32 = 37;

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

/// Restrict drawing to an inclusive rectangle. The animated weather particles are drawn through
/// this so they can slide off the edges of the strip without ever touching the text label or
/// bleeding down into the clock area below.
struct ClipView<'a, D> {
    target: &'a mut D,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl<'a, D> ClipView<'a, D> {
    fn new(target: &'a mut D, x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self {
            target,
            x0,
            y0,
            x1,
            y1,
        }
    }
}

impl<D> Dimensions for ClipView<'_, D> {
    fn bounding_box(&self) -> Rectangle {
        Rectangle::new(
            Point::new(self.x0, self.y0),
            Size::new(
                (self.x1 - self.x0 + 1) as u32,
                (self.y1 - self.y0 + 1) as u32,
            ),
        )
    }
}

impl<D> DrawTarget for ClipView<'_, D>
where
    D: DrawTarget<Color = BinaryColor>,
{
    type Color = BinaryColor;
    type Error = D::Error;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let (x0, y0, x1, y1) = (self.x0, self.y0, self.x1, self.y1);
        self.target.draw_iter(
            pixels
                .into_iter()
                .filter(move |Pixel(p, _)| (x0..=x1).contains(&p.x) && (y0..=y1).contains(&p.y)),
        )
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

// --- Animated weather strip ---------------------------------------------------------------
// The weather fills a thin, full-width band above the clock. Everything here is integer-only and
// confined to `[STRIP_X0, STRIP_X1] x [0, STRIP_BOT]` by a `ClipView`, so particles scatter right
// across the row and slide off the edges without ever bleeding down into the clock area.

const STRIP_X0: i32 = 0;
const STRIP_X1: i32 = 127;
const STRIP_W: i32 = STRIP_X1 - STRIP_X0;
const STRIP_BOT: i32 = 13;

/// 16 evenly spaced unit directions (Q4, scaled by 16) used to "step rotate" the sun's rays off a
/// tick counter without any runtime trigonometry.
const SUN_RAY: [(i32, i32); 16] = [
    (16, 0),
    (15, 6),
    (11, 11),
    (6, 15),
    (0, 16),
    (-6, 15),
    (-11, 11),
    (-15, 6),
    (-16, 0),
    (-15, -6),
    (-11, -11),
    (-6, -15),
    (0, -16),
    (6, -15),
    (11, -11),
    (15, -6),
];

fn draw_weather_anim<D>(target: &mut D, weather: WeatherState, frame: u32, seed: u32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    match weather {
        WeatherState::Clear => draw_sun_anim(target, frame),
        WeatherState::Clouds => draw_clouds_anim(target, frame, seed),
        WeatherState::Rain => draw_rain_anim(target, frame, seed),
        WeatherState::Thunderstorm => {
            draw_rain_anim(target, frame, seed);
            draw_bolt_anim(target, frame, seed);
        }
        WeatherState::Mist => draw_mist_anim(target, frame),
    }
}

/// A bright sun disc whose eight rays slowly spin.
fn draw_sun_anim<D>(target: &mut D, frame: u32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let on = BinaryColor::On;
    let center = Point::new((STRIP_X0 + STRIP_X1) / 2, 6);
    let _ = Circle::with_center(center, 7)
        .into_styled(PrimitiveStyle::with_fill(on))
        .draw(target);

    // Eight rays stepped through the 16-direction table for a slow, smooth rotation.
    let phase = (frame / 3) as usize;
    for k in 0..8 {
        let (dx, dy) = SUN_RAY[(k * 2 + phase) & 15];
        let inner = Point::new(center.x + dx * 5 / 16, center.y + dy * 5 / 16);
        let outer = Point::new(center.x + dx * 8 / 16, center.y + dy * 8 / 16);
        let _ = Line::new(inner, outer)
            .into_styled(PrimitiveStyle::with_stroke(on, 1))
            .draw(target);
    }
}

/// A small outlined cloud anchored at its left puff.
fn draw_cloud_shape<D>(target: &mut D, x: i32, y: i32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let stroke = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let _ = Circle::with_center(Point::new(x, y + 1), 5)
        .into_styled(stroke)
        .draw(target);
    let _ = Circle::with_center(Point::new(x + 5, y - 1), 7)
        .into_styled(stroke)
        .draw(target);
    let _ = Circle::with_center(Point::new(x + 10, y + 1), 5)
        .into_styled(stroke)
        .draw(target);
    let _ = Line::new(Point::new(x - 2, y + 3), Point::new(x + 12, y + 3))
        .into_styled(stroke)
        .draw(target);
}

/// Four clouds scattered across the strip, drifting left with a gentle vertical bob.
fn draw_clouds_anim<D>(target: &mut D, frame: u32, seed: u32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    const N: u32 = 4;
    let span = STRIP_W + 24; // overscan so clouds slide fully off both edges before wrapping
    let drift = (frame / 2) % span as u32;
    for i in 0..N {
        let jitter = ((seed >> (i * 5)) & 31) as i32;
        let base = (i as i32) * span / (N as i32) + jitter;
        let pos = (base - drift as i32).rem_euclid(span);
        let x = STRIP_X0 - 12 + pos;
        let bob = (((frame / 6) + i * 2) % 3) as i32 - 1;
        draw_cloud_shape(target, x, 6 + bob);
    }
}

/// Rain streaks scattered across the strip, falling and recycling at varied speeds.
fn draw_rain_anim<D>(target: &mut D, frame: u32, seed: u32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    const N: u32 = 12;
    let stroke = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let span = (STRIP_BOT + 4) as u32; // fall a little past the bottom before respawning at the top
    for i in 0..N {
        let x = STRIP_X0 + ((i * 37 + (seed & 63)) as i32 % STRIP_W);
        let vy = 2 + (i & 1);
        let y = (frame.wrapping_mul(vy).wrapping_add(i * 11) % span) as i32 - 2;
        let _ = Line::new(Point::new(x, y), Point::new(x, y + 3))
            .into_styled(stroke)
            .draw(target);
    }
}

/// An occasional lightning bolt that flashes briefly at a shifting position.
fn draw_bolt_anim<D>(target: &mut D, frame: u32, seed: u32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    const CYCLE: u32 = 40;
    if frame % CYCLE >= 3 {
        return;
    }
    let flash = frame / CYCLE;
    let bx =
        STRIP_X0 + 6 + (flash.wrapping_mul(53).wrapping_add(seed) % (STRIP_W as u32 - 12)) as i32;
    let pts = [
        Point::new(bx, 0),
        Point::new(bx - 4, 5),
        Point::new(bx + 2, 7),
        Point::new(bx - 3, 13),
    ];
    let _ = Polyline::new(&pts)
        .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
        .draw(target);
}

/// Drifting dashed haze bands at three depths, each sliding at its own speed.
fn draw_mist_anim<D>(target: &mut D, frame: u32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    const ROWS: [i32; 3] = [3, 7, 11];
    const SPD: [u32; 3] = [1, 2, 1];
    const STEP: i32 = 12;
    for (&y, &spd) in ROWS.iter().zip(SPD.iter()) {
        let off = ((frame / 3).wrapping_mul(spd) % STEP as u32) as i32;
        let mut x = STRIP_X0 - STEP + off;
        while x < STRIP_X1 {
            let _ = Rectangle::new(Point::new(x, y), Size::new(6, 1))
                .into_styled(fill)
                .draw(target);
            x += STEP;
        }
    }
}

fn draw_weather_strip<D>(target: &mut D, weather: WeatherState, frame: u32, seed: u32)
where
    D: DrawTarget<Color = BinaryColor>,
{
    // Confine the particles to the top band so nothing bleeds down into the clock area.
    let mut strip = ClipView::new(target, STRIP_X0, 0, STRIP_X1, STRIP_BOT);
    draw_weather_anim(&mut strip, weather, frame, seed);
}

/// Draw a HH:MM token centred at `(cx, cy)` at the given Q8 `scale`.
fn draw_token<D>(target: &mut D, cx: i32, cy: i32, scale: i32, hh: u8, mm: u8, colon_on: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    draw_token_inner(target, cx, cy, scale, hh, mm, colon_on, false);
}

#[allow(
    clippy::too_many_arguments,
    reason = "a low-level glyph drawing helper that legitimately needs target, position, scale, \
    both time digits, and the colon/dither flags"
)]
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

/// The floating clock: tracks wall-clock time and renders the big extruded HH:MM.
pub struct Clock {
    anchor: Option<TimeSync>,
    /// Minute-of-day currently shown by the foreground time.
    current_min: i32,
    /// Small frame countdown used to pulse the time's depth when the minute changes.
    pulse: u8,
    colon_on: bool,
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            anchor: None,
            current_min: 0,
            pulse: 0,
            colon_on: true,
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

    /// Advance one frame: update the colon blink and minute pulse.
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
    }

    /// Draw the time as one large, centred, extruded HH:MM block.
    pub fn draw<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let hh = (self.current_min / 60) as u8;
        let mm = (self.current_min % 60) as u8;
        // A minute change briefly pushes the time one layer deeper for a subtle 3D "pop".
        let depth = if self.pulse > 0 { DEPTH + 1 } else { DEPTH };

        // Stack dithered copies from the far layer in toward the front, then a solid face on top.
        // The constant diagonal step keeps every layer on the same checkerboard phase, so the
        // stack reads as a cleanly shaded extrusion instead of a smear.
        for i in (1..=depth).rev() {
            draw_token_inner(
                target,
                CLOCK_CX + i,
                CLOCK_CY + i,
                BIG_SCALE,
                hh,
                mm,
                self.colon_on,
                true,
            );
        }
        draw_token(target, CLOCK_CX, CLOCK_CY, BIG_SCALE, hh, mm, self.colon_on);
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
    /// Free-running frame counter driving the weather strip animation.
    frame: u32,
    /// Per-boot entropy used to scatter the weather particles.
    seed: u32,
}

impl Scene {
    pub fn new(seed: u32) -> Self {
        Scene {
            clock: Clock::new(),
            weather: WeatherState::Clear,
            frame: 0,
            seed,
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
        self.frame = self.frame.wrapping_add(1);
    }

    /// Render the current frame into `target`'s back buffer. Drawing order gives the depth
    /// illusion: the weather animates in the top strip, then the big extruded clock fills the rest.
    pub fn draw<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let _ = target.clear(BinaryColor::Off);
        let mut cube_view = CubeView::new(target);
        draw_weather_strip(&mut cube_view, self.weather, self.frame, self.seed);
        self.clock.draw(&mut cube_view);
    }
}
