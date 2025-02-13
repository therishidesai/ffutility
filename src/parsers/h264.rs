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
use std::io::Read;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, mpsc::channel};

pub struct AnnexBStreamImport {
    broadcast: Arc<Mutex<BroadcastProducer>>,
    codec: Option<H264>,
    ctx: Option<Context>,
}

impl AnnexBStreamImport {
    pub fn new(broadcast: Arc<Mutex<BroadcastProducer>>) -> Self {
        Self {
            broadcast,
            codec: None,
            ctx: None,
        }
    }

    pub async fn init_from<T: Stream<Item = BytesMut> + Unpin>(&mut self, input: &mut T) -> Result<TrackProducer> {
        let mut ctx = Context::new();
        let mut sps: Option<SeqParameterSet> = None;
        let found_sps = AtomicBool::new(false);
        let found_pps = AtomicBool::new(false);

        let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
            let nal_unit_type = nal.header().unwrap().nal_unit_type();
            match nal_unit_type {
                UnitType::SeqParameterSet => {
                    let sps_local= SeqParameterSet::from_bits(nal.rbsp_bits()).unwrap();
                    ctx.put_seq_param_set(sps_local.clone());
                    sps = Some(sps_local);
                    found_sps.store(true, Ordering::SeqCst);
                    NalInterest::Buffer
                },
                UnitType::PicParameterSet => {
                    let pps = PicParameterSet::from_bits(&ctx, nal.rbsp_bits()).unwrap();
                    ctx.put_pic_param_set(pps);
                    found_pps.store(true, Ordering::SeqCst);
                    NalInterest::Buffer
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

            // let description = BytesMut::new();
            let track = Video {
                track: Track { name: String::from("video0"), priority: 2 },
                resolution: Dimensions {
                    width: 2560,
                    height: 720,
                },
                codec: codec.into(),
                description: None,
                bitrate: None,
            };

            let mut broadcast = self.broadcast.lock().unwrap();
            let track = broadcast.publish_video(track).unwrap();
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

        let ctx = self.ctx.as_ref().unwrap();
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
            let nal_unit_type = nal.header().unwrap().nal_unit_type();
            match nal_unit_type {
                UnitType::PicParameterSet => {
                    if nal.is_complete() {
                        let mut nal_reader = nal.reader();
                        let mut full_nal_buf = BytesMut::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let n = nal_reader.read(&mut buf).unwrap();
                            if n == 0 {
                                break
                            } else {
                                full_nal_buf.extend_from_slice(&buf);
                            }
                        }
                        pps.set(full_nal_buf);
                    }
                    NalInterest::Buffer
                }
                UnitType::SeqParameterSet => {
                    if nal.is_complete() {
                        let mut nal_reader = nal.reader();
                        let mut full_nal_buf = BytesMut::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let n = nal_reader.read(&mut buf).unwrap();
                            if n == 0 {
                                break
                            } else {
                                full_nal_buf.extend_from_slice(&buf);
                            }
                        }

                        sps.set(full_nal_buf);
                    }
                    NalInterest::Buffer
                },
                UnitType::SliceLayerWithoutPartitioningNonIdr => {
                    if nal.is_complete() && first_keyframe {
                        let ts = now.elapsed().as_micros();

                        let slice_header = SliceHeader::from_bits(&ctx, &mut nal.rbsp_bits(), nal.header().unwrap()).unwrap().0;
                        let keyframe = slice_header.slice_type.family == SliceFamily::I;

                        let mut nal_reader = nal.reader();
                        let mut full_nal_buf = BytesMut::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let n = nal_reader.read(&mut buf).unwrap();
                            if n == 0 {
                                break
                            } else {
                                full_nal_buf.extend_from_slice(&buf);
                            }
                        }

                        if !keyframe {
                            let mut payload = BytesMut::new();
                            payload.extend_from_slice(&[0u8, 0u8, 0u8, 1u8]);
                            payload.extend_from_slice(&full_nal_buf);
                            let frame = Frame {
                                timestamp: Timestamp::from_micros(ts as u64),
                                keyframe,
                                payload: payload.freeze(),
                            };
                            frame_tx.send(frame).unwrap();
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
                            frame_tx.send(frame).unwrap();
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
                        let mut nal_reader = nal.reader();
                        let mut full_nal_buf = BytesMut::new();
                        let mut buf = [0u8; 1024];
                        loop {
                            let n = nal_reader.read(&mut buf).unwrap();
                            if n == 0 {
                                break
                            } else {
                                full_nal_buf.extend_from_slice(&buf);
                            }
                        }

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
                        frame_tx.send(frame).unwrap();
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
                    Err(_) => panic!("frame_rx channel disconnected"),
                }
            }
        }

        Ok(())
    }
}
