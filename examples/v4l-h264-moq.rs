use anyhow::{bail, Result};

use bytes::BytesMut;

use h264_reader::Context;
use h264_reader::annexb::AnnexBReader;
use h264_reader::nal::{pps::PicParameterSet, slice::{SliceFamily, SliceHeader}, sps::SeqParameterSet, Nal, RefNal, UnitType};
use h264_reader::push::NalInterest;

use moq_karp::{catalog::{Dimensions, H264, Track, Video}, media::{Frame, Timestamp}, produce};
use moq_native::quic;

use std::cell::Cell;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::mpsc::channel;

use tokio::io::{AsyncRead, AsyncReadExt};

use url::Url;

struct AnnexBImport {
    broadcast: produce::Broadcast,
    codec: Option<H264>,
    ctx: Option<Context>,
}

impl AnnexBImport {
    fn publish(&mut self, session: &mut moq_transfork::Session) -> Result<()> {
        if self.codec.is_none() {
            bail!("AnnexBImport not initialized");
        }

        self.broadcast.publish(session)?;

        Ok(())
    }

    async fn init_from<T: AsyncRead + Unpin>(&mut self, input: &mut T) -> Result<produce::Track> {
        let mut ctx = Context::new();
        let mut sps: Option<SeqParameterSet> = None;
        let found_sps = Cell::new(false);
        let found_pps = Cell::new(false);

        let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
            let nal_unit_type = nal.header().unwrap().nal_unit_type();
            match nal_unit_type {
                UnitType::SeqParameterSet => {
                    let sps_local= SeqParameterSet::from_bits(nal.rbsp_bits()).unwrap();
                    ctx.put_seq_param_set(sps_local.clone());
                    sps = Some(sps_local);
                    found_sps.set(true);
                    NalInterest::Buffer
                },
                UnitType::PicParameterSet => {
                    let pps = PicParameterSet::from_bits(&ctx, nal.rbsp_bits()).unwrap();
                    ctx.put_pic_param_set(pps);
                    found_pps.set(true);
                    NalInterest::Buffer
                },
                _ => NalInterest::Ignore,
            }
        });

        let mut buffer = BytesMut::new();
        
        while !found_pps.get() && !found_sps.get() && input.read_buf(&mut buffer).await? > 0 {
            reader.push(&buffer);
        }

        if let Some(sps) = sps {
            let codec = H264 {
                profile: sps.profile().profile_idc(),
                constraints: sps.constraint_flags.reserved_zero_two_bits(),
                level: sps.level_idc,
            };
            self.codec = Some(codec.clone());

            let description = BytesMut::new();
            let track = Video {
                track: Track { name: String::from("video0"), priority: 2 },
                resolution: Dimensions {
                    width: 2560,
                    height: 720,
                },
                codec: codec.into(),
                description: description.freeze(),
                bitrate: None,
            };

            let track = self.broadcast.create_video(track).unwrap();
            self.ctx = Some(ctx);

            Ok(track)
        } else {
            bail!("Failed to find valid SPS in input!");
        }


    }

    async fn read_from<T: AsyncRead + Unpin>(&mut self, input: &mut T, track: &mut produce::Track) -> Result<()> {
        if self.ctx.is_none() || self.codec.is_none() {
            bail!("AnnexBImport not initialized");
        }

        let ctx = self.ctx.as_ref().unwrap();
        // NOTE: Have to use a sync channel since I want to mutate
        // both in the AnnexBReader accumulate closure and out of the
        // closure. There is probably a better way of doing this. The
        // h264_reader library has some weird API choices so it may
        // also be worth writing our own h264 NAL parser.
        let (full_nal_tx, full_nal_rx) = channel();
        let is_keyframe = Cell::new(false);
        let found_nal = Cell::new(false);
        let mut found_sps = Cell::new(false);
        let mut found_pps = Cell::new(false);

        let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
            let nal_unit_type = nal.header().unwrap().nal_unit_type();
            match nal_unit_type {
                UnitType::SeqParameterSet | UnitType::PicParameterSet => {
                    if nal_unit_type == UnitType::SeqParameterSet {
                        found_sps.set(true);
                    }

                    if nal_unit_type == UnitType::PicParameterSet {
                        found_pps.set(true);
                    }
                    eprintln!("got sps/pps");
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

                        full_nal_tx.send(full_nal_buf).unwrap();
                    }
                    NalInterest::Buffer
                },
                UnitType::SliceLayerWithoutPartitioningNonIdr => {
                    if nal.is_complete() {
                        found_nal.set(true);
                        let slice_header = SliceHeader::from_bits(&ctx, &mut nal.rbsp_bits(), nal.header().unwrap()).unwrap().0;
                        if slice_header.slice_type.family == SliceFamily::I {
                            is_keyframe.set(true);
                        }

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

                        full_nal_tx.send(full_nal_buf).unwrap();
                    }
                    NalInterest::Buffer
                },
                UnitType::SliceLayerWithoutPartitioningIdr => {
                    if nal.is_complete() {
                        found_nal.set(true);
                        is_keyframe.set(true);

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
                        full_nal_tx.send(full_nal_buf).unwrap();
                    }

                    NalInterest::Buffer
                },
                _ => {
                    eprintln!("got other frames!");
                    NalInterest::Ignore
                },
            }
        });

        let mut buffer = BytesMut::new();
        let now = std::time::Instant::now();
        
        
        while input.read_buf(&mut buffer).await? > 0 {
            reader.push(&buffer);

            if found_nal.get() && found_sps.get() && found_pps.get() {
                println!("Got a full NAL, is_keyframe: {}", is_keyframe.get());

                let ts = now.elapsed().as_micros();

                let mut nal_payload = BytesMut::new();

                loop {
                    match full_nal_rx.try_recv() {
                        Ok(nal) => {
                            nal_payload.extend_from_slice(&nal);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            break
                        }
                        Err(_) => {
                            panic!("mpsc crashed");
                        }
                    }
                }


                // println!("NAL Payload: {:?}", nal_payload);

                let frame = Frame {
                    timestamp: Timestamp::from_micros(ts as u64),
                    keyframe: is_keyframe.get(),
                    payload: nal_payload.freeze(),
                };
                track.write(frame);
                found_nal.set(false);
                found_sps.set(false);
                found_pps.set(false);
                is_keyframe.set(false);
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let tls = moq_native::tls::Args::default().load()?;
    let quic_client = quic::Endpoint::new(quic::Config {
        bind: SocketAddr::from(([0, 0, 0, 0], 0)),
        tls
    })?;

    let session = quic_client.client.connect(&Url::parse("https://relay.quic.video").unwrap()).await?;
    let mut session = moq_transfork::Session::connect(session).await?;

    let path = moq_transfork::Path::new(vec!["test-zed"]);

    let broadcast = produce::Resumable::new(path).broadcast();

    let mut annexb_import = AnnexBImport {
        broadcast,
        codec: None,
        ctx: None
    };

    let mut input = tokio::io::stdin();

    let mut track = annexb_import.init_from(&mut input).await?;
    println!("{:?}", annexb_import.codec);

    annexb_import.publish(&mut session)?;

    tokio::select! {
        res = annexb_import.read_from(&mut input, &mut track) => Ok(res?),
        res = session.closed() => Err(res.into()),
    }
}
