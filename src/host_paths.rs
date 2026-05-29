use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn phoxal_home() -> Result<PathBuf> {
    dirs::home_dir()
        .context("$HOME is not set; cannot locate ~/.phoxal")
        .map(|home| home.join(".phoxal"))
}

pub fn cache_dir() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("cache"))
}

pub fn worlds_dir() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("worlds"))
}

pub fn config_path() -> Result<PathBuf> {
    phoxal_home().map(|path| path.join("config.yaml"))
}
