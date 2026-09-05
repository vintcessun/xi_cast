//! 一层薄薄的日志转接。
//!
//! 同一份逻辑在两个地方跑，日志出口不一样：
//!
//! * 固件里是 `defmt`（通过 RTT 打到调试器 / `probe-rs`）；
//! * 电脑上是 `log`（`env_logger` 打到终端）。
//!
//! 两个特性都不开时，日志语句会被完全编译掉（参数也不会被求值），
//! 固件里不想要日志的时候就是零开销。

#[macro_export]
macro_rules! trace_info {
    ($($arg:tt)*) => {{
        #[cfg(feature = "defmt")]
        ::defmt::info!($($arg)*);
        #[cfg(all(feature = "log", not(feature = "defmt")))]
        ::log::info!($($arg)*);
        #[cfg(not(any(feature = "defmt", feature = "log")))]
        let _ = ::core::format_args!($($arg)*);
    }};
}

#[macro_export]
macro_rules! trace_warn {
    ($($arg:tt)*) => {{
        #[cfg(feature = "defmt")]
        ::defmt::warn!($($arg)*);
        #[cfg(all(feature = "log", not(feature = "defmt")))]
        ::log::warn!($($arg)*);
        #[cfg(not(any(feature = "defmt", feature = "log")))]
        let _ = ::core::format_args!($($arg)*);
    }};
}

#[macro_export]
macro_rules! trace_debug {
    ($($arg:tt)*) => {{
        #[cfg(feature = "defmt")]
        ::defmt::debug!($($arg)*);
        #[cfg(all(feature = "log", not(feature = "defmt")))]
        ::log::debug!($($arg)*);
        #[cfg(not(any(feature = "defmt", feature = "log")))]
        let _ = ::core::format_args!($($arg)*);
    }};
}
