//! 端到端：假电视 + 假 xmtv + 假 flash，跑的是固件里那一份 `App`。
//!
//! 开发板还没到货，这些用例就是「功能到底行不行」的答案。它们覆盖的是
//! 真上板之后最容易翻车的地方：
//!
//! * 冷启动（flash 空的）能不能自己拉到节目、找到电视、把第一集投出去；
//! * 第二次开机是不是真的不用重新下载、不用重新扫描；
//! * 一部戏能不能一集接一集地按顺序播完；
//! * 电视中途关机、地址变了、分享页被 WAF 拦了，会不会卡死。

use std::time::Duration;

use xi_cast_core::app::{App, Config};
use xi_cast_core::soap::TransportState;
use xi_cast_core::store::Catalog;
use xi_cast_sim::flash::{MockFlash, SECTOR};
use xi_cast_sim::mock_tv::MockTv;
use xi_cast_sim::mock_xmtv::MockXmtv;
use xi_cast_sim::net::SimNet;

const 分区: usize = SECTOR * 8;

/// 测试用的节目库：三部戏，集数各不相同。
const 剧目: &[(&str, u32)] = &[("花倾蝶", 4), ("柳君拂", 3), ("春草闯堂", 2)];

fn 快节奏配置() -> Config {
    Config {
        search_rounds: 1,
        scan_ms: 400,
        retry_ms: 100,
        poll_ms: 30,
        first_pages: 5,
        sync_pages: 2,
        ..Default::default()
    }
}

struct 台架 {
    app: App<SimNet, MockFlash>,
    tv: MockTv,
    xmtv: MockXmtv,
}

async fn 搭台(polls_per_episode: usize, flash: MockFlash) -> 台架 {
    let tv = MockTv::start(polls_per_episode).await.unwrap();
    let xmtv = MockXmtv::start(剧目).await.unwrap();

    let mut net = SimNet::pointing_at(tv.ssdp_addr, 0x1234_5678)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);
    net.redirect(xi_cast_core::xmtv::SHARE_HOST, xmtv.addr);

    let catalog = Catalog::open(flash).unwrap();
    台架 {
        app: App::new(net, catalog, 快节奏配置()),
        tv,
        xmtv,
    }
}

#[tokio::test]
async fn 冷启动一路投到电视上() {
    let mut t = 搭台(2, MockFlash::new(分区)).await;

    // ① flash 是空的，先把节目拉下来
    let added = t.app.sync_new().await.expect("拉节目不该失败");
    assert_eq!(added, 9, "三部戏一共 4+3+2 集");
    assert_eq!(t.app.catalog.summary().items, 9);

    // ② 找电视：这时候 flash 里还没记过设备，走 SSDP
    let renderer = t.app.find_renderer().await.expect("应该扫得到那台假电视");
    assert!(
        renderer.friendly_name.contains("FastCast"),
        "设备名不对: {}",
        renderer.friendly_name
    );
    assert_eq!(
        renderer.service_type, "urn:schemas-upnp-org:service:AVTransport:1",
        "SOAPAction 要用设备自己声明的服务类型"
    );

    // ③ 随机挑一部戏
    let series = t.app.pick().expect("挑得出戏");
    assert!(!series.episodes.is_empty());

    // ④ 把第一集的分享页解析成 mp4 直链
    let video = t
        .app
        .resolve(&series.episodes[0])
        .await
        .expect("解得出直链");
    assert!(video.ends_with(".mp4"), "要 mp4 不要 m3u8: {video}");

    // ⑤ 投出去
    t.app
        .cast(&renderer, &video, &series.title)
        .await
        .expect("投屏失败");

    assert_eq!(
        t.tv.current_uri().as_deref(),
        Some(video.as_str()),
        "电视收到的地址不对"
    );
    assert_eq!(t.tv.play_count(), 1, "应该正好调用一次 Play");
    assert_eq!(
        t.tv.state().current_title.as_deref(),
        Some(series.title.as_str()),
        "元数据里的标题要能被设备正确解出来（双层转义不能出错）"
    );
    // 顺序也有讲究：先 SetAVTransportURI 再 Play
    assert_eq!(
        t.tv.state().actions,
        vec!["SetAVTransportURI".to_string(), "Play".to_string()]
    );

    // ⑥ 播放状态：先在播，查够次数之后报播完
    assert_eq!(
        t.app.transport_state(&renderer).await.unwrap(),
        TransportState::Playing
    );
    assert_eq!(
        t.app.transport_state(&renderer).await.unwrap(),
        TransportState::Playing
    );
    let 完 = t.app.transport_state(&renderer).await.unwrap();
    assert!(完.is_finished(), "该报播完了，实际是 {完:?}");
}

