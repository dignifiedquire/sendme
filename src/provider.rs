use std::fmt::{self, Display};
use std::io::{BufReader, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::{collections::HashMap, sync::Arc};

use anyhow::{anyhow, bail, ensure, Context, Result};
use bao::encode::SliceExtractor;
use bytes::{Bytes, BytesMut};
use s2n_quic::stream::BidirectionalStream;
use s2n_quic::Server as QuicServer;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::io::SyncIoBridge;
use tracing::{debug, warn};

use crate::blobs::{Blob, Collection};
use crate::protocol::{read_lp, write_lp, AuthToken, Handshake, Request, Res, Response, VERSION};
use crate::tls::{self, Keypair, PeerId};
use crate::util::{self, Hash};

const MAX_CONNECTIONS: u64 = 1024;
const MAX_STREAMS: u64 = 10;

pub type Database = Arc<HashMap<Hash, BlobOrCollection>>;

/// Builder for the [`Provider`].
///
/// You must supply a database which can be created using [`create_collection`], everything else is
/// optional.  Finally you can create and run the provider by calling [`Builder::spawn`].
///
/// The returned [`Provider`] provides [`Provider::join`] to wait for the spawned task.
/// Currently it needs to be aborted using [`Provider::abort`], graceful shutdown will be
/// implemented in the immediate future.
#[derive(Debug)]
pub struct Builder {
    bind_addr: SocketAddr,
    keypair: Keypair,
    auth_token: AuthToken,
    db: Database,
}

#[derive(Debug)]
pub enum BlobOrCollection {
    Blob(Data),
    Collection((Bytes, Bytes)),
}

impl Builder {
    /// Creates a new builder for [`Provider`] using the given [`Database`].
    pub fn with_db(db: Database) -> Self {
        Self {
            bind_addr: "127.0.0.1:4433".parse().unwrap(),
            keypair: Keypair::generate(),
            auth_token: AuthToken::generate(),
            db,
        }
    }

    /// Binds the provider service to a different socket.
    ///
    /// By default it binds to `127.0.0.1:4433`.
    pub fn bind_addr(mut self, addr: SocketAddr) -> Self {
        self.bind_addr = addr;
        self
    }

    /// Uses the given [`Keypair`] for the [`PeerId`] instead of a newly generated one.
    pub fn keypair(mut self, keypair: Keypair) -> Self {
        self.keypair = keypair;
        self
    }

    /// Uses the given [`AuthToken`] instead of a newly generated one.
    pub fn auth_token(mut self, auth_token: AuthToken) -> Self {
        self.auth_token = auth_token;
        self
    }

    /// Spawns the [`Provider`] in a tokio task.
    ///
    /// This will create the underlying network server and spawn a tokio task accepting
    /// connections.  The returned [`Provider`] can be used to control the task as well as
    /// get information about it.
    pub fn spawn(self) -> Result<Provider> {
        let server_config = tls::make_server_config(&self.keypair)?;
        let tls = s2n_quic::provider::tls::rustls::Server::from(server_config);
        let limits = s2n_quic::provider::limits::Limits::default()
            .with_max_active_connection_ids(MAX_CONNECTIONS)?
            .with_max_open_local_bidirectional_streams(MAX_STREAMS)?
            .with_max_open_remote_bidirectional_streams(MAX_STREAMS)?;

        let server = QuicServer::builder()
            .with_tls(tls)?
            .with_io(self.bind_addr)?
            .with_limits(limits)?
            .start()
            .map_err(|e| anyhow!("{:?}", e))?;
        let listen_addr = server.local_addr().unwrap();
        let db2 = self.db.clone();
        let (events_sender, _events_receiver) = broadcast::channel(8);
        let events = events_sender.clone();
        let task =
            tokio::spawn(
                async move { Self::run(server, db2, self.auth_token, events_sender).await },
            );

        Ok(Provider {
            listen_addr,
            keypair: self.keypair,
            auth_token: self.auth_token,
            task,
            events,
        })
    }

    async fn run(
        mut server: s2n_quic::server::Server,
        db: Database,
        token: AuthToken,
        events: broadcast::Sender<Event>,
    ) {
        debug!("\nlistening at: {:#?}", server.local_addr().unwrap());

        while let Some(mut connection) = server.accept().await {
            let db = db.clone();
            let events = events.clone();
            tokio::spawn(async move {
                debug!("connection accepted from {:?}", connection.remote_addr());
                while let Ok(Some(stream)) = connection.accept_bidirectional_stream().await {
                    let _ = events.send(Event::ClientConnected {
                        connection_id: connection.id(),
                    });
                    let db = db.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        if let Err(err) = handle_stream(db, token, stream, events).await {
                            warn!("error: {:#?}", err);
                        }
                        debug!("disconnected");
                    });
                }
            });
        }
    }
}

