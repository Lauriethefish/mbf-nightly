mod requests;
mod manifest;
mod axml;
mod patching;
mod mod_man;
mod handlers;
mod data_fix;
mod downloads;

use crate::requests::Request;
use anyhow::{Context, Result};
use const_format::formatcp;
use downloads::DownloadConfig;
use log::{debug, error, warn, Level};
use mbf_res_man::res_cache::ResCache;
use requests::Response;
use serde::{Deserialize, Serialize};
use std::{io::{BufRead, BufReader, Write}, panic, path::Path, process::Command, sync};

#[cfg(feature = "request_timing")]
use log::info;
#[cfg(feature = "request_timing")]
use std::time::Instant;

// Directories accessed by the agent, in one place so that they can be easily changed.
pub const APK_ID: &str = "com.beatgames.beatsaber";
// `$` is replaced with the game version
pub const QMODS_DIR: &str = formatcp!("/sdcard/ModData/{APK_ID}/Packages/$");
pub const NOMEDIA_PATH: &str = formatcp!("/sdcard/ModData/{APK_ID}/.nomedia");
pub const MODLOADER_DIR: &str = formatcp!("/sdcard/ModData/{APK_ID}/Modloader");
pub const LATE_MODS_DIR: &str = formatcp!("{MODLOADER_DIR}/mods");
pub const EARLY_MODS_DIR: &str = formatcp!("{MODLOADER_DIR}/early_mods");
pub const LIBS_DIR: &str = formatcp!("{MODLOADER_DIR}/libs");
pub const APP_DATA_PATH: &str = formatcp!("/sdcard/Android/data/{APK_ID}/files");
pub const PLAYER_DATA_PATH: &str = formatcp!("{APP_DATA_PATH}/PlayerData.dat");
pub const PLAYER_DATA_BAK_PATH: &str = formatcp!("{APP_DATA_PATH}/PlayerData.dat.bak");
pub const APP_OBB_PATH: &str = formatcp!("/sdcard/Android/obb/{APK_ID}/");

pub const DATAKEEPER_PATH: &str = "/sdcard/ModData/com.beatgames.beatsaber/Mods/datakeeper/PlayerData.dat";
pub const DATA_BACKUP_PATH: &str = "/sdcard/ModsBeforeFriday/PlayerData.backup.dat";
pub const OLD_QMODS_DIR: &str = "/sdcard/ModsBeforeFriday/Mods";

pub const SONGS_PATH: &str = formatcp!("/sdcard/ModData/{APK_ID}/Mods/SongCore/CustomLevels");
pub const DOWNLOADS_PATH: &str = "/data/local/tmp/mbf/downloads";
pub const TEMP_PATH: &str = "/data/local/tmp/mbf/tmp";
pub const RES_CACHE_PATH: &str = "/data/local/tmp/mbf/res-cache";
pub const LEGACY_DIRS: &[&str] = &[
    "/data/local/tmp/mbf-downloads",
    "/data/local/tmp/mbf-res-cache",
    "/data/local/tmp/mbf-tmp",
    "/data/local/tmp/mbf-uploads"
];

/// Attempts to delete legacy directories no longer used by MBF to free up space
/// Logs on failure
pub fn try_delete_legacy_dirs() {
    for dir in LEGACY_DIRS {
        if Path::new(dir).exists() {
            match std::fs::remove_dir_all(dir) {
                Ok(_) => debug!("Successfully removed legacy dir {dir}"),
                Err(err) => warn!("Failed to remove legacy dir {dir}: {err}")
            }
        }
    }
}


static DOWNLOAD_CFG: sync::OnceLock<DownloadConfig> = sync::OnceLock::new();

/// Gets the default config used for downloads in MBF
pub fn get_dl_cfg() -> &'static DownloadConfig<'static> {
    DOWNLOAD_CFG.get_or_init(|| {
        DownloadConfig {
            max_disconnections: 10,
            // If downloads data successfully for 10 seconds, reset disconnection attempts
            disconnection_reset_time: Some(std::time::Duration::from_secs_f32(10.0)),
            disconnect_wait_time: std::time::Duration::from_secs_f32(5.0),
            progress_update_interval: Some(std::time::Duration::from_secs_f32(2.0)),
            ureq_agent: mbf_res_man::default_agent::get_agent(),
        }
    })
}

/// Creates a ResCache for downloading files using mbf_res_man
/// This should be reused where possible.
pub fn load_res_cache() -> Result<ResCache<'static>> {
    std::fs::create_dir_all(RES_CACHE_PATH).expect("Failed to create resource cache folder");
    Ok(ResCache::new(RES_CACHE_PATH.into(),
        mbf_res_man::default_agent::get_agent()))
}

pub fn get_apk_path() -> Result<Option<String>> {
    let pm_output = Command::new("pm")
        .args(["path", APK_ID])
        .output()
        .context("Working out APK path")?;
    if 8 > pm_output.stdout.len() {
        // App not installed
        Ok(None)
    }   else {
        Ok(Some(std::str::from_utf8(pm_output.stdout.split_at(8).1)?
            .trim_end()
            .to_owned()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct ModTag {
    patcher_name: String,
    patcher_version: Option<String>,
    modloader_name: String,
    modloader_version: Option<String>
}

struct ResponseLogger {}

impl log::Log for ResponseLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &log::Record) {
        // Skip logs that are not from mbf_agent, mbf_zip, etc.
        // ...as these are spammy logs from ureq or rustls, and we do nto want them.
        match record.module_path() {
            Some(module_path) => if !module_path.starts_with("mbf") {
                return;
            },
            None => return
        }

        // Ignore errors, logging should be infallible and we don't want to panic
        let _result = write_response(Response::LogMsg {
            message: format!("{}", record.args()),
            level: match record.level() {
                Level::Debug => requests::LogLevel::Debug,
                Level::Info => requests::LogLevel::Info,
                Level::Warn => requests::LogLevel::Warn,
                Level::Error => requests::LogLevel::Error,
                Level::Trace => requests::LogLevel::Trace
            }
        });
    }

    fn flush(&self) {
        let _ = std::io::stdout().flush();
    }
}

fn write_response(response: Response) -> Result<()> {
    let mut lock = std::io::stdout().lock();
    serde_json::to_writer(&mut lock, &response).context("Serializing JSON response")?;
    writeln!(lock)?;
    Ok(())
}

static LOGGER: ResponseLogger = ResponseLogger {};

fn main() -> Result<()> {
    #[cfg(feature = "request_timing")]
    let start_time = Instant::now();

    log::set_logger(&LOGGER).expect("Failed to set up logging");
    log::set_max_level(log::LevelFilter::Debug);

    let mut reader = BufReader::new(std::io::stdin());
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let req: Request = serde_json::from_str(&line)?;

    // Set a panic hook that writes the panic as a JSON Log
    // (we don't do this in catch_unwind as we get an `Any` there, which doesn't implement Display)
    panic::set_hook(Box::new(|info| error!("Request failed due to a panic!: {info}")));

    match std::panic::catch_unwind(|| handlers::handle_request(req)) {
        Ok(resp) => match resp {
            Ok(resp) => {
                #[cfg(feature = "request_timing")]
                {
                    let req_time = Instant::now() - start_time;
                    info!("Request complete in {}ms", req_time.as_millis());
                }

                write_response(resp)?;
            },
            Err(err) => error!("{err:?}")
        },
        Err(_) => {} // Panic will be outputted above
    };

    Ok(())
}