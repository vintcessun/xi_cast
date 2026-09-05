//! flash 目录的行为测试，跑在**较真的**假 NOR flash 上
//! （见 `sim::flash`：写只能把 1 变 0、擦除按扇区、写按 4 字节对齐）。
//!
//! 这些用例回答的是「板子到货之前，怎么知道存储这块是对的」：
//! 掉电撕裂、bank 写满、整理到一半断电、重启之后数据还在不在。

use xi_cast_core::store::{Catalog, Visit};
use xi_cast_core::xmtv::{Date, Item};
use xi_cast_sim::flash::{MockFlash, SECTOR};

/// 测试分区：8 个扇区 32KB，对半分成两个 16KB 的 bank。
const 分区: usize = SECTOR * 8;

fn 造一条(id: u32, publish_time: u32, title: &str, day: u8) -> Item {
    let mut slug = [0u8; 16];
    slug.copy_from_slice(format!("{id:016x}").as_bytes());
    Item {
        id,
        publish_time,
        date: Date {
            year: 26,
            month: 1,
            day,
        },
        slug,
        title: heapless::String::try_from(title).unwrap(),
    }
}

/// 模拟拔电源再插上：flash 内容原样保留，重新打开目录。
fn 重启(catalog: Catalog<MockFlash>) -> Catalog<MockFlash> {
    let data = catalog.release().snapshot();
    Catalog::open(MockFlash::from_snapshot(data)).expect("重启后应该能打开")
}

#[test]
fn 全新的板子能直接用() {
    let c = Catalog::open(MockFlash::new(分区)).unwrap();
    assert_eq!(c.summary().items, 0);
    assert_eq!(c.summary().newest, 0);
    assert!(c.summary().device.is_none());
    assert!(!c.summary().damaged);
}

#[test]
fn 分区太小时明确报错() {
    // 与其在运行时莫名其妙，不如开机就说清楚分区表写小了
    assert!(Catalog::open(MockFlash::new(SECTOR * 2)).is_err());
}

#[test]
fn 写进去的节目重启之后还在() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    for i in 1..=5u32 {
        c.append_item(&造一条(i, 1000 + i, "花倾蝶", i as u8))
            .unwrap();
    }
    assert_eq!(c.summary().items, 5);

    let c = 重启(c);
    assert_eq!(c.summary().items, 5, "重启之后条数要对得上");
    assert_eq!(c.summary().newest, 1005);
    assert_eq!(c.summary().oldest, 1001);
}

#[test]
fn 追加不动已经写过的字节() {
    // 这是「追加式」的全部意义所在：老数据一个字节都不重写，
    // 既不磨损 flash，也不存在「重写到一半掉电全丢」
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    c.append_item(&造一条(1, 1001, "甲", 1)).unwrap();

    let 前 = c.release().snapshot();
    let mut c = Catalog::open(MockFlash::from_snapshot(前.clone())).unwrap();
    c.append_item(&造一条(2, 1002, "乙", 2)).unwrap();
    let flash = c.release();
    let 后 = flash.snapshot();

    assert_eq!(flash.erase_count, 0, "追加不该触发任何擦除");
    // 老数据所在的那一段（写过的、非 0xFF 的前缀）必须一个字节都没变
    let 老数据长度 = 前.iter().rposition(|&b| b != 0xFF).unwrap() + 1;
    assert_eq!(
        &前[..老数据长度],
        &后[..老数据长度],
        "老记录被动过了：追加式存储的前提就是不重写已有数据"
    );
    assert!(后.len() > 老数据长度 && 后[老数据长度..].iter().any(|&b| b != 0xFF));
}

#[test]
fn 一天一条的话很多年都不用擦一次() {
    // 用「擦除次数」衡量 flash 磨损：整份重写的方案每次更新都要擦满分区
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    for i in 0..300u32 {
        c.append_item(&造一条(i, 1000 + i, "戏", 1)).unwrap();
    }
    let flash = c.release();
    assert_eq!(
        flash.erase_count, 1,
        "只有第一次格式化那一次擦除，300 条追加下来一次都不该再擦"
    );
}

