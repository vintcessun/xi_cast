//! 从分区表里找到存节目的那块 flash。
//!
//! # 为什么不写死一个地址
//!
//! 写死偏移量的话，哪天固件变大越过了那个地址，就会**静默地**把自己的代码
//! 当成节目数据覆盖掉。分区表是 ESP-IDF 引导器本来就要读的东西，
//! 按名字找过去既安全又能在改布局时自动跟上。
//!
//! # ESP32-S3 的 cache 问题
//!
//! 这块是最容易在真机上翻车、而在电脑上永远测不出来的地方，说清楚：
//!
//! 代码是**从 flash 上直接执行**的（XIP，靠指令 cache 加速）。而写 flash
//! 的时候，cache 必须先关掉 —— 关掉的那一小段时间里，任何「从 flash 取指令
//! 或取数据」的行为都会当场炸掉。esp-storage 的处理办法是把写操作那几十行
//! 放进 IRAM（片上 RAM）、并且关中断，所以**写 flash 的那个核**是安全的。
//!
//! 但另一个核不安全。ESP32-S3 是双核，如果第二个核正在跑（而且正从 flash
//! 取指令），一次 flash 写就能让它跑飞。esp-storage 默认的策略是
//! [`MultiCoreStrategy::Error`]：发现另一个核在跑就**直接让写失败**，
//! 而不是赌一把。
//!
//! 这个固件的做法是**根本不启用第二个核**：esp-rtos 只跑在核 0 上，
//! 所有任务（WiFi、网络栈、投屏）都是一个 executor 上的 async 任务。
//! 于是默认策略就是对的，一次 flash 写既不会失败也不会伤到谁。
//!
//! 还有两个配套的讲究：
//!
//! * **写要小块、要短。** cache 关着的时候 WiFi 收不了包。所以目录是
//!   「一条记录一次写」（几十字节），而不是攒一大批一起写。
//! * **第一次开机的整块擦除放在联网之前做。** 擦一个 256KB 的 bank 是
//!   64 个扇区，累计几百毫秒 cache 是关着的；这时候还没连 WiFi，
//!   没有连接会因此断掉。
//!
//! [`MultiCoreStrategy::Error`]: https://docs.rs/esp-storage

use esp_bootloader_esp_idf::partitions::{
    self, PARTITION_TABLE_MAX_LEN, PartitionEntry, PartitionTable,
};
use esp_storage::FlashStorage;

/// 分区表里那块数据分区叫什么（见 `firmware/partitions.csv`）。
pub const PARTITION_LABEL: &str = "xicast";

/// 存节目的那块 flash。
pub type CatalogFlash = partitions::FlashRegion<'static, FlashStorage<'static>>;

#[derive(Debug)]
pub enum Error {
    /// 分区表读不出来（没按 `partitions.csv` 烧过？）
    TableUnreadable,
    /// 分区表里没有叫 `xicast` 的分区
    NotFound,
}

/// 按名字找到数据分区，返回一块只覆盖它、偏移量已经算好的存储。
///
/// 缓冲区要 `'static`：分区表项借用着它，而这块存储要用到关机为止。
pub fn open(
    flash: &'static mut FlashStorage<'static>,
    table_buf: &'static mut [u8; PARTITION_TABLE_MAX_LEN],
) -> Result<CatalogFlash, Error> {
    let table: PartitionTable<'static> =
        partitions::read_partition_table(flash, table_buf).map_err(|_| Error::TableUnreadable)?;

    let entry: PartitionEntry<'static> = table
        .iter()
        .find(|e| e.label_as_str() == PARTITION_LABEL)
        .ok_or(Error::NotFound)?;

    log::info!(
        "找到数据分区 {}：偏移 {:#x}，大小 {} 字节",
        PARTITION_LABEL,
        entry.offset(),
        entry.len()
    );

    Ok(entry.as_embedded_storage(flash))
}
