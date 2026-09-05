//! 手写的极小 HTTP/1.1 客户端零件：拼请求、拆响应头、解 chunked。
//!
//! 为什么不用现成的库：板子上要的功能就三个 —— 发一个 GET、发一个 POST、
//! 把响应体流式地喂给上层扫描。而两个真实服务器都有各自的脾气，
//! 这些是实测出来的（见 `fixtures/` 里抓的原始报文）：
//!
//! * xmtv 的接口用 **chunked** 传输，没有 `Content-Length`，
//!   不解 chunked 就会把 `1f4a\r\n` 这种长度行当成 JSON 内容；
//! * 分享页在 CDN 的 WAF 后面，**不带浏览器 User-Agent 会返回 418**
//!   （抓包实测：curl 默认 UA → `HTTP/1.1 418`，`Block-Event-Id: ...`）；
//! * 分享页有 43KB，但 `<source src=` 出现在第 5712 字节，
//!   所以响应体必须能边收边扫、扫到就断开，不能等收完。
//!
//! 这里所有函数都不分配内存，缓冲区由调用方给。

use core::fmt::Write as _;

use crate::url::HttpUrl;

/// 发请求时用的 User-Agent。
///
/// 必须长得像浏览器：分享页所在的 CDN（华为云 WAF）会按 UA 拦截，
/// 用默认的 `curl/8.x` 直接 418。这一行是实测能过的。
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 给的缓冲区装不下要拼的请求
    BufferTooSmall,
    /// 响应头还没收全（不是错误，继续收就行）
    Incomplete,
    /// 对方回的东西不像 HTTP
    Malformed,
}

#[cfg(feature = "defmt")]
impl defmt::Format for Error {
    fn format(&self, f: defmt::Formatter<'_>) {
        match self {
            Error::BufferTooSmall => defmt::write!(f, "缓冲区太小"),
            Error::Incomplete => defmt::write!(f, "响应头未收全"),
            Error::Malformed => defmt::write!(f, "响应格式非法"),
        }
    }
}

/// 往一段 `&mut [u8]` 里写文本，装不下就记一个标记（而不是 panic）。
pub struct BufWriter<'b> {
    buf: &'b mut [u8],
    len: usize,
    overflow: bool,
}

impl<'b> BufWriter<'b> {
    pub fn new(buf: &'b mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            overflow: false,
        }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        if self.overflow || self.len + bytes.len() > self.buf.len() {
            self.overflow = true;
            return;
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 全部写成功才给出结果，中途溢出就是错误 —— 半截的 HTTP 请求发出去
    /// 比不发更糟糕（服务器会一直等 body）。
    pub fn finish(self) -> Result<usize, Error> {
        if self.overflow {
            Err(Error::BufferTooSmall)
        } else {
            Ok(self.len)
        }
    }
}

impl core::fmt::Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.push_bytes(s.as_bytes());
        Ok(())
    }
}

/// 拼一个 GET 请求。
///
/// 固定发 `Connection: close`：板子上一次连接只干一件事，让服务器主动关，
/// 上层读到 EOF 就知道完了，省掉一套连接复用的状态。
pub fn get(buf: &mut [u8], url: &HttpUrl<'_>) -> Result<usize, Error> {
    let mut w = BufWriter::new(buf);
    let _ = write!(w, "GET {} HTTP/1.1\r\n", url.path);
    write_host(&mut w, url);
    let _ = write!(
        w,
        "User-Agent: {USER_AGENT}\r\n\
         Accept: */*\r\n\
         Accept-Encoding: identity\r\n\
         Connection: close\r\n\r\n"
    );
    w.finish()
}

