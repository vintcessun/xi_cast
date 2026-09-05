//! 一上电就自动找电视、自动投戏曲。
//!
//! 开机顺序（顺序是有讲究的，见下面每一步的注释）：
//!
//! ```text
//!   1. 打开 flash 目录        ← 先做，这时候还没联网，擦写最安全
//!   2. 扫一遍周围的 AP        ← 只为了把现场情况打进日志
//!   3. 连 WiFi + DHCP
//!   4. 找电视（写死的地址 / 上次那台 / SSDP 扫描）
//!   5. 增量更新节目（失败也没关系，手上有缓存）
//!   6. 一部接一部地投，直到断电
//! ```
//!
//! 除了这个文件和 `net.rs` / `storage.rs`，别的逻辑全在 `xi-cast-core` 里，
//! 而那一份在电脑上被完整测过（`cargo test`，一百多个用例，包括掉电、
//! 电视关机、接口挂掉、TCP 分片各种情况）。
//!
//! # 日志走串口，不走 RTT
//!
//! 这块板子用的是 ESP32-S3 内置的 USB-Serial-JTAG。实测 `probe-rs run` 烧录
//! 一切正常，但 RTT 一个字都读不出来。所以日志改成 `esp-println` 打到串口：
//! 同一根 USB 线，`espflash monitor` 直接就能看，也不需要调试器。

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{
    Config, ControllerConfig, Interface, PowerSaveMode, WifiController,
    scan::ScanConfig,
    sta::{ScanMethod, StationConfig},
};
use esp_storage::FlashStorage;
use log::{error, info, warn};
use xi_cast::net::EmbassyNet;
use xi_cast::{mk_static, storage};
use xi_cast_core::app::{App, Config as AppConfig};
use xi_cast_core::store::Catalog;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

/// WiFi 账号密码由 `build.rs` 从 `firmware/wifi.toml` 读进来（那个文件不进版本库）。
const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");
/// 写死的电视地址（`wifi.toml` 里的 `tv_url`，可以留空）。
///
/// 留空就走 SSDP 扫描；填了就只认这一台，连不上也不会去扫别的
/// —— 电视 IP 在路由器里绑死之后，这是最省事也最不会投错的做法。
const TV_URL: &str = env!("TV_URL");
/// 电视的 IP（`wifi.toml` 里的 `tv_ip`）。**推荐用这个。**
///
/// 填了之后板子只跟这一个 IP 打交道：先单播问它要设备描述地址，
/// 问不到再试几个常见端口。全程不发组播 —— 也就不可能把戏投到
/// 邻居家的盒子上。端口和路径不用管，板子自己问。
const TV_IP: &str = env!("TV_IP");
/// 只连这个 BSSID 的 AP（`wifi.toml` 里的 `bssid`，形如 `aa:bb:cc:11:22:33`）。
///
/// **这是对付「2.4G 和 5G 用同一个 SSID」的关键。** ESP32 只有 2.4G，
/// 而路由器开了 band steering 时会试图把客户端往 5G 上赶，表现就是
/// 「连上一下又被踢掉」。把 2.4G 那个射频的 MAC 填在这里，板子就只认它，
/// 整套 band steering 逻辑都绕开了 —— 不用改路由器任何设置。
///
/// 填之前先看开机日志：下面那段扫描会把每个 AP 的 BSSID、信道、信号都打出来。
const BSSID: &str = env!("WIFI_BSSID");
/// 只连这个信道（`wifi.toml` 里的 `channel`，2.4G 是 1~13）。同样是绕开 band steering 用的。
const CHANNEL: &str = env!("WIFI_CHANNEL");

