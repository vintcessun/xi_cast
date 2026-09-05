//! 从设备描述 XML 里挖出投屏要用的三样东西。
//!
//! 设备描述长这样（省略无关部分）：
//!
//! ```xml
//! <root xmlns="urn:schemas-upnp-org:device-1-0">
//!   <URLBase>http://192.168.1.20:8200/</URLBase>
//!   <device>
//!     <friendlyName>FastCast</friendlyName>
//!     <serviceList>
//!       <service>
//!         <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
//!         <controlURL>/ctl/AVTransport</controlURL>
//!       </service>
//!     </serviceList>
//!   </device>
//! </root>
//! ```
//!
//! 我们要的是 friendlyName（给人看的名字）、serviceType（发 SOAPAction 要原样带上）
//! 和 controlURL（往哪儿发 SOAP）。
//!
//! 三个坑，都是上位机项目踩过的：
//!
//! * **版本号不能写死成 `AVTransport:1`。** 有的渲染器只提供 `:2` / `:3`，
//!   硬匹配 `:1` 会把它们全漏掉。所以按「服务类型里含 AVTransport」来认，
//!   但发 SOAPAction 时必须用设备自己写的那个完整字符串。
//! * **controlURL 可能是相对地址**，得拿 `URLBase`（没有就拿 LOCATION）去拼。
//! * **标签可能带命名空间前缀**（`<u:friendlyName>`），比对时要忽略前缀。
//!
//! 这里不做通用 XML 解析：设备描述就是固定的几层，扫标签比拉一个解析器进来
//! 便宜太多，而且 no_std 下不用分配内存。

use heapless::String;

use crate::http::contains_ci;
use crate::url::{self, Endpoint};

pub const NAME_LEN: usize = 64;
pub const SERVICE_TYPE_LEN: usize = 64;

/// 一台可以投屏的设备。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renderer {
    /// 给人看的名字，比如 `FastCast`
    pub friendly_name: String<NAME_LEN>,
    /// 设备自己声明的服务类型，原样用在 SOAPAction 上
    pub service_type: String<SERVICE_TYPE_LEN>,
    /// 已经拼成绝对地址的控制入口
    pub control: Endpoint,
}

/// 解析设备描述。不是渲染器（没有 AVTransport 服务）就返回 `None`。
///
/// `location` 是这份 XML 的来源地址，用来解析相对的 controlURL。
pub fn parse(location: &str, xml: &str) -> Option<Renderer> {
    let location_url = url::parse(location)?;

    // URLBase 优先，它就是为「描述文件和控制入口不在一个地方」准备的
    let base_owned =
        element_text(xml, "URLBase").and_then(|t| url::parse(t.trim()).map(|u| (t, u)));
    let base = match &base_owned {
        Some((_, u)) => *u,
        None => location_url,
    };

    let friendly_name = element_text(xml, "friendlyName")
        .map(decode_entities)
        .unwrap_or_else(|| String::try_from("未命名设备").unwrap_or_default());

    // 遍历所有 <service>，挑出 AVTransport 那一个
    for service in elements(xml, "service") {
        let Some(service_type) = element_text(service, "serviceType") else {
            continue;
        };
        if !contains_ci(service_type, "AVTransport") {
            continue;
        }
        let Some(control_url) = element_text(service, "controlURL") else {
            // 声明了服务却没给控制地址，这台没法投
            continue;
        };
        let control = url::join(&base, control_url.trim())?;
        return Some(Renderer {
            friendly_name,
            service_type: String::try_from(service_type.trim()).ok()?,
            control,
        });
    }

    None
}

/// 取 `<name>...</name>` 之间的原文（第一个）。
pub fn element_text<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    elements(xml, name).next()
}

/// 依次给出所有 `<name>...</name>` 的内容。
pub fn elements<'a, 'n>(
    xml: &'a str,
    name: &'n str,
) -> impl Iterator<Item = &'a str> + use<'a, 'n> {
    let mut rest = xml;
    core::iter::from_fn(move || {
        loop {
            let open = rest.find('<')?;
            let after = &rest[open + 1..];
            // 跳过 `</`、`<?`、`<!`
            if after.starts_with(['/', '?', '!']) {
                rest = after;
                continue;
            }
            // 标签名到 `>`、空格或 `/` 为止
            let name_end = after.find(['>', ' ', '\t', '\r', '\n', '/'])?;
            let raw_name = &after[..name_end];
            // 忽略命名空间前缀：`u:friendlyName` 和 `friendlyName` 一视同仁
            let local = raw_name.rsplit(':').next().unwrap_or(raw_name);

            let Some(gt) = after.find('>') else {
                rest = after;
                continue;
            };
            // 自闭合标签没有内容
            let self_closing = after[..gt].ends_with('/');
            let content_start = gt + 1;

            if !local.eq_ignore_ascii_case(name) || self_closing {
                rest = &after[content_start.min(after.len())..];
                continue;
            }

            let body = &after[content_start..];
            // 结束标签同样可能带前缀，所以只找 `name>` 再往前核对一个 `/`
            let mut search = 0usize;
            let end = loop {
                let idx = body[search..].find("</")? + search;
                let tail = &body[idx + 2..];
                let close_end = tail.find('>')?;
                let close_name = tail[..close_end].trim();
                let close_local = close_name.rsplit(':').next().unwrap_or(close_name);
                if close_local.eq_ignore_ascii_case(name) {
                    break idx;
                }
                search = idx + 2;
            };

            let content = &body[..end];
            rest = &body[end..];
            return Some(content);
        }
    })
}

