//! Filesystem layout of udnf, all under the user's XDG directories.

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    /// Relocated root: `/usr/bin/x` lives at `root/usr/bin/x`.
    pub root: PathBuf,
    pub state_file: PathBuf,
    pub lock_file: PathBuf,
    pub persist: PathBuf,
    /// User rpm keystore (gpg-pubkey files).
    pub keyring: PathBuf,
    /// `~/.cache/udnf` (libdnf5 metadata cache)
    pub cache: PathBuf,
    /// Downloaded RPMs.
    pub packages: PathBuf,
    pub user_config: PathBuf,
    pub repos_d: PathBuf,
    pub repos_override_d: PathBuf,
    pub vars: PathBuf,
    /// `~/.local/state/udnf`
    pub logs: PathBuf,
    /// Export targets.
    pub bin: PathBuf,
    /// XDG_DATA_HOME (`~/.local/share`): applications, icons, fonts, man pages.
    pub data_home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = dirs::home_dir().context("HOME introuvable")?;
        let data_home = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));
        let cache_home = dirs::cache_dir().unwrap_or_else(|| home.join(".cache"));
        let config_home = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        let state_home = dirs::state_dir().unwrap_or_else(|| home.join(".local/state"));
        let bin = dirs::executable_dir().unwrap_or_else(|| home.join(".local/bin"));

        let data = data_home.join("udnf");
        let cache = cache_home.join("udnf");
        let config = config_home.join("udnf");
        Ok(Self {
            root: data.join("root"),
            state_file: data.join("state.json"),
            lock_file: data.join("lock"),
            persist: data.join("persist"),
            keyring: data.join("pubkeys"),
            packages: cache.join("packages"),
            user_config: config.join("udnf.conf"),
            repos_d: config.join("repos.d"),
            repos_override_d: config.join("repos.override.d"),
            vars: config.join("vars"),
            logs: state_home.join("udnf"),
            bin,
            data_home,
            cache,
        })
    }
}
