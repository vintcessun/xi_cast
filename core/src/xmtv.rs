//! 厦门卫视《斗阵来看戏》的两个数据来源，流式解析。
//!
//! ## 一、节目列表接口
//!
//! ```text
//! http://mapi1.kxm.xmtv.cn/api/open/xiamen/web_search_list.php
//!     ?count=20&offset=0&search_text=斗阵来看戏&bundle_id=livmedia
//!     &order_by=publish_time&time=0&with_count=1
//! ```
//!
//! 实测要点（`fixtures/` 里有原样抓下来的报文）：
//!
//! * **http 就能访问**，不用 TLS —— 这是整个方案能塞进 ESP32 的前提；
//! * 结果按 `publish_time` **倒序**，`offset` 分页正常，所以增量更新只要
//!   从头翻几页、翻到已经存过的那条就停，不用像上位机那样每次拉全量
//!   （全量是 2291 条 2.3MB，板子上收一遍要几十秒）；
//! * 响应是 chunked 的，正文里的中文是 `\uXXXX` 转义，`/` 写成 `\/`；
//! * 单条记录 1KB 出头，所以按「一条一条地切出来立刻处理」的方式解析，
//!   任何时候内存里只有一条，跟总条数无关。
//!
//! ## 二、分享页
//!
//! 列表里给的是 `https://share1.kxm.xmtv.cn/xmtv/<日期>/<16位id>.html`，
//! 真正的视频地址在页面里的 `<source src="...mp4">`。这一页 43KB，
//! 但那一行在第 5712 字节，扫到就可以断开，不用收完。
//!
//! 分享页在 CDN 的 WAF 后面：**不带浏览器 User-Agent 会被 418 拦掉**，
//! 见 [`crate::http::USER_AGENT`]。

use heapless::String;

use crate::http::{Error, find};

/// 列表接口的主机名（http 直连，不带 TLS）。
pub const API_HOST: &str = "mapi1.kxm.xmtv.cn";
/// 分享页主机名。2291 条记录全都是这一个，所以 flash 里只存路径不存主机。
pub const SHARE_HOST: &str = "share1.kxm.xmtv.cn";

/// 剧目名最长存多少字节。
///
/// 全量 2291 条实测：最长 66 字节（「第十四届福建省戏剧水仙花奖颁奖典礼暨汇报演出」
/// 这类晚会名），平均只有 12 字节。给到 72 是留一点余量 —— 名字被截断的后果
/// 不只是难看：分组是按名字比对的，截断会把同一部戏拆成两部。
pub const TITLE_LEN: usize = 72;

/// 一部戏最多多少集。实测最多的是《柳君拂》41 集，64 有富余。
pub const MAX_EPISODES: usize = 64;
/// 视频地址最长多少字节。
pub const VIDEO_URL_LEN: usize = 128;
/// 分享页 id 固定 16 个字符（2291 条全部如此，含 2020 年那批大小写混合的）。
pub const SLUG_LEN: usize = 16;

/// 单条节目。字段是精挑过的 —— 每多一个字节，flash 里就多 2291 个字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// 接口给的自增 id，用来判重
    pub id: u32,
    /// 发布时间（unix 秒），排序和增量更新的水位都看它
    pub publish_time: u32,
    /// 分享页路径里的日期。
    ///
    /// **不能拿 `publish_time` 换算**：2291 条里有 91 条对不上
    /// （补录的节目，路径日期和发布日期差一两天），换算出来的地址是 404。
    pub date: Date,
    /// 分享页路径里那 16 个字符
    pub slug: [u8; SLUG_LEN],
    /// 剧目名（去掉集数和栏目名之后的部分），同一部戏的每一集都一样
    pub title: String<TITLE_LEN>,
}

/// 分享页路径里的日期，压成 3 字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date {
    /// 年份减 2000
    pub year: u8,
    pub month: u8,
    pub day: u8,
}

