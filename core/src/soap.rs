//! 拼 AVTransport 的三个 SOAP 请求，以及读回来的播放状态。
//!
//! 投屏其实就三步：
//!
//! 1. `SetAVTransportURI` —— 告诉电视「去放这个地址」；
//! 2. `Play` —— 让它开始放；
//! 3. `GetTransportInfo` —— 隔一会儿问一次「放完了没」，放完就换下一集。
//!
//! 注意视频**不经过板子**：ESP32 只是把 URL 递过去，真正下载视频的是电视。
//! 所以视频是 https 的也无所谓，板子这边一行 TLS 代码都不用写。

use core::fmt::Write as _;

use crate::http::{BufWriter, Error};
use crate::upnp;

/// `SetAVTransportURI` 里塞的那坨元数据。
///
/// 严格说只发 `CurrentURI` 也能放，但很多设备（尤其国产盒子）拿不到
/// `CurrentURIMetaData` 就不认，或者放出来没有标题。这份 DIDL-Lite 是
/// nano-dlna 那套久经考验的模板，上位机项目一直用它。
fn write_didl(w: &mut BufWriter<'_>, url: &str, title: &str, mime: &str) {
    let _ = write!(
        w,
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\">\
<item id=\"0\" parentID=\"-1\" restricted=\"1\">\
<dc:title>"
    );
    escape_into(w, title);
    let _ = write!(w, "</dc:title><res protocolInfo=\"http-get:*:{mime}:*\">");
    escape_into(w, url);
    let _ = write!(
        w,
        "</res><upnp:class>object.item.videoItem.movie</upnp:class></item></DIDL-Lite>"
    );
}

/// 拼 `SetAVTransportURI` 的完整 SOAP 报文。
///
/// `service_type` 必须是设备自己在描述里写的那个（可能是 `:1` `:2` `:3`），
/// 用错版本号有的设备直接 401 / 500。
pub fn set_av_transport_uri(
    buf: &mut [u8],
    service_type: &str,
    url: &str,
    title: &str,
) -> Result<usize, Error> {
    let mime = mime_of(url);

    // DIDL 先拼在一个临时缓冲里，因为它整个要作为**文本**塞进
    // <CurrentURIMetaData>，得再转义一遍（`<` → `&lt;`）。
    let mut didl = [0u8; DIDL_BUF];
    let didl_len = {
        let mut dw = BufWriter::new(&mut didl);
        write_didl(&mut dw, url, title, mime);
        dw.finish()?
    };
    let didl = core::str::from_utf8(&didl[..didl_len]).map_err(|_| Error::Malformed)?;

    let mut w = BufWriter::new(buf);
    envelope_start(&mut w, service_type, "SetAVTransportURI");
    let _ = write!(w, "<InstanceID>0</InstanceID><CurrentURI>");
    escape_into(&mut w, url);
    let _ = write!(w, "</CurrentURI><CurrentURIMetaData>");
    escape_into(&mut w, didl);
    let _ = write!(w, "</CurrentURIMetaData>");
    envelope_end(&mut w, service_type, "SetAVTransportURI");
    w.finish()
}

/// 拼 `Play`。
pub fn play(buf: &mut [u8], service_type: &str) -> Result<usize, Error> {
    let mut w = BufWriter::new(buf);
    envelope_start(&mut w, service_type, "Play");
    let _ = write!(w, "<InstanceID>0</InstanceID><Speed>1</Speed>");
    envelope_end(&mut w, service_type, "Play");
    w.finish()
}

/// 拼 `Stop`。换剧目之前先停一下，免得有的设备在播放中不接受新地址。
pub fn stop(buf: &mut [u8], service_type: &str) -> Result<usize, Error> {
    let mut w = BufWriter::new(buf);
    envelope_start(&mut w, service_type, "Stop");
    let _ = write!(w, "<InstanceID>0</InstanceID>");
    envelope_end(&mut w, service_type, "Stop");
    w.finish()
}

/// 拼 `GetTransportInfo`（查播放状态）。
pub fn get_transport_info(buf: &mut [u8], service_type: &str) -> Result<usize, Error> {
    let mut w = BufWriter::new(buf);
    envelope_start(&mut w, service_type, "GetTransportInfo");
    let _ = write!(w, "<InstanceID>0</InstanceID>");
    envelope_end(&mut w, service_type, "GetTransportInfo");
    w.finish()
}

/// HTTP 头里 `SOAPAction` 的值（不含外层引号，引号由 [`crate::http::post`] 加）。
pub fn action_header(buf: &mut [u8], service_type: &str, action: &str) -> Result<usize, Error> {
    let mut w = BufWriter::new(buf);
    let _ = write!(w, "{service_type}#{action}");
    w.finish()
}

/// SOAP 的 Content-Type，所有请求都一样。
pub const CONTENT_TYPE: &str = "text/xml; charset=\"utf-8\"";

/// DIDL 临时缓冲。标题 + 地址各留了很宽的余量。
const DIDL_BUF: usize = 1024;

