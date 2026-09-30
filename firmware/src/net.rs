//! 把 [`xi_cast_core::net::Net`] 接到 embassy-net 上。
//!
//! 这个文件和 `sim/src/net.rs`（接 tokio 的那份）是一对：
//! 上面的投屏逻辑一个字都不知道自己跑在哪边。电脑上测过的东西，
//! 上板之后行为是一样的 —— 除了下面这几处**只有板子上才存在**的讲究：
//!
//! * **TCP 要设超时。** smoltcp 默认可以永远等下去。电视死机时会出现
//!   「连得上、但一个字节都不回」，没有超时的话查播放状态那一步就永远挂着。
//! * **socket 缓冲区必须是 `'static` 的。** `TcpSocket` 借用着收发缓冲区，
//!   所以缓冲区放在结构体里，连接的生命周期绑在 `&mut self` 上
//!   （核心库那个 `type Conn<'a>` 就是为这件事准备的）。
//! * **一次只开一条连接。** 板子上 RAM 有限，而且我们本来也是一步一步走的：
//!   拿设备描述 → 拉节目 → 解析直链 → 发 SOAP，从来不需要并发。

use core::net::Ipv4Addr;
use core::str::FromStr;

use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::{ConnectError, TcpSocket};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::rng::Rng;
use log::{debug, warn};
use xi_cast_core::net::Net;
use xi_cast_core::ssdp;

/// 单次收发的超时。
///
/// 10 秒是这么定的：分享页 43KB，慢的时候几秒；而电视没反应时又不想干等太久。
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// **建连接**的超时，和 [`IO_TIMEOUT`] 是两件事。
///
/// 收一个 43KB 的分享页可能要好几秒，所以读写给 10 秒；但「对面这个端口上
/// 到底有没有人听着」是毫秒级的问题 —— 局域网里 SYN 一来一回不到 1 毫秒。
///
/// 两件事共用 10 秒的代价实测很贵：电视没开机时 `probe_fixed_ip` 要挨个试
/// 8 个常见端口，每个都等满 10 秒，一轮就是 90 秒 —— 也就是**开了电视之后
/// 最多要等一分半才被发现**。3 秒够慢速链路重传两次，又把一轮压到半分钟内。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum Error {
    /// 域名解析失败（DHCP 给的 DNS 不通，或者根本还没连上网）
    Dns,
    /// TCP 连不上
    Connect,
    /// UDP 收发出错
    Udp,
}

pub struct EmbassyNet {
    stack: Stack<'static>,
    rx: &'static mut [u8],
    tx: &'static mut [u8],
    udp: UdpSocket<'static>,
    rng: Rng,
}

