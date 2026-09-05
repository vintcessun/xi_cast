//! 在电脑上跑整套固件逻辑，接真网络。
//!
//! 这是开发板到货之前的最后一道验证：**同一份 `App`**，
//! 只是网络换成了 tokio、flash 换成了一个磁盘文件。它会真的去 xmtv 拉节目、
//! 真的在局域网里扫 DLNA 设备、真的把戏投到家里那台电视上。
//!
//! ```powershell
//! # 全自动：扫设备 → 拉节目 → 一直放
//! cargo run -p xi-cast-sim
//!
//! # 扫不到设备时，直接指定设备描述地址（组播被路由器拦了就用这个）
//! $env:XI_CAST_TV_URL = "http://192.168.1.20:8200/rootDesc.xml"
//! cargo run -p xi-cast-sim
//!
//! # 想连着假电视跑（不打扰家里人看电视）
//! cargo run -p xi-cast-sim --bin mock-tv     # 窗口 1
//! $env:XI_CAST_TV_URL = "<上面打印出来的地址>"
//! cargo run -p xi-cast-sim                   # 窗口 2
//! ```
//!
//! 环境变量：
//!
//! | 变量 | 作用 |
//! | --- | --- |
//! | `XI_CAST_TV_URL` | 直接指定设备描述地址，跳过 SSDP 扫描 |
//! | `XI_CAST_DEVICE_NAME` | 有多台设备时按名字挑，默认 `FastCast` |
//! | `XI_CAST_FLASH` | 假 flash 文件路径，默认 `xi_cast_flash.bin` |
//! | `XI_CAST_POLL_MS` | 查播放状态的间隔，默认 2000 |
//! | `XI_CAST_SHARE_PAGE` | 设成 1 就直接投分享页地址，不解析 mp4 |

use std::time::Duration;

use xi_cast_core::app::{App, Config};
use xi_cast_core::store::Catalog;
use xi_cast_sim::flash::FileFlash;
use xi_cast_sim::net::SimNet;

/// 假 flash 给 256KB（板子上分区表里给的是 1MB）。
/// 对着 2291 条节目够用，而且模拟器每次写都要整份刷盘，别太大。
const 分区大小: usize = 256 * 1024;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let path = std::env::var("XI_CAST_FLASH").unwrap_or_else(|_| "xi_cast_flash.bin".into());
    let flash = FileFlash::open(&path, 分区大小)?;
    let mut catalog = Catalog::open(flash).map_err(|e| e.as_str())?;
    log::info!(
        "假 flash: {path}（已有 {} 条节目，{} 字节空闲）",
        catalog.summary().items,
        catalog.free()
    );

    // 直接指定设备时，把它当成「上次记住的那台」写进目录 ——
    // 走的就是板子上「记着地址直接连」的那条路径
    if let Ok(url) = std::env::var("XI_CAST_TV_URL") {
        let url = url.trim();
        if !url.is_empty() {
            log::info!("按 XI_CAST_TV_URL 指定设备: {url}");
            catalog
                .remember_device("uuid:手动指定", url)
                .map_err(|e| e.as_str())?;
        }
    }

    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos() as u64
        | 1;
    let net = SimNet::real(seed).await?;

    let name = std::env::var("XI_CAST_DEVICE_NAME").unwrap_or_else(|_| "FastCast".into());
    let cfg = Config {
        // App 的配置里这个字段是 &'static str，泄漏一个字符串换来配置可调，
        // 在一个跑到天荒地老的程序里无所谓
        preferred_name: Box::leak(name.into_boxed_str()),
        poll_ms: std::env::var("XI_CAST_POLL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000),
        cast_share_page: std::env::var("XI_CAST_SHARE_PAGE").is_ok_and(|v| v.trim() == "1"),
        ..Default::default()
    };

    log::info!("开始：找电视 → 更新节目 → 一直放（Ctrl-C 退出）");
    let mut app = App::new(net, catalog, cfg);
    app.run().await
}
