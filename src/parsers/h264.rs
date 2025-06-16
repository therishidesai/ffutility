use anyhow::{Result, bail};

use bytes::BytesMut;

use futures::StreamExt;
use futures::stream::Stream;

use h264_reader::Context;
use h264_reader::annexb::AnnexBReader;
use h264_reader::nal::{pps::PicParameterSet, slice::{SliceFamily, SliceHeader}, sps::SeqParameterSet, Nal, RefNal, UnitType};
use h264_reader::push::NalInterest;

use moq_karp::{BroadcastProducer, Dimensions, H264, Frame, Timestamp, Track, TrackProducer, Video};

use std::cell::Cell;
use std::io::{self, Read};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, mpsc::{channel, SendError}};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum H264ParserError {
    #[error("NAL header error: {0}")]
    NalHeaderError(String),
    
    #[error("SPS parse error: {0}")]
    SpsParseError(String),
    
    #[error("PPS parse error: {0}")]
    PpsParseError(String),
    
    #[error("Slice header parse error: {0}")]
    SliceHeaderError(String),
    
    #[error("IO error: {0}")]
    IoError(#[from] io::Error),
    
    #[error("Lock error: {0}")]
    LockError(String),
    
    #[error("Channel error: {0}")]
    ChannelError(String),
    
    #[error("Missing context: parser not initialized")]
    MissingContext,
    
    #[error("Broadcast error: {0}")]
    BroadcastError(String),
}

pub struct AnnexBStreamImport {
    broadcast: Arc<Mutex<BroadcastProducer>>,
    codec: Option<H264>,
    ctx: Option<Context>,
    width: u32,
    height: u32,
}

impl AnnexBStreamImport {
    pub fn new(broadcast: Arc<Mutex<BroadcastProducer>>, width: u32, height: u32) -> Self {
        Self {
            broadcast,
            codec: None,
            ctx: None,
            width,
            height,
        }
    }

    pub async fn init_from<T: Stream<Item = BytesMut> + Unpin>(&mut self, input: &mut T) -> Result<TrackProducer> {
        let mut ctx = Context::new();
        let mut sps: Option<SeqParameterSet> = None;
        let found_sps = AtomicBool::new(false);
        let found_pps = AtomicBool::new(false);

        let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
            // Get NAL header
            let header = match nal.header() {
                Ok(h) => h,
                Err(e) => {
                    // Just ignore this NAL and continue
                    return NalInterest::Ignore;
                }
            };
            
            let nal_unit_type = header.nal_unit_type();
            
            match nal_unit_type {
                UnitType::SeqParameterSet => {
                    match SeqParameterSet::from_bits(nal.rbsp_bits()) {
                        Ok(sps_local) => {
                            ctx.put_seq_param_set(sps_local.clone());
                            sps = Some(sps_local);
                            found_sps.store(true, Ordering::SeqCst);
                            NalInterest::Buffer
                        },
                        Err(e) => {
                            // Log error but continue parsing
                            eprintln!("Error parsing SPS: {:?}", e);
                            NalInterest::Ignore
                        }
                    }
                },
                UnitType::PicParameterSet => {
                    match PicParameterSet::from_bits(&ctx, nal.rbsp_bits()) {
                        Ok(pps) => {
                            ctx.put_pic_param_set(pps);
                            found_pps.store(true, Ordering::SeqCst);
                            NalInterest::Buffer
                        },
                        Err(e) => {
                            // Log error but continue parsing
                            eprintln!("Error parsing PPS: {:?}", e);
                            NalInterest::Ignore
                        }
                    }
                },
                _ => NalInterest::Ignore,
            }
        });

        while !found_pps.load(Ordering::SeqCst) && !found_sps.load(Ordering::SeqCst) {
            if let Some(buffer) = input.next().await {
                reader.push(&buffer);
            } else {
                break
            }
        }

        if let Some(sps) = sps {
            let codec = H264 {
                profile: sps.profile().profile_idc(),
                constraints: sps.constraint_flags.reserved_zero_two_bits(),
                level: sps.level_idc,
            };
            self.codec = Some(codec.clone());

            let track = Video {
                track: Track { name: String::from("video0"), priority: 2 },
                resolution: Dimensions {
                    width: self.width,
                    height: self.height,
                },
                codec: codec.into(),
                description: None,
                bitrate: None,
            };

            // Get broadcast lock
            let mut broadcast = match self.broadcast.lock() {
                Ok(b) => b,
                Err(e) => bail!("Failed to lock broadcast mutex: {}", e),
            };
            
            // Publish the video track
            let track = match broadcast.publish_video(track) {
                Ok(t) => t,
                Err(e) => bail!("Failed to publish video track: {}", e),
            };
            
            self.ctx = Some(ctx);

            Ok(track)
        } else {
            bail!("Failed to find valid SPS in input!");
        }
    }

    pub async fn read_from<T: Stream<Item = BytesMut> + Unpin>(&mut self, input: &mut T, track: &mut TrackProducer) -> Result<()> {
        if self.ctx.is_none() || self.codec.is_none() {
            bail!("AnnexBImport not initialized");
        }

        let now = std::time::Instant::now();

        let ctx = self.ctx.as_ref()
            .ok_or_else(|| H264ParserError::MissingContext)?;
            
        // NOTE: Have to use a sync channel since I want to mutate
        // both in the AnnexBReader accumulate closure and out of the
        // closure. There is probably a better way of doing this. The
        // h264_reader library has some weird API choices so it may
        // also be worth writing our own h264 NAL parser.
        let (frame_tx, frame_rx) = channel();

        let mut sps = Cell::new(BytesMut::new());
        let mut pps = Cell::new(BytesMut::new());
        let mut first_keyframe = false;

        let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
            // Get NAL header
            let header = match nal.header() {
                Ok(h) => h,
                Err(_) => return NalInterest::Ignore,
            };
            
            let nal_unit_type = header.nal_unit_type();
            
            match nal_unit_type {
                UnitType::PicParameterSet => {
                    if nal.is_complete() {
                        if let Some(nal_bytes) = read_nal_bytes(nal) {
                            pps.set(nal_bytes);
                        }
                    }
                    NalInterest::Buffer
                }
                UnitType::SeqParameterSet => {
                    if nal.is_complete() {
                        if let Some(nal_bytes) = read_nal_bytes(nal) {
                            sps.set(nal_bytes);
                        }
                    }
                    NalInterest::Buffer
                },
                UnitType::SliceLayerWithoutPartitioningNonIdr => {
                    if nal.is_complete() && first_keyframe {
                        let ts = now.elapsed().as_micros();

                        // Parse slice header
                        let slice_header_result = match nal.header() {
                            Ok(h) => SliceHeader::from_bits(&ctx, &mut nal.rbsp_bits(), h),
                            Err(_) => return NalInterest::Ignore,
                        };
                        
                        let slice_header = match slice_header_result {
                            Ok((header, _)) => header,
                            Err(_) => return NalInterest::Ignore,
                        };
                        
                        let keyframe = slice_header.slice_type.family == SliceFamily::I;

                        if let Some(full_nal_buf) = read_nal_bytes(nal) {
                            if !keyframe {
                                let mut payload = BytesMut::new();
                                payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                                payload.extend_from_slice(&full_nal_buf);
                                let frame = Frame {
                                    timestamp: Timestamp::from_micros(ts as u64),
                                    keyframe,
                                    payload: payload.freeze(),
                                };
                                
                                if let Err(e) = frame_tx.send(frame) {
                                    eprintln!("Failed to send frame: {:?}", e);
                                }
                            } else if keyframe && !pps.get_mut().is_empty() && !sps.get_mut().is_empty() {
                                let sps = sps.get_mut();
                                let pps = pps.get_mut();
                                let mut payload = BytesMut::new();
                                payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                                payload.extend_from_slice(&sps);
                                payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                                payload.extend_from_slice(&pps);
                                payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                                payload.extend_from_slice(&full_nal_buf);
                                let frame = Frame {
                                    timestamp: Timestamp::from_micros(ts as u64),
                                    keyframe,
                                    payload: payload.freeze(),
                                };
                                
                                if let Err(e) = frame_tx.send(frame) {
                                    eprintln!("Failed to send frame: {:?}", e);
                                }
                            }
                        }
                    }
                    NalInterest::Buffer
                },
                UnitType::SliceLayerWithoutPartitioningIdr => {
                    if nal.is_complete() && !pps.get_mut().is_empty() && !sps.get_mut().is_empty() {
                        if !first_keyframe {
                            first_keyframe = true;
                        }
                        let ts = now.elapsed().as_micros();
                        
                        if let Some(full_nal_buf) = read_nal_bytes(nal) {
                            let sps = sps.get_mut();
                            let pps = pps.get_mut();
                            let mut payload = BytesMut::new();
                            payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                            payload.extend_from_slice(&sps);
                            payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                            payload.extend_from_slice(&pps);
                            payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                            payload.extend_from_slice(&full_nal_buf);

                            let frame = Frame {
                                timestamp: Timestamp::from_micros(ts as u64),
                                keyframe: true,
                                payload: payload.freeze(),
                            };
                            
                            if let Err(e) = frame_tx.send(frame) {
                                eprintln!("Failed to send frame: {:?}", e);
                            }
                        }
                    }
                    NalInterest::Buffer
                },
                _ => {
                    NalInterest::Ignore
                },
            }
        });
        
        while let Some(buffer) = input.next().await {
            reader.push(&buffer);
            loop {
                match frame_rx.try_recv() {
                    Ok(f) => {
                        track.write(f);
                    },
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(e) => {
                        eprintln!("Channel error: {:?}", e);
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

// Helper function to read all bytes from a NAL
fn read_nal_bytes(nal: RefNal<'_>) -> Option<BytesMut> {
    let mut nal_reader = nal.reader();
    let mut full_nal_buf = BytesMut::new();
    let mut buf = [0u8; 1024];
    
    loop {
        match nal_reader.read(&mut buf) {
            Ok(n) => {
                if n == 0 {
                    break;
                } else {
                    full_nal_buf.extend_from_slice(&buf[..n]);
                }
            },
            Err(e) => {
                eprintln!("Error reading NAL: {:?}", e);
                return None;
            }
        }
    }
    
    Some(full_nal_buf)
}
}
