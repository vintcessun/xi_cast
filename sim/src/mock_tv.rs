//! 一台假电视：会回应 SSDP 搜索，也会真的处理 SOAP 投屏请求。
//!
//! 家里那台电视没法拿来跑自动化测试（要开机、要有人看着、还不能重复），
//! 所以照着 DLNA 渲染器的行为写了一台。上位机项目里也有同样的东西
//! （`examples/mock_renderer.rs`），这里按板子这边的需要重写了一份。
//!
//! 它认真做了这几件事，因为这几件事最容易出错：
//!
//! * SSDP 响应里带 `LOCATION` 和 `USN`，格式和真设备一致；
//! * 设备描述里放两个服务（RenderingControl 和 AVTransport），
//!   逼着代码去挑对的那一个；
//! * SOAP 请求会检查 `SOAPAction` 头，不对就返回 500 —— 真设备就是这样；
//! * 播放状态会按「查几次之后就报播完」变化，用来验证自动切集。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

/// SSDP 组播组和端口，和核心库里用的是同一组常量。
const SSDP_GROUP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(
    xi_cast_core::ssdp::MULTICAST_ADDR[0],
    xi_cast_core::ssdp::MULTICAST_ADDR[1],
    xi_cast_core::ssdp::MULTICAST_ADDR[2],
    xi_cast_core::ssdp::MULTICAST_ADDR[3],
);
const SSDP_PORT: u16 = xi_cast_core::ssdp::PORT;
/// 局域网模式下 HTTP 监听的端口。固定下来，设备描述地址才能写进板子的配置。
pub const LAN_HTTP_PORT: u16 = 8200;

pub const FRIENDLY_NAME: &str = "FastCast 客厅电视";
pub const USN: &str = "uuid:4d696e69-444c-164e-9d41-b8b3c9a1f001";
pub const SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";

#[derive(Debug, Default)]
pub struct TvState {
    /// 最后一次被投的地址
    pub current_uri: Option<String>,
    /// 被投过的所有地址，按顺序 —— 用来验证「一集接一集、顺序不乱」
    pub uris: Vec<String>,
    /// 从元数据里解出来的标题
    pub current_title: Option<String>,
    /// Play 被调了几次
    pub play_count: usize,
    /// 收到过哪些 SOAPAction（按顺序）
    pub actions: Vec<String>,
    /// 这一集还能被查几次「在播」，减到 0 就报播完
    polls_left: usize,
    /// 每次 Play 之后重置成多少
    polls_per_episode: usize,
    /// 设为 true 之后所有 SOAP 一律返回 500，模拟电视被关掉
    pub offline: bool,
}

#[derive(Clone)]
pub struct MockTv {
    pub location: String,
    pub ssdp_addr: SocketAddr,
    pub http_addr: SocketAddr,
    state: Arc<Mutex<TvState>>,
}

impl MockTv {
    /// 起一台假电视。`polls_per_episode` = 每投一集之后，
    /// 被查几次播放状态就报「播完了」。
    ///
    /// 只在回环上监听，SSDP 也只收单播 —— 给自动化测试用的，
    /// 不依赖组播，不会被防火墙拦，CI 上也稳。
    pub async fn start(polls_per_episode: usize) -> std::io::Result<Self> {
        let http = TcpListener::bind("127.0.0.1:0").await?;
        let udp = UdpSocket::bind("127.0.0.1:0").await?;
        Self::spawn(polls_per_episode, http, udp, None).await
    }

    /// 起一台假电视，HTTP 绑在**指定端口**上（SSDP 仍然只收单播）。
    ///
    /// 用来测「只知道 IP、不知道端口」那条路：把端口设成 49152 这种常见值，
    /// 代码在单播搜索没人应答之后，应该能靠试常见地址找到它。
    pub async fn start_on_port(polls_per_episode: usize, port: u16) -> std::io::Result<Self> {
        let http = TcpListener::bind(("127.0.0.1", port)).await?;
        let udp = UdpSocket::bind("127.0.0.1:0").await?;
        Self::spawn(polls_per_episode, http, udp, None).await
    }

