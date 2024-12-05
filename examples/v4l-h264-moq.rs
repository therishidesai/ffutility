use anyhow::Result;

use ffutility::{encoders::{EncoderConfig, EncoderType, FfmpegOptions, InputType}, parsers::AnnexBStreamImport, streams::V4lH264Stream};

use moq_karp::BroadcastProducer;
use moq_native::quic;

use std::net::SocketAddr;

use tracing_subscriber::EnvFilter;

use url::Url;

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
    let session = moq_transfork::Session::connect(session).await?;

    let path = moq_transfork::Path::new().push("test-zed");

    let broadcast = BroadcastProducer::new(session.clone(), path)?;

    let mut annexb_import = AnnexBStreamImport::new(broadcast);

    let encoder_config = EncoderConfig {
        input_width: 2560,
        input_height: 720,
        output_width: 736,
        output_height: 414,
        framerate: 30,
        gop: Some(8),    // group of pictures, 2 keyframes/s
        bitrate: 300000, // bitrate
        disable_b_frames: false,
        enc_type: EncoderType::X264,
        input_type: InputType::YUYV422,
    };

    let mut ffmpeg_opts = FfmpegOptions::new();
    ffmpeg_opts.set("preset", "superfast");

    let mut rx_stream = V4lH264Stream::new(encoder_config, ffmpeg_opts)?;
    // let mut rx_reader = StreamReader::new(rx_stream);

    let mut track = annexb_import.init_from(&mut rx_stream).await?;
    eprintln!("init track");
    // annexb_import.publish(&mut session)?;

    tokio::select! {
        res = annexb_import.read_from(&mut rx_stream, &mut track) => Ok(res?),
        res = session.closed() => Err(res.into()),
    }
}
