//! flash 上的追加式节目目录。
//!
//! # 为什么不是一个 JSON 文件
//!
//! 上位机那份是 `data.txt`：一个 436KB 的 JSON，每次更新都
//! 「全量拉 2291 条 → 和旧的比一遍 → 整个文件重写一遍」。
//! 在电脑上无所谓，搬到 ESP32 上三条都要命：
//!
//! * 全量拉一次是 2.3MB，板子上要几十秒，而且**每次开机都要**；
//! * 整个重写意味着把 110KB 的分区全擦一遍再全写一遍 —— flash 擦写次数是
//!   有限的（10 万次量级），一天开关几次就在白白磨损；
//! * 重写到一半掉电，整份数据就没了。
//!
//! 所以这里换成**只追加的日志**：
//!
//! ```text
//!  ┌──────────── bank 0 (分区的前一半) ────────────┐
//!  │ 头(16B) │ 记录 │ 记录 │ 记录 │ …… │ 0xFF 0xFF │
//!  └────────────────────────────────────────────────┘
//!  ┌──────────── bank 1 (分区的后一半) ────────────┐
//!  │ 整理(compact)的时候才用到，平时是空的          │
//!  └────────────────────────────────────────────────┘
//! ```
//!
//! 新节目就在末尾接一条，**不动已经写过的任何一个字节**。开机不用联网也能直接
//! 播，联网之后只补当天新增的那几条（一般一天一条）。
//!
//! # 一条记录长什么样
//!
//! ```text
//!   0      1      2      3      4                    4+n            +4
//!   ┌──────┬──────┬─────────────┬────────────────────┬──────┬──────┐
//!   │ 类型 │ 标志 │ 长度 n (LE) │ 负载（补齐到 4 的倍数）│ CRC16│ 填充 │
//!   └──────┴──────┴─────────────┴────────────────────┴──────┴──────┘
//! ```
//!
//! 全部按 4 字节对齐 —— ESP32 的 flash 写入粒度就是 4 字节，不对齐直接报错。
//! 类型 `0xFF` 表示「这里还是擦除态」，也就是日志的末尾：
//! 开机扫一遍就知道该从哪儿接着写，不需要额外记指针。
//!
//! # 掉电了怎么办
//!
//! 这东西是「一上电就投屏」的设备，用户随手拔电源是常态，所以：
//!
//! * 每条记录带 CRC，写到一半掉电留下的半截记录**校验一定过不了**；
//! * 扫描扫到坏记录就停在那儿，把前面的好记录整理到另一个 bank，
//!   坏的那条连同后面的空间一起丢掉（[`Catalog::compact`]）；
//! * bank 头是**整理完成之后最后才写**的。整理到一半掉电，新 bank 没有头，
//!   就还是无效的，老 bank 原封不动 —— 数据不会两边都不完整。

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use heapless::{String, Vec};

use crate::xmtv::{Date, Item, MAX_EPISODES, SLUG_LEN, TITLE_LEN, VIDEO_URL_LEN};

/// bank 头的魔数，换格式时改这里。
pub const MAGIC: [u8; 4] = *b"XiC1";
/// bank 头占多少字节（魔数 4 + 代数 4 + CRC 2 + 保留 6）。
pub const BANK_HEADER: u32 = 16;

/// 负载最长多少字节。最长的是 DEVICE 记录（USN + LOCATION）。
pub const MAX_PAYLOAD: usize = 256;
/// 一条记录连头带尾最多多少字节。
pub const MAX_RECORD: usize = 4 + MAX_PAYLOAD + 4;

/// 设备 USN 最长多少字节。
pub const USN_LEN: usize = 96;
/// 设备描述地址最长多少字节。
pub const LOCATION_LEN: usize = 128;

mod kind {
    /// 一条节目
    pub const ITEM: u8 = 0x01;
    /// 某条节目解析出来的视频直链（缓存，丢了可以重新解析）
    pub const RESOLVED: u8 = 0x02;
    /// 记住上次投屏的那台设备
    pub const DEVICE: u8 = 0x03;
    /// 一些零碎状态（比如「历史节目补齐了没」）
    pub const STATE: u8 = 0x04;
    /// 擦除态，也就是日志末尾
    pub const ERASED: u8 = 0xFF;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// 底层 flash 报错
    Flash(E),
    /// 整理过之后还是放不下（分区给小了）
    Full,
    /// 分区尺寸不合法（太小、或者不是扇区整数倍）
    BadPartition,
    /// 要写的东西超过一条记录的上限
    TooLarge,
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Error::Flash(e)
    }
}

