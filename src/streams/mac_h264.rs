#![cfg(target_os = "macos")]

//! Capture raw pixel frames from macOS AVFoundation via FFmpeg CLI,
//! encode via `H264Encoder`, and send Annex B NAL bytes downstream.

use anyhow::Result;
use bytes::BytesMut;
use crate::encoders::{EncoderConfig, EncoderType, FfmpegOptions, H264Encoder, InputType};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Configuration for macOS AVFoundation→H.264.
///
/// This spawns:
///   ffmpeg \
///     -f avfoundation \
///     -pixel_format {pix_fmt} \
///     -video_size {input_width}x{input_height} \
///     -framerate {framerate} \
///     -i {device_index} \
///     -f rawvideo pipe:1
///
/// Then we read exactly `frame_size` bytes per raw frame (depending on pix_fmt),
/// feed them to `H264Encoder::encode_raw(...)`, and send out each `EncodedFrame.nal_bytes`.
pub struct MacRawH264Config {
    /// AVFoundation device index (e.g. "0" for default FaceTime HD camera).
    pub device_index: String,

    /// Resolution you want from AVFoundation (must be one of the device’s “Supported modes”).
    pub input_width: u32,
    pub input_height: u32,

    /// Resolution you want the encoder to output.
    pub output_width: u32,
    pub output_height: u32,

    pub framerate: u32,
    pub bitrate: usize,

    /// Must be one of the pixel formats that AVFoundation actually supports (and that `InputType` has).
    /// Valid choices here are:
    ///   InputType::UYVY422, InputType::YUYV422, InputType::NV12,
    ///   InputType::BGR24, InputType::RGB24
    pub input_type: InputType,

    pub enc_type: EncoderType,
    pub ffmpeg_opts: FfmpegOptions,
}

pub struct MacRawH264Stream {}

impl MacRawH264Stream {
    /// Spawns a blocking thread that:
    /// 1. Launches `ffmpeg -f avfoundation … -f rawvideo pipe:1` to capture raw frames.
    /// 2. Reads exactly `frame_size` bytes per frame (according to `input_type`).
    /// 3. Calls `H264Encoder::encode_raw(Some(pts), &raw_frame_bytes)`.
    /// 4. If `Some(EncodedFrame)`, sends `encoded_frame.nal_bytes` via `tx.blocking_send(...)`.
    ///
    /// Returns a `ReceiverStream<BytesMut>` that downstream can treat exactly like `V4lH264Stream`.
    pub fn new(cfg: MacRawH264Config) -> Result<ReceiverStream<BytesMut>> {
        let (tx, rx) = mpsc::channel::<BytesMut>(10);

        // 1) Determine ffmpeg’s "-pixel_format" string from our InputType:
        let pix_fmt_str: &'static str = match cfg.input_type {
            InputType::UYVY422 => "uyvy422",
            InputType::YUYV422 => "yuyv422",
            InputType::NV12 => "nv12",
            InputType::BGR24 => "bgr24",
            InputType::RGB24 => "rgb24",
            other => {
                panic!(
                    "mac_raw_h264: unsupported InputType::{:?}. \
                     Only UYVY422, YUYVY422, NV12, BGR24, or RGB24 are allowed here.",
                    other
                );
            }
        };

        // 2) Compute how many bytes each raw frame is, before encoding.
        let frame_size: usize = match cfg.input_type {
            InputType::UYVY422 | InputType::YUYV422 => {
                // 2 bytes per pixel (4:2:2)
                (cfg.input_width as usize) * (cfg.input_height as usize) * 2
            }
            InputType::NV12 => {
                // NV12 = Y plane (W*H bytes) + UV plane (W*H/2 bytes) = (3/2) * W*H
                ((cfg.input_width as usize) * (cfg.input_height as usize) * 3) / 2
            }
            InputType::BGR24 | InputType::RGB24 => {
                // 3 bytes per pixel (packed 24-bit)
                (cfg.input_width as usize) * (cfg.input_height as usize) * 3
            }
            _ => unreachable!("pix_fmt_str match above should have panicked for other variants"),
        };

        thread::spawn(move || {
            // 3) Build & spawn the ffmpeg command to capture rawvideo from AVFoundation:
            // ffmpeg -f avfoundation \
            //        -pixel_format {pix_fmt_str} \
            //        -video_size {input_width}x{input_height} \
            //        -framerate {framerate} \
            //        -i {device_index} \
            //        -f rawvideo pipe:1
            let mut cmd = Command::new("ffmpeg");
            cmd.arg("-f")
                .arg("avfoundation")
                .arg("-pixel_format")
                .arg(pix_fmt_str)
                .arg("-video_size")
                .arg(format!("{}x{}", cfg.input_width, cfg.input_height))
                .arg("-framerate")
                .arg(cfg.framerate.to_string())
                .arg("-i")
                .arg(&cfg.device_index)
                .arg("-f")
                .arg("rawvideo")
                .arg("pipe:1");

            // Show ffmpeg logs on stderr (so we can diagnose any errors).
            cmd.stderr(Stdio::inherit());
            // Capture stdout as a pipe so we can read raw frames.
            cmd.stdout(Stdio::piped());

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("mac_raw_h264: failed to spawn ffmpeg: {:?}", e);
                    return;
                }
            };

            // 4) Initialize our H264Encoder with the same input_type, so swscale can convert properly.
            let ec = EncoderConfig {
                input_width: cfg.input_width,
                input_height: cfg.input_height,
                output_width: cfg.output_width,
                output_height: cfg.output_height,
                framerate: cfg.framerate,
                gop: None,
                bitrate: cfg.bitrate,
                disable_b_frames: false,
                enc_type: cfg.enc_type.clone(),
                input_type: cfg.input_type,
            };
            let mut encoder = match H264Encoder::new(ec, &cfg.ffmpeg_opts) {
                Ok(enc) => enc,
                Err(e) => {
                    eprintln!("mac_raw_h264: failed to create H264Encoder: {:?}", e);
                    let _ = child.kill();
                    return;
                }
            };
            let mut pts: i64 = 0;

            // 5) Read raw frames from ffmpeg’s stdout:
            let stdout_pipe = match child.stdout.take() {
                Some(s) => s,
                None => {
                    eprintln!("mac_raw_h264: ffmpeg child had no stdout");
                    return;
                }
            };
            let mut reader = std::io::BufReader::new(stdout_pipe);
            let mut raw_buf = vec![0u8; frame_size];

            loop {
                // read_exact blocks until we’ve read exactly `frame_size` bytes, or hit EOF/error
                match reader.read_exact(&mut raw_buf) {
                    Ok(()) => {
                        // 6) Encode the raw pixel buffer
                        if let Ok(Some(encoded_frame)) =
                            encoder.encode_raw(Some(pts), &raw_buf)
                        {
                            // Send Annex B NAL bytes downstream
                            if tx.blocking_send(encoded_frame.nal_bytes).is_err() {
                                // Receiver was dropped → exit thread
                                break;
                            }
                        }
                        pts += 1;
                    }
                    Err(_) => {
                        // EOF or read error → stop
                        break;
                    }
                }
            }

            let _ = child.wait(); // clean up ffmpeg process
            // Dropping `tx` here closes the channel automatically.
        });

        Ok(ReceiverStream::new(rx))
    }
}