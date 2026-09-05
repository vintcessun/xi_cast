//! 电脑上的模拟环境：假电视、假 xmtv 服务器、假 flash。
//!
//! 目的很直接 —— **开发板还没到货，但功能要先测完**。固件里真正的逻辑都在
//! `xi-cast-core` 里，这个 crate 给它接上电脑这边的网络和存储，
//! 于是同一份代码可以在 `cargo test` 里跑完整条投屏链路。

pub mod flash;
pub mod mock_tv;
pub mod mock_xmtv;
pub mod net;