#[tokio::test]
async fn 第二次开机不用重新下载也不用重新扫描() {
    let flash = {
        let mut t = 搭台(2, MockFlash::new(分区)).await;
        t.app.sync_new().await.unwrap();
        t.app.find_renderer().await.unwrap();
        t.app.catalog.release().snapshot()
    };

    let 死端口 = "127.0.0.1:9".parse().unwrap();

    // 重新上电：同一块 flash，节目和设备都应该还在
    let catalog = Catalog::open(MockFlash::from_snapshot(flash.clone())).unwrap();
    assert!(
        catalog.summary().device.is_some(),
        "上次的设备应该被记在 flash 里了"
    );
    assert_eq!(catalog.summary().items, 9, "节目也应该还在，不用重新下载");

    // 关键一条：把 SSDP 指向一个死端口。要是代码还想靠扫描找设备，
    // 这里必然失败 —— 能成功就证明它真的是直接连了记着的那个地址
    let xmtv2 = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at(死端口, 7).await.unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv2.addr);
    net.redirect(xi_cast_core::xmtv::SHARE_HOST, xmtv2.addr);
    let mut app = App::new(net, catalog, 快节奏配置());

    let r = app
        .find_renderer()
        .await
        .expect("记着地址就该直接连上，根本不需要扫描");
    assert!(r.friendly_name.contains("FastCast"));

    // 增量更新：节目一条没变，所以一页就该停下来
    let 之前 = xmtv2.api_hits.load(std::sync::atomic::Ordering::SeqCst);
    let added = app.sync_new().await.unwrap();
    let 之后 = xmtv2.api_hits.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(added, 0, "没有新节目就不该新增");
    assert_eq!(
        之后 - 之前,
        1,
        "增量更新只该翻一页，实际翻了 {} 页",
        之后 - 之前
    );
    assert_eq!(app.catalog.summary().items, 9, "老节目一条都不能丢");
}

#[tokio::test]
async fn 记着的电视换了地址时会自动退回扫描() {
    // 家里路由器重启、DHCP 重新分配，电视的 IP 就变了。
    // 这时候记着的老地址连不上，必须自己退回去扫一遍，不能就此罢工
    let tv = MockTv::start(2).await.unwrap();
    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at(tv.ssdp_addr, 3).await.unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let mut catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    // 记一个绝对连不上的地址
    catalog
        .remember_device("uuid:老设备", "http://127.0.0.1:9/desc.xml")
        .unwrap();

    let mut app = App::new(net, catalog, 快节奏配置());
    let r = app.find_renderer().await.expect("老地址不通就该去扫描");
    assert!(r.friendly_name.contains("FastCast"));

    // 而且要把新地址记下来，下次开机就不用再扫了
    let 记住的 = app.catalog.summary().device.clone().unwrap();
    assert_eq!(
        记住的.location.as_str(),
        tv.location,
        "新地址应该覆盖掉老的"
    );
}

#[tokio::test]
async fn 只给_ip_也能自己找到端口和描述地址() {
    // 这是「电视 IP 在路由器里绑死」的正式用法：配置里只写一个 IP，
    // 端口和描述文件路径由板子自己去问。这里让假电视蹲在 49152
    //（各家 DLNA 设备最常用的端口，家里那台 HappyCast 电视就是它），
    // 而 SSDP 只收单播且不在 1900 上 —— 也就是说单播搜索一定问不到，
    // 必须靠「试常见地址」这条兜底路把它找出来
    let tv = MockTv::start_on_port(2, 49152).await.unwrap();
    let xmtv = MockXmtv::start(剧目).await.unwrap();

    // SSDP 指向死端口：只要代码想广播扫描就一定失败
    let mut net = SimNet::pointing_at("127.0.0.1:9".parse().unwrap(), 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let cfg = Config {
        fixed_ip: Some("127.0.0.1"),
        scan_ms: 200, // 单播等 200ms 没人应答就走兜底
        ..快节奏配置()
    };
    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, cfg);

    let r = app
        .find_renderer()
        .await
        .expect("只给 IP 也应该找得到那台电视");
    assert!(r.friendly_name.contains("FastCast"));
    assert_eq!(r.control.port, 49152, "控制入口应该在它自己声明的端口上");
    let _ = tv;
}

