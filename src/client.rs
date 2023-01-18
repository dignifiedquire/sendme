use std::time::Duration;
use std::{net::SocketAddr, time::Instant};

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use futures::{AsyncReadExt, Stream};
use postcard::experimental::max_size::MaxSize;
use s2n_quic::Connection;
use s2n_quic::{client::Connect, Client};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::compat::*;
use tracing::debug;

use crate::protocol::{read_lp_data, write_lp, Handshake, Request, Res, Response};
use crate::tls::{self, Keypair};

const MAX_DATA_SIZE: usize = 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Options {
    pub addr: SocketAddr,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            addr: "127.0.0.1:4433".parse().unwrap(),
        }
    }
}

/// Setup a QUIC connection to the provided server address
async fn setup(server_addr: SocketAddr) -> Result<(Client, Connection)> {
    let keypair = Keypair::generate();

    let client_config = tls::make_client_config(&keypair, None)?;
    let tls = s2n_quic::provider::tls::rustls::Client::from(client_config);

    let client = Client::builder()
        .with_tls(tls)?
        .with_io("0.0.0.0:0")?
        .start()
        .map_err(|e| anyhow!("{:?}", e))?;

    debug!("client: connecting to {}", server_addr);
    let connect = Connect::new(server_addr).with_server_name("localhost");
    let mut connection = client.connect(connect).await?;

    connection.keep_alive(true)?;
    Ok((client, connection))
}

/// Stats about the transfer.
pub struct Stats {
    pub data_len: usize,
    pub elapsed: Duration,
    pub mbits: f64,
}

pub enum Event {
    Connected,
    Requested { size: usize },
    Done(Stats),
}

pub fn run<D: AsyncWrite + Unpin>(
    hash: bao::Hash,
    opts: Options,
    mut dest: D,
) -> impl Stream<Item = Result<Event>> {
    async_stream::try_stream! {
        let now = Instant::now();
        let (_client, mut connection) = setup(opts.addr).await?;

        let stream = connection.open_bidirectional_stream().await?;
        let (mut reader, mut writer) = stream.split();

        yield Event::Connected;


        let mut out_buffer = BytesMut::zeroed(std::cmp::max(
            Request::POSTCARD_MAX_SIZE,
            Handshake::POSTCARD_MAX_SIZE,
        ));

        // 1. Send Handshake
        {
            debug!("sending handshake");
            let handshake = Handshake::default();
            let used = postcard::to_slice(&handshake, &mut out_buffer)?;
            write_lp(&mut writer, used).await?;
        }

        // 2. Send Request
        {
            debug!("sending request");
            let req = Request {
                id: 1,
                name: hash.into(),
            };

            let used = postcard::to_slice(&req, &mut out_buffer)?;
            write_lp(&mut writer, used).await?;
        }

        // 3. Read response
        {
            debug!("reading response");
            let mut in_buffer = BytesMut::with_capacity(1024);

            // read next message
            match read_lp_data(&mut reader, &mut in_buffer).await? {
                Some(response_buffer) => {
                    let response: Response = postcard::from_bytes(&response_buffer)?;
                    match response.data {
                        Res::Found { size, outboard } => {
                            yield Event::Requested { size };

                            // Need to read the message now
                            if size > MAX_DATA_SIZE {
                                Err(anyhow!("size too large: {} > {}", size, MAX_DATA_SIZE))?;
                            }

                            let concat_reader = in_buffer.chain(
                                reader.take((size - in_buffer.len()) as u64)
                            );
                            let mut decoder = bao::decode_fut::Decoder::new_outboard(
                                concat_reader,
                                outboard,
                                &hash,
                            ).compat();

                            tokio::io::copy(&mut decoder, &mut dest).await?;
                            dest.flush().await?;

                            // Shut down the stream
                            debug!("shutting down stream");
                            writer.close().await?;

                            let data_len = size;
                            let elapsed = now.elapsed();
                            let elapsed_s = elapsed.as_secs_f64();
                            let data_len_bit = data_len * 8;
                            let mbits = data_len_bit as f64 / (1000. * 1000.) / elapsed_s;

                            let stats = Stats {
                                data_len,
                                elapsed,
                                mbits,
                            };

                            yield Event::Done(stats);
                        }
                        Res::NotFound => {
                            Err(anyhow!("data not found"))?;
                        }
                    }
                }
                None => {
                    Err(anyhow!("server disconnected"))?;
                }
            }
        }
    }
}
