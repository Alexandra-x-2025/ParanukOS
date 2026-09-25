//! 最小串口日志设施。
//!
//! 交接之后固件控制台不再可用（接口文档 §6.2），因此日志只有串口这一个出口。
//! 格式统一为 `[kernel] ` + 可选的级别标签 + 内容，便于用日志做测试断言。

use core::fmt;

use crate::serial::Serial;

/// 日志级别。`Info` 不加标签，保持与既有断言兼容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 常规信息。
    Info,
    /// 警告：不致命但需要留意。
    Warn,
    /// 错误：功能无法继续。
    Error,
}

impl Level {
    const fn tag(self) -> &'static str {
        match self {
            Self::Info => "",
            Self::Warn => "[WARN] ",
            Self::Error => "[ERROR] ",
        }
    }
}

/// 输出一行日志。
pub fn log(level: Level, args: fmt::Arguments<'_>) {
    use core::fmt::Write as _;
    let mut out = Serial;
    let _ = writeln!(&mut out, "[kernel] {}{}", level.tag(), args);
}

macro_rules! kinfo {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Info, format_args!($($arg)*))
    };
}

macro_rules! kwarn {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Warn, format_args!($($arg)*))
    };
}

macro_rules! kerror {
    ($($arg:tt)*) => {
        $crate::logging::log($crate::logging::Level::Error, format_args!($($arg)*))
    };
}

pub(crate) use {kerror, kinfo, kwarn};
