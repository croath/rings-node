//! Processor of rings-node jsonrpc-server.
#![warn(missing_docs)]
use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::Join;
use futures::Future;
#[cfg(feature = "node")]
use jsonrpc_core::Metadata;

use crate::backend::types::BackendMessage;
use crate::backend::types::HttpRequest;
use crate::backend::types::MessageType;
use crate::backend::types::Timeout;
use crate::error;
use crate::error::Error;
use crate::error::Result;
use crate::jsonrpc::method;
use crate::jsonrpc::response::TransportAndIce;
use crate::jsonrpc_client::SimpleClient;
use crate::measure::PeriodicMeasure;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::dht::Stabilization;
use crate::prelude::rings_core::dht::TStabilize;
use crate::prelude::rings_core::ecc::PublicKey;
use crate::prelude::rings_core::ecc::SecretKey;
use crate::prelude::rings_core::message::Encoded;
use crate::prelude::rings_core::message::Encoder;
use crate::prelude::rings_core::message::Message;
use crate::prelude::rings_core::message::PayloadSender;
use crate::prelude::rings_core::prelude::libsecp256k1;
use crate::prelude::rings_core::prelude::uuid;
use crate::prelude::rings_core::prelude::web3::contract::tokens::Tokenizable;
use crate::prelude::rings_core::prelude::web3::ethabi::Token;
use crate::prelude::rings_core::prelude::RTCSdpType;
use crate::prelude::rings_core::session::AuthorizedInfo;
use crate::prelude::rings_core::session::SessionManager;
use crate::prelude::rings_core::storage::PersistenceStorage;
use crate::prelude::rings_core::swarm::Swarm;
use crate::prelude::rings_core::swarm::SwarmBuilder;
use crate::prelude::rings_core::transports::manager::TransportManager;
use crate::prelude::rings_core::transports::Transport;
use crate::prelude::rings_core::types::ice_transport::IceTransportInterface;
use crate::prelude::rings_core::types::ice_transport::IceTrickleScheme;
use crate::prelude::rings_core::types::message::MessageListener;
use crate::prelude::vnode;
use crate::prelude::web3::signing::keccak256;
use crate::prelude::CallbackFn;
use crate::prelude::ChordStorageInterface;
use crate::prelude::CustomMessage;
use crate::prelude::Signer;

/// AddressType enum contains `DEFAULT` and `ED25519`.
pub enum AddressType {
    /// default address type
    DEFAULT,
    /// ED25519 address type
    ED25519,
}

/// A UnsignedInfo use for wasm.
#[derive(Clone)]
pub struct UnsignedInfo {
    /// Did identify
    key_addr: Did,
    /// auth information
    auth: AuthorizedInfo,
    /// random secrekey generate by service
    random_key: SecretKey,
}

impl UnsignedInfo {
    /// Create a new `UnsignedInfo` instance with SignerMode::EIP191
    pub fn new(key_addr: String) -> Result<Self> {
        Self::new_with_signer(key_addr, Signer::EIP191)
    }

    /// Create a new `UnsignedInfo` instance
    ///   * key_addr: wallet address
    ///   * signer: `SignerMode`
    pub fn new_with_signer(key_addr: String, signer: Signer) -> Result<Self> {
        let key_addr = Did::from_str(key_addr.as_str()).map_err(|_| Error::InvalidDid)?;
        let (auth, random_key) = SessionManager::gen_unsign_info(key_addr, None, Some(signer));

        Ok(UnsignedInfo {
            auth,
            random_key,
            key_addr,
        })
    }