#[test]
fn 写到一半掉电留下的半截记录会被丢掉() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    c.append_item(&造一条(1, 1001, "甲", 1)).unwrap();
    c.append_item(&造一条(2, 1002, "乙", 2)).unwrap();

    // 第三条写到第 8 个字节时断电
    let mut flash = c.release();
    flash.tear_next_write(8);
    let mut c = Catalog::open(flash).unwrap();
    let 结果 = c.append_item(&造一条(3, 1003, "丙", 3));
    assert!(结果.is_err(), "掉电的写入必须报错");

    // 重新上电
    let c = 重启(c);
    assert_eq!(c.summary().items, 2, "只该剩下完整的两条");
    assert_eq!(c.summary().newest, 1002);
    assert!(!c.summary().damaged, "打开时就应该已经整理干净了");
}

#[test]
fn 掉电整理之后还能继续追加() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    c.append_item(&造一条(1, 1001, "甲", 1)).unwrap();

    let mut flash = c.release();
    flash.tear_next_write(8);
    let mut c = Catalog::open(flash).unwrap();
    let _ = c.append_item(&造一条(2, 1002, "乙", 2));

    let mut c = 重启(c);
    // 关键：坏记录后面的空间不能就这么废了，整理完要能接着写
    c.append_item(&造一条(3, 1003, "丙", 3)).unwrap();
    let c = 重启(c);
    assert_eq!(c.summary().items, 2);
    assert_eq!(c.summary().newest, 1003);
}

#[test]
fn 中间某条被宇宙射线打坏时不会读出垃圾() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    for i in 1..=4u32 {
        c.append_item(&造一条(i, 1000 + i, "甲", 1)).unwrap();
    }
    let mut data = c.release().snapshot();
    // 把第二条记录的内容改一个位（CRC 应该拦下来）
    data[16 + 48 + 6] ^= 0x08;

    let c = Catalog::open(MockFlash::from_snapshot(data)).unwrap();
    assert_eq!(c.summary().items, 1, "坏记录以及它后面的都不能算数");
}

#[test]
fn bank_写满时自动整理并接着用() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    let 代数前 = c.generation();

    // 真实情况下把 bank 撑满的是**直链缓存**（一条 88 字节，每播一集就多一条），
    // 节目本身一天才一条。所以：先写 100 条节目，再拿缓存把 bank 填满，
    // 整理时缓存全丢、节目一条不少
    for i in 0..100u32 {
        c.append_item(&造一条(i, 1000 + i, "戏", 1)).unwrap();
    }
    let mut i = 0u32;
    while c.generation() == 代数前 {
        c.append_resolved(
            i % 100,
            "https://vod1.kxm.xmtv.cn/video/2026/09/02/aaaaaaaabbbbbbbbccccccccdddddddd.mp4",
        )
        .unwrap();
        i += 1;
        assert!(i < 10_000, "写这么多都没满，说明整理没被触发");
    }

    assert!(c.generation() > 代数前, "整理过之后代数要往上走");
    assert_eq!(c.summary().items, 100, "整理不能弄丢节目");
    assert!(c.free() > 8000, "整理之后要真的腾出空间来");

    let c = 重启(c);
    assert_eq!(c.summary().items, 100, "重启后仍然是 100 条");
    assert_eq!(c.summary().newest, 1099);
}

#[test]
fn 整理时丢掉视频直链缓存但保留节目() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    for i in 0..100u32 {
        c.append_item(&造一条(i, 1000 + i, "戏", 1)).unwrap();
        c.append_resolved(
            i,
            "https://vod1.kxm.xmtv.cn/video/2026/09/02/aaaaaaaabbbbbbbbccccccccdddddddd.mp4",
        )
        .unwrap();
    }
    assert!(c.resolved_url(50).unwrap().is_some());

    c.compact().unwrap();
    assert_eq!(c.summary().items, 100, "节目要全部保留");
    assert_eq!(
        c.resolved_url(50).unwrap(),
        None,
        "直链缓存整理时丢掉，需要时重新解析一次就有了"
    );
}