    /// 起一台**真的能被局域网里其它设备找到**的假电视。
    ///
    /// 这是给「板子发、电脑收」那种联调用的：
    ///
    /// * HTTP 绑 `0.0.0.0`，但对外公布的地址用 `iface` —— 板子要能访问到；
    /// * SSDP 绑 `0.0.0.0:1900` 并加入组播组，这样板子发出来的 M-SEARCH
    ///   才收得到。
    ///
    /// 1900 端口在 Windows 上被系统的 SSDPSRV 服务占着，所以必须
    /// `SO_REUSEADDR` 和它共享；共享之后组播包两边都收得到，互不影响。
    pub async fn start_lan(
        polls_per_episode: usize,
        iface: std::net::Ipv4Addr,
    ) -> std::io::Result<Self> {
        use socket2::{Domain, Protocol, SockAddr, Socket, Type};

        // 固定端口，这样设备描述地址每次都一样，可以直接写进板子的
        // `tv_url` 配置里。8200 是 DLNA 服务端的常用端口，被占了就随机挑一个
        let http = match TcpListener::bind(("0.0.0.0", LAN_HTTP_PORT)).await {
            Ok(listener) => listener,
            Err(_) => TcpListener::bind(("0.0.0.0", 0)).await?,
        };

        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        // 必须在 bind 之前设
        sock.set_reuse_address(true)?;
        sock.bind(&SockAddr::from(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::UNSPECIFIED,
            SSDP_PORT,
        )))?;
        // 在指定网卡上加入组播组。本机有好几块虚拟网卡（VMware 之类），
        // 不指定的话系统会挑一块，多半挑错
        sock.join_multicast_v4(&SSDP_GROUP, &iface)?;
        sock.set_multicast_loop_v4(true)?;
        sock.set_nonblocking(true)?;
        let udp = UdpSocket::from_std(sock.into())?;

        Self::spawn(polls_per_episode, http, udp, Some(iface)).await
    }

    async fn spawn(
        polls_per_episode: usize,
        http: TcpListener,
        udp: UdpSocket,
        advertise: Option<std::net::Ipv4Addr>,
    ) -> std::io::Result<Self> {
        let http_addr = http.local_addr()?;
        let ssdp_addr = udp.local_addr()?;
        // 对外公布的地址：局域网模式下要用真实网卡 IP，别人才连得上
        let location = match advertise {
            Some(ip) => format!("http://{ip}:{}/desc.xml", http_addr.port()),
            None => format!("http://{http_addr}/desc.xml"),
        };

        let state = Arc::new(Mutex::new(TvState {
            polls_per_episode,
            ..Default::default()
        }));

        // HTTP：设备描述 + SOAP 控制
        {
            let state = Arc::clone(&state);
            let location = location.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = http.accept().await else {
                        break;
                    };
                    let state = Arc::clone(&state);
                    let location = location.clone();
                    tokio::spawn(async move {
                        let _ = serve_http(&mut sock, &state, &location).await;
                    });
                }
            });
        }

        // SSDP：回应 M-SEARCH
        {
            let location = location.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                loop {
                    let Ok((n, from)) = udp.recv_from(&mut buf).await else {
                        break;
                    };
                    let text = String::from_utf8_lossy(&buf[..n]);
                    if !text.starts_with("M-SEARCH") {
                        continue;
                    }
                    // 真设备对每个 ST 都回一次；我们的代码要按 USN 把它们归并成一台
                    let st = text
                        .lines()
                        .find_map(|l| l.strip_prefix("ST: "))
                        .unwrap_or("ssdp:all")
                        .trim()
                        .to_string();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\n\
                         CACHE-CONTROL: max-age=1800\r\n\
                         EXT:\r\n\
                         LOCATION: {location}\r\n\
                         SERVER: Linux/3.10 UPnP/1.0 FastCast/1.0\r\n\
                         ST: {st}\r\n\
                         USN: {USN}::{st}\r\n\r\n"
                    );
                    let _ = udp.send_to(reply.as_bytes(), from).await;
                }
            });
        }

        Ok(Self {
            location,
            ssdp_addr,
            http_addr,
            state,
        })
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, TvState> {
        self.state.lock().unwrap()
    }

    pub fn current_uri(&self) -> Option<String> {
        self.state().current_uri.clone()
    }

    pub fn play_count(&self) -> usize {
        self.state().play_count
    }

    /// 模拟拔电视电源。
    pub fn go_offline(&self) {
        self.state().offline = true;
    }
}

