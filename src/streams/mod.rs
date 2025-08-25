#[cfg(all(target_os = "linux", feature = "v4l"))]
mod v4l_h264;

#[cfg(all(target_os = "linux", feature = "v4l"))]
pub use v4l_h264::*;

mod stdin_h264;
pub use stdin_h264::*;