    /// Create a new `UnsignedInfo` instance
    ///   * pubkey: solana wallet pubkey
    pub fn new_with_address(address: String, addr_type: AddressType) -> Result<Self> {
        let (key_addr, auth, random_key) = match addr_type {
            AddressType::DEFAULT => {
                let key_addr = Did::from_str(address.as_str()).map_err(|_| Error::InvalidDid)?;
                let (auth, random_key) =
                    SessionManager::gen_unsign_info(key_addr, None, Some(Signer::EIP191));
                (key_addr, auth, random_key)
            }
            AddressType::ED25519 => {
                let pubkey =
                    PublicKey::try_from_b58t(&address).map_err(|_| Error::InvalidAddress)?;
                let (auth, random_key) =
                    SessionManager::gen_unsign_info_with_ed25519_pubkey(None, pubkey)
                        .map_err(|_| Error::InvalidAddress)?;
                (pubkey.address().into(), auth, random_key)
            }
        };
        Ok(UnsignedInfo {
            auth,
            random_key,
            key_addr,
        })
    }

    /// Get auth string
    pub fn auth(&self) -> Result<String> {
        let s = self.auth.to_string().map_err(|_| Error::InvalidAuthData)?;
        Ok(s)
    }
}

/// Processor for rings-node jsonrpc server
#[derive(Clone)]
pub struct Processor {
    /// a swarm instance
    pub swarm: Arc<Swarm>,
    /// a stabilization instance,
    pub stabilization: Arc<Stabilization>,
}

#[cfg(feature = "node")]
impl Metadata for Processor {}

impl From<(Arc<Swarm>, Arc<Stabilization>)> for Processor {
    fn from((swarm, stabilization): (Arc<Swarm>, Arc<Stabilization>)) -> Self {
        Self {
            swarm,
            stabilization,
        }
    }
}

impl Processor {
    /// Create a new Processor instance.
    pub async fn new(
        unsigned_info: &UnsignedInfo,
        signed_data: &[u8],
        stuns: String,
    ) -> Result<Self> {
        Self::new_with_storage(unsigned_info, signed_data, stuns, "rings-node".to_owned()).await
    }

    /// Create a new Processor
    pub async fn new_with_storage(
        unsigned_info: &UnsignedInfo,
        signed_data: &[u8],
        stuns: String,
        storage_name: String,
    ) -> Result<Self> {
        let unsigned_info = unsigned_info.clone();
        let signed_data = signed_data.to_vec();

        let storage_path = storage_name.as_str();
        let measure_path = [storage_path, "measure"].join("/");

        let storage = PersistenceStorage::new_with_cap_and_name(50000, storage_path)
            .await
            .map_err(Error::Storage)?;

        let ms = PersistenceStorage::new_with_cap_and_path(50000, measure_path)
            .await
            .map_err(Error::Storage)?;
        let measure = PeriodicMeasure::new(ms);

        let random_key = unsigned_info.random_key;
        let session_manager = SessionManager::new(&signed_data, &unsigned_info.auth, &random_key);

        let swarm = Arc::new(
            SwarmBuilder::new(&stuns, storage)
                .session_manager(unsigned_info.key_addr, session_manager)
                .measure(Box::new(measure))
                .build()
                .map_err(Error::Swarm)?,
        );

        let stabilization = Arc::new(Stabilization::new(swarm.clone(), 20));
        Ok(Processor::from((swarm, stabilization)))
    }

    /// Listen processor message
    pub fn listen(&self, callback: Option<CallbackFn>) -> Join<impl Future, impl Future> {
        let message_handler = Arc::new(self.swarm.create_message_handler(callback, None));
        let stab = Arc::clone(&self.stabilization);
        futures::future::join(async { message_handler.listen().await }, async {
            stab.wait().await
        })
    }
}

impl Processor {
    /// Generate Signature for Authorization
    pub fn generate_signature(secret_key: &SecretKey) -> String {
        let message = format!("rings-node: {}", secret_key.address().into_token());
        let (signature, _recovery_id) = libsecp256k1::sign(
            &libsecp256k1::Message::parse(&keccak256(message.as_bytes())),
            secret_key,
        );
        base64::encode(signature.serialize())
    }

