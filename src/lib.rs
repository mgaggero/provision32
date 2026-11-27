//! # wifi-provision
//!
//! A configurable ESP32 WiFi **captive-portal provisioning** library.
//!
//! - Spins up an open AP (customizable name) with a DHCP + DNS hijack so any
//!   device that joins is redirected to a provisioning page.
//! - Serves a captive-portal form (Chinese / English) listing nearby WiFi
//!   networks in a dropdown.
//! - Receives the chosen SSID + password, persists them to flash
//!   (survives power loss) and verifies them by switching to STA mode.
//! - On the next boot, if valid credentials are stored, the device
//!   auto-connects in pure STA mode and **never** opens the AP again.
//!
//! ## Quick start
//!
//! ```ignore
//! #[esp_rtos::main]
//! async fn main(spawner: Spawner) -> ! {
//!     // ... init hal / heap / timer / sw-int ...
//!     let peripherals = esp_hal::init(esp_hal::Config::default()
//!         .with_cpu_clock(CpuClock::max()));
//!     let (controller, interfaces) = wifi_provision::start_wifi(
//!         peripherals.WIFI,
//!         &wifi_provision::ProvisionConfig::default()
//!             .with_ap_ssid("MyDevice-Setup")
//!             .with_lang(wifi_provision::Lang::English),
//!     );
//!     wifi_provision::run(spawner, controller, interfaces,
//!         wifi_provision::ProvisionConfig::default()).await
//! }
//! ```
#![no_std]

extern crate alloc;

use {
    core::net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    embassy_executor::Spawner,
    embassy_net::{
        tcp::TcpSocket, udp::PacketMetadata, Config as NetConfig, IpListenEndpoint, Ipv4Cidr,
        Runner, Stack, StackResources, StaticConfigV4,
    },
    embassy_sync::{
        blocking_mutex::raw::CriticalSectionRawMutex,
        channel::Channel,
        mutex::Mutex,
    },
    embassy_time::{with_timeout, Duration, Timer},
    embedded_io_async::Write,
    embedded_storage::{ReadStorage, Storage},
    esp_hal::rng::Rng,
    esp_radio::wifi::{
        ap::AccessPointConfig, sta::StationConfig, Config, ControllerConfig, Interface,
        WifiController,
    },
    esp_storage::FlashStorage,
};

/// Max SSID length accepted on the wire.
pub const MAX_SSID: usize = 32;
/// Max password length accepted on the wire.
pub const MAX_PASSWORD: usize = 64;
/// Max number of surrounding WiFi networks kept for the dropdown.
pub const MAX_WIFI_ENTRIES: usize = 20;

/// Default gateway IP for the AP subnet (192.168.4.1/24).
pub const DEFAULT_GW_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
/// Default flash offset used to persist credentials (4-byte aligned).
pub const DEFAULT_STORE_ADDR: u32 = 0x9000;
/// Default AP (hotspot) name.
pub const DEFAULT_AP_SSID: &str = "ESP32配网页面";

/// Page language.
///
/// The available variants depend on the `i18n-zh` / `i18n-en` cargo features:
/// only the strings for enabled languages are compiled into the firmware, so
/// disabling a language shrinks `.rodata`. When both features are off at most
/// one variant exists and the runtime branch disappears.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    /// 简体中文
    #[cfg(feature = "i18n-zh")]
    Chinese,
    /// English
    #[cfg(feature = "i18n-en")]
    English,
}

impl Lang {
    /// Select the language by a two-letter hint. Falls back to the only
    /// compiled language when the other is not built in.
    pub fn from_hint(hint: &str) -> Lang {
        #[cfg(all(feature = "i18n-zh", feature = "i18n-en"))]
        {
            if hint.eq_ignore_ascii_case("en") || hint.eq_ignore_ascii_case("english") {
                Lang::English
            } else {
                Lang::Chinese
            }
        }
        #[cfg(all(feature = "i18n-zh", not(feature = "i18n-en")))]
        {
            let _ = hint;
            Lang::Chinese
        }
        #[cfg(all(feature = "i18n-en", not(feature = "i18n-zh")))]
        {
            let _ = hint;
            Lang::English
        }
    }
}

/// Configuration for the provisioning flow.
#[derive(Clone)]
pub struct ProvisionConfig {
    /// AP (hotspot) SSID shown to the user's phone.
    pub ap_ssid: heapless::String<32>,
    /// Gateway IP of the AP subnet.
    pub gw_ip: Ipv4Addr,
    /// Flash offset where credentials are persisted.
    pub store_addr: u32,
    /// Page language.
    pub lang: Lang,
    /// Seconds to wait after the portal receives the password before
    /// switching to STA and verifying it (keeps the "received" page visible).
    pub wait_before_connect_secs: u32,
    /// Timeout (seconds) for each STA connect attempt.
    pub connect_timeout_secs: u32,
    /// Number of STA connect retries before reopening the AP.
    pub connect_retries: u32,
    /// HTTP worker task count.
    pub http_workers: usize,
}

impl Default for ProvisionConfig {
    fn default() -> Self {
        let mut ap_ssid = heapless::String::new();
        let _ = ap_ssid.push_str(DEFAULT_AP_SSID);
        Self {
            ap_ssid,
            gw_ip: DEFAULT_GW_IP,
            store_addr: DEFAULT_STORE_ADDR,
            #[cfg(all(feature = "i18n-zh", feature = "i18n-en"))]
            lang: Lang::Chinese,
            #[cfg(all(feature = "i18n-zh", not(feature = "i18n-en")))]
            lang: Lang::Chinese,
            #[cfg(all(feature = "i18n-en", not(feature = "i18n-zh")))]
            lang: Lang::English,
            wait_before_connect_secs: 25,
            connect_timeout_secs: 20,
            connect_retries: 2,
            http_workers: 4,
        }
    }
}

