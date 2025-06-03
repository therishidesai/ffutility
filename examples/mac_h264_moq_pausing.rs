#![cfg(target_os = "macos")]

use anyhow::Result;
use bytes::BytesMut;
use futures::StreamExt;
use tokio_stream::Stream;
use tracing_subscriber::EnvFilter;

use ffutility::parsers::AnnexBStreamImport;
use moq_karp::BroadcastProducer;
use moq_native::quic;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use url::Url;

// Import our macOS raw→H.264 source
use ffutility::streams::{MacRawH264Config, MacRawH264Stream};
use ffutility::encoders::{EncoderType, InputType};

#[tokio::main]
async fn main() -> Result<()> {
    // 1) Setup logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // 2) QUIC + broadcast setup (same as before)
    let mut tls_args = moq_native::tls::Args::default();
    tls_args.disable_verify = true;
    let tls = tls_args.load()?;
    let quic_client = quic::Endpoint::new(quic::Config {
        bind: "0.0.0.0:0".parse().unwrap(),
        tls,
    })?;
    let session = quic_client
        .client
        .connect(Url::parse("https://relay.quic.video").unwrap())
        .await?;
    let session = moq_transfork::Session::connect(session).await?;
    let path = moq_transfork::Path::new().push("test-zed");
    let broadcast = BroadcastProducer::new(session.clone(), path)?;
    
    let pause_flag = Arc::new(AtomicBool::new(false));
    // 3) Create AnnexBStreamImport
    let mut annexb_import =
        AnnexBStreamImport::new(Arc::new(Mutex::new(broadcast)), /*width=*/640, /*height=*/480, pause_flag.clone());

    // 4) Build macOS capture config, matching a supported mode:
    //
    //    - We pick 640×480 @30fps UYVY422 because the camera advertises that exactly.
    //    - Then our H264Encoder scalers do UYVY422 → YUV420P internally.
    let mac_cfg = MacRawH264Config {
        device_index: "0".to_string(),    // use AVFoundation device “0”
        input_width:  640,
        input_height: 480,
        output_width:  640,
        output_height: 480,
        framerate: 30,                    // match “30 tbr” from camera
        bitrate: 300_000,                 // 300 kbps
        input_type: InputType::UYVY422,   // ← Must match ffmpeg’s “-pixel_format uyvy422”
        enc_type: EncoderType::X264,
        ffmpeg_opts: vec![(String::from("preset"), String::from("superfast"))],
    };

    // 5) Spawn ffmpeg→rawvideo thread + H264Encoder
    let mut rx_stream = MacRawH264Stream::new(mac_cfg)
        .expect("Failed to start macOS ffmpeg→raw → H264 pipeline");

    // 6) Initialize AnnexB importer (reads SPS/PPS out of the first few encoded NALs)
    let mut track = annexb_import.init_from(&mut rx_stream).await?;
    eprintln!("Initialized track (macOS camera → H.264)");
    
    let pause_task = async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            pause_flag.store(!pause_flag.load(std::sync::atomic::Ordering::Acquire), std::sync::atomic::Ordering::Relaxed);
            println!("Pause Flag: {:?}", pause_flag.load(std::sync::atomic::Ordering::Relaxed));
        }
    };
    // 7) Forward until session closes
    tokio::select! {
        res = pause_task => Ok(res),
        res = annexb_import.read_from(&mut rx_stream, &mut track) => Ok(res?),
        res = session.closed() => Err(res.into()),
    }
}