/// TCP 收发缓冲。1500 是一个以太网帧，再大对我们这点数据没有意义。
const TCP_BUF: usize = 1536;
/// SSDP 用的 UDP 缓冲。一次搜索会收到好几台设备的响应，留宽一点。
const UDP_BUF: usize = 2048;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // 显式指定等级，不用 init_logger_from_env()：那个的等级来自编译期的
    // ESP_LOG 环境变量，没设的话默认是「全关」，表现就是一条日志都不出来
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // esp-radio 要用堆。这两行的大小照抄 esp-hal 官方的 wifi 例子 ——
    // 模板里默认的 73744 对 WiFi + 网络栈是不够的
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    // 注意：只启动核 0。第二个核一旦跑起来，写 flash 就会失败
    //（esp-storage 默认策略是「另一个核在跑就拒绝写」），
    // 详见 storage.rs 里关于 cache 的那段说明
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("================ xi_cast 启动 ================");

    // ---------------------------------------------------------------- ① flash
    //
    // 放在连 WiFi **之前**：全新的板子第一次开机要擦掉一整个 bank
    //（256KB = 64 个扇区），那段时间指令 cache 是反复关掉的，
    // WiFi 收不了包。现在还没连上网，没有连接会因此断掉。
    let flash = mk_static!(FlashStorage<'static>, FlashStorage::new(peripherals.FLASH));
    let table_buf = mk_static!(
        [u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN],
        [0u8; esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN]
    );
    let region = match storage::open(flash, table_buf) {
        Ok(region) => region,
        Err(e) => {
            error!(
                "打不开数据分区（{e:?}）。是不是没按 partitions.csv 烧过？\
                 用这条命令烧一次：espflash flash --partition-table partitions.csv <elf>"
            );
            halt().await
        }
    };

    let catalog = match Catalog::open(region) {
        Ok(catalog) => catalog,
        Err(e) => {
            error!("flash 目录打不开: {}", e.as_str());
            halt().await
        }
    };
    info!(
        "flash 目录就绪：{} 条节目，{} 字节空闲，第 {} 代",
        catalog.summary().items,
        catalog.free(),
        catalog.generation()
    );

    // ---------------------------------------------------------------- ② WiFi
    if SSID.is_empty() {
        error!("没有配 WiFi：把 firmware/wifi.toml.example 复制成 wifi.toml 填好再编译");
        halt().await
    }

    let mut station = StationConfig::default()
        .with_ssid(SSID)
        .with_password(PASSWORD.into())
        // 加密方式这里**不要动**，用默认的 Wpa2Personal。
        //
        // 这个字段是「最低门槛」，不是「我支持哪些」。ESP-IDF 里的档次是
        // Open < WEP < WPA < WPA2 < WPA/WPA2混合 < WPA3 < WPA2/WPA3混合，
        // 低于门槛的 AP 会被直接跳过。曾经把它设成 Wpa2Wpa3Personal，
        // 结果家里那个路由器（WPA/WPA2 混合）档次比门槛低，固件自己把它滤掉了，
        // 报 NoAccessPointFoundInAuthmodeThreshold —— 看起来像「连不上路由器」，
        // 实际上是自己把路给堵了。默认的 Wpa2Personal 对 WPA2 和
        // WPA/WPA2 混合都放行，实测可用。
        //
        // 默认的 Fast 是「扫到第一个同名 AP 就连」。2.4G 和 5G 同名时，
        // 这个策略容易连上信号差的那个、或者被 band steering 折腾。
        // AllChannels 会把所有信道扫完再挑最好的
        .with_scan_method(ScanMethod::AllChannels)
        // 连不上时多试几次再放弃（要配合 AllChannels 才生效）
        .with_failure_retry_cnt(5);

    if let Some(bssid) = parse_bssid(BSSID) {
        info!("只连指定的 AP: {BSSID}（绕开 band steering）");
        station = station.with_bssid(bssid);
    }
    if let Ok(channel) = CHANNEL.parse::<u8>() {
        info!("只连信道 {channel}");
        station = station.with_channel(channel);
    }

    let (mut controller, interfaces) = match esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(Config::Station(station)),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            error!("WiFi 初始化失败: {e:?}");
            halt().await
        }
    };

    // 关掉省电模式。省电时板子会周期性睡过去，有的路由器会因此把它踢下线，
    // 表现就是「连上一会儿又断」。这东西插着电源用，没必要省
    if let Err(e) = controller.set_power_saving(PowerSaveMode::None) {
        warn!("关省电模式失败（不致命）: {e:?}");
    }

    // 先扫一圈，把现场情况打进日志。连不上的时候这段是唯一能区分
    // 「压根看不见 AP」和「看得见但连不上」的地方
    scan_and_report(&mut controller).await;

    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        // 要几个 socket：DHCP、DNS、一条 TCP、一条 UDP，留点余量
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        seed,
    );

    spawner.spawn(wifi_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    info!("等 WiFi 连上「{SSID}」…");
    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        info!("拿到 IP: {} 网关 {:?}", cfg.address, cfg.gateway);
    }

    // ---------------------------------------------------------------- ③ 开跑
    let net = match EmbassyNet::new(
        stack,
        mk_static!([u8; TCP_BUF], [0u8; TCP_BUF]),
        mk_static!([u8; TCP_BUF], [0u8; TCP_BUF]),
        mk_static!(
            [embassy_net::udp::PacketMetadata; 8],
            [embassy_net::udp::PacketMetadata::EMPTY; 8]
        ),
        mk_static!([u8; UDP_BUF], [0u8; UDP_BUF]),
        mk_static!(
            [embassy_net::udp::PacketMetadata; 8],
            [embassy_net::udp::PacketMetadata::EMPTY; 8]
        ),
        mk_static!([u8; UDP_BUF], [0u8; UDP_BUF]),
        rng,
    ) {
        Ok(net) => net,
        Err(e) => {
            error!("网络初始化失败: {e:?}");
            halt().await
        }
    };

    let app = mk_static!(
        App<EmbassyNet, storage::CatalogFlash>,
        App::new(
            net,
            catalog,
            AppConfig {
                fixed_ip: if TV_IP.is_empty() { None } else { Some(TV_IP) },
                fixed_device: if TV_URL.is_empty() { None } else { Some(TV_URL) },
                ..AppConfig::default()
            },
        )
    );
    // App 的 future 挺大（里面有 8KB 的设备描述缓冲之类），
    // 放进任务池（.bss）而不是主任务的栈上
    spawner.spawn(cast_task(app).unwrap());

    // 主任务没别的事了。留一个心跳，串口上能一眼看出板子还活着
    loop {
        Timer::after(Duration::from_secs(30)).await;
        info!("心跳：还活着");
    }
}