    /// verify signature
    /// will throw error when signature is illegal
    pub fn verify_signature(signature: &[u8], public_key: &PublicKey) -> Result<bool> {
        let message = format!("rings-node: {}", public_key.address().into_token());
        Ok(libsecp256k1::verify(
            &libsecp256k1::Message::parse(&keccak256(message.as_bytes())),
            &libsecp256k1::Signature::parse_standard_slice(
                &base64::decode(signature).map_err(|_| Error::DecodedError)?,
            )
            .map_err(|_| Error::DecodedError)?,
            &TryInto::<libsecp256k1::PublicKey>::try_into(*public_key)
                .map_err(|_| Error::DecodedError)?,
        ))
    }

    /// Get current did
    pub fn did(&self) -> Did {
        self.swarm.did()
    }

    /// Create an Offer and waiting for connection.
    /// The process of manually handshake is:
    /// 1. PeerA: create_offer
    /// 2. PeerA: send the handshake info to PeerB.
    /// 3. PeerB: answer_offer
    /// 4. PeerB: send the handshake info to PeerA.
    /// 5. PeerA: accept_answer.
    pub async fn create_offer(&self) -> Result<(Arc<Transport>, Encoded)> {
        let transport = self
            .swarm
            .new_transport()
            .await
            .map_err(|_| Error::NewTransportError)?;
        let transport_cloned = transport.clone();
        let task = async move {
            let hs_info = transport_cloned
                .get_handshake_info(self.swarm.session_manager(), RTCSdpType::Offer)
                .await
                .map_err(Error::CreateOffer)?;
            self.swarm
                .push_pending_transport(&transport_cloned)
                .map_err(Error::PendingTransport)?;
            Ok(hs_info)
        };
        let hs_info = match task.await {
            Ok(hs_info) => (transport, hs_info),
            Err(e) => {
                transport.close().await.ok();
                return Err(e);
            }
        };
        Ok(hs_info)
    }

    /// Connect peer with remote rings-node jsonrpc server.
    /// * peer_url: the remote rings-node jsonrpc server url.
    pub async fn connect_peer_via_http(&self, peer_url: &str) -> Result<Arc<Transport>> {
        // request remote offer and sand answer to remote
        tracing::debug!("connect_peer_via_http: {}", peer_url);
        let (transport, _hs_info) = self.do_connect_peer_via_http(peer_url).await?;
        Ok(transport)
    }

    async fn do_connect_peer_via_http(&self, node_url: &str) -> Result<(Arc<Transport>, String)> {
        let client = SimpleClient::new_with_url(node_url);
        let (transport, hs_info) = self.create_offer().await?;
        tracing::debug!(
            "sending offer and candidate {:?} to {:?}",
            hs_info.to_owned(),
            node_url,
        );

        let addr_result = {
            let resp = client
                .call_method(
                    method::Method::AnswerOffer.as_str(),
                    jsonrpc_core::Params::Array(vec![serde_json::json!(hs_info)]),
                )
                .await
                .map_err(|e| Error::RemoteRpcError(e.to_string()))?;
            let info: TransportAndIce =
                serde_json::from_value(resp).map_err(|_| Error::JsonDeserializeError)?;
            let did = transport
                .register_remote_info(Encoded::from_encoded_str(info.ice.as_str()))
                .await
                .map_err(Error::RegisterIceError)?;
            self.swarm
                .register(did, transport.clone())
                .await
                .map_err(Error::RegisterIceError)?;
            Ok(did)
        };
        if let Err(e) = addr_result {
            if let Err(close_e) = transport.close().await {
                tracing::warn!(
                    "connect_peer_via_http failed, close tranposrt error: {}",
                    close_e
                );
            }
            return Err(e);
        }
        Ok((transport, addr_result.unwrap().to_string()))
    }

