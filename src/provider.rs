use std::net::SocketAddr;
use std::path::PathBuf;
use std::{collections::HashMap, sync::Arc};

use anyhow::{anyhow, bail, ensure, Result};
use bytes::{Bytes, BytesMut};
use s2n_quic::stream::BidirectionalStream;
use s2n_quic::Server as QuicServer;
use tokio::io::AsyncWrite;
use tokio_util::io::SyncIoBridge;
use tracing::{debug, warn};

use crate::protocol::{read_lp, write_lp, AuthToken, Handshake, Request, Res, Response, VERSION};
use crate::tls::{self, Keypair, PeerId};

#[derive(Clone, Debug)]
pub struct Options {
    /// Address to listen on.
    pub addr: SocketAddr,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            addr: "127.0.0.1:4433".parse().unwrap(),
        }
    }
}

const MAX_CONNECTIONS: u64 = 1024;
const MAX_STREAMS: u64 = 10;

pub type Database = Arc<HashMap<bao::Hash, Data>>;

pub struct Provider {
    keypair: Keypair,
    auth_token: AuthToken,
    db: Database,
}

impl Provider {
    pub fn new(db: Database) -> Self {
        let keypair = Keypair::generate();
        let auth_token = AuthToken::generate();
        Provider {
            keypair,
            db,
            auth_token,
        }
    }

    pub fn peer_id(&self) -> PeerId {
        self.keypair.public().into()
    }

    pub fn auth_token(&self) -> AuthToken {
        self.auth_token
    }

    pub fn set_auth_token(&mut self, auth_token: AuthToken) {
        self.auth_token = auth_token;
    }

    pub async fn run(&mut self, opts: Options) -> Result<()> {
        let server_config = tls::make_server_config(&self.keypair)?;
        let tls = s2n_quic::provider::tls::rustls::Server::from(server_config);
        let limits = s2n_quic::provider::limits::Limits::default()
            .with_max_active_connection_ids(MAX_CONNECTIONS)?
            .with_max_open_local_bidirectional_streams(MAX_STREAMS)?
            .with_max_open_remote_bidirectional_streams(MAX_STREAMS)?;

        let mut server = QuicServer::builder()
            .with_tls(tls)?
            .with_io(opts.addr)?
            .with_limits(limits)?
            .start()
            .map_err(|e| anyhow!("{:?}", e))?;
        let token = self.auth_token;
        debug!("\nlistening at: {:#?}", server.local_addr().unwrap());

        while let Some(mut connection) = server.accept().await {
            let db = self.db.clone();
            tokio::spawn(async move {
                debug!("connection accepted from {:?}", connection.remote_addr());

                while let Ok(Some(stream)) = connection.accept_bidirectional_stream().await {
                    let db = db.clone();
                    tokio::spawn(async move {
                        if let Err(err) = handle_stream(db, token, stream).await {
                            warn!("error: {:#?}", err);
                        }
                        debug!("disconnected");
                    });
                }
            });
        }

        Ok(())
    }
}

async fn handle_stream(
    db: Arc<HashMap<bao::Hash, Data>>,
    token: AuthToken,
    stream: BidirectionalStream,
) -> Result<()> {
    debug!("stream opened from {:?}", stream.connection().remote_addr());
    let (mut reader, mut writer) = stream.split();
    let mut out_buffer = BytesMut::with_capacity(1024);
    let mut in_buffer = BytesMut::with_capacity(1024);

    // 1. Read Handshake
    debug!("reading handshake");
    if let Some((handshake, size)) = read_lp::<_, Handshake>(&mut reader, &mut in_buffer).await? {
        ensure!(
            handshake.version == VERSION,
            "expected version {} but got {}",
            VERSION,
            handshake.version
        );
        ensure!(handshake.token == token, "AuthToken mismatch");
        let _ = in_buffer.split_to(size);
    } else {
        bail!("no valid handshake received");
    }

    // 2. Decode protocol messages.
    loop {
        debug!("reading request");
        match read_lp::<_, Request>(&mut reader, &mut in_buffer).await? {
            Some((request, _size)) => {
                let name = bao::Hash::from(request.name);
                debug!("got request({}): {}", request.id, name.to_hex());

                match db.get(&name) {
                    Some(Data {
                        outboard,
                        path,
                        size,
                    }) => {
                        debug!("found {}", name.to_hex());
                        write_response(
                            &mut writer,
                            &mut out_buffer,
                            request.id,
                            Res::Found {
                                size: *size,
                                outboard: &[],
                            },
                        )
                        .await?;

                        debug!("writing data");
                        let file = tokio::fs::File::open(&path).await?;
                        let mut reader = tokio::io::BufReader::new(file);
                        let path = path.clone();
                        let outboard = outboard.clone();
                        let size = *size;
                        // need to thread the writer though the spawn_blocking, since
                        // taking a reference does not work. spawn_blocking requires
                        // 'static lifetime.
                        writer = tokio::task::spawn_blocking(move || {
                            let file_reader = std::fs::File::open(&path)?;
                            let outboard_reader = std::io::Cursor::new(outboard);
                            let mut wrapper = SyncIoBridge::new(&mut writer);
                            let mut slice_extractor = bao::encode::SliceExtractor::new_outboard(
                                file_reader,
                                outboard_reader,
                                0,
                                size as u64,
                            );
                            let _copied = std::io::copy(&mut slice_extractor, &mut wrapper)?;
                            std::io::Result::Ok(writer)
                        })
                        .await
                        .unwrap()?;
                        tokio::io::copy(&mut reader, &mut writer).await?;
                    }
                    None => {
                        debug!("not found {}", name.to_hex());
                        write_response(&mut writer, &mut out_buffer, request.id, Res::NotFound)
                            .await?;
                    }
                }

                debug!("finished response");
            }
            None => {
                break;
            }
        }
        in_buffer.clear();
    }

    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Data {
    /// Outboard data from bao.
    outboard: Bytes,
    /// Path to the original data, which must not change while in use.
    path: PathBuf,
    /// Size of the original data.
    size: usize,
}

#[derive(Debug)]
pub enum DataSource {
    File(PathBuf),
}

pub async fn create_db(data_sources: Vec<DataSource>) -> Result<Arc<HashMap<bao::Hash, Data>>> {
    println!("Available Data:");

    let mut db = HashMap::new();
    for data in data_sources {
        match data {
            DataSource::File(path) => {
                ensure!(
                    path.is_file(),
                    "can only transfer blob data: {}",
                    path.display()
                );
                let data = tokio::fs::read(&path).await?;
                let (outboard, hash) = bao::encode::outboard(&data);

                println!("- {}: {}bytes", hash.to_hex(), data.len());
                db.insert(
                    hash,
                    Data {
                        outboard: Bytes::from(outboard),
                        path,
                        size: data.len(),
                    },
                );
            }
        }
    }

    Ok(Arc::new(db))
}

async fn write_response<W: AsyncWrite + Unpin>(
    mut writer: W,
    buffer: &mut BytesMut,
    id: u64,
    res: Res<'_>,
) -> Result<()> {
    let response = Response { id, data: res };

    if buffer.len() < 20 + response.data.len() {
        buffer.resize(20 + response.data.len(), 0u8);
    }
    let used = postcard::to_slice(&response, buffer)?;

    write_lp(&mut writer, used).await?;

    debug!("written response of length {}", used.len());
    Ok(())
}