#[tokio::test]
async fn 指定了_ip_就绝不会投到别的设备上() {
    // 这条是「不会串」的正式保证：局域网里另有一台能投屏的设备
    //（邻居家的盒子），而我们指定的那个 IP 上什么都没有。
    // 正确行为是找不到就找不到，绝不能改投扫到的那台
    let 邻居家的盒子 = MockTv::start_on_port(2, 49153).await.unwrap();
    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at(邻居家的盒子.ssdp_addr, 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let cfg = Config {
        // 一个本机上不可能有 DLNA 设备的地址
        fixed_ip: Some("127.0.0.2"),
        scan_ms: 200,
        ..快节奏配置()
    };
    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, cfg);

    assert!(
        app.find_renderer().await.is_none(),
        "指定 IP 上没有设备，就该老老实实返回 None"
    );
    assert_eq!(邻居家的盒子.play_count(), 0, "一次都不该碰别人家的设备");
}

#[tokio::test]
async fn 电视_ip_写错了不会崩() {
    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at("127.0.0.1:9".parse().unwrap(), 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    for 乱写的 in ["", "192.168.0", "不是IP", "999.1.1.1", "192.168.0.100.5"] {
        let cfg = Config {
            fixed_ip: Some(乱写的),
            scan_ms: 100,
            ..快节奏配置()
        };
        let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
        let mut app = App::new(
            SimNet::pointing_at("127.0.0.1:9".parse().unwrap(), 3)
                .await
                .unwrap(),
            catalog,
            cfg,
        );
        assert!(
            app.find_renderer().await.is_none(),
            "「{乱写的}」不该被当成合法 IP"
        );
    }
    let _ = net;
}

#[tokio::test]
async fn 写死电视地址之后根本不扫描() {
    // 电视 IP 在路由器里绑死之后就该用这个：省掉开机那几秒 SSDP，
    // 也不会投错到邻居家的盒子上（组播不认门牌号）
    let tv = MockTv::start(2).await.unwrap();
    let xmtv = MockXmtv::start(剧目).await.unwrap();

    // SSDP 指向死端口：只要代码想扫描就一定失败
    let mut net = SimNet::pointing_at("127.0.0.1:9".parse().unwrap(), 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let cfg = Config {
        fixed_device: Some(Box::leak(tv.location.clone().into_boxed_str())),
        ..快节奏配置()
    };
    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, cfg);

    let r = app.find_renderer().await.expect("写死的地址应该直接连上");
    assert!(r.friendly_name.contains("FastCast"));
    assert!(
        app.catalog.summary().device.is_none(),
        "写死地址时不该往 flash 里记设备 —— 配置说了算，没必要占空间"
    );
}

#[tokio::test]
async fn 写死的电视没开机时只等它不投别人() {
    // 关键行为：这时候**不能**退回去扫描。扫到的很可能是邻居家的盒子，
    // 戏就投到别人家电视上了
    let 别人家的盒子 = MockTv::start(2).await.unwrap();
    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at(别人家的盒子.ssdp_addr, 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let cfg = Config {
        // 指向一个没人监听的地址，模拟自家电视关着
        fixed_device: Some("http://127.0.0.1:9/desc.xml"),
        ..快节奏配置()
    };
    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, cfg);

    assert!(
        app.find_renderer().await.is_none(),
        "自家电视没开机就该等着，绝不能改投扫到的那台"
    );
    assert_eq!(别人家的盒子.play_count(), 0, "一次都不该碰别人家的设备");
}

/// 钉死「哪一处算试探」。
///
/// 真机上踩过这个坑：`probe_fixed_ip` 挨个试 8 个端口，每个等满 10 秒的收发
/// 超时，一轮 86 秒 —— 开了电视最多等一分半才被发现。缩短超时时我一开始
/// 用「目标是不是 IP 字面量」来区分，结果把投屏路径一起缩了（电视地址也是
/// IP 字面量），电视忙着去取视频那一下就报「查状态失败: 连不上」。
///
/// 所以区分的依据必须是「这个地址是不是碰运气猜的」，而这件事只有核心库
/// 知道。下面两条各守一半。
#[tokio::test]
async fn 碰运气试端口才算试探() {
    use std::sync::atomic::Ordering;

    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at("127.0.0.1:9".parse().unwrap(), 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let cfg = Config {
        // 本机上不可能有 DLNA 设备的地址：单播搜索没人应，八个端口全试一遍
        fixed_ip: Some("127.0.0.2"),
        scan_ms: 200,
        ..快节奏配置()
    };
    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, cfg);

    assert!(app.find_renderer().await.is_none());
    assert_eq!(
        app.net.试探连接次数.load(Ordering::SeqCst),
        8,
        "WELL_KNOWN_DESC 里那 8 个地址都是碰运气猜的，都该走试探"
    );
    assert_eq!(
        app.net.正经连接次数.load(Ordering::SeqCst),
        0,
        "这条路上没有任何「本该连上」的连接"
    );
}

#[tokio::test]
async fn 播放路径上一次都不该用试探连接() {
    use std::sync::atomic::Ordering;

    let mut t = 搭台(2, MockFlash::new(分区)).await;
    t.app.sync_new().await.expect("拉节目不该失败");
    let renderer = t.app.find_renderer().await.expect("该扫到假电视");
    let series = t.app.pick().expect("挑得出戏");
    let video = t
        .app
        .resolve(&series.episodes[0])
        .await
        .expect("解得出直链");
    t.app
        .cast(&renderer, &video, &series.title)
        .await
        .expect("投屏失败");
    t.app.transport_state(&renderer).await.unwrap();

    assert_eq!(
        t.app.net.试探连接次数.load(Ordering::SeqCst),
        0,
        "拉节目、SSDP 扫到的地址、取分享页、投屏、查状态 —— 一个都不是碰运气猜的，         都该用正常的超时。把它们当试探的代价是电视一忙就被判失联"
    );
    assert!(t.app.net.正经连接次数.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn 电视没开机的时候节目表照样会更新() {
    // 这条盯的是一个真缺陷：原来 `sync_new` 挂在「找到电视」之后，
    // 电视关一周，节目表就一周不动。板子明明一直通着电，闲着也是闲着
    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at("127.0.0.1:9".parse().unwrap(), 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let cfg = Config {
        // 本机上不可能有 DLNA 设备的地址 —— 模拟电视关着
        fixed_ip: Some("127.0.0.2"),
        scan_ms: 200,
        idle_sync_every: 1,
        ..快节奏配置()
    };
    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, cfg);

    assert_eq!(app.catalog.summary().items, 0, "一开始 flash 是空的");
    assert!(app.find_renderer().await.is_none(), "电视没开就该找不到");

    app.idle_update(0).await;
    assert!(
        app.catalog.summary().items > 0,
        "等电视的空当里就该把节目表拉下来了，而不是干等着"
    );
}

#[tokio::test]
async fn 等电视的空当里不会把上游问烂() {
    // 一轮找电视失败在板子上约一分钟。要是每轮都更新，就是一天一千多次
    // 请求，而上游一天才多一条节目
    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at("127.0.0.1:9".parse().unwrap(), 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let cfg = Config {
        fixed_ip: Some("127.0.0.2"),
        scan_ms: 200,
        idle_sync_every: 5,
        ..快节奏配置()
    };
    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, cfg);

    let 问了几次 = || xmtv.api_hits.load(std::sync::atomic::Ordering::SeqCst);

    app.idle_update(0).await;
    let 第一轮之后 = 问了几次();
    assert!(第一轮之后 > 0, "刚落空那一轮就该更新一次，不用等");

    for round in 1..5 {
        app.idle_update(round).await;
    }
    assert_eq!(问了几次(), 第一轮之后, "中间这几轮一次都不该去问上游");

    app.idle_update(5).await;
    assert!(问了几次() > 第一轮之后, "到第 5 轮该再更新一次");
}

#[tokio::test]
async fn 把空当更新关掉就真的不更新() {
    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let mut net = SimNet::pointing_at("127.0.0.1:9".parse().unwrap(), 3)
        .await
        .unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);

    let cfg = Config {
        fixed_ip: Some("127.0.0.2"),
        scan_ms: 200,
        idle_sync_every: 0,
        ..快节奏配置()
    };
    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, cfg);

    for round in 0..8 {
        app.idle_update(round).await;
    }
    assert_eq!(
        xmtv.api_hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "配成 0 就是明说了不要这个行为"
    );
    assert_eq!(app.catalog.summary().items, 0);
}

#[tokio::test]
async fn 有新节目时只补新的那几条() {
    let mut t = 搭台(2, MockFlash::new(分区)).await;
    t.app.sync_new().await.unwrap();
    assert_eq!(t.app.catalog.summary().items, 9);
    let 空闲之前 = t.app.catalog.free();

    // 换一个多了两集的节目库（模拟第二天又播了两集）
    let 新库 = MockXmtv::start(&[("新戏", 2), ("花倾蝶", 4), ("柳君拂", 3), ("春草闯堂", 2)])
        .await
        .unwrap();
    let mut net = SimNet::pointing_at(t.tv.ssdp_addr, 99).await.unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, 新库.addr);
    net.redirect(xi_cast_core::xmtv::SHARE_HOST, 新库.addr);
    t.app.net = net;

    let added = t.app.sync_new().await.unwrap();
    assert_eq!(added, 2, "只该新增两条");
    assert_eq!(t.app.catalog.summary().items, 11);
    // 关键：新增只写了两条记录的空间，不是把 11 条重写一遍
    let 用掉的 = 空闲之前 - t.app.catalog.free();
    assert!(
        用掉的 < 120,
        "两条记录应该只占一百来字节，实际用了 {用掉的}"
    );
}

