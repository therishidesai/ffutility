#![cfg(target_os = "linux")]

use anyhow::Result;

use bytes::BytesMut;

use crate::encoders::{EncoderConfig, EncoderType, FfmpegOptions, H264Encoder, InputType};
use ffmpeg_next::util::format::Pixel as AvPixel;

use tokio::sync::mpsc;

use tokio_stream::wrappers::ReceiverStream;

use tracing::debug;

use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::video::traits::Capture;
use v4l::prelude::*;

// TODO: make this more generic so you can have a v4l stream with
// different encoder types (e.g AV1)

fn fourcc_to_input_type(fourcc: v4l::FourCC) -> Option<InputType> {
    match &fourcc.repr[..] {
        b"BGR4" => Some(AvPixel::BGR24),
        b"BGR3" => Some(AvPixel::BGR24),
        b"RGB3" => Some(AvPixel::RGB24),
        b"YUYV" => Some(AvPixel::YUYV422),
        b"UYVY" => Some(AvPixel::UYVY422),
        b"YV12" => Some(AvPixel::YUV420P),
        b"NV12" => Some(AvPixel::NV12),
        b"NV21" => Some(AvPixel::NV21),
        b"MJPG" => Some(AvPixel::YUVJ420P),
        b"GREY" => Some(AvPixel::GRAY8),
        _ => None,
    }
}

pub struct V4lH264Config {
    pub output_width: u32,
    pub output_height: u32,
    pub bitrate: usize,
    pub video_dev: String,
}

impl V4lH264Config {
    pub fn detect_input_type(&self) -> Result<(InputType, v4l::FourCC)> {
        let video_dev = Device::with_path(&self.video_dev)?;
        let format = video_dev.format()?;
        let fourcc = format.fourcc;
        
        if let Some(input_type) = fourcc_to_input_type(fourcc) {
            Ok((input_type, fourcc))
        } else {
            anyhow::bail!(
                "Unsupported FourCC format: {} ({}). Supported formats: BGR3, RGB3, YUYV, UYVY, YV12, NV12, NV21, MJPG, GREY",
                fourcc,
                std::str::from_utf8(&fourcc.repr).unwrap_or("invalid")
            )
        }
    }
}

pub struct V4lH264Stream {
}

impl V4lH264Stream {
    pub fn new(cfg: V4lH264Config, ffmpeg_opts: FfmpegOptions) -> Result<ReceiverStream<BytesMut>> {
        let (tx, rx) = mpsc::channel::<BytesMut>(10);

        std::thread::spawn(move || {
            // Detect input type and fourcc from the device
            let (input_type, detected_fourcc) = cfg.detect_input_type()
                .expect("failed to detect input type from V4L device");
            
            tracing::info!("detected fourcc: {}, input type: {:?}", detected_fourcc, input_type);

            // TODO: better error handling, should close the channel correctly instead of exploding
            loop {
                // block until the v4l_device is up
                let v4l_dev = Device::with_path(&cfg.video_dev)
                    .expect("Failed to open v4l device. Device may not exist.");
                let formats = v4l_dev.enum_formats().expect("Failed to get v4l formats.");

                tracing::trace!("{} got formats: {:?}", &cfg.video_dev.as_str(), formats);

                if !formats.iter().any(|fmt| {
                    fmt.fourcc == detected_fourcc
                }) {
                    tracing::error!("{} doesn't have detected FourCC {}!", &cfg.video_dev.as_str(), detected_fourcc);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                } else {
                    tracing::info!("{} has detected FourCC {}!", &cfg.video_dev.as_str(), detected_fourcc);
                    break;
                }
            }


            let video_dev = Device::with_path(&cfg.video_dev).unwrap();
            let mut stream = MmapStream::new(&video_dev, Type::VideoCapture).unwrap();

            let format = video_dev.format().unwrap();
            debug!("V4L Format: {:?}", format);
            let ec = EncoderConfig {
                input_width: format.width,
                input_height: format.height,
                output_width: cfg.output_width,
                output_height: cfg.output_height,
                framerate: 15,
                gop: None,
                bitrate: cfg.bitrate,
                disable_b_frames: false,
                enc_type: EncoderType::X264,
                input_type,
            };

            let mut pts = 0;
            let mut encoder = H264Encoder::new(ec, &ffmpeg_opts).unwrap();
            
            loop {
                // TODO: Better error handling
                let (m_buf, meta) = stream.next().unwrap();
                let bytesused = meta.bytesused as usize;
                // debug!("V4L bytesused: {}", meta.bytesused);
                if let Some(encoded_frame) = encoder.encode_raw(Some(pts), &m_buf[..bytesused]).unwrap() {
                    tx.blocking_send(encoded_frame.nal_bytes).unwrap();
                }
                pts += 1;
            }
        });

        Ok(ReceiverStream::from(rx))
    }
}
