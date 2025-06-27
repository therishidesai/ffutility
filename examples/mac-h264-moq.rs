#![cfg(target_os = "macos")]

//! Read raw video data from stdin, encode to H.264, and stream via MoQ.
//! 
//! Usage:
//! ```bash
//! # Webcam streaming (raw YUV420P)
//! ffmpeg -f avfoundation -r 30 -s 1920x1080 -i "0" -f rawvideo -pix_fmt yuv420p - | cargo run --example mac-h264-moq
//! 
//! # File streaming (raw YUV420P)
//! ffmpeg -stream_loop -1 -i test.mp4 -f rawvideo -pix_fmt yuv420p - | cargo run --example mac-h264-moq
//! ```
//! 
use anyhow::Result;
use bytes::BytesMut;
use ffutility::parsers::AnnexBStreamImport;
use ffutility::encoders::InputType;
use ffutility::streams::StdinH264Stream;

// MoQ imports
use moq_native::client;
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
    
    let mut tls = moq_native::client::ClientTls::default();
    tls.disable_verify = Some(true);

    let quic_client = client::Client::new(client::ClientConfig {
        bind: SocketAddr::from(([0, 0, 0, 0], 0)),
        tls    
    })?;
    
    //TODO: fix this code, the code up to 236 is a bit atrocious (moq-lite and hang 
    //both have broadcastconsumer and broadcastproducer, kixelated wrote the comments)

    // NOTE: You can now start producing a broadcast even when the session is severed
    let broadcast: hang::BroadcastProducer = hang::BroadcastProducer::new();

    let url = Url::parse("https://127.0.0.1:4443/demo/").unwrap();

    // Perform the web_transport handshake.
    let session: web_transport_quinn::Session = quic_client.connect(url).await?;

    // Perform the moq handshake.
    let mut session: moq_lite::Session = moq_lite::Session::connect(session).await?;

    // Publish the broadcast
    session.publish("test", broadcast.inner.consume());
    let mut annexb_import = AnnexBStreamImport::new(Arc::new(Mutex::new(broadcast)), 736, 414);

    // Create stdin H.264 stream
    // TODO: change this so that it isn't hardcoded but rather can automatically detect the resolution, current resolution is webcam
    let mut rx_stream = StdinH264Stream::new(1920, 1080, InputType::YUV420P)?;
    let mut track = annexb_import.init_from(&mut rx_stream).await?;

    tokio::select! {
        res = annexb_import.read_from(&mut rx_stream, &mut track) => {
            Ok(res?)
        },
        res = session.closed() => {
            Err(res.into())
        },
    }
}