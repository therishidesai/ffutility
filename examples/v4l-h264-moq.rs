#![cfg(target_os = "linux")]

use anyhow::Result;

use ffutility::{encoders::FfmpegOptions, parsers::AnnexBStreamImport, streams::{V4lH264Stream, V4lH264Config}};

use hang::BroadcastProducer;
use moq_native::quic;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;

use url::Url;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let mut tls_args = moq_native::tls::Args::default();
    tls_args.disable_verify = true;
    let tls = tls_args.load()?;
    let quic_client = quic::Endpoint::new(quic::Config {
        bind: SocketAddr::from(([0, 0, 0, 0], 0)),
        tls
    })?;

    let session = quic_client.client.connect(Url::parse("https://relay.quic.video").unwrap()).await?;

    let session = moq_lite::Session::connect(session.into()).await?;

    let path = moq_lite::Path::new().push("test-zed");

    let broadcast = BroadcastProducer::new(session.clone(), path)?;

    let mut annexb_import = AnnexBStreamImport::new(Arc::new(Mutex::new(broadcast)), 736, 414);

    let v4l_config = V4lH264Config {
        output_width: 736,
        output_height: 414,
        bitrate: 300000, // bitrate
        video_dev: String::from("/dev/video4"),
    };

    let mut ffmpeg_opts = FfmpegOptions::new();
    ffmpeg_opts.push((String::from("preset"), String::from("superfast")));

    let mut rx_stream = V4lH264Stream::new(v4l_config, ffmpeg_opts)?;
    // let mut rx_reader = StreamReader::new(rx_stream);

    let mut track = annexb_import.init_from(&mut rx_stream).await?;
    eprintln!("init track");
    // annexb_import.publish(&mut session)?;

    tokio::select! {
        res = annexb_import.read_from(&mut rx_stream, &mut track) => Ok(res?),
        res = session.closed() => Err(res.into()),
    }
}