    /// Answer an Offer.
    /// The process of manually handshake is:
    /// 1. PeerA: create_offer
    /// 2. PeerA: send the handshake info to PeerB.
    /// 3. PeerB: answer_offer
    /// 4. PeerB: send the handshake info to PeerA.
    /// 5. PeerA: accept_answer.
    pub async fn answer_offer(&self, ice_info: &str) -> Result<(Arc<Transport>, Encoded)> {
        tracing::info!("connect peer via ice: {}", ice_info);
        let transport = self.swarm.new_transport().await.map_err(|e| {
            tracing::error!("new_transport failed: {}", e);
            Error::NewTransportError
        })?;
        match self.handshake(&transport, ice_info).await {
            Ok(v) => Ok((transport, v)),
            Err(e) => {
                transport
                    .close()
                    .await
                    .map_err(Error::CloseTransportError)?;
                Err(e)
            }
        }
    }

    /// Connect peer with web3 did.
    /// There are 3 peers: PeerA, PeerB, PeerC.
    /// 1. PeerA has a connection with PeerB.
    /// 2. PeerC has a connection with PeerB.
    /// 3. PeerC can connect PeerA with PeerA's web3 address.
    pub async fn connect_with_did(&self, did: Did, wait_for_open: bool) -> Result<Peer> {
        let transport = self
            .swarm
            .connect(did)
            .await
            .map_err(Error::ConnectWithDidError)?;
        tracing::debug!("wait for transport connected");
        if wait_for_open {
            transport
                .wait_for_data_channel_open()
                .await
                .map_err(Error::ConnectWithDidError)?;
        }
        Ok(Peer::from((did, transport)))
    }

    async fn handshake(&self, transport: &Arc<Transport>, data: &str) -> Result<Encoded> {
        // get offer from remote and send answer back
        let hs_info = Encoded::from_encoded_str(data);
        let did = transport
            .register_remote_info(hs_info.to_owned())
            .await
            .map_err(Error::RegisterIceError)?;

        tracing::debug!("register: {}", did);
        self.swarm
            .register(did, Arc::clone(transport))
            .await
            .map_err(Error::RegisterIceError)?;

        let hs_info = transport
            .get_handshake_info(self.swarm.session_manager(), RTCSdpType::Answer)
            .await
            .map_err(Error::CreateAnswer)?;
        tracing::debug!("answer hs_info: {:?}", hs_info);
        Ok(hs_info)
    }

    /// Accept an answer of a connection.
    /// The process of manually handshake is:
    /// 1. PeerA: create_offer
    /// 2. PeerA: send the handshake info to PeerB.
    /// 3. PeerB: answer_offer
    /// 4. PeerB: send the handshake info to PeerA.
    /// 5. PeerA: accept_answer.
    pub async fn accept_answer(&self, transport_id: &str, ice: &str) -> Result<Peer> {
        let ice = Encoded::from_encoded_str(ice);
        tracing::debug!("accept_answer/ice: {:?}, uuid: {}", ice, transport_id);
        let transport_id =
            uuid::Uuid::from_str(transport_id).map_err(|_| Error::InvalidTransportId)?;
        let transport = self
            .swarm
            .find_pending_transport(transport_id)
            .map_err(Error::PendingTransport)?
            .ok_or(Error::TransportNotFound)?;
        let did = transport
            .register_remote_info(ice)
            .await
            .map_err(Error::RegisterIceError)?;
        self.swarm
            .register(did, transport.clone())
            .await
            .map_err(Error::RegisterIceError)?;
        if let Err(e) = self.swarm.pop_pending_transport(transport.id) {
            tracing::warn!("pop_pending_transport err: {}", e)
        };
        Ok(Peer::from((did, transport)))
    }

    /// List all peers.
    pub async fn list_peers(&self) -> Result<Vec<Peer>> {
        let transports = self.swarm.get_transports();
        tracing::debug!(
            "addresses: {:?}",
            transports.iter().map(|(a, _b)| a).collect::<Vec<_>>()
        );
        let data = transports.iter().map(|x| x.into()).collect::<Vec<Peer>>();
        Ok(data)
    }

