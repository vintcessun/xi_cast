//! 板子和电脑各自提供网络能力的那一层接口。
//!
//! 上面的业务逻辑（找电视、更新节目、投屏、轮播）**只依赖这个 trait**，
//! 于是同一份逻辑可以：
//!
//! * 在 ESP32 上跑，底下是 esp-radio + embassy-net；
//! * 在电脑上跑，底下是 tokio。
//!
//! 这就是「开发板还没到货，功能先测完」的实现方式 —— 测的不是仿写的一份，
//! 就是上板要跑的那一份。

use embedded_io_async::{Read, Write};

/// 一次网络交互的失败原因。核心逻辑不关心底层是 smoltcp 还是 winsock，
/// 只关心「成没成」和「大概是哪一步没成」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 连不上（对方没开机、地址变了、防火墙）
    Connect,
    /// 连上了但收发出错
    Io,
    /// 对方回的东西不符合协议
    Protocol,
    /// 响应太大，缓冲区放不下
    TooLarge,
    /// HTTP 状态码不是 2xx
    Status(u16),
    /// 超时
    Timeout,
}

impl Error {
    /// 一句话说清是哪一类失败。
    ///
    /// 用 `&'static str` 而不是 `Debug`：`defmt` 打不了任意 `Debug` 类型，
    /// 而日志里真正有用的就是「哪一步没成」。
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::Connect => "连不上",
            Error::Io => "收发出错",
            Error::Protocol => "对方回的东西不符合协议",
            Error::TooLarge => "响应太大放不下",
            Error::Status(_) => "HTTP 状态码不对",
            Error::Timeout => "超时",
        }
    }
}

#[cfg(feature = "defmt")]
impl defmt::Format for Error {
    fn format(&self, f: defmt::Formatter<'_>) {
        match self {
            Error::Status(code) => defmt::write!(f, "HTTP {}", code),
            other => defmt::write!(f, "{}", other.as_str()),
        }
    }
}

/// 平台要提供的东西。
///
/// 关联类型上的生命周期（GAT）是给 embassy-net 准备的：那边的 `TcpSocket`
/// 借用着收发缓冲区，而缓冲区放在平台结构体里，所以连接的生命周期
/// 必须绑在 `&mut self` 上。一次只开一条连接，够用。
// 这个 trait 只在单核、单线程的执行器上用（板子上是 esp-rtos，
// 电脑上是 tokio 的 current_thread），future 不需要 `Send`。
#[allow(async_fn_in_trait)]
pub trait Net {
    type Error: core::fmt::Debug;
    type Conn<'a>: Read + Write
    where
        Self: 'a;

    /// 连一个 TCP 端口。
    async fn connect(&mut self, host: &str, port: u16) -> Result<Self::Conn<'_>, Self::Error>;

    /// 连一个 TCP 端口，但这次只是**试探**：连不上是预期结果。
    ///
    /// 谁会用它：`probe_fixed_ip` 挨个试一串厂商常用的端口，里面绝大多数本来
    /// 就没人听着 —— 电视开着的时候也有 7 条是失败的。
    ///
    /// 为什么要和 [`Net::connect`] 分开：这两种连接对「超时多久」和「失败要不要
    /// 吵」的答案正好相反。试探要快（局域网里 SYN 一来一回不到 1 毫秒，等 10 秒
    /// 纯属浪费，8 个端口就是 80 秒）、要安静（不然每轮刷 8 条把真问题淹掉）；
    /// 而查播放状态、拉节目、取分享页那些**本该连上**，超时该给足、失败该吵。
    ///
    /// 平台不想区分就不用实现，默认就是 [`Net::connect`]。
    async fn connect_probe(
        &mut self,
        host: &str,
        port: u16,
    ) -> Result<Self::Conn<'_>, Self::Error> {
        self.connect(host, port).await
    }

    /// 发一个 SSDP 报文。
    ///
    /// `dest` 为 `None` 时发到组播地址 239.255.255.250:1900（满世界找设备）；
    /// 给了 IP 就**单播**给那一台（只问它一个）。
    ///
    /// 单播这条路是给「电视 IP 已经在路由器里绑死」准备的：只问指定的那台，
    /// 既快，又绝不可能把戏投到邻居家的盒子上 —— 组播是不认门牌号的。
    async fn ssdp_send(&mut self, payload: &[u8], dest: Option<[u8; 4]>)
    -> Result<(), Self::Error>;

    /// 收一个 SSDP 响应；`timeout_ms` 内没收到就返回 `Ok(None)`。
    ///
    /// M-SEARCH 的响应是**单播**回我们的源端口的，所以不加入组播组也收得到。
    async fn ssdp_recv(
        &mut self,
        buf: &mut [u8],
        timeout_ms: u32,
    ) -> Result<Option<usize>, Self::Error>;

    /// 睡一会儿。
    async fn sleep_ms(&mut self, ms: u32);

    /// 一个随机数，用来挑戏。板子上接硬件 RNG。
    fn random(&mut self) -> u32;
}

