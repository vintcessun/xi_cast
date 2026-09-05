//! 一块**较真**的假 NOR flash。
//!
//! 电脑上测 flash 逻辑最容易自欺欺人的地方，是把它当成普通内存写：
//! 那样测出来全绿，一上板子就烂。真实的 NOR flash 有三条硬规矩，
//! 这里全部强制执行，违反了直接报错：
//!
//! 1. **写只能把 1 变 0。** 想把 0 变回 1 必须先擦除。
//!    往一段没擦过的地方再写一遍，得到的是两次数据按位与的结果 —— 一堆垃圾。
//! 2. **擦除以扇区为单位**（ESP32 是 4096 字节），起止地址都要对齐。
//! 3. **写有最小粒度**（ESP32 是 4 字节），偏移和长度都要对齐。
//!
//! 另外还能模拟掉电：写到一半断电，前半截进了 flash、后半截没有。
//! 「一上电就投屏」这种设备随时可能被人直接拔电源，这条路径必须测。

use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

/// ESP32-S3 的 flash 参数。
pub const SECTOR: usize = 4096;
pub const WRITE_GRANULARITY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 越界
    OutOfBounds,
    /// 偏移或长度没对齐
    NotAligned,
    /// 往没擦除的地方写（把 0 写成 1）—— 真实 flash 上这是静默的数据损坏，
    /// 这里让它响一声
    NotErased { offset: u32 },
    /// 模拟掉电
    PowerLoss,
}

impl NorFlashError for Error {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            Error::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            Error::NotAligned => NorFlashErrorKind::NotAligned,
            _ => NorFlashErrorKind::Other,
        }
    }
}

/// 假 flash。`capacity` 必须是扇区的整数倍。
pub struct MockFlash {
    data: Vec<u8>,
    /// 还能执行多少次写/擦操作，到 0 就开始报 [`Error::PowerLoss`]。
    /// `None` 表示电源稳得很。
    budget: Option<usize>,
    /// 下一次写只写前多少字节然后「断电」，用来制造半截记录
    tear_at: Option<usize>,
    pub erase_count: usize,
    pub write_count: usize,
    pub written_bytes: usize,
}

impl MockFlash {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_multiple_of(SECTOR), "容量必须是扇区的整数倍");
        Self {
            data: vec![0xFF; capacity],
            budget: None,
            tear_at: None,
            erase_count: 0,
            write_count: 0,
            written_bytes: 0,
        }
    }

    /// 直接看某一段内容（测试断言用，不走 flash 规则）。
    pub fn peek(&self, offset: usize, len: usize) -> &[u8] {
        &self.data[offset..offset + len]
    }

    pub fn raw(&self) -> &[u8] {
        &self.data
    }

    /// 把整块 flash 的内容克隆出来，用来模拟「断电重启后还是这块 flash」。
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.clone()
    }

    pub fn from_snapshot(data: Vec<u8>) -> Self {
        assert!(data.len().is_multiple_of(SECTOR));
        Self {
            data,
            budget: None,
            tear_at: None,
            erase_count: 0,
            write_count: 0,
            written_bytes: 0,
        }
    }

    /// 再执行 `ops` 次写/擦操作之后掉电。
    pub fn power_loss_after(&mut self, ops: usize) {
        self.budget = Some(ops);
    }

    /// 下一次写只写进前 `bytes` 个字节，然后掉电 —— 这就是「半截记录」。
    pub fn tear_next_write(&mut self, bytes: usize) {
        self.tear_at = Some(bytes);
    }

    pub fn power_restored(&mut self) {
        self.budget = None;
        self.tear_at = None;
    }

    fn spend(&mut self) -> Result<(), Error> {
        match self.budget.as_mut() {
            Some(0) => Err(Error::PowerLoss),
            Some(n) => {
                *n -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl ErrorType for MockFlash {
    type Error = Error;
}

impl ReadNorFlash for MockFlash {
    // esp-storage 默认也是 4：不开 `bytewise-read` 特性的话，读同样要对齐。
    // 这里按最严的来，代码在任何配置下都能跑。
    const READ_SIZE: usize = 4;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        if !(offset as usize).is_multiple_of(Self::READ_SIZE)
            || !bytes.len().is_multiple_of(Self::READ_SIZE)
        {
            return Err(Error::NotAligned);
        }
        let start = offset as usize;
        let end = start.checked_add(bytes.len()).ok_or(Error::OutOfBounds)?;
        if end > self.data.len() {
            return Err(Error::OutOfBounds);
        }
        bytes.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.data.len()
    }
}

impl NorFlash for MockFlash {
    const WRITE_SIZE: usize = WRITE_GRANULARITY;
    const ERASE_SIZE: usize = SECTOR;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        let (from, to) = (from as usize, to as usize);
        if from % SECTOR != 0 || to % SECTOR != 0 {
            return Err(Error::NotAligned);
        }
        if to > self.data.len() || from > to {
            return Err(Error::OutOfBounds);
        }
        self.spend()?;
        self.erase_count += 1;
        self.data[from..to].fill(0xFF);
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        let start = offset as usize;
        if !start.is_multiple_of(WRITE_GRANULARITY)
            || !bytes.len().is_multiple_of(WRITE_GRANULARITY)
        {
            return Err(Error::NotAligned);
        }
        let end = start.checked_add(bytes.len()).ok_or(Error::OutOfBounds)?;
        if end > self.data.len() {
            return Err(Error::OutOfBounds);
        }
        self.spend()?;

        // 掉电撕裂：只写进前一部分
        let (take, tear) = match self.tear_at.take() {
            Some(n) if n < bytes.len() => (n - n % WRITE_GRANULARITY, true),
            _ => (bytes.len(), false),
        };

        for (i, &b) in bytes[..take].iter().enumerate() {
            let old = self.data[start + i];
            // NOR 的物理规则：写入等于按位与
            let new = old & b;
            if new != b {
                return Err(Error::NotErased {
                    offset: (start + i) as u32,
                });
            }
            self.data[start + i] = new;
        }
        self.write_count += 1;
        self.written_bytes += take;

        if tear {
            return Err(Error::PowerLoss);
        }
        Ok(())
    }
}

/// 落在磁盘文件上的假 flash：模拟器重启之后数据还在，
/// 和板子上「拔电再插上，flash 里的东西一条不少」是一个意思。
///
/// 每次写完就整份刷回文件，慢，但语义最接近真板子：只要函数返回了 `Ok`，
/// 数据就一定已经落地。
pub struct FileFlash {
    inner: MockFlash,
    path: std::path::PathBuf,
}

impl FileFlash {
    pub fn open(path: impl Into<std::path::PathBuf>, capacity: usize) -> std::io::Result<Self> {
        let path = path.into();
        let inner = match std::fs::read(&path) {
            Ok(data) if data.len() == capacity => MockFlash::from_snapshot(data),
            _ => MockFlash::new(capacity),
        };
        Ok(Self { inner, path })
    }

    fn flush(&self) -> Result<(), Error> {
        std::fs::write(&self.path, self.inner.raw()).map_err(|_| Error::OutOfBounds)
    }
}

impl ErrorType for FileFlash {
    type Error = Error;
}

impl ReadNorFlash for FileFlash {
    const READ_SIZE: usize = MockFlash::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.inner.read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        ReadNorFlash::capacity(&self.inner)
    }
}