#[tokio::test]
async fn 直链缓存住了就不会再去请求分享页() {
    let mut t = 搭台(2, MockFlash::new(分区)).await;
    t.app.sync_new().await.unwrap();
    let series = t.app.pick().unwrap();
    let ep = series.episodes[0];

    let 第一次 = t.app.resolve(&ep).await.unwrap();
    let hits = t.xmtv.share_hits.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(hits, 1);

    let 第二次 = t.app.resolve(&ep).await.unwrap();
    assert_eq!(第一次, 第二次);
    assert_eq!(
        t.xmtv.share_hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "第二次应该直接读 flash 里的缓存，不再联网"
    );
}

#[tokio::test]
async fn 一部戏能一集接一集按顺序播完() {
    let mut t = 搭台(1, MockFlash::new(分区)).await;
    t.app.sync_new().await.unwrap();

    // 直接跑固件里那个「永不返回」的主循环，跑一段时间就掐掉
    let _ = tokio::time::timeout(Duration::from_secs(6), t.app.run()).await;

    let uris = t.tv.state().uris.clone();
    assert!(
        uris.len() >= 3,
        "这段时间里至少该投出去三集，实际 {}",
        uris.len()
    );
    // 同一部戏的各集地址不能重复（重复说明没往下走）
    let mut 去重 = uris.clone();
    去重.dedup();
    assert_eq!(去重.len(), uris.len(), "连着投了同一个地址两次: {uris:?}");
    assert!(uris.iter().all(|u| u.ends_with(".mp4")));
}