const DESC_XML: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>__NAME__</friendlyName>
    <manufacturer>Mock</manufacturer>
    <UDN>__USN__</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
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

async fn serve_http(
    sock: &mut tokio::net::TcpStream,
    state: &Arc<Mutex<TvState>>,
    _location: &str,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];

    // 收到头部结束为止
    let head_end = loop {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at + 4;
        }
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);

    while buf.len() < head_end + content_length {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();

    // 真设备的描述文件路径各家不一样（/description.xml、/desc.xml、/dmr.xml…），
    // 这里几个都认，好让「只知道 IP，去试常见地址」那条路能被真正测到
    let response = if head.starts_with("GET /desc.xml")
        || head.starts_with("GET /description.xml")
        || head.starts_with("GET /dmr.xml")
    {
        let xml = DESC_XML
            .replace("__NAME__", FRIENDLY_NAME)
            .replace("__USN__", USN);
        http_ok("text/xml", &xml)
    } else if head.starts_with("POST /ctl/AVTransport") {
        handle_soap(&head, &body, state)
    } else {
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
    };

    sock.write_all(response.as_bytes()).await?;
    sock.flush().await?;
    Ok(())
}

fn handle_soap(head: &str, body: &str, state: &Arc<Mutex<TvState>>) -> String {
    // 真设备靠这个头分派动作，缺了或者不带引号都会出问题
    let Some(action) = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("soapaction")
            .then(|| v.trim().trim_matches('"').to_string())
    }) else {
        return soap_fault(401, "缺少 SOAPAction 头");
    };
    let name = action.rsplit('#').next().unwrap_or("").to_string();

    let mut st = state.lock().unwrap();
    st.actions.push(name.clone());
    if st.offline {
        return soap_fault(501, "设备已关机");
    }

    match name.as_str() {
        "SetAVTransportURI" => {
            let Some(uri) = element(body, "CurrentURI") else {
                return soap_fault(402, "没有 CurrentURI");
            };
            let meta = element(body, "CurrentURIMetaData").unwrap_or_default();
            // 元数据是被转义过的一整段 DIDL，还原之后才能读出标题
            let didl = unescape_xml(&meta);
            st.current_title = element(&didl, "dc:title").map(|t| unescape_xml(&t));
            let uri = unescape_xml(&uri);
            st.uris.push(uri.clone());
            st.current_uri = Some(uri);
            st.polls_left = st.polls_per_episode;
            soap_ok("SetAVTransportURI", "")
        }
        "Play" => {
            if st.current_uri.is_none() {
                // 没设地址就 Play，真设备会报 701
                return soap_fault(701, "还没有媒体");
            }
            st.play_count += 1;
            st.polls_left = st.polls_per_episode;
            soap_ok("Play", "")
        }
        "Stop" => {
            st.polls_left = 0;
            soap_ok("Stop", "")
        }
        "GetTransportInfo" => {
            let transport = if st.current_uri.is_none() {
                "NO_MEDIA_PRESENT"
            } else if st.polls_left > 0 {
                st.polls_left -= 1;
                "PLAYING"
            } else {
                "STOPPED"
            };
            soap_ok(
                "GetTransportInfo",
                &format!(
                    "<CurrentTransportState>{transport}</CurrentTransportState>\
                     <CurrentTransportStatus>OK</CurrentTransportStatus>\
                     <CurrentSpeed>1</CurrentSpeed>"
                ),
            )
        }
        _ => soap_fault(401, "不支持的动作"),
    }
}

fn soap_ok(action: &str, inner: &str) -> String {
    let body = format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action}Response xmlns:u=\"{SERVICE_TYPE}\">{inner}</u:{action}Response>\
         </s:Body></s:Envelope>"
    );
    http_ok("text/xml; charset=\"utf-8\"", &body)
}

fn soap_fault(code: u16, why: &str) -> String {
    let body = format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault>\
         <faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
         <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{code}</errorCode><errorDescription>{why}</errorDescription>\
         </UPnPError></detail></s:Fault></s:Body></s:Envelope>"
    );
    format!(
        "HTTP/1.1 500 Internal Server Error\r\n\
         Content-Type: text/xml\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn http_ok(content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

fn unescape_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
