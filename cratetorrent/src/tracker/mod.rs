//! BitTorrent tracker communication module.
//!
//! This module provides the functionality for communicating with BitTorrent trackers
//! via HTTP/HTTPS using the standard BitTorrent tracker protocol.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use bytes::Buf;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};
use reqwest::{Client, Url};
use serde::de;
use serde::Deserialize;

pub use reqwest::Error as HttpError;
pub(crate) type Result<T> = std::result::Result<T, TrackerError>;

/// All errors that may occur when contacting the tracker.
#[derive(Debug)]
#[non_exhaustive]
pub enum TrackerError {
    /// Bencode (de)serialization errors.
    Bencode(serde_bencode::Error),
    /// HTTP errors from reqwest.
    Http(HttpError),
}

impl From<serde_bencode::Error> for TrackerError {
    fn from(e: serde_bencode::Error) -> Self {
        TrackerError::Bencode(e)
    }
}

impl From<HttpError> for TrackerError {
    fn from(e: HttpError) -> Self {
        TrackerError::Http(e)
    }
}

impl fmt::Display for TrackerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrackerError::Bencode(e) => write!(f, "Bencode error: {}", e),
            TrackerError::Http(e)    => write!(f, "HTTP error: {}", e),
        }
    }
}

/// Optional announce events.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Event {
    Started,
    Completed,
    Stopped,
}

/// Parameters for an HTTP announce to a tracker.
pub(crate) struct Announce {
    pub info_hash:  [u8; 20],
    pub peer_id:    [u8; 20],
    pub port:       u16,
    pub ip:         Option<IpAddr>,
    pub downloaded: u64,
    pub uploaded:   u64,
    pub left:       u64,
    pub peer_count: Option<usize>,
    pub tracker_id: Option<String>,
    pub event:      Option<Event>,

    /// Append `&client=<string>` when `--features spoofing` is enabled.
    #[cfg(feature = "spoofing")]
    pub spoof_client: Option<String>,

    /// Inject extra peers when `--features peer_inject` is enabled.
    #[cfg(feature = "peer_inject")]
    pub extra_peers: Vec<SocketAddr>,

    /// Use this parameter when `--features upload_multiplier` is enabled to make torrent
    /// appear as a seeder by sending left=0
    #[cfg(feature = "upload_multiplier")]
    pub show_as_seeder: bool,
}

/// Bencoded tracker response.
#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct Response {
    #[serde(rename = "tracker id")]
    pub tracker_id:     Option<String>,
    #[serde(rename = "failure reason")]
    pub failure_reason: Option<String>,
    #[serde(rename = "warning message")]
    pub warning_message: Option<String>,

    #[serde(default)]
    #[serde(deserialize_with = "deserialize_seconds")]
    pub interval:       Option<Duration>,

    #[serde(default)]
    #[serde(rename = "min interval")]
    #[serde(deserialize_with = "deserialize_seconds")]
    pub min_interval:   Option<Duration>,

    #[serde(rename = "complete")]
    pub seeder_count:   Option<usize>,
    #[serde(rename = "incomplete")]
    pub leecher_count:  Option<usize>,

    #[serde(default)]
    #[serde(deserialize_with = "deserialize_peers")]
    pub peers: Vec<SocketAddr>,
}

/// HTTP tracker client.
#[derive(Clone)]
pub(crate) struct Tracker {
    client: Client,
    url: Url,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
}

impl Tracker {
    /// Construct a new `Tracker` pointing at `url`.
    pub fn new(url: Url, info_hash: [u8; 20], peer_id: [u8; 20]) -> Self {
        Tracker {
            client: Client::new(),
            url,
            info_hash,
            peer_id,
        }

    }