impl NorFlash for FileFlash {
    const WRITE_SIZE: usize = MockFlash::WRITE_SIZE;
    const ERASE_SIZE: usize = MockFlash::ERASE_SIZE;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.inner.erase(from, to)?;
        self.flush()
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.inner.write(offset, bytes)?;
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 新的_flash_全是_ff() {
        let f = MockFlash::new(SECTOR);
        assert!(f.raw().iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn 写过的地方不能再写别的() {
        let mut f = MockFlash::new(SECTOR);
        f.write(0, b"AAAA").unwrap();
        // 真板子上这里会静默地写出 A&B 的按位与结果
        assert!(matches!(f.write(0, b"BBBB"), Err(Error::NotErased { .. })));
    }

    #[test]
    fn 只把_1_变_0_是允许的() {
        let mut f = MockFlash::new(SECTOR);
        f.write(0, &[0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        f.write(0, &[0x0F, 0xFF, 0xFF, 0xFF]).unwrap();
        assert_eq!(f.peek(0, 1), &[0x0F]);
    }

    #[test]
    fn 擦除之后可以重写() {
        let mut f = MockFlash::new(SECTOR);
        f.write(0, b"AAAA").unwrap();
        f.erase(0, SECTOR as u32).unwrap();
        f.write(0, b"BBBB").unwrap();
        assert_eq!(f.peek(0, 4), b"BBBB");
    }

    #[test]
    fn 不对齐的操作被拒绝() {
        let mut f = MockFlash::new(SECTOR * 2);
        assert_eq!(f.write(1, b"AAAA"), Err(Error::NotAligned));
        assert_eq!(f.write(0, b"AAA"), Err(Error::NotAligned));
        assert_eq!(f.erase(1, SECTOR as u32), Err(Error::NotAligned));
        assert_eq!(f.erase(0, 100), Err(Error::NotAligned));
    }

    #[test]
    fn 越界被拒绝() {
        let mut f = MockFlash::new(SECTOR);
        assert_eq!(f.write(SECTOR as u32, b"AAAA"), Err(Error::OutOfBounds));
        let mut buf = [0u8; 8];
        assert_eq!(f.read(SECTOR as u32 - 4, &mut buf), Err(Error::OutOfBounds));
    }

    #[test]
    fn 写到一半掉电() {
        let mut f = MockFlash::new(SECTOR);
        f.tear_next_write(4);
        assert_eq!(f.write(0, b"AAAABBBB"), Err(Error::PowerLoss));
        assert_eq!(
            f.peek(0, 8),
            b"AAAA\xff\xff\xff\xff",
            "前半截应该已经进去了"
        );
    }

    #[test]
    fn 掉电之后所有写操作都失败() {
        let mut f = MockFlash::new(SECTOR);
        f.power_loss_after(1);
        f.write(0, b"AAAA").unwrap();
        assert_eq!(f.write(4, b"BBBB"), Err(Error::PowerLoss));
        assert_eq!(f.erase(0, SECTOR as u32), Err(Error::PowerLoss));
    }
}