fn envelope_start(w: &mut BufWriter<'_>, service_type: &str, action: &str) {
    let _ = write!(
        w,
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><u:{action} xmlns:u=\"{service_type}\">"
    );
}

fn envelope_end(w: &mut BufWriter<'_>, _service_type: &str, action: &str) {
    let _ = write!(w, "</u:{action}></s:Body></s:Envelope>");
}

/// 按扩展名猜 MIME。猜错了大多数设备也能放，但写对能省掉一些设备的挑剔。
pub fn mime_of(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        e if e.eq_ignore_ascii_case("mp4") => "video/mp4",
        e if e.eq_ignore_ascii_case("m3u8") => "application/x-mpegURL",
        e if e.eq_ignore_ascii_case("mkv") => "video/x-matroska",
        e if e.eq_ignore_ascii_case("ts") => "video/mp2t",
        e if e.eq_ignore_ascii_case("avi") => "video/x-msvideo",
        // 分享页那种 .html 也可能被直接投（上位机项目就是这么干的），
        // 但按 video/mp4 报给设备，不然设备一看类型不对根本不接
        _ => "video/mp4",
    }
}

/// 往缓冲里写一段做过 XML 转义的文本。
fn escape_into(w: &mut BufWriter<'_>, text: &str) {
    for chunk in text.split_inclusive(['&', '<', '>', '"', '\'']) {
        let (body, last) = match chunk.chars().last() {
            Some(c @ ('&' | '<' | '>' | '"' | '\'')) => {
                (&chunk[..chunk.len() - c.len_utf8()], Some(c))
            }
            _ => (chunk, None),
        };
        w.push_bytes(body.as_bytes());
        match last {
            Some('&') => w.push_bytes(b"&amp;"),
            Some('<') => w.push_bytes(b"&lt;"),
            Some('>') => w.push_bytes(b"&gt;"),
            Some('"') => w.push_bytes(b"&quot;"),
            Some('\'') => w.push_bytes(b"&apos;"),
            _ => {}
        }
    }
}

/// 设备报告的播放状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    Playing,
    Transitioning,
    Paused,
    Stopped,
    NoMedia,
    /// 设备回了个没见过的状态
    Unknown,
}

impl TransportState {
    /// 这一集是不是已经放完了（该换下一集）。
    ///
    /// `TRANSITIONING` 特意**不算**停止：刚发完 Play 的头几秒设备常常处在
    /// 这个状态，算成停止的话会一秒钟跳一集，整个列表瞬间刷完。
    pub fn is_finished(self) -> bool {
        matches!(self, TransportState::Stopped | TransportState::NoMedia)
    }
}

/// 从 `GetTransportInfoResponse` 里读出状态。
pub fn parse_transport_state(xml: &str) -> Option<TransportState> {
    let state = upnp::element_text(xml, "CurrentTransportState")?.trim();
    Some(match state {
        s if s.eq_ignore_ascii_case("PLAYING") => TransportState::Playing,
        s if s.eq_ignore_ascii_case("TRANSITIONING") => TransportState::Transitioning,
        s if s.eq_ignore_ascii_case("PAUSED_PLAYBACK") || s.eq_ignore_ascii_case("PAUSED") => {
            TransportState::Paused
        }
        s if s.eq_ignore_ascii_case("STOPPED") => TransportState::Stopped,
        s if s.eq_ignore_ascii_case("NO_MEDIA_PRESENT") => TransportState::NoMedia,
        _ => TransportState::Unknown,
    })
}