/// 发一个 HTTP GET，把响应体一段段交给 `sink`。
///
/// `sink` 返回 `false` 表示「够了，别收了」——分享页有 43KB，
/// 而我们要的东西在第 5712 字节，收到就该断开。
pub async fn get<C: Read + Write>(
    conn: &mut C,
    url: &crate::url::HttpUrl<'_>,
    sink: &mut impl FnMut(&[u8]) -> bool,
) -> Result<u16, Error> {
    let mut req = [0u8; 512];
    let n = crate::http::get(&mut req, url).map_err(|_| Error::TooLarge)?;
    conn.write_all(&req[..n]).await.map_err(|_| Error::Io)?;
    conn.flush().await.map_err(|_| Error::Io)?;
    read_response(conn, sink).await
}

/// 发一个 SOAP 请求，把响应体收进 `out`（SOAP 响应都很小）。
pub async fn soap_call<'b, C: Read + Write>(
    conn: &mut C,
    url: &crate::url::HttpUrl<'_>,
    service_type: &str,
    action: &str,
    body: &[u8],
    out: &'b mut [u8],
) -> Result<(u16, &'b [u8]), Error> {
    let mut action_buf = [0u8; 128];
    let action_len = crate::soap::action_header(&mut action_buf, service_type, action)
        .map_err(|_| Error::TooLarge)?;
    let action_str =
        core::str::from_utf8(&action_buf[..action_len]).map_err(|_| Error::Protocol)?;

    let mut req = [0u8; 4096];
    let n = crate::http::post(
        &mut req,
        url,
        crate::soap::CONTENT_TYPE,
        Some(action_str),
        body,
    )
    .map_err(|_| Error::TooLarge)?;
    conn.write_all(&req[..n]).await.map_err(|_| Error::Io)?;
    conn.flush().await.map_err(|_| Error::Io)?;

    let mut len = 0usize;
    let mut truncated = false;
    let status = {
        let out = &mut *out;
        read_response(conn, &mut |part| {
            let take = part.len().min(out.len() - len);
            out[len..len + take].copy_from_slice(&part[..take]);
            truncated |= take < part.len();
            len += take;
            len < out.len()
        })
        .await?
    };
    // 截断了就得说出来。默不作声地把半截 XML 交上去，上层解不出
    // `CurrentTransportState`，只会看到一个含糊的「协议不对」，
    // 而真正的原因是缓冲区给小了
    if truncated {
        return Err(Error::TooLarge);
    }
    Ok((status, &out[..len]))
}

