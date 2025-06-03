mod v4l_h264;
mod mac_h264;

#[cfg(target_os = "linux")]
pub use v4l_h264::*;

#[cfg(target_os = "macos")]
pub use mac_h264::*;