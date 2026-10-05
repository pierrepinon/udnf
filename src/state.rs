//! Persistent state: the world set and what udnf installed, in `~/.local/share/udnf/state.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// Specs explicitly requested by the user (`udnf install <spec>`).
    pub world: BTreeSet<String>,
    /// Installed packages keyed by `name.arch`.
    pub packages: BTreeMap<String, InstalledPackage>,
    /// Files udnf created outside of the root (wrappers, desktop entries...): path -> kind.
    pub exports: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstalledPackage {
    pub nevra: String,
    pub name: String,
    pub arch: String,
    pub evr: String,
    pub repo_id: String,
    pub files: Vec<InstalledFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InstalledFile {
    /// Path as packaged (`/usr/bin/foo`).
    pub path: String,
    /// `file`, `dir`, `symlink` or `other`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    /// Original symlink target as packaged.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
}

pub fn key(name: &str, arch: &str) -> String {
    format!("{name}.{arch}")
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self { version: STATE_VERSION, ..Default::default() });
        }
        let data = fs::read(path).with_context(|| format!("lecture de {}", path.display()))?;
        let state: State =
            serde_json::from_slice(&data).with_context(|| format!("état corrompu: {}", path.display()))?;
        Ok(state)
    }

    /// Atomic write (temp file + rename): a crash never leaves a truncated state.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = File::create(&tmp)?;
            serde_json::to_writer_pretty(&mut f, self)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Packaged path -> keys of the packages owning it.
    pub fn owners(&self) -> BTreeMap<&str, Vec<&str>> {
        let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (k, pkg) in &self.packages {
            for f in &pkg.files {
                map.entry(f.path.as_str()).or_default().push(k.as_str());
            }
        }
        map
    }
}

/// Exclusive lock held for the whole run of a mutating command.
pub struct Lock {
    _file: File,
}

impl Lock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = File::create(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => {
                anyhow::bail!("une autre instance de udnf est en cours ({})", path.display())
            }
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = State::load(&path).unwrap();
        assert_eq!(s.version, STATE_VERSION);
        s.world.insert("htop".into());
        s.packages.insert(
            key("htop", "x86_64"),
            InstalledPackage {
                nevra: "htop-3.4.1-3.fc44.x86_64".into(),
                name: "htop".into(),
                arch: "x86_64".into(),
                evr: "3.4.1-3.fc44".into(),
                repo_id: "fedora".into(),
                files: vec![InstalledFile {
                    path: "/usr/bin/htop".into(),
                    kind: "file".into(),
                    digest: "abc".into(),
                    ..Default::default()
                }],
            },
        );
        s.save(&path).unwrap();
        let back = State::load(&path).unwrap();
        assert_eq!(back.world, s.world);
        assert_eq!(back.packages, s.packages);
        assert_eq!(back.owners()["/usr/bin/htop"], vec!["htop.x86_64"]);
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let first = Lock::acquire(&path).unwrap();
        assert!(Lock::acquire(&path).is_err());
        drop(first);
        assert!(Lock::acquire(&path).is_ok());
    }
}
