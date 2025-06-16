use anyhow::{Result, Context as AnyhowContext};

use bytes::BytesMut;

use crate::encoders::{EncoderConfig, EncoderType, FfmpegOptions, H264Encoder, InputType, H264EncoderError};

use tokio::sync::mpsc;

use tokio_stream::wrappers::ReceiverStream;

use tracing::{debug, error, info, trace, warn};

use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::video::traits::Capture;
use v4l::prelude::*;
use v4l::Device;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum V4lH264Error {
    #[error("V4L device error: {0}")]
    DeviceError(String),
    
    #[error("V4L format error: {0}")]
    FormatError(String),
    
    #[error("V4L stream error: {0}")]
    StreamError(String),
    
    #[error("Encoder error: {0}")]
    EncoderError(#[from] H264EncoderError),
    
    #[error("Channel send error: {0}")]
    ChannelError(String),
}

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
        let device_path = cfg.video_dev.clone();

        std::thread::spawn(move || {
            // Error handling - if we encounter an error, log it and continue trying
            if let Err(e) = Self::capture_thread(cfg, ffmpeg_opts, tx.clone()) {
                error!("V4L capture thread error: {}", e);
                // Signal that the stream is closed by dropping tx
            }
        });

        Ok(ReceiverStream::from(rx))
    }
    
    fn capture_thread(
        cfg: V4lH264Config, 
        ffmpeg_opts: FfmpegOptions, 
        tx: mpsc::Sender<BytesMut>
    ) -> Result<(), V4lH264Error> {
        // Wait for the v4l device to be available with the correct format
        loop {
            // Try to open the device
            let v4l_dev = match Device::with_path(&cfg.video_dev) {
                Ok(dev) => dev,
                Err(e) => {
                    error!("Failed to open v4l device {}: {}", cfg.video_dev, e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };
            
            // Check if it has the correct formats
            let formats = v4l_dev.enum_formats()
                .map_err(|e| V4lH264Error::DeviceError(format!("Failed to get formats: {}", e)))?;

            trace!("{} got formats: {:?}", &cfg.video_dev, formats);

            if !formats.iter().any(|fmt| fmt.fourcc == cfg.v4l_fourcc) {
                error!("{} doesn't have correct FourCC!", &cfg.video_dev);
                std::thread::sleep(std::time::Duration::from_secs(1));
            } else {
                info!("{} has correct FourCC!", &cfg.video_dev);
                break;
            }
        }

        // Now that we know the device is ready, set up the capture stream
        let video_dev = Device::with_path(&cfg.video_dev)
            .map_err(|e| V4lH264Error::DeviceError(format!("Failed to open video device: {}", e)))?;
            
        let mut stream = MmapStream::new(&video_dev, Type::VideoCapture)
            .map_err(|e| V4lH264Error::StreamError(format!("Failed to create capture stream: {}", e)))?;

        let format = video_dev.format()
            .map_err(|e| V4lH264Error::FormatError(format!("Failed to get format: {}", e)))?;
            
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

        let mut pts = 0;
        let mut encoder = H264Encoder::new(ec, &ffmpeg_opts)?;
        
        // Main capture loop
        loop {
            // Get the next frame from the V4L device
            let (m_buf, meta) = match stream.next() {
                Ok(frame) => frame,
                Err(e) => {
                    warn!("Error getting next frame: {}", e);
                    continue;
                }
            };
            
            let bytesused = meta.bytesused as usize;
            
            // Encode the frame
            match encoder.encode_raw(Some(pts), &m_buf[..bytesused]) {
                Ok(Some(encoded_frame)) => {
                    // Send the encoded frame over the channel
                    if let Err(e) = tx.blocking_send(encoded_frame.nal_bytes) {
                        return Err(V4lH264Error::ChannelError(format!("Failed to send frame: {}", e)));
                    }
                },
                Ok(None) => {
                    // No frame was produced, this is normal
                    trace!("No frame produced for input frame at PTS {}", pts);
                },
                Err(e) => {
                    warn!("Error encoding frame at PTS {}: {}", pts, e);
                    // Continue even if we have an encoding error
                }
            }
            
            pts += 1;
        }
    }
}
