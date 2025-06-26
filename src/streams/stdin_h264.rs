use anyhow::Result;
use bytes::BytesMut;
use crate::encoders::{EncoderConfig, EncoderType, H264Encoder, InputType};
use std::io::{self, Read};
use std::thread;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// A stream that reads raw video data from stdin and encodes it to H.264.
pub struct StdinH264Stream {}

impl StdinH264Stream {
    /// Creates a new stream that reads raw video from stdin and encodes to H.264.
    /// Expects raw video data (not H.264) from ffmpeg.
    pub fn new(width: u32, height: u32, input_type: InputType) -> Result<ReceiverStream<BytesMut>> {
        let (tx, rx) = mpsc::channel::<BytesMut>(10);

        thread::spawn(move || {
            let frame_size = match input_type {
                InputType::YUV420P => (width * height * 3) / 2,
                InputType::NV12 => (width * height * 3) / 2,
                InputType::RGB24 => width * height * 3,
                InputType::BGR24 => width * height * 3,
                _ => {
                    eprintln!("Unsupported input type: {:?}", input_type);
                    return;
                }
            } as usize;

            let ec = EncoderConfig {
                input_width: width,
                input_height: height,
                output_width: width,
                output_height: height,
                framerate: 30,
                gop: None,
                bitrate: 2000000,
                disable_b_frames: false,
                enc_type: EncoderType::X264,
                input_type,
            };

            let mut encoder = match H264Encoder::new(ec, &vec![]) {
                Ok(enc) => enc,
                Err(e) => {
                    eprintln!("Failed to create H264Encoder: {:?}", e);
                    return;
                }
            };

            let mut stdin = io::stdin();
            let mut buffer = vec![0u8; frame_size];
            let mut pts = 0;

            loop {
                match stdin.read_exact(&mut buffer) {
                    Ok(()) => {
                        match encoder.encode_raw(Some(pts), &buffer) {
                            Ok(Some(encoded_frame)) => {
                                if tx.blocking_send(encoded_frame.nal_bytes).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {
                                // Encoder buffering
                            }
                            Err(_) => break,
                        }
                        pts += 1;
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }
}