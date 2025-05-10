// src/tracker.rs

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
    /// HTTP errors from `reqwest`.
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
            TrackerError::Http(e) => write!(f, "HTTP error: {}", e),
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
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub ip: Option<IpAddr>,
    pub downloaded: u64,
    pub uploaded: u64,
    pub left: u64,
    pub peer_count: Option<usize>,
    #[allow(dead_code)]
    pub tracker_id: Option<String>,
    #[allow(dead_code)]
    pub event: Option<Event>,
}

/// Tracker’s announce response.
#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct Response {
    #[serde(rename = "tracker id")]
    pub tracker_id: Option<String>,
    #[serde(rename = "failure reason")]
    pub failure_reason: Option<String>,
    #[serde(rename = "warning message")]
    pub warning_message: Option<String>,

    #[serde(default)]
    #[serde(deserialize_with = "deserialize_seconds")]
    pub interval: Option<Duration>,

    #[serde(default)]
    #[serde(rename = "min interval")]
    #[serde(deserialize_with = "deserialize_seconds")]
    pub min_interval: Option<Duration>,

    #[serde(rename = "complete")]
    pub seeder_count: Option<usize>,
    #[serde(rename = "incomplete")]
    pub leecher_count: Option<usize>,

    #[serde(default)]
    #[serde(deserialize_with = "deserialize_peers")]
    pub peers: Vec<SocketAddr>,
}

/// HTTP tracker client.
pub(crate) struct Tracker {
    client: Client,
    url: Url,
}

impl Tracker {
    /// Construct a new `Tracker` pointing at `url`.
    pub fn new(url: Url) -> Self {
        Tracker {
            client: Client::new(),
            url,
        }
    }

    /// Send an announce and parse the bencoded response.
    pub async fn announce(&self, params: Announce) -> Result<Response> {
        // Build up query string
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
            q.append_pair("left", &params.left.to_string());
            q.append_pair("compact", "1");
            if let Some(nw) = params.peer_count {
                q.append_pair("numwant", &nw.to_string());
            }
            if let Some(ip) = params.ip {
                q.append_pair("ip", &ip.to_string());
            }
        }

        // Fire the GET, ensure status is 2XX, collect bytes
        let bytes = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;

        // Decode bencode
        let resp = serde_bencode::from_bytes(&bytes)?;
        Ok(resp)
    }
}

impl fmt::Display for Tracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tracker({})", self.url)
    }
}

/// Percent‐encode all non‐alphanumeric except `-._~`
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

/// Deserialize either a compact peer string or full list of dicts.
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

        fn visit_bytes<E>(self, mut b: &[u8]) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            const ENTRY: usize = 6;
            if b.len() % ENTRY != 0 {
                return Err(de::Error::custom(
                    "compact peers length must be multiple of 6",
                ));
            }
            let mut peers = Vec::with_capacity(b.len() / ENTRY);
            while !b.is_empty() {
                let ip = Ipv4Addr::from(b.get_u32());
                let port = b.get_u16();
                peers.push(SocketAddr::new(IpAddr::V4(ip), port));
            }
            Ok(peers)
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Server, Matcher};
    use serde::Serialize;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use reqwest::Url as ReqwestUrl;

    #[test]
    fn compact_parse_works() {
        #[derive(serde::Deserialize)]
        struct S { #[serde(deserialize_with = "deserialize_peers")] peers: Vec<SocketAddr> }

        let ip = Ipv4Addr::new(1, 2, 3, 4);
        let port = 6881u16;
        let mut enc = Vec::new();
        let data = {
            let mut v = Vec::new();
            v.extend_from_slice(&ip.octets());
            v.extend_from_slice(&port.to_be_bytes());
            v
        };
        enc.extend_from_slice(b"d5:peers");
        enc.extend_from_slice(format!("{}:", data.len()).as_bytes());
        enc.extend(data);
        enc.push(b'e');

        let got: S = serde_bencode::from_bytes(&enc).unwrap();
        assert_eq!(got.peers, vec![SocketAddr::new(ip.into(), port)]);
    }

    #[test]
    fn full_list_parse_works() {
        #[derive(Serialize)]
        struct RawPeer { ip: String, port: u16 }
        #[derive(Serialize)]
        struct RawPeers { peers: Vec<RawPeer> }

        let peers = RawPeers {
            peers: vec![
                RawPeer { ip: "127.0.0.1".into(), port: 1000 },
                RawPeer { ip: "8.8.8.8".into(), port: 53 },
            ],
        };

        let enc = serde_bencode::to_string(&peers).unwrap();
        #[derive(serde::Deserialize)]
        struct S { #[serde(deserialize_with = "deserialize_peers")] peers: Vec<SocketAddr> }

        let got: S = serde_bencode::from_str(&enc).unwrap();
        let want: Vec<_> = peers
            .peers
            .iter()
            .map(|p| SocketAddr::new(p.ip.parse().unwrap(), p.port))
            .collect();
        assert_eq!(got.peers, want);
    }

    #[tokio::test]
    async fn announce_returns_peers() {
        let mut server = Server::new_async().await;
        let m = server
            .mock("GET", "/")
            .match_query(Matcher::UrlEncoded("compact".into(), "1".into()))
            .with_status(200)
            .with_body({
                let mut v = Vec::new();
                v.extend_from_slice(b"d8:completei1e10:incompletei2e8:intervali5e5:peers6:");
                // one peer 10.0.0.1:6881
                v.extend_from_slice(&[10, 0, 0, 1, (6881 >> 8) as u8, (6881 & 0xff) as u8]);
                v.push(b'e');
                v
            })
            .create_async()
            .await;

        let url = ReqwestUrl::parse(&server.url()).unwrap();
        let tracker = Tracker::new(url);

        let ann = Announce {
            info_hash: [0u8; 20],
            peer_id: [0u8; 20],
            port: 6881,
            ip: None,
            downloaded: 0,
            uploaded: 0,
            left: 0,
            peer_count: None,
            tracker_id: None,
            event: None,
        };

        let resp = tracker.announce(ann).await.unwrap();
        assert_eq!(resp.seeder_count, Some(1));
        assert_eq!(resp.leecher_count, Some(2));
        assert_eq!(resp.interval, Some(Duration::from_secs(5)));
        assert_eq!(
            resp.peers,
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 6881)]
        );

        m.assert_async().await;
    }
}
