//! 整机逻辑：开机 → 找电视 → 更新节目 → 一集接一集地投。
//!
//! 这里没有一行代码知道自己跑在 ESP32 上还是电脑上 —— 网络走
//! [`crate::net::Net`]，存储走 [`embedded_storage`] 的标准 trait。
//! 所以 `sim/tests/` 里那条端到端测试跑的就是上板要跑的这一份。
//!
//! # 开机流程
//!
//! ```text
//!   上电
//!    │
//!    ├─ 打开 flash 目录（扫一遍日志，几十毫秒）
//!    │    └─ 有节目吗？
//!    │         ├─ 有 → 直接往下走，先投上再说
//!    │         └─ 没有（第一次开机）→ 先拉几页节目
//!    │
//!    ├─ 找电视
//!    │    ├─ 上次记着的那台还在吗？（一次 HTTP，几十毫秒）
//!    │    └─ 不在 → SSDP 扫描
//!    │
//!    └─ 循环：随机挑一部戏 → 逐集投 → 播完换下一集
//!         └─ 空隙里顺手做增量更新 / 往回补历史节目
//! ```
//!
//! 「记住上次那台电视」是开机速度的关键：SSDP 扫描要好几秒，
//! 而直接问一台已知地址的设备只要几十毫秒。地址变了（换了 DHCP 租约）
//! 就自动退回扫描，不用人管。

use heapless::{String, Vec};

use crate::net::{self, Net};
use crate::soap::{self, TransportState};
use crate::store::{Catalog, Episode, Series};
use crate::upnp::{self, Renderer};
use crate::url;
use crate::xmtv::{self, Item, VIDEO_URL_LEN};
use crate::{trace_debug, trace_info, trace_warn};

/// 一次拉多少条。20 条约 20KB，正好是一次 HTTP 的舒适区间。
pub const PAGE: usize = 20;

/// 设备描述 XML 最多收多少。见过最大的 6KB 左右。
const DESC_BUF: usize = 8192;
/// SOAP 响应最多收多少。
const SOAP_BUF: usize = 2048;
/// SSDP 单个报文最多多少字节。
const SSDP_BUF: usize = 1500;
/// 一轮扫描里最多记多少台候选设备。
const MAX_CANDIDATES: usize = 8;

/// 指定了 IP 但它不应答单播搜索时，依次试这几个地址。
///
/// 都是各家 DLNA 设备实际用过的端口和路径。49152 排最前面是因为它是动态端口段
/// 的第一个，绝大多数盒子和电视用的就是它 —— 家里这台 HappyCast 电视实测
/// 就是 `49152/description.xml`。
const WELL_KNOWN_DESC: [(u16, &str); 8] = [
    (49152, "/description.xml"),
    (49152, "/dmr.xml"),
    (49152, "/MediaRenderer.xml"),
    (49153, "/description.xml"),
    (8200, "/rootDesc.xml"),
    (9197, "/dmr"),
    (80, "/description.xml"),
    (8080, "/description.xml"),
];

/// 把 `192.168.0.100` 拆成四个字节。不合法就返回 `None`。
fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut count = 0usize;
    for part in text.trim().split('.') {
        if count == 4 {
            return None;
        }
        out[count] = part.parse().ok()?;
        count += 1;
    }
    (count == 4).then_some(out)
}