    /// Send an announce and parse the bencoded response.
    pub async fn announce(&self, params: Announce) -> Result<Response> {
        // Build query string
        let mut url = self.url.clone();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair(
                "info_hash",
                &percent_encoding::percent_encode(&params.info_hash, URL_ENCODE_RESERVED)
                    .to_string(),
            );
            q.append_pair(
                "peer_id",
                &percent_encoding::percent_encode(&params.peer_id, URL_ENCODE_RESERVED)
                    .to_string(),
            );
            q.append_pair("port", &params.port.to_string());
            q.append_pair("downloaded", &params.downloaded.to_string());
            q.append_pair("uploaded", &params.uploaded.to_string());

            // Handle upload_multiplier's show_as_seeder feature
            #[cfg(feature = "upload_multiplier")]
            let left = if params.show_as_seeder { 0 } else { params.left };
            #[cfg(not(feature = "upload_multiplier"))]
            let left = params.left;
            q.append_pair("left", &left.to_string());

            q.append_pair("compact", "1");

            if let Some(nw) = params.peer_count {
                q.append_pair("numwant", &nw.to_string());
            }
            if let Some(ip) = params.ip {
                q.append_pair("ip", &ip.to_string());
            }
            if let Some(event) = params.event {
                let event_str = match event {
                    Event::Started => "started",
                    Event::Completed => "completed",
                    Event::Stopped => "stopped",
                };
                q.append_pair("event", event_str);
            }
            if let Some(tracker_id) = &params.tracker_id {
                q.append_pair("trackerid", tracker_id);
            }

            // Optional client-string spoofing
            #[cfg(feature = "spoofing")]
            if let Some(client_str) = &params.spoof_client {
                q.append_pair("client", client_str);
            }
        }

        // Perform GET and collect bytes
        let bytes = self.client
            .get(url)
            .send().await?
            .error_for_status()?
            .bytes().await?;

        // Decode bencode into Response
        let mut resp: Response = serde_bencode::from_bytes(&bytes)?;

        // Optional peer injection
        #[cfg(feature = "peer_inject")]
        if !params.extra_peers.is_empty() {
            resp.peers.extend(params.extra_peers.clone());
        }

        Ok(resp)
    }

    /// Send a "stopped" announce to tell tracker we're no longer participating.
    ///
    /// This is a helper method used when ratio limits are reached or when shutting down.pub
    #[cfg(feature = "ratio")]
    pub async fn send_stopped(&self, torrent_id: crate::TorrentId, stats: &crate::torrent::stats::TorrentStats) -> Result<Response> {
        let announce = Announce {
            info_hash: [0; 20], // This should be the actual info_hash for the torrent
            peer_id: [0; 20],   // This should be the actual peer_id
            port: 0,
            ip: None,
            downloaded: stats.thruput.payload.down.total,  // Access total field directly
            uploaded: stats.thruput.payload.up.total,     // Access total field directly
            left: 0,            // When stopping, we report 0 left
            peer_count: Some(0), // Request no peers
            tracker_id: None,
            event: Some(Event::Stopped),

            #[cfg(feature = "spoofing")]
            spoof_client: None,

            #[cfg(feature = "peer_inject")]
            extra_peers: Vec::new(),

            #[cfg(feature = "upload_multiplier")]
            show_as_seeder: true,
        };

        self.announce(announce).await
    }
}

impl fmt::Display for Tracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tracker({})", self.url)
    }
}

/// Percent-encode all non-alphanumeric except `-._~`
const URL_ENCODE_RESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Deserialize a bencoded integer of seconds into `Duration`.
fn deserialize_seconds<'de, D>(deserializer: D) -> std::result::Result<Option<Duration>, D::Error>
where
    D: de::Deserializer<'de>,
{
    let opt: Option<u64> = Option::deserialize(deserializer)?;
    Ok(opt.map(Duration::from_secs))
}

/// Deserialize either a compact peer string or a list of `{ip, port}` dicts.
fn deserialize_peers<'de, D>(deserializer: D) -> std::result::Result<Vec<SocketAddr>, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct Visitor;
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = Vec<SocketAddr>;

        fn expecting(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
            fmt.write_str("compact peer string or list of {ip,port} dicts")
        }

        // Handle compact format: 6-byte entries
        fn visit_bytes<E>(self, mut b: &[u8]) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            const ENTRY: usize = 6;
            if b.len() % ENTRY != 0 {
                return Err(de::Error::custom("compact peers length must be multiple of 6"));
            }
            let mut peers = Vec::with_capacity(b.len() / ENTRY);
            while !b.is_empty() {
                let ip = Ipv4Addr::from(b.get_u32());
                let port = b.get_u16();
                peers.push(SocketAddr::new(IpAddr::V4(ip), port));
            }
            Ok(peers)
        }

        // Handle list of dicts
        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            #[derive(Deserialize)]
            struct Raw { ip: String, port: u16 }

            let mut peers = Vec::new();
            while let Some(Raw { ip, port }) = seq.next_element()? {
                if let Ok(addr) = ip.parse() {
                    peers.push(SocketAddr::new(addr, port));
                }
            }
            Ok(peers)
        }
    }

    deserializer.deserialize_any(Visitor)
}