    /// Get peer by remote did
    pub async fn get_peer(&self, did: Did) -> Result<Peer> {
        let transport = self
            .swarm
            .get_transport(did)
            .ok_or(Error::TransportNotFound)?;
        Ok(Peer::from(&(did, transport)))
    }

    /// Disconnect a peer with web3 did.
    pub async fn disconnect(&self, did: Did) -> Result<()> {
        let transport = self
            .swarm
            .get_transport(did)
            .ok_or(Error::TransportNotFound)?;
        transport
            .close()
            .await
            .map_err(Error::CloseTransportError)?;
        self.swarm.remove_transport(did);
        Ok(())
    }

    /// List all pending transport.
    pub async fn list_pendings(&self) -> Result<Vec<Arc<Transport>>> {
        let pendings = self
            .swarm
            .pending_transports()
            .await
            .map_err(|_| Error::InternalError)?;
        Ok(pendings)
    }

    /// Close pending transport
    pub async fn close_pending_transport(&self, transport_id: &str) -> Result<()> {
        let transport_id =
            uuid::Uuid::from_str(transport_id).map_err(|_| Error::InvalidTransportId)?;
        let transport = self
            .swarm
            .find_pending_transport(transport_id)
            .map_err(|_| Error::TransportNotFound)?
            .ok_or(Error::TransportNotFound)?;
        if transport.is_connected().await {
            transport
                .close()
                .await
                .map_err(Error::CloseTransportError)?;
        }
        self.swarm
            .pop_pending_transport(transport_id)
            .map_err(Error::CloseTransportError)?;
        Ok(())
    }

    /// Send custom message to a did.
    pub async fn send_message(&self, destination: &str, msg: &[u8]) -> Result<uuid::Uuid> {
        tracing::info!(
            "send_message, destination: {}, text: {:?}",
            destination,
            msg,
        );
        let destination = Did::from_str(destination).map_err(|_| Error::InvalidDid)?;

        let mut new_msg = Vec::with_capacity(msg.len() + 4);
        // chunked mark
        new_msg.push(0);
        new_msg.extend_from_slice(&[0u8; 3]);
        new_msg.extend_from_slice(msg);

        let msg = Message::custom(&new_msg, None).map_err(Error::SendMessage)?;

        let uuid = self
            .swarm
            .send_message(msg, destination)
            .await
            .map_err(Error::SendMessage)?;
        Ok(uuid)
    }

    /// send http request message to node
    /// - destination: did of destination
    /// - url: ipfs url
    /// - timeout: timeout in millisecond
    #[allow(clippy::too_many_arguments)]
    pub async fn send_http_request_message<U, T>(
        &self,
        destination: &str,
        name: U,
        method: http::Method,
        url: U,
        timeout: T,
        headers: &[(U, U)],
        body: Option<Bytes>,
    ) -> Result<uuid::Uuid>
    where
        U: ToString,
        T: Into<Timeout>,
    {
        let timeout: Timeout = timeout.into();
        tracing::info!(
            "send_http_request_message, destination: {}, url: {:?}, timeout: {:?}",
            destination,
            url.to_string(),
            timeout,
        );
        let msg: BackendMessage = BackendMessage::try_from((
            MessageType::HttpRequest,
            &HttpRequest::new(name, method, url, timeout, headers, body),
        ))?;
        let msg: Vec<u8> = msg.into();

        self.send_message(destination, &msg).await
    }

    /// send simple text message
    /// - destination: did of destination
    /// - text: text message
    pub async fn send_simple_text_message(
        &self,
        destination: &str,
        text: &str,
    ) -> Result<uuid::Uuid> {
        tracing::info!(
            "send_simple_text_message, destination: {}, text: {:?}",
            destination,
            text,
        );

        let msg: BackendMessage =
            BackendMessage::from((MessageType::SimpleText.into(), text.as_bytes()));
        let msg: Vec<u8> = msg.into();
        self.send_message(destination, &msg).await
    }