impl core::fmt::Display for Date {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "20{:02}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl Item {
    /// 把分享页地址拼回来。
    pub fn share_path(&self) -> String<64> {
        let mut s = String::new();
        let _ = core::fmt::write(
            &mut s,
            format_args!(
                "/xmtv/{}/{}.html",
                self.date,
                core::str::from_utf8(&self.slug).unwrap_or("")
            ),
        );
        s
    }
}

/// 拼列表接口的请求路径。
///
/// `search_text` 是「斗阵来看戏」的 URL 编码，写死在这里 ——
/// 板子上没必要带一个 URL 编码器。
pub fn search_path(buf: &mut [u8], count: u32, offset: u32) -> Result<usize, Error> {
    use core::fmt::Write as _;
    let mut w = crate::http::BufWriter::new(buf);
    let _ = write!(
        w,
        "/api/open/xiamen/web_search_list.php\
         ?count={count}&offset={offset}\
         &search_text=%E6%96%97%E9%98%B5%E6%9D%A5%E7%9C%8B%E6%88%8F\
         &bundle_id=livmedia&order_by=publish_time&time=0&with_count=1"
    );
    w.finish()
}

// ---------------------------------------------------------------- 列表流式解析

/// 一条记录最多缓存多少字节。实测单条 1.1KB 左右，2KB 有余量。
const ITEM_BUF: usize = 2048;

const DATA_NEEDLE: &[u8] = b"\"data\":[";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 还没找到 `"data":[`
    Seeking,
    /// 在数组里，两条记录之间
    Between,
    /// 正在收一条记录
    InItem,
    /// 数组结束
    Done,
}

/// 把接口响应流式切成一条条记录。
///
/// 用法：TCP 读到什么就 [`feed`](Self::feed) 什么，每切出完整的一条就回调一次。
/// 内存占用是常数（一条记录的大小），跟总条数无关 —— 全量 2291 条也是这个占用。
#[derive(Debug)]
pub struct ItemStream {
    phase: Phase,
    /// `"data":[` 匹配到第几个字节（跨 TCP 分片时用）
    matched: usize,
    buf: heapless::Vec<u8, ITEM_BUF>,
    /// 当前记录已经超长了，丢掉它但要继续跟踪括号
    overflow: bool,
    depth: u32,
    in_string: bool,
    escaped: bool,
    /// 一共切出过多少条（含解析失败的）
    pub count: u32,
    /// 有多少条因为超长被丢掉
    pub dropped: u32,
}

impl Default for ItemStream {
    fn default() -> Self {
        Self::new()
    }
}

impl ItemStream {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Seeking,
            matched: 0,
            buf: heapless::Vec::new(),
            overflow: false,
            depth: 0,
            in_string: false,
            escaped: false,
            count: 0,
            dropped: 0,
        }
    }

    pub fn finished(&self) -> bool {
        self.phase == Phase::Done
    }

    /// 喂一段响应体。每完整切出一条记录就调用一次 `on_item`。
    ///
    /// 回调拿到的是这条记录的原始 JSON 文本，用 [`parse_item`] 解析。
    pub fn feed(&mut self, input: &[u8], on_item: &mut impl FnMut(&str)) {
        for &byte in input {
            match self.phase {
                Phase::Done => return,
                Phase::Seeking => {
                    // 逐字节匹配 `"data":[`，被 TCP 切开也不影响
                    if byte == DATA_NEEDLE[self.matched] {
                        self.matched += 1;
                        if self.matched == DATA_NEEDLE.len() {
                            self.phase = Phase::Between;
                        }
                    } else {
                        // 失配时要考虑「刚好是下一次匹配的开头」
                        self.matched = usize::from(byte == DATA_NEEDLE[0]);
                    }
                }
                Phase::Between => match byte {
                    b'{' => {
                        self.phase = Phase::InItem;
                        self.depth = 1;
                        self.in_string = false;
                        self.escaped = false;
                        self.overflow = false;
                        self.buf.clear();
                        let _ = self.buf.push(byte);
                    }
                    b']' => self.phase = Phase::Done,
                    _ => {}
                },
                Phase::InItem => {
                    if self.buf.push(byte).is_err() {
                        self.overflow = true;
                    }
                    self.step(byte);
                    if self.depth == 0 {
                        self.count += 1;
                        if self.overflow {
                            self.dropped += 1;
                        } else if let Ok(text) = core::str::from_utf8(&self.buf) {
                            on_item(text);
                        }
                        self.buf.clear();
                        self.phase = Phase::Between;
                    }
                }
            }
        }
    }

    /// 跟踪 JSON 的括号层级，字符串里的括号不算。
    fn step(&mut self, byte: u8) {
        if self.in_string {
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
            }
            return;
        }
        match byte {
            b'"' => self.in_string = true,
            b'{' | b'[' => self.depth += 1,
            b'}' | b']' => self.depth = self.depth.saturating_sub(1),
            _ => {}
        }
    }
}

