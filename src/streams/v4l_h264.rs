use anyhow::Result;

use bytes::BytesMut;

use crate::encoders::{EncoderConfig, EncoderType, FfmpegOptions, H264Encoder, InputType};

use tokio::sync::mpsc;

use tokio_stream::wrappers::ReceiverStream;

use tracing::debug;

use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::video::traits::Capture;
use v4l::prelude::*;

// TODO: make this more generic so you can have a v4l stream with
// different encoder types (e.g AV1)

pub struct V4lH264Config {
    pub output_width: u32,
    pub output_height: u32,
    pub bitrate: usize,
    pub input_type: InputType,
    pub v4l_fourcc: v4l::FourCC,
    pub video_dev: String,
}

pub struct V4lH264Stream {
}

impl V4lH264Stream {
    pub fn new(cfg: V4lH264Config, ffmpeg_opts: FfmpegOptions) -> Result<ReceiverStream<BytesMut>> {
        let (tx, rx) = mpsc::channel::<BytesMut>(10);

        std::thread::spawn(move || {
            // TODO: better error handling, should close the channel correctly instead of exploding
            loop {
                // block until the v4l_device is up
                let v4l_dev = Device::with_path(&cfg.video_dev)
                    .expect("Failed to open v4l device. Device may not exist.");
                let formats = v4l_dev.enum_formats().expect("Failed to get v4l formats.");

                tracing::trace!("{} got formats: {:?}", &cfg.video_dev.as_str(), formats);

                if !formats.iter().any(|fmt| {
                    fmt.fourcc == cfg.v4l_fourcc
                }) {
                    tracing::error!("{} doesn't have correct FourCC!", &cfg.video_dev.as_str());
                    std::thread::sleep(std::time::Duration::from_secs(1));
                } else {
                    tracing::info!("{} has correct FourCC!", &cfg.video_dev.as_str());
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
                input_type: cfg.input_type,
            };

            let mut encoder = H264Encoder::new(ec, &ffmpeg_opts).unwrap();
            
            loop {
                // TODO: Better error handling
                let (m_buf, meta) = stream.next().unwrap();
                let bytesused = meta.bytesused as usize;
                // debug!("V4L bytesused: {}", meta.bytesused);
                if let Some(encoded_frame) = encoder.encode_raw(&m_buf[..bytesused]) {
                    tx.blocking_send(encoded_frame.nal_bytes).unwrap();
                }
            }
        });

        Ok(ReceiverStream::from(rx))
    }
}
