//! Turns a libdnf5 resolution of the world set into a plan against what udnf installed.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::Result;

use crate::ffi::bridge::{Resolution, ResolvedPackage};
use crate::json::{self, Kind};
use crate::state::{self, State};

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub key: String,
    pub nevra: String,
    pub name: String,
    pub arch: String,
    pub evr: String,
    pub repo_id: String,
    pub download_size: u64,
    pub install_size: u64,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub install: Vec<Target>,
    /// (installed NEVRA, new target)
    pub update: Vec<(String, Target)>,
    /// Keys of installed packages to remove.
    pub remove: Vec<String>,
    /// World specs already satisfied by the system.
    pub system_specs: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.install.is_empty() && self.update.is_empty() && self.remove.is_empty()
    }

    /// Packages to download and unpack, in resolution order.
    pub fn to_fetch(&self) -> Vec<&Target> {
        self.install.iter().chain(self.update.iter().map(|(_, t)| t)).collect()
    }
}

fn is_reason_change(action: &str) -> bool {
    action.eq_ignore_ascii_case("reason change")
}

/// Builds the plan. Fails when the solver touches a system package: system packages are frozen.
pub fn build(resolution: &Resolution, state: &State) -> Result<Plan> {
    if !resolution.ok {
        let mut msg = String::from("la résolution des dépendances a échoué:");
        for p in &resolution.problems {
            let _ = write!(msg, "\n  {p}");
        }
        return Err(json::error(Kind::Resolve, msg));
    }

    let mut touched_system = Vec::new();
    let mut desired: BTreeMap<String, &ResolvedPackage> = BTreeMap::new();
    for p in &resolution.packages {
        if is_reason_change(&p.action) {
            continue;
        }
        if p.system || !p.action.eq_ignore_ascii_case("install") {
            touched_system.push(format!("{} {}", p.action, p.nevra));
            continue;
        }
        desired.insert(state::key(&p.name, &p.arch), p);
    }
    if !touched_system.is_empty() {
        let mut msg = String::from(
            "cette opération modifierait des paquets système, ce que udnf refuse \
             (mettez à jour le système avec rpm-ostree, ou choisissez une autre version):",
        );
        for t in &touched_system {
            let _ = write!(msg, "\n  {t}");
        }
        return Err(json::error(Kind::System, msg));
    }

    let mut plan = Plan { system_specs: resolution.system_specs.clone(), ..Default::default() };
    for (key, p) in &desired {
        let target = Target {
            key: key.clone(),
            nevra: p.nevra.clone(),
            name: p.name.clone(),
            arch: p.arch.clone(),
            evr: p.evr.clone(),
            repo_id: p.repo_id.clone(),
            download_size: p.download_size,
            install_size: p.install_size,
        };
        match state.packages.get(key) {
            None => plan.install.push(target),
            Some(installed) if installed.nevra != p.nevra => plan.update.push((installed.nevra.clone(), target)),
            Some(_) => {}
        }
    }
    for key in state.packages.keys() {
        if !desired.contains_key(key) {
            plan.remove.push(key.clone());
        }
    }
    Ok(plan)
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Kio", "Mio", "Gio", "Tio"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} o") } else { format!("{value:.1} {}", UNITS[unit]) }
}