#[derive(Debug, Clone)]
pub struct Config {
    /// 有多台设备时优先选名字里带这个词的。
    ///
    /// 默认沿用上位机项目的 `FastCast`。家里只有一台投屏设备的话
    /// 这个字段基本用不上 —— 扫到唯一一台能投的就直接用了。
    pub preferred_name: &'static str,
    /// 每种搜索目标发几轮（对抗组播丢包）。
    pub search_rounds: u32,
    /// 一轮 SSDP 等多少毫秒响应。
    pub scan_ms: u32,
    /// 没找到设备时隔多久重试。
    pub retry_ms: u32,
    /// 播放中多久查一次状态。
    ///
    /// 上位机项目最早是空转查询，一秒几十上百次，便宜盒子直接被问死。
    /// 真正的控制端一般 1~2 秒一次。
    pub poll_ms: u32,
    /// 第一次开机最多拉几页就开播（先播上，剩下的慢慢补）。
    pub first_pages: u32,
    /// 增量更新最多翻几页（正常一天就一条，翻一页就够）。
    pub sync_pages: u32,
    /// 只认这个 IP 上的设备，**绝不广播扫描**。
    ///
    /// 电视的 IP 在路由器里绑死之后就该用这个。和写死完整地址
    /// （[`Self::fixed_device`]）比，这里只要一个 IP —— 端口和描述文件路径
    /// 由板子自己去问那台设备，设备固件升级换了端口也不用跟着改。
    ///
    /// 「不扫描」这件事本身就是目的：组播是不认门牌号的，扫描扫到的可能是
    /// 邻居家的盒子，戏就投到别人家去了。只问指定 IP 就不可能串。
    pub fixed_ip: Option<&'static str>,
    /// 写死一台设备的描述地址，不再扫描。
    ///
    /// 电视的 IP 在路由器里绑死之后就该用这个：省掉开机那几秒 SSDP，
    /// 而且**永远不会投错设备** —— 扫描是有可能扫到邻居家的盒子的
    /// （组播不认门牌号），写死了就没这个风险。
    ///
    /// 设了它之后连不上也不会退回扫描，只会一直重试这一个地址：
    /// 既然说好了是固定的，那连不上就是电视没开机，等着就行。
    pub fixed_device: Option<&'static str>,
    /// 直接投分享页地址，不去解析 mp4 直链。
    ///
    /// 上位机项目就是这么干的。留这个开关是因为**没有开发板也没有电视**
    /// 时无法确定家里那台盒子认哪一种；万一它只认网页地址，
    /// 改一行配置就能切回去。
    pub cast_share_page: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            preferred_name: "FastCast",
            search_rounds: 3,
            scan_ms: 2500,
            retry_ms: 3000,
            poll_ms: 2000,
            first_pages: 5,
            sync_pages: 3,
            fixed_ip: None,
            fixed_device: None,
            cast_share_page: false,
        }
    }
}

/// 整机。
pub struct App<N, F> {
    pub net: N,
    pub catalog: Catalog<F>,
    pub cfg: Config,
    /// 一页节目的暂存区。放在结构体里而不是栈上 —— 板子上任务栈很紧
    page: Vec<Item, PAGE>,
    /// 补历史时额外要跳过多少条（见 [`Self::backfill_step`]）。
    /// 只存在内存里：重启之后从头算一遍就好，不值得占 flash。
    backfill_extra: u32,
}