/// A server which implements the sendme provider.
///
/// Clients can connect to this server and requests hashes from it.
///
/// The only way to create this is by using the [`Builder::spawn`].  [`Provider::builder`]
/// is a shorthand to create a suitable [`Builder`].
///
/// This runs a tokio task which can be aborted and joined if desired.
#[derive(Debug)]
pub struct Provider {
    listen_addr: SocketAddr,
    keypair: Keypair,
    auth_token: AuthToken,
    task: JoinHandle<()>,
    events: broadcast::Sender<Event>,
}

/// Events emitted by the [`Provider`] informing about the current status.
#[derive(Debug, Clone)]
pub enum Event {
    ClientConnected {
        connection_id: u64,
    },
    RequestReceived {
        connection_id: u64,
        request_id: u64,
        hash: Hash,
    },
    TransferCompleted {
        connection_id: u64,
        request_id: u64,
    },
    TransferAborted {
        connection_id: u64,
        request_id: u64,
    },
}

impl Provider {
    /// Returns a new builder for the [`Provider`].
    ///
    /// Once the done with the builder call [`Builder::spawn`] to create the provider.
    pub fn builder(db: Database) -> Builder {
        Builder::with_db(db)
    }

    /// Returns the address on which the server is listening for connections.
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Returns the [`PeerId`] of the provider.
    pub fn peer_id(&self) -> PeerId {
        self.keypair.public().into()
    }

    /// Returns the [`AuthToken`] needed to connect to the provider.
    pub fn auth_token(&self) -> AuthToken {
        self.auth_token
    }

    /// Subscribe to [`Event`]s emitted from the provider, informing about connections and
    /// progress.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Return a single token containing everything needed to get a hash.
    ///
    /// See [`Ticket`] for more details of how it can be used.
    pub fn ticket(&self, hash: Hash) -> Ticket {
        // TODO: Verify that the hash exists in the db?
        Ticket {
            hash,
            peer: self.peer_id(),
            addr: self.listen_addr,
            token: self.auth_token,
        }
    }

    /// Blocks until the provider task completes.
    // TODO: Maybe implement Future directly?
    pub async fn join(self) -> Result<(), JoinError> {
        self.task.await
    }

    /// Aborts the provider.
    ///
    /// TODO: temporary, do graceful shutdown instead.
    pub fn abort(&self) {
        self.task.abort();
    }
}

