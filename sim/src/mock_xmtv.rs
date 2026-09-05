//! 一个假的 xmtv 服务器：节目列表接口 + 分享页。
//!
//! 刻意把真服务器的几个「脾气」都复刻了出来，因为板子上踩的就是这几个坑：
//!
//! * 列表接口用 **chunked** 传输，而且故意切成很碎的块 ——
//!   长度行被 TCP 切开的情况在这里必然发生，解码器写错立刻暴露；
//! * 中文一律 `\uXXXX` 转义，`/` 写成 `\/`，和真接口一致；
//! * 分享页有几十 KB，`<source src=>` 埋在中间，逼着代码边收边扫、扫到就断；
//! * 分享页在 WAF 后面：**User-Agent 不像浏览器就回 418**。
//!   这是抓包实测出来的行为，不复刻的话真机上会莫名其妙拿不到视频地址。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 假节目库里的一条。
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: u32,
    pub publish_time: u32,
    pub title: String,
    pub date: String,
    pub slug: String,
}

#[derive(Clone)]
pub struct MockXmtv {
    pub addr: SocketAddr,
    pub entries: Arc<Vec<Entry>>,
    /// 列表接口被请求了几次（用来验证「增量更新只翻一页」）
    pub api_hits: Arc<AtomicUsize>,
    /// 分享页被请求了几次（用来验证直链缓存真的生效了）
    pub share_hits: Arc<AtomicUsize>,
}

impl MockXmtv {
    pub async fn start(series: &[(&str, u32)]) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let entries = Arc::new(build_catalog(series));
        let api_hits = Arc::new(AtomicUsize::new(0));
        let share_hits = Arc::new(AtomicUsize::new(0));

        let me = Self {
            addr,
            entries,
            api_hits,
            share_hits,
        };

        let server = me.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let server = server.clone();
                tokio::spawn(async move {
                    let _ = server.serve(&mut sock).await;
                });
            }
        });

        Ok(me)
    }

    async fn serve(&self, sock: &mut tokio::net::TcpStream) -> std::io::Result<()> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let path = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let ua = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("user-agent")
                    .then(|| v.trim())
            })
            .unwrap_or("");

        if path.starts_with("/api/open/xiamen/web_search_list.php") {
            self.api_hits.fetch_add(1, Ordering::SeqCst);
            let count = query(&path, "count").unwrap_or(20);
            let offset = query(&path, "offset").unwrap_or(0);
            let body = self.search_json(count, offset);
            write_chunked(sock, "application/json", &body).await?;
        } else if path.starts_with("/xmtv/") {
            self.share_hits.fetch_add(1, Ordering::SeqCst);
            // 真服务器就是这么拦的：不带浏览器 UA 一律 418
            if !ua.contains("Mozilla") {
                let body = "<html><title>访问被拦截！</title></html>";
                sock.write_all(
                    format!(
                        "HTTP/1.1 418 \r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
                return Ok(());
            }
            let slug = path
                .trim_end_matches(".html")
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            let known = self.entries.iter().any(|e| e.slug == slug);
            if !known {
                sock.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
                return Ok(());
            }
            let body = share_page(&slug);
            sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await?;
            sock.write_all(body.as_bytes()).await?;
        } else {
            sock.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        }
        sock.flush().await
    }

    fn search_json(&self, count: usize, offset: usize) -> String {
        let total = self.entries.len();
        let slice: Vec<&Entry> = self.entries.iter().skip(offset).take(count).collect();
        let mut out = format!("{{\"total\":{total},\"data\":[");
        for (i, e) in slice.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            // 字段顺序和真接口一致，而且故意保留了 content_id / detail_id /
            // publish_time_stamp 这些「长得像」的字段，防止取值时取错
            out.push_str(&format!(
                "{{\"id\":{id},\"site_id\":1,\"module_id\":\"vod\",\"content_id\":{cid},\
                 \"detail_id\":{did},\"type\":\"video\",\"title\":\"{title}\",\
                 \"create_time\":{pt},\"publish_time\":{pt},\"is_publish\":1,\
                 \"content_urls\":{{\"www\":\"{www}\",\"h5\":\"{h5}\",\"share\":\"{share}\"}},\
                 \"publish_time_stamp\":\"{date}\"}}",
                id = e.id,
                cid = e.id + 1,
                did = e.id + 2,
                title = escape_json(&e.title),
                pt = e.publish_time,
                www = escape_json(&format!(
                    "https://www.xmtv.cn/xmtv/{}/{}.html",
                    e.date, e.slug
                )),
                h5 = escape_json(&format!(
                    "https://h5.kxm.xmtv.cn/xmtv/{}/{}.html",
                    e.date, e.slug
                )),
                share = escape_json(&format!(
                    "https://share1.kxm.xmtv.cn/xmtv/{}/{}.html",
                    e.date, e.slug
                )),
                date = e.date,
            ));
        }
        out.push_str("]}");
        out
    }
}

