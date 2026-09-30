//! 在电脑上给核心逻辑接上真网络（tokio）。
//!
//! 板子上这一层是 esp-radio + embassy-net，这里是 tokio，
//! 上面的 [`xi_cast_core::app::App`] 一个字都不用改。

use std::net::SocketAddr;
use std::time::Duration;

use embedded_io_async::{ErrorKind, ErrorType, Read, Write};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use xi_cast_core::net::Net;
use xi_cast_core::ssdp;

/// 把 tokio 的 `TcpStream` 包成 `embedded-io-async` 的样子。
///
/// 只有二十行，但它就是「同一份逻辑两个平台」的接缝所在。
pub struct Conn {
    stream: TcpStream,
    timeout: Duration,
}

impl ErrorType for Conn {
    type Error = ErrorKind;
}

/// 收发的超时。
///
/// 这条必须有，而且板子那边也要有对应的 `TcpSocket::set_timeout`：
/// 「电视接了连接然后一声不吭」是真实会发生的（盒子死机、Wi-Fi 掉了但
/// TCP 还没超时），没有超时的话查状态那一步会永远挂着 —— 表现出来就是
/// 「投了一集之后再也不动了」，而重连逻辑一次都不会被触发。
const IO_TIMEOUT: Duration = Duration::from_secs(10);

impl Read for Conn {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        match tokio::time::timeout(self.timeout, AsyncReadExt::read(&mut self.stream, buf)).await {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(_)) => Err(ErrorKind::Other),
            Err(_) => Err(ErrorKind::TimedOut),
        }
    }
}

impl Write for Conn {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        match tokio::time::timeout(self.timeout, AsyncWriteExt::write(&mut self.stream, buf)).await
        {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(_)) => Err(ErrorKind::Other),
            Err(_) => Err(ErrorKind::TimedOut),
        }
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        match tokio::time::timeout(self.timeout, AsyncWriteExt::flush(&mut self.stream)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(ErrorKind::Other),
            Err(_) => Err(ErrorKind::TimedOut),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// 电脑上的网络实现。
pub struct SimNet {
    ssdp: UdpSocket,
    /// M-SEARCH 发到哪儿。
    ///
    /// 真跑的时候是组播地址 239.255.255.250:1900；
    /// 跑测试的时候指向本机那台假电视的端口 —— 这样测试不依赖组播，
    /// 不会被 Windows 防火墙拦、也不会在 CI 上时灵时不灵。
    ssdp_dest: SocketAddr,
    /// 简单的伪随机，测试里可以固定种子
    seed: u64,
    /// 把某个主机名重定向到别的地址（测试里把 mapi1.kxm.xmtv.cn 指到本机）
    overrides: Vec<(String, SocketAddr)>,
    /// 单次收发的超时，测试里可以调小
    io_timeout: Duration,
    /// 分别数一数「本该连上」和「碰运气试探」各发生了多少次。
    ///
    /// 电脑上这两种连接行为一样（`connect_probe` 用默认实现），所以模拟器
    /// 测不出超时长短的差别。但**哪一处该算试探**是能测的，而且值得钉死：
    /// 要是哪天有人把查播放状态那条路也标成试探，板子上就会变成 3 秒超时，
    /// 电视一忙就判它失联 —— 这正是真机上踩过的坑。
    pub 正经连接次数: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub 试探连接次数: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SimNet {
    /// 真实环境：往组播地址搜设备。
    pub async fn real(seed: u64) -> std::io::Result<Self> {
        let ssdp = UdpSocket::bind("0.0.0.0:0").await?;
        ssdp.set_multicast_ttl_v4(4)?;
        // 本机跑假设备时要靠组播回环把包送回来
        ssdp.set_multicast_loop_v4(true)?;
        Ok(Self {
            ssdp,
            ssdp_dest: SocketAddr::from((ssdp::MULTICAST_ADDR, ssdp::PORT)),
            seed,
            overrides: Vec::new(),
            io_timeout: IO_TIMEOUT,
            正经连接次数: Default::default(),
            试探连接次数: Default::default(),
        })
    }

    /// 测试环境：M-SEARCH 直接单播给指定地址。
    pub async fn pointing_at(dest: SocketAddr, seed: u64) -> std::io::Result<Self> {
        let ssdp = UdpSocket::bind("127.0.0.1:0").await?;
        Ok(Self {
            ssdp,
            ssdp_dest: dest,
            seed,
            overrides: Vec::new(),
            io_timeout: IO_TIMEOUT,
            正经连接次数: Default::default(),
            试探连接次数: Default::default(),
        })
    }

    /// 把一个主机名解析到指定地址。
    pub fn redirect(&mut self, host: &str, to: SocketAddr) {
        self.overrides.push((host.to_string(), to));
    }

    /// 调小收发超时，好让「对方装死」这类用例几秒钟就跑完。
    pub fn with_io_timeout(mut self, timeout: Duration) -> Self {
        self.io_timeout = timeout;
        self
    }

    fn resolve(&self, host: &str, port: u16) -> Option<SocketAddr> {
        self.overrides
            .iter()
            .find(|(h, _)| h == host)
            .map(|(_, addr)| *addr)
            .or_else(|| {
                use std::net::ToSocketAddrs;
                (host, port).to_socket_addrs().ok()?.next()
            })
    }

    /// `connect` 和 `connect_probe` 的共同实现。
    async fn 连接(&mut self, host: &str, port: u16) -> Result<Conn, Error> {
        let addr = self
            .resolve(host, port)
            .ok_or(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "解析不出地址",
            )))?;
        // 加个连接超时：板子上 smoltcp 自己有超时，电脑上默认能等很久，
        // 不加的话「电视关机了」这条路径要卡两分钟才失败
        let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Io(std::io::Error::from(std::io::ErrorKind::TimedOut)))??;
        Ok(Conn {
            stream,
            timeout: self.io_timeout,
        })
    }
}

