//! The engine is the top-level coordinator that runs and manages all entities
//! in the torrent engine. The user interacts with the engine via the
//! [`EngineHandle`] which exposes a restricted public API.
//!
//! The engine is spawned as a tokio task and runs in the background.
//! As with spawning other tokio tasks, it must be done within the context of
//! a tokio executor.
//!
//! The engine is run until an unrecoverable error occurs, or until the user
//! sends a shutdown command.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
};

use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task,
};

use crate::{
    alert::{AlertReceiver, AlertSender},
    conf::{Conf, TorrentConf},
    disk::{self, error::NewTorrentError},
    error::*,
    metainfo::Metainfo,
    storage_info::StorageInfo,
    torrent::{self, Torrent},
    tracker::Tracker,
    Bitfield, TorrentId,
};

/// Spawns the engine as a tokio task.
///
/// As with spawning other tokio tasks, it must be done within the context of
/// a tokio executor.
///
/// The return value is a tuple of an [`EngineHandle`], which may be used to
/// send the engine commands, and an [`crate::alert::AlertReceiver`], to which
/// various components in the engine will send alerts of events.
pub fn spawn(conf: Conf) -> Result<(EngineHandle, AlertReceiver)> {
    log::info!("Spawning engine task");

    // create alert channels and return alert port to user
    let (alert_tx, alert_rx) = mpsc::unbounded_channel();
    let (mut engine, tx) = Engine::new(conf, alert_tx)?;

    let join_handle = task::spawn(async move { engine.run().await });
    log::info!("Spawned engine task");

    Ok((
        EngineHandle {
            tx,
            join_handle: Some(join_handle),
        },
        alert_rx,
    ))
}

type JoinHandle = task::JoinHandle<Result<()>>;

/// A handle to the currently running torrent engine.
pub struct EngineHandle {
    tx: Sender,
    join_handle: Option<JoinHandle>,
}

impl EngineHandle {
    /// Creates and starts a torrent, if its metainfo is valid.
    ///
    /// If successful, it returns the id of the torrent. This id can be used to
    /// identify the torrent when issuing further commands to engine.
    pub fn create_torrent(&self, params: TorrentParams) -> Result<TorrentId> {
        log::trace!("Creating torrent");
        let id = TorrentId::new();
        self.tx.send(Command::CreateTorrent { id, params })?;
        Ok(id)
    }

    /// Gracefully shuts down the engine and waits for all its torrents to do
    /// the same.
    ///
    /// # Panics
    ///
    /// This method panics if the engine has already been shut down.
    pub async fn shutdown(mut self) -> Result<()> {
        log::trace!("Shutting down engine task");
        self.tx.send(Command::Shutdown)?;
        if let Err(e) = self
            .join_handle
            .take()
            .expect("engine already shut down")
            .await
            .expect("task error")
        {
            log::error!("Engine error: {}", e);
        }
        Ok(())
    }
}

/// Information for creating a new torrent.
pub struct TorrentParams {
    /// Contains the torrent's metadata.
    pub metainfo: Metainfo,
    /// If set, overrides the default global config.
    pub conf: Option<TorrentConf>,
    /// Whether to download or seed the torrent.
    ///
    /// This is expected to be removed as this will become automatic once
    /// torrent resume data is supported.
    pub mode: Mode,
    /// The address on which the torrent should listen for new peers.
    ///
    /// This has to be unique for each torrent. If not set, or if already in
    /// use, a random port is assigned.
    pub listen_addr: Option<SocketAddr>,
}

/// The download mode.
#[derive(Debug)]
pub enum Mode {
    Download { seeds: Vec<SocketAddr> },
    Seed,
}

/// The channel through which the user can send commands to the engine.
pub(crate) type Sender = UnboundedSender<Command>;
/// The channel on which the engine listens for commands from the user.
type Receiver = UnboundedReceiver<Command>;

/// The type of commands that the engine can receive.
pub(crate) enum Command {
    /// Contains the information for creating a new torrent.
    CreateTorrent {
        id: TorrentId,
        params: TorrentParams,
    },
    /// Torrent allocation result. If successful, the id of the allocated
    /// torrent is returned for identification, if not, the reason of the error
    /// is included.
    TorrentAllocation {
        id: TorrentId,
        result: Result<(), NewTorrentError>,
    },
    /// Gracefully shuts down the engine and waits for all its torrents to do
    /// the same.
    Shutdown,
}

struct Engine {
    /// All currently running torrents in engine.
    torrents: HashMap<TorrentId, TorrentEntry>,

    /// The port on which other entities in the engine, or the API consumer
    /// sends the engine commands.
    cmd_rx: Receiver,

    /// The disk channel.
    disk_tx: disk::Sender,
    disk_join_handle: Option<disk::JoinHandle>,

    /// The channel on which tasks in the engine post alerts to user.
    alert_tx: AlertSender,

    /// The global engine configuration that includes defaults for torrents
    /// whose config is not overridden.
    conf: Conf,
}

#[cfg(feature = "ratio")]
struct TorrentEntry {
    tx: torrent::Sender,
    join_handle: Option<task::JoinHandle<torrent::error::Result<()>>>,
    info_hash: [u8; 20],
    trackers: Vec<Tracker>,
}

#[cfg(not(feature = "ratio"))]
struct TorrentEntry {
    tx: torrent::Sender,
    join_handle: Option<task::JoinHandle<torrent::error::Result<()>>>,
}


