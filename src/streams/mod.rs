#[cfg(target_os = "linux")]
mod v4l_h264;

#[cfg(target_os = "linux")]
pub use v4l_h264::*;

mod stdin_h264;
pub use stdin_h264::*;
