//! Implements a socket that can change its communication path while in use, actively searching for the best way to communicate.
//!
//! Based on tailscale/wgengine/magicsock

use std::collections::HashSet;
use std::{net::SocketAddr, time::Duration};

use tokio::time::Instant;

mod conn;
mod endpoint;
mod rebinding_conn;
mod timer;

pub use self::conn::Conn;
pub use self::timer::Timer;

use self::endpoint::Endpoint;

/// UDP socket read/write buffer size (7MB). The value of 7MB is chosen as it
/// is the max supported by a default configuration of macOS. Some platforms will silently clamp the value.
const SOCKET_BUFFER_SIZE: usize = 7 << 20;

/// All the information magicsock tracks about a particular peer.
#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub ep: Endpoint,
    /// An inverted version of `PeerMap.by_ip_port` (below), so
    /// that when we're deleting this node, we can rapidly find out the
    /// keys that need deleting from `PeerMap::by_ip_port` without having to
    /// iterate over every `SocketAddr known for any peer.
    pub ip_ports: HashSet<SocketAddr>, // TODO: figure out clone behaviour
}

impl PeerInfo {
    pub fn new(ep: Endpoint) -> Self {
        PeerInfo {
            ep,
            ip_ports: Default::default(),
        }
    }
}

/// How long since the last activity we try to keep an established endpoint peering alive.
/// It's also the idle time at which we stop doing STUN queries to keep NAT mappings alive.
const SESSION_ACTIVE_TIMEOUT: Duration = Duration::from_secs(45);

/// How often we try to upgrade to a better patheven if we have some non-DERP route that works.
const UPGRADE_INTERVAL: Duration = Duration::from_secs(1 * 60);

/// How often pings to the best UDP address are sent.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);

/// How long we trust a UDP address as the exclusive path (without using DERP) without having heard a Pong reply.
const TRUST_UDP_ADDR_DURATION: Duration = Duration::from_millis(6500);

/// The latency at or under which we don't try to upgrade to a better path.
const GOOD_ENOUGH_LATENCY: Duration = Duration::from_millis(5);

/// How long a non-home DERP connection needs to be idle (last written to) before we close it.
const DERP_INACTIVE_CLEANUP_TIME: Duration = Duration::from_secs(60);

/// How often `clean_stale_derp` runs when there are potentially-stale DERP connections to close.
const DERP_CLEAN_STALE_INTERVAL: Duration = Duration::from_secs(15);

/// How long we consider a STUN-derived endpoint valid for. UDP NAT mappings typically
/// expire at 30 seconds, so this is a few seconds shy of that.
const ENDPOINTS_FRESH_ENOUGH_DURATION: Duration = Duration::from_secs(27);

/// How long we wait for a pong reply before assuming it's never coming.
const PING_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// The minimum time between pings to an endpoint. (Except in the case of CallMeMaybe frames
/// resetting the counter, as the first pings likely didn't through the firewall)
const DISCO_PING_INTERVAL: Duration = Duration::from_secs(5);

/// How many `PongReply` values we keep per `EndpointState`.
const PONG_HISTORY_COUNT: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PongReply {
    latency: Duration,
    /// When we received the pong.
    pong_at: Instant,
    // The pong's src (usually same as endpoint map key).
    from: SocketAddr,
    // What they reported they heard.
    pong_src: SocketAddr,
}

#[derive(Debug)]
pub struct SentPing {
    pub to: SocketAddr,
    pub at: Instant,
    // timeout timer
    pub timer: Timer,
    pub purpose: DiscoPingPurpose,
}

/// The reason why a discovery ping message was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoPingPurpose {
    /// Means that purpose of a ping was to see if a path was valid.
    Discovery,
    /// Means that purpose of a ping was whether a peer was still there.
    Heartbeat,
    /// Mmeans that the user is running "tailscale ping" from the CLI. These types of pings can go over DERP.
    Cli,
}

// TODO: metrics
// var (
// 	metricNumPeers     = clientmetric.NewGauge("magicsock_netmap_num_peers")
// 	metricNumDERPConns = clientmetric.NewGauge("magicsock_num_derp_conns")

// 	metricRebindCalls     = clientmetric.NewCounter("magicsock_rebind_calls")
// 	metricReSTUNCalls     = clientmetric.NewCounter("magicsock_restun_calls")
// 	metricUpdateEndpoints = clientmetric.NewCounter("magicsock_update_endpoints")