/// 从一条记录的 JSON 文本里取出我们要的字段。
///
/// 认不出来就返回 `None`（比如接口哪天加了别的栏目），跳过这一条继续 ——
/// 上位机项目就栽在这里：一条解析不了整次更新全部失败。
pub fn parse_item(json: &str) -> Option<Item> {
    let id = number_field(json, "id")?;
    let publish_time = number_field(json, "publish_time")?;

    let mut raw_title = String::<128>::new();
    string_field(json, "title", &mut raw_title)?;
    let title = clean_title(&raw_title)?;

    let mut share = String::<128>::new();
    string_field(json, "share", &mut share)?;
    let (date, slug) = split_share_path(&share)?;

    Some(Item {
        id,
        publish_time,
        date,
        slug,
        title,
    })
}

/// 取 `"name":123` 这种数字字段。
fn number_field(json: &str, name: &str) -> Option<u32> {
    let mut needle = String::<32>::new();
    needle.push('"').ok()?;
    needle.push_str(name).ok()?;
    needle.push_str("\":").ok()?;
    let at = find(json.as_bytes(), needle.as_bytes())? + needle.len();
    let rest = &json[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// 取 `"name":"值"` 这种字符串字段，顺便把 JSON 转义还原掉。
fn string_field<const N: usize>(json: &str, name: &str, out: &mut String<N>) -> Option<()> {
    let mut needle = String::<32>::new();
    needle.push('"').ok()?;
    needle.push_str(name).ok()?;
    needle.push_str("\":\"").ok()?;
    let at = find(json.as_bytes(), needle.as_bytes())? + needle.len();
    let rest = &json[at..];

    // 找到没有被转义的那个收尾引号
    let mut end = None;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            end = Some(i);
            break;
        }
    }
    unescape(&rest[..end?], out)
}

/// 还原 JSON 字符串转义。中文全是 `\uXXXX`，路径里的 `/` 是 `\/`。
fn unescape<const N: usize>(src: &str, out: &mut String<N>) -> Option<()> {
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c).ok()?;
            continue;
        }
        match chars.next()? {
            'u' => {
                let hi = hex4(&mut chars)?;
                let ch = if (0xD800..0xDC00).contains(&hi) {
                    // 代理对：高位后面必须紧跟 \uDCxx
                    if chars.next()? != '\\' || chars.next()? != 'u' {
                        return None;
                    }
                    let lo = hex4(&mut chars)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return None;
                    }
                    let cp = 0x1_0000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
                    char::from_u32(cp)?
                } else {
                    char::from_u32(u32::from(hi))?
                };
                out.push(ch).ok()?;
            }
            'n' => out.push('\n').ok()?,
            'r' => out.push('\r').ok()?,
            't' => out.push('\t').ok()?,
            'b' | 'f' => {}
            other => out.push(other).ok()?, // \" \\ \/ 都走这里
        }
    }
    Some(())
}

fn hex4(chars: &mut core::str::Chars<'_>) -> Option<u16> {
    let mut v: u16 = 0;
    for _ in 0..4 {
        v = v.checked_mul(16)? + u16::try_from(chars.next()?.to_digit(16)?).ok()?;
    }
    Some(v)
}

/// 栏目名，节目标题里靠它把剧目名和日期分开。
const PROGRAM: &str = "斗阵来看戏";

