use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn phoxal_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PHOXAL_HOME") {
        return Ok(PathBuf::from(path));
    }
    dirs::home_dir()
        .context("$HOME is not set; cannot locate ~/.phoxal")
        .map(|home| home.join(".phoxal"))
}

pub fn cache_dir() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("cache"))
}

pub fn tools_cache_dir() -> Result<PathBuf> {
    cache_dir().map(|path| path.join("tools"))
}

pub fn worlds_dir() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("worlds"))
}

pub fn run_dir() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("run"))
}

pub fn config_path() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("config.yaml"))
}
