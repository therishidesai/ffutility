use anyhow::Result;

use bytes::BytesMut;

use crate::encoders::{EncoderConfig, FfmpegOptions, H264Encoder};

use tokio::sync::mpsc;

use tokio_stream::wrappers::ReceiverStream;

use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::video::traits::Capture;
use v4l::prelude::*;

// TODO: make this more generic so you can have a v4l stream with
// different encoder types (e.g AV1)

pub struct V4lH264Stream {
}

impl V4lH264Stream {
    pub fn new(ec: EncoderConfig, ffmpeg_opts: FfmpegOptions) -> Result<ReceiverStream<BytesMut>> {
        let (tx, rx) = mpsc::channel::<BytesMut>(10);
        let video_dev = Device::with_path("/dev/video0")?;
        eprintln!("{:?}", video_dev.format());
        let mut stream = MmapStream::new(&video_dev, Type::VideoCapture)?;
        let mut encoder = H264Encoder::new(ec, ffmpeg_opts)?;

        std::thread::spawn(move || {
            loop {
                // TODO: Better error handling
                let (m_buf, _meta) = stream.next().unwrap();
                if let Some(encoded_frame) = encoder.encode_raw(&m_buf) {
                    tx.blocking_send(encoded_frame.nal_bytes).unwrap();
                }
            }
        });

        Ok(ReceiverStream::from(rx))
    }
}