/// SOAP 出错时设备会回 500 + 一个 `UPnPError`，把错误码抠出来方便排查。
pub fn parse_fault_code(xml: &str) -> Option<u16> {
    upnp::element_text(xml, "errorCode")?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const 服务: &str = "urn:schemas-upnp-org:service:AVTransport:1";

    fn 拼(f: impl FnOnce(&mut [u8]) -> Result<usize, Error>) -> std::string::String {
        let mut buf = [0u8; 4096];
        let n = f(&mut buf).unwrap();
        std::string::String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    #[test]
    fn setavtransporturi_带上地址和元数据() {
        let url = "https://vod1.kxm.xmtv.cn/video/2026/09/02/abc.mp4";
        let s = 拼(|b| set_av_transport_uri(b, 服务, url, "大名春秋"));

        assert!(s.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(s.contains(
            "<u:SetAVTransportURI xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">"
        ));
        assert!(s.contains(&std::format!("<CurrentURI>{url}</CurrentURI>")));
        // 元数据整个是被转义过的文本，所以里面看到的是 &lt; 不是 <
        assert!(s.contains("<CurrentURIMetaData>&lt;DIDL-Lite"), "{s}");
        assert!(
            s.contains("&lt;dc:title&gt;大名春秋&lt;/dc:title&gt;"),
            "{s}"
        );
        assert!(s.contains("http-get:*:video/mp4:*"));
        assert!(s.ends_with("</u:SetAVTransportURI></s:Body></s:Envelope>"));
    }

    #[test]
    fn 地址里的特殊字符会被正确转义() {
        // 这些 URL 现在没有 &，但 CDN 哪天加个 token 参数就有了，
        // 不转义的话 SOAP 直接变成非法 XML，设备一律报错
        let url = "http://h/v.mp4?a=1&b=2";
        let s = 拼(|b| set_av_transport_uri(b, 服务, url, "x"));
        assert!(
            s.contains("<CurrentURI>http://h/v.mp4?a=1&amp;b=2</CurrentURI>"),
            "{s}"
        );
        // 元数据里那份被转义了两层：& → &amp; → &amp;amp;
        assert!(s.contains("&amp;amp;b=2"), "{s}");
        assert!(!s.contains("?a=1&b=2"), "裸的 & 不能出现在 XML 里: {s}");
    }

    #[test]
    fn 标题里的尖括号也要转义() {
        let s = 拼(|b| set_av_transport_uri(b, 服务, "http://h/v.mp4", "<戏> & \"名\""));
        assert!(!s.contains("<戏>"), "标题里的裸尖括号会毁掉整个 XML: {s}");
        // 标题只出现在 DIDL 里，而 DIDL 整个又被转义了一层，所以是两层
        assert!(s.contains("&amp;lt;戏&amp;gt;"), "{s}");
        assert!(s.contains("&amp;quot;名&amp;quot;"), "{s}");
    }

    #[test]
    fn play_报文() {
        let s = 拼(|b| play(b, 服务));
        assert!(s.contains("<u:Play xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">"));
        assert!(s.contains("<InstanceID>0</InstanceID><Speed>1</Speed>"));
    }

    #[test]
    fn 服务类型版本跟着设备走() {
        // 设备声明 :3 就得发 :3，写死 :1 有的设备会拒绝
        let s = 拼(|b| play(b, "urn:schemas-upnp-org:service:AVTransport:3"));
        assert!(s.contains("xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:3\""));
    }

    #[test]
    fn soapaction_头的值() {
        let mut buf = [0u8; 128];
        let n = action_header(&mut buf, 服务, "Play").unwrap();
        assert_eq!(
            core::str::from_utf8(&buf[..n]).unwrap(),
            "urn:schemas-upnp-org:service:AVTransport:1#Play"
        );
    }

    #[test]
    fn 缓冲区太小时报错而不是发半截() {
        let mut buf = [0u8; 64];
        assert_eq!(
            set_av_transport_uri(&mut buf, 服务, "http://h/v.mp4", "x"),
            Err(Error::BufferTooSmall)
        );
    }

    #[test]
    fn 读得出播放状态() {
        let xml = r#"<?xml version="1.0"?><s:Envelope><s:Body>
            <u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
            <CurrentTransportState>PLAYING</CurrentTransportState>
            <CurrentTransportStatus>OK</CurrentTransportStatus>
            <CurrentSpeed>1</CurrentSpeed>
            </u:GetTransportInfoResponse></s:Body></s:Envelope>"#;
        assert_eq!(parse_transport_state(xml), Some(TransportState::Playing));
        assert!(!parse_transport_state(xml).unwrap().is_finished());
    }

    #[test]
    fn 停止和没有媒体都算这一集放完了() {
        for (s, want) in [
            ("STOPPED", TransportState::Stopped),
            ("NO_MEDIA_PRESENT", TransportState::NoMedia),
        ] {
            let xml = std::format!("<CurrentTransportState>{s}</CurrentTransportState>");
            let st = parse_transport_state(&xml).unwrap();
            assert_eq!(st, want);
            assert!(st.is_finished());
        }
    }

    #[test]
    fn 过渡状态不能当成放完了() {
        // 刚发完 Play 的头几秒设备常常报 TRANSITIONING，
        // 算成放完的话会一秒一集把整部戏刷完
        let xml = "<CurrentTransportState>TRANSITIONING</CurrentTransportState>";
        assert!(!parse_transport_state(xml).unwrap().is_finished());
    }

    #[test]
    fn 没见过的状态不算放完() {
        let xml = "<CurrentTransportState>WHATEVER</CurrentTransportState>";
        let st = parse_transport_state(xml).unwrap();
        assert_eq!(st, TransportState::Unknown);
        assert!(!st.is_finished(), "认不出来时宁可继续等，也别乱跳集");
    }

    #[test]
    fn 响应里没有状态字段时返回_none() {
        assert_eq!(parse_transport_state("<s:Envelope/>"), None);
    }

    #[test]
    fn 读得出_upnp_错误码() {
        let xml = r#"<s:Fault><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
            <errorCode>701</errorCode><errorDescription>Transition not available</errorDescription>
            </UPnPError></detail></s:Fault>"#;
        assert_eq!(parse_fault_code(xml), Some(701));
    }

    #[test]
    fn 按扩展名给出_mime() {
        assert_eq!(mime_of("http://h/a/b.mp4"), "video/mp4");
        assert_eq!(mime_of("http://h/a/b.m3u8"), "application/x-mpegURL");
        assert_eq!(mime_of("http://h/a/b.MP4?x=1"), "video/mp4");
        // 认不出来的一律按 mp4 报，设备至少会试着放
        assert_eq!(mime_of("http://h/a/b.html"), "video/mp4");
    }
}
