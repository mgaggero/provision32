//! Example firmware: ESP32 WiFi captive-portal provisioning with `provision32`.
//!
//! Build the library with a single language to shrink flash usage, e.g.:
//! `cargo build --release --no-default-features --features i18n-zh`

#![no_std]
#![no_main]

use {
    embassy_executor::Spawner,
    esp_alloc as _, esp_backtrace as _,
    esp_bootloader_esp_idf::esp_app_desc, esp_hal::{
        clock::CpuClock,
        interrupt::software::SoftwareInterruptControl,
        timer::timg::TimerGroup,
    },
    esp_println::println,
    provision32::{Lang, ProvisionConfig},
};

esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // Configure: custom AP name + UI language.
    let cfg = ProvisionConfig::default()
        .with_ap_ssid("ESP32配网页面")
        .with_lang(Lang::Chinese);

    println!("Starting WiFi provisioning (`{}`) ...", cfg.ap_ssid);
    let (controller, interfaces) = provision32::start_wifi(peripherals.WIFI, &cfg);
    let _stack = provision32::run(spawner, controller, interfaces, cfg).await;

    // `run` returns once all background services are up; the caller owns the
    // main loop. Drive an LED, poll sensors, idle, etc. here.
    loop {
        match provision32::connection_state().await {
            provision32::ConnectionState::Connected { ssid } => {
                println!("Online on `{ssid}`");
            }
            provision32::ConnectionState::Provisioning { ap_ssid } => {
                println!("Portal up — join `{ap_ssid}`");
            }
            provision32::ConnectionState::Connecting { ssid } => {
                println!("Connecting to `{ssid}` ...");
            }
            provision32::ConnectionState::Failed { ssid } => {
                println!("Failed to connect to `{ssid}`; AP reopened");
            }
        }
        embassy_time::Timer::after(embassy_time::Duration::from_secs(10)).await;
    }
}
