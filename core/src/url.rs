//! 够用就好的 URL 拆解。
//!
//! 板子上不需要一个完整的 URL 库：真正要处理的只有三种地址
//!
//! 1. SSDP 响应里的 `LOCATION`（设备描述 XML），例如
//!    `http://192.168.1.20:8200/rootDesc.xml`；
//! 2. 设备描述里的 `controlURL`，它**可能是相对地址**（`/ctl/AVTransport`
//!    甚至 `ctl/AVTransport`），要拿 LOCATION 当基准拼出来；
//! 3. xmtv 的接口地址和分享页地址，纯 http。
//!
//! 所以这里只做这三件事，不碰 query 转义、不碰 IPv6、不碰用户名密码。

use heapless::String;

/// 主机名最长多少字节。IP 字面量最长 15 字节，域名给到 63 足够。
pub const HOST_LEN: usize = 64;
/// 路径最长多少字节。设备的 controlURL 一般都很短，给 192 很宽裕。
pub const PATH_LEN: usize = 192;

/// 一个已经拆好的 http/https 地址（借用原字符串，不复制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpUrl<'a> {
    pub host: &'a str,
    pub port: u16,
    /// 一定以 `/` 开头；原地址没写路径时是 `/`。
    pub path: &'a str,
    /// `https://` 为 true。板子自己不发 https，但要能认出来（投给电视的
    /// 视频地址就是 https，那是电视去下载，不关我们的事）。
    pub tls: bool,
}

/// 一个自带存储的地址，用在需要「拼出来再拿着用」的场合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String<HOST_LEN>,
    pub port: u16,
    pub path: String<PATH_LEN>,
    pub tls: bool,
}

impl Endpoint {
    pub fn as_url(&self) -> HttpUrl<'_> {
        HttpUrl {
            host: &self.host,
            port: self.port,
            path: &self.path,
            tls: self.tls,
        }
    }
}

impl core::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{}://{}:{}{}", scheme, self.host, self.port, self.path)
    }
}

/// 拆一个绝对地址。认不出来就返回 `None`，绝不 panic —— 输入全部来自网络。
pub fn parse(url: &str) -> Option<HttpUrl<'_>> {
    let url = url.trim();
    let (tls, rest) = match strip_prefix_ci(url, "http://") {
        Some(rest) => (false, rest),
        None => (true, strip_prefix_ci(url, "https://")?),
    };

    // 权威部分到第一个 '/'、'?' 或 '#' 为止
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let path = if path.is_empty() || !path.starts_with('/') {
        "/"
    } else {
        path
    };

    // 有 userinfo 就丢掉（`user:pass@host`）
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    if authority.is_empty() || authority.starts_with('[') {
        // 空的、或者 IPv6 字面量 —— 后者家里的电视不会用，直接不支持
        return None;
    }

    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => {
            let port: u16 = port.parse().ok()?;
            (host, port)
        }
        None => (authority, if tls { 443 } else { 80 }),
    };

    if host.is_empty() {
        return None;
    }

    Some(HttpUrl {
        host,
        port,
        path,
        tls,
    })
}

/// 把 `target` 按 `base` 解析成一个完整地址。
///
/// `target` 可以是
/// * 绝对地址 `http://host:port/path`
/// * 根相对 `/ctl/AVTransport`
/// * 纯相对 `ctl/AVTransport`（少见但标准允许，按 base 的目录拼）
///
/// 设备描述里的 `controlURL` 三种写法都见过，少支持一种就是一台电视投不了。
pub fn join(base: &HttpUrl<'_>, target: &str) -> Option<Endpoint> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }

    if let Some(abs) = parse(target) {
        return endpoint(abs.host, abs.port, abs.path, abs.tls);
    }

    if let Some(rest) = target.strip_prefix('/') {
        let mut path = String::<PATH_LEN>::new();
        path.push('/').ok()?;
        path.push_str(rest).ok()?;
        return endpoint(base.host, base.port, &path, base.tls);
    }

    // 纯相对：拿 base 路径的目录部分拼
    let dir = match base.path.rfind('/') {
        Some(i) => &base.path[..=i],
        None => "/",
    };
    let mut path = String::<PATH_LEN>::new();
    path.push_str(dir).ok()?;
    path.push_str(target).ok()?;
    endpoint(base.host, base.port, &path, base.tls)
}