#[test]
fn 整理到一半掉电时老数据一条不少() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    for i in 1..=20u32 {
        c.append_item(&造一条(i, 1000 + i, "甲", 1)).unwrap();
    }
    let 代数 = c.generation();

    // 整理过程：擦目标 bank(1 次) + 逐条搬(20 次) + 写头(1 次)。
    // 在搬到一半的时候断电
    let mut flash = c.release();
    flash.power_loss_after(8);
    let mut c = Catalog::open(flash).unwrap();
    assert!(c.compact().is_err(), "掉电时整理必须失败");

    let c = 重启(c);
    assert_eq!(c.summary().items, 20, "老 bank 必须完好无损");
    assert_eq!(c.generation(), 代数, "新 bank 没写头，代数不该变");
}

#[test]
fn 记住的设备重启之后还认得() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    c.remember_device("uuid:abcd", "http://192.168.1.20:8200/rootDesc.xml")
        .unwrap();

    let c = 重启(c);
    let d = c.summary().device.as_ref().unwrap();
    assert_eq!(d.usn, "uuid:abcd");
    assert_eq!(d.location, "http://192.168.1.20:8200/rootDesc.xml");
}

#[test]
fn 同一台设备不会每次开机都写一条() {
    // 每次开机写一条的话，用不了两年就把 bank 写满、白白多整理很多次
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    c.remember_device("uuid:abcd", "http://192.168.1.20:8200/d.xml")
        .unwrap();
    let 空闲 = c.free();
    for _ in 0..50 {
        c.remember_device("uuid:abcd", "http://192.168.1.20:8200/d.xml")
            .unwrap();
    }
    assert_eq!(c.free(), 空闲, "地址没变就不该再写");

    // 电视换了 IP（DHCP 续租）就要更新
    c.remember_device("uuid:abcd", "http://192.168.1.77:8200/d.xml")
        .unwrap();
    assert!(c.free() < 空闲);
    let c = 重启(c);
    assert_eq!(
        c.summary().device.as_ref().unwrap().location,
        "http://192.168.1.77:8200/d.xml",
        "要认最后写的那条"
    );
}

#[test]
fn 直链缓存取最后写的那条() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    c.append_resolved(7, "http://old/a.mp4").unwrap();
    c.append_resolved(7, "http://new/a.mp4").unwrap();
    assert_eq!(c.resolved_url(7).unwrap().unwrap(), "http://new/a.mp4");
    assert_eq!(c.resolved_url(8).unwrap(), None);
}

#[test]
fn 补齐标记会被记住() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    assert!(!c.summary().backfilled);
    c.mark_backfilled().unwrap();
    let c = 重启(c);
    assert!(c.summary().backfilled, "补齐过的事实要能跨重启");
}

#[test]
fn 挑一部戏能把同名的都收齐并按时间排好() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    // 故意乱序写入，而且两部戏交叉 —— 真实数据就是这样
    c.append_item(&造一条(3, 3000, "花倾蝶", 3)).unwrap();
    c.append_item(&造一条(9, 9000, "柳君拂", 9)).unwrap();
    c.append_item(&造一条(1, 1000, "花倾蝶", 1)).unwrap();
    c.append_item(&造一条(2, 2000, "花倾蝶", 2)).unwrap();

    // seed 取到「花倾蝶」那几条中的一条
    let mut 找到 = None;
    for seed in 0..4u32 {
        let s = c.pick_series(seed).unwrap().unwrap();
        if s.title == "花倾蝶" {
            找到 = Some(s);
            break;
        }
    }
    let s = 找到.expect("四条里三条是花倾蝶，不该挑不到");
    assert_eq!(s.episodes.len(), 3);
    let 时间: Vec<u32> = s.episodes.iter().map(|e| e.publish_time).collect();
    assert_eq!(时间, vec![1000, 2000, 3000], "必须从第一集开始放");
}

