use anyhow::Result;

use ffutility::{encoders::FfmpegOptions, parsers::AnnexBStreamImport, streams::{V4lH264Stream, V4lH264Config}};

use moq_native::Session;

use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;

use url::Url;

#[tokio::main]
async fn main() -> Result<()> {
    // This setup initializes logging.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // The Session::connect function is the high-level entry point that handles
    // creating the underlying QUIC connection and MoQ session in one step.
    let url = Url::parse("https://relay.quic.video")?;
    let session = Session::connect(url).await?;

    // Announce a namespace on the session to get a publisher.
    // This allows you to create and send new tracks.
    let namespace = "test-zed".to_string();
    let publisher = session.announce(namespace).await?;

    // This part of the code uses your custom ffutility logic.
    // It wraps the publisher so it can be used to send video frames.
    let mut annexb_import = AnnexBStreamImport::new(Arc::new(Mutex::new(publisher)), 736, 414);

    // This configures the V4L2 device and Ffmpeg encoder settings.
    let v4l_config = V4lH264Config {
        output_width: 736,
        output_height: 414,
        bitrate: 300000,
        video_dev: String::from("/dev/video4"),
    };

    let mut ffmpeg_opts = FfmpegOptions::new();
    ffmpeg_opts.push((String::from("preset"), String::from("superfast")));

    // Create the video stream from your V4L device.
    let mut rx_stream = V4lH264Stream::new(v4l_config, ffmpeg_opts)?;

    // Initialize the MoQ track from the video stream's initial data.
    let mut track = annexb_import.init_from(&mut rx_stream).await?;

    // This loop runs two tasks concurrently:
    // 1. Reads video frames from your camera and sends them to the relay.
    // 2. Watches for the session to be closed by the remote server.
    tokio::select! {
        res = annexb_import.read_from(&mut rx_stream, &mut track) => Ok(res?),
        res = session.closed() => Err(res.into()),
    }
}