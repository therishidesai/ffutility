use ffmpeg::codec::codec::Codec as AvCodec;
use ffmpeg::codec::context::Context as AvContext;
use ffmpeg::codec::encoder::video::Encoder as AvVideoEncoder;
use ffmpeg::codec::packet::Packet as AvPacket;
use ffmpeg::software::scaling::{context::Context as AvScalingContext, flag::Flags};
use ffmpeg::util::format::Pixel as AvPixel;
use ffmpeg::util::frame::Video as AvFrame;
use ffmpeg::util::rational::Rational;
use ffmpeg::Dictionary as AvDictionary;
use ffmpeg::Error as AvError;
use ffmpeg_next as ffmpeg;

#[cfg(feature = "opencv")]
use opencv::{
    core::Mat,
    prelude::{MatTraitConst, MatTraitConstManual},
};

use serde::{Deserialize, Serialize};

use thiserror::Error;

use tracing::{debug, error};

// pub type FfmpegOptions<'a> = AvDictionary<'a>;
// NOTE: does this have to be a tuple of Strings? Maybe CoW can help
pub type FfmpegOptions = Vec<(String, String)>;
pub type InputType = AvPixel;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EncoderType {
    #[serde(rename = "libx264")]
    X264,
    #[serde(rename = "h264_nvenc")]
    H264Nvenc,
    #[serde(rename = "h264_nvmpi")]
    // NOTE: Only for Jetson NVENC systems. Will not work on any other
    // systems
    H264Nvmpi,
}

impl EncoderType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::X264 => "libx264",
            Self::H264Nvenc => "h264_nvenc",
            Self::H264Nvmpi => "h264_nvmpi",
        }
    }
}

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub input_width: u32,
    pub input_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub framerate: u32,
    pub gop: Option<u32>,
    pub bitrate: usize,
    pub disable_b_frames: bool,
    pub enc_type: EncoderType,
    pub input_type: InputType,
}

#[derive(Error, Debug)]
pub enum H264EncoderError {
    #[error("PTS was not monotonically increasing: previous PTS: {prev_pts} > PTS: {curr_pts}")]
    PTSNotMonotonic {
        prev_pts: i64,
        curr_pts: i64
    },
    
    #[error("FFmpeg error: {0}")]
    FfmpegError(#[from] AvError),

    #[error("Failed to allocate AVCodec context")]
    AvCodecAllocContextError,
    
    #[error("Codec not found: {name}")]
    CodecNotFound { name: String },
    
    #[error("Scaling context error: {0}")]
    ScalingError(String),
    
    #[error("Frame encoding error: {0}")]
    EncodingError(String),
    
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    
    #[error("Invalid state: {0}")]
    InvalidState(String),
}

pub struct H264Encoder {
    encoder: AvVideoEncoder,
    scaler: AvScalingContext,
    prev_pts: Option<i64>,
    input_type: AvPixel,
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
}

fn codec_context_as(codec: &AvCodec) -> Result<AvContext, H264EncoderError> {
    unsafe {
        let context_ptr = ffmpeg::ffi::avcodec_alloc_context3(codec.as_ptr());
        if !context_ptr.is_null() {
            Ok(AvContext::wrap(context_ptr, None))
        } else {
            Err(H264EncoderError::AvCodecAllocContextError)
        }
    }
}

pub struct EncodedFrame {
    pub nal_bytes: bytes::BytesMut,
    pub is_keyframe: bool,
    pub duration: i64,
    pub pts: Option<i64>,
}

unsafe impl Send for H264Encoder {}
unsafe impl Sync for H264Encoder {}

impl H264Encoder {
    pub fn new(
        ec: EncoderConfig,
        extra_ffmpeg_opts: &FfmpegOptions,
    ) -> Result<Self, H264EncoderError> {
        let mut extra_opts = AvDictionary::new();
        for opt in extra_ffmpeg_opts {
            extra_opts.set(opt.0.as_str(), opt.1.as_str());
        }

        let ffmpeg_codec = ffmpeg::encoder::find_by_name(ec.enc_type.as_str())
            .ok_or_else(|| H264EncoderError::CodecNotFound { 
                name: ec.enc_type.as_str().to_string() 
            })?;
        let ffmpeg_context = codec_context_as(&ffmpeg_codec)?;
        let mut ffmpeg_vid_encoder = ffmpeg_context.encoder().video()
            .ok_or_else(|| H264EncoderError::InvalidState(
                "Failed to create video encoder from context".to_string()
            ))?;
        ffmpeg_vid_encoder.set_width(ec.output_width);
        ffmpeg_vid_encoder.set_height(ec.output_height);
        ffmpeg_vid_encoder.set_format(AvPixel::YUV420P);
        ffmpeg_vid_encoder.set_frame_rate(Some((ec.framerate as i32, 1)));
        ffmpeg_vid_encoder.set_time_base(Rational(1, ec.framerate as i32));
        ffmpeg_vid_encoder.set_bit_rate(ec.bitrate);
        if ec.disable_b_frames {
            ffmpeg_vid_encoder.set_max_b_frames(0_usize);
        }
        if let Some(gop) = ec.gop {
            ffmpeg_vid_encoder.set_gop(gop);
        }
        let encoder = ffmpeg_vid_encoder.open_with(extra_opts)?;

        let scaler = AvScalingContext::get(
            ec.input_type,
            ec.input_width,
            ec.input_height,
            AvPixel::YUV420P,
            ec.output_width,
            ec.output_height,
            Flags::BILINEAR,
        )?;

        Ok(Self {
            encoder,
            scaler,
            prev_pts: None,
            input_type: ec.input_type,
            input_width: ec.input_width,
            input_height: ec.input_height,
            output_width: ec.output_width,
            output_height: ec.output_height,
        })
    }