impl EmbassyNet {
    /// 缓冲区全部由调用方以 `'static` 的形式给进来（板子上一般是 `mk_static!`）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stack: Stack<'static>,
        rx: &'static mut [u8],
        tx: &'static mut [u8],
        udp_rx_meta: &'static mut [PacketMetadata],
        udp_rx: &'static mut [u8],
        udp_tx_meta: &'static mut [PacketMetadata],
        udp_tx: &'static mut [u8],
        rng: Rng,
    ) -> Result<Self, Error> {
        let mut udp = UdpSocket::new(stack, udp_rx_meta, udp_rx, udp_tx_meta, udp_tx);
        // 绑一个随机源端口就行：M-SEARCH 的响应是**单播**回源端口的，
        // 不需要加入组播组，也不需要占 1900
        udp.bind(0).map_err(|_| Error::Udp)?;
        Ok(Self {
            stack,
            rx,
            tx,
            udp,
            rng,
        })
    }

    /// 主机名 → IP。是 IP 字面量就直接用，省掉一次 DNS。
    ///
    /// 设备的 LOCATION 里永远是 IP（`http://192.168.1.20:8200/...`），
    /// 只有 xmtv 那两个域名需要真的查 DNS。
    async fn resolve(&mut self, host: &str) -> Result<IpAddress, Error> {
        if let Ok(ip) = Ipv4Addr::from_str(host) {
            return Ok(IpAddress::Ipv4(ip));
        }
        // 为什么要打这条日志：上层把「解析不了」和「连不上」都归成
        // core 那个 `net::Error::Connect`，日志里只剩「连不上」三个字。
        // 真出问题时这两件事的排查方向完全不同 —— 一个查 DHCP 给的 DNS，
        // 一个查路由和对方端口。板子上唯一的窗口就是串口，这里不说就没人知道了
        let found = self
            .stack
            .dns_query(host, DnsQueryType::A)
            .await
            .map_err(|e| {
                warn!("解析域名 {host} 失败: {e:?}（DHCP 给的 DNS 不通？）");
                Error::Dns
            })?;
        found.first().copied().ok_or_else(|| {
            warn!("解析域名 {host} 没返回任何地址");
            Error::Dns
        })
    }

    /// `connect` 和 `connect_probe` 的共同实现。
    ///
    /// `建连接超时` 只卡建连接这一步，**不包括**上面的 `resolve`：这个网络上
    /// DHCP 发的第一个 DNS 要穿过路由器问上游，解析本身就可能十几秒，
    /// 一起卡的话所有按域名的连接全必败。
    ///
    /// 读写超时始终是 [`IO_TIMEOUT`]：收一个 43KB 的分享页可能要好几秒，
    /// 这和「对面有没有人听着」是两件事。
    async fn 建连接(
        &mut self,
        host: &str,
        port: u16,
        建连接超时: Duration,
        试探: bool,
    ) -> Result<TcpSocket<'_>, Error> {
        let addr = self.resolve(host).await?;

        // 拆开借用：stack 是 Copy 的，收发缓冲区各借一次
        let Self { stack, rx, tx, .. } = self;
        let mut socket = TcpSocket::new(*stack, &mut rx[..], &mut tx[..]);
        // 没有这一行，「电视接了连接然后装死」会让整个程序停在这里
        socket.set_timeout(Some(IO_TIMEOUT));

        let 开始 = Instant::now();
        let 结果 = with_timeout(建连接超时, socket.connect(IpEndpoint::new(addr, port))).await;
        let 为什么 = match 结果 {
            Ok(Ok(())) => return Ok(socket),
            // 具体是哪一种失败，排查方向差很远
            Ok(Err(ConnectError::NoRoute)) => "没有路由（网关 ARP 问不到？）",
            Ok(Err(ConnectError::ConnectionReset)) => "对方 reset",
            Ok(Err(ConnectError::InvalidState)) => "socket 状态不对",
            Ok(Err(ConnectError::TimedOut)) => "对方不理（smoltcp 自己的超时）",
            Err(_) => "对方不理（等满建连接超时）",
        };
        let 等了 = 开始.elapsed().as_millis();
        if 试探 {
            // 试探失败是日常 —— 8 个端口里本来就只有一个能连上。
            // 打成 warn 会每轮刷 8 条，把真问题淹掉
            debug!("试探 {host}:{port} 没人听着: {为什么}，等了 {等了} 毫秒");
        } else {
            warn!("连 {host}:{port}（{addr:?}）失败: {为什么}，等了 {等了} 毫秒");
        }
        Err(Error::Connect)
    }
}

impl Net for EmbassyNet {
    type Error = Error;
    type Conn<'a> = TcpSocket<'a>;

    async fn connect(&mut self, host: &str, port: u16) -> Result<Self::Conn<'_>, Self::Error> {
        // 本该连上的连接：超时给足，失败要吵
        self.建连接(host, port, IO_TIMEOUT, false).await
    }

    async fn connect_probe(
        &mut self,
        host: &str,
        port: u16,
    ) -> Result<Self::Conn<'_>, Self::Error> {
        // 碰运气试端口：快，而且失败不吵
        self.建连接(host, port, CONNECT_TIMEOUT, true).await
    }

    async fn ssdp_send(
        &mut self,
        payload: &[u8],
        dest: Option<[u8; 4]>,
    ) -> Result<(), Self::Error> {
        // 没指定就发组播，指定了就只发给那一台
        let [a, b, c, d] = dest.unwrap_or(ssdp::MULTICAST_ADDR);
        let target = IpEndpoint::new(IpAddress::Ipv4(Ipv4Addr::new(a, b, c, d)), ssdp::PORT);
        self.udp
            .send_to(payload, target)
            .await
            .map_err(|_| Error::Udp)
    }

    async fn ssdp_recv(
        &mut self,
        buf: &mut [u8],
        timeout_ms: u32,
    ) -> Result<Option<usize>, Self::Error> {
        match with_timeout(
            Duration::from_millis(u64::from(timeout_ms)),
            self.udp.recv_from(buf),
        )
        .await
        {
            Ok(Ok((n, _))) => Ok(Some(n)),
            Ok(Err(_)) => Err(Error::Udp),
            // 等够了没人回，这不是错误
            Err(_) => Ok(None),
        }
    }

    async fn sleep_ms(&mut self, ms: u32) {
        Timer::after(Duration::from_millis(u64::from(ms))).await;
    }

    fn random(&mut self) -> u32 {
        self.rng.random()
    }
}
