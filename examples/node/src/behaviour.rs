use libp2p::core::identity::Keypair;
use libp2p::swarm::{behaviour::toggle::Toggle, NetworkBehaviour};
use libp2p::{autonat, dcutr, identify, kad, mdns::tokio as mdns, ping, relay};
use std::time::Duration;

use libp2p_bistream as bistream;

pub const PROTOCOL_VERSION: &str = "ipfs/0.1.0";
pub const AGENT_VERSION: &str = concat!("p2pshare/", env!("CARGO_PKG_VERSION"));

/// Libp2p behaviour for the node.
#[derive(NetworkBehaviour)]
pub struct NodeBehaviour {
    pub(crate) ping: ping::Behaviour,
    pub(crate) identify: identify::Behaviour,
    pub(crate) mdns: mdns::Behaviour,
    pub(crate) autonat: Toggle<autonat::Behaviour>,
    pub(crate) relay: Toggle<relay::v2::relay::Relay>,
    pub(crate) relay_client: relay::v2::client::Client,
    pub(crate) dcutr: dcutr::behaviour::Behaviour,
    pub(crate) bistream: bistream::Behaviour,
    pub(crate) kademlia: Toggle<kad::Kademlia<kad::store::MemoryStore>>,
}

impl NodeBehaviour {
    pub fn new(
        local_key: &Keypair,
        relay_client: relay::v2::client::Client,
    ) -> anyhow::Result<Self> {
        let autonat = false;
        let kad = false;
        let relay = true;

        let local_peer_id = local_key.public().to_peer_id();

        let autonat = if autonat {
            let pub_key = local_key.public();
            let config = autonat::Config {
                use_connected: true,
                boot_delay: Duration::from_secs(0),
                refresh_interval: Duration::from_secs(5),
                retry_interval: Duration::from_secs(5),
                ..Default::default()
            }; // TODO: configurable
            Some(autonat::Behaviour::new(pub_key.to_peer_id(), config))
        } else {
            None
        };

        let kad = if kad {
            let store = kad::store::MemoryStore::new(local_peer_id);
            let kademlia_config = kad::KademliaConfig::default();
            let kademlia = kad::Kademlia::with_config(local_peer_id, store, kademlia_config);
            Some(kademlia)
        } else {
            None
        };

        let relay = if relay {
            let config = relay::v2::relay::Config::default();
            Some(relay::v2::relay::Relay::new(local_peer_id, config))
        } else {
            None
        };

        let identify = {
            let config = identify::Config::new(PROTOCOL_VERSION.into(), local_key.public())
                .with_agent_version(String::from(AGENT_VERSION))
                .with_cache_size(64 * 1024);
            identify::Behaviour::new(config)
        };

        Ok(Self {
            autonat: autonat.into(),
            bistream: bistream::Behaviour::new(),
            dcutr: dcutr::behaviour::Behaviour::new(),
            identify,
            kademlia: kad.into(),
            mdns: mdns::Behaviour::new(Default::default())?,
            ping: ping::Behaviour::default(),
            relay: relay.into(),
            relay_client,
        })
    }
}