impl<E> Error<E> {
    /// 一句话说清是哪一类失败（`defmt` 打不了任意 `Debug` 类型）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::Flash(_) => "flash 读写出错",
            Error::Full => "分区满了",
            Error::BadPartition => "分区尺寸不合法",
            Error::TooLarge => "记录太大",
        }
    }
}

/// 记住的那台电视。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberedDevice {
    pub usn: String<USN_LEN>,
    pub location: String<LOCATION_LEN>,
}

/// 一集。比 [`Item`] 少一个剧目名 —— 同一部戏每集都一样，没必要存 64 份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Episode {
    pub id: u32,
    pub publish_time: u32,
    pub date: Date,
    pub slug: [u8; SLUG_LEN],
}

impl Episode {
    /// 拼出分享页路径。
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

/// 挑中的一部戏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    pub title: String<TITLE_LEN>,
    /// 按播出时间从早到晚，也就是第 1 集在前
    pub episodes: Vec<Episode, MAX_EPISODES>,
}

/// 扫完一遍日志之后掌握的全部情况。开机时算一次，之后增量维护。
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// 有多少条节目
    pub items: u32,
    /// 最新一条的发布时间（增量更新的水位）
    pub newest: u32,
    /// 最老一条的发布时间（往回补历史节目时用）
    pub oldest: u32,
    /// 历史节目补齐了没
    pub backfilled: bool,
    /// 上次记住的设备
    pub device: Option<RememberedDevice>,
    /// 扫描中途遇到坏记录（掉电留下的），需要整理
    pub damaged: bool,
}

/// flash 上的目录。
pub struct Catalog<F> {
    flash: F,
    /// 分区总大小
    capacity: u32,
    /// 单个 bank 大小（= capacity / 2，按扇区对齐）
    bank_size: u32,
    /// 当前在用哪个 bank
    active: u8,
    generation: u32,
    /// 下一条记录写在 bank 内的什么偏移
    write_off: u32,
    summary: Summary,
}

