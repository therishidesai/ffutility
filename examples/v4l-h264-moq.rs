#![cfg(all(target_os = "linux", feature = "v4l"))]

use anyhow::{Context, Result};

use ffutility::{
    encoders::{FfmpegOptions, InputType},
    streams::{LoadingImage, V4lH264Config, V4lH264Stream},
};

use hang::{annexb::Import, moq_lite};
use image::{imageops::FilterType, ImageFormat};
use moq_native::client::{ClientConfig, ClientTls};
use std::path::Path;
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
        tls,
    }
    .init()?;

    let broadcast = moq_lite::Broadcast::produce();

    let session = quic_client
        .connect(Url::parse("https://localhost:4443/sartv").unwrap())
        .await?;

    // Create an origin producer to publish to the broadcast.
    let origin = moq_lite::Origin::produce();
    origin
        .producer
        .publish_broadcast("test-zed", broadcast.consumer);

    let session = moq_lite::Session::connect(session, origin.consumer, None).await?;

    let mut import = Import::new(broadcast.producer);

    let v4l_config = V4lH264Config {
        output_width: 736,
        output_height: 414,
        bitrate: 300000,
        video_dev: String::from("/dev/video4"),
        v4l_fourcc: v4l::FourCC::new(b"RGB3"),
        loading_image: Some(load_loading_image(Path::new("./test-image.png"))?),
    };

    let mut ffmpeg_opts = FfmpegOptions::new();
    ffmpeg_opts.push((String::from("preset"), String::from("superfast")));

    let mut rx_stream = V4lH264Stream::new(v4l_config, ffmpeg_opts)?;

    tokio::select! {
        res = import.read_from(&mut rx_stream) => Ok(res?),
        res = session.closed() => Ok(res?),
    }
}

fn load_loading_image(path: &Path) -> Result<LoadingImage> {
    let image = image::open(path)
        .with_context(|| format!("failed to open loading image at {}", path.display()))?;

    let resized = image.to_rgba8();

    let data = resized.clone().into_raw();

    Ok(LoadingImage {
        data,
        input_width: resized.width(),
        input_height: resized.height(),
        input_type: InputType::RGBA,
    })
}
