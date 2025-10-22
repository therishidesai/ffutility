#![cfg(all(target_os = "linux", feature = "v4l"))]

use anyhow::{bail, Result};

use bytes::BytesMut;

use crate::encoders::{EncoderConfig, EncoderType, FfmpegOptions, H264Encoder, InputType};

use ffmpeg_next::util::format::Pixel as AvPixel;

use tracing::debug;

use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::video::traits::Capture;
use v4l::prelude::*;

use std::io;
use tokio_util::io::StreamReader;
use tokio_stream::wrappers::ReceiverStream;
use tokio::sync::mpsc;

// TODO: make this more generic so you can have a v4l stream with
// different encoder types (e.g AV1)

fn fourcc_to_input_type(fourcc: v4l::FourCC) -> Result<InputType> {
    match &fourcc.repr[..] {
        b"BGR4" => Ok(AvPixel::BGR32),
        b"BGR3" => Ok(AvPixel::BGR24),
        b"RGB3" => Ok(AvPixel::RGB24),
        b"YUYV" => Ok(AvPixel::YUYV422),
        b"UYVY" => Ok(AvPixel::UYVY422),
        b"YV12" => Ok(AvPixel::YUV420P),
        b"NV12" => Ok(AvPixel::NV12),
        b"NV21" => Ok(AvPixel::NV21),
        b"MJPG" => Ok(AvPixel::YUVJ420P),
        b"GREY" => Ok(AvPixel::GRAY8),
        _ => bail!("Unsupported v4l FourCC type: {:?}", fourcc),
    }
}

pub struct V4lH264Config {
    pub output_width: u32,
    pub output_height: u32,
    pub bitrate: usize,
    pub video_dev: String,
    pub v4l_fourcc: v4l::FourCC,
}

pub struct V4lH264Stream {
}

impl V4lH264Stream {
    pub fn new(cfg: V4lH264Config, ffmpeg_opts: FfmpegOptions) -> Result<StreamReader<ReceiverStream<Result<BytesMut, io::Error>>, BytesMut>> {

        let input_type = fourcc_to_input_type(cfg.v4l_fourcc)?;
        // only allow 10 frames to be buffered
        // TODO: maybe make this a configurable option
        let (tx, rx) = mpsc::channel::<Result<BytesMut, io::Error>>(10);

        std::thread::spawn(move || {
            // TODO: better error handling, should close the channel correctly instead of exploding
            let mut v4l_dev = Device::with_path(&cfg.video_dev)
                    .expect("Failed to open v4l device. Device may not exist.");
            loop {
                // block until the v4l_device is up
                if cfg.v4l_fourcc == v4l_dev.format().unwrap().fourcc {
                    break;
                } else {
                    tracing::error!("{} doesn't have requested FourCC {}!", &cfg.video_dev.as_str(), cfg.v4l_fourcc);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    v4l_dev = Device::with_path(&cfg.video_dev)
                        .expect("Failed to open v4l device. Device may not exist.");
                }
            }

            let mut stream = MmapStream::new(&v4l_dev, Type::VideoCapture).unwrap();

            let format = v4l_dev.format().unwrap();
            debug!("V4L Format: {:?}", format);
            // TODO: Make this EncoderConfig settable by the user
            let ec = EncoderConfig {
                input_width: format.width,
                input_height: format.height,
                output_width: cfg.output_width,
                output_height: cfg.output_height,
                framerate: 15,
                gop: None,
                bitrate: cfg.bitrate,
                disable_b_frames: true,
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
                    tx.blocking_send(Ok(encoded_frame.nal_bytes)).unwrap();
                }
                pts += 1;
            }
        });

        Ok(StreamReader::new(ReceiverStream::new(rx)))
    }
}
