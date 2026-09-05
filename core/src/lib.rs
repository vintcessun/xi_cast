//! xi_cast 的「大脑」：投屏协议、节目数据解析、flash 里的追加式目录。
//!
//! 这个 crate 是 `no_std` 且**不分配内存**的，所有缓冲区由调用方给。
//! 它同时被两个地方用：
//!
//! * `firmware/` —— 真正烧进 ESP32-S3 的固件；
//! * `sim/` —— 在电脑上跑的模拟器和集成测试。
//!
//! 这么分是为了让「没有开发板也能把功能测完」成立：协议拼错一个字节、
//! chunked 少解一个分片、flash 掉电少写一条记录，这些都在电脑上暴露出来，
//! 而不是等板子到货了对着串口日志猜。
//!
//! 模块之间的关系：
//!
//! ```text
//!   ssdp  ──找到设备地址──▶  upnp  ──控制入口──▶  soap  ──▶ 电视
//!   xmtv  ──节目列表────▶  store ──随机挑一部──▶  ↑
//!                          （flash 追加式日志）
//! ```

#![cfg_attr(not(test), no_std)]
#![deny(clippy::mem_forget)]

pub mod app;
pub mod http;
pub mod net;
pub mod soap;
pub mod ssdp;
pub mod store;
#[macro_use]
pub mod trace;
pub mod upnp;
pub mod url;
pub mod xmtv;