// 	// Sends (data or disco)
// 	metricSendDERPQueued      = clientmetric.NewCounter("magicsock_send_derp_queued")
// 	metricSendDERPErrorChan   = clientmetric.NewCounter("magicsock_send_derp_error_chan")
// 	metricSendDERPErrorClosed = clientmetric.NewCounter("magicsock_send_derp_error_closed")
// 	metricSendDERPErrorQueue  = clientmetric.NewCounter("magicsock_send_derp_error_queue")
// 	metricSendUDP             = clientmetric.NewCounter("magicsock_send_udp")
// 	metricSendUDPError        = clientmetric.NewCounter("magicsock_send_udp_error")
// 	metricSendDERP            = clientmetric.NewCounter("magicsock_send_derp")
// 	metricSendDERPError       = clientmetric.NewCounter("magicsock_send_derp_error")

// 	// Data packets (non-disco)
// 	metricSendData            = clientmetric.NewCounter("magicsock_send_data")
// 	metricSendDataNetworkDown = clientmetric.NewCounter("magicsock_send_data_network_down")
// 	metricRecvDataDERP        = clientmetric.NewCounter("magicsock_recv_data_derp")
// 	metricRecvDataIPv4        = clientmetric.NewCounter("magicsock_recv_data_ipv4")
// 	metricRecvDataIPv6        = clientmetric.NewCounter("magicsock_recv_data_ipv6")

// 	// Disco packets
// 	metricSendDiscoUDP         = clientmetric.NewCounter("magicsock_disco_send_udp")
// 	metricSendDiscoDERP        = clientmetric.NewCounter("magicsock_disco_send_derp")
// 	metricSentDiscoUDP         = clientmetric.NewCounter("magicsock_disco_sent_udp")
// 	metricSentDiscoDERP        = clientmetric.NewCounter("magicsock_disco_sent_derp")
// 	metricSentDiscoPing        = clientmetric.NewCounter("magicsock_disco_sent_ping")
// 	metricSentDiscoPong        = clientmetric.NewCounter("magicsock_disco_sent_pong")
// 	metricSentDiscoCallMeMaybe = clientmetric.NewCounter("magicsock_disco_sent_callmemaybe")
// 	metricRecvDiscoBadPeer     = clientmetric.NewCounter("magicsock_disco_recv_bad_peer")
// 	metricRecvDiscoBadKey      = clientmetric.NewCounter("magicsock_disco_recv_bad_key")
// 	metricRecvDiscoBadParse    = clientmetric.NewCounter("magicsock_disco_recv_bad_parse")

// 	metricRecvDiscoUDP                 = clientmetric.NewCounter("magicsock_disco_recv_udp")
// 	metricRecvDiscoDERP                = clientmetric.NewCounter("magicsock_disco_recv_derp")
// 	metricRecvDiscoPing                = clientmetric.NewCounter("magicsock_disco_recv_ping")
// 	metricRecvDiscoPong                = clientmetric.NewCounter("magicsock_disco_recv_pong")
// 	metricRecvDiscoCallMeMaybe         = clientmetric.NewCounter("magicsock_disco_recv_callmemaybe")
// 	metricRecvDiscoCallMeMaybeBadNode  = clientmetric.NewCounter("magicsock_disco_recv_callmemaybe_bad_node")
// 	metricRecvDiscoCallMeMaybeBadDisco = clientmetric.NewCounter("magicsock_disco_recv_callmemaybe_bad_disco")

// 	// metricDERPHomeChange is how many times our DERP home region DI has
// 	// changed from non-zero to a different non-zero.
// 	metricDERPHomeChange = clientmetric.NewCounter("derp_home_change")

// 	// Disco packets received bpf read path
// 	metricRecvDiscoPacketIPv4 = clientmetric.NewCounter("magicsock_disco_recv_bpf_ipv4")
// 	metricRecvDiscoPacketIPv6 = clientmetric.NewCounter("magicsock_disco_recv_bpf_ipv6")
// )

// TODO: better place

#[macro_export]
macro_rules! measure {
    ($name:expr, $block:expr) => {{
        let start = Instant::now();
        let res = $block;
        tracing::info!("{} took {}ms", $name, start.elapsed().as_millis());
        res
    }};
}
