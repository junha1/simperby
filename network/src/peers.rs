use super::*;
use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_tc::http::*;
use serde_tc::{serde_tc_full, StubCall};
use simperby_core::serde_spb;
use simperby_core::FinalizationInfo;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

#[derive(Debug)]
struct PeerStorage {
    path: String,
}

impl PeerStorage {
    pub async fn new(path: &str) -> Result<Self> {
        Ok(Self {
            path: path.to_owned(),
        })
    }

    pub async fn write(&mut self, peers: Vec<Peer>) -> Result<()> {
        let mut file = File::open(&self.path).await?;
        file.write_all(serde_spb::to_string(&peers)?.as_bytes())
            .await?;
        Ok(())
    }

    pub async fn read(&self) -> Result<Vec<Peer>> {
        let mut file = File::open(&self.path).await?;
        let peers: Vec<Peer> = serde_spb::from_str(&{
            let mut buf = String::new();
            file.read_to_string(&mut buf).await?;
            buf
        })?;
        Ok(peers)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PingResponse {
    pub public_key: PublicKey,
    pub timestamp: Timestamp,
    pub msg: String,
}

#[serde_tc_full]
pub(super) trait PeerRpcInterface: Send + Sync + 'static {
    /// Requests to response some packets.
    async fn ping(&self) -> Result<PingResponse, String>;
    /// Requests to response the port map of this node
    async fn port_map(&self) -> Result<BTreeMap<String, u16>, String>;
}

pub struct PeerRpcImpl {
    internal: Arc<RwLock<PeersInternal>>,
}

/// Server-side implementation of the RPC interface.
#[async_trait]
impl PeerRpcInterface for PeerRpcImpl {
    async fn ping(&self) -> Result<PingResponse, String> {
        let public_key = self.internal.read().await.private_key.public_key();
        Ok(PingResponse {
            public_key,
            timestamp: simperby_core::utils::get_timestamp(),
            msg: "hello?".to_string(),
        })
    }

    async fn port_map(&self) -> Result<BTreeMap<String, u16>, String> {
        Ok(self.internal.read().await.port_map.clone())
    }
}

#[derive(Debug)]
pub struct PeersInternal {
    storage: PeerStorage,
    lfi: FinalizationInfo,
    server_network_config: ServerNetworkConfig,
    port_map: BTreeMap<String, u16>,
    private_key: PrivateKey,
}

pub struct Peers {
    internal: Arc<RwLock<PeersInternal>>,
}

impl Peers {
    pub async fn new(
        path: &str,
        lfi: FinalizationInfo,
        server_network_config: ServerNetworkConfig,
        port_map: BTreeMap<String, u16>,
        private_key: PrivateKey,
    ) -> Result<Self> {
        let this = Arc::new(RwLock::new(PeersInternal {
            storage: PeerStorage::new(path).await?,
            lfi,
            server_network_config,
            port_map,
            private_key,
        }));
        run_server(
            server_network_config.port,
            [(
                "peer".to_owned(),
                create_http_object(Arc::new(PeerRpcImpl {
                    internal: Arc::clone(&this),
                }) as Arc<dyn PeerRpcInterface>),
            )]
            .iter()
            .cloned()
            .collect(),
        )
        .await;
        Ok(())
    }
}

impl PeersInternal {
    pub async fn update_block(&mut self, lfi: FinalizationInfo) -> Result<()> {
        let peers = self.storage.read().await?;
        self.storage.write(vec![]).await?;
        for peer in peers {
            self.add_peer(peer.name, peer.address).await?;
        }
        self.lfi = lfi;
        Ok(())
    }

    pub async fn add_peer_raw(&mut self, peer: Peer) -> Result<()> {
        let mut peers = self.storage.read().await?;
        peers.push(peer);
        self.storage.write(peers).await?;
        Ok(())
    }

    /// Adds a peer to the list of known peers. This will try to connect to the peer and ask information.
    ///
    /// - `name` - the name of the peer as it is known in the reserved state.
    /// - `addr` - the address of the peer. The port must be the one of the peer discovery RPC.
    pub async fn add_peer(&mut self, name: MemberName, addr: SocketAddrV4) -> Result<()> {
        let stub = PeerRpcInterfaceStub::new(Box::new(HttpClient::new(
            format!("{}:{}/peer", addr.ip(), addr.port()),
            reqwest::Client::new(),
        )));
        stub.ping()
            .await
            .map_err(|e| eyre!("failed to ping peer {}: {}", name, e))?;
        let port_map = stub.port_map().await?;

        let peer = Peer {
            public_key: self
                .lfi
                .reserved_state
                .query_public_key(&name)
                .ok_or_else(|| eyre!("peer does not exist: {}", name))?,
            name,
            address: addr,
            ports: vec![
                (
                    format!("dms-governance-{}", self.lfi.header.to_hash256()),
                    addr.port() + 1,
                ),
                (
                    format!("dms-consensus-{}", self.lfi.header.to_hash256()),
                    addr.port() + 2,
                ),
                ("repository".to_owned(), addr.port() + 3),
            ]
            .into_iter()
            .collect(),
            message: "".to_owned(),
            recently_seen_timestamp: 0,
        };

        let mut peers = self.storage.read().await?;
        peers.push(peer);
        self.storage.write(peers).await?;
        Ok(())
    }

    pub async fn list_peers(&self) -> Result<Vec<Peer>> {
        self.storage.read().await
    }
}