/// 拼一个 POST（用来发 SOAP）。
pub fn post(
    buf: &mut [u8],
    url: &HttpUrl<'_>,
    content_type: &str,
    soap_action: Option<&str>,
    body: &[u8],
) -> Result<usize, Error> {
    let mut w = BufWriter::new(buf);
    let _ = write!(w, "POST {} HTTP/1.1\r\n", url.path);
    write_host(&mut w, url);
    let _ = write!(w, "Content-Type: {content_type}\r\n");
    if let Some(action) = soap_action {
        // SOAPAction 的值必须带引号，少了引号有的设备直接 500
        let _ = write!(w, "SOAPAction: \"{action}\"\r\n");
    }
    let _ = write!(
        w,
        "User-Agent: {USER_AGENT}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    w.push_bytes(body);
    w.finish()
}

fn write_host(w: &mut BufWriter<'_>, url: &HttpUrl<'_>) {
    // 端口是默认值时不写出来，个别设备对 `Host: x:80` 会认死
    let default_port = if url.tls { 443 } else { 80 };
    if url.port == default_port {
        let _ = write!(w, "Host: {}\r\n", url.host);
    } else {
        let _ = write!(w, "Host: {}:{}\r\n", url.host, url.port);
    }
}

/// 响应头里我们关心的那几样。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponseHead {
    pub status: u16,
    pub content_length: Option<usize>,
    pub chunked: bool,
    /// 头部（含结尾空行）总共占多少字节
    pub header_len: usize,
}

/// 解析响应头。头还没收全时返回 [`Error::Incomplete`]，接着收就行。
pub fn parse_head(buf: &[u8]) -> Result<ResponseHead, Error> {
    let end = find(buf, b"\r\n\r\n").ok_or(Error::Incomplete)?;
    let head = core::str::from_utf8(&buf[..end]).map_err(|_| Error::Malformed)?;
    let mut lines = head.split("\r\n");

    let status_line = lines.next().ok_or(Error::Malformed)?;
    if status_line.len() < 12 || !status_line.as_bytes()[..7].eq_ignore_ascii_case(b"HTTP/1.") {
        return Err(Error::Malformed);
    }
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .ok_or(Error::Malformed)?
        .parse()
        .map_err(|_| Error::Malformed)?;

    let mut out = ResponseHead {
        status,
        header_len: end + 4,
        ..Default::default()
    };

    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.trim().eq_ignore_ascii_case("content-length") {
            out.content_length = value.parse().ok();
        } else if name.trim().eq_ignore_ascii_case("transfer-encoding")
            && contains_ci(value, "chunked")
        {
            out.chunked = true;
        }
    }

    Ok(out)
}

/// 取响应头里某个头的值（大小写不敏感）。SSDP 那边也用得上。
pub fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
            .filter(|v| !v.is_empty())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    /// 正在读长度行
    Size,
    /// 正在读数据，还剩 n 字节
    Data(usize),
    /// 数据读完，正在吃掉结尾的 CRLF
    AfterData,
    Done,
}

/// 响应体解码器：按 `Content-Length` 或 chunked 把 body 一段段吐出来。
///
/// 用法是「喂多少解多少」：TCP 读上来的每一块原样丢进 [`feed`](Self::feed)，
/// 解出来的 body 片段通过回调交给上层。上层可以随时不管剩下的直接断开
/// （分享页那 43KB 我们只要前 6KB）。
#[derive(Debug)]
pub struct BodyDecoder {
    chunked: bool,
    /// 非 chunked 时还剩多少字节；`None` 表示「读到对方关连接为止」
    remaining: Option<usize>,
    state: ChunkState,
    /// 长度行可能被 TCP 切成两半，先攒着
    line: heapless::Vec<u8, 32>,
}

impl BodyDecoder {
    pub fn new(head: &ResponseHead) -> Self {
        Self {
            chunked: head.chunked,
            remaining: head.content_length,
            state: if head.chunked {
                ChunkState::Size
            } else {
                ChunkState::Done // 非 chunked 不用状态机
            },
            line: heapless::Vec::new(),
        }
    }

    pub fn finished(&self) -> bool {
        if self.chunked {
            self.state == ChunkState::Done
        } else {
            self.remaining == Some(0)
        }
    }

