use anyhow::Result;

use ffutility::{encoders::FfmpegOptions, parsers::AnnexBStreamImport, streams::{V4lH264Stream, V4lH264Config}};

use moq_karp::BroadcastProducer;
use moq_native::{ClientConfig, ClientTls};
use moq_transfork::Session;

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

    // Step 1: Create client config with TLS settings
    let client_config = ClientConfig {
        bind: SocketAddr::from(([0, 0, 0, 0], 0)),
        tls: ClientTls {
            root: vec![],
            disable_verify: Some(true),
        },
    };

    // Step 2: Initialize client and connect to relay
    let client = client_config.init()?;
    let url = Url::parse("https://relay.quic.video")?;
    let web_transport_session = client.connect(url).await?;

    // Step 3: Create MoQ session from WebTransport session
    let session = Session::connect(web_transport_session).await?;

    // Step 4: Create broadcast producer with namespace
    let namespace = "test-zed".to_string();
    let broadcast = BroadcastProducer::new(session.clone(), namespace)?;

    // --- Major API Change End ---


    let mut annexb_import = AnnexBStreamImport::new(Arc::new(Mutex::new(broadcast)), 736, 414);

    let v4l_config = V4lH264Config {
        output_width: 736,
        output_height: 414,
        bitrate: 300000,
        video_dev: String::from("/dev/video4"),
    };

    let mut ffmpeg_opts = FfmpegOptions::new();
    ffmpeg_opts.push((String::from("preset"), String::from("superfast")));

    let mut rx_stream = V4lH264Stream::new(v4l_config, ffmpeg_opts)?;

    let mut track = annexb_import.init_from(&mut rx_stream).await?;

    tokio::select! {
        res = annexb_import.read_from(&mut rx_stream, &mut track) => Ok(res?),
        res = session.closed() => Err(res.into()),
    }
}