/// 从完整标题里取出剧目名。
///
/// 标题格式：`剧目名（集数） 斗阵来看戏 2026.09.02 - 厦门卫视`，
/// 但有 57 条没有集数括号，还有的剧目名本身就带《斗阵来看戏》字样。
///
/// 所以：
/// * 用 **`rfind`** 找栏目名 —— 用 `find` 的话，
///   「《斗阵来看戏》栏目曾小真歌仔戏剧团签约仪式」会被切成一个「《」；
/// * 集数括号全角半角都要认，没有括号就整段都是剧目名。
pub fn clean_title(full: &str) -> Option<String<TITLE_LEN>> {
    let head = match full.rfind(PROGRAM) {
        Some(at) => &full[..at],
        None => full,
    };
    let head = match head.find(['（', '(']) {
        Some(at) => &head[..at],
        None => head,
    };
    let head = head.trim();
    if head.is_empty() {
        return None;
    }

    let mut out = String::<TITLE_LEN>::new();
    for c in head.chars() {
        if c == ' ' || c == '\u{3000}' {
            continue; // 标题里的空格位置很随意，去掉才好按名字分组
        }
        if out.push(c).is_err() {
            break; // 超长就截断，剧目名只是给人看的
        }
    }
    (!out.is_empty()).then_some(out)
}

/// 拆分享页地址，只留下日期和 16 位 id。
///
/// 要求形如 `.../xmtv/2026-09-02/b2071f1bd886c153.html`。
/// 2291 条全部符合，不符合的直接跳过（宁可少放一集，也不要往 flash 里
/// 写一条拼不出地址的记录）。
pub fn split_share_path(url: &str) -> Option<(Date, [u8; SLUG_LEN])> {
    let at = url.find("/xmtv/")? + "/xmtv/".len();
    let rest = &url[at..];
    let (date, file) = rest.split_once('/')?;
    let slug = file.strip_suffix(".html")?;

    if date.len() != 10 || slug.len() != SLUG_LEN || !slug.is_ascii() {
        return None;
    }
    let year: u16 = date.get(0..4)?.parse().ok()?;
    let month: u8 = date.get(5..7)?.parse().ok()?;
    let day: u8 = date.get(8..10)?.parse().ok()?;
    if !(2000..2256).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut bytes = [0u8; SLUG_LEN];
    bytes.copy_from_slice(slug.as_bytes());
    Some((
        Date {
            year: (year - 2000) as u8,
            month,
            day,
        },
        bytes,
    ))
}

// ---------------------------------------------------------------- 分享页扫描

const SOURCE_NEEDLE: &[u8] = b"<source src=\"";

/// 边收边找分享页里的 `<source src="...mp4">`。
///
/// 页面 43KB，目标在第 5712 字节；扫到就可以立刻断开连接，
/// 剩下 37KB 不用收 —— 板子上这点流量和时间都值钱。
///
/// 页面里有两个 source（mp4 和 m3u8），只要 mp4：DLNA 设备对 m3u8 的支持
/// 参差不齐，mp4 是最保险的。
#[derive(Debug, Default)]
pub struct SourceScanner {
    matched: usize,
    capturing: bool,
    current: String<VIDEO_URL_LEN>,
    found: Option<String<VIDEO_URL_LEN>>,
}

impl SourceScanner {
    pub const fn new() -> Self {
        Self {
            matched: 0,
            capturing: false,
            current: String::new(),
            found: None,
        }
    }

    /// 喂一段页面内容，返回是否已经找到（找到就可以断开了）。
    pub fn feed(&mut self, input: &[u8]) -> bool {
        for &byte in input {
            if self.found.is_some() {
                return true;
            }
            if self.capturing {
                if byte == b'"' {
                    self.capturing = false;
                    if self.current.ends_with(".mp4") {
                        self.found = Some(core::mem::take(&mut self.current));
                        return true;
                    }
                    // 是 m3u8 那一条，继续往下找
                    self.current.clear();
                } else if self.current.push(byte as char).is_err() || !byte.is_ascii() {
                    // 地址超长或者出现非 ASCII，肯定不是我们要的
                    self.capturing = false;
                    self.current.clear();
                }
                continue;
            }
            if byte == SOURCE_NEEDLE[self.matched] {
                self.matched += 1;
                if self.matched == SOURCE_NEEDLE.len() {
                    self.matched = 0;
                    self.capturing = true;
                    self.current.clear();
                }
            } else {
                self.matched = usize::from(byte == SOURCE_NEEDLE[0]);
            }
        }
        self.found.is_some()
    }