impl ProvisionConfig {
    /// Set the AP (hotspot) name.
    pub fn with_ap_ssid(mut self, ssid: &str) -> Self {
        let mut s = heapless::String::new();
        let _ = s.push_str(ssid);
        self.ap_ssid = s;
        self
    }

    /// Set the gateway IP for the AP subnet.
    pub fn with_gw_ip(mut self, ip: Ipv4Addr) -> Self {
        self.gw_ip = ip;
        self
    }

    /// Set the flash offset used to persist credentials.
    pub fn with_store_addr(mut self, addr: u32) -> Self {
        self.store_addr = addr;
        self
    }

    /// Set the page language.
    pub fn with_lang(mut self, lang: Lang) -> Self {
        self.lang = lang;
        self
    }

    /// Set seconds to wait after the portal receives the password.
    pub fn with_wait_before_connect(mut self, secs: u32) -> Self {
        self.wait_before_connect_secs = secs;
        self
    }

    /// Set the connect attempt timeout (seconds).
    pub fn with_connect_timeout(mut self, secs: u32) -> Self {
        self.connect_timeout_secs = secs;
        self
    }

    /// Set the number of connect retries.
    pub fn with_connect_retries(mut self, n: u32) -> Self {
        self.connect_retries = n.max(1);
        self
    }

    /// Set the number of HTTP worker tasks.
    pub fn with_http_workers(mut self, n: usize) -> Self {
        self.http_workers = n.max(1);
        self
    }
}

/// A provisioning request: the chosen target SSID + password.
#[derive(Clone)]
pub struct ConnectReq {
    /// Target WiFi SSID.
    pub ssid: heapless::String<MAX_SSID>,
    /// Target WiFi password (empty = open network).
    pub password: heapless::String<MAX_PASSWORD>,
}

/// Channel between the HTTP server and the reconnection task.
pub(crate) static CONNECT_CH: Channel<CriticalSectionRawMutex, ConnectReq, 1> = Channel::new();

/// Scanned nearby WiFi SSID list shared between tasks.
pub type WifiList = heapless::Vec<heapless::String<33>, MAX_WIFI_ENTRIES>;
/// Mutex-protected shared WiFi list.
pub type SharedWifiList = Mutex<CriticalSectionRawMutex, WifiList>;

/// Convenience macro to create a `static` cell.
#[macro_export]
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}

/// Re-export of `static_cell` so downstream crates don't need the dependency
/// directly when using [`mk_static!`].
pub use static_cell;

