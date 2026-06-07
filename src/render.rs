//! The holographic scene: a 3D horizontal carousel clock, a procedural mascot, and weather
//! particles, all composed onto the 128x64 monochrome buffer.
//!
//! Design constraints (ESP32-C3 has no FPU, I2C bandwidth is precious):
//!
//! * **No floats.** Carousel depth is driven by per-slot lookup tables and Q8 fixed-point
//!   interpolation (`lerp`).
//! * **Vector digits.** Numbers are drawn as scalable 7-segment shapes rather than font bitmaps,
//!   so they zoom smoothly and cost almost nothing in flash.
//! * **Stepped/snapping motion.** The carousel is perfectly static except for a short 4-frame
//!   "snap" when the minute changes, which keeps the I2C bus quiet.
//! * **Dithered depth.** Background time tokens are masked with a checkerboard so they read as
//!   "further away" on a display that has no grey.

use embassy_time::Instant;
use embedded_graphics::{
    Pixel,
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Circle, Line, Polyline, PrimitiveStyle, Rectangle, RoundedRectangle},
};

use crate::weather::{ParticleField, WeatherState};
use crate::{TimeSync, Update};

// ---- carousel geometry -----------------------------------------------------
// One entry per slot s = -3..=3 (index s + 3). The arrays describe where a token sits and how big
// it is when it occupies that slot. Spacing is compressed towards the edges to fake perspective.
const SLOT_X: [i32; 7] = [4, 16, 36, 64, 92, 112, 124];
const SLOT_Y: [i32; 7] = [20, 24, 34, 54, 34, 24, 20];
/// Token scale in Q8 fixed point (256 == 1.0).
const SLOT_S: [i32; 7] = [36, 64, 120, 256, 120, 64, 36];

/// Fixed-point progress added per frame during a snap (256 / 4 == a 4-frame snap).
const SNAP_STEP: i32 = 64;

// Base (full-scale) digit metrics in pixels; everything else scales off these.
const DW: i32 = 11; // digit width
const DH: i32 = 18; // digit height
const CW: i32 = 6; // colon column width
const G: i32 = 3; // gap between elements
const TH: i32 = 2; // segment thickness

/// Below this scale a token is drawn as a dithered blob instead of legible digits.
const BLOB_SCALE: i32 = 96;
/// At or above this scale a token is drawn solid; in between it is dithered for depth.
const SOLID_SCALE: i32 = 210;

const BLINK_PERIOD: u32 = 48;
const BLINK_LEN: u32 = 3;

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

/// Linear interpolation in Q8: returns `a` at `t == 0` and `b` at `t == 256`.
#[inline]
fn lerp(a: i32, b: i32, t: i32) -> i32 {
    a + (((b - a) * t) >> 8)
}