    /// 喂一段原始字节，解出来的 body 片段交给 `sink`。
    ///
    /// 返回消费了多少字节。chunked 结束后剩下的（trailer 等）不再消费。
    pub fn feed<'a>(
        &mut self,
        input: &'a [u8],
        sink: &mut impl FnMut(&'a [u8]),
    ) -> Result<usize, Error> {
        if !self.chunked {
            let take = match self.remaining {
                Some(rem) => input.len().min(rem),
                None => input.len(),
            };
            if take > 0 {
                sink(&input[..take]);
            }
            if let Some(rem) = self.remaining.as_mut() {
                *rem -= take;
            }
            return Ok(take);
        }

        let mut pos = 0;
        while pos < input.len() {
            match self.state {
                ChunkState::Done => break,
                ChunkState::Size => {
                    // 一个字节一个字节地攒长度行，直到看见 \n
                    let byte = input[pos];
                    pos += 1;
                    if byte == b'\n' {
                        let size = parse_chunk_size(&self.line)?;
                        self.line.clear();
                        self.state = if size == 0 {
                            // 收尾块。trailer 我们不关心，直接算结束
                            ChunkState::Done
                        } else {
                            ChunkState::Data(size)
                        };
                    } else if byte != b'\r' {
                        self.line.push(byte).map_err(|_| Error::Malformed)?;
                    }
                }
                ChunkState::Data(need) => {
                    let take = need.min(input.len() - pos);
                    sink(&input[pos..pos + take]);
                    pos += take;
                    self.state = if take == need {
                        ChunkState::AfterData
                    } else {
                        ChunkState::Data(need - take)
                    };
                }
                ChunkState::AfterData => {
                    // 数据块后面跟着 CRLF，吃掉
                    if input[pos] == b'\n' {
                        self.state = ChunkState::Size;
                    }
                    pos += 1;
                }
            }
        }
        Ok(pos)
    }
}

fn parse_chunk_size(line: &[u8]) -> Result<usize, Error> {
    // 长度行可能带扩展参数：`1f4a;foo=bar`
    let line = match line.iter().position(|&b| b == b';') {
        Some(i) => &line[..i],
        None => line,
    };
    let line = core::str::from_utf8(line)
        .map_err(|_| Error::Malformed)?
        .trim();
    if line.is_empty() {
        return Err(Error::Malformed);
    }
    usize::from_str_radix(line, 16).map_err(|_| Error::Malformed)
}