/// Start WiFi in AP+STA mode using the given config (initial AP SSID).
/// Returns the controller plus the station/AP interfaces.
pub fn start_wifi(
    wifi: esp_hal::peripherals::WIFI<'static>,
    cfg: &ProvisionConfig,
) -> (WifiController<'static>, esp_radio::wifi::Interfaces<'static>) {
    let ap_cfg = Config::AccessPointStation(
        StationConfig::default(),
        AccessPointConfig::default().with_ssid(cfg.ap_ssid.as_str()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        wifi,
        ControllerConfig::default().with_initial_config(ap_cfg),
    )
    .unwrap();
    (controller, interfaces)
}

/// Persist credentials to flash at `cfg.store_addr`.
pub fn store_credentials(cfg: &ProvisionConfig, ssid: &str, password: &str) {
    let mut flash = FlashStorage::new();
    let mut blob = [0xFFu8; 128];
    let mut off = 0usize;
    blob[off] = ssid.len() as u8;
    off += 1;
    blob[off..off + ssid.len()].copy_from_slice(ssid.as_bytes());
    off += ssid.len();
    blob[off] = password.len() as u8;
    off += 1;
    blob[off..off + password.len()].copy_from_slice(password.as_bytes());
    off += password.len();

    let len = (off + 3) & !3; // write requires 4-byte alignment
    if let Err(e) = flash.write(cfg.store_addr, &blob[..len]) {
        esp_println::println!("Flash write error: {e:?}");
    } else {
        esp_println::println!("Credentials persisted to flash @ {:#x} ({} bytes)", cfg.store_addr, len);
    }
}

/// Load persisted credentials, if any.
pub fn load_credentials(
    cfg: &ProvisionConfig,
) -> Option<(heapless::String<MAX_SSID>, heapless::String<MAX_PASSWORD>)> {
    let mut flash = FlashStorage::new();
    let mut buf = [0u8; 128];
    flash.read(cfg.store_addr, &mut buf).ok()?;
    if buf[0] == 0xFF {
        return None;
    }
    let mut off = 0usize;
    let slen = buf[off] as usize;
    off += 1;
    if slen == 0 || slen > MAX_SSID {
        return None;
    }
    let mut ssid = heapless::String::new();
    let _ = ssid.push_str(core::str::from_utf8(&buf[off..off + slen]).ok()?);
    off += slen;
    let plen = buf[off] as usize;
    off += 1;
    if plen > MAX_PASSWORD {
        return None;
    }
    let mut password = heapless::String::new();
    let _ = password.push_str(core::str::from_utf8(&buf[off..off + plen]).ok()?);
    Some((ssid, password))
}

/// Try to auto-connect to a stored network in pure STA mode.
/// Returns `true` on success (caller should then just idle).
pub async fn try_auto_connect(
    controller: &mut WifiController<'static>,
    cfg: &ProvisionConfig,
    ssid: &str,
    password: &str,
) -> bool {
    let sta_cfg = if password.is_empty() {
        StationConfig::default().with_ssid(ssid)
    } else {
        StationConfig::default()
            .with_ssid(ssid)
            .with_password(alloc::string::String::from(password))
    };
    let _ = controller.set_config(&Config::Station(sta_cfg));
    match with_timeout(
        Duration::from_secs(cfg.connect_timeout_secs as u64),
        controller.connect_async(),
    )
    .await
    {
        Ok(Ok(_)) => {
            esp_println::println!("Auto-connect OK — staying on target WiFi, AP OFF.");
            true
        }
        other => {
            esp_println::println!("Auto-connect FAIL: {other:?} — falling back to portal AP.");
            false
        }
    }
}

/// Bring up the full provisioning stack (AP + DHCP + DNS + HTTP) and run it.
/// This is the main entry point: call it from `main` after initializing the
/// HAL and starting the WiFi controller.
/// Current provisioning / connection state, queryable at runtime via
/// [`connection_state`].
#[derive(Clone)]
pub enum ConnectionState {
    /// Device is in provisioning (AP) mode, waiting for the user to submit
    /// credentials through the captive portal.
    Provisioning {
        /// The hotspot (AP) name currently advertised.
        ap_ssid: heapless::String<32>,
    },
    /// Credentials were submitted and are being verified in STA mode.
    Connecting {
        /// Target WiFi SSID being connected to.
        ssid: heapless::String<MAX_SSID>,
    },
    /// Successfully connected to the target WiFi (pure STA, AP closed).
    Connected {
        /// Target WiFi SSID.
        ssid: heapless::String<MAX_SSID>,
    },
    /// Last attempt failed; the AP was re-opened so the user can retry.
    Failed {
        /// Target WiFi SSID that failed.
        ssid: heapless::String<MAX_SSID>,
    },
}

/// Shared runtime state, updated by the provisioning tasks.
static STATE: Mutex<CriticalSectionRawMutex, ConnectionState> =
    Mutex::new(ConnectionState::Provisioning {
        ap_ssid: heapless::String::new(),
    });

/// Update the shared connection state.
async fn set_state(s: ConnectionState) {
    *STATE.lock().await = s;
}

/// Query the current provisioning / connection state.
///
/// ```ignore
/// let st = provision32::connection_state().await;
/// match st {
///     provision32::ConnectionState::Connected { ssid } => { /* online */ }
///     provision32::ConnectionState::Provisioning { .. } => { /* portal up */ }
///     _ => {}
/// }
/// ```
pub async fn connection_state() -> ConnectionState {
    STATE.lock().await.clone()
}

/// Bring up the full provisioning stack (AP + DHCP + DNS + HTTP) and run it,
/// or — when valid credentials are already stored — connect in pure STA mode.
///
/// This function only **starts** the background services (network task, DHCP
/// server, DNS hijack, HTTP portal, reconnection task) and returns the live
/// [`Stack`] handle. It does **not** block: the caller is expected to own the
/// main loop afterwards (drive an LED, poll sensors, idle, etc.).
///
/// The returned `Stack` is a `Copy` handle moved out by value. Hold it in the
/// caller's scope for your own networking (e.g. after a successful STA
/// auto-connect). Background services keep independent copies, so the handle
/// stays valid as long as you keep it alive.
pub async fn run(
    spawner: Spawner,
    controller: WifiController<'static>,
    interfaces: esp_radio::wifi::Interfaces<'static>,
    cfg: ProvisionConfig,
) -> Stack<'static> {
    set_state(ConnectionState::Provisioning {
        ap_ssid: cfg.ap_ssid.clone(),
    })
    .await;

    let saved = load_credentials(&cfg);
    match &saved {
        Some((ssid, pass)) => {
            esp_println::println!("Stored credentials: SSID=`{ssid}` PASSWORD=`{pass}`")
        }
        None => esp_println::println!("No stored credentials yet."),
    }

    // Attempt boot auto-connect to stored network.
    let mut controller = controller;
    let mut auto_ok = false;
    if let Some((ref ssid, ref pass)) = saved {
        esp_println::println!("Auto-connecting to `{ssid}` ...");
        auto_ok = try_auto_connect(&mut controller, &cfg, ssid.as_str(), pass.as_str()).await;
    }

    if auto_ok {
        let device = interfaces.station;
        let net_cfg = NetConfig::dhcpv4(Default::default());
        let seed = random_seed();
        let (_stack, runner) = embassy_net::new(
            device,
            net_cfg,
            mk_static!(StackResources<10>, StackResources::<10>::new()),
            seed,
        );
        spawner.spawn(net_task(runner).expect("spawn net_task"));
        esp_println::println!("Device is now on target WiFi (no AP). Provisioning done.");
        set_state(ConnectionState::Connected {
            ssid: saved.as_ref().unwrap().0.clone(),
        })
        .await;
        // Hand the live stack back to the caller; it owns the main loop now.
        return _stack;
    }

    // --- Provisioning mode: open AP and start the portal services. ---
    let device = interfaces.access_point;
    esp_println::println!("Entering provisioning mode (AP `{}`).", cfg.ap_ssid);

    let wifi_list = mk_static!(SharedWifiList, Mutex::new(WifiList::new()));

    let net_cfg = NetConfig::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(cfg.gw_ip, 24),
        gateway: Some(cfg.gw_ip),
        dns_servers: Default::default(),
    });

    let seed = random_seed();
    let (stack, runner) = embassy_net::new(
        device,
        net_cfg,
        mk_static!(StackResources<10>, StackResources::<10>::new()),
        seed,
    );
    // `Stack` is a `Copy` handle — services below receive independent copies
    // via `stack`, and the original is moved out to the caller on return.
    spawner.spawn(net_task(runner).expect("spawn net_task"));
    spawner.spawn(dhcp_server(stack, cfg.gw_ip).expect("spawn dhcp_server"));
    spawner.spawn(dns_server(stack, cfg.gw_ip).expect("spawn dns_server"));
    spawner.spawn(https_reject(stack).expect("spawn https_reject"));

    spawner.spawn(
        http_server(spawner, stack, wifi_list, cfg.clone()).expect("spawn http_server"),
    );

    let controller = mk_static!(WifiController<'static>, controller);
    spawner.spawn(
        reconnect_task(controller, wifi_list, cfg.clone()).expect("spawn reconnect_task"),
    );

    esp_println::println!(
        "AP up: connect to `{}` and open http://{}/",
        cfg.ap_ssid, cfg.gw_ip
    );
    esp_println::println!("(or any URL — DNS hijack redirects to the portal)");

    // All background services are running. Move the stack handle out to the
    // caller and let it drive the main loop.
    stack
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    esp_println::println!("net_task: started");
    runner.run().await
}