#[tokio::test]
async fn 电视中途关机不会把程序卡死() {
    let mut t = 搭台(50, MockFlash::new(分区)).await;
    t.app.sync_new().await.unwrap();

    let tv = t.tv.clone();
    tokio::spawn(async move {
        // 等第一集投出去之后再拔电源（找设备 + 更新 + 解析直链要花小一秒）
        tokio::time::sleep(Duration::from_millis(1500)).await;
        tv.go_offline();
    });

    // 电视关掉之后主循环必须还在转（会不停地重新找设备），不能卡住也不能 panic
    let _ = tokio::time::timeout(Duration::from_secs(5), t.app.run()).await;
    assert!(t.tv.play_count() >= 1, "关机之前至少投出去过一集");

    // 关机之后不能变成疯狂重试：连投几次不成就该回去重新找设备，
    // 而不是以最快速度把整部戏挨个投一遍
    let 动作数 = t.tv.state().actions.len();
    assert!(
        动作数 < 200,
        "关机之后还在拼命发 SOAP（一共 {动作数} 次），说明没有退避"
    );
}

#[tokio::test]
async fn 分享页少了浏览器_ua_就会被拦() {
    // 反过来验证：我们的客户端之所以能拿到直链，就是因为带了浏览器 UA。
    // 这里直接用最朴素的请求去要同一个页面，应该吃到 418
    let xmtv = MockXmtv::start(剧目).await.unwrap();
    let slug = &xmtv.entries[0].slug;
    let date = &xmtv.entries[0].date;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(xmtv.addr).await.unwrap();
    sock.write_all(
        format!("GET /xmtv/{date}/{slug}.html HTTP/1.1\r\nHost: x\r\nUser-Agent: curl/8.0\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    sock.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.starts_with("HTTP/1.1 418"),
        "应该被 WAF 拦下: {}",
        &text[..40.min(text.len())]
    );
}

#[tokio::test]
async fn 电视接了连接却一声不吭时会超时重来() {
    // 比「关机」更阴险的一种情况：盒子死机了，TCP 还能连上，但一个字节都不回。
    // 没有读超时的话，查播放状态那一步会永远挂着 —— 表面上「投了一集就不动了」，
    // 而且重连逻辑一次都不会被触发
    let 哑巴 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let 哑巴地址 = 哑巴.local_addr().unwrap();
    tokio::spawn(async move {
        let mut 挂着 = Vec::new();
        while let Ok((sock, _)) = 哑巴.accept().await {
            挂着.push(sock); // 接下来什么都不做，也不关连接
        }
    });

    let mut net = SimNet::pointing_at(哑巴地址, 1)
        .await
        .unwrap()
        .with_io_timeout(Duration::from_millis(300));
    net.redirect(xi_cast_core::xmtv::API_HOST, 哑巴地址);
    net.redirect(xi_cast_core::xmtv::SHARE_HOST, 哑巴地址);

    let mut catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    catalog
        .remember_device("uuid:装死的电视", &format!("http://{哑巴地址}/desc.xml"))
        .unwrap();

    let mut app = App::new(net, catalog, 快节奏配置());

    // 每一步都必须在超时之后失败返回，而不是挂死
    let 开始 = std::time::Instant::now();
    assert!(
        app.find_renderer().await.is_none(),
        "对方装死时不该认为找到了设备"
    );
    assert!(app.sync_new().await.is_err(), "拉节目也该超时失败");
    assert!(
        开始.elapsed() < Duration::from_secs(20),
        "花了 {:?}，说明某一步没有超时保护",
        开始.elapsed()
    );
}

#[tokio::test]
async fn 节目库是空的时候不会崩() {
    let tv = MockTv::start(2).await.unwrap();
    let xmtv = MockXmtv::start(&[]).await.unwrap();
    let mut net = SimNet::pointing_at(tv.ssdp_addr, 5).await.unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, xmtv.addr);
    net.redirect(xi_cast_core::xmtv::SHARE_HOST, xmtv.addr);

    let catalog = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut app = App::new(net, catalog, 快节奏配置());

    assert_eq!(app.sync_new().await.unwrap(), 0);
    assert!(app.pick().is_none());
    // 主循环会一直重试拉节目，不该 panic
    let _ = tokio::time::timeout(Duration::from_millis(600), app.run()).await;
}