/// 大小写不敏感的「包含」。no_std 下没有 `to_ascii_lowercase`（那要分配内存），
/// 而 `Transfer-Encoding: Chunked` 这种写法是允许的，必须不敏感地比。
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// 在字节流里找子串。`memchr` 那种优化对我们这个数据量没意义。
pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url;

    fn collect(decoder: &mut BodyDecoder, chunks: &[&[u8]]) -> std::vec::Vec<u8> {
        let mut out = std::vec::Vec::new();
        for chunk in chunks {
            decoder
                .feed(chunk, &mut |part| out.extend_from_slice(part))
                .unwrap();
        }
        out
    }

    #[test]
    fn get_请求带上浏览器_ua() {
        // 少了这个 CDN 的 WAF 会回 418，实测过
        let u = url::parse("http://share1.kxm.xmtv.cn/xmtv/2026-09-02/abc.html").unwrap();
        let mut buf = [0u8; 512];
        let n = get(&mut buf, &u).unwrap();
        let req = core::str::from_utf8(&buf[..n]).unwrap();

        assert!(req.starts_with("GET /xmtv/2026-09-02/abc.html HTTP/1.1\r\n"));
        assert!(req.contains("Host: share1.kxm.xmtv.cn\r\n"), "{req}");
        assert!(req.contains("Mozilla/5.0"), "UA 必须像浏览器: {req}");
        assert!(req.contains("Connection: close\r\n"));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn 非默认端口才写进_host() {
        let mut buf = [0u8; 512];
        let u = url::parse("http://192.168.1.20:8200/d.xml").unwrap();
        let n = get(&mut buf, &u).unwrap();
        assert!(
            core::str::from_utf8(&buf[..n])
                .unwrap()
                .contains("Host: 192.168.1.20:8200\r\n")
        );
    }

    #[test]
    fn post_带_soapaction_和_content_length() {
        let u = url::parse("http://192.168.1.20:49152/ctl").unwrap();
        let mut buf = [0u8; 512];
        let n = post(
            &mut buf,
            &u,
            "text/xml; charset=\"utf-8\"",
            Some("urn:schemas-upnp-org:service:AVTransport:1#Play"),
            b"<body/>",
        )
        .unwrap();
        let req = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(
            req.contains("SOAPAction: \"urn:schemas-upnp-org:service:AVTransport:1#Play\"\r\n")
        );
        assert!(req.contains("Content-Length: 7\r\n"));
        assert!(req.ends_with("\r\n\r\n<body/>"));
    }

    #[test]
    fn 缓冲区装不下时报错而不是发半截请求() {
        let u = url::parse("http://192.168.1.20:8200/d.xml").unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(get(&mut buf, &u), Err(Error::BufferTooSmall));
    }

    #[test]
    fn 解析_chunked_响应头() {
        // 这就是 xmtv 接口真实回的样子（fixtures/search_headers.txt）
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n{";
        let head = parse_head(raw).unwrap();
        assert_eq!(head.status, 200);
        assert!(head.chunked);
        assert_eq!(head.content_length, None);
        assert_eq!(&raw[head.header_len..], b"{");
    }

    #[test]
    fn 解析带_content_length_的响应头() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 43872\r\nContent-Type: text/html\r\n\r\n";
        let head = parse_head(raw).unwrap();
        assert_eq!(head.content_length, Some(43872));
        assert!(!head.chunked);
    }

    #[test]
    fn 头没收全时是_incomplete_而不是错误() {
        assert_eq!(
            parse_head(b"HTTP/1.1 200 OK\r\nContent-Len"),
            Err(Error::Incomplete)
        );
    }

    #[test]
    fn 认得出_waf_的_418() {
        // 不带浏览器 UA 时 CDN 就回这个，要能识别出来而不是当成正常响应
        let raw = b"HTTP/1.1 418 \r\nTransfer-Encoding: chunked\r\nServer: CW\r\n\r\n";
        assert_eq!(parse_head(raw).unwrap().status, 418);
    }

    #[test]
    fn chunked_解码() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        let mut d = BodyDecoder::new(&head);
        let body = collect(&mut d, &[b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"]);
        assert_eq!(body, b"hello world");
        assert!(d.finished());
    }

    #[test]
    fn chunked_被_tcp_切在长度行中间也要能解() {
        // 这是最容易写错的地方：`1f\r\n` 被拆成 `1` 和 `f\r\n` 两次读上来
        let head = parse_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        let mut d = BodyDecoder::new(&head);
        // "hello world!" 是 0xc 字节，长度行 `0c\r\n` 被切成 `0` 和 `c\r\n`
        let body = collect(&mut d, &[b"0", b"c\r\nhello wor", b"ld!\r", b"\n0\r\n\r\n"]);
        assert_eq!(body, b"hello world!");
        assert!(d.finished());
    }

    #[test]
    fn chunked_长度行带扩展参数() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        let mut d = BodyDecoder::new(&head);
        assert_eq!(
            collect(&mut d, &[b"5;foo=bar\r\nhello\r\n0\r\n\r\n"]),
            b"hello"
        );
    }

    #[test]
    fn content_length_模式下多余的字节不会被吐出来() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n").unwrap();
        let mut d = BodyDecoder::new(&head);
        let mut out = std::vec::Vec::new();
        let n = d
            .feed(b"hello???", &mut |p| out.extend_from_slice(p))
            .unwrap();
        assert_eq!(out, b"hello");
        assert_eq!(n, 5, "多出来的字节不该被消费");
        assert!(d.finished());
    }

    #[test]
    fn 没有长度也没有_chunked_时读到断开为止() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
        let mut d = BodyDecoder::new(&head);
        assert_eq!(collect(&mut d, &[b"abc", b"def"]), b"abcdef");
        assert!(!d.finished(), "只能靠对方关连接来判断结束");
    }

    #[test]
    fn 长度行非法时报错() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        let mut d = BodyDecoder::new(&head);
        assert_eq!(d.feed(b"zzzz\r\n", &mut |_| {}), Err(Error::Malformed));
    }

    #[test]
    fn 用真实抓到的响应头做一遍() {
        let raw = include_bytes!("../../fixtures/search_headers.txt");
        // 抓下来的文件是纯头部，补一个空行凑成完整的头
        let mut buf = std::vec::Vec::from(&raw[..]);
        buf.extend_from_slice(b"\r\n");
        let head = parse_head(&buf).unwrap();
        assert_eq!(head.status, 200);
        assert!(head.chunked, "xmtv 接口是 chunked 的，必须认出来");
    }

    #[test]
    fn 用真实抓到的分享页响应头做一遍() {
        let raw = include_bytes!("../../fixtures/share_headers.txt");
        let mut buf = std::vec::Vec::from(&raw[..]);
        buf.extend_from_slice(b"\r\n");
        let head = parse_head(&buf).unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(43872));
        assert!(!head.chunked);
    }
}
