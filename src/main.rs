#![feature(if_let_guard)]
#![feature(f16)]

mod app;
mod process;

use std::{
    env::home_dir,
    path::PathBuf,
    str::FromStr,
    sync::{LazyLock, Mutex},
};

use tracing::level_filters::LevelFilter;
use tracing_subscriber::util::SubscriberInitExt;

use crate::app::App;

static HOME_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| home_dir().expect("no home directory set up."));

static APP_DIR: LazyLock<PathBuf> = LazyLock::new(|| HOME_DIR.join(".jo/"));

static LOG_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    let env_file = std::env::var("JO_LOG").map(|path_str| PathBuf::from_str(&path_str).unwrap());
    let log_folder = env_file.unwrap_or(APP_DIR.join("logs/"));
    let path = log_folder.join(format!("{}.log", chrono::Local::now().date_naive()));
    if let Err(err) = std::fs::create_dir_all(log_folder) {
        tracing::warn!(%err, "unable to create logs directory: ")
    }
    path
});

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() -> color_eyre::Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();
    color_eyre::install()?;
    let writer = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(LOG_PATH.as_path())?;
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::DEBUG)
        .with_writer(Mutex::new(writer))
        .with_ansi(false)
        .finish()
        .init();
    tracing::info!("jo started");

    let mut terminal = ratatui::init();
    let mut app = App::default();
    app.tree_view = false;
    let app_result = smol::block_on(app.run(&mut terminal));

    ratatui::restore();
    app_result
}
