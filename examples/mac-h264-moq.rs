#![cfg(target_os = "macos")]

//! Read raw video data from stdin, encode to H.264, and stream via MoQ.
//! 
//! Usage:
//! ```bash
//! # Webcam streaming (raw YUV420P)
//! ffmpeg -stream_loop -1 -i test.mp4 -f rawvideo -pix_fmt yuv420p - | cargo run --example mac-h264-moq
//! 
//! # File streaming (raw YUV420P)
//! ffmpeg -i test.mp4 -f rawvideo -pix_fmt yuv420p - | cargo run --example mac-h264-moq
//! ```
use anyhow::Result;
use bytes::BytesMut;
use ffutility::parsers::AnnexBStreamImport;
use ffutility::encoders::{EncoderConfig, EncoderType, H264Encoder, InputType};
use std::io::{self, Read};
use std::thread;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

// MoQ imports
use moq_native::client;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use url::Url;

/// A stream that reads raw video data from stdin and encodes it to H.264.
pub struct StdinH264Stream {}

impl StdinH264Stream {
    /// Creates a new stream that reads raw video from stdin and encodes to H.264.
    /// Expects raw video data (not H.264) from ffmpeg.
    pub fn new(width: u32, height: u32, input_type: InputType) -> Result<ReceiverStream<BytesMut>> {
        let (tx, rx) = mpsc::channel::<BytesMut>(10);

        thread::spawn(move || {
            let frame_size = match input_type {
                InputType::YUV420P => (width * height * 3) / 2,
                InputType::NV12 => (width * height * 3) / 2,
                InputType::RGB24 => width * height * 3,
                InputType::BGR24 => width * height * 3,
                _ => {
                    eprintln!("Unsupported input type: {:?}", input_type);
                    return;
                }
            } as usize;
            

            let ec = EncoderConfig {
                input_width: width,
                input_height: height,
                output_width: width,
                output_height: height,
                framerate: 30,
                gop: None,
                bitrate: 2000000,
                disable_b_frames: false,
                enc_type: EncoderType::X264,
                input_type,
            };

            let mut encoder = match H264Encoder::new(ec, &vec![]) {
                Ok(enc) => enc,
                Err(e) => {
                    eprintln!("Failed to create H264Encoder: {:?}", e);
                    return;
                }
            };

            let mut stdin = io::stdin();
            let mut buffer = vec![0u8; frame_size];
            let mut pts = 0;

            loop {
                match stdin.read_exact(&mut buffer) {
                    Ok(()) => {
                        match encoder.encode_raw(Some(pts), &buffer) {
                            Ok(Some(encoded_frame)) => {
                                if tx.blocking_send(encoded_frame.nal_bytes).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {
                                // Encoder buffering
                            }
                            Err(_) => break,
                        }
                        pts += 1;
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }
}

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

    let url = Url::parse("https://127.0.0.1:4443/anon/test-mac").unwrap();

    // Perform the web_transport handshake.
    let session: web_transport_quinn::Session = quic_client.connect(url).await?;

    // Perform the moq handshake.
    let mut session: moq_lite::Session = moq_lite::Session::connect(session).await?;

    // Publish the broadcast
    session.publish("", broadcast.inner.consume());
    let mut annexb_import = AnnexBStreamImport::new(Arc::new(Mutex::new(broadcast)), 736, 414);

    // Create stdin H.264 stream
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