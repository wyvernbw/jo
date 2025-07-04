#![feature(if_let_guard)]
#![feature(f16)]
#![feature(try_blocks)]
#![feature(unboxed_closures)]
#![feature(fn_traits)]

mod app;
mod process;

use std::{
    env::home_dir,
    io::Write,
    path::PathBuf,
    str::FromStr,
    sync::{LazyLock, Mutex},
};

use tracing::level_filters::LevelFilter;
use tracing_subscriber::util::SubscriberInitExt;

use crate::app::{App, Config};

static HOME_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| home_dir().expect("no home directory set up."));

static APP_DIR: LazyLock<PathBuf> = LazyLock::new(|| HOME_DIR.join(".jo/"));

static CONFIG_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    let res = std::env::var("XDG_CONFIG_HOME")
        .map(|str| PathBuf::from(str))
        .unwrap_or_else(|_| HOME_DIR.join(".config"))
        .join("jo/");
    if let Err(err) = std::fs::create_dir_all(&res) {
        tracing::error!(%err, "failed to create config dir");
    };
    res
});

static LOG_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    let env_file = std::env::var("JO_LOG").map(|path_str| PathBuf::from_str(&path_str).unwrap());
    let log_folder = env_file.unwrap_or(APP_DIR.join("logs/"));
    let path = log_folder.join(format!("{}.log", chrono::Local::now().date_naive()));
    if let Err(err) = std::fs::create_dir_all(log_folder) {
        tracing::warn!(%err, "unable to create logs directory: ")
    }
    path
});

static CONFIG_PATH: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let cfg = CONFIG_DIR.join("config.ron");
    if !cfg.exists() {
        let Ok(mut file) = std::fs::File::create(&cfg) else {
            return None;
        };
        let buf =
            ron::ser::to_string(&Config::default()).expect("failed to serialize default config.");
        file.write_all(buf.as_bytes());
    }
    Some(cfg)
});
static THEMES_DIR: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let res = CONFIG_DIR.join("themes/");
    if let Err(err) = std::fs::create_dir_all(&res) {
        tracing::error!(%err, "failed to create themes dir");
        return None;
    };
    Some(res)
});

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

pub fn default<T: Default>() -> T {
    T::default()
}

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

    LazyLock::force(&THEMES_DIR);
    LazyLock::force(&CONFIG_PATH);

    tracing::info!(?THEMES_DIR);
    tracing::info!(?CONFIG_PATH);
    tracing::info!(?LOG_PATH);

    let res = std::panic::catch_unwind(|| {
        ratatui::restore();
    });
    if let Err(err) = res {
        tracing::warn!(?err, "failed to hook panic handler.");
    }

    let mut terminal = ratatui::init();
    let app = App::default();
    let app_result = smol::block_on(app.run(&mut terminal));
    ratatui::restore();

    app_result
}
