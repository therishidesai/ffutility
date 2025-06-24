use anyhow::Result;

use ffutility::{encoders::FfmpegOptions, parsers::AnnexBStreamImport, streams::{V4lH264Stream, V4lH264Config}};

use moq_native::client::Client;

use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;

use url::Url;

#[tokio::main]
async fn main() -> Result<()> {
    // Tracing/logging setup remains the same.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // --- Major API Change Start ---

    // 1. Connection and session setup is now much simpler.
    // The old method of manually creating a TLS config and a QUIC endpoint is gone.
    // Now, you create a ClientConfig and use the high-level `Client::connect`.
    let url = Url::parse("https://relay.quic.video")?;
    let client = Client::connect(url).await?;

    // The `Client` object you get back IS the session. There's no separate `moq_transfork::Session`.

    // 2. The concept of a "producer" or "broadcast" is now built into the client.
    // You "announce" a namespace to the relay to get a `Publisher` object.
    // This replaces `moq_karp::BroadcastProducer` and `moq_transfork::Path`.
    let namespace = "test-zed".to_string();

    let publisher = client.announce(namespace).await?;

    // --- Major API Change End ---


    let mut annexb_import = AnnexBStreamImport::new(Arc::new(Mutex::new(publisher)), 736, 414);

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
        res = client.closed() => Err(res.into()),
    }
}