    #[cfg(feature = "opencv")]
    pub fn encode_mat(&mut self, pts: Option<i64>, input: &Mat) -> Result<Option<EncodedFrame>, H264EncoderError> {
        let width = input.cols();
        let height = input.rows();
        
        // Verify values are positive before converting
        if width <= 0 || height <= 0 {
            return Err(H264EncoderError::InvalidInput(
                format!("Invalid dimensions: width={}, height={}", width, height)
            ));
        }
            
        let mut out_frame = AvFrame::new(
            AvPixel::YUV420P,
            width as u32,
            height as u32,
        );

        let mut in_frame = AvFrame::new(
            self.input_type,
            width as u32,
            height as u32,
        );

        let data_bytes = input.data_bytes()
            .ok_or_else(|| H264EncoderError::InvalidInput("Failed to get data bytes from Mat".to_string()))?;
        
        in_frame
            .data_mut(0)
            .copy_from_slice(data_bytes);

        self.scaler.run(&in_frame, &mut out_frame)
            .map_err(|e| H264EncoderError::ScalingError(e.to_string()))?;
            
        self.encode(pts, out_frame)
    }

    pub fn encode_raw(&mut self, pts: Option<i64>, input: &[u8]) -> Result<Option<EncodedFrame>, H264EncoderError> {
        debug!("input len: {}", input.len());
        
        let mut out_frame = AvFrame::new(
            AvPixel::YUV420P,
            self.output_width as i32,
            self.output_height as i32,
        );

        let mut in_frame = AvFrame::new(
            self.input_type,
            self.input_width as i32,
            self.input_height as i32,
        );

        if in_frame.planes() > 1 {
            let mut start_id = 0;
            for i in 0..in_frame.planes() {
                let buf_size = in_frame.data(i).len();
                let end_id = start_id + buf_size;
                
                if end_id > input.len() {
                    return Err(H264EncoderError::InvalidInput(
                        format!("Input buffer too small: needed {}, got {}", end_id, input.len())
                    ));
                }
                
                in_frame.data_mut(i).copy_from_slice(&input[start_id..end_id]);
                start_id = end_id;
            }
        } else {
            if input.len() < in_frame.data(0).len() {
                return Err(H264EncoderError::InvalidInput(
                    format!("Input buffer too small: needed {}, got {}", in_frame.data(0).len(), input.len())
                ));
            }
            
            in_frame.data_mut(0).copy_from_slice(input);
        }
        
        self.scaler.run(&in_frame, &mut out_frame)
            .map_err(|e| H264EncoderError::ScalingError(e.to_string()))?;
            
        self.encode(pts, out_frame)
    }

    pub fn encode(&mut self, pts: Option<i64>, mut frame: AvFrame) -> Result<Option<EncodedFrame>, H264EncoderError> {
        if let Some(prev_pts) = self.prev_pts {
            if let Some(curr_pts) = pts {
                if prev_pts > curr_pts {
                    return Err(H264EncoderError::PTSNotMonotonic {
                        prev_pts,
                        curr_pts
                    })
                }
            }
        }

        frame.set_pts(pts);
        self.prev_pts = pts;
        
        self.encoder.send_frame(&frame)
            .map_err(|e| H264EncoderError::EncodingError(format!("Failed to send frame: {}", e)))?;
            
        self.retrieve_nal()
    }

    /// Drains and consumes this encoder
    pub fn drain(mut self) -> Result<Vec<EncodedFrame>, H264EncoderError> {
        self.encoder.send_eof()?;
        let mut result = vec![];
        while let Some(nal) = self.retrieve_nal()? {
            result.push(nal);
        }
        Ok(result)
    }

    /// Drain a NAL out of the encoder. Typically not used directly,
    /// except after `begin_drain`.
    fn retrieve_nal(&mut self) -> Result<Option<EncodedFrame>, H264EncoderError> {
        let mut encoded_packet = AvPacket::empty();
        let encoder_res = self.encoder.receive_packet(&mut encoded_packet);

        match encoder_res {
            Ok(_) => {
                let encoded_data = encoded_packet.data();
                if encoded_data.is_none() {
                    error!("encoder likely dropping frames!! data packet is empty");
                    Ok(None)
                } else {
                    if let Some(nal) = encoded_data {
                        Ok(Some(EncodedFrame {
                            nal_bytes: bytes::BytesMut::from(nal),
                            is_keyframe: encoded_packet.is_key(),
                            duration: encoded_packet.duration(),
                            pts: encoded_packet.pts(),
                        }))
                    } else {
                        Ok(None)
                    }
                }
            }
            Err(e) => {
                // For AVERROR(EAGAIN), this is not a real error, just means "try again later"
                if matches!(e, AvError::Other { errno } if errno == ffmpeg::libc::EAGAIN) {
                    debug!("No output frames available yet (EAGAIN): {e}");
                    Ok(None)
                } else if matches!(e, AvError::Eof) {
                    debug!("End of stream reached");
                    Ok(None)
                } else {
                    debug!("FFmpeg encoder error: {e}");
                    Err(H264EncoderError::FfmpegError(e))
                }
            }
        }
    }
}