/// 扫一遍周围的 AP，把结果打进日志。
///
/// 只为排查用，但很值：连不上的时候，这段能立刻区分是「看不见这个 SSID」
/// 还是「看得见但连不上」，而且顺手把 2.4G 那个射频的 BSSID 和信道告诉你
/// —— 想绕开 band steering 就是要填这两个值。
async fn scan_and_report(controller: &mut WifiController<'static>) {
    info!("扫描周围的 WiFi…");
    let config = ScanConfig::default().with_max(20);
    match controller.scan_async(&config).await {
        Ok(found) => {
            info!("扫到 {} 个 AP：", found.len());
            let mut 目标可见 = false;
            for ap in &found {
                let b = ap.bssid;
                let 是目标 = ap.ssid.as_str() == SSID;
                目标可见 |= 是目标;
                info!(
                    "  {}{} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  信道 {}  {}dBm  {:?}",
                    if 是目标 { "★ " } else { "  " },
                    ap.ssid.as_str(),
                    b[0],
                    b[1],
                    b[2],
                    b[3],
                    b[4],
                    b[5],
                    ap.channel,
                    ap.signal_strength,
                    ap.auth_method,
                );
            }
            if 目标可见 {
                info!(
                    "★ 就是带星号那个（板子只能看见 2.4G，所以它一定是 2.4G）。\
                     如果连不上，把它的 BSSID 填进 firmware/wifi.toml 的 bssid= 再重新编译，\
                     板子就只认它，不会被 band steering 赶到 5G 上。"
                );
            } else {
                // 注意措辞：这一次扫描只是个快照，每个信道停留时间很短，
                // 漏掉一个 AP 很正常。实测就出现过「这里说没扫到，下一步却连上了」。
                // 所以这里只能是提示，不能说得像结论 —— 真正算数的是
                // 后面那条「WiFi 已连上 / 连 WiFi 失败」
                info!(
                    "这一轮没扫到「{SSID}」。扫描是一次性快照，漏掉很正常，以下面的连接结果为准；\
                     要是连接也一直失败，再检查路由器 2.4G 射频是不是关着（ESP32 只支持 2.4G）。"
                );
            }
        }
        Err(e) => warn!("扫描失败（不影响后面连接）: {e:?}"),
    }
}

/// 把 `aa:bb:cc:11:22:33` 这样的字符串解析成 6 个字节。空字符串返回 `None`。
fn parse_bssid(text: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut count = 0usize;
    for part in text.split([':', '-']) {
        if count == 6 {
            return None;
        }
        out[count] = u8::from_str_radix(part.trim(), 16).ok()?;
        count += 1;
    }
    (count == 6).then_some(out)
}

/// 投屏主循环。永远不返回。
#[embassy_executor::task]
async fn cast_task(app: &'static mut App<EmbassyNet, storage::CatalogFlash>) {
    app.run().await
}

/// 断线自动重连。
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    let mut 失败次数 = 0u32;
    loop {
        match controller.connect_async().await {
            Ok(info) => {
                失败次数 = 0;
                info!("WiFi 已连上: {info:?}");
                // 断开原因很关键：band steering 把我们踢掉时会给出明确的 reason
                let why = controller.wait_for_disconnect_async().await.ok();
                warn!("WiFi 断了: {why:?}");
            }
            Err(e) => {
                失败次数 += 1;
                warn!("连 WiFi 失败（第 {失败次数} 次），5 秒后重试: {e:?}");
                if 失败次数 == 3 {
                    warn!(
                        "连着失败了几次。如果路由器把 2.4G 和 5G 合成了一个 SSID，\
                         把开机日志里带 ★ 那个 BSSID 填进 firmware/wifi.toml 的 bssid=，\
                         重新编译烧录即可 —— 不用改路由器。"
                    );
                }
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// 起不来的时候停在这儿，但别让看门狗以为死机了。
///
/// 不 panic 是故意的：panic 会重启，重启之后还是同样的错误，
/// 串口上就变成刷屏，反而看不清第一条错误是什么。
async fn halt() -> ! {
    loop {
        Timer::after(Duration::from_secs(10)).await;
        error!("已停止。上面那条错误说明了原因。");
    }
}

/// 自己的 panic 处理：打到串口。
///
/// 模板用的是 `panic-rtt-target`，那个只往 RTT 写 —— 而这块板子的 RTT
/// 读不出来，等于 panic 了也看不见。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("\n!!!!!! panic: {} !!!!!!", info);
    loop {}
}