#[test]
fn 重复写进去的同一条只算一集() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    c.append_item(&造一条(1, 1000, "花倾蝶", 1)).unwrap();
    c.append_item(&造一条(1, 1000, "花倾蝶", 1)).unwrap();
    let s = c.pick_series(0).unwrap().unwrap();
    assert_eq!(s.episodes.len(), 1, "id 相同的要去重，否则会连放两遍同一集");
}

#[test]
fn 空目录挑不出戏但也不会崩() {
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    assert!(c.pick_series(12345).unwrap().is_none());
}

#[test]
fn 用真实的二十条数据走一遍() {
    use xi_cast_core::xmtv::{ItemStream, parse_item};

    let body = include_str!("../../fixtures/search_20.json");
    let mut stream = ItemStream::new();
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();

    let mut items = Vec::new();
    stream.feed(body.as_bytes(), &mut |text| {
        if let Some(item) = parse_item(text) {
            items.push(item);
        }
    });
    for item in &items {
        c.append_item(item).unwrap();
    }
    assert_eq!(c.summary().items, 20);

    let c = 重启(c);
    // 重启之后，节目内容要和当初解析出来的完全一致
    let mut 读回 = Vec::new();
    let mut c = c;
    c.for_each(|v| {
        if let Visit::Item {
            id,
            publish_time,
            date,
            slug,
            title,
        } = v
        {
            读回.push((id, publish_time, date, slug, title.to_string()));
        }
        true
    })
    .unwrap();

    assert_eq!(读回.len(), 20);
    for (i, item) in items.iter().enumerate() {
        assert_eq!(读回[i].0, item.id);
        assert_eq!(读回[i].1, item.publish_time);
        assert_eq!(读回[i].2, item.date);
        assert_eq!(读回[i].3, item.slug);
        assert_eq!(读回[i].4, item.title.as_str());
    }
}

#[test]
fn 一个_bank_装得下多少条() {
    // 这个数字决定分区表里要给多大空间，变了就要回去改 partitions.csv
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    let mut n = 0u32;
    let 代数 = c.generation();
    while c.generation() == 代数 {
        c.append_item(&造一条(n, 1000 + n, "大名春秋", 1)).unwrap();
        n += 1;
        assert!(n < 10_000, "不该写不满");
    }
    // 剧目名 12 字节（全量数据的平均值）时一条 48 字节；(16384-16)/48 ≈ 341
    assert!(
        (335..350).contains(&n),
        "一个 16KB bank 装了 {n} 条，和预期的 341 差太多"
    );
    // 换算到真实数据：2291 条 × 48 ≈ 110KB。
    // 一个 bank 给 512KB（分区 1MB）能装约 1 万条，够用二十年
    println!("一条 48 字节，{n} 条/16KB bank");
}

#[test]
fn bank_真的满了就丢最老的而不是从此写不进() {
    // 这条路二十年内走不到，但它必须是「丢老数据」而不是「拒绝新数据」：
    // 后者会让设备永远停在某一天的节目上
    let mut c = Catalog::open(MockFlash::new(分区)).unwrap();
    for i in 0..2000u32 {
        c.append_item(&造一条(i, 1000 + i, "大名春秋一二三四五六七八九十", 1))
            .unwrap();
    }
    let s = c.summary();
    assert!(s.items < 2000, "满了之后应该丢掉了一部分");
    assert!(s.items > 100, "但不能丢得只剩渣，实际剩 {}", s.items);
    assert_eq!(s.newest, 2999, "最新的那条必须还在");
    assert!(s.oldest > 1000, "丢掉的应该是最老的那些");

    // 丢过之后还得能继续写
    let mut c = 重启(c);
    let 之前 = c.summary().items;
    c.append_item(&造一条(9999, 99999, "新戏", 1)).unwrap();
    assert_eq!(c.summary().items, 之前 + 1);
}
