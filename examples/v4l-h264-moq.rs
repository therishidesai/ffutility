#![cfg(all(target_os = "linux", feature = "v4l"))]

use anyhow::Result;

use ffutility::{encoders::FfmpegOptions, streams::{V4lH264Stream, V4lH264Config}};

use hang::moq_lite;
use hang::annexb::Import;

use moq_native::client::{ClientTls, ClientConfig};

use tracing_subscriber::EnvFilter;

use url::Url;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let mut tls = ClientTls::default();
    tls.disable_verify = Some(true);

    let quic_client = ClientConfig {
        bind: "[::]:0".parse().unwrap(),
        tls    
    }.init()?;

    let broadcast = moq_lite::Broadcast::produce();

    let session = quic_client.connect(Url::parse("http://localhost:4443/anon").unwrap()).await?;

    // Create an origin producer to publish to the broadcast.
    let origin = moq_lite::Origin::produce();
    origin.producer.publish_broadcast("bbb", broadcast.consumer);

    let session = moq_lite::Session::connect(session, origin.consumer, None).await?;

    let mut import = Import::new(broadcast.producer);

    let v4l_config = V4lH264Config {
        output_width: 736,
        output_height: 414,
        bitrate: 300000, // bitrate
        video_dev: String::from("/dev/video0"),
    };

    let mut ffmpeg_opts = FfmpegOptions::new();
    ffmpeg_opts.push((String::from("preset"), String::from("superfast")));

    let mut rx_stream = V4lH264Stream::new(v4l_config, ffmpeg_opts)?;

    // let _ = import.init_from(&mut rx_stream).await?;

    tokio::select! {
        res = import.read_from(&mut rx_stream) => Ok(res?),
        res = session.closed() => Err(res.into()),
    }
}