impl<F> Catalog<F>
where
    F: NorFlash + ReadNorFlash,
{
    /// 打开（或初始化）一个分区上的目录。
    ///
    /// 会完整扫一遍当前 bank：算出该从哪儿接着写、最新/最旧是什么时候、
    /// 上次用的哪台设备。2000 多条记录扫一遍是几十毫秒的事，
    /// 换来的是不用维护任何额外的索引结构。
    pub fn open(mut flash: F) -> Result<Self, Error<F::Error>> {
        let capacity = ReadNorFlash::capacity(&flash) as u32;
        let sector = F::ERASE_SIZE as u32;
        if capacity < sector * 4 || !capacity.is_multiple_of(sector) {
            return Err(Error::BadPartition);
        }
        // 对半分，再向下取整到扇区
        let bank_size = (capacity / 2) / sector * sector;

        // 两个 bank 谁的代数大谁有效
        let mut active = None;
        let mut generation = 0;
        for bank in 0..2u8 {
            if let Some(found) = read_bank_header(&mut flash, u32::from(bank) * bank_size)?
                && (active.is_none() || found > generation)
            {
                active = Some(bank);
                generation = found;
            }
        }

        let mut me = match active {
            Some(bank) => Self {
                flash,
                capacity,
                bank_size,
                active: bank,
                generation,
                write_off: BANK_HEADER,
                summary: Summary::default(),
            },
            None => {
                // 全新的板子（或者两个 bank 都坏了）：擦一个出来用
                let mut me = Self {
                    flash,
                    capacity,
                    bank_size,
                    active: 0,
                    generation: 1,
                    write_off: BANK_HEADER,
                    summary: Summary::default(),
                };
                me.erase_bank(0)?;
                me.write_bank_header(0, 1)?;
                return Ok(me);
            }
        };

        me.rescan()?;
        if me.summary.damaged {
            // 上次写到一半掉电了：把好记录搬到另一个 bank，坏的丢掉
            me.compact()?;
        }
        Ok(me)
    }

    pub fn summary(&self) -> &Summary {
        &self.summary
    }

    /// 当前 bank 还剩多少字节可写。
    pub fn free(&self) -> u32 {
        self.bank_size.saturating_sub(self.write_off)
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// 整个分区多大（两个 bank 加起来）。
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// 单个 bank 多大。日志里看「还能写多少」时要用它做参照。
    pub fn bank_size(&self) -> u32 {
        self.bank_size
    }

    /// 追加一条节目。调用方负责判重（用 [`Summary::newest`] 当水位）。
    pub fn append_item(&mut self, item: &Item) -> Result<(), Error<F::Error>> {
        let mut payload = [0u8; MAX_PAYLOAD];
        let n = encode_item(item, &mut payload)?;
        self.append(kind::ITEM, &payload[..n])?;

        self.summary.items += 1;
        self.summary.newest = self.summary.newest.max(item.publish_time);
        self.summary.oldest = if self.summary.oldest == 0 {
            item.publish_time
        } else {
            self.summary.oldest.min(item.publish_time)
        };
        Ok(())
    }

    /// 缓存一条节目解析出来的视频直链。
    ///
    /// 存不下也不算错 —— 这只是省一次 HTTP 请求的缓存，丢了重新解析就是。
    pub fn append_resolved(&mut self, id: u32, url: &str) -> Result<(), Error<F::Error>> {
        let mut payload = [0u8; MAX_PAYLOAD];
        payload[..4].copy_from_slice(&id.to_le_bytes());
        let bytes = url.as_bytes();
        if bytes.len() > VIDEO_URL_LEN {
            return Err(Error::TooLarge);
        }
        payload[4] = bytes.len() as u8;
        payload[5..5 + bytes.len()].copy_from_slice(bytes);
        self.append(kind::RESOLVED, &payload[..5 + bytes.len()])
    }

    /// 记住这次用的设备，下次开机直接找它，省掉一轮 SSDP 扫描。
    pub fn remember_device(&mut self, usn: &str, location: &str) -> Result<(), Error<F::Error>> {
        if self
            .summary
            .device
            .as_ref()
            .is_some_and(|d| d.usn == usn && d.location == location)
        {
            // 和上次记的一模一样，不用再写一条 —— 每次开机都写一条的话，
            // 用不了多久就会把 bank 写满，白白多整理几次
            return Ok(());
        }

        let mut payload = [0u8; MAX_PAYLOAD];
        let (u, l) = (usn.as_bytes(), location.as_bytes());
        if u.len() > USN_LEN || l.len() > LOCATION_LEN {
            return Err(Error::TooLarge);
        }
        payload[0] = u.len() as u8;
        payload[1] = l.len() as u8;
        payload[2..2 + u.len()].copy_from_slice(u);
        payload[2 + u.len()..2 + u.len() + l.len()].copy_from_slice(l);
        self.append(kind::DEVICE, &payload[..2 + u.len() + l.len()])?;

        self.summary.device = Some(RememberedDevice {
            usn: String::try_from(usn).map_err(|_| Error::TooLarge)?,
            location: String::try_from(location).map_err(|_| Error::TooLarge)?,
        });
        Ok(())
    }

    /// 标记历史节目已经补齐（以后开机就只查当天新增的了）。
    pub fn mark_backfilled(&mut self) -> Result<(), Error<F::Error>> {
        if self.summary.backfilled {
            return Ok(());
        }
        self.append(kind::STATE, &[1])?;
        self.summary.backfilled = true;
        Ok(())
    }

    /// 找某条节目缓存过的视频直链。
    ///
    /// 从头扫一遍取**最后**一条匹配的 —— 同一条节目可能被解析过多次
    /// （CDN 换地址了），后写的那条才是最新的。
    pub fn resolved_url(
        &mut self,
        id: u32,
    ) -> Result<Option<String<VIDEO_URL_LEN>>, Error<F::Error>> {
        let mut found = None;
        self.for_each(|record| {
            if let Visit::Resolved { id: rid, url } = record
                && rid == id
            {
                found = String::try_from(url).ok();
            }
            true
        })?;
        Ok(found)
    }

    /// 随机挑一部戏，把它的所有集按播出顺序理出来。
    ///
    /// `seed` 给一个随机数就行（板子上用硬件 RNG）。
    ///
    /// 挑法是「先随机挑**一集**，再把和它同名的都收上来」，而不是
    /// 「在剧目名里等概率挑一个」。这样集数多的大戏更容易被挑中 ——
    /// 家里放戏曲要的就是这个：一部 41 集的《柳君拂》比一场一集的晚会更该被放到。
    ///
    /// 全程只扫两遍 flash，内存里最多只有一部戏的集目录（64 集 × 28 字节）。
    pub fn pick_series(&mut self, seed: u32) -> Result<Option<Series>, Error<F::Error>> {
        if self.summary.items == 0 {
            return Ok(None);
        }
        let target = seed % self.summary.items;

        // 第一遍：找到第 target 条节目，记下它的剧目名
        let mut seen = 0u32;
        let mut title: Option<String<TITLE_LEN>> = None;
        self.for_each(|record| {
            if let Visit::Item { title: t, .. } = record {
                if seen == target {
                    title = String::try_from(t).ok();
                    return false;
                }
                seen += 1;
            }
            true
        })?;
        let Some(title) = title else {
            return Ok(None);
        };

        // 第二遍：把同名的都收上来
        let mut episodes: Vec<Episode, MAX_EPISODES> = Vec::new();
        self.for_each(|record| {
            if let Visit::Item {
                title: t,
                id,
                publish_time,
                date,
                slug,
            } = record
                && t == title.as_str()
                // 同一条被写进去两次（比如更新时判重没拦住）只算一次
                && !episodes.iter().any(|e| e.id == id)
            {
                let _ = episodes.push(Episode {
                    id,
                    publish_time,
                    date,
                    slug,
                });
            }
            true
        })?;

        // 按播出时间从早到晚：日志里的顺序是「先全量倒序、后面再追加新的」，
        // 本来就不是有序的，必须自己排
        episodes.sort_unstable_by_key(|e| e.publish_time);

        Ok(Some(Series { title, episodes }))
    }

    /// 从头到尾遍历日志里的记录。`f` 返回 `false` 就提前停下。
    pub fn for_each(
        &mut self,
        mut f: impl FnMut(Visit<'_>) -> bool,
    ) -> Result<(), Error<F::Error>> {
        let base = self.bank_base();
        let mut off = BANK_HEADER;
        let mut buf = [0u8; MAX_RECORD];

        while off + 8 <= self.bank_size {
            self.flash.read(base + off, &mut buf[..4])?;
            let (ty, len) = (buf[0], u16::from_le_bytes([buf[2], buf[3]]) as usize);
            if ty == kind::ERASED {
                break;
            }
            let body = align4(len) + 4;
            if len > MAX_PAYLOAD || off + 4 + body as u32 > self.bank_size {
                break;
            }
            self.flash.read(base + off + 4, &mut buf[4..4 + body])?;
            if !crc_ok(&buf[..4 + body], len) {
                break;
            }
            if let Some(visit) = decode(ty, &buf[4..4 + len])
                && !f(visit)
            {
                return Ok(());
            }
            off += 4 + body as u32;
        }
        Ok(())
    }

    /// 把当前 bank 里还有用的记录搬到另一个 bank，回收空间。
    ///
    /// 顺序很讲究：**先擦目标 bank，搬完了最后才写 bank 头**。
    /// 中途掉电的话新 bank 没有头 → 无效 → 老 bank 还是原来那个老 bank，
    /// 数据一条不少。
    ///
    /// 搬运时的取舍：
    ///
    /// * 视频直链缓存（`RESOLVED`）全部丢掉 —— 它只是省一次 HTTP 请求，
    ///   保留它要多一套「哪些还有用」的判断，不值当；
    /// * 设备和状态直接从内存里重写一条，放在最前面，永远不会因为空间不够被挤掉；
    /// * 节目实在装不下时，**丢最老的**。真按现在的数据量（2291 条 110KB）
    ///   和 512KB 一个 bank 算，这条路二十年内都走不到，
    ///   但它必须存在：不然 bank 满了之后设备就再也存不进新节目了。
    pub fn compact(&mut self) -> Result<(), Error<F::Error>> {
        let src = self.active;
        let dst = 1 - src;
        let dst_base = u32::from(dst) * self.bank_size;
        let usable = self.bank_size - BANK_HEADER;

        // 先算：全部搬过去装得下吗？装不下就定一个发布时间的门槛，只留新的
        let (item_bytes, threshold) = self.plan_compaction(usable)?;
        let _ = item_bytes;

        self.erase_bank(dst)?;

        let mut write_off = BANK_HEADER;
        let mut buf = [0u8; MAX_RECORD];

        // ① 设备和状态先落地，它们只占一百多字节，但丢了就要重新扫一轮 SSDP
        if let Some(device) = self.summary.device.clone() {
            let mut payload = [0u8; MAX_PAYLOAD];
            let (u, l) = (device.usn.as_bytes(), device.location.as_bytes());
            payload[0] = u.len() as u8;
            payload[1] = l.len() as u8;
            payload[2..2 + u.len()].copy_from_slice(u);
            payload[2 + u.len()..2 + u.len() + l.len()].copy_from_slice(l);
            let n = frame(kind::DEVICE, &payload[..2 + u.len() + l.len()], &mut buf);
            self.flash.write(dst_base + write_off, &buf[..n])?;
            write_off += n as u32;
        }
        if self.summary.backfilled {
            let n = frame(kind::STATE, &[1], &mut buf);
            self.flash.write(dst_base + write_off, &buf[..n])?;
            write_off += n as u32;
        }

        // ② 逐条搬节目
        let mut moved_items = 0u32;
        let (mut newest, mut oldest) = (0u32, 0u32);
        let src_base = u32::from(src) * self.bank_size;
        let mut off = BANK_HEADER;

        while off + 8 <= self.bank_size {
            self.flash.read(src_base + off, &mut buf[..4])?;
            let (ty, len) = (buf[0], u16::from_le_bytes([buf[2], buf[3]]) as usize);
            if ty == kind::ERASED {
                break;
            }
            let body = align4(len) + 4;
            if len > MAX_PAYLOAD || off + 4 + body as u32 > self.bank_size {
                break;
            }
            self.flash.read(src_base + off + 4, &mut buf[4..4 + body])?;
            if !crc_ok(&buf[..4 + body], len) {
                // 坏记录（掉电留下的半截）：到此为止，后面的一律不要
                break;
            }
            off += 4 + body as u32;

            // 只搬节目，其它类型要么已经重写过（设备/状态），要么是缓存（直链）
            if ty != kind::ITEM {
                continue;
            }
            let Some(Visit::Item { publish_time, .. }) = decode(ty, &buf[4..4 + len]) else {
                continue;
            };
            if publish_time < threshold {
                continue; // 太老，丢掉
            }
            let total = 4 + body;
            if write_off + total as u32 > self.bank_size {
                break; // 真装不下了，剩下的更老，一起丢
            }
            self.flash.write(dst_base + write_off, &buf[..total])?;
            write_off += total as u32;
            moved_items += 1;
            newest = newest.max(publish_time);
            oldest = if oldest == 0 {
                publish_time
            } else {
                oldest.min(publish_time)
            };
        }

        // ③ 到这一步数据已经全在新 bank 里了，最后落这一笔头才让它生效
        let generation = self.generation + 1;
        self.write_bank_header(dst, generation)?;
        self.active = dst;
        self.generation = generation;
        self.write_off = write_off;
        self.summary.damaged = false;
        self.summary.items = moved_items;
        self.summary.newest = newest;
        self.summary.oldest = oldest;
        Ok(())
    }

    /// 整理之前先估一估：全搬过去要多少字节？装不下的话从哪个发布时间往上留？
    ///
    /// 算门槛用的是**等距抽样**（最多 128 个样本）而不是把 2291 个时间戳
    /// 全读进内存排序 —— 板子上没有那么多 RAM，而这只是缓存淘汰策略，
    /// 差几条无所谓。
    fn plan_compaction(&mut self, usable: u32) -> Result<(u32, u32), Error<F::Error>> {
        const SAMPLES: usize = 128;

        let items = self.summary.items;
        if items == 0 {
            return Ok((0, 0));
        }

        let mut bytes = 0u32;
        let mut sample = [0u32; SAMPLES];
        let mut taken = 0usize;
        let step = (items as usize).div_ceil(SAMPLES).max(1);
        let mut index = 0usize;

        self.for_each(|record| {
            if let Visit::Item {
                publish_time,
                title,
                ..
            } = record
            {
                bytes += (4 + align4(12 + SLUG_LEN + title.len()) + 4) as u32;
                if index.is_multiple_of(step) && taken < SAMPLES {
                    sample[taken] = publish_time;
                    taken += 1;
                }
                index += 1;
            }
            true
        })?;

        // 留出 1/4 空间，免得刚整理完又立刻要整理
        let budget = usable / 4 * 3;
        if bytes <= budget || taken == 0 {
            return Ok((bytes, 0));
        }

        // 要丢掉的比例 = 1 - budget/bytes
        let sample = &mut sample[..taken];
        sample.sort_unstable();
        let drop_ratio_num = bytes.saturating_sub(budget) as u64;
        let cut = (drop_ratio_num * taken as u64 / bytes as u64) as usize;
        Ok((bytes, sample[cut.min(taken - 1)]))
    }

    /// 重新扫一遍当前 bank，重建 [`Summary`]。
    pub fn rescan(&mut self) -> Result<(), Error<F::Error>> {
        let base = self.bank_base();
        let mut off = BANK_HEADER;
        let mut buf = [0u8; MAX_RECORD];
        let mut summary = Summary::default();

        while off + 8 <= self.bank_size {
            self.flash.read(base + off, &mut buf[..4])?;
            let (ty, len) = (buf[0], u16::from_le_bytes([buf[2], buf[3]]) as usize);
            if ty == kind::ERASED {
                // 剩下的必须全是擦除态，否则说明中间有一段垃圾
                break;
            }
            let body = align4(len) + 4;
            if len > MAX_PAYLOAD || off + 4 + body as u32 > self.bank_size {
                summary.damaged = true;
                break;
            }
            self.flash.read(base + off + 4, &mut buf[4..4 + body])?;
            if !crc_ok(&buf[..4 + body], len) {
                // 上次写到一半掉电，或者别的原因坏了一条
                summary.damaged = true;
                break;
            }

            match decode(ty, &buf[4..4 + len]) {
                Some(Visit::Item { publish_time, .. }) => {
                    summary.items += 1;
                    summary.newest = summary.newest.max(publish_time);
                    summary.oldest = if summary.oldest == 0 {
                        publish_time
                    } else {
                        summary.oldest.min(publish_time)
                    };
                }
                Some(Visit::Device { usn, location }) => {
                    summary.device = Some(RememberedDevice {
                        usn: String::try_from(usn).unwrap_or_default(),
                        location: String::try_from(location).unwrap_or_default(),
                    });
                }
                Some(Visit::State { backfilled }) => summary.backfilled = backfilled,
                _ => {}
            }
            off += 4 + body as u32;
        }

        self.write_off = off;
        self.summary = summary;
        Ok(())
    }

    /// 把整个分区清空（换格式、或者用户要求重来时用）。
    pub fn erase_all(&mut self) -> Result<(), Error<F::Error>> {
        self.erase_bank(0)?;
        self.erase_bank(1)?;
        self.active = 0;
        self.generation = 1;
        self.write_off = BANK_HEADER;
        self.summary = Summary::default();
        self.write_bank_header(0, 1)
    }

    /// 交还底层 flash。
    pub fn release(self) -> F {
        self.flash
    }

    fn bank_base(&self) -> u32 {
        u32::from(self.active) * self.bank_size
    }

    fn erase_bank(&mut self, bank: u8) -> Result<(), Error<F::Error>> {
        let base = u32::from(bank) * self.bank_size;
        self.flash.erase(base, base + self.bank_size)?;
        Ok(())
    }

    fn write_bank_header(&mut self, bank: u8, generation: u32) -> Result<(), Error<F::Error>> {
        let mut head = [0xFFu8; BANK_HEADER as usize];
        head[..4].copy_from_slice(&MAGIC);
        head[4..8].copy_from_slice(&generation.to_le_bytes());
        let crc = crc16(&head[..8]);
        head[8..10].copy_from_slice(&crc.to_le_bytes());
        // 10..16 保留，留 0xFF 方便以后加字段（0xFF 是可以再写下去的）
        let base = u32::from(bank) * self.bank_size;
        self.flash.write(base, &head)?;
        Ok(())
    }

    /// 真正往日志末尾接一条。放不下就先整理，整理完还放不下才报错。
    fn append(&mut self, ty: u8, payload: &[u8]) -> Result<(), Error<F::Error>> {
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::TooLarge);
        }
        let total = 4 + align4(payload.len()) + 4;

        if self.write_off + total as u32 > self.bank_size {
            self.compact()?;
            if self.write_off + total as u32 > self.bank_size {
                return Err(Error::Full);
            }
        }

        let mut buf = [0u8; MAX_RECORD];
        let n = frame(ty, payload, &mut buf);
        debug_assert_eq!(n, total);

        let base = self.bank_base();
        self.flash.write(base + self.write_off, &buf[..n])?;
        self.write_off += n as u32;
        Ok(())
    }
}