    pub fn url(&self) -> Option<&str> {
        self.found.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 抓下来的真实响应（20 条，chunked 解码后的正文）
    const 真实响应: &str = include_str!("../../fixtures/search_20.json");
    const 分享页: &str = include_str!("../../fixtures/share_page.html");

    fn 解析全部(body: &str, chunk: usize) -> std::vec::Vec<Item> {
        let mut stream = ItemStream::new();
        let mut items = std::vec::Vec::new();
        for part in body.as_bytes().chunks(chunk) {
            stream.feed(part, &mut |text| {
                if let Some(item) = parse_item(text) {
                    items.push(item);
                }
            });
        }
        items
    }

    #[test]
    fn 从真实响应里解出_20_条() {
        let items = 解析全部(真实响应, 真实响应.len());
        assert_eq!(items.len(), 20, "抓的这一页就是 20 条");

        let first = &items[0];
        assert_eq!(first.id, 670419);
        assert_eq!(first.publish_time, 1788336662);
        assert_eq!(first.title, "大名春秋");
        assert_eq!(first.date.to_string(), "2026-09-02");
        assert_eq!(
            core::str::from_utf8(&first.slug).unwrap(),
            "b2071f1bd886c153"
        );
        assert_eq!(first.share_path(), "/xmtv/2026-09-02/b2071f1bd886c153.html");
    }

    #[test]
    fn 无论_tcp_怎么切都解得一样() {
        // 板子上一次 read 拿到多少字节完全看运气，1 字节一喂也必须对
        let 完整 = 解析全部(真实响应, 真实响应.len());
        for size in [1, 2, 7, 64, 512, 1500] {
            let 分片 = 解析全部(真实响应, size);
            assert_eq!(分片, 完整, "按 {size} 字节切之后结果就不一样了");
        }
    }

    #[test]
    fn 结果按发布时间倒序() {
        // 增量更新依赖这个顺序：翻到已经存过的那条就能停
        let items = 解析全部(真实响应, 4096);
        for w in items.windows(2) {
            assert!(
                w[0].publish_time > w[1].publish_time,
                "{} 应该比 {} 新",
                w[0].title,
                w[1].title
            );
        }
    }

    #[test]
    fn 第二页接得上第一页() {
        let p1 = 解析全部(真实响应, 4096);
        let p2 = 解析全部(include_str!("../../fixtures/search_offset20.json"), 4096);
        assert_eq!(p2.len(), 20);
        assert!(
            p1.last().unwrap().publish_time > p2[0].publish_time,
            "offset=20 拿到的必须比第一页更旧，否则分页是错的"
        );
        let 重复 = p1.iter().any(|a| p2.iter().any(|b| a.id == b.id));
        assert!(!重复, "两页之间不该有重复记录");
    }

    #[test]
    fn 同一部戏的各集剧目名一致() {
        // 分组就靠这个，名字不一致会把一部戏拆成好几部。
        // 抓下来这一页里《凤箫怨》有连着好几集（含两集都标「（2）」的），
        // 它们的剧目名必须一模一样
        let items = 解析全部(真实响应, 4096);
        let 凤箫怨: std::vec::Vec<_> = items.iter().filter(|i| i.title == "凤箫怨").collect();
        assert!(
            凤箫怨.len() >= 3,
            "这一页里应该有好几集凤箫怨，实际 {}",
            凤箫怨.len()
        );
    }

    #[test]
    fn 老记录的_id_是大小写混合的() {
        // 2020 年那批不是 16 位十六进制，是随机大小写字母数字，
        // 按十六进制解码存会直接把这 192 条弄丢
        let items = 解析全部(include_str!("../../fixtures/search_oldest.json"), 4096);
        assert!(!items.is_empty());
        let 混合 = items
            .iter()
            .any(|i| i.slug.iter().any(|b| b.is_ascii_uppercase()));
        assert!(混合, "老记录里有大写字母，说明必须原样按字符串存");
    }

    #[test]
    fn 标题里带栏目名的那种不会被切坏() {
        // 用 find 会把它切成一个「《」
        let t = clean_title(
            "《斗阵来看戏》栏目曾小真歌仔戏剧团签约仪式 斗阵来看戏 2026.05.28 - 厦门卫视",
        );
        assert_eq!(t.unwrap(), "《斗阵来看戏》栏目曾小真歌仔戏剧团签约仪式");
    }

    #[test]
    fn 没有集数括号的标题() {
        assert_eq!(
            clean_title("皇帝告状 斗阵来看戏 2026.05.01 - 厦门卫视").unwrap(),
            "皇帝告状"
        );
    }

    #[test]
    fn 全角半角括号都认() {
        assert_eq!(
            clean_title("花倾蝶（14） 斗阵来看戏 2025.08.24").unwrap(),
            "花倾蝶"
        );
        assert_eq!(
            clean_title("花倾蝶(14) 斗阵来看戏 2025.08.24").unwrap(),
            "花倾蝶"
        );
    }

    #[test]
    fn 标题里的空格会被去掉() {
        // 「碧海青天（3）斗阵来看戏」和「碧海青天（3） 斗阵来看戏」都出现过，
        // 不统一的话同一部戏会被分成两组
        assert_eq!(
            clean_title("碧 海 青 天（3） 斗阵来看戏").unwrap(),
            "碧海青天"
        );
    }

    #[test]
    fn 剧目名超长时截断而不是丢弃() {
        let long = "长".repeat(100);
        let t = clean_title(&std::format!("{long}（1） 斗阵来看戏")).unwrap();
        assert!(t.len() <= TITLE_LEN);
        assert!(!t.is_empty());
    }

    #[test]
    fn 空标题解析失败() {
        assert_eq!(clean_title("  "), None);
        assert_eq!(clean_title("斗阵来看戏 2026.09.02"), None);
    }

    #[test]
    fn 拆分享页地址() {
        let (date, slug) =
            split_share_path("https://share1.kxm.xmtv.cn/xmtv/2026-09-02/b2071f1bd886c153.html")
                .unwrap();
        assert_eq!(date.to_string(), "2026-09-02");
        assert_eq!(core::str::from_utf8(&slug).unwrap(), "b2071f1bd886c153");
    }

    #[test]
    fn 形状不对的分享页地址一律拒绝() {
        for bad in [
            "https://share1.kxm.xmtv.cn/xmtv/2026-09-02/short.html",
            "https://share1.kxm.xmtv.cn/xmtv/2026-9-2/b2071f1bd886c153.html",
            "https://share1.kxm.xmtv.cn/other/2026-09-02/b2071f1bd886c153.html",
            "https://share1.kxm.xmtv.cn/xmtv/2026-09-02/b2071f1bd886c153.php",
            "https://share1.kxm.xmtv.cn/xmtv/2026-13-02/b2071f1bd886c153.html",
            "",
        ] {
            assert_eq!(split_share_path(bad), None, "{bad} 不该通过");
        }
    }

    #[test]
    fn 解码_json_里的中文和斜杠() {
        let mut out = String::<64>::new();
        unescape("\\u5927\\u540d\\u6625\\u79cb", &mut out).unwrap();
        assert_eq!(out, "大名春秋");

        let mut out = String::<64>::new();
        unescape("https:\\/\\/a.cn\\/b", &mut out).unwrap();
        assert_eq!(out, "https://a.cn/b");
    }

    #[test]
    fn 解码代理对() {
        let mut out = String::<16>::new();
        unescape("\\ud83c\\udfad", &mut out).unwrap();
        assert_eq!(out, "🎭");
    }

    #[test]
    fn 半截的响应不会_panic() {
        // WiFi 断了、服务器提前关连接，都会喂进来半条记录
        for cut in [0, 10, 100, 500, 1000, 5000] {
            let 部分 = &真实响应[..cut.min(真实响应.len())];
            let _ = 解析全部(部分, 64);
        }
    }

    #[test]
    fn 超长的记录被丢掉但不影响后面的() {
        let mut body = std::string::String::from("{\"data\":[{\"junk\":\"");
        body.push_str(&"x".repeat(ITEM_BUF * 2));
        body.push_str("\"},{\"id\":7,\"publish_time\":8,\"title\":\"甲（1） 斗阵来看戏\",\"content_urls\":{\"share\":\"https://share1.kxm.xmtv.cn/xmtv/2026-01-02/abcdefabcdef0123.html\"}}]}");

        let mut stream = ItemStream::new();
        let mut items = std::vec::Vec::new();
        stream.feed(body.as_bytes(), &mut |t| {
            if let Some(i) = parse_item(t) {
                items.push(i);
            }
        });
        assert_eq!(stream.dropped, 1, "超长那条应该被记一笔");
        assert_eq!(items.len(), 1, "后面那条要正常解出来");
        assert_eq!(items[0].id, 7);
    }

    #[test]
    fn 缺字段的记录跳过而不是整批失败() {
        // 上位机项目就栽在这儿：一条解析不了，整次更新全部失败
        let body = r#"{"data":[{"id":1,"title":"没有分享地址 斗阵来看戏"},
            {"id":2,"publish_time":3,"title":"乙（1） 斗阵来看戏","content_urls":{"share":"https://share1.kxm.xmtv.cn/xmtv/2026-01-02/abcdefabcdef0123.html"}}]}"#;
        let items = 解析全部(body, 16);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 2);
    }

    #[test]
    fn 字段名不会被别的字段带偏() {
        // content_id / detail_id / publish_time_stamp 都长得很像
        let json = r#"{"id":670419,"site_id":1,"content_id":999,"detail_id":888,
            "publish_time":1788336662,"publish_time_stamp":"2026-09-02"}"#;
        assert_eq!(number_field(json, "id"), Some(670419));
        assert_eq!(number_field(json, "publish_time"), Some(1788336662));
    }

    #[test]
    fn 请求路径拼得对() {
        let mut buf = [0u8; 256];
        let n = search_path(&mut buf, 20, 40).unwrap();
        let p = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(p.starts_with("/api/open/xiamen/web_search_list.php?count=20&offset=40&"));
        assert!(p.contains("order_by=publish_time"));
    }

    #[test]
    fn 从真实分享页里扫出_mp4_地址() {
        let mut s = SourceScanner::new();
        assert!(s.feed(分享页.as_bytes()));
        assert_eq!(
            s.url().unwrap(),
            "https://vod1.kxm.xmtv.cn/video/2026/09/02/847562566bae2b3a690c85f6033f7d75.mp4"
        );
    }

    #[test]
    fn 扫到就能提前收工() {
        // 43KB 的页面，目标在 5712 字节处；只喂前 8KB 就该找到
        let mut s = SourceScanner::new();
        assert!(s.feed(&分享页.as_bytes()[..8192]), "前 8KB 里就应该扫到");
    }

    #[test]
    fn 分片喂也能扫到() {
        for size in [1, 3, 64, 1500] {
            let mut s = SourceScanner::new();
            let mut ok = false;
            for part in 分享页.as_bytes().chunks(size) {
                if s.feed(part) {
                    ok = true;
                    break;
                }
            }
            assert!(ok, "按 {size} 字节喂就扫不到了");
        }
    }

    #[test]
    fn 只要_mp4_不要_m3u8() {
        let html =
            r#"<source src="http://h/a.m3u8" type="x"><source src="http://h/a.mp4" type="y">"#;
        let mut s = SourceScanner::new();
        assert!(s.feed(html.as_bytes()));
        assert_eq!(s.url().unwrap(), "http://h/a.mp4");
    }

    #[test]
    fn 页面里没有视频地址时不会误报() {
        let mut s = SourceScanner::new();
        assert!(!s.feed("<html>被拦截了</html>".as_bytes()));
        assert_eq!(s.url(), None);
    }
}