/// 收一个 HTTP 响应：解头、解 body（含 chunked），一段段喂给 `sink`。
async fn read_response<C: Read>(
    conn: &mut C,
    sink: &mut impl FnMut(&[u8]) -> bool,
) -> Result<u16, Error> {
    // 头部缓冲。实测最长的响应头 821 字节（一串 Set-Cookie 加 Via），
    // 给 2KB 是因为 CDN 的头只会越加越多。
    //
    // 注意这里是**直接读进 head_buf 的剩余空间**，不是先读进一个小块再拷过去。
    // 那样写有个只在真板子上才会犯的错：一次 read 返回多少字节完全看 TCP 分片，
    // 「这一片放不下」不等于「整个头放不下」。回环网络上一次能读 512 字节、
    // 凑巧总是对齐，WiFi 上分片是 300/512/512 这种，第三片就会被误判成
    // 「响应头太大」而失败。读进剩余空间的话，n 天然不会超过剩余量。
    let mut head_buf = [0u8; 2048];
    let mut head_len = 0usize;
    let mut chunk = [0u8; 512];

    let (head, mut body_start) = loop {
        if head_len == head_buf.len() {
            return Err(Error::TooLarge); // 真的塞满 2KB 都没见到空行
        }
        let n = conn
            .read(&mut head_buf[head_len..])
            .await
            .map_err(|_| Error::Io)?;
        if n == 0 {
            return Err(Error::Protocol); // 头都没收全就断了
        }
        head_len += n;

        match crate::http::parse_head(&head_buf[..head_len]) {
            Ok(head) => {
                let start = head.header_len;
                break (head, start);
            }
            Err(crate::http::Error::Incomplete) => continue,
            Err(_) => return Err(Error::Protocol),
        }
    };

    let mut decoder = crate::http::BodyDecoder::new(&head);
    let mut want_more = true;

    // 头部之后可能已经带了一截 body
    if body_start < head_len {
        decoder
            .feed(&head_buf[body_start..head_len], &mut |part| {
                want_more &= sink(part);
            })
            .map_err(|_| Error::Protocol)?;
        body_start = head_len;
    }
    let _ = body_start;

    while want_more && !decoder.finished() {
        let n = conn.read(&mut chunk).await.map_err(|_| Error::Io)?;
        if n == 0 {
            break; // 对方关连接 = 正文结束（`Connection: close` 就是这么用的）
        }
        decoder
            .feed(&chunk[..n], &mut |part| {
                want_more &= sink(part);
            })
            .map_err(|_| Error::Protocol)?;
    }

    Ok(head.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个「每次最多给 n 个字节」的假连接。
    ///
    /// 这是本文件里最重要的测试工具：电脑上跑回环时一次 read 动辄几 KB，
    /// 而板子上 WiFi 分片是几百字节一片，很多只在真机上出现的 bug
    /// （比如响应头被切成三片时误判成「头太大」）在回环上永远碰不到。
    /// 有了它，那类 bug 在 `cargo test` 里就能抓出来。
    struct FragmentedReader<'a> {
        data: &'a [u8],
        at: usize,
        max: usize,
    }

    impl embedded_io_async::ErrorType for FragmentedReader<'_> {
        type Error = embedded_io_async::ErrorKind;
    }

    impl embedded_io_async::Read for FragmentedReader<'_> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            let take = (self.data.len() - self.at).min(self.max).min(buf.len());
            buf[..take].copy_from_slice(&self.data[self.at..self.at + take]);
            self.at += take;
            Ok(take)
        }
    }

    /// 极简 `block_on`：这些假连接永远是就绪的，不会真的挂起。
    fn block_on<F: Future>(mut future: F) -> F::Output {
        use core::task::{Context, Poll, Waker};
        let mut future = unsafe { core::pin::Pin::new_unchecked(&mut future) };
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    fn 收一遍(raw: &[u8], 每次最多: usize) -> Result<(u16, std::vec::Vec<u8>), Error> {
        let mut conn = FragmentedReader {
            data: raw,
            at: 0,
            max: 每次最多,
        };
        let mut body = std::vec::Vec::new();
        let status = block_on(read_response(&mut conn, &mut |part| {
            body.extend_from_slice(part);
            true
        }))?;
        Ok((status, body))
    }

    /// 用真实抓到的响应头（819 字节，一堆 Set-Cookie 和 Via）拼一个响应。
    ///
    /// `curl -D` 存下来的文件末尾已经带了那个空行，所以直接接正文就行。
    fn 真实响应() -> std::vec::Vec<u8> {
        let mut raw = std::vec::Vec::from(&include_bytes!("../../fixtures/search_headers.txt")[..]);
        debug_assert!(raw.ends_with(b"\r\n\r\n"));
        // 这个接口是 chunked 的
        raw.extend_from_slice(b"5\r\nhello\r\n7\r\n world!\r\n0\r\n\r\n");
        raw
    }

    #[test]
    fn 响应头被_tcp_切成任意大小都能收全() {
        let raw = 真实响应();
        // 300 这一档最关键：821 字节的头会被切成 300+300+221 三片，
        // 之前那版实现在第三片上会误报「响应头太大」
        for 每次最多 in [1, 7, 64, 200, 300, 512, 1024, 4096] {
            let (status, body) = 收一遍(&raw, 每次最多)
                .unwrap_or_else(|e| panic!("每次读 {每次最多} 字节时失败了: {e:?}"));
            assert_eq!(status, 200);
            assert_eq!(body, b"hello world!");
        }
    }

    #[test]
    fn 响应头真的超过缓冲区才报太大() {
        let mut raw = std::vec::Vec::from(&b"HTTP/1.1 200 OK\r\n"[..]);
        for i in 0..200 {
            raw.extend_from_slice(format!("X-Filler-{i}: {}\r\n", "x".repeat(40)).as_bytes());
        }
        raw.extend_from_slice(b"\r\n");
        assert_eq!(收一遍(&raw, 512).unwrap_err(), Error::TooLarge);
    }

    #[test]
    fn 头还没收全对方就断了() {
        assert_eq!(
            收一遍(b"HTTP/1.1 200 OK\r\nContent-Len", 512).unwrap_err(),
            Error::Protocol
        );
    }

    #[test]
    fn content_length_模式下正文跟着头一起到也不会丢() {
        // 头和正文在同一片里到达是最常见的情况
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let (status, body) = 收一遍(raw, 4096).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn 上层说够了就不再往下收() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world";
        let mut conn = FragmentedReader {
            data: raw,
            at: 0,
            max: 3,
        };
        let mut got = std::vec::Vec::new();
        let status = block_on(read_response(&mut conn, &mut |part| {
            got.extend_from_slice(part);
            // 拿到 5 个字节就喊停 —— 分享页那 43KB 就是这么提前收工的
            got.len() < 5
        }))
        .unwrap();
        assert_eq!(status, 200);
        assert!(
            got.len() < 11,
            "喊停之后不该把整个正文都收完: {}",
            got.len()
        );
    }
}