/// 造一份假节目库，返回时按发布时间**从新到旧**（和真接口一致）。
///
/// 输入 `series` 也是新的在前。内部却是**从最老的一部开始**编号的，
/// 这一点很关键：测「第二天多播了两集」时，只要在 `series` 前面加一部戏，
/// 已有节目的 id、发布时间、分享地址就全都保持不变 —— 真实世界就是这样。
/// 要是按「从新往老」编号，加两条会把所有老节目的时间戳整体挪一遍，
/// 增量更新的水位判断就测不出真问题了。
fn build_catalog(series: &[(&str, u32)]) -> Vec<Entry> {
    /// 最老那一集的发布时间：2020-01-01 08:00 前后
    const 起点: u32 = 1_577_836_800;

    let mut out = Vec::new();
    let mut index = 0u32;
    for (name, episodes) in series.iter().rev() {
        for ep in 1..=*episodes {
            let publish_time = 起点 + index * 86_400;
            let month = 1 + (index / 28) % 12;
            let day = 1 + index % 28;
            out.push(Entry {
                id: 600_000 + index,
                publish_time,
                title: format!("{name}（{ep}） 斗阵来看戏 2026.{month:02}.{day:02} - 厦门卫视"),
                date: format!("2026-{month:02}-{day:02}"),
                slug: format!("{:016x}", 0xabcd_0000_0000_0000u64 + u64::from(index)),
            });
            index += 1;
        }
    }
    // 接口是按发布时间倒序给的
    out.reverse();
    out
}

/// 分享页：真页面 43KB，`<source src=>` 在第 5712 字节。
/// 这里也把它埋在几 KB 之后，好让「边收边扫、扫到就断」这条路径真的被走到。
fn share_page(slug: &str) -> String {
    let mut html =
        String::from("<!DOCTYPE html><html><head><title>斗阵来看戏</title></head><body>");
    while html.len() < 5000 {
        html.push_str("<div class=\"filler\">节目介绍占位内容，真页面里是一堆导航和推荐。</div>");
    }
    html.push_str(&format!(
        "<video controls>\
         <source src=\"https://vod1.kxm.xmtv.cn/video/2026/09/02/{slug}.mp4\" type=\"video/mp4\">\
         <source src=\"https://vod1.kxm.xmtv.cn/video/2026/09/02/{slug}.m3u8\" type=\"video/ogg\">\
         您的浏览器不支持 video 标签。</video>"
    ));
    while html.len() < 43_000 {
        html.push_str("<div class=\"tail\">页面后面还有很多东西，我们不该把它们收完。</div>");
    }
    html.push_str("</body></html>");
    html
}

/// 按真接口的样子转义：非 ASCII 全部 `\uXXXX`，`/` 写成 `\/`。
fn escape_json(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            c if c.is_ascii() => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out
}

/// 用 chunked 发出去，而且**故意切得很碎**：
/// 长度行被 TCP 分片切开是最容易写错的地方，这里保证它一定会发生。
async fn write_chunked(
    sock: &mut tokio::net::TcpStream,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    sock.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
             Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )
    .await?;

    let bytes = body.as_bytes();
    // 137 是随手挑的质数，保证块边界和 JSON 结构错开
    for part in bytes.chunks(137) {
        sock.write_all(format!("{:x}\r\n", part.len()).as_bytes())
            .await?;
        sock.write_all(part).await?;
        sock.write_all(b"\r\n").await?;
    }
    sock.write_all(b"0\r\n\r\n").await?;
    sock.flush().await
}

fn query(path: &str, key: &str) -> Option<usize> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.parse().ok())?
    })
}