/// 还原 XML 的五个基本实体。设备名里出现 `&amp;` 的不少见。
fn decode_entities(text: &str) -> String<NAME_LEN> {
    let mut out = String::<NAME_LEN>::new();
    let mut rest = text.trim();
    while let Some(amp) = rest.find('&') {
        let _ = out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let (decoded, len) = if tail.starts_with("&amp;") {
            ("&", 5)
        } else if tail.starts_with("&lt;") {
            ("<", 4)
        } else if tail.starts_with("&gt;") {
            (">", 4)
        } else if tail.starts_with("&quot;") {
            ("\"", 6)
        } else if tail.starts_with("&apos;") {
            ("'", 6)
        } else {
            ("&", 1)
        };
        let _ = out.push_str(decoded);
        rest = &tail[len..];
    }
    let _ = out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const 描述: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>FastCast</friendlyName>
    <manufacturer>Shenzhen</manufacturer>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>/ctl/RenderingControl</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <SCPDURL>/AVTransport.xml</SCPDURL>
        <controlURL>/ctl/AVTransport</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn 挑出_avtransport_而不是第一个服务() {
        let r = parse("http://192.168.1.20:8200/rootDesc.xml", 描述).unwrap();
        assert_eq!(r.friendly_name, "FastCast");
        assert_eq!(r.service_type, "urn:schemas-upnp-org:service:AVTransport:1");
        assert_eq!(r.control.host, "192.168.1.20");
        assert_eq!(r.control.port, 8200);
        assert_eq!(r.control.path, "/ctl/AVTransport");
    }

    #[test]
    fn 认任意版本的_avtransport() {
        // 只认 `:1` 正是上位机项目里漏设备的原因
        let xml = 描述.replace("AVTransport:1", "AVTransport:3");
        let r = parse("http://192.168.1.20:8200/d.xml", &xml).unwrap();
        assert_eq!(
            r.service_type, "urn:schemas-upnp-org:service:AVTransport:3",
            "SOAPAction 必须用设备自己写的版本号"
        );
    }

    #[test]
    fn urlbase_优先于_location() {
        let xml = 描述.replace(
            "<device>",
            "<URLBase>http://10.0.0.9:49152/</URLBase>\n  <device>",
        );
        let r = parse("http://192.168.1.20:8200/d.xml", &xml).unwrap();
        assert_eq!(r.control.host, "10.0.0.9");
        assert_eq!(r.control.port, 49152);
    }

    #[test]
    fn 相对的_control_url_按描述文件的目录拼() {
        let xml = 描述.replace("/ctl/AVTransport", "ctl/AVTransport");
        let r = parse("http://192.168.1.20:8200/dev/d.xml", &xml).unwrap();
        assert_eq!(r.control.path, "/dev/ctl/AVTransport");
    }

    #[test]
    fn 绝对的_control_url() {
        let xml = 描述.replace(
            "<controlURL>/ctl/AVTransport</controlURL>",
            "<controlURL>http://10.1.1.1:5000/x</controlURL>",
        );
        let r = parse("http://192.168.1.20:8200/d.xml", &xml).unwrap();
        assert_eq!(r.control.host, "10.1.1.1");
        assert_eq!(r.control.port, 5000);
        assert_eq!(r.control.path, "/x");
    }

    #[test]
    fn 带命名空间前缀的标签也认() {
        let xml = 描述
            .replace("<friendlyName>", "<u:friendlyName>")
            .replace("</friendlyName>", "</u:friendlyName>");
        let r = parse("http://192.168.1.20:8200/d.xml", &xml).unwrap();
        assert_eq!(r.friendly_name, "FastCast");
    }

    #[test]
    fn 不是渲染器的设备返回_none() {
        // 路由器、NAS 都会回应 SSDP，但它们没有 AVTransport
        let xml = 描述.replace("AVTransport", "ConnectionManager");
        assert_eq!(parse("http://192.168.1.20:8200/d.xml", &xml), None);
    }

    #[test]
    fn 声明了服务却没给控制地址时跳过() {
        let xml = 描述.replace("<controlURL>/ctl/AVTransport</controlURL>", "");
        assert_eq!(parse("http://192.168.1.20:8200/d.xml", &xml), None);
    }

    #[test]
    fn 设备名里的实体会被还原() {
        let xml = 描述.replace("FastCast", "客厅&amp;卧室 &lt;TV&gt;");
        let r = parse("http://192.168.1.20:8200/d.xml", &xml).unwrap();
        assert_eq!(r.friendly_name, "客厅&卧室 <TV>");
    }

    #[test]
    fn 没有设备名也能用() {
        let xml = 描述.replace("<friendlyName>FastCast</friendlyName>", "");
        let r = parse("http://192.168.1.20:8200/d.xml", &xml).unwrap();
        assert_eq!(r.friendly_name, "未命名设备");
    }

    #[test]
    fn 半截的_xml_不会死循环也不会_panic() {
        // 设备描述被 TCP 截断、或者对方回了一坨 HTML，都不能把板子卡住
        for bad in [
            "",
            "<",
            "<root><device><friendlyName>没关闭",
            "<<<<>>>>",
            "not xml at all",
            "<root><service><serviceType>AVTransport</serviceType>",
        ] {
            let _ = parse("http://1.2.3.4/d.xml", bad);
        }
    }

    #[test]
    fn 自闭合标签不会被当成有内容() {
        let xml =
            "<root><friendlyName/><device><friendlyName>真名字</friendlyName></device></root>";
        assert_eq!(element_text(xml, "friendlyName"), Some("真名字"));
    }

    #[test]
    fn 能数出所有同名标签() {
        let count = elements(描述, "service").count();
        assert_eq!(count, 2);
    }
}