impl<N, F> App<N, F>
where
    N: Net,
    F: embedded_storage::nor_flash::NorFlash + embedded_storage::nor_flash::ReadNorFlash,
{
    pub fn new(net: N, catalog: Catalog<F>, cfg: Config) -> Self {
        Self {
            net,
            catalog,
            cfg,
            page: Vec::new(),
            backfill_extra: 0,
        }
    }

    // ------------------------------------------------------------ 找电视

    /// 找一台能投屏的设备。
    ///
    /// 四条路，从「最不会出错」到「最省事」：
    /// 1. 指定了 IP（[`Config::fixed_ip`]）→ **只问那一台**，永不广播；
    /// 2. 写死了完整地址（[`Config::fixed_device`]）→ 只认它；
    /// 3. flash 里记着上次那台 → 先试它（几十毫秒）；
    /// 4. 都没有 → SSDP 扫描（几秒）。
    pub async fn find_renderer(&mut self) -> Option<Renderer> {
        if let Some(ip) = self.cfg.fixed_ip {
            return self.probe_fixed_ip(ip).await;
        }

        if let Some(fixed) = self.cfg.fixed_device {
            trace_info!("用写死的设备地址: {}", fixed);
            let found = self.fetch_renderer(fixed).await;
            if found.is_none() {
                // 不退回扫描：地址是人指定的，连不上就是电视没开机
                trace_warn!("写死的那台连不上（电视没开机？），不会去扫别的设备");
            }
            return found;
        }

        if let Some(device) = self.catalog.summary().device.clone() {
            trace_info!("先试上次那台设备: {}", device.location.as_str());
            if let Some(r) = self.fetch_renderer(&device.location).await {
                trace_info!("上次那台还在: {}", r.friendly_name.as_str());
                return Some(r);
            }
            trace_warn!("上次那台连不上了（多半是换了 IP），改扫描");
        }

        let found = self.scan().await?;
        // 记下来，下次开机就不用扫了
        if let Err(e) = self.catalog.remember_device(&found.usn, &found.location) {
            trace_warn!("记住设备失败: {}", e.as_str());
        }
        Some(found.renderer)
    }

    /// 只问指定 IP 上的那一台设备，全程不发组播。
    ///
    /// 两步：
    ///
    /// 1. **单播 M-SEARCH** —— 直接问它「你的设备描述在哪」。这是标准做法，
    ///    好处是设备换了端口也照样问得到；
    /// 2. 它不理单播（有些设备只应答组播）就退而求其次，试几个厂商常用的
    ///    描述地址。
    ///
    /// 两步都只跟这一个 IP 打交道，收到的响应还会再核对一遍
    /// 「LOCATION 里的主机是不是就是它」—— 别人抢答也不算数。
    async fn probe_fixed_ip(&mut self, ip: &str) -> Option<Renderer> {
        let Some(octets) = parse_ipv4(ip) else {
            trace_warn!("配置里的电视 IP 不合法: {}", ip);
            return None;
        };

        // ① 单播 M-SEARCH
        for st in crate::ssdp::SEARCH_TARGETS {
            let mut buf = [0u8; 256];
            if let Ok(n) = crate::ssdp::msearch(&mut buf, st)
                && self.net.ssdp_send(&buf[..n], Some(octets)).await.is_err()
            {
                trace_warn!("单播 M-SEARCH 发送失败（网卡还没就绪？）");
            }
            self.net.sleep_ms(20).await;
        }

        let mut buf = [0u8; SSDP_BUF];
        let mut location: Option<String<{ crate::ssdp::LOCATION_LEN }>> = None;
        while location.is_none() {
            match self.net.ssdp_recv(&mut buf, self.cfg.scan_ms).await {
                Ok(Some(n)) => {
                    let Ok(text) = core::str::from_utf8(&buf[..n]) else {
                        continue;
                    };
                    let Some(endpoint) = crate::ssdp::parse_endpoint(text) else {
                        continue;
                    };
                    // 核对：回应里的地址必须就是我们问的那台
                    if url::parse(&endpoint.location).is_some_and(|u| u.host == ip) {
                        trace_info!("{} 回应了: {}", ip, endpoint.location.as_str());
                        location = Some(endpoint.location);
                    } else {
                        trace_debug!("忽略别人的抢答: {}", endpoint.location.as_str());
                    }
                }
                // 等够了它也没回，走第二步
                Ok(None) => break,
                Err(_) => break,
            }
        }

        if let Some(location) = location
            && let Some(renderer) = self.fetch_renderer(&location).await
        {
            trace_info!("找到电视: {}", renderer.friendly_name.as_str());
            return Some(renderer);
        }

        // ② 试几个常见的描述地址
        trace_info!("{} 没应答单播搜索，改试几个常见的设备描述地址", ip);
        for (port, path) in WELL_KNOWN_DESC {
            let mut url_buf = String::<128>::new();
            if core::fmt::write(&mut url_buf, format_args!("http://{ip}:{port}{path}")).is_err() {
                continue;
            }
            trace_debug!("试 {}", url_buf.as_str());
            if let Some(renderer) = self.fetch_renderer(&url_buf).await {
                trace_info!(
                    "找到电视: {} @ {}",
                    renderer.friendly_name.as_str(),
                    url_buf.as_str()
                );
                return Some(renderer);
            }
        }

        trace_warn!(
            "{} 上没找到能投屏的设备。电视开机了吗？IP 是不是这一个？",
            ip
        );
        None
    }

    /// 扫一轮 SSDP，返回第一台能投屏的设备。
    async fn scan(&mut self) -> Option<Found> {
        let mut candidates: Vec<crate::ssdp::Endpoint, MAX_CANDIDATES> = Vec::new();

        for round in 0..self.cfg.search_rounds {
            // 四种搜索目标全发一遍：不同厂商认的不一样，只发一种会漏设备
            for st in crate::ssdp::SEARCH_TARGETS {
                let mut buf = [0u8; 256];
                if let Ok(n) = crate::ssdp::msearch(&mut buf, st)
                    && self.net.ssdp_send(&buf[..n], None).await.is_err()
                {
                    trace_warn!("SSDP 发送失败（网卡还没就绪？）");
                }
                // 岔开一点，别让设备一次收到四个包直接丢
                self.net.sleep_ms(20).await;
            }

            // 收响应
            let mut buf = [0u8; SSDP_BUF];
            loop {
                match self.net.ssdp_recv(&mut buf, self.cfg.scan_ms).await {
                    Ok(Some(n)) => {
                        let Ok(text) = core::str::from_utf8(&buf[..n]) else {
                            continue;
                        };
                        let Some(endpoint) = crate::ssdp::parse_endpoint(text) else {
                            continue;
                        };
                        // 同一台设备会回好几次（我们发了四种 ST），按 USN 去重
                        if candidates.iter().any(|c| c.usn == endpoint.usn) {
                            continue;
                        }
                        trace_debug!("发现 {}", endpoint.location.as_str());
                        let _ = candidates.push(endpoint);
                    }
                    Ok(None) => break, // 这一轮等够了
                    Err(_) => {
                        trace_warn!("SSDP 接收出错，这一轮就到这儿");
                        break;
                    }
                }
            }

            if !candidates.is_empty() {
                trace_info!("第 {} 轮扫到 {} 台设备", round + 1, candidates.len());
                break;
            }
        }

        if candidates.is_empty() {
            trace_warn!(
                "没扫到任何 UPnP 设备。检查：板子和电视在同一个 WiFi 吗？路由器开了 AP 隔离吗？"
            );
            return None;
        }

        // 逐个抓设备描述，挑出能投屏的
        let mut fallback: Option<Found> = None;
        for endpoint in &candidates {
            let Some(renderer) = self.fetch_renderer(&endpoint.location).await else {
                continue;
            };
            let 名字对上了 =
                crate::http::contains_ci(&renderer.friendly_name, self.cfg.preferred_name);
            let found = Found {
                usn: endpoint.usn.clone(),
                location: endpoint.location.clone(),
                renderer,
            };
            if 名字对上了 {
                trace_info!("按名字选中: {}", found.renderer.friendly_name.as_str());
                return Some(found);
            }
            // 家里就一台投屏设备，扫到能投的就用它 —— 这是需求里明确说的
            if fallback.is_none() {
                fallback = Some(found);
            }
        }

        if let Some(f) = &fallback {
            trace_info!("用扫到的这台: {}", f.renderer.friendly_name.as_str());
        } else {
            trace_warn!("扫到了设备，但没有一台支持 AVTransport（都不是能投屏的）");
        }
        fallback
    }

    /// 抓设备描述并认出 AVTransport 服务。
    async fn fetch_renderer(&mut self, location: &str) -> Option<Renderer> {
        let parsed = url::parse(location)?;
        let mut conn = self.net.connect(parsed.host, parsed.port).await.ok()?;

        let mut xml = [0u8; DESC_BUF];
        let mut len = 0usize;
        let status = net::get(&mut conn, &parsed, &mut |part| {
            let take = part.len().min(xml.len() - len);
            xml[len..len + take].copy_from_slice(&part[..take]);
            len += take;
            len < xml.len()
        })
        .await
        .ok()?;
        drop(conn);

        if !(200..300).contains(&status) {
            trace_warn!("设备描述返回 HTTP {}", status);
            return None;
        }
        upnp::parse(location, core::str::from_utf8(&xml[..len]).ok()?)
    }

    // ------------------------------------------------------------ 更新节目

    /// 增量更新：从最新的一页往下翻，翻到已经存过的那条就停。
    ///
    /// 正常情况下一天新增一条，也就是**翻一页、写一条**。
    /// 和上位机那种「每次全量 2291 条重写一遍」比，流量和 flash 磨损差着两个数量级。
    pub async fn sync_new(&mut self) -> Result<u32, net::Error> {
        let watermark = self.catalog.summary().newest;
        let mut added = 0u32;
        let max_pages = if watermark == 0 {
            self.cfg.first_pages
        } else {
            self.cfg.sync_pages
        };

        for page in 0..max_pages {
            let offset = page * PAGE as u32;
            self.fetch_page(offset).await?;

            let mut 到头了 = false;
            // 先收进内存，连接关掉之后再写 flash：写 flash 会短暂关掉指令缓存，
            // 不要和正在收包的 TCP 连接搅在一起
            let items = core::mem::take(&mut self.page);
            for item in &items {
                if watermark != 0 && item.publish_time <= watermark {
                    到头了 = true;
                    break;
                }
                match self.catalog.append_item(item) {
                    Ok(()) => added += 1,
                    Err(e) => {
                        trace_warn!("写入节目失败: {}", e.as_str());
                        return Ok(added);
                    }
                }
            }
            let empty = items.is_empty();
            self.page = items;
            self.page.clear();

            if 到头了 || empty {
                break;
            }
        }

        if added > 0 {
            trace_info!(
                "新增 {} 条节目，现在一共 {} 条",
                added,
                self.catalog.summary().items
            );
        }
        Ok(added)
    }

    /// 往回补一页历史节目。
    ///
    /// 偏移量基本上就是「已经存了多少条」——接口是按发布时间倒序给的，
    /// 所以第 n 条之后就是我们还没有的那些。这样即使期间新增了节目
    /// 导致偏移漂移，也会自动对齐，不用把进度写进 flash。
    ///
    /// 但有一种情况会卡住：某一页里的节目全都解析不了（接口新加了别的栏目、
    /// 标题格式变了），那么「已存条数」就永远追不上真实位置，
    /// 同一页会被反复拉。所以额外记一个只存在内存里的跳过量，
    /// 遇到「整整一页一条都没收」就往前跨一页。
    pub async fn backfill_step(&mut self) -> Result<u32, net::Error> {
        if self.catalog.summary().backfilled {
            return Ok(0);
        }
        let oldest = self.catalog.summary().oldest;
        let offset = self.catalog.summary().items + self.backfill_extra;
        self.fetch_page(offset).await?;

        let items = core::mem::take(&mut self.page);
        let mut added = 0u32;
        for item in &items {
            // 只收比手上最老的还老的，避免和已有的重复
            if oldest != 0 && item.publish_time >= oldest {
                continue;
            }
            if self.catalog.append_item(item).is_ok() {
                added += 1;
            }
        }
        let 这页有多少 = items.len();
        self.page = items;
        self.page.clear();

        if 这页有多少 < PAGE {
            // 接口给的比要的少，说明翻到最后一页了
            trace_info!("历史节目补齐，一共 {} 条", self.catalog.summary().items);
            let _ = self.catalog.mark_backfilled();
        } else if added == 0 {
            // 满满一页却一条都没收下：这一页全是我们已经有的（或者全都解析失败）。
            // 注意**不能**就此认为补完了 —— 中间有一段解析不了的节目时，
            // 「已存条数」会永远落后于真实位置，判成补完会让后面几百条再也拉不到
            self.backfill_extra += PAGE as u32;
            trace_debug!("补历史：offset={} 这一页没有新东西，往前跨一页", offset);
        } else {
            trace_debug!("补历史：offset={} 新增 {} 条", offset, added);
        }
        Ok(added)
    }

    /// 拉一页节目到 [`Self::page`]。
    async fn fetch_page(&mut self, offset: u32) -> Result<(), net::Error> {
        let mut path_buf = [0u8; 256];
        let n = xmtv::search_path(&mut path_buf, PAGE as u32, offset)
            .map_err(|_| net::Error::TooLarge)?;
        let path = core::str::from_utf8(&path_buf[..n]).map_err(|_| net::Error::Protocol)?;
        let target = url::HttpUrl {
            host: xmtv::API_HOST,
            port: 80,
            path,
            tls: false,
        };

        let Self {
            net: netif, page, ..
        } = self;
        page.clear();

        let mut conn = netif
            .connect(xmtv::API_HOST, 80)
            .await
            .map_err(|_| net::Error::Connect)?;
        let mut stream = xmtv::ItemStream::new();
        let status = net::get(&mut conn, &target, &mut |part| {
            stream.feed(part, &mut |text| {
                if let Some(item) = xmtv::parse_item(text) {
                    let _ = page.push(item);
                }
            });
            !stream.finished()
        })
        .await?;

        if !(200..300).contains(&status) {
            return Err(net::Error::Status(status));
        }
        Ok(())
    }

    /// 把一集的分享页解析成视频直链，顺手缓存进 flash。
    pub async fn resolve(&mut self, episode: &Episode) -> Option<String<VIDEO_URL_LEN>> {
        if let Ok(Some(cached)) = self.catalog.resolved_url(episode.id) {
            trace_debug!("直链命中缓存");
            return Some(cached);
        }

        let path = episode.share_path();
        let target = url::HttpUrl {
            host: xmtv::SHARE_HOST,
            port: 80,
            path: &path,
            tls: false,
        };
        let mut conn = self.net.connect(xmtv::SHARE_HOST, 80).await.ok()?;

        let mut scanner = xmtv::SourceScanner::new();
        // 找到就返回 false 断开：整页 43KB，要的东西在第 5712 字节
        let status = net::get(&mut conn, &target, &mut |part| !scanner.feed(part))
            .await
            .ok()?;
        drop(conn);

        if status == 418 {
            // CDN 的 WAF 拦的。User-Agent 不像浏览器就是这个下场
            trace_warn!("分享页被 WAF 拦了（HTTP 418）");
            return None;
        }
        let url = scanner.url()?;
        let out = String::try_from(url).ok()?;
        if let Err(e) = self.catalog.append_resolved(episode.id, &out) {
            trace_debug!("直链没缓存下来（不影响播放）: {}", e.as_str());
        }
        Some(out)
    }

    // ------------------------------------------------------------ 投屏

    /// 投一个地址过去并让它开始播。
    pub async fn cast(&mut self, r: &Renderer, video: &str, title: &str) -> Result<(), net::Error> {
        let target = r.control.as_url();

        let mut body = [0u8; 4096];
        let n = soap::set_av_transport_uri(&mut body, &r.service_type, video, title)
            .map_err(|_| net::Error::TooLarge)?;
        self.soap(&target, &r.service_type, "SetAVTransportURI", &body[..n])
            .await?;

        let n = soap::play(&mut body, &r.service_type).map_err(|_| net::Error::TooLarge)?;
        self.soap(&target, &r.service_type, "Play", &body[..n])
            .await?;
        Ok(())
    }

    /// 问一句「放到哪儿了」。
    pub async fn transport_state(&mut self, r: &Renderer) -> Result<TransportState, net::Error> {
        let target = r.control.as_url();
        let mut body = [0u8; 512];
        let n = soap::get_transport_info(&mut body, &r.service_type)
            .map_err(|_| net::Error::TooLarge)?;

        let mut out = [0u8; SOAP_BUF];
        let Self { net: netif, .. } = self;
        let mut conn = netif
            .connect(target.host, target.port)
            .await
            .map_err(|_| net::Error::Connect)?;
        let (status, response) = net::soap_call(
            &mut conn,
            &target,
            &r.service_type,
            "GetTransportInfo",
            &body[..n],
            &mut out,
        )
        .await?;
        drop(conn);

        let text = core::str::from_utf8(response).map_err(|_| net::Error::Protocol)?;
        if !(200..300).contains(&status) {
            return Err(net::Error::Status(status));
        }
        soap::parse_transport_state(text).ok_or(net::Error::Protocol)
    }

    async fn soap(
        &mut self,
        target: &url::HttpUrl<'_>,
        service_type: &str,
        action: &str,
        body: &[u8],
    ) -> Result<(), net::Error> {
        let mut out = [0u8; SOAP_BUF];
        let mut conn = self
            .net
            .connect(target.host, target.port)
            .await
            .map_err(|_| net::Error::Connect)?;
        let (status, response) =
            net::soap_call(&mut conn, target, service_type, action, body, &mut out).await?;

        if !(200..300).contains(&status) {
            if let Ok(text) = core::str::from_utf8(response)
                && let Some(code) = soap::parse_fault_code(text)
            {
                trace_warn!("{} 被设备拒绝：UPnP 错误 {}", action, code);
            }
            return Err(net::Error::Status(status));
        }
        Ok(())
    }

    // ------------------------------------------------------------ 主循环

    /// 挑一部戏。
    pub fn pick(&mut self) -> Option<Series> {
        let seed = self.net.random();
        match self.catalog.pick_series(seed) {
            Ok(series) => series,
            Err(e) => {
                trace_warn!("挑戏时读 flash 出错: {}", e.as_str());
                None
            }
        }
    }

    /// 播完一集要么是设备说停了，要么是问不动了。
    ///
    /// 返回 `None` 表示设备失联（该重新找设备了）；
    /// `Some(true)` 表示这一集真的播过（中途见到过 PLAYING）；
    /// `Some(false)` 表示设备从头到尾没进入播放状态 —— 多半是它下载不了那个地址。
    async fn wait_until_finished(&mut self, r: &Renderer) -> Option<bool> {
        // 刚发完 Play，设备常常还在 TRANSITIONING 甚至 STOPPED（还没开始加载），
        // 所以给一小段宽限期，别一上来就以为播完了
        let mut 宽限 = 5u32;
        let mut 连续失败 = 0u32;
        let mut 真的播过 = false;

        loop {
            self.net.sleep_ms(self.cfg.poll_ms).await;

            match self.transport_state(r).await {
                Ok(state) => {
                    连续失败 = 0;
                    if state == TransportState::Playing {
                        真的播过 = true;
                    }
                    if state.is_finished() {
                        if 宽限 > 0 {
                            宽限 -= 1;
                            continue;
                        }
                        return Some(真的播过);
                    }
                    宽限 = 0;
                }
                Err(e) => {
                    连续失败 += 1;
                    trace_warn!("查状态失败（第 {} 次）: {}", 连续失败, e.as_str());
                    // 电视被关掉、拔网线、换 IP，都长这样
                    if 连续失败 >= 5 {
                        return None;
                    }
                }
            }

            // 播放的空隙里顺手把历史节目往回补
            if !self.catalog.summary().backfilled {
                let _ = self.backfill_step().await;
            }
        }
    }

    /// 一上电就一直放下去，除非断电。
    ///
    /// 这个函数不返回：设备找不到就重试，网络断了就重连，
    /// 节目更新失败就用手上的缓存接着播 —— 一台放在电视机旁边的盒子，
    /// 任何一步失败都不该让它停下来。
    pub async fn run(&mut self) -> ! {
        // 第一次开机（flash 里什么都没有）必须先拉一点节目下来
        if self.catalog.summary().items == 0 {
            trace_info!("flash 里还没有节目，先拉几页");
            while self.catalog.summary().items == 0 {
                if let Err(e) = self.sync_new().await {
                    trace_warn!(
                        "拉节目失败，{} 毫秒后重试: {}",
                        self.cfg.retry_ms,
                        e.as_str()
                    );
                    self.net.sleep_ms(self.cfg.retry_ms).await;
                }
            }
        } else {
            trace_info!(
                "flash 里已经有 {} 条节目，直接开播",
                self.catalog.summary().items
            );
        }

        loop {
            let Some(renderer) = self.find_renderer().await else {
                trace_warn!("没找到电视，{} 毫秒后再找", self.cfg.retry_ms);
                self.net.sleep_ms(self.cfg.retry_ms).await;
                continue;
            };

            // 有电视了，顺手更新一下节目（失败不影响播放）
            if let Err(e) = self.sync_new().await {
                trace_warn!("更新节目失败，先用手上的缓存播: {}", e.as_str());
            }

            if !self.play_forever(&renderer).await {
                trace_warn!("和电视失联了，重新找一遍");
            }
        }
    }

    /// 一部接一部地放，直到和设备失联。返回 `false` 表示设备没了。
    async fn play_forever(&mut self, r: &Renderer) -> bool {
        /// 连着这么多集都投不出去，就当设备没了，回去重新找。
        ///
        /// 少了这个判断会出大问题：电视被关掉之后每次投屏都失败，
        /// 而失败只是「跳下一集」，于是程序会以最快速度空转着把整部戏
        /// 挨个投一遍、再挑一部接着投 —— 既问死了设备也问死了自己，
        /// 而且永远不会回去重新扫描。
        const 连续失败上限: u32 = 3;
        let mut 连续失败 = 0u32;
        // 连着几集「投出去了但设备根本没播起来」，就提示一次
        let mut 没真播过 = 0u32;

        loop {
            let Some(series) = self.pick() else {
                trace_warn!("挑不出戏（目录是空的？）");
                self.net.sleep_ms(self.cfg.retry_ms).await;
                return true;
            };
            trace_info!(
                "这部放《{}》，一共 {} 集",
                series.title.as_str(),
                series.episodes.len()
            );

            for (index, episode) in series.episodes.iter().enumerate() {
                let video = if self.cfg.cast_share_page {
                    // 兜底：直接把分享页地址丢给设备（上位机项目就是这么干的）
                    let mut u = String::<VIDEO_URL_LEN>::new();
                    let _ = u.push_str("http://");
                    let _ = u.push_str(xmtv::SHARE_HOST);
                    let _ = u.push_str(&episode.share_path());
                    Some(u)
                } else {
                    self.resolve(episode).await
                };

                let Some(video) = video else {
                    // 分享页打不开（xmtv 挂了 / 被 WAF 拦了 / 断网）。
                    // 这里必须歇一下：不歇的话会以最快速度把整部戏的每一集
                    // 都试一遍，把自己和对方都拖垮
                    trace_warn!("第 {} 集拿不到播放地址，跳过", index + 1);
                    self.net.sleep_ms(self.cfg.retry_ms).await;
                    continue;
                };

                trace_info!("投第 {} 集: {}", index + 1, video.as_str());
                if let Err(e) = self.cast(r, &video, &series.title).await {
                    连续失败 += 1;
                    trace_warn!("投屏失败（连续第 {} 次）: {}", 连续失败, e.as_str());
                    if 连续失败 >= 连续失败上限 {
                        return false; // 多半是电视关了，回去重新找
                    }
                    // 偶尔一次可能只是设备忙，歇一下再试下一集
                    self.net.sleep_ms(self.cfg.retry_ms).await;
                    continue;
                }
                连续失败 = 0;

                match self.wait_until_finished(r).await {
                    None => return false,
                    Some(true) => 没真播过 = 0,
                    Some(false) => {
                        // 设备收下了地址、也没报错，但从头到尾没进入 PLAYING。
                        // 最可能的原因是它下载不了那个 https 的 mp4
                        // （便宜盒子不少不支持 TLS）。这条日志是没有开发板、
                        // 没法在现场盯着看时，唯一能判断出这件事的线索
                        没真播过 += 1;
                        if 没真播过 >= 3 {
                            trace_warn!(
                                "连着 {} 集都没真正播起来：设备收下了地址却没开始播。\
                                 多半是它下载不了 https 的视频 —— 试试把配置里的 \
                                 cast_share_page 打开，改投分享页地址",
                                没真播过
                            );
                            没真播过 = 0;
                        }
                    }
                }
            }
        }
    }
}

/// 扫描的结果：设备本体 + 它的 SSDP 身份（记进 flash 用）。
struct Found {
    usn: String<{ crate::ssdp::USN_LEN }>,
    location: String<{ crate::ssdp::LOCATION_LEN }>,
    renderer: Renderer,
}
