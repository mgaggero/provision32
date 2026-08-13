# provision32

一个**可配置**的 ESP32 WiFi **强制门户（captive portal）配网** Rust 库。

- 开放一个热点（名称可自定义），配合 DHCP + DNS 劫持，让任何连入设备都被重定向到配网页。
- 提供配网表单（支持**中文 / 英文**界面），以下拉列表展示附近 WiFi。
- 接收用户选择的 SSID + 密码，**持久化到 Flash**（掉电不丢），并切到 STA 模式验证。
- 下次上电若已有有效凭据，则**纯 STA 模式自动连接，不再开放热点**。

界面文案由 `i18n-zh` / `i18n-en` 两个 cargo feature 控制，**只编译你真正需要的语言**，从而减小 `.rodata` 静态存储占用。

---

## 特性

| 特性 | 说明 |
|------|------|
| 可配置热点名 | `ProvisionConfig::with_ap_ssid("MyDevice-Setup")` |
| 中 / 英文界面 | `Lang::Chinese` / `Lang::English`（由 feature 控制） |
| 自定义网关 IP | 默认 `192.168.4.1/24` |
| 自定义存储地址 | Flash 偏移，默认 `0x9000` |
| 可调超时 / 重试 | 连接超时、重试次数、等待时长 |
| 强制门户兼容 | 同时兼容 Android / iOS / Windows / macOS 探测 |
| 凭据持久化 | `esp_storage` 写入 Flash，重启自动回连 |

### Cargo features

| Feature | 默认 | 作用 |
|---------|------|------|
| `i18n-zh` | 是 | 编译简体中文配网页 / "已收到密码"页 |
| `i18n-en` | 是 | 编译英文配网页 / "已收到密码"页 |

关闭某个语言即可把对应静态字符串从 Flash 中剔除，例如只发布英文版：

```toml
[dependencies]
provision32 = { version = "0.2", default-features = false, features = ["i18n-en"] }
```

`Lang` 始终作为公共 API 存在；当只编译一种语言时，另一种变体不可用，页面在编译期即固定（无运行时分支、不浪费 `.rodata`）。

---

## 快速开始（详见 `examples/basic.rs`）

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

    // 只需两步：构造配置 + 启动
    let cfg = ProvisionConfig::default()
        .with_ap_ssid("ESP32配网页面")      // 自定义热点名
        .with_lang(Lang::Chinese);          // 界面语言

    println!("Starting WiFi provisioning (`{}`) ...", cfg.ap_ssid);
    let (controller, interfaces) = provision32::start_wifi(peripherals.WIFI, &cfg);
    let _stack = provision32::run(spawner, controller, interfaces, cfg).await;

    // `run` 在后台服务全部启动后返回，主循环由你掌控。
    // 可驱动 LED、轮询传感器，或直接空闲。
    loop {
        match provision32::connection_state().await {
            provision32::ConnectionState::Connected { ssid } => {
                println!("已连上 `{ssid}`");
            }
            provision32::ConnectionState::Provisioning { ap_ssid } => {
                println!("配网热点已开启 — 请连接 `{ap_ssid}`");
            }
            provision32::ConnectionState::Connecting { ssid } => {
                println!("正在连接 `{ssid}` …");
            }
            provision32::ConnectionState::Failed { ssid } => {
                println!("连接 `{ssid}` 失败，热点已重新开放");
            }
        }
        embassy_time::Timer::after(embassy_time::Duration::from_secs(10)).await;
    }
}
```

---

## 公共 API

### 配置：`ProvisionConfig`

```rust
let cfg = ProvisionConfig::default()
    .with_ap_ssid("MyDevice-Setup")        // 热点名（默认 "ESP32配网页面"）
    .with_gw_ip(Ipv4Addr::new(192,168,4,1))// 网关 IP（默认 192.168.4.1）
    .with_store_addr(0x9000)               // Flash 存储偏移
    .with_lang(Lang::Chinese)              // 界面语言
    .with_wait_before_connect(25)          // 收到密码后等待秒数（让"已收到"页可见）
    .with_connect_timeout(20)              // 每次 STA 连接超时（秒）
    .with_connect_retries(2)               // 连接失败重试次数
    .with_http_workers(4);                 // HTTP 工作线程数
