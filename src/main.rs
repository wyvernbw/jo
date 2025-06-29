mod app;
mod process;

use std::{
    env::home_dir,
    path::PathBuf,
    str::FromStr,
    sync::{LazyLock, Mutex},
};

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

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .append(true)
        .open(LOG_PATH.as_path())?;
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(writer))
        .with_ansi(false)
        .finish()
        .init();
    tracing::info!("jo started");

    let mut terminal = ratatui::init();
    let app_result = App::default().run(&mut terminal);

    ratatui::restore();
    app_result
}