/// 遍历时看到的一条记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visit<'a> {
    Item {
        id: u32,
        publish_time: u32,
        date: Date,
        slug: [u8; SLUG_LEN],
        title: &'a str,
    },
    Resolved {
        id: u32,
        url: &'a str,
    },
    Device {
        usn: &'a str,
        location: &'a str,
    },
    State {
        backfilled: bool,
    },
}

/// 把负载装进一条完整记录（头 + 负载 + CRC + 补齐），返回总字节数。
fn frame(ty: u8, payload: &[u8], out: &mut [u8; MAX_RECORD]) -> usize {
    let total = 4 + align4(payload.len()) + 4;
    out[..total].fill(0);
    out[0] = ty;
    out[1] = 0; // 标志位，留给以后
    out[2..4].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    out[4..4 + payload.len()].copy_from_slice(payload);
    // 负载补齐的部分保持 0，CRC 只算到真实长度
    let crc_at = 4 + align4(payload.len());
    let crc = crc16(&out[..4 + payload.len()]);
    out[crc_at..crc_at + 2].copy_from_slice(&crc.to_le_bytes());
    total
}

fn encode_item<E>(item: &Item, out: &mut [u8]) -> Result<usize, Error<E>> {
    let title = item.title.as_bytes();
    let n = 4 + 4 + 3 + SLUG_LEN + 1 + title.len();
    if n > MAX_PAYLOAD || title.len() > u8::MAX as usize {
        return Err(Error::TooLarge);
    }
    out[0..4].copy_from_slice(&item.id.to_le_bytes());
    out[4..8].copy_from_slice(&item.publish_time.to_le_bytes());
    out[8] = item.date.year;
    out[9] = item.date.month;
    out[10] = item.date.day;
    out[11..11 + SLUG_LEN].copy_from_slice(&item.slug);
    out[11 + SLUG_LEN] = title.len() as u8;
    out[12 + SLUG_LEN..12 + SLUG_LEN + title.len()].copy_from_slice(title);
    Ok(n)
}

