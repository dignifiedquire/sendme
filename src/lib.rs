mod blobs;
pub mod get;
pub mod protocol;
pub mod provider;

mod tls;

pub use tls::{Keypair, PeerId, PeerIdError, PublicKey, SecretKey, Signature};

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf};

    use crate::get::Event;
    use crate::protocol::AuthToken;
    use crate::tls::PeerId;

    use super::*;
    use anyhow::Result;
    use futures::StreamExt;
    use rand::RngCore;
    use testdir::testdir;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn basics() -> Result<()> {
        let dir: PathBuf = testdir!();
        let path = dir.join("hello_world");
        tokio::fs::write(&path, "hello world!").await?;
        let db = provider::create_db(vec![provider::DataSource::File(path.clone())]).await?;
        let hash = *db.iter().next().unwrap().0;
        let addr = "127.0.0.1:4443".parse().unwrap();
        let mut provider = provider::Provider::builder().database(db).build()?;
        let peer_id = provider.peer_id();
        let token = provider.auth_token();

        tokio::task::spawn(async move {
            provider.run(provider::Options { addr }).await.unwrap();
        });

        let opts = get::Options {
            addr,
            peer_id: Some(peer_id),
        };
        let stream = get::run(hash, token, opts);
        tokio::pin!(stream);
        while let Some(event) = stream.next().await {
            let event = event?;
            if let Event::Receiving {
                hash: new_hash,
                mut reader,
            } = event
            {
                assert_eq!(hash, new_hash);
                let expect = tokio::fs::read(&path).await?;
                let mut got = Vec::new();
                reader.read_to_end(&mut got).await?;
                assert_eq!(expect, got);
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn sizes() -> Result<()> {
        let addr = "127.0.0.1:4445".parse().unwrap();

        let sizes = [
            10,
            100,
            1024,
            1024 * 100,
            1024 * 500,
            1024 * 1024,
            1024 * 1024 + 10,
        ];

        for size in sizes {
            println!("testing {size} bytes");

            let dir: PathBuf = testdir!();
            let path = dir.join("hello_world");

            let mut content = vec![0u8; size];
            rand::thread_rng().fill_bytes(&mut content);

            tokio::fs::write(&path, &content).await?;

            let db = provider::create_db(vec![provider::DataSource::File(path)]).await?;
            let hash = *db.iter().next().unwrap().0;
            let mut provider = provider::Provider::builder().database(db).build()?;
            let peer_id = provider.peer_id();
            let token = provider.auth_token();

            let provider_task = tokio::task::spawn(async move {
                provider.run(provider::Options { addr }).await.unwrap();
            });

            let opts = get::Options {
                addr,
                peer_id: Some(peer_id),
            };
            let stream = get::run(hash, token, opts);
            tokio::pin!(stream);
            while let Some(event) = stream.next().await {
                let event = event?;
                if let Event::Receiving {
                    hash: new_hash,
                    mut reader,
                } = event
                {
                    assert_eq!(hash, new_hash);
                    let mut got = Vec::new();
                    reader.read_to_end(&mut got).await?;
                    assert_eq!(content, got);
                }
            }

            provider_task.abort();
            let _ = provider_task.await;
        }

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multiple_clients() -> Result<()> {
        let dir: PathBuf = testdir!();
        let path = dir.join("hello_world");
        let content = b"hello world!";
        let addr = "127.0.0.1:4444".parse().unwrap();

        tokio::fs::write(&path, content).await?;
        let db = provider::create_db(vec![provider::DataSource::File(path)]).await?;
        let hash = *db.iter().next().unwrap().0;
        let mut provider = provider::Provider::builder().database(db).build()?;
        let peer_id = provider.peer_id();
        let token = provider.auth_token();

        tokio::task::spawn(async move {
            provider.run(provider::Options { addr }).await.unwrap();
        });

        async fn run_client(
            hash: bao::Hash,
            token: AuthToken,
            addr: SocketAddr,
            peer_id: PeerId,
            content: Vec<u8>,
        ) -> Result<()> {
            let opts = get::Options {
                addr,
                peer_id: Some(peer_id),
            };
            let stream = get::run(hash, token, opts);
            tokio::pin!(stream);
            while let Some(event) = stream.next().await {
                let event = event?;
                if let Event::Receiving {
                    hash: new_hash,
                    mut reader,
                } = event
                {
                    assert_eq!(hash, new_hash);
                    let mut got = Vec::new();
                    reader.read_to_end(&mut got).await?;
                    assert_eq!(content, got);
                }
            }
            Ok(())
        }

        let mut tasks = Vec::new();
        for _i in 0..3 {
            tasks.push(tokio::task::spawn(run_client(
                hash,
                token,
                addr,
                peer_id,
                content.to_vec(),
            )));
        }

        for task in tasks {
            task.await??;
        }

        Ok(())
    }
}
