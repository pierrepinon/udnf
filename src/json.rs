//! Machine interface (`--json`): one JSON value per line on stdout, used by the gnome-software
//! plugin. Human messages are printed only when it is off.

use std::fmt;
use std::io::Write;

use serde::Serialize;

/// Classified failures, reported as `{"event":"error","kind":...}`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The solver found no solution (unknown package, missing dependency...).
    Resolve,
    /// The transaction would touch system packages.
    System,
    /// A package uses scriptlets.
    Scriptlets,
    /// A remote repo key would have to be imported.
    Key,
    Signature,
    Conflict,
}

#[derive(Debug)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
}

/// A classified error, ready to be returned.
pub fn error(kind: Kind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Error { kind, message: message.into() })
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Kind of the first classified error in the chain, if any.
pub fn kind_of(err: &anyhow::Error) -> Option<Kind> {
    err.chain().find_map(|e| e.downcast_ref::<Error>()).map(|e| e.kind)
}

#[derive(Serialize)]
pub struct PlanPackage {
    pub name: String,
    pub arch: String,
    pub evr: String,
    pub nevra: String,
    pub repo_id: String,
    pub download_size: u64,
    pub install_size: u64,
}

#[derive(Serialize)]
pub struct PlanUpdate {
    pub name: String,
    pub arch: String,
    pub from: String,
    pub to: String,
    pub repo_id: String,
    pub download_size: u64,
}

#[derive(Serialize)]
pub struct PlanRemove {
    pub name: String,
    pub arch: String,
    pub nevra: String,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    Plan {
        install: Vec<PlanPackage>,
        update: Vec<PlanUpdate>,
        remove: Vec<PlanRemove>,
        /// Requested specs already provided by the system.
        system: Vec<String>,
    },
    Download {
        packages: usize,
        bytes: u64,
    },
    Unpack {
        nevra: String,
        index: usize,
        total: usize,
    },
    Done,
    Error {
        kind: Option<Kind>,
        message: String,
    },
}

/// An installed package, for `udnf status --json`.
#[derive(Serialize)]
pub struct Installed {
    pub name: String,
    pub arch: String,
    pub evr: String,
    pub nevra: String,
    pub repo_id: String,
    /// Explicitly requested (in the world set), as opposed to pulled as a dependency.
    pub requested: bool,
    /// Desktop entry ids shipped by the package (`org.gnome.Meld.desktop`).
    pub desktop_ids: Vec<String>,
}

/// An installable package, for `udnf available --json`.
#[derive(Serialize)]
pub struct Available {
    pub name: String,
    pub arch: String,
    pub evr: String,
    pub repo_id: String,
    pub summary: String,
    pub download_size: u64,
    pub install_size: u64,
}

pub fn line<T: Serialize>(value: &T) {
    let mut out = std::io::stdout().lock();
    // A closed pipe (consumer gone) is not worth a panic.
    let _ = serde_json::to_writer(&mut out, value);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Prints either human messages or JSON events.
#[derive(Clone, Copy)]
pub struct Reporter {
    pub json: bool,
}

impl Reporter {
    pub fn say(&self, text: impl AsRef<str>) {
        if !self.json {
            println!("{}", text.as_ref());
        }
    }

    pub fn event(&self, event: &Event) {
        if self.json {
            line(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_with_a_tag() {
        let e = Event::Unpack { nevra: "a-1-1.x86_64".into(), index: 1, total: 2 };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"event":"unpack","nevra":"a-1-1.x86_64","index":1,"total":2}"#
        );
        let e = Event::Error { kind: Some(Kind::Scriptlets), message: "x".into() };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"event":"error","kind":"scriptlets","message":"x"}"#);
        assert_eq!(serde_json::to_string(&Event::Done).unwrap(), r#"{"event":"done"}"#);
    }

    #[test]
    fn kind_is_found_through_context() {
        let err = error(Kind::Conflict, "boom").context("extraction");
        assert_eq!(kind_of(&err), Some(Kind::Conflict));
        assert_eq!(kind_of(&anyhow::anyhow!("plain")), None);
        assert_eq!(format!("{:#}", error(Kind::Key, "clé")), "clé");
    }
}