fn decode(ty: u8, payload: &[u8]) -> Option<Visit<'_>> {
    match ty {
        kind::ITEM => {
            if payload.len() < 12 + SLUG_LEN {
                return None;
            }
            let title_len = payload[11 + SLUG_LEN] as usize;
            let title = payload.get(12 + SLUG_LEN..12 + SLUG_LEN + title_len)?;
            let mut slug = [0u8; SLUG_LEN];
            slug.copy_from_slice(&payload[11..11 + SLUG_LEN]);
            Some(Visit::Item {
                id: u32::from_le_bytes(payload[0..4].try_into().ok()?),
                publish_time: u32::from_le_bytes(payload[4..8].try_into().ok()?),
                date: Date {
                    year: payload[8],
                    month: payload[9],
                    day: payload[10],
                },
                slug,
                title: core::str::from_utf8(title).ok()?,
            })
        }
        kind::RESOLVED => {
            let len = *payload.get(4)? as usize;
            Some(Visit::Resolved {
                id: u32::from_le_bytes(payload[0..4].try_into().ok()?),
                url: core::str::from_utf8(payload.get(5..5 + len)?).ok()?,
            })
        }
        kind::DEVICE => {
            let (ulen, llen) = (*payload.first()? as usize, *payload.get(1)? as usize);
            Some(Visit::Device {
                usn: core::str::from_utf8(payload.get(2..2 + ulen)?).ok()?,
                location: core::str::from_utf8(payload.get(2 + ulen..2 + ulen + llen)?).ok()?,
            })
        }
        kind::STATE => Some(Visit::State {
            backfilled: *payload.first()? != 0,
        }),
        _ => None, // 以后加的新类型，老固件跳过就好
    }
}

