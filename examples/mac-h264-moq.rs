#![cfg(target_os = "macos")]

//! Read H.264 data from stdin and stream via MoQ.
//! 
//! Usage:
//! ```bash
//! # Webcam streaming
//! ffmpeg -f avfoundation -i "0" -f h264 - | cargo run --example mac-h264-moq
//! 
//! # File streaming  (use this command, it works better)
//! ffmpeg -i test.mp4 -f h264 - | cargo run --example mac-h264-moq
//! ```
use anyhow::Result;
use bytes::BytesMut;
use ffutility::parsers::AnnexBStreamImport;
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

/// A stream that reads H.264 data from stdin and yields it as BytesMut chunks.
pub struct StdinH264Stream {}

impl StdinH264Stream {
    /// Creates a new stream that reads H.264 data from stdin.
    pub fn new() -> Result<ReceiverStream<BytesMut>> {
        let (tx, rx) = mpsc::channel::<BytesMut>(10);

        thread::spawn(move || {
            let mut stdin = io::stdin();
            let mut buffer = vec![0u8; 8192]; // 8KB chunks

            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) => {
                        break;
                    }
                    Ok(bytes_read) => {
                        let mut chunk = BytesMut::with_capacity(bytes_read);
                        chunk.extend_from_slice(&buffer[..bytes_read]);
                        
                        if tx.blocking_send(chunk).is_err() {
                            break;
                        }
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
    let mut rx_stream = StdinH264Stream::new()?;
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