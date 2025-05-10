//! Global and per‐torrent configuration, with optional mod flags.

use std::{path::PathBuf, time::Duration};

// Keep the first import conditional
#[cfg(feature = "spoofing")]
use crate::PeerId;

// Import for all builds (remove the duplicate below)
#[cfg(not(feature = "spoofing"))]
use crate::PeerId;

#[cfg(feature = "peer_inject")]
use std::net::SocketAddr;

/// The default cratetorrent client id (for spoofing).
#[cfg(feature = "spoofing")]
pub const CRATETORRENT_CLIENT_ID: &PeerId = b"cbt-0000000000000000";

/// Top‐level configuration for the engine.
#[derive(Clone, Debug)]
pub struct Conf {
    pub engine: EngineConf,
    pub torrent: TorrentConf,
    #[cfg(any(feature = "ghostleech", feature = "ratio"))]
    pub mod_conf: ExtremeModConf,
}

impl Conf {
    pub fn new(download_dir: impl Into<PathBuf>) -> Self {
        Self {
            engine: EngineConf {
                #[cfg(feature = "spoofing")]
                client_id: *CRATETORRENT_CLIENT_ID,
                #[cfg(not(feature = "spoofing"))]
                client_id: Default::default(),
                download_dir: download_dir.into(),
            },
            torrent: TorrentConf::default(),
            #[cfg(any(feature = "ghostleech", feature = "ratio"))]
            mod_conf: ExtremeModConf::default(),
        }
    }
}

/// Engine‐wide settings.
#[derive(Clone, Debug)]
pub struct EngineConf {
    /// The 20‐byte peer_id sent in tracker announces.
    pub client_id: PeerId,
    /// Directory for downloads and seeds.
    pub download_dir: PathBuf,
}

/// Per‐torrent settings.
#[derive(Clone, Debug)]
pub struct TorrentConf {
    pub min_requested_peer_count: usize,
    pub max_connected_peer_count: usize,
    pub announce_interval: Duration,
    pub tracker_error_threshold: usize,

    #[cfg(feature = "spoofing")]
    /// Append `&client=<string>` to tracker announces.
    pub spoof_client: Option<String>,

    #[cfg(feature = "peer_inject")]
    /// Extra peers to append to each tracker response.
    pub extra_peers: Vec<SocketAddr>,

    #[cfg(feature = "ghostleech")]
    /// Never honor peer REQUESTs, always choke.
    pub ghost_leech: bool,

    #[cfg(feature = "ratio")]
    /// Stop uploading when uploaded/downloaded ≥ this ratio.
    pub max_ratio: Option<f64>,

    #[cfg(feature = "upload_multiplier")]
    /// Multiply reported upload stats by this factor
    pub upload_multiplier: Option<f64>,

    pub alerts: TorrentAlertConf,
}


impl Default for TorrentConf {
    fn default() -> Self {
        Self {
            min_requested_peer_count: 10,
            max_connected_peer_count: 50,
            announce_interval: Duration::from_secs(60 * 60),
            tracker_error_threshold: 15,
            #[cfg(feature = "spoofing")]
            spoof_client: None,
            #[cfg(feature = "peer_inject")]
            extra_peers: Vec::new(),
            #[cfg(feature = "ghostleech")]
            ghost_leech: false,
            #[cfg(feature = "ratio")]
            max_ratio: None,
            #[cfg(feature = "upload_multiplier")]
            upload_multiplier: None,
            alerts: Default::default(),
        }
    }
}


/// Optional engine alerts per torrent.
#[derive(Clone, Debug, Default)]
pub struct TorrentAlertConf {
    pub completed_pieces: bool,
    pub peers: bool,
}

/// "Extreme mod" flags: ghost‐leech and upload‐ratio guard.
#[derive(Clone, Debug)]
pub struct ExtremeModConf {
    /// Never honor peer REQUESTs, always choke.
    #[cfg(feature = "ghostleech")]
    pub ghost_leech: bool,

    /// Stop uploading when uploaded/downloaded ≥ this ratio.
    #[cfg(feature = "ratio")]
    pub max_ratio: Option<f64>,
}

impl Default for ExtremeModConf {
    fn default() -> Self {
        Self {
            #[cfg(feature = "ghostleech")]
            ghost_leech: false,
            #[cfg(feature = "ratio")]
            max_ratio: None,
        }
    }
}