pub fn render(plan: &Plan, state: &State) -> String {
    let mut out = String::new();
    for spec in &plan.system_specs {
        let _ = writeln!(out, "Déjà fourni par le système: {spec}");
    }
    if !plan.install.is_empty() {
        let _ = writeln!(out, "Installation:");
        for t in &plan.install {
            let _ = writeln!(out, "  + {:<50} {:<20} {:>10}", t.nevra, t.repo_id, human_size(t.download_size));
        }
    }
    if !plan.update.is_empty() {
        let _ = writeln!(out, "Mise à jour:");
        for (from, t) in &plan.update {
            let _ = writeln!(out, "  ~ {from} -> {}  ({}, {})", t.evr, t.repo_id, human_size(t.download_size));
        }
    }
    if !plan.remove.is_empty() {
        let _ = writeln!(out, "Suppression:");
        for k in &plan.remove {
            let _ = writeln!(out, "  - {}", state.packages.get(k).map_or(k.as_str(), |p| p.nevra.as_str()));
        }
    }
    let fetch = plan.to_fetch();
    if !fetch.is_empty() {
        let dl: u64 = fetch.iter().map(|t| t.download_size).sum();
        let inst: u64 = fetch.iter().map(|t| t.install_size).sum();
        let _ = writeln!(out, "Téléchargement: {}, espace disque: {}", human_size(dl), human_size(inst));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InstalledPackage;

    fn rp(name: &str, evr: &str, action: &str, system: bool) -> ResolvedPackage {
        ResolvedPackage {
            nevra: format!("{name}-{evr}.x86_64"),
            name: name.into(),
            arch: "x86_64".into(),
            evr: evr.into(),
            repo_id: if system { "@System".into() } else { "fedora".into() },
            download_size: 1000,
            install_size: 4000,
            action: action.into(),
            reason: "Dependency".into(),
            system,
        }
    }

    fn installed(name: &str, evr: &str) -> InstalledPackage {
        InstalledPackage {
            nevra: format!("{name}-{evr}.x86_64"),
            name: name.into(),
            arch: "x86_64".into(),
            evr: evr.into(),
            repo_id: "fedora".into(),
            files: vec![],
        }
    }

    fn resolution(packages: Vec<ResolvedPackage>) -> Resolution {
        Resolution { ok: true, problems: vec![], packages, system_specs: vec![] }
    }

    #[test]
    fn diff_install_update_remove() {
        let mut state = State::default();
        state.packages.insert("keep.x86_64".into(), installed("keep", "1-1"));
        state.packages.insert("old.x86_64".into(), installed("old", "1-1"));
        state.packages.insert("gone.x86_64".into(), installed("gone", "1-1"));
        let res = resolution(vec![
            rp("keep", "1-1", "Install", false),
            rp("old", "2-1", "Install", false),
            rp("new", "1-1", "Install", false),
        ]);
        let plan = build(&res, &state).unwrap();
        assert_eq!(plan.install.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["new"]);
        assert_eq!(plan.update.len(), 1);
        assert_eq!(plan.update[0].0, "old-1-1.x86_64");
        assert_eq!(plan.update[0].1.evr, "2-1");
        assert_eq!(plan.remove, ["gone.x86_64"]);
    }

    #[test]
    fn empty_world_removes_everything() {
        let mut state = State::default();
        state.packages.insert("a.x86_64".into(), installed("a", "1-1"));
        let plan = build(&resolution(vec![]), &state).unwrap();
        assert_eq!(plan.remove, ["a.x86_64"]);
        assert!(plan.install.is_empty());
    }

    #[test]
    fn refuses_to_touch_system_packages() {
        let res = resolution(vec![
            rp("app", "1-1", "Install", false),
            rp("glibc", "2.44-1", "Upgrade", false),
            rp("glibc", "2.43-1", "Replaced", true),
        ]);
        let err = build(&res, &State::default()).unwrap_err().to_string();
        assert!(err.contains("Upgrade glibc-2.44-1.x86_64"), "{err}");
        assert!(err.contains("Replaced glibc-2.43-1.x86_64"), "{err}");
    }

    #[test]
    fn reason_change_is_ignored() {
        let res = resolution(vec![rp("htop", "3-1", "Reason Change", true)]);
        let plan = build(&res, &State::default()).unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn solver_failure_is_reported() {
        let res = Resolution {
            ok: false,
            problems: vec!["No match for argument: nope".into()],
            packages: vec![],
            system_specs: vec![],
        };
        let err = build(&res, &State::default()).unwrap_err().to_string();
        assert!(err.contains("No match for argument: nope"));
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 o");
        assert_eq!(human_size(1536), "1.5 Kio");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 Mio");
    }
}
