//! Weather model + procedural background particle systems.
//!
//! Everything here is integer-only and allocation-free. [`WeatherState`] is derived from the
//! wttr.in condition string and selects both the mascot expression (in [`crate::render`]) and the
//! particle animation drawn behind the clock. Randomness comes from a tiny xorshift PRNG seeded
//! once from the hardware RNG, so no floating point or heap is involved.

use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Circle, Ellipse, Line, PrimitiveStyle, Rectangle},
};

const W: i32 = 128;
const H: i32 = 64;

/// Coarse weather buckets covering what Bengaluru actually sees through the year.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WeatherState {
    Clear,
    Clouds,
    Rain,
    Thunderstorm,
    Mist,
}

impl WeatherState {
    /// Maps a free-form wttr.in condition (e.g. "Patchy light drizzle") to a bucket. Matching is
    /// case-insensitive and order-sensitive: the most specific conditions are checked first.
    pub fn from_condition(condition: &str) -> Self {
        let has = |needle: &str| ci_contains(condition, needle);
        if has("thunder") {
            WeatherState::Thunderstorm
        } else if has("rain") || has("drizzle") || has("shower") || has("sleet") {
            WeatherState::Rain
        } else if has("mist") || has("fog") || has("haze") || has("smoke") {
            WeatherState::Mist
        } else if has("cloud") || has("overcast") {
            WeatherState::Clouds
        } else if has("sun") || has("clear") {
            WeatherState::Clear
        } else {
            WeatherState::Clouds
        }
    }

    /// Short human-readable label, handy for logging.
    pub fn label(self) -> &'static str {
        match self {
            WeatherState::Clear => "clear",
            WeatherState::Clouds => "clouds",
            WeatherState::Rain => "rain",
            WeatherState::Thunderstorm => "thunderstorm",
            WeatherState::Mist => "mist",
        }
    }
}

