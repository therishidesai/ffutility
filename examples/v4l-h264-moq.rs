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

use tracing_subscriber::EnvFilter;

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
                        eprintln!("got pps, {}", full_nal_buf.is_empty());
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

                        eprintln!("got sps, {}", full_nal_buf.is_empty());
                        sps.set(full_nal_buf);
                    }
                    NalInterest::Buffer
                },
                UnitType::SliceLayerWithoutPartitioningNonIdr => {
                    if nal.is_complete() {
                        let ts = now.elapsed().as_micros();

                        let slice_header = SliceHeader::from_bits(&ctx, &mut nal.rbsp_bits(), nal.header().unwrap()).unwrap().0;
                        eprintln!("slice type: {:?}", slice_header.slice_type.family);
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
                            let frame = Frame {
                                timestamp: Timestamp::from_micros(ts as u64),
                                keyframe,
                                payload: full_nal_buf.freeze(),
                            };
                            frame_tx.send(frame).unwrap();
                        } else if keyframe && !pps.get_mut().is_empty() && !sps.get_mut().is_empty() {
                            let sps = sps.get_mut();
                            let pps = pps.get_mut();
                            let mut payload = BytesMut::new();
                            payload.extend_from_slice(&sps);
                            payload.extend_from_slice(&pps);
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
                    eprintln!("got IDR");
                    if nal.is_complete() && !pps.get_mut().is_empty() && !sps.get_mut().is_empty() {
                        eprintln!("FULL NAL and SPS and PPS");
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
                        payload.extend_from_slice(&sps);
                        payload.extend_from_slice(&pps);
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
                    eprintln!("got other frames!");
                    NalInterest::Ignore
                },
            }
        });

        let mut buffer = BytesMut::new();

        
        
        while input.read_buf(&mut buffer).await? > 0 {
            reader.push(&buffer);
            loop {
                match frame_rx.try_recv() {
                    Ok(f) => track.write(f),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => panic!("frame_rx channel disconnected"),
                }
            }
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
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
