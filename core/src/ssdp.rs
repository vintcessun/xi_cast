//! SSDP：发 M-SEARCH、认设备的回应。
//!
//! 这一份是从上位机项目 getvideo 的 `src/discovery.rs` 搬过来的，
//! 那边踩过的坑这里一个都不能少（原项目注释里写得很清楚）：
//!
//! 1. **只发一种 ST 会漏设备。** 很多国产盒子（FastCast / 乐播 / 当贝）
//!    只回应 `ssdp:all` 或 MediaRenderer，对按服务查询的
//!    `urn:...:service:AVTransport:1` 根本不理 —— 「手机能投屏，程序却扫不到」
//!    就是这么来的。
//! 2. **一次只发一个包会丢。** UDP 组播本来就允许丢包，所以要分轮重发。
//! 3. **要顺带收 NOTIFY。** 有些设备不回应 M-SEARCH，只会自己周期广播
//!    `ssdp:alive`，被动听得到。
//! 4. **同一台设备会以多个身份出现**（回应了多种 ST、或者有线无线两个 IP），
//!    必须按 USN 里的 uuid 去重，否则一台电视会被当成好几台。
//!
//! 板子这边比上位机简单的地方：ESP32 只有一块网卡，不用枚举网卡；
//! 家里只有一台电视，扫到一台能投的就直接用。

use heapless::String;

use crate::http::header;

/// SSDP 组播地址。
pub const MULTICAST_ADDR: [u8; 4] = [239, 255, 255, 250];
pub const PORT: u16 = 1900;

/// M-SEARCH 的 MX：设备回应前的随机等待上限（秒）。
///
/// 调小可以让设备回得更快，但太小会让一堆设备同时回、互相撞掉。2 是常用值。
pub const MX: u32 = 2;

/// 依次发这几种搜索目标。顺序有讲究：最宽的排前面，先发出去先有回应。
pub const SEARCH_TARGETS: [&str; 4] = [
    // 一网打尽，国产盒子基本都认这个
    "ssdp:all",
    // 标准「媒体渲染器」设备类型
    "urn:schemas-upnp-org:device:MediaRenderer:1",
    // 按服务查（很多设备恰恰不认这个，所以它不能是唯一的一种）
    "urn:schemas-upnp-org:service:AVTransport:1",
    // 有些设备只在根设备查询时应答
    "upnp:rootdevice",
];

pub const LOCATION_LEN: usize = 128;
pub const USN_LEN: usize = 96;

/// SSDP 报文里指向的一台设备。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// 设备描述 XML 的地址（`LOCATION` 头）
    pub location: String<LOCATION_LEN>,
    /// 设备唯一标识，取 USN 里 `::` 前面那截（一般是 `uuid:xxxx`）。
    ///
    /// 用它去重：同一台电视既会因为回应了多种 ST 而重复出现，
    /// 也会因为有线 / 无线两个 IP 而以不同 location 出现。
    pub usn: String<USN_LEN>,
}

/// 拼一条 M-SEARCH 报文。
pub fn msearch(buf: &mut [u8], search_target: &str) -> Result<usize, crate::http::Error> {
    use core::fmt::Write as _;
    let mut w = crate::http::BufWriter::new(buf);
    let [a, b, c, d] = MULTICAST_ADDR;
    let _ = write!(
        w,
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {a}.{b}.{c}.{d}:{PORT}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         ST: {search_target}\r\n\
         MX: {MX}\r\n\
         USER-AGENT: esp32s3/1.0 UPnP/1.0 xi_cast/1.0\r\n\r\n"
    );
    w.finish()
}

