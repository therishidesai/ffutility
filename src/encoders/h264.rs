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

pub type FfmpegOptions<'a> = AvDictionary<'a>;
pub type InputType = AvPixel;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EncoderType {
    #[serde(rename = "libx264")]
    X264,
    #[serde(rename = "h264_nvenc")]
    H264Nvenc,
}

impl EncoderType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::X264 => "libx264",
            Self::H264Nvenc => "h264_nvenc",
        }
    }
}

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
    #[error("ffmpeg error: ")]
    FfmpegError(#[from] AvError),
    #[error("failed to alloc avcodec context")]
    AvCodecAllocContextError,
}

pub struct H264Encoder {
    encoder: AvVideoEncoder,
    scaler: AvScalingContext,
    pts_count: usize,
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
    pub pts: i64,
}

unsafe impl Send for H264Encoder {}
unsafe impl Sync for H264Encoder {}

impl H264Encoder {
    pub fn new(
        ec: EncoderConfig,
        extra_ffmpeg_opts: FfmpegOptions,
    ) -> Result<Self, H264EncoderError> {
        let ffmpeg_codec = ffmpeg::encoder::find_by_name(ec.enc_type.as_str()).unwrap();
        let ffmpeg_context = codec_context_as(&ffmpeg_codec)?;
        let mut ffmpeg_vid_encoder = ffmpeg_context.encoder().video().unwrap();
        ffmpeg_vid_encoder.set_width(ec.output_width);
        ffmpeg_vid_encoder.set_height(ec.output_height);
        ffmpeg_vid_encoder.set_format(AvPixel::YUV420P);
        ffmpeg_vid_encoder.set_frame_rate(Some((ec.framerate as i32, 1)));
        ffmpeg_vid_encoder.set_time_base(Rational(1, ec.framerate as i32));
        ffmpeg_vid_encoder.set_bit_rate(ec.bitrate as usize);
        if ec.disable_b_frames {
            ffmpeg_vid_encoder.set_max_b_frames(0_usize);
        }
        if let Some(gop) = ec.gop {
            ffmpeg_vid_encoder.set_gop(gop);
        }
        let encoder = ffmpeg_vid_encoder.open_with(extra_ffmpeg_opts)?;

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
            pts_count: 0,
            input_type: ec.input_type,
            input_width: ec.input_width,
            input_height: ec.input_height,
            output_width: ec.output_width,
            output_height: ec.output_height,
        })
    }

    #[cfg(feature = "opencv")]
    pub fn encode_mat(&mut self, input: &Mat) -> Option<EncodedFrame> {
        let width = input.cols();
        let height = input.rows();
        let mut out_frame = AvFrame::new(
            AvPixel::YUV420P,
            width.try_into().unwrap(),
            height.try_into().unwrap(),
        );

        let mut in_frame = AvFrame::new(
            self.input_type,
            width.try_into().unwrap(),
            height.try_into().unwrap(),
        );

        in_frame
            .data_mut(0)
            .copy_from_slice(input.data_bytes().unwrap());

        self.scaler.run(&in_frame, &mut out_frame).unwrap();
        self.encode(out_frame)
    }

    pub fn encode_raw(&mut self, input: &[u8]) -> Option<EncodedFrame> {
        let mut out_frame = AvFrame::new(
            AvPixel::YUV420P,
            self.output_width.try_into().unwrap(),
            self.output_height.try_into().unwrap(),
        );

        let mut in_frame = AvFrame::new(
            self.input_type,
            self.input_width.try_into().unwrap(),
            self.input_height.try_into().unwrap(),
        );

        in_frame.data_mut(0).copy_from_slice(input);
        self.scaler.run(&in_frame, &mut out_frame).unwrap();
        self.encode(out_frame)
    }

    pub fn encode(&mut self, mut frame: AvFrame) -> Option<EncodedFrame> {
        frame.set_pts(Some(self.pts_count as i64));
        let curr_pts = self.pts_count;
        self.pts_count += 1;

        self.encoder.send_frame(&frame).unwrap();
        let mut encoded_packet = AvPacket::empty();
        let encoder_res = self.encoder.receive_packet(&mut encoded_packet);

        match encoder_res {
            Ok(_) => {
                let encoded_data = encoded_packet.data();
                if encoded_data.is_none() {
                    error!("encoder likely dropping frames!! data packet is empty");
                    None
                } else {
                    if let Some(nal) = encoded_data {
                        Some(EncodedFrame {
                            nal_bytes: bytes::BytesMut::from(nal),
                            is_keyframe: encoded_packet.is_key(),
                            duration: encoded_packet.duration(),
                            pts: curr_pts as i64,
                        })
                    } else {
                        None
                    }
                }
            }
            Err(e) => {
                debug!("got ffmpeg encoder error: {e}");
                None
            }
        }
    }
}
