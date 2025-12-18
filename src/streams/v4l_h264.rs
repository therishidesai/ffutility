#![cfg(all(target_os = "linux", feature = "v4l"))]

use anyhow::{bail, Result};

use bytes::{Bytes, BytesMut};

use crate::encoders::{EncoderConfig, EncoderType, FfmpegOptions, H264Encoder, InputType};

use ffmpeg_next::util::format::Pixel as AvPixel;

use tracing::{debug, error};

use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::traits::Capture;

use std::fs::File;
use std::{io, io::Write};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::StreamReader;

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

#[derive(Clone)]
pub struct LoadingImage {
    pub data: Vec<u8>,
    pub input_width: u32,
    pub input_height: u32,
    pub input_type: InputType,
}

pub struct V4lH264Config {
    pub output_width: u32,
    pub output_height: u32,
    pub bitrate: usize,
    pub video_dev: String,
    pub v4l_fourcc: v4l::FourCC,
    pub loading_image: Option<LoadingImage>,
}

pub struct V4lH264Stream {}

impl V4lH264Stream {
    pub fn new(
        cfg: V4lH264Config,
        ffmpeg_opts: FfmpegOptions,
    ) -> Result<StreamReader<ReceiverStream<Result<BytesMut, io::Error>>, BytesMut>> {
        let input_type = fourcc_to_input_type(cfg.v4l_fourcc)?;
        // only allow 10 frames to be buffered
        // TODO: maybe make this a configurable option
        let (tx, rx) = mpsc::channel::<Result<BytesMut, io::Error>>(10);

        std::thread::spawn(move || {
            let mut loading_ffmpeg_opts = FfmpegOptions::new();
            loading_ffmpeg_opts.push((String::from("preset"), String::from("medium")));
            loading_ffmpeg_opts.push((String::from("tune"), String::from("stillimage")));
            loading_ffmpeg_opts.push((
                String::from("x264-params"),
                String::from("repeat-headers=1:keyint=1:min-keyint=1:scenecut=0"),
            ));
            // TODO: better error handling, should close the channel correctly instead of exploding
            let cached_loading_frames = cfg.loading_image.as_ref().map(|loading_image| {
                let loading_ec = EncoderConfig {
                    input_width: loading_image.input_width,
                    input_height: loading_image.input_height,
                    output_width: cfg.output_width,
                    output_height: cfg.output_height,
                    framerate: 10,
                    gop: Some(1),
                    bitrate: 1000000,
                    disable_b_frames: true,
                    enc_type: EncoderType::X264,
                    input_type: loading_image.input_type,
                };

                let mut f = File::create("loading-video.h264").unwrap();

                let mut loading_encoder =
                    H264Encoder::new(loading_ec, &loading_ffmpeg_opts).unwrap();
                let mut nal_frames = Vec::new();
                loop {
                    if let Some(encoded_frame) = loading_encoder
                        .encode_raw(Some(0), &loading_image.data)
                        .unwrap()
                    {
                        nal_frames.push(encoded_frame.nal_bytes.freeze());
                        break;
                    }
                }

                for encoded_frame in loading_encoder.drain().unwrap() {
                    let frame = encoded_frame.nal_bytes.clone().freeze();
                    f.write_all(&frame);
                    f.flush();
                    nal_frames.push(frame);
                }

                tracing::error!("Total Loaded Image NAL frames: {}", nal_frames.len());

                nal_frames
            });

            let mut v4l_dev = Device::with_path(&cfg.video_dev)
                .expect("Failed to open v4l device. Device may not exist.");
            loop {
                // block until the v4l_device is up
                if cfg.v4l_fourcc == v4l_dev.format().unwrap().fourcc {
                    break;
                } else {
                    tracing::error!(
                        "{} doesn't have requested FourCC {}!",
                        &cfg.video_dev.as_str(),
                        cfg.v4l_fourcc
                    );
                    if let Some(frames) = cached_loading_frames.as_ref() {
                        if !send_loading_frames(&tx, frames) {
                            return;
                        }
                    } else {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }

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

            // Add repeat-headers=1 to ensure SPS/PPS are emitted regularly.
            // This is critical for seamless transitions from overlay to live feed,
            // as the decoder needs fresh SPS/PPS headers to reinitialize.
            let mut live_ffmpeg_opts = ffmpeg_opts.clone();
            live_ffmpeg_opts.push((
                String::from("x264-params"),
                String::from("repeat-headers=1"),
            ));

            let mut pts: i64 = 0;
            let mut encoder = H264Encoder::new(ec, &live_ffmpeg_opts).unwrap();

            loop {
                // TODO: Better error handling
                match stream.next() {
                    Ok((m_buf, meta)) => {
                        let bytesused = meta.bytesused as usize;
                        // debug!("V4L bytesused: {}", meta.bytesused);
                        if let Some(encoded_frame) =
                            encoder.encode_raw(Some(pts), &m_buf[..bytesused]).unwrap()
                        {
                            tx.blocking_send(Ok(encoded_frame.nal_bytes)).unwrap();
                        }
                        pts += 1;
                    }
                    Err(e) => {
                        if let Some(error_code) = e.raw_os_error() {
                            if error_code == 5 {
                                error!(
                                    "Got I/O Error: {} for {}. Retrying in 1 second",
                                    error_code, &cfg.video_dev
                                );
                                if let Some(frames) = cached_loading_frames.as_ref() {
                                    if !send_loading_frames(&tx, frames) {
                                        return;
                                    }
                                } else {
                                    std::thread::sleep(std::time::Duration::from_secs(1));
                                }
                            } else {
                                panic!("Unrecoverable OS Error: {}", e);
                            }
                        }
                    }
                }
            }
        });

        Ok(StreamReader::new(ReceiverStream::new(rx)))
    }
}

fn send_loading_frames(tx: &mpsc::Sender<Result<BytesMut, io::Error>>, frames: &[Bytes]) -> bool {
    if frames.is_empty() {
        return true;
    }

    for frame in frames {
        if tx
            .blocking_send(Ok(BytesMut::from(frame.as_ref())))
            .is_err()
        {
            return false;
        }

        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    true
}