async fn handle_stream(
    db: Database,
    token: AuthToken,
    stream: BidirectionalStream,
    events: broadcast::Sender<Event>,
) -> Result<()> {
    debug!("stream opened from {:?}", stream.connection().remote_addr());
    let connection_id = stream.connection().id();
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
                let hash = request.name;
                debug!("got request({})", request.id);
                let _ = events.send(Event::RequestReceived {
                    connection_id,
                    request_id: request.id,
                    hash,
                });

                match db.get(&hash) {
                    // We only respond to requests for collections, not individual blobs
                    Some(BlobOrCollection::Collection((outboard, data))) => {
                        debug!("found collection {}", hash);

                        let mut extractor = SliceExtractor::new_outboard(
                            std::io::Cursor::new(&data[..]),
                            std::io::Cursor::new(&outboard[..]),
                            0,
                            data.len() as u64,
                        );
                        let encoded_size: usize = bao::encode::encoded_size(data.len() as u64)
                            .try_into()
                            .unwrap();
                        let mut encoded = Vec::with_capacity(encoded_size);
                        extractor.read_to_end(&mut encoded)?;

                        let c: Collection = postcard::from_bytes(data)?;

                        // TODO: we should check if the blobs referenced in this container
                        // actually exist in this provider before returning `FoundCollection`
                        write_response(
                            &mut writer,
                            &mut out_buffer,
                            request.id,
                            Res::FoundCollection {
                                total_blobs_size: c.total_blobs_size,
                            },
                        )
                        .await?;

                        let mut data = BytesMut::from(&encoded[..]);
                        writer.write_buf(&mut data).await?;
                        for blob in c.blobs {
                            let (status, writer1) = send_blob(
                                db.clone(),
                                blob.hash,
                                writer,
                                &mut out_buffer,
                                request.id,
                            )
                            .await?;
                            writer = writer1;
                            if SentStatus::NotFound == status {
                                break;
                            }
                        }
                        let _ = events.send(Event::TransferCompleted {
                            connection_id,
                            request_id: request.id,
                        });
                    }
                    _ => {
                        debug!("not found {}", hash);
                        write_response(&mut writer, &mut out_buffer, request.id, Res::NotFound)
                            .await?;

                        let _ = events.send(Event::TransferAborted {
                            connection_id,
                            request_id: request.id,
                        });
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum SentStatus {
    Sent,
    NotFound,
}

async fn send_blob<W: AsyncWrite + Unpin + Send + 'static>(
    db: Database,
    name: Hash,
    mut writer: W,
    buffer: &mut BytesMut,
    id: u64,
) -> Result<(SentStatus, W)> {
    match db.get(&name) {
        Some(BlobOrCollection::Blob(Data {
            outboard,
            path,
            size,
        })) => {
            write_response(&mut writer, buffer, id, Res::Found).await?;
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
                    size,
                );
                let _copied = std::io::copy(&mut slice_extractor, &mut wrapper)?;
                std::io::Result::Ok(writer)
            })
            .await??;
            Ok((SentStatus::Sent, writer))
        }
        _ => {
            write_response(&mut writer, buffer, id, Res::NotFound).await?;
            Ok((SentStatus::NotFound, writer))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Data {
    /// Outboard data from bao.
    outboard: Bytes,
    /// Path to the original data, which must not change while in use.
    path: PathBuf,
    /// Size of the original data.
    size: u64,
}

#[derive(Debug)]
pub enum DataSource {
    /// A blob of data originating from the filesystem. The name of the blob is derived from
    /// the filename.
    File(PathBuf),
    /// NamedFile is treated the same as [`DataSource::File`], except you can pass in a custom
    /// name. Passing in the empty string will explicitly _not_ persist the filename.
    NamedFile { path: PathBuf, name: String },
}

impl DataSource {
    pub fn new(path: PathBuf) -> Self {
        DataSource::File(path)
    }
    pub fn with_name(path: PathBuf, name: String) -> Self {
        DataSource::NamedFile { path, name }
    }
}

impl From<PathBuf> for DataSource {
    fn from(value: PathBuf) -> Self {
        DataSource::new(value)
    }
}

impl From<&std::path::Path> for DataSource {
    fn from(value: &std::path::Path) -> Self {
        DataSource::new(value.to_path_buf())
    }
}

/// Synchronously compute the outboard of a file, and return hash and outboard.
///
/// It is assumed that the file is not modified while this is running.
///
/// If it is modified while or after this is running, the outboard will be
/// invalid, so any attempt to compute a slice from it will fail.
///
/// If the size of the file is changed while this is running, an error will be
/// returned.
fn compute_outboard(path: PathBuf) -> anyhow::Result<(Hash, Vec<u8>)> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    // compute outboard size so we can pre-allocate the buffer.
    //
    // outboard is ~1/16 of data size, so this will fail for really large files
    // on really small devices. E.g. you want to transfer a 1TB file from a pi4 with 1gb ram.
    //
    // The way to solve this would be to have larger blocks than the blake3 chunk size of 1024.
    // I think we really want to keep the outboard in memory for simplicity.
    let outboard_size = usize::try_from(bao::encode::outboard_size(len))
        .context("outboard too large to fit in memory")?;
    let mut outboard = Vec::with_capacity(outboard_size);

    // copy the file into the encoder. Data will be skipped by the encoder in outboard mode.
    let outboard_cursor = std::io::Cursor::new(&mut outboard);
    let mut encoder = bao::encode::Encoder::new_outboard(outboard_cursor);

    let mut reader = BufReader::new(file);
    // the length we have actually written, should be the same as the length of the file.
    let len2 = std::io::copy(&mut reader, &mut encoder)?;
    // this can fail if the file was appended to during encoding.
    ensure!(len == len2, "file changed during encoding");
    // this flips the outboard encoding from post-order to pre-order
    let hash = encoder.finalize()?;

    Ok((hash.into(), outboard))
}

/// Creates a database of blobs (stored in outboard storage) and Collections, stored in memory.
/// Returns a the hash of the collection created by the given list of DataSources
pub async fn create_collection(data_sources: Vec<DataSource>) -> Result<(Database, Hash)> {
    // +1 is for the collection itself
    let mut db = HashMap::with_capacity(data_sources.len() + 1);
    let mut blobs = Vec::with_capacity(data_sources.len());
    let mut total_blobs_size: u64 = 0;

    let mut blobs_encoded_size_estimate = 0;
    for data in data_sources {
        let (path, name) = match data {
            DataSource::File(path) => (path, None),
            DataSource::NamedFile { path, name } => (path, Some(name)),
        };

        ensure!(
            path.is_file(),
            "can only transfer blob data: {}",
            path.display()
        );
        // spawn a blocking task for computing the hash and outboard.
        // pretty sure this is best to remain sync even once bao is async.
        let path2 = path.clone();
        let (hash, outboard) =
            tokio::task::spawn_blocking(move || compute_outboard(path2)).await??;

        debug_assert!(outboard.len() >= 8, "outboard must at least contain size");
        let size = u64::from_le_bytes(outboard[..8].try_into().unwrap());
        db.insert(
            hash,
            BlobOrCollection::Blob(Data {
                outboard: Bytes::from(outboard),
                path: path.clone(),
                size,
            }),
        );
        total_blobs_size += size;
        // if the given name is `None`, use the filename from the given path as the name
        let name = name.unwrap_or_else(|| {
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string()
        });
        blobs_encoded_size_estimate += name.len() + 32;
        blobs.push(Blob { name, hash });
    }
    let c = Collection {
        name: "collection".to_string(),
        blobs,
        total_blobs_size,
    };
    blobs_encoded_size_estimate += c.name.len();

    // NOTE: we can't use the postcard::MaxSize to estimate the encoding buffer size
    // because the Collection and Blobs have `String` fields.
    // So instead, we are tracking the filename + hash sizes of each blob, plus an extra 1024
    // to account for any postcard encoding data.
    let mut buffer = BytesMut::zeroed(blobs_encoded_size_estimate + 1024);
    let data = postcard::to_slice(&c, &mut buffer)?;
    let (outboard, hash) = bao::encode::outboard(&data);
    let hash = Hash::from(hash);
    println!("Collection: {}\n", hash);
    for el in db.values() {
        if let BlobOrCollection::Blob(blob) = el {
            println!("- {}: {} bytes", blob.path.display(), blob.size);
        }
    }
    println!();
    db.insert(
        hash,
        BlobOrCollection::Collection((Bytes::from(outboard), Bytes::from(data.to_vec()))),
    );

    Ok((Arc::new(db), hash))
}

async fn write_response<W: AsyncWrite + Unpin>(
    mut writer: W,
    buffer: &mut BytesMut,
    id: u64,
    res: Res,
) -> Result<()> {
    let response = Response { id, data: res };

    // TODO: do not transfer blob data as part of the responses
    if buffer.len() < 1024 {
        buffer.resize(1024, 0u8);
    }
    let used = postcard::to_slice(&response, buffer)?;

    write_lp(&mut writer, used).await?;

    debug!("written response of length {}", used.len());
    Ok(())
}

/// A token containing everything to get a file from the provider.
///
/// It is a single item which can be easily serialized and deserialized.  The [`Display`]
/// and [`FromStr`] implementations serialize to base64.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ticket {
    /// The hash to retrieve.
    pub hash: Hash,
    /// The peer ID identifying the provider.
    pub peer: PeerId,
    /// The socket address the provider is listening on.
    pub addr: SocketAddr,
    /// The authentication token with permission to retrieve the hash.
    pub token: AuthToken,
}