fn random_seed() -> u64 {
    let rng = Rng::new();
    ((rng.random() as u64) << 32) | rng.random() as u64
}

/// Escapes an SSID for safe insertion into an HTML attribute / text node.
fn escape_html(out: &mut heapless::String<2048>, s: &str) {
    for &b in s.as_bytes() {
        match b {
            b'"' => {
                let _ = out.push_str("&quot;");
            }
            b'&' => {
                let _ = out.push_str("&amp;");
            }
            b'<' => {
                let _ = out.push_str("&lt;");
            }
            b'>' => {
                let _ = out.push_str("&gt;");
            }
            _ => {
                let _ = out.push(b as char);
            }
        }
    }
}

/// Build `<option>` entries for the WiFi dropdown. The first entry is marked
/// `selected`.
pub fn wifi_list_options(list: &WifiList) -> heapless::String<2048> {
    let mut out = heapless::String::new();
    let mut first = true;
    for ssid in list {
        let _ = out.push_str("<option value=\"");
        escape_html(&mut out, ssid.as_str());
        let _ = out.push('"');
        if first {
            let _ = out.push_str(" selected");
            first = false;
        }
        let _ = out.push('>');
        let _ = out.push_str(ssid.as_str());
        let _ = out.push_str("</option>");
    }
    out
}

/// Render the portal page with the WiFi options filled in.
pub fn render_portal(cfg: &ProvisionConfig, list: &WifiList) -> heapless::Vec<u8, 4096> {
    let opts = wifi_list_options(list);
    let body_tmpl = portal_body(cfg.lang);
    let mut body: heapless::Vec<u8, 3072> = heapless::Vec::new();
    let placeholder = b"__WIFI_LIST__";
    let mut rest = body_tmpl.as_slice();
    while let Some(idx) = find_subslice(rest, placeholder) {
        let _ = body.extend_from_slice(&rest[..idx]);
        let _ = body.extend_from_slice(opts.as_bytes());
        rest = &rest[idx + placeholder.len()..];
    }
    let _ = body.extend_from_slice(rest);
    build_html_response(&body)
}

/// Render the "password received, connecting" page.
pub fn render_pending(cfg: &ProvisionConfig, ssid: &str) -> heapless::Vec<u8, 4096> {
    let body = pending_body(cfg.lang, ssid, cfg.ap_ssid.as_str());
    build_html_response(&body)
}

/// Wrap a body in a complete HTTP response with Content-Length.
fn build_html_response(body: &[u8]) -> heapless::Vec<u8, 4096> {
    let mut out: heapless::Vec<u8, 4096> = heapless::Vec::new();
    let _ = out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    let _ = out.extend_from_slice(b"Content-Type: text/html; charset=utf-8\r\n");
    let _ = out.extend_from_slice(b"Connection: close\r\n");
    let _ = out.extend_from_slice(b"Cache-Control: no-store\r\n");
    let _ = out.extend_from_slice(b"Content-Length: ");
    let _ = out.extend_from_slice(itoa(body.len()).as_bytes());
    let _ = out.extend_from_slice(b"\r\n\r\n");
    let _ = out.extend_from_slice(body);
    out
}

/// Minimal itoa (heapless-friendly, no alloc).
fn itoa(n: usize) -> heapless::String<10> {
    let mut s = heapless::String::new();
    if n == 0 {
        let _ = s.push('0');
        return s;
    }
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    let mut n = n;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _ = s.push_str(core::str::from_utf8(&buf[i..]).unwrap_or("0"));
    s
}

/// Find a subslice, returning its start index.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| haystack[i..].starts_with(needle))
}