    /// send custom message
    /// - destination: did of destination
    /// - message_type: custom message type u16
    /// - extra: extra data
    /// - data: payload data
    pub async fn send_custom_message(
        &self,
        destination: &str,
        message_type: u16,
        data: Vec<u8>,
        extra: [u8; 30],
    ) -> Result<uuid::Uuid> {
        tracing::info!(
            "send_custom_message, destination: {}, message_type: {}",
            destination,
            message_type,
        );

        let msg: BackendMessage = BackendMessage::new(message_type, extra, data.as_ref());
        let msg: Vec<u8> = msg.into();
        self.send_message(destination, &msg[..]).await
    }

    /// check local cache of dht
    pub async fn storage_check_cache(&self, did: Did) -> Option<vnode::VirtualNode> {
        self.swarm.storage_check_cache(did).await
    }

    /// fetch virtual node from DHT
    pub async fn storage_fetch(&self, did: Did) -> Result<()> {
        self.swarm
            .storage_fetch(did)
            .await
            .map_err(error::Error::VNodeError)
    }

    /// store virtual node on DHT
    pub async fn storage_store(&self, vnode: vnode::VirtualNode) -> Result<()> {
        self.swarm
            .storage_store(vnode)
            .await
            .map_err(error::Error::VNodeError)
    }

    /// append data to a virtual node on DHT
    pub async fn storage_append_data(&self, topic: &str, data: Encoded) -> Result<()> {
        self.swarm
            .storage_append_data(topic, data)
            .await
            .map_err(error::Error::VNodeError)
    }

    /// register service
    pub async fn register_service(&self, name: &str) -> Result<()> {
        let encoded_did = self
            .did()
            .to_string()
            .encode()
            .map_err(Error::ServiceRegisterError)?;
        self.swarm
            .storage_touch_data(name, encoded_did)
            .await
            .map_err(error::Error::ServiceRegisterError)
    }
}

/// Peer struct
#[derive(Clone)]
pub struct Peer {
    /// web3 did of a peer.
    pub did: Token,
    /// transport of the connection.
    pub transport: Arc<Transport>,
}

impl From<(Did, Arc<Transport>)> for Peer {
    fn from((did, transport): (Did, Arc<Transport>)) -> Self {
        Self {
            did: did.into_token(),
            transport,
        }
    }
}

impl From<&(Did, Arc<Transport>)> for Peer {
    fn from((did, transport): &(Did, Arc<Transport>)) -> Self {
        Self {
            did: did.into_token(),
            transport: transport.clone(),
        }
    }
}

/// unpack custom message to text
pub fn unpack_text_message(msg: &CustomMessage) -> Result<String> {
    let (left, right) = msg.0.split_at(4);
    if left[0] != 0 {
        return Err(Error::InvalidData);
    }
    let text = String::from_utf8(right.to_vec()).unwrap();
    Ok(text)
}

#[cfg(test)]
#[cfg(feature = "node")]
mod test {
    use futures::lock::Mutex;

    use super::*;
    use crate::prelude::rings_core::ecc::SecretKey;
    use crate::prelude::rings_core::message::MessageHandler;
    use crate::prelude::rings_core::storage::PersistenceStorage;
    use crate::prelude::rings_core::swarm::SwarmBuilder;
    use crate::prelude::*;

    async fn new_processor() -> (Processor, String) {
        let key = SecretKey::random();

        let stun = "stun://stun.l.google.com:19302";
        let path = PersistenceStorage::random_path("./tmp");
        let storage = PersistenceStorage::new_with_path(path.as_str())
            .await
            .unwrap();

        let swarm = Arc::new(SwarmBuilder::new(stun, storage).key(key).build().unwrap());
        let stabilization = Arc::new(Stabilization::new(swarm.clone(), 200));
        ((swarm, stabilization).into(), path)
    }