/// Case-insensitive substring search. `needle` must already be lowercase.
fn ci_contains(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    'outer: for start in 0..=h.len() - n.len() {
        for j in 0..n.len() {
            if h[start + j].to_ascii_lowercase() != n[j] {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

const RAIN_N: usize = 14;
const CLOUD_N: usize = 3;
const MIST_N: usize = 4;

/// Rows used by the drifting fog bands, plus their per-tick horizontal speeds.
const MIST_Y: [i32; MIST_N] = [12, 26, 44, 56];
const MIST_SPEED: [i32; MIST_N] = [1, 2, 1, 2];
const MIST_STEP: i32 = 12;

#[derive(Clone, Copy)]
struct Drop {
    x: i16,
    y: i16,
    len: i16,
    vy: i16,
    /// Frames remaining in the splash animation (0 = falling).
    splash: u8,
}

#[derive(Clone, Copy)]
struct Cloud {
    x: i16,
    y: i16,
}

/// 16 evenly spaced unit directions scaled by 16 (Q4 fixed point). Pre-computed so the sun's rays
/// can "step rotate" off a tick counter without any runtime trigonometry.
const RAY_DIR: [(i32, i32); 16] = [
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

/// Holds the mutable state of every particle system. Only the subsystem matching the active
/// [`WeatherState`] is advanced/drawn each frame, so the cost stays tiny.
pub struct ParticleField {
    rng: u32,
    tick: u32,
    rain: [Drop; RAIN_N],
    clouds: [Cloud; CLOUD_N],
    mist: [i32; MIST_N],
    /// Lightning flash frames remaining.
    bolt: u8,
    bolt_x: i32,
}

impl ParticleField {
    pub fn new(seed: u32) -> Self {
        let mut field = ParticleField {
            rng: seed | 1,
            tick: 0,
            rain: [Drop {
                x: 0,
                y: 0,
                len: 4,
                vy: 2,
                splash: 0,
            }; RAIN_N],
            clouds: [Cloud { x: 0, y: 10 }; CLOUD_N],
            mist: [0; MIST_N],
            bolt: 0,
            bolt_x: 40,
        };
        for i in 0..RAIN_N {
            field.rain[i] = field.spawn_drop();
            // Stagger the initial vertical positions across the whole screen.
            field.rain[i].y = (field.rnd() % H as u32) as i16;
        }
        for i in 0..CLOUD_N {
            field.clouds[i] = Cloud {
                x: (field.rnd() % (W as u32 + 40)) as i16 - 20,
                y: 6 + i as i16 * 8,
            };
        }
        field
    }

    /// xorshift32 — fast, deterministic, no FPU.
    fn rnd(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x
    }

    fn spawn_drop(&mut self) -> Drop {
        let x = (self.rnd() % W as u32) as i16;
        let len = 4 + (self.rnd() % 5) as i16;
        let vy = 3 + (self.rnd() % 3) as i16;
        // Start a little above the top so drops are nicely staggered in time.
        let y = -(1 + (self.rnd() % 24) as i16);
        Drop {
            x,
            y,
            len,
            vy,
            splash: 0,
        }
    }

    /// Advance one frame of simulation for the active weather.
    pub fn step(&mut self, weather: WeatherState) {
        self.tick = self.tick.wrapping_add(1);
        match weather {
            WeatherState::Clear => {} // the sun only rotates; nothing to integrate
            WeatherState::Clouds => self.step_clouds(),
            WeatherState::Mist => self.step_mist(),
            WeatherState::Rain => self.step_rain(),
            WeatherState::Thunderstorm => {
                self.step_rain();
                self.step_bolt();
            }
        }
    }

    fn step_rain(&mut self) {
        for i in 0..RAIN_N {
            let mut d = self.rain[i];
            if d.splash > 0 {
                d.splash -= 1;
                if d.splash == 0 {
                    d = self.spawn_drop();
                }
            } else {
                d.y += d.vy;
                if d.y >= H as i16 - 1 {
                    d.y = H as i16 - 1;
                    d.splash = 4;
                }
            }
            self.rain[i] = d;
        }
    }

    fn step_bolt(&mut self) {
        if self.bolt > 0 {
            self.bolt -= 1;
        } else if self.rnd().is_multiple_of(45) {
            self.bolt = 3;
            self.bolt_x = 24 + (self.rnd() % 80) as i32;
        }
    }

    fn step_clouds(&mut self) {
        if !self.tick.is_multiple_of(2) {
            return; // drift at half frame-rate for a slow, calm motion
        }
        for i in 0..CLOUD_N {
            self.clouds[i].x -= 1;
            if self.clouds[i].x < -40 {
                self.clouds[i].x = W as i16 + 8;
                self.clouds[i].y = 4 + (self.rnd() % 18) as i16;
            }
        }
    }

    fn step_mist(&mut self) {
        for (offset, &speed) in self.mist.iter_mut().zip(MIST_SPEED.iter()) {
            *offset = (*offset + speed).rem_euclid(MIST_STEP);
        }
    }

    /// Draw the active weather behind the rest of the scene.
    pub fn draw<D>(&self, target: &mut D, weather: WeatherState)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        match weather {
            WeatherState::Clear => self.draw_sun(target),
            WeatherState::Clouds => self.draw_clouds(target),
            WeatherState::Mist => self.draw_mist(target),
            WeatherState::Rain => self.draw_rain(target),
            WeatherState::Thunderstorm => {
                self.draw_rain(target);
                if self.bolt > 0 {
                    self.draw_bolt(target);
                }
            }
        }
    }

    fn draw_sun<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let on = BinaryColor::On;
        let center = Point::new(14, 12);
        let _ = Circle::with_center(center, 9)
            .into_styled(PrimitiveStyle::with_fill(on))
            .draw(target);

        // Step-rotate the 8 rays off the tick counter (one step every 4 frames).
        let phase = (self.tick / 4) as usize;
        for k in 0..8 {
            let dir = RAY_DIR[(k * 2 + phase) & 15];
            let inner = Point::new(center.x + dir.0 * 6 / 16, center.y + dir.1 * 6 / 16);
            let outer = Point::new(center.x + dir.0 * 12 / 16, center.y + dir.1 * 12 / 16);
            let _ = Line::new(inner, outer)
                .into_styled(PrimitiveStyle::with_stroke(on, 1))
                .draw(target);
        }
    }

    fn draw_rain<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let on = BinaryColor::On;
        for d in &self.rain {
            let x = d.x as i32;
            if d.splash > 0 {
                // A tiny 2-pixel-tall splash ellipse at the floor.
                let _ = Ellipse::new(Point::new(x - 2, H - 3), Size::new(4, 2))
                    .into_styled(PrimitiveStyle::with_stroke(on, 1))
                    .draw(target);
            } else {
                let y = d.y as i32;
                let _ = Line::new(Point::new(x, y), Point::new(x, y + d.len as i32))
                    .into_styled(PrimitiveStyle::with_stroke(on, 1))
                    .draw(target);
            }
        }
    }

    fn draw_bolt<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let on = BinaryColor::On;
        let x = self.bolt_x;
        let pts = [
            Point::new(x, 0),
            Point::new(x - 5, 9),
            Point::new(x + 3, 17),
            Point::new(x - 3, 26),
            Point::new(x + 4, 34),
        ];
        for seg in pts.windows(2) {
            let _ = Line::new(seg[0], seg[1])
                .into_styled(PrimitiveStyle::with_stroke(on, 1))
                .draw(target);
        }
    }

    fn draw_clouds<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let stroke = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
        for c in &self.clouds {
            let (x, y) = (c.x as i32, c.y as i32);
            let _ = Circle::with_center(Point::new(x, y), 9)
                .into_styled(stroke)
                .draw(target);
            let _ = Circle::with_center(Point::new(x + 9, y - 3), 12)
                .into_styled(stroke)
                .draw(target);
            let _ = Circle::with_center(Point::new(x + 18, y), 9)
                .into_styled(stroke)
                .draw(target);
            let _ = Line::new(Point::new(x - 4, y + 5), Point::new(x + 22, y + 5))
                .into_styled(stroke)
                .draw(target);
        }
    }

    fn draw_mist<D>(&self, target: &mut D)
    where
        D: DrawTarget<Color = BinaryColor>,
    {
        let on = BinaryColor::On;
        for (&y, &offset) in MIST_Y.iter().zip(self.mist.iter()) {
            let mut x = -offset;
            // Dashed horizontal streaks that slide sideways read as drifting haze.
            while x < W {
                let _ = Rectangle::new(Point::new(x, y), Size::new(6, 1))
                    .into_styled(PrimitiveStyle::with_fill(on))
                    .draw(target);
                x += MIST_STEP;
            }
        }
    }
}