impl Net for SimNet {
    type Error = Error;
    type Conn<'a> = Conn;

    async fn connect(&mut self, host: &str, port: u16) -> Result<Self::Conn<'_>, Self::Error> {
        self.正经连接次数
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.连接(host, port).await
    }

    async fn connect_probe(
        &mut self,
        host: &str,
        port: u16,
    ) -> Result<Self::Conn<'_>, Self::Error> {
        self.试探连接次数
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 电脑上不区分快慢，只记一笔是谁来的
        self.连接(host, port).await
    }

    async fn ssdp_send(
        &mut self,
        payload: &[u8],
        dest: Option<[u8; 4]>,
    ) -> Result<(), Self::Error> {
        let target = match dest {
            // 单播：只问这一台
            Some([a, b, c, d]) => {
                SocketAddr::from((std::net::Ipv4Addr::new(a, b, c, d), ssdp::PORT))
            }
            // 组播（测试里被指向假电视的端口）
            None => self.ssdp_dest,
        };
        self.ssdp.send_to(payload, target).await?;
        Ok(())
    }

    async fn ssdp_recv(
        &mut self,
        buf: &mut [u8],
        timeout_ms: u32,
    ) -> Result<Option<usize>, Self::Error> {
        match tokio::time::timeout(
            Duration::from_millis(u64::from(timeout_ms)),
            self.ssdp.recv_from(buf),
        )
        .await
        {
            Ok(Ok((n, _))) => Ok(Some(n)),
            Ok(Err(e)) => Err(Error::Io(e)),
            Err(_) => Ok(None),
        }
    }

    async fn sleep_ms(&mut self, ms: u32) {
        tokio::time::sleep(Duration::from_millis(u64::from(ms))).await;
    }

    fn random(&mut self) -> u32 {
        // xorshift，够用了
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed >> 32) as u32
    }
}
