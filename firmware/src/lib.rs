//! ESP32-S3 这一侧的适配层。
//!
//! 真正的投屏逻辑在 `xi-cast-core` 里，这里只做两件板子特有的事：
//!
//! * [`net`] —— 把 embassy-net 接到核心库的 [`xi_cast_core::net::Net`] 上；
//! * [`storage`] —— 从分区表里找到那块存节目的 flash 区域。

#![no_std]

pub mod net;
pub mod storage;

/// 把一个值放进 `'static` 存储里。
///
/// embassy 的 socket、网络栈资源都要求 `'static`，而它们又不能是 `const`
/// （要在运行时构造），所以只能走 `StaticCell`。
#[macro_export]
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write(($val))
    }};
}