impl Engine {
    /// Creates a new engine, spawning the disk task.
    fn new(conf: Conf, alert_tx: AlertSender) -> Result<(Self, Sender)> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (disk_join_handle, disk_tx) = disk::spawn(cmd_tx.clone())?;

        Ok((
            Self {
                torrents: HashMap::new(),
                cmd_rx,
                disk_tx,
                disk_join_handle: Some(disk_join_handle),
                alert_tx,
                conf,
            },
            cmd_tx,
        ))
    }

    /// Runs the engine until an unrecoverable error occurs, or until the user
    /// sends a shutdown command.
    async fn run(&mut self) -> Result<()> {
        log::info!("Starting engine");

        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                Command::CreateTorrent { id, params } => {
                    self.create_torrent(id, params).await?;
                }
                Command::TorrentAllocation { id, result } => match result {
                    Ok(_) => {
                        log::info!("Torrent {} allocated on disk", id);
                    }
                    Err(e) => {
                        log::error!(
                            "Error allocating torrent {} on disk: {}",
                            id,
                            e
                        );
                    }
                },
                Command::Shutdown => {
                    self.shutdown().await?;
                    break;
                }
            }
        }

        Ok(())
    }

    /// Creates and spawns a new torrent based on the parameters given.
    async fn create_torrent(
        &mut self,
        id: TorrentId,
        params: TorrentParams,
    ) -> Result<()> {
        let conf = params.conf.unwrap_or_else(|| self.conf.torrent.clone());
        let storage_info = StorageInfo::new(
            &params.metainfo,
            self.conf.engine.download_dir.clone(),
        );

        // Create trackers from the metainfo URLs
        let trackers: Vec<_> = params
            .metainfo
            .trackers
            .into_iter()
            .map(|url| Tracker::new(
                url,
                params.metainfo.info_hash,
                self.conf.engine.client_id
            ))
            .collect();

        let own_pieces = params.mode.own_pieces(storage_info.piece_count);

        // Create and spawn the torrent
        let (mut torrent, torrent_tx) = Torrent::new(torrent::Params {
            id,
            disk_tx: self.disk_tx.clone(),
            info_hash: params.metainfo.info_hash,
            storage_info: storage_info.clone(),
            own_pieces,
            trackers: trackers.clone(),
            client_id: self.conf.engine.client_id,
            listen_addr: params.listen_addr.unwrap_or_else(|| {
                // the port 0 tells the kernel to assign a free port from the
                // dynamic range
                SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)
            }),
            conf,
            alert_tx: self.alert_tx.clone(),
        });

        // Allocate torrent on disk
        self.disk_tx.send(disk::Command::NewTorrent {
            id,
            storage_info,
            piece_hashes: params.metainfo.pieces,
            torrent_tx: torrent_tx.clone(),
        })?;

        let seeds = params.mode.seeds();
        let join_handle =
            task::spawn(async move { torrent.start(&seeds).await });

        // Create the torrent entry based on feature flags
        #[cfg(feature = "ratio")]
        let entry = TorrentEntry {
            tx: torrent_tx,
            join_handle: Some(join_handle),
            info_hash: params.metainfo.info_hash,
            trackers,
        };

        #[cfg(not(feature = "ratio"))]
        let entry = TorrentEntry {
            tx: torrent_tx,
            join_handle: Some(join_handle),
        };

        self.torrents.insert(id, entry);

        Ok(())
    }

    /// Gracefully shuts down the engine and all its components.
    async fn shutdown(&mut self) -> Result<()> {
        log::info!("Shutting down engine");

        // First get stats from all torrents before shutting them down
        #[cfg(feature = "ratio")]
        let mut stats_map = HashMap::new();
        #[cfg(feature = "ratio")]
        {
            for (id, torrent) in &self.torrents {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if torrent.tx.send(torrent::Command::GetStats { resp: tx }).is_ok() {
                    if let Ok(stats) = rx.await {
                        stats_map.insert(*id, stats);
                    }
                }
            }
        }

        // Tell all torrents to shut down and join their tasks
        for (id, torrent) in self.torrents.iter_mut() {
            // Send stopped announcement to trackers before shutting down
            #[cfg(feature = "ratio")]
            if let Some(stats) = stats_map.get(id) {
                for tracker in &torrent.trackers {
                    if let Err(e) = tracker.send_stopped(*id, stats).await {
                        log::warn!("Failed to send stopped announcement to tracker: {}", e);
                    }
                }
            }

            // The torrent task may no longer be running, so don't panic here
            torrent.tx.send(torrent::Command::Shutdown).ok();
        }

        // Then join all torrent task handles. Shutting down a torrent may take
        // a while, so join as a separate step to first initiate the shutdown of
        // all torrents.
        for torrent in self.torrents.values_mut() {
            if let Some(handle) = torrent.join_handle.take() {
                if let Err(e) = handle.await.expect("task error") {
                    log::error!("Torrent error: {}", e);
                }
            }
        }

        // Send a shutdown command to disk
        self.disk_tx.send(disk::Command::Shutdown)?;
        // And join on its handle
        if let Some(handle) = self.disk_join_handle.take() {
            handle.await
                .expect("Disk task has panicked")
                .map_err(Error::from)?;
        }

        Ok(())
    }
}

impl Mode {
    fn own_pieces(&self, piece_count: usize) -> Bitfield {
        match self {
            Self::Download { .. } => Bitfield::repeat(false, piece_count),
            Self::Seed => Bitfield::repeat(true, piece_count),
        }
    }

    fn seeds(self) -> Vec<SocketAddr> {
        match self {
            Self::Download { seeds } => seeds,
            _ => Vec::new(),
        }
    }
}