/// Value of the `Content-Length` header in a raw request head, if present.
fn content_length(head: &[u8]) -> Option<usize> {
    let s = core::str::from_utf8(head).ok()?;
    s.split("\r\n").skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

/// Parse a `application/x-www-form-urlencoded` body (`ssid=..&password=..`).
pub fn parse_form(body: &[u8]) -> Option<(heapless::String<MAX_SSID>, heapless::String<MAX_PASSWORD>)> {
    let mut ssid = heapless::String::new();
    let mut password = heapless::String::new();
    let s = core::str::from_utf8(body).ok()?;
    for pair in s.split('&') {
        let mut it = pair.splitn(2, '=');
        let key = it.next().unwrap_or("");
        let val = it.next().unwrap_or("");
        let val = urldecode(val);
        match key {
            "ssid" => {
                let _ = ssid.push_str(&val);
            }
            "password" => {
                let _ = password.push_str(&val);
            }
            _ => {}
        }
    }
    if ssid.is_empty() {
        None
    } else {
        Some((ssid, password))
    }
}

/// url-decode a form value (`+` → space, `%XX` → byte).
pub fn urldecode(input: &str) -> heapless::String<MAX_PASSWORD> {
    let mut out = heapless::String::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                let _ = out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    let _ = out.push((h * 16 + l) as u8 as char);
                }
                i += 3;
            }
            c => {
                let _ = out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

// ---------- Network services ----------

/// DHCP server: hands out `192.168.4.x` and points DNS to the gateway.
#[embassy_executor::task]
async fn dhcp_server(stack: Stack<'static>, gw_ip: Ipv4Addr) {
    use edge_dhcp::{io::{self, DEFAULT_SERVER_PORT}, server::{Server, ServerOptions}};
    use edge_nal::UdpBind;
    use edge_nal_embassy::{Udp, UdpBuffers};

    let mut buf = [0u8; 1500];
    let mut gw_buf = [gw_ip];
    let buffers = UdpBuffers::<3, 1024, 1024, 10>::new();
    let unbound = Udp::new(stack, &buffers);
    let mut socket = unbound
        .bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DEFAULT_SERVER_PORT)))
        .await
        .unwrap();
    esp_println::println!("dhcp_server: bound on :{DEFAULT_SERVER_PORT}");

    let mut opts = ServerOptions::new(gw_ip, Some(&mut gw_buf));
    let gateways = [gw_ip];
    let dns = [gw_ip];
    opts.gateways = &gateways;
    opts.dns = &dns;
    opts.subnet = Some(Ipv4Addr::new(255, 255, 255, 0));
    opts.lease_duration_secs = 3600;
    opts.captive_url = Some("http://192.168.4.1/");

    loop {
        match io::server::run(&mut Server::<_, 64>::new_with_et(gw_ip), &opts, &mut socket, &mut buf).await {
            Ok(_) => esp_println::println!("DHCP: request handled"),
            Err(e) => esp_println::println!("DHCP server error: {e:?}"),
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}

/// DNS server: hijacks every A query to the gateway IP (captive portal).
#[embassy_executor::task]
async fn dns_server(stack: Stack<'static>, gw_ip: Ipv4Addr) {
    const DNS_PORT: u16 = 53;
    let mut rx_meta = [PacketMetadata::EMPTY; 10];
    let mut tx_meta = [PacketMetadata::EMPTY; 10];
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 1024];

    let mut socket = embassy_net::udp::UdpSocket::new(stack, &mut rx_meta, &mut rx_buffer, &mut tx_meta, &mut tx_buffer);
    socket.bind(DNS_PORT).unwrap();
    esp_println::println!("dns_server: bound on :{DNS_PORT}");

    let mut buf = [0u8; 512];
    let mut out = [0u8; 512];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, src)) => {
                esp_println::println!("DNS: query {n} bytes from {src:?}");
                if let Some(resp_len) = build_dns_reply(&buf[..n], &mut out, gw_ip) {
                    let _ = socket.send_to(&out[..resp_len], src).await;
                }
            }
            Err(e) => {
                esp_println::println!("DNS recv error: {e:?}");
                Timer::after(Duration::from_millis(500)).await;
            }
        }
    }
}

/// Build a DNS reply that answers every query with `gw_ip`. Returns the length.
fn build_dns_reply(query: &[u8], out: &mut [u8], gw_ip: Ipv4Addr) -> Option<usize> {
    if query.len() < 12 {
        return None;
    }
    out[..12].copy_from_slice(&query[..12]);
    out[2] = 0x81;
    out[3] = 0x80;
    out[6] = 0x00;
    out[7] = 0x01;
    out[8..12].copy_from_slice(&[0u8; 4]);

    let mut pos = 12;
    while pos < query.len() {
        let len = query[pos] as usize;
        pos += 1;
        if len == 0 {
            pos += 4;
            break;
        }
        pos += len;
    }
    if pos > query.len() {
        return None;
    }
    let ans_start = pos;
    if ans_start + 16 > out.len() {
        return None;
    }
    out[12..pos].copy_from_slice(&query[12..pos]);

    out[ans_start..ans_start + 2].copy_from_slice(&[0xC0, 0x0C]);
    out[ans_start + 2..ans_start + 4].copy_from_slice(&[0x00, 0x01]);
    out[ans_start + 4..ans_start + 6].copy_from_slice(&[0x00, 0x01]);
    out[ans_start + 6..ans_start + 10].copy_from_slice(&[0x00, 0x00, 0x00, 0x3c]);
    out[ans_start + 10..ans_start + 12].copy_from_slice(&[0x00, 0x04]);
    out[ans_start + 12..ans_start + 16].copy_from_slice(&gw_ip.octets());
    Some(ans_start + 16)
}

/// Port 443: cleanly reject (no TLS on ESP32) so HTTPS captive probes fail
/// without hanging the portal.
#[embassy_executor::task]
async fn https_reject(stack: Stack<'static>) {
    esp_println::println!("https_reject: listening on :443 (clean close)");
    loop {
        let mut rx_buffer = [0; 1024];
        let mut tx_buffer = [0; 1024];
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(5)));
        if socket.accept(IpListenEndpoint { addr: None, port: 443 }).await.is_err() {
            continue;
        }
        esp_println::println!("https_reject: accepted 443, closing cleanly");
        socket.close();
        Timer::after(Duration::from_millis(200)).await;
    }
}

/// HTTP server: spawns N workers that each accept + handle one connection.
#[embassy_executor::task]
async fn http_server(spawner: Spawner, stack: Stack<'static>, wifi_list: &'static SharedWifiList, cfg: ProvisionConfig) {
    esp_println::println!("http_server: spawning {} workers on :80", cfg.http_workers);
    for _ in 0..cfg.http_workers {
        spawner.spawn(http_worker(stack, wifi_list, cfg.clone()).expect("spawn http_worker"));
    }
}

