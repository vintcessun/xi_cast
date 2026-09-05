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
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::rng::Rng;
use xi_cast_core::net::Net;
use xi_cast_core::ssdp;

/// 单次收发的超时。
///
/// 10 秒是这么定的：分享页 43KB，慢的时候几秒；而电视没反应时又不想干等太久。
const IO_TIMEOUT: Duration = Duration::from_secs(10);

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
        let found = self
            .stack
            .dns_query(host, DnsQueryType::A)
            .await
            .map_err(|_| Error::Dns)?;
        found.first().copied().ok_or(Error::Dns)
    }
}

impl Net for EmbassyNet {
    type Error = Error;
    type Conn<'a> = TcpSocket<'a>;

    async fn connect(&mut self, host: &str, port: u16) -> Result<Self::Conn<'_>, Self::Error> {
        let addr = self.resolve(host).await?;

        // 拆开借用：stack 是 Copy 的，收发缓冲区各借一次
        let Self { stack, rx, tx, .. } = self;
        let mut socket = TcpSocket::new(*stack, &mut rx[..], &mut tx[..]);
        // 没有这一行，「电视接了连接然后装死」会让整个程序停在这里
        socket.set_timeout(Some(IO_TIMEOUT));
        socket
            .connect(IpEndpoint::new(addr, port))
            .await
            .map_err(|_| Error::Connect)?;
        Ok(socket)
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