/// 从一个 SSDP 报文里解析出设备。
///
/// 两种报文都认：M-SEARCH 的单播响应（`HTTP/1.1 200 OK`）和设备主动广播的
/// `NOTIFY`。NOTIFY 只收 alive/update —— `ssdp:byebye` 是设备下线公告，
/// 把它当成「发现了设备」会导致刚关机的电视又被选中。
pub fn parse_endpoint(text: &str) -> Option<Endpoint> {
    let first = text.lines().next()?.trim();

    if first.len() >= 8 && first.as_bytes()[..7].eq_ignore_ascii_case(b"HTTP/1.") {
        // 只要成功响应
        if first.split_whitespace().nth(1) != Some("200") {
            return None;
        }
    } else if first.len() >= 6 && first.as_bytes()[..6].eq_ignore_ascii_case(b"NOTIFY") {
        let nts = header(text, "NTS")?;
        if !nts.eq_ignore_ascii_case("ssdp:alive") && !nts.eq_ignore_ascii_case("ssdp:update") {
            return None;
        }
    } else {
        // 自己发出去又被组播回环收回来的 M-SEARCH，以及别的杂七杂八
        return None;
    }

    let location = header(text, "LOCATION")?;
    // 没给 USN 的设备（不规范但确实存在）就退回用地址当标识
    let usn = header(text, "USN")
        .and_then(|usn| usn.split("::").next())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .unwrap_or(location);

    Some(Endpoint {
        location: String::try_from(location).ok()?,
        usn: String::try_from(usn).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const 搜索响应: &str = concat!(
        "HTTP/1.1 200 OK\r\n",
        "CACHE-CONTROL: max-age=1800\r\n",
        "EXT:\r\n",
        "LOCATION: http://192.168.1.20:8200/rootDesc.xml\r\n",
        "SERVER: Linux/3.10 UPnP/1.0 FastCast/1.0\r\n",
        "ST: urn:schemas-upnp-org:service:AVTransport:1\r\n",
        "USN: uuid:abcd::urn:schemas-upnp-org:service:AVTransport:1\r\n\r\n"
    );

    #[test]
    fn 解析_msearch_响应() {
        let e = parse_endpoint(搜索响应).unwrap();
        assert_eq!(e.location, "http://192.168.1.20:8200/rootDesc.xml");
        assert_eq!(e.usn, "uuid:abcd");
    }

    #[test]
    fn 同一台设备的不同响应算同一台() {
        // 我们发四种 ST，同一台设备每种都回一次，USN 后半截不一样但 uuid 相同；
        // 设备有多个 IP 时 location 也不一样。这些都得归并成一台。
        let another = 搜索响应
            .replace(
                "USN: uuid:abcd::urn:schemas-upnp-org:service:AVTransport:1",
                "USN: uuid:abcd::upnp:rootdevice",
            )
            .replace("192.168.1.20", "10.0.0.7");
        let a = parse_endpoint(搜索响应).unwrap();
        let b = parse_endpoint(&another).unwrap();
        assert_ne!(a.location, b.location);
        assert_eq!(a.usn, b.usn, "应该被认成同一台设备");
    }

    #[test]
    fn 没有_usn_时退回用地址当标识() {
        let msg =
            "HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.40:80/d.xml\r\nST: ssdp:all\r\n\r\n";
        let e = parse_endpoint(msg).unwrap();
        assert_eq!(e.usn, e.location);
    }

    #[test]
    fn 头部大小写不敏感() {
        let msg = 搜索响应.replace("LOCATION:", "Location:");
        assert!(parse_endpoint(&msg).is_some());
    }

    #[test]
    fn 非_200_响应被丢弃() {
        let msg = 搜索响应.replace("HTTP/1.1 200 OK", "HTTP/1.1 404 Not Found");
        assert_eq!(parse_endpoint(&msg), None);
    }

    #[test]
    fn 接受_notify_alive() {
        let msg = concat!(
            "NOTIFY * HTTP/1.1\r\n",
            "HOST: 239.255.255.250:1900\r\n",
            "LOCATION: http://192.168.1.30:49152/desc.xml\r\n",
            "NT: urn:schemas-upnp-org:device:MediaRenderer:1\r\n",
            "NTS: ssdp:alive\r\n",
            "USN: uuid:ef01::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n"
        );
        let e = parse_endpoint(msg).unwrap();
        assert_eq!(e.location, "http://192.168.1.30:49152/desc.xml");
        assert_eq!(e.usn, "uuid:ef01");
    }

    #[test]
    fn 丢弃_notify_byebye() {
        // 设备下线公告不能当成「发现了设备」
        let msg = concat!(
            "NOTIFY * HTTP/1.1\r\n",
            "LOCATION: http://192.168.1.30:49152/desc.xml\r\n",
            "NTS: ssdp:byebye\r\n\r\n"
        );
        assert_eq!(parse_endpoint(msg), None);
    }

    #[test]
    fn 丢弃自己发出去的_msearch() {
        // ESP32 上组播回环默认是开的，自己会收到自己的包
        let mut buf = [0u8; 256];
        let n = msearch(&mut buf, "ssdp:all").unwrap();
        let msg = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(parse_endpoint(msg), None);
    }

    #[test]
    fn 缺少_location_头() {
        let msg = "HTTP/1.1 200 OK\r\nST: ssdp:all\r\nUSN: uuid:abcd\r\n\r\n";
        assert_eq!(parse_endpoint(msg), None);
    }

    #[test]
    fn 地址太长时不会截断成一个错地址() {
        let long = "x".repeat(LOCATION_LEN);
        let msg = std::format!("HTTP/1.1 200 OK\r\nLOCATION: http://{long}/d.xml\r\n\r\n");
        assert_eq!(parse_endpoint(&msg), None, "宁可丢掉也不能截断");
    }

    #[test]
    fn msearch_报文格式合法() {
        let mut buf = [0u8; 256];
        let n = msearch(&mut buf, "ssdp:all").unwrap();
        let msg = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(msg.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(msg.ends_with("\r\n\r\n"));
        assert!(msg.contains("MAN: \"ssdp:discover\""));
        assert!(msg.contains("HOST: 239.255.255.250:1900"));
        assert!(msg.contains("ST: ssdp:all"));
        assert!(msg.contains("MX: 2"));
    }

    #[test]
    fn 至少发一种最宽的搜索目标() {
        // 只发 AVTransport:1 正是上位机项目里扫不到国产盒子的原因
        assert!(SEARCH_TARGETS.contains(&"ssdp:all"));
        assert!(SEARCH_TARGETS.len() > 1);
    }
}
