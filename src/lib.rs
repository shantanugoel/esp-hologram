#![no_std]
//! Shared types and global state for the esp-hologram firmware.
//!
//! The firmware is split into three cooperative concerns that communicate through a single
//! [`UPDATE`] signal:
//!
//! * [`net`] connects to WiFi and fetches the current weather + local time over plain HTTP.
//! * [`weather`] owns the [`weather::WeatherState`] model and the procedural particle systems.
//! * [`render`] turns the latest [`Update`] into pixels (3D carousel clock, mascot, weather).
//!
//! All rendering math is integer / fixed-point — there is no `f32`/`f64` anywhere in the scene
//! pipeline, which matters on the FPU-less ESP32-C3.

pub mod net;
pub mod render;
pub mod weather;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Instant;
use weather::WeatherState;

/// WiFi SSID, baked in at build time from `wifi_config.toml` (see `build.rs`).
pub const WIFI_SSID: &str = env!("WIFI_SSID");
/// WiFi password, baked in at build time. Empty string means an open network.
pub const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
/// City used for the wttr.in weather lookup.
pub const WEATHER_CITY: &str = env!("WEATHER_CITY");

/// A wall-clock anchor: `secs_of_day` is the local time-of-day in seconds at the [`Instant`]
/// `at`. The renderer extrapolates the live time from this anchor using the monotonic clock,
/// so the display keeps ticking between network syncs.
#[derive(Clone, Copy)]
pub struct TimeSync {
    pub secs_of_day: u32,
    pub at: Instant,
}

/// A single fused update produced by the network task and consumed by the renderer.
#[derive(Clone, Copy)]
pub struct Update {
    pub weather: WeatherState,
    pub time: TimeSync,
}

/// Latest weather + time, published by `wifi_weather_fetcher` and drained by `render_loop`.
/// A [`Signal`] only retains the most recent value, which is exactly what we want here.
pub static UPDATE: Signal<CriticalSectionRawMutex, Update> = Signal::new();