#[inline]
fn slot_idx(s: i32) -> usize {
    (s + 3).clamp(0, 6) as usize
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

/// Draw a HH:MM token centred at `(cx, cy)` at the given Q8 `scale`.
fn draw_token<D>(target: &mut D, cx: i32, cy: i32, scale: i32, hh: u8, mm: u8, colon_on: bool)
where
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

    let dither = scale < SOLID_SCALE;
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

/// The carousel clock: tracks wall-clock time and the snap animation between minutes.
pub struct Clock {
    anchor: Option<TimeSync>,
    /// Minute-of-day currently shown in the centre slot.
    base_min: i32,
    /// Q8 snap progress, 0 when at rest.
    snap: i32,
    colon_on: bool,
    blink: bool,
    frame: u32,
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            anchor: None,
            base_min: 0,
            snap: 0,
            colon_on: true,
            blink: false,
            frame: 0,
        }
    }

    /// Adopt a fresh wall-clock anchor from the network. The next [`tick`](Self::tick) snaps the
    /// carousel straight to the real time (jumps of more than one minute are not animated).
    pub fn sync(&mut self, ts: TimeSync) {
        self.anchor = Some(ts);
    }

    pub fn blink(&self) -> bool {
        self.blink
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

    /// Advance one frame: update the colon blink, the eye blink, and the snap animation.
    pub fn tick(&mut self) {
        let secs = self.now_secs();
        let cur_min = ((secs / 60) % 1440) as i32;
        self.colon_on = secs.is_multiple_of(2);

        if self.snap > 0 {
            self.snap += SNAP_STEP;
            if self.snap >= 256 {
                self.snap = 0;
                self.base_min = (self.base_min + 1).rem_euclid(1440);
            }
        } else {
            let diff = (cur_min - self.base_min).rem_euclid(1440);
            if diff == 1 {
                self.snap = SNAP_STEP; // animate a one-minute advance
            } else if diff != 0 {
                self.base_min = cur_min; // large jump (first sync / wrap): snap instantly
            }
        }

        self.frame = self.frame.wrapping_add(1);
        self.blink = self.frame % BLINK_PERIOD < BLINK_LEN;
    }

    /// Draw the whole carousel of minute tokens.
    pub fn draw<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        for s in -3..=3 {
            let i = slot_idx(s);
            let j = slot_idx(s - 1);
            let cx = lerp(SLOT_X[i], SLOT_X[j], self.snap);
            let cy = lerp(SLOT_Y[i], SLOT_Y[j], self.snap);
            let scale = lerp(SLOT_S[i], SLOT_S[j], self.snap);

            let minute = (self.base_min + s).rem_euclid(1440);
            let hh = (minute / 60) as u8;
            let mm = (minute % 60) as u8;
            draw_token(target, cx, cy, scale, hh, mm, self.colon_on);
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// Draw the central 32x32 mascot. Its expression is the weather "sprite variant"; `blink` closes
/// the eyes for the blink frame.
fn draw_mascot<D>(target: &mut D, weather: WeatherState, blink: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let on = BinaryColor::On;
    let stroke = PrimitiveStyle::with_stroke(on, 1);
    let fill_style = PrimitiveStyle::with_fill(on);

    // Glowing outline head (hologram look: bright lines on true black).
    let _ = RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(50, 18), Size::new(28, 28)),
        Size::new(7, 7),
    )
    .into_styled(stroke)
    .draw(target);

    // Antenna.
    let _ = Line::new(Point::new(64, 18), Point::new(64, 13))
        .into_styled(stroke)
        .draw(target);
    let _ = Circle::with_center(Point::new(64, 12), 3)
        .into_styled(fill_style)
        .draw(target);

    // Eyes (open circles, or closed lines on the blink frame).
    let eye_y = 30;
    for eye_x in [59, 69] {
        if blink {
            let _ = Line::new(Point::new(eye_x - 2, eye_y), Point::new(eye_x + 2, eye_y))
                .into_styled(stroke)
                .draw(target);
        } else {
            let _ = Circle::with_center(Point::new(eye_x, eye_y), 4)
                .into_styled(fill_style)
                .draw(target);
        }
    }

    draw_mouth(target, weather);
}

fn draw_mouth<D>(target: &mut D, weather: WeatherState)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let stroke = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    match weather {
        WeatherState::Clear => {
            // Smile.
            let pts = [
                Point::new(59, 38),
                Point::new(62, 41),
                Point::new(66, 41),
                Point::new(69, 38),
            ];
            let _ = Polyline::new(&pts).into_styled(stroke).draw(target);
        }
        WeatherState::Clouds => {
            let _ = Line::new(Point::new(60, 40), Point::new(68, 40))
                .into_styled(stroke)
                .draw(target);
        }
        WeatherState::Rain => {
            // Frown.
            let pts = [
                Point::new(59, 41),
                Point::new(62, 38),
                Point::new(66, 38),
                Point::new(69, 41),
            ];
            let _ = Polyline::new(&pts).into_styled(stroke).draw(target);
        }
        WeatherState::Thunderstorm => {
            // Surprised "o".
            let _ = Circle::with_center(Point::new(64, 40), 4)
                .into_styled(stroke)
                .draw(target);
        }
        WeatherState::Mist => {
            // Sleepy wavy mouth.
            let pts = [
                Point::new(59, 40),
                Point::new(61, 38),
                Point::new(63, 40),
                Point::new(65, 38),
                Point::new(67, 40),
                Point::new(69, 38),
            ];
            let _ = Polyline::new(&pts).into_styled(stroke).draw(target);
        }
    }
}

/// The full animated scene: clock + weather particles + mascot. Owns all per-frame state so the
/// render loop stays a thin driver.
pub struct Scene {
    clock: Clock,
    particles: ParticleField,
    weather: WeatherState,
}

impl Scene {
    pub fn new(seed: u32) -> Self {
        Scene {
            clock: Clock::new(),
            particles: ParticleField::new(seed),
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
        self.particles.step(self.weather);
    }

    /// Render the current frame into `target`'s back buffer. Drawing order gives the depth
    /// illusion: weather is behind, then the carousel, then the mascot in front.
    pub fn draw<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let _ = target.clear(BinaryColor::Off);
        self.particles.draw(target, self.weather);
        self.clock.draw(target);
        draw_mascot(target, self.weather, self.clock.blink());
    }
}