```

### 便利函数

| 函数 | 作用 |
|------|------|
| `start_wifi(wifi, &cfg)` | 以 AP+STA 启动 WiFi，返回 `(controller, interfaces)` |
| `run(spawner, controller, interfaces, cfg)` -> `Stack` | 启动完整配网栈（AP/DHCP/DNS/HTTP + 自动回连）并返回活动 `Stack`，**主入口**（**不阻塞**） |
| `load_credentials(&cfg)` | 读取已保存凭据 `(ssid, password)`，无则返回 `None` |
| `store_credentials(&cfg, ssid, password)` | 将凭据写入 Flash |
| `try_auto_connect(&mut controller, &cfg, ssid, password)` | 仅 STA 模式连接，成功返回 `true` |
| `connection_state()` | 异步查询当前 `ConnectionState`（配网中 / 连接中 / 已连接 / 失败） |
| `Lang::from_hint("en")` | 由语言提示字符串选语言 |

### 连接状态

库会将当前进度记录到共享的 `ConnectionState` 中，你可在自己的任务里轮询它来驱动 LED、显示屏或日志：

```rust
match provision32::connection_state().await {
    provision32::ConnectionState::Provisioning { ap_ssid } => {
        // 热点已开启，提示用户连接 `ap_ssid`。
    }
    provision32::ConnectionState::Connecting { ssid } => {
        // 正在验证 `ssid` 的密码……
    }
    provision32::ConnectionState::Connected { ssid } => {
        // 已连上 `ssid`（纯 STA，无热点）。
    }
    provision32::ConnectionState::Failed { ssid } => {
        // `ssid` 上次连接失败，热点已重新开放。
    }
}
```

> 说明：内部的通道（`CONNECT_CH`）、WiFi 列表类型及若干解析辅助函数为刻意**非公开**，只有上文列出的项目构成稳定 API 面。

### 内部工具（如需自定义页面）

- `render_portal(&cfg, &wifi_list)`：生成配网页 HTML
- `render_pending(&cfg, ssid)`：生成"已收到密码"页
- `parse_form(body)` / `urldecode(s)`：解析表单
- `wifi_list_options(&list)`：生成下拉 `<option>`
- `mk_static!(T, val)`：创建 `static` cell 的便利宏

---

## 工作流程

1. **首次上电**（无凭据）：开放热点 `ESP32配网页面`，手机连入后任意网址都被 DNS 劫持到 `http://192.168.4.1/`。
2. **用户操作**：在页面下拉选择 WiFi 并输入密码，提交。
3. **验证**：表单提交后返回"已收到密码"页面（约 25 秒可见），ESP32 后台切 STA 验证密码。
4. **成功**：凭据已存 Flash，软件复位 → 下次纯 STA 自动连接，热点**不再出现**。
5. **失败**：热点重新开放，用户可重试。

---

## 构建与烧录

需安装 `esp` 工具链（nightly）与目标 `xtensa-esp32-none-elf`，并将 `xtensa-esp-elf-gcc` 加入 `PATH`：

```powershell
$env:PATH = 'f:\Arduino\xtensa-esp-elf\xtensa-esp-elf\bin;' + $env:PATH
cargo build --release
cargo espflash flash --release
```

只保留一种语言以节省 Flash：

```powershell
cargo build --release --no-default-features --features i18n-zh
```

配置已包含在 `.cargo/config.toml`（build-std + `linkall.x`）与 `rust-toolchain.toml`（`channel = "esp"`）中。

---

## 示例

- `examples/basic.rs` —— 把本库接入真实 ESP32 的最小固件示例。

---

## 依赖

- `esp-hal` / `esp-radio`（WiFi）/ `esp-storage`（Flash 持久化）
- `embassy-net`、`embassy-executor`、`embassy-sync`
- `edge-dhcp`、`edge-nal`、`edge-nal-embassy`（DHCP 服务）
- `static_cell`、`heapless`、`embedded-io-async`
