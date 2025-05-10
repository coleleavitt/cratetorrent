use std::{io, net::SocketAddr, path::PathBuf};

use cratetorrent::prelude::*;
use flexi_logger::FileSpec;
use structopt::StructOpt;
use termion::raw::IntoRawMode;
use termion::screen::IntoAlternateScreen;
use termion::input::MouseTerminal;
use tui::{backend::TermionBackend, Terminal};

use app::App;
use key::Keys;

mod app;
mod key;
mod ui;
mod unit;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(StructOpt, Debug)]
pub struct Args {
    /// Whether to 'seed' or 'download' the torrent.
    #[structopt(
        long,
        parse(from_str = parse_mode),
        default_value = "Mode::Download { seeds: Vec::new() }",
    )]
    mode: Mode,

    /// The path of the folder where to download file.
    #[structopt(short, long)]
    download_dir: PathBuf,

    /// The path to the torrent metainfo file.
    #[structopt(short, long)]
    metainfo: PathBuf,

    /// A comma separated list of <ip>:<port> pairs of the seeds.
    #[structopt(short, long)]
    seeds: Option<Vec<SocketAddr>>,

    /// The socket address on which to listen for new connections.
    #[structopt(short, long)]
    listen: Option<SocketAddr>,

    #[structopt(short, long)]
    quit_after_complete: bool,
}

fn parse_mode(s: &str) -> Mode {
    match s {
        "seed" => Mode::Seed,
        _ => Mode::Download { seeds: Vec::new() },
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // initialize logging
    flexi_logger::Logger::try_with_env()?
        .log_to_file(FileSpec::default().directory("/tmp/cratetorrent"))
        .start()?;

    // parse CLI arguments
    let mut args = Args::from_args();
    if let Mode::Download { seeds } = &mut args.mode {
        *seeds = args.seeds.clone().unwrap_or_default();
    }
    let quit_after_complete = args.quit_after_complete;

    // set up TUI backend
    let stdout = io::stdout()
        .into_raw_mode()?                // RawTerminal<Stdout>
        .into_alternate_screen()?        // AlternateScreen<RawTerminal<Stdout>>
        ;
    let stdout = MouseTerminal::from(stdout);  // Mouse enabled
    let backend = TermionBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // initialize application state
    let mut app = App::new(args.download_dir.clone())?;
    let mut keys = Keys::new(key::EXIT_KEY);

    // start the single torrent
    app.create_torrent(args)?;

    // initial draw
    terminal.draw(|f| ui::draw(f, &mut app))?;

    // main event loop
    let mut run = true;
    while run {
        tokio::select! {
            Some(key) = keys.rx.recv() => {
                if key == key::EXIT_KEY {
                    run = false;
                }
            }
            Some(alert) = app.alert_rx.recv() => {
                match alert {
                    Alert::TorrentStats { id, stats } => {
                        app.update_torrent_state(id, *stats);
                    }
                    Alert::TorrentComplete(_) if quit_after_complete => {
                        run = false;
                    }
                    _ => {}
                }
            }
        }

        terminal.draw(|f| ui::draw(f, &mut app))?;

        // one final draw before exit so completion state is visible
        if !run {
            break;
        }
    }

    // shut down the engine gracefully
    app.engine.shutdown().await?;
    Ok(())
}