#[embassy_executor::task(pool_size = 8)]
async fn http_worker(stack: Stack<'static>, wifi_list: &'static SharedWifiList, cfg: ProvisionConfig) {
    loop {
        let mut rx_buf = alloc::vec![0u8; 1536];
        let mut tx_buf = alloc::vec![0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf[..], &mut tx_buf[..]);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if let Err(e) = socket.accept(IpListenEndpoint { addr: None, port: 80 }).await {
            esp_println::println!("worker accept error: {e:?}");
            continue;
        }
        esp_println::println!("http_worker: accepted connection");

        let mut buf = [0u8; 1024];
        let mut pos = 0;
        let mut headers_done = false;
        let mut method_post = false;
        let mut body_start = 0usize;

        while !headers_done {
            match socket.read(&mut buf[pos..]).await {
                Ok(0) => break,
                Ok(len) => {
                    pos += len;
                    if let Some(idx) = find_subslice(&buf[..pos], b"\r\n\r\n") {
                        headers_done = true;
                        body_start = idx + 4;
                        if buf[..pos].starts_with(b"POST") {
                            method_post = true;
                        }
                    }
                }
                Err(e) => {
                    esp_println::println!("read error: {e:?}");
                    socket.close();
                    return;
                }
            }
            if pos >= buf.len() {
                break;
            }
        }

        if !headers_done {
            socket.close();
            return;
        }

        // Some clients send the body in a later segment than the headers
        // (e.g. `Expect: 100-continue`), so read until `Content-Length` is met.
        let mut body_end = pos;
        if method_post && let Some(len) = content_length(&buf[..body_start]) {
            body_end = body_start.saturating_add(len);
            while pos < body_end && pos < buf.len() {
                match socket.read(&mut buf[pos..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => pos += n,
                }
            }
        }

        if method_post {
            // Reject a body that is incomplete or larger than the buffer
            // rather than parsing a truncated password.
            let body = (pos >= body_end).then(|| &buf[body_start..body_end]);
            if let Some((ssid, password)) = body.and_then(parse_form) {
                esp_println::println!("Received SSID=`{ssid}` PASSWORD=`{password}`");
                store_credentials(&cfg, ssid.as_str(), password.as_str());

                let req = ConnectReq {
                    ssid: ssid.clone(),
                    password: password.clone(),
                };
                CONNECT_CH.send(req).await;

                let html = render_pending(&cfg, ssid.as_str());
                match socket.write_all(&html).await {
                    Ok(_) => esp_println::println!("http_server: wrote pending OK"),
                    Err(e) => esp_println::println!("http_server: write pending ERR: {e:?}"),
                }
            } else {
                let _ = socket
                    .write_all(b"HTTP/1.0 400 Bad Request\r\nConnection: close\r\n\r\n")
                    .await;
            }
        } else {
            let req_line = core::str::from_utf8(&buf[..pos]).unwrap_or("");
            let first_line = req_line.split("\r\n").next().unwrap_or("");
            let path = first_line.split_whitespace().nth(1).unwrap_or("/");
            esp_println::println!("http_server: GET path=`{path}`");

            let is_probe = path.contains("generate_204")
                || path.contains("gen_204")
                || path.contains("/blank")
                || path.contains("/check_network")
                || path.contains("ncsi")
                || path.contains("connecttest")
                || path.contains("clients3.google.com")
                || path.contains("gstatic.com/generate_204")
                || path.contains("msftconnecttest")
                || path.contains("microsoft.com")
                || path.contains("apple.com")
                || path.contains("captive.apple")
                || path.contains("wifi.google")
                || path.contains("detectportal")
                || path.contains("network-check")
                || path.contains("/time");
            let is_ios = path.contains("hotspot-detect") || path.contains("captive.apple") || path.contains("apple.com");

            if is_probe && !is_ios {
                let _ = socket
                    .write_all(b"HTTP/1.0 302 Found\r\nLocation: http://192.168.4.1/\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Length: 0\r\n\r\n")
                    .await;
                esp_println::println!("http_server: wrote 302 redirect for captive probe");
            } else if is_ios {
                let _ = socket
                    .write_all(b"HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<html><body>Success</body></html>")
                    .await;
                esp_println::println!("http_server: wrote hotspot-detect Success");
            } else {
                let snapshot = {
                    let guard = wifi_list.lock().await;
                    let mut s: WifiList = WifiList::new();
                    for item in guard.iter() {
                        let _ = s.push(item.clone());
                    }
                    s
                };
                let html = render_portal(&cfg, &snapshot);
                match socket.write_all(&html).await {
                    Ok(_) => esp_println::println!("http_server: wrote portal OK"),
                    Err(e) => esp_println::println!("http_server: write portal ERR: {e:?}"),
                }
            }
        }

        let _ = socket.flush().await;
        Timer::after(Duration::from_millis(200)).await;
        socket.close();
        Timer::after(Duration::from_millis(200)).await;
    }
}

/// Background task: scans WiFi, then verifies submitted credentials and
/// reboots into STA mode on success (or reopens the AP on failure).
#[embassy_executor::task]
async fn reconnect_task(controller: &'static mut WifiController<'static>, wifi_list: &'static SharedWifiList, cfg: ProvisionConfig) {
    esp_println::println!("Scanning nearby WiFi networks...");
    match controller.scan_async(&esp_radio::wifi::scan::ScanConfig::default()).await {
        Ok(aps) => {
            let mut list = wifi_list.lock().await;
            for ap in &aps {
                let ssid = ap.ssid.as_str();
                if !ssid.is_empty() {
                    let mut s: heapless::String<33> = heapless::String::new();
                    let _ = s.push_str(ssid);
                    let _ = list.push(s);
                }
            }
            esp_println::println!("Scan found {} networks:", list.len());
            for s in list.iter() {
                esp_println::println!("  - {s}");
            }
        }
        Err(e) => esp_println::println!("WiFi scan failed: {e:?}"),
    }

    loop {
        let req = CONNECT_CH.receive().await;
        esp_println::println!("reconnect_task: verify SSID=`{}`", req.ssid);

        set_state(ConnectionState::Connecting {
            ssid: req.ssid.clone(),
        })
        .await;

        Timer::after(Duration::from_secs(cfg.wait_before_connect_secs as u64)).await;

        let sta_cfg = if req.password.is_empty() {
            StationConfig::default().with_ssid(req.ssid.as_str())
        } else {
            StationConfig::default()
                .with_ssid(req.ssid.as_str())
                .with_password(alloc::string::String::from(req.password.as_str()))
        };

        let _ = controller.disconnect_async().await;
        let _ = controller.set_config(&Config::AccessPoint(AccessPointConfig::default().with_ssid(cfg.ap_ssid.as_str())));

        let ap_cfg = AccessPointConfig::default().with_ssid(cfg.ap_ssid.as_str());
        let _ = controller.set_config(&Config::AccessPointStation(sta_cfg.clone(), ap_cfg));

        let mut ok = false;
        for attempt in 1..=cfg.connect_retries {
            match with_timeout(Duration::from_secs(cfg.connect_timeout_secs as u64), controller.connect_async()).await {
                Ok(Ok(_)) => {
                    esp_println::println!("STA connect OK (attempt {attempt})");
                    ok = true;
                    break;
                }
                other => {
                    esp_println::println!("STA connect FAIL (attempt {attempt}): {other:?}");
                    if attempt < cfg.connect_retries {
                        Timer::after(Duration::from_secs(3)).await;
                        let _ = controller.disconnect_async().await;
                    }
                }
            }
        }

        if ok {
            set_state(ConnectionState::Connected {
                ssid: req.ssid.clone(),
            })
            .await;
            esp_println::println!("Connected successfully; credentials saved. Rebooting into STA mode...");
            Timer::after(Duration::from_secs(2)).await;
            esp_hal::system::software_reset();
        } else {
            set_state(ConnectionState::Failed {
                ssid: req.ssid.clone(),
            })
            .await;
            esp_println::println!("Re-enabling AP so user can retry...");
            let _ = controller.disconnect_async().await;
            let _ = controller.set_config(&Config::AccessPoint(AccessPointConfig::default().with_ssid(cfg.ap_ssid.as_str())));
            esp_println::println!("AP re-enabled: `{}`", cfg.ap_ssid);
        }
    }
}

// ---------- HTML templates (i18n) ----------

/// Portal form body for the given language. `__WIFI_LIST__` is replaced at
/// runtime with the `<option>` entries.
///
/// Only the strings for enabled `i18n-zh` / `i18n-en` features are compiled.
fn portal_body(lang: Lang) -> heapless::Vec<u8, 3072> {
    let mut v = heapless::Vec::new();

    #[cfg(feature = "i18n-zh")]
    const ZH: &[u8] = b"<!DOCTYPE html><html lang=\"zh\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>WiFi \xe9\x85\x8d\xe7\xbd\x91</title>\
<style>body{font-family:sans-serif;max-width:420px;margin:40px auto;padding:0 16px}input,select{width:100%;padding:10px;margin:6px 0;box-sizing:border-box;font-size:16px}button{width:100%;padding:12px;background:#007bff;color:#fff;border:0;font-size:16px;margin-top:8px}h2{color:#333}</style>\
</head><body><h2>WiFi \xe9\x85\x8d\xe7\xbd\x91</h2>\
<p>\xe8\xaf\xb7\xe9\x80\x89\xe6\x8b\xa9\xe5\xb9\xb6\xe8\xbe\x93\xe5\x85\xa5 WiFi \xe5\xaf\x86\xe7\xa0\x81\xef\xbc\x8c\xe8\xae\xa9 ESP32 \xe8\xbf\x9e\xe6\x8e\xa5\xe5\x88\xb0\xe4\xbd\xa0\xe7\x9a\x84\xe7\xbd\x91\xe7\xbb\x9c\xef\xbc\x9a</p>\
<form method=\"POST\" action=\"/\">\
<label>WiFi \xe5\x90\x8d\xe7\xa7\xb0\xef\xbc\x88SSID\xef\xbc\x89</label>\
<select name=\"ssid\" id=\"ssid_sel\">\
__WIFI_LIST__\
</select>\
<label>WiFi \xe5\xaf\x86\xe7\xa0\x81</label>\
<input name=\"password\" type=\"password\" placeholder=\"WiFi \xe5\xaf\x86\xe7\xa0\x81\" autocomplete=\"off\">\
<button type=\"submit\">\xe4\xbf\x9d\xe5\xad\x98\xe5\xb9\xb6\xe8\xbf\x9e\xe6\x8e\xa5</button>\
</form>\
</body></html>";

    #[cfg(feature = "i18n-en")]
    const EN: &[u8] = b"<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>WiFi Setup</title>\
<style>body{font-family:sans-serif;max-width:420px;margin:40px auto;padding:0 16px}input,select{width:100%;padding:10px;margin:6px 0;box-sizing:border-box;font-size:16px}button{width:100%;padding:12px;background:#007bff;color:#fff;border:0;font-size:16px;margin-top:8px}h2{color:#333}</style>\
</head><body><h2>WiFi Setup</h2>\
<p>Select your WiFi network and enter the password so ESP32 can connect:</p>\
<form method=\"POST\" action=\"/\">\
<label>WiFi Name (SSID)</label>\
<select name=\"ssid\" id=\"ssid_sel\">\
__WIFI_LIST__\
</select>\
<label>WiFi Password</label>\
<input name=\"password\" type=\"password\" placeholder=\"WiFi password\" autocomplete=\"off\">\
<button type=\"submit\">Save &amp; Connect</button>\
</form>\
</body></html>";

    #[cfg(all(feature = "i18n-zh", feature = "i18n-en"))]
    {
        match lang {
            Lang::Chinese => {
                let _ = v.extend_from_slice(ZH);
            }
            Lang::English => {
                let _ = v.extend_from_slice(EN);
            }
        }
    }
    #[cfg(all(feature = "i18n-zh", not(feature = "i18n-en")))]
    {
        let _ = lang;
        let _ = v.extend_from_slice(ZH);
    }
    #[cfg(all(feature = "i18n-en", not(feature = "i18n-zh")))]
    {
        let _ = lang;
        let _ = v.extend_from_slice(EN);
    }
    v
}

/// "Password received, connecting" body for the given language.
///
/// Only the strings for enabled `i18n-zh` / `i18n-en` features are compiled.
fn pending_body(lang: Lang, ssid: &str, ap_ssid: &str) -> heapless::Vec<u8, 1536> {
    let mut v = heapless::Vec::new();
    let _ = v.extend_from_slice(
        b"<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Connecting</title>\
<style>body{font-family:sans-serif;max-width:420px;margin:40px auto;padding:0 16px;text-align:center}h1{color:#007bff}</style>\
</head><body>",
    );

    #[cfg(feature = "i18n-zh")]
    const ZH_HEAD: &[u8] = b"<h1>\xe2\x9c\x93 \xe8\xae\xbe\xe5\xa4\x87\xe5\xb7\xb2\xe6\x94\xb6\xe5\x88\xb0\xe5\xaf\x86\xe7\xa0\x81</h1><p>ESP32 \xe5\xb0\x86\xe7\xa8\x8d\xe5\x90\x8e\xe5\xb0\x9d\xe8\xaf\x95\xe8\xbf\x9e\xe6\x8e\xa5\xe5\x88\xb0 WiFi\xe3\x80\x8c";
    #[cfg(feature = "i18n-zh")]
    const ZH_MID: &[u8] = b"\xe3\x80\x8d\xe3\x80\x82</p>\
<p style=\"color:#888;font-size:14px\">\xe6\xad\xa3\xe5\x9c\xa8\xe9\xaa\x8c\xe8\xaf\x81\xe5\xaf\x86\xe7\xa0\x81\xef\xbc\x8c\xe8\xaf\xb7\xe5\x8b\xbf\xe5\x85\xb3\xe9\x97\xad\xe6\xad\xa4\xe9\xa1\xb5\xe9\x9d\xa2\xe3\x80\x82<br>\
\xe9\xaa\x8c\xe8\xaf\x81\xe6\x88\x90\xe5\x8a\x9f\xe5\x90\x8e\xe7\x83\xad\xe7\x82\xb9\xe5\xb0\x86\xe8\x87\xaa\xe5\x8a\xa8\xe5\x85\xb3\xe9\x97\xad\xef\xbc\x8c\xe6\x89\x8b\xe6\x9c\xba\xe8\x87\xaa\xe5\x8a\xa8\xe5\x9b\x9e\xe5\x88\xb0\xe6\xad\xa3\xe5\xb8\xb8 WiFi\xe3\x80\x82<br>\
\xe8\x8b\xa5\xe9\x95\xbf\xe6\x97\xb6\xe9\x97\xb4\xe6\x97\xa0\xe5\x93\x8d\xe5\xba\x94\xef\xbc\x8c\xe5\x8f\xaf\xe8\x83\xbd\xe5\xaf\x86\xe7\xa0\x81\xe6\x9c\x89\xe8\xaf\xaf\xef\xbc\x8c\xe8\xaf\xb7\xe9\x87\x8d\xe6\x96\xb0\xe8\xbf\x9e\xe6\x8e\xa5\xe3\x80\x8c";
    #[cfg(feature = "i18n-zh")]
    const ZH_TAIL: &[u8] = b"\xe3\x80\x8d\xe7\x83\xad\xe7\x82\xb9\xe9\x87\x8d\xe8\xaf\x95\xe3\x80\x82</p></body></html>";

    #[cfg(feature = "i18n-en")]
    const EN_HEAD: &[u8] = b"<h1>&#10003; Password received</h1><p>ESP32 will now try to connect to WiFi &quot;";
    #[cfg(feature = "i18n-en")]
    const EN_MID: &[u8] = b"&quot;.</p>\
<p style=\"color:#888;font-size:14px\">Verifying the password, please keep this page open.<br>\
Once verified, the hotspot will close automatically and your phone returns to normal WiFi.<br>\
If there is no response for a long time, the password may be wrong &mdash; please reconnect to the &quot;";
    #[cfg(feature = "i18n-en")]
    const EN_TAIL: &[u8] = b"&quot; hotspot and try again.</p></body></html>";

    #[cfg(all(feature = "i18n-zh", feature = "i18n-en"))]
    {
        match lang {
            Lang::Chinese => {
                let _ = v.extend_from_slice(ZH_HEAD);
                let _ = v.extend_from_slice(ssid.as_bytes());
                let _ = v.extend_from_slice(ZH_MID);
                let _ = v.extend_from_slice(ap_ssid.as_bytes());
                let _ = v.extend_from_slice(ZH_TAIL);
            }
            Lang::English => {
                let _ = v.extend_from_slice(EN_HEAD);
                let _ = v.extend_from_slice(ssid.as_bytes());
                let _ = v.extend_from_slice(EN_MID);
                let _ = v.extend_from_slice(ap_ssid.as_bytes());
                let _ = v.extend_from_slice(EN_TAIL);
            }
        }
    }
    #[cfg(all(feature = "i18n-zh", not(feature = "i18n-en")))]
    {
        let _ = lang;
        let _ = v.extend_from_slice(ZH_HEAD);
        let _ = v.extend_from_slice(ssid.as_bytes());
        let _ = v.extend_from_slice(ZH_MID);
        let _ = v.extend_from_slice(ap_ssid.as_bytes());
        let _ = v.extend_from_slice(ZH_TAIL);
    }
    #[cfg(all(feature = "i18n-en", not(feature = "i18n-zh")))]
    {
        let _ = lang;
        let _ = v.extend_from_slice(EN_HEAD);
        let _ = v.extend_from_slice(ssid.as_bytes());
        let _ = v.extend_from_slice(EN_MID);
        let _ = v.extend_from_slice(ap_ssid.as_bytes());
        let _ = v.extend_from_slice(EN_TAIL);
    }
    v
}