fn endpoint(host: &str, port: u16, path: &str, tls: bool) -> Option<Endpoint> {
    let mut h = String::<HOST_LEN>::new();
    h.push_str(host).ok()?;
    let mut p = String::<PATH_LEN>::new();
    p.push_str(path).ok()?;
    Some(Endpoint {
        host: h,
        port,
        path: p,
        tls,
    })
}

/// `str::strip_prefix` 的大小写不敏感版本。
///
/// 需要它是因为设备回的 `LOCATION` 有写成 `HTTP://` 的（见过），
/// 大小写敏感地比一下就当成非法地址扔了。
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拆一个带端口的地址() {
        let u = parse("http://192.168.1.20:8200/rootDesc.xml").unwrap();
        assert_eq!(u.host, "192.168.1.20");
        assert_eq!(u.port, 8200);
        assert_eq!(u.path, "/rootDesc.xml");
        assert!(!u.tls);
    }

    #[test]
    fn 不写端口时按协议取默认值() {
        assert_eq!(parse("http://a.cn/x").unwrap().port, 80);
        assert_eq!(parse("https://a.cn/x").unwrap().port, 443);
    }

    #[test]
    fn 不写路径时补一个斜杠() {
        let u = parse("http://192.168.1.20:8200").unwrap();
        assert_eq!(u.path, "/");
    }

    #[test]
    fn 协议大小写不敏感() {
        // 真的见过设备回 `HTTP://`
        assert_eq!(parse("HTTP://1.2.3.4/d.xml").unwrap().host, "1.2.3.4");
    }

    #[test]
    fn 非法地址一律返回_none() {
        for bad in [
            "",
            "这不是一个地址",
            "ftp://a.cn/x",
            "http://",
            "http://a.cn:99999/x", // 端口溢出 u16
            "http://[::1]/x",      // IPv6 不支持，但不能 panic
        ] {
            assert_eq!(parse(bad), None, "{bad} 不该被解析成功");
        }
    }

    #[test]
    fn 拼绝对的_control_url() {
        let base = parse("http://192.168.1.20:8200/rootDesc.xml").unwrap();
        let e = join(&base, "http://192.168.1.20:49152/ctl").unwrap();
        assert_eq!(e.host, "192.168.1.20");
        assert_eq!(e.port, 49152);
        assert_eq!(e.path, "/ctl");
    }

    #[test]
    fn 拼根相对的_control_url() {
        let base = parse("http://192.168.1.20:8200/rootDesc.xml").unwrap();
        let e = join(&base, "/ctl/AVTransport").unwrap();
        assert_eq!(e.port, 8200, "端口要跟着设备描述那一个");
        assert_eq!(e.path, "/ctl/AVTransport");
    }

    #[test]
    fn 拼纯相对的_control_url() {
        // 相对地址要按「设备描述所在目录」拼，不能直接接在根上
        let base = parse("http://192.168.1.20:8200/dev/desc.xml").unwrap();
        let e = join(&base, "ctl/AVTransport").unwrap();
        assert_eq!(e.path, "/dev/ctl/AVTransport");
    }

    #[test]
    fn 地址过长时拼接失败而不是截断() {
        let base = parse("http://192.168.1.20:8200/d.xml").unwrap();
        let long = "/".repeat(PATH_LEN + 10);
        assert_eq!(join(&base, &long), None);
    }

    #[test]
    fn endpoint_能打印回一个完整地址() {
        let base = parse("http://192.168.1.20:8200/d.xml").unwrap();
        let e = join(&base, "/ctl").unwrap();
        let mut s = std::string::String::new();
        core::fmt::write(&mut s, format_args!("{e}")).unwrap();
        assert_eq!(s, "http://192.168.1.20:8200/ctl");
    }
}