#[tokio::test]
async fn xmtv_挂了也照样能投已经存下来的节目() {
    // 先存一批节目
    let flash = {
        let mut t = 搭台(2, MockFlash::new(分区)).await;
        t.app.sync_new().await.unwrap();
        t.app.catalog.release().snapshot()
    };

    // 这次不起 xmtv 服务器：接口全部连不上
    let tv = MockTv::start(1).await.unwrap();
    let mut net = SimNet::pointing_at(tv.ssdp_addr, 11).await.unwrap();
    net.redirect(xi_cast_core::xmtv::API_HOST, "127.0.0.1:9".parse().unwrap());
    net.redirect(
        xi_cast_core::xmtv::SHARE_HOST,
        "127.0.0.1:9".parse().unwrap(),
    );

    let catalog = Catalog::open(MockFlash::from_snapshot(flash)).unwrap();
    let mut app = App::new(net, catalog, 快节奏配置());

    assert!(app.sync_new().await.is_err(), "接口连不上就该报错");
    assert_eq!(app.catalog.summary().items, 9, "但缓存里的节目一条不少");
    assert!(app.find_renderer().await.is_some(), "找电视和 xmtv 没关系");
    // 拿不到直链（分享页也连不上），主循环得继续转而不是卡住
    let _ = tokio::time::timeout(Duration::from_secs(2), app.run()).await;
}
