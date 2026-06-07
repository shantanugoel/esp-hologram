#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

//! esp-hologram — a desk-hologram clock for an ESP32-C3 driving a 128x64 SSD1306 OLED.
//!
//! Two application tasks do the real work (plus the unavoidable embassy-net runner):
//!
//! * [`wifi_weather_fetcher`] keeps WiFi up and, every 15 minutes, fetches the current weather and
//!   local time from wttr.in, publishing them on [`esp_hologram::UPDATE`].
//! * [`render_loop`] runs a fixed-rate, non-blocking pipeline: drain updates, advance the carousel
//!   snap + weather particles, draw into the ssd1306 back buffer, then `flush().await` over async
//!   I2C. The display buffer is the back buffer; `flush` only ships the changed region.

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Runner, Stack, StackResources};
use embassy_time::{Duration, Ticker, Timer};
use esp_backtrace as _;
use esp_hal::Async;
use esp_hal::clock::CpuClock;
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::{Rng, TrngSource};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hologram::net::{self, NetScratch};
use esp_hologram::render::Scene;
use esp_hologram::{UPDATE, WEATHER_CITY, WIFI_PASSWORD, WIFI_SSID};
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethod, Config as WifiConfig, ControllerConfig, Interface as WifiInterface,
    WifiController,
};
use log::{info, warn};
use ssd1306::Ssd1306Async;
use ssd1306::mode::BufferedGraphicsModeAsync;
use ssd1306::prelude::*;

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

/// I2C address of the OLED (0x3C is the SSD1306 default, matching `diagram.json`).
const OLED_ADDR: u8 = 0x3C;
/// Render tick (~12.5 fps). Far below 60 fps so the I2C bus is never choked.
const FRAME_MS: u64 = 80;
/// How often to refresh weather + re-sync the clock from the network.
const WEATHER_PERIOD: Duration = Duration::from_secs(900);

/// Concrete buffered, async SSD1306 over async I2C.
type Display = Ssd1306Async<
    I2CInterface<I2c<'static, Async>>,
    DisplaySize128x64,
    BufferedGraphicsModeAsync<DisplaySize128x64>,
>;

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 66320);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
    info!("Embassy initialized!");

    // --- True RNG: enables the entropy source WiFi needs and seeds the stack + particles ---
    let _trng = TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let rng = Rng::new();
    let net_seed = ((rng.random() as u64) << 32) | rng.random() as u64;
    let particle_seed = rng.random();

    // --- OLED over async I2C @ 400kHz (fast mode keeps full-frame flushes short) ---
    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .expect("Failed to initialize I2C")
    .with_scl(peripherals.GPIO9)
    .with_sda(peripherals.GPIO8)
    .into_async();

    let interface = I2CInterface::new(i2c, OLED_ADDR, 0x40);
    let mut display = Ssd1306Async::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
        .into_buffered_graphics_mode();
    match display.init().await {
        Ok(()) => {
            // Hand the 1KB framebuffer to a static so the render task future stays small.
            static DISPLAY: static_cell::StaticCell<Display> = static_cell::StaticCell::new();
            let display = DISPLAY.init(display);
            spawner.spawn(render_loop(display, particle_seed).unwrap());
        }
        Err(e) => {
            warn!(
                "OLED init failed at I2C address 0x{:02X} on SCL=GPIO9/SDA=GPIO8: {:?}. \
                 Check wiring, power, pull-ups, and whether the panel address is 0x3C or 0x3D.",
                OLED_ADDR, e
            );
        }
    }

    // --- WiFi + embassy-net stack (DHCP) ---
    let (controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, ControllerConfig::default())
            .expect("Failed to initialize WiFi");
    let wifi_interface = interfaces.station;

    static RESOURCES: static_cell::StaticCell<StackResources<4>> = static_cell::StaticCell::new();
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        NetConfig::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        net_seed,
    );

    // Network scratch buffers, also parked in static memory.
    static NET_RX: static_cell::StaticCell<[u8; 1024]> = static_cell::StaticCell::new();
    static NET_TX: static_cell::StaticCell<[u8; 512]> = static_cell::StaticCell::new();
    static NET_RESP: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    let scratch = NetScratch {
        rx: NET_RX.init([0; 1024]),
        tx: NET_TX.init([0; 512]),
        resp: NET_RESP.init([0; 256]),
    };

    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(wifi_weather_fetcher(controller, stack, scratch).unwrap());

    info!("esp-hologram started");
    // Keep `_trng` alive for the whole program lifetime so the entropy source stays enabled.
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}

/// Drives the embassy-net stack. Pure infrastructure required by any embassy-net application.
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiInterface<'static>>) -> ! {
    runner.run().await
}

/// Connects (and reconnects) to WiFi, then periodically fetches weather + local time and publishes
/// the result for the renderer. Network failures are logged and retried; the UI never blocks.
#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "the awaited TCP/connect futures legitimately exceed the strict 1KB threshold"
)]
async fn wifi_weather_fetcher(
    mut controller: WifiController<'static>,
    stack: Stack<'static>,
    scratch: NetScratch,
) {
    let auth_method = if WIFI_PASSWORD.is_empty() {
        AuthenticationMethod::None
    } else {
        AuthenticationMethod::Wpa2Personal
    };
    let station = StationConfig::default()
        .with_ssid(WIFI_SSID)
        .with_auth_method(auth_method)
        .with_password(WIFI_PASSWORD.into());
    if let Err(e) = controller.set_config(&WifiConfig::Station(station)) {
        warn!("WiFi config error: {:?}", e);
    }

    loop {
        if !controller.is_connected() {
            info!("Connecting to WiFi SSID '{}'...", WIFI_SSID);
            match controller.connect_async().await {
                Ok(_) => {
                    info!("WiFi connected, waiting for DHCP...");
                    stack.wait_config_up().await;
                    if let Some(cfg) = stack.config_v4() {
                        info!("Got IP: {}", cfg.address);
                    }
                }
                Err(e) => {
                    warn!("WiFi connect failed: {:?}", e);
                    Timer::after(Duration::from_secs(5)).await;
                    continue;
                }
            }
        }

        match net::fetch(stack, WEATHER_CITY, scratch.rx, scratch.tx, scratch.resp).await {
            Some(update) => {
                info!("Weather: {}", update.weather.label());
                UPDATE.signal(update);
            }
            None => warn!("Weather fetch failed"),
        }
        Timer::after(WEATHER_PERIOD).await;
    }
}

/// The double-buffered render pipeline, clocked to a safe I2C rate. The ssd1306 buffered mode is
/// the back buffer; `flush` ships only the changed region to the panel.
#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "the awaited async flush future legitimately exceeds the strict 1KB threshold"
)]
async fn render_loop(display: &'static mut Display, seed: u32) {
    let mut scene = Scene::new(seed);
    let mut ticker = Ticker::every(Duration::from_millis(FRAME_MS));
    loop {
        if let Some(update) = UPDATE.try_take() {
            scene.apply(update);
        }
        scene.tick();
        scene.draw(display);
        let _ = display.flush().await;
        ticker.next().await;
    }
}
