# provision32

A **configurable** ESP32 WiFi **captive-portal provisioning** library in Rust.

- Brings up an open AP (customizable name) with DHCP + DNS hijack, so any
  device that joins is redirected to a provisioning page.
- Serves a captive-portal form (**Chinese / English** UI) listing nearby WiFi
  networks in a dropdown.
- Receives the chosen SSID + password, **persists it to Flash** (survives
  power loss) and verifies it by switching to STA mode.
- On the next boot, if valid credentials are stored, the device
  **auto-connects in pure STA mode and never opens the AP again**.

The UI strings are gated behind the `i18n-zh` / `i18n-en` cargo features, so
you only pay for the languages you actually ship (smaller `.rodata`).

---

## Features

| Feature | Description |
|---------|-------------|
| Configurable AP name | `ProvisionConfig::with_ap_ssid("MyDevice-Setup")` |
| Chinese / English UI | `Lang::Chinese` / `Lang::English` (feature-gated) |
| Custom gateway IP | default `192.168.4.1/24` |
| Custom storage address | Flash offset, default `0x9000` |
| Tunable timeouts / retries | connect timeout, retries, wait duration |
| Captive-portal compatible | works with Android / iOS / Windows / macOS probes |
| Credential persistence | written to Flash via `esp_storage`, auto-reconnect on boot |

### Cargo features

| Feature | Default | Effect |
|---------|---------|--------|
| `i18n-zh` | yes | Compile the Simplified-Chinese portal / pending pages |
| `i18n-en` | yes | Compile the English portal / pending pages |

Disable a language to drop its static strings from flash, e.g. ship English
only:

```toml
[dependencies]
provision32 = { version = "0.2", default-features = false, features = ["i18n-en"] }
```

`Lang` always exists as the public API; when only one language is compiled the
other variant is unavailable and the page is fixed at compile time (no runtime
branch, no wasted `.rodata`).

---

## Quick start (see `examples/basic.rs`)

```rust
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
        .with_ap_ssid("MyDevice-Setup")   // custom AP name
        .with_lang(Lang::English);        // UI language

    println!("Starting WiFi provisioning (`{}`) ...", cfg.ap_ssid);
    let (controller, interfaces) = provision32::start_wifi(peripherals.WIFI, &cfg);
    let _stack = provision32::run(spawner, controller, interfaces, cfg).await;

    // `run` returns once all background services are up; you own the loop.
    // Drive an LED, poll sensors, or just idle.
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
```

---

## Public API

### Configuration: `ProvisionConfig`

```rust
let cfg = ProvisionConfig::default()
    .with_ap_ssid("MyDevice-Setup")       // AP name (default "ESP32配网页面")
    .with_gw_ip(Ipv4Addr::new(192,168,4,1)) // gateway IP (default 192.168.4.1)
    .with_store_addr(0x9000)              // Flash storage offset
    .with_lang(Lang::English)             // UI language
    .with_wait_before_connect(25)         // seconds to wait after password received
    .with_connect_timeout(20)             // per-attempt STA connect timeout (s)
    .with_connect_retries(2)               // connect retry count
    .with_http_workers(4);                 // HTTP worker task count
```

### Convenience functions

| Function | Purpose |
|----------|---------|
| `start_wifi(wifi, &cfg)` | Start WiFi in AP+STA mode, returns `(controller, interfaces)` |
| `run(spawner, controller, interfaces, cfg)` -> `Stack` | Start the full provisioning stack (AP/DHCP/DNS/HTTP + auto-reconnect) and return the live `Stack` — **main entry** (does **not** block) |
| `load_credentials(&cfg)` | Read saved credentials `(ssid, password)`, `None` if absent |
| `store_credentials(&cfg, ssid, password)` | Write credentials to Flash |
| `try_auto_connect(&mut controller, &cfg, ssid, password)` | Connect in STA-only mode, returns `true` on success |
| `connection_state()` | Async query of the current `ConnectionState` (provisioning / connecting / connected / failed) |
| `Lang::from_hint("en")` | Pick language from a hint string |

### Connection state

The library tracks its progress in a shared `ConnectionState`, which you can
poll from your own task to drive an LED, a display, or logging:

```rust
match provision32::connection_state().await {
    provision32::ConnectionState::Provisioning { ap_ssid } => {
        // AP is up; tell the user to join `ap_ssid`.
    }
    provision32::ConnectionState::Connecting { ssid } => {
        // Verifying credentials for `ssid`...
    }
    provision32::ConnectionState::Connected { ssid } => {
        // Online on `ssid` (STA only, no AP).
    }
    provision32::ConnectionState::Failed { ssid } => {
        // Last attempt for `ssid` failed; AP re-opened.
    }
}
```

> Note: the internal channel (`CONNECT_CH`), WiFi-list types, and a few
> parsing helpers are intentionally **not** public — only the items documented
> above form the stable API surface.

### Internal helpers (for custom pages)

- `render_portal(&cfg, &wifi_list)` — build the portal HTML
- `render_pending(&cfg, ssid)` — build the "password received" page
- `parse_form(body)` / `urldecode(s)` — parse the form
- `wifi_list_options(&list)` — build `<option>` entries
- `mk_static!(T, val)` — convenience macro for a `static` cell

---

## Workflow

1. **First boot** (no credentials): open AP `ESP32配网页面`; once a phone
   joins, any URL is DNS-hijacked to `http://192.168.4.1/`.
2. **User action**: pick WiFi from the dropdown and enter the password, submit.
3. **Verify**: after submit the "password received" page shows (~25s), and the
   ESP32 verifies the password in STA mode in the background.
4. **Success**: credentials are saved to Flash, software reset → next boot
   auto-connects in pure STA mode, AP **never appears again**.
5. **Failure**: the AP re-opens so the user can retry.

---

## Build & flash

Requires the `esp` toolchain (nightly) and target `xtensa-esp32-none-elf`, with
`xtensa-esp-elf-gcc` on your `PATH`:

```powershell
$env:PATH = 'f:\Arduino\xtensa-esp-elf\xtensa-esp-elf\bin;' + $env:PATH
cargo build --release
cargo espflash flash --release
```

To shrink flash by dropping a language:

```powershell
cargo build --release --no-default-features --features i18n-en
```

Configuration is already included in `.cargo/config.toml` (build-std +
`linkall.x`) and `rust-toolchain.toml` (`channel = "esp"`).

---

## Examples

- `examples/basic.rs` — minimal firmware wiring the library to a real ESP32.

---

## Dependencies

- `esp-hal` / `esp-radio` (WiFi) / `esp-storage` (Flash persistence)
- `embassy-net`, `embassy-executor`, `embassy-sync`
- `edge-dhcp`, `edge-nal`, `edge-nal-embassy` (DHCP server)
- `static_cell`, `heapless`, `embedded-io-async`
