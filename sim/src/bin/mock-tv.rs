//! 起一台假电视，摆在电脑上当接收端。
//!
//! 两种用法：
//!
//! ```powershell
//! # ① 局域网模式：板子（或者别的设备）能通过 SSDP 真的搜到它
//! cargo run -p xi-cast-sim --bin mock-tv -- 192.168.0.105
//!
//! # ② 回环模式：只在本机用，不碰组播，不会被防火墙拦
//! cargo run -p xi-cast-sim --bin mock-tv
//! ```
//!
//! 局域网模式要绑 1900 端口，而 Windows 的 SSDPSRV 服务占着它 ——
//! 用了 `SO_REUSEADDR` 和它共享。第一次跑的时候 Windows 防火墙会弹窗，
//! **专用网络和公用网络都要勾上**，不然板子的包进不来。
//!
//! 它会把收到的每一次投屏都打出来，包括电视那边实际拿到的播放地址。

use std::time::Duration;

use xi_cast_sim::mock_tv::MockTv;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 每投一集之后被查 10 次状态就报「播完了」，好观察自动切集
    let tv = match std::env::args().nth(1) {
        Some(ip) => {
            let iface: std::net::Ipv4Addr = ip
                .parse()
                .map_err(|_| std::io::Error::other(format!("这不是一个 IPv4 地址: {ip}")))?;
            log::info!("局域网模式，网卡 {iface}");
            MockTv::start_lan(10, iface).await?
        }
        None => {
            log::info!("回环模式（要让局域网里的设备搜到，请把本机网卡 IP 当参数传进来）");
            MockTv::start(10).await?
        }
    };

    log::info!("假电视已启动");
    log::info!("  设备描述: {}", tv.location);
    log::info!("  SSDP 监听: {}", tv.ssdp_addr);
    log::info!("等着被投屏…（Ctrl-C 退出）");

    let mut 上次 = 0usize;
    let mut 上次动作 = 0usize;
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let state = tv.state();
        if state.uris.len() != 上次 {
            上次 = state.uris.len();
            log::info!(
                "▶ 第 {} 次投屏：《{}》",
                上次,
                state.current_title.as_deref().unwrap_or("(没有标题)")
            );
            log::info!("   地址 {}", state.current_uri.as_deref().unwrap_or("(空)"));
        }
        // 把收到的 SOAP 动作也打出来，联调时能一眼看出走到哪一步了
        if state.actions.len() != 上次动作 {
            for action in &state.actions[上次动作..] {
                log::debug!("   收到 SOAP: {action}");
            }
            上次动作 = state.actions.len();
        }
    }
}