    #[tokio::test]
    async fn test_processor_create_offer() {
        let (processor, path) = new_processor().await;
        let ti = processor.create_offer().await.unwrap();
        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 1);
        assert_eq!(pendings.get(0).unwrap().id.to_string(), ti.0.id.to_string());
        tokio::fs::remove_dir_all(path).await.unwrap();
    }

    #[tokio::test]
    async fn test_processor_list_pendings() {
        let (processor, path) = new_processor().await;
        let ti0 = processor.create_offer().await.unwrap();
        let ti1 = processor.create_offer().await.unwrap();
        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 2);
        let pending_ids = processor.list_pendings().await.unwrap();
        assert_eq!(pendings.len(), pending_ids.len());
        let ids = vec![ti0.0.id.to_string(), ti1.0.id.to_string()];
        for item in pending_ids {
            assert!(
                ids.contains(&item.id.to_string()),
                "id[{}] not in list",
                item.id
            );
        }
        tokio::fs::remove_dir_all(path).await.unwrap();
    }

    #[tokio::test]
    async fn test_processor_close_pending_transport() {
        let (processor, path) = new_processor().await;
        let ti0 = processor.create_offer().await.unwrap();
        let _ti1 = processor.create_offer().await.unwrap();
        let ti2 = processor.create_offer().await.unwrap();
        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 3);
        assert!(
            processor.close_pending_transport("abc").await.is_err(),
            "close_pending_transport() should be error"
        );
        let transport1 = processor
            .swarm
            .find_pending_transport(uuid::Uuid::from_str(ti0.0.id.to_string().as_str()).unwrap())
            .unwrap();
        assert!(transport1.is_some(), "transport_1 should be Some()");
        let transport1 = transport1.unwrap();
        assert!(
            processor
                .close_pending_transport(ti0.0.id.to_string().as_str())
                .await
                .is_ok(),
            "close_pending_transport({}) should be ok",
            ti0.0.id
        );
        assert!(!transport1.is_connected().await, "transport1 should closed");

        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 2);

        assert!(
            !pendings
                .iter()
                .any(|x| x.id.to_string() == ti0.0.id.to_string()),
            "transport[{}] should not in pending_transports",
            ti0.0.id
        );

        let transport2 = processor
            .swarm
            .find_pending_transport(uuid::Uuid::from_str(ti2.0.id.to_string().as_str()).unwrap())
            .unwrap();
        assert!(transport2.is_some(), "transport2 should be Some()");
        let transport2 = transport2.unwrap();
        assert!(
            processor
                .close_pending_transport(ti2.0.id.to_string().as_str())
                .await
                .is_ok(),
            "close_pending_transport({}) should be ok",
            ti0.0.id
        );
        assert!(!transport2.is_connected().await, "transport2 should closed");

        let pendings = processor.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings.len(), 1);

        assert!(
            !pendings
                .iter()
                .any(|x| x.id.to_string() == ti2.0.id.to_string()),
            "transport[{}] should not in pending_transports",
            ti0.0.id
        );
        tokio::fs::remove_dir_all(path).await.unwrap();
    }

    struct MsgCallbackStruct {
        msgs: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl MessageCallback for MsgCallbackStruct {
        async fn custom_message(
            &self,
            handler: &MessageHandler,
            _ctx: &MessagePayload<Message>,
            msg: &MaybeEncrypted<CustomMessage>,
        ) {
            let msg = handler.decrypt_msg(msg).unwrap();
            let text = unpack_text_message(&msg).unwrap();
            let mut msgs = self.msgs.try_lock().unwrap();
            msgs.push(text);
        }

        async fn builtin_message(&self, _handler: &MessageHandler, _ctx: &MessagePayload<Message>) {
        }
    }

    #[tokio::test]
    async fn test_processor_handshake_msg() {
        let (p1, path1) = new_processor().await;
        let (p2, path2) = new_processor().await;
        let did1 = p1.did().to_string();
        let did2 = p2.did().to_string();

        println!("p1_did: {}", did1);
        println!("p2_did: {}", did2);

        let msgs1: Arc<Mutex<Vec<String>>> = Default::default();
        let msgs2: Arc<Mutex<Vec<String>>> = Default::default();
        let callback1 = Box::new(MsgCallbackStruct {
            msgs: msgs1.clone(),
        });
        let callback2 = Box::new(MsgCallbackStruct {
            msgs: msgs2.clone(),
        });

        let msg_handler_1 = Arc::new(p1.swarm.create_message_handler(Some(callback1), None));
        let msg_handler_2 = Arc::new(p2.swarm.create_message_handler(Some(callback2), None));
        tokio::spawn(async { msg_handler_1.listen().await });
        tokio::spawn(async { msg_handler_2.listen().await });

        let (transport_1, offer) = p1.create_offer().await.unwrap();

        let pendings_1 = p1.swarm.pending_transports().await.unwrap();
        assert_eq!(pendings_1.len(), 1);
        assert_eq!(
            pendings_1.get(0).unwrap().id.to_string(),
            transport_1.id.to_string()
        );

        let (transport_2, answer) = p2.answer_offer(offer.as_str()).await.unwrap();
        let peer = p1
            .accept_answer(transport_1.id.to_string().as_str(), answer.as_str())
            .await
            .unwrap();

        assert!(peer.transport.id.eq(&transport_1.id), "transport not same");
        assert!(
            peer.did.to_string().eq(&did2),
            "peer.address got {}, expect: {}",
            peer.did,
            did2
        );
        println!("waiting for connection");
        transport_1
            .connect_success_promise()
            .await
            .unwrap()
            .await
            .unwrap();
        transport_2
            .connect_success_promise()
            .await
            .unwrap()
            .await
            .unwrap();

        assert!(
            transport_1.is_connected().await,
            "transport_1 not connected"
        );
        assert!(
            p1.swarm
                .get_transport(p2.did())
                .unwrap()
                .is_connected()
                .await,
            "p1 transport not connected"
        );
        assert!(
            transport_2.is_connected().await,
            "transport_2 not connected"
        );
        assert!(
            p2.swarm
                .get_transport(p1.did())
                .unwrap()
                .is_connected()
                .await,
            "p2 transport not connected"
        );

        println!("waiting for data channel ready");
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        let test_text1 = "test1";
        let test_text2 = "test2";

        println!("send_message 1");
        let uuid1 = p1
            .send_message(did2.as_str(), test_text1.as_bytes())
            .await
            .unwrap();
        println!("send_message 1 done, msg id: {}", uuid1);

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        println!("send_message 2");
        let uuid2 = p2
            .send_message(did1.as_str(), test_text2.as_bytes())
            .await
            .unwrap();
        println!("send_message 2 done, msg id: {}", uuid2);

        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        println!("check received");

        let mut msgs2 = msgs2.try_lock().unwrap();
        let got_msg2 = msgs2.pop().unwrap();
        assert!(
            got_msg2.eq(test_text1),
            "msg received, expect {}, got {}",
            test_text1,
            got_msg2
        );

        let mut msgs1 = msgs1.try_lock().unwrap();
        let got_msg1 = msgs1.pop().unwrap();
        assert!(
            got_msg1.eq(test_text2),
            "msg received, expect {}, got {}",
            test_text2,
            got_msg1
        );
        tokio::fs::remove_dir_all(path1).await.unwrap();
        tokio::fs::remove_dir_all(path2).await.unwrap();
    }

    #[test]
    fn test_create_and_verify_signature() {
        let key1 = SecretKey::random();
        let key2 = SecretKey::random();
        let signature = Processor::generate_signature(&key1);
        let verify1 = Processor::verify_signature(signature.as_bytes(), &key1.pubkey()).unwrap();
        assert!(verify1, "signature should be verified");
        let verify2 = Processor::verify_signature(b"abc", &key1.pubkey());
        assert!(verify2.is_err(), "verify2 should be error");
        let verify3 = Processor::verify_signature(signature.as_bytes(), &key2.pubkey()).unwrap();
        assert!(!verify3, "verify3 should be false");
    }
}