fn read_bank_header<F: ReadNorFlash>(
    flash: &mut F,
    base: u32,
) -> Result<Option<u32>, Error<F::Error>> {
    let mut head = [0u8; BANK_HEADER as usize];
    flash.read(base, &mut head)?;
    if head[..4] != MAGIC {
        return Ok(None);
    }
    let crc = u16::from_le_bytes([head[8], head[9]]);
    if crc != crc16(&head[..8]) {
        return Ok(None);
    }
    Ok(Some(u32::from_le_bytes([
        head[4], head[5], head[6], head[7],
    ])))
}

fn crc_ok(record: &[u8], len: usize) -> bool {
    let at = 4 + align4(len);
    if record.len() < at + 2 {
        return false;
    }
    let stored = u16::from_le_bytes([record[at], record[at + 1]]);
    stored == crc16(&record[..4 + len])
}

const fn align4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// CRC-16/CCITT-FALSE。
///
/// 用它不是为了防篡改，是为了识别「写到一半掉电」留下的半截记录 ——
/// 那种记录的长度字段和内容对不上，CRC 一定过不了。
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_能认出被改过一个位的数据() {
        let a = crc16(b"hello world");
        let mut data = *b"hello world";
        data[3] ^= 0x01;
        assert_ne!(a, crc16(&data));
    }

    #[test]
    fn 对齐算得对() {
        assert_eq!(align4(0), 0);
        assert_eq!(align4(1), 4);
        assert_eq!(align4(4), 4);
        assert_eq!(align4(5), 8);
    }

    #[test]
    fn 节目记录编解码是一对一的() {
        let item = Item {
            id: 670419,
            publish_time: 1788336662,
            date: Date {
                year: 26,
                month: 9,
                day: 2,
            },
            slug: *b"b2071f1bd886c153",
            title: String::try_from("大名春秋").unwrap(),
        };
        let mut buf = [0u8; MAX_PAYLOAD];
        let n = encode_item::<()>(&item, &mut buf).unwrap();

        let Some(Visit::Item {
            id,
            publish_time,
            date,
            slug,
            title,
        }) = decode(kind::ITEM, &buf[..n])
        else {
            panic!("解不出来");
        };
        assert_eq!(id, item.id);
        assert_eq!(publish_time, item.publish_time);
        assert_eq!(date, item.date);
        assert_eq!(slug, item.slug);
        assert_eq!(title, "大名春秋");
    }

    #[test]
    fn 一条节目记录多大() {
        // 平均剧目名 12 字节 → 负载 40 → 整条 48 字节。
        // 2291 条全存下来大约 110KB，这决定了分区要开多大
        let item = Item {
            id: 1,
            publish_time: 2,
            date: Date {
                year: 26,
                month: 1,
                day: 1,
            },
            slug: *b"0123456789abcdef",
            title: String::try_from("大名春秋").unwrap(),
        };
        let mut buf = [0u8; MAX_PAYLOAD];
        let n = encode_item::<()>(&item, &mut buf).unwrap();
        let total = 4 + align4(n) + 4;
        assert_eq!(total, 48, "记录大小变了的话要重新算分区尺寸");
    }

    #[test]
    fn 不认识的记录类型被跳过() {
        // 以后固件加了新记录类型，老固件读到不能崩
        assert_eq!(decode(0x7F, b"whatever"), None);
    }

    #[test]
    fn 半截的负载解不出来而不是_panic() {
        assert_eq!(decode(kind::ITEM, b"short"), None);
        assert_eq!(decode(kind::RESOLVED, b"\x01\x02\x03\x04\xff"), None);
        assert_eq!(decode(kind::DEVICE, b"\xff\xff"), None);
    }
}