/// Serializes to base64.
impl Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let encoded = postcard::to_stdvec(self).map_err(|_| fmt::Error)?;
        write!(f, "{}", util::encode(encoded))
    }
}

/// Deserializes from base64.
impl FromStr for Ticket {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = util::decode(s)?;
        let slf = postcard::from_bytes(&bytes)?;
        Ok(slf)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use testdir::testdir;

    use super::*;

    #[test]
    fn test_ticket_base64_roundtrip() {
        let (_encoded, hash) = bao::encode::encode(b"hi there");
        let hash = Hash::from(hash);
        let peer = PeerId::from(Keypair::generate().public());
        let addr = SocketAddr::from_str("127.0.0.1:1234").unwrap();
        let token = AuthToken::generate();
        let ticket = Ticket {
            hash,
            peer,
            addr,
            token,
        };
        let base64 = ticket.to_string();
        println!("Ticket: {base64}");
        println!("{} bytes", base64.len());

        let ticket2: Ticket = base64.parse().unwrap();
        assert_eq!(ticket2, ticket);
    }

    #[tokio::test]
    async fn test_create_collection() -> Result<()> {
        let dir: PathBuf = testdir!();
        let mut expect_blobs = vec![];
        let (_, hash) = bao::encode::outboard(vec![]);
        let hash = Hash::from(hash);

        // DataSource::File
        let foo = dir.join("foo");
        tokio::fs::write(&foo, vec![]).await?;
        let foo = DataSource::new(foo);
        expect_blobs.push(Blob {
            name: "foo".to_string(),
            hash,
        });

        // DataSource::NamedFile
        let bar = dir.join("bar");
        tokio::fs::write(&bar, vec![]).await?;
        let bar = DataSource::with_name(bar, "bat".to_string());
        expect_blobs.push(Blob {
            name: "bat".to_string(),
            hash,
        });

        // DataSource::NamedFile, empty string name
        let baz = dir.join("baz");
        tokio::fs::write(&baz, vec![]).await?;
        let baz = DataSource::with_name(baz, "".to_string());
        expect_blobs.push(Blob {
            name: "".to_string(),
            hash,
        });

        let expect_collection = Collection {
            name: "collection".to_string(),
            blobs: expect_blobs,
            total_blobs_size: 0,
        };

        let (db, hash) = create_collection(vec![foo, bar, baz]).await?;

        let collection = {
            let c = db.get(&hash).unwrap();
            if let BlobOrCollection::Collection((_, data)) = c {
                Collection::from_bytes(data)?
            } else {
                panic!("expected hash to correspond with a `Collection`, found `Blob` instead");
            }
        };

        assert_eq!(expect_collection, collection);

        Ok(())
    }
}
