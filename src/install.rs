//! Relocated installation: `/P` from a package lands at `root/P`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::ffi::bridge::{FileEntry, RpmContents};
use crate::json::{self, Kind};
use crate::plan::Target;
use crate::state::{InstalledFile, InstalledPackage, State};

/// `root` + packaged absolute path.
pub fn rooted(root: &Path, packaged: &str) -> PathBuf {
    root.join(packaged.trim_start_matches('/'))
}

fn installed_files(contents: &RpmContents) -> Vec<InstalledFile> {
    contents
        .files
        .iter()
        .filter(|f| !f.ghost && f.kind != "other")
        .map(|f| InstalledFile {
            path: f.path.clone(),
            kind: f.kind.clone(),
            digest: if f.kind == "file" { f.digest.clone() } else { String::new() },
            target: f.symlink_target.clone(),
        })
        .collect()
}

fn same_content(a: &InstalledFile, b: &FileEntry) -> bool {
    match (a.kind.as_str(), b.kind.as_str()) {
        ("dir", "dir") => true,
        ("symlink", "symlink") => a.target == b.symlink_target,
        ("file", "file") => !a.digest.is_empty() && a.digest == b.digest,
        _ => false,
    }
}

/// File conflicts between incoming packages and the ones that stay installed.
/// Identical files (same digest / same link target) and directories may be shared.
pub fn check_conflicts(state: &State, leaving: &BTreeSet<String>, incoming: &[(String, RpmContents)]) -> Result<()> {
    let mut owners: BTreeMap<&str, (&str, InstalledFile)> = BTreeMap::new();
    for (key, pkg) in &state.packages {
        if leaving.contains(key) {
            continue;
        }
        for f in &pkg.files {
            owners.insert(f.path.as_str(), (pkg.nevra.as_str(), f.clone()));
        }
    }
    let mut conflicts = Vec::new();
    for (_, contents) in incoming {
        for f in contents.files.iter().filter(|f| !f.ghost && f.kind != "other") {
            match owners.get(f.path.as_str()) {
                Some((owner, existing)) if !same_content(existing, f) => {
                    conflicts.push(format!("{} (de {}) entre en conflit avec {}", f.path, contents.nevra, owner));
                }
                Some(_) => {}
                None => {
                    let entry = InstalledFile {
                        path: f.path.clone(),
                        kind: f.kind.clone(),
                        digest: f.digest.clone(),
                        target: f.symlink_target.clone(),
                    };
                    owners.insert(f.path.as_str(), (contents.nevra.as_str(), entry));
                }
            }
        }
    }
    if !conflicts.is_empty() {
        return Err(json::error(Kind::Conflict, format!("conflits de fichiers:\n  {}", conflicts.join("\n  "))));
    }
    Ok(())
}

/// Paths whose type changes between what is on disk and an incoming package (directory ↔ file
/// or symlink). Extraction cannot replace them in place: rpm relies on `%pretrans` for this,
/// udnf removes the old entry itself, after these checks and before unpacking. Fails when the
/// old directory still holds files of packages that stay installed.
pub fn type_changes(
    root: &Path,
    state: &State,
    leaving: &BTreeSet<String>,
    incoming: &[(String, RpmContents)],
) -> Result<Vec<PathBuf>> {
    let staying: Vec<(&str, &str)> = state
        .packages
        .iter()
        .filter(|(k, _)| !leaving.contains(*k))
        .flat_map(|(_, p)| p.files.iter().map(move |f| (f.path.as_str(), p.nevra.as_str())))
        .collect();
    let mut out = Vec::new();
    for (_, contents) in incoming {
        for f in contents.files.iter().filter(|f| !f.ghost && f.kind != "other") {
            let path = rooted(root, &f.path);
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() == (f.kind == "dir") {
                continue;
            }
            if meta.is_dir() {
                let prefix = format!("{}/", f.path);
                if let Some((p, owner)) = staying.iter().find(|(p, _)| p.starts_with(&prefix)) {
                    return Err(json::error(
                        Kind::Conflict,
                        format!(
                            "{} (de {}) remplace un répertoire qui contient {p} de {owner}",
                            f.path, contents.nevra
                        ),
                    ));
                }
            }
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Removes the entries found by [`type_changes`].
pub fn clear_type_changes(paths: &[PathBuf]) -> Result<()> {
    for p in paths {
        let removed =
            if fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) { fs::remove_dir_all(p) } else { fs::remove_file(p) };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("suppression de {}", p.display())),
        }
    }
    Ok(())
}

/// Scriptlets ignored rather than refused. `%pretrans` runs before any file is written; in
/// practice it migrates an older system layout (e.g. a symlink replaced by a directory), which a
/// udnf root never has.
const IGNORED_SCRIPTLETS: [&str; 1] = ["%pretrans"];

/// Packages relying on scriptlets (`%post`, triggers...) are refused: running them needs root
/// and emulating them is guesswork. The whole transaction is rejected before any change.
pub fn check_no_scriptlets(incoming: &[(String, RpmContents)]) -> Result<()> {
    let offenders: Vec<String> = incoming
        .iter()
        .filter_map(|(_, c)| {
            let blocking: Vec<&str> =
                c.scriptlets.iter().map(String::as_str).filter(|s| !IGNORED_SCRIPTLETS.contains(s)).collect();
            (!blocking.is_empty()).then(|| format!("{} ({})", c.nevra, blocking.join(", ")))
        })
        .collect();
    if !offenders.is_empty() {
        return Err(json::error(
            Kind::Scriptlets,
            format!(
                "udnf n'installe pas les paquets qui utilisent des scriptlets; rien n'a été modifié:\n  {}",
                offenders.join("\n  ")
            ),
        ));
    }
    Ok(())
}

/// Whether reaching `path` goes through a symlink below `root` (e.g. an old `dir/file` once `dir`
/// became a symlink): such a path names another package's file, never ours to delete.
fn through_symlink(root: &Path, path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .take_while(|a| a.starts_with(root) && *a != root)
        .any(|a| fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
}

/// Removes `files` from `root` unless another package (other than `except`) still owns them.
fn remove_files(root: &Path, state: &State, except: &str, files: &[InstalledFile]) -> Result<()> {
    let owners = state.owners();
    let still_owned = |path: &str| owners.get(path).is_some_and(|keys| keys.iter().any(|k| *k != except));
    let mut dirs = Vec::new();
    for f in files {
        if still_owned(&f.path) {
            continue;
        }
        let p = rooted(root, &f.path);
        if through_symlink(root, &p) {
            continue;
        }
        if f.kind == "dir" {
            dirs.push(p);
            continue;
        }
        match fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("suppression de {}", p.display())),
        }
    }
    // Deepest first; non-empty directories (user files, other packages) are kept.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = fs::remove_dir(&d);
    }
    Ok(())
}

/// Records a freshly extracted package and drops the files its previous version had alone.
pub fn commit_package(root: &Path, state: &mut State, target: &Target, contents: &RpmContents) -> Result<()> {
    let files = installed_files(contents);
    if let Some(old) = state.packages.get(&target.key).cloned() {
        let new_paths: BTreeSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let stale: Vec<InstalledFile> =
            old.files.into_iter().filter(|f| !new_paths.contains(f.path.as_str())).collect();
        remove_files(root, state, &target.key, &stale)?;
    }
    state.packages.insert(
        target.key.clone(),
        InstalledPackage {
            nevra: target.nevra.clone(),
            name: target.name.clone(),
            arch: target.arch.clone(),
            evr: target.evr.clone(),
            repo_id: target.repo_id.clone(),
            files,
        },
    );
    Ok(())
}

pub fn remove_package(root: &Path, state: &mut State, key: &str) -> Result<()> {
    if let Some(pkg) = state.packages.get(key).cloned() {
        remove_files(root, state, key, &pkg.files)?;
        state.packages.remove(key);
    }
    Ok(())
}

/// Where an installed symlink must point: absolute targets that exist in the root are
/// redirected into it (`/usr/bin/code -> /usr/share/code/bin/code`), others keep pointing
/// at the system.
pub fn symlink_destination(root: &Path, original: &str) -> PathBuf {
    if original.starts_with('/') {
        let inside = rooted(root, original);
        if fs::symlink_metadata(&inside).is_ok() {
            return inside;
        }
    }
    PathBuf::from(original)
}

/// Re-points every absolute symlink of every installed package. Run after each transaction:
/// a target can appear (new package) or disappear (now provided by the system).
pub fn fix_symlinks(root: &Path, state: &State) -> Result<()> {
    for pkg in state.packages.values() {
        for f in pkg.files.iter().filter(|f| f.kind == "symlink" && f.target.starts_with('/')) {
            let link = rooted(root, &f.path);
            let Ok(current) = fs::read_link(&link) else { continue };
            let wanted = symlink_destination(root, &f.target);
            if current == wanted {
                continue;
            }
            let tmp = link.with_file_name(format!(
                ".udnf-tmp.{}",
                link.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            ));
            let _ = fs::remove_file(&tmp);
            symlink(&wanted, &tmp).with_context(|| format!("symlink {}", tmp.display()))?;
            fs::rename(&tmp, &link).with_context(|| format!("rename {}", link.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: &str, digest: &str, target: &str) -> FileEntry {
        FileEntry {
            path: path.into(),
            mode: 0o644,
            kind: kind.into(),
            symlink_target: target.into(),
            digest: digest.into(),
            size: 0,
            config: false,
            ghost: false,
            doc: false,
        }
    }

    fn contents(nevra: &str, files: Vec<FileEntry>) -> RpmContents {
        RpmContents { nevra: nevra.into(), files, scriptlets: vec![] }
    }

    fn target(name: &str) -> Target {
        Target {
            key: format!("{name}.x86_64"),
            nevra: format!("{name}-1-1.x86_64"),
            name: name.into(),
            arch: "x86_64".into(),
            evr: "1-1".into(),
            repo_id: "test".into(),
            download_size: 0,
            install_size: 0,
        }
    }

    #[test]
    fn conflicts_detected_between_and_within_transactions() {
        let mut state = State::default();
        let a = contents("a-1-1.x86_64", vec![entry("/usr", "dir", "", ""), entry("/usr/bin/x", "file", "aa", "")]);
        let root = tempfile::tempdir().unwrap();
        commit_package(root.path(), &mut state, &target("a"), &a).unwrap();

        // Same digest and shared directory: fine.
        let same = contents("b-1-1.x86_64", vec![entry("/usr", "dir", "", ""), entry("/usr/bin/x", "file", "aa", "")]);
        check_conflicts(&state, &BTreeSet::new(), &[("b.x86_64".into(), same)]).unwrap();

        // Different digest: conflict, unless the owner is leaving.
        let diff = contents("c-1-1.x86_64", vec![entry("/usr/bin/x", "file", "bb", "")]);
        let err = check_conflicts(&state, &BTreeSet::new(), &[("c.x86_64".into(), diff)]).unwrap_err();
        assert!(err.to_string().contains("/usr/bin/x"));
        let diff = contents("c-1-1.x86_64", vec![entry("/usr/bin/x", "file", "bb", "")]);
        let leaving = BTreeSet::from(["a.x86_64".to_string()]);
        check_conflicts(&state, &leaving, &[("c.x86_64".into(), diff)]).unwrap();

        // Two incoming packages disagreeing.
        let d = contents("d-1-1.x86_64", vec![entry("/opt/y", "symlink", "", "/a")]);
        let e = contents("e-1-1.x86_64", vec![entry("/opt/y", "symlink", "", "/b")]);
        assert!(check_conflicts(&state, &BTreeSet::new(), &[("d".into(), d), ("e".into(), e)]).is_err());
    }

    #[test]
    fn packages_with_scriptlets_are_refused() {
        let clean = contents("clean-1-1.x86_64", vec![]);
        let mut post = contents("withpost-1-1.x86_64", vec![]);
        post.scriptlets = vec!["%post".into(), "%postun".into()];
        check_no_scriptlets(&[("clean".into(), clean)]).unwrap();
        let clean = contents("clean-1-1.x86_64", vec![]);
        let err = check_no_scriptlets(&[("clean".into(), clean), ("withpost".into(), post)]).unwrap_err().to_string();
        assert!(err.contains("withpost-1-1.x86_64 (%post, %postun)"), "{err}");
        assert!(!err.contains("clean-1-1"), "{err}");

        // %pretrans alone is ignored; listed only alongside a blocking scriptlet, it is not shown.
        let mut pretrans = contents("node-1-1.x86_64", vec![]);
        pretrans.scriptlets = vec!["%pretrans".into()];
        check_no_scriptlets(&[("node".into(), pretrans)]).unwrap();
        let mut java = contents("java-1-1.x86_64", vec![]);
        java.scriptlets = vec!["%pretrans".into(), "%post".into()];
        let err = check_no_scriptlets(&[("java".into(), java)]).unwrap_err().to_string();
        assert!(err.contains("java-1-1.x86_64 (%post)"), "{err}");
    }

    #[test]
    fn directory_replaced_by_symlink_on_update() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir_all(r.join("usr/share/app/compat")).unwrap();
        fs::write(r.join("usr/share/app/compat/data"), "old").unwrap();
        let mut state = State::default();
        let v1 = contents(
            "app-1-1.x86_64",
            vec![entry("/usr/share/app/compat", "dir", "", ""), entry("/usr/share/app/compat/data", "file", "1", "")],
        );
        commit_package(r, &mut state, &target("app"), &v1).unwrap();
        let v2 = || contents("app-2-1.x86_64", vec![entry("/usr/share/app/compat", "symlink", "", "new")]);
        let leaving = BTreeSet::from(["app.x86_64".to_string()]);

        let paths = type_changes(r, &state, &leaving, &[("app.x86_64".into(), v2())]).unwrap();
        assert_eq!(paths, vec![r.join("usr/share/app/compat")]);
        clear_type_changes(&paths).unwrap();
        assert!(!r.join("usr/share/app/compat").exists());

        // After unpacking v2 (compat -> new), dropping v1's stale compat/data must not follow
        // the symlink and delete new/data.
        fs::create_dir_all(r.join("usr/share/app/new")).unwrap();
        fs::write(r.join("usr/share/app/new/data"), "new").unwrap();
        symlink("new", r.join("usr/share/app/compat")).unwrap();
        let mut updated = state.clone();
        commit_package(r, &mut updated, &target("app"), &v2()).unwrap();
        assert_eq!(fs::read_to_string(r.join("usr/share/app/new/data")).unwrap(), "new");
        fs::remove_file(r.join("usr/share/app/compat")).unwrap();

        // Another installed package still has a file in the directory: refused before any change.
        fs::create_dir_all(r.join("usr/share/app/compat")).unwrap();
        let other = contents("other-1-1.x86_64", vec![entry("/usr/share/app/compat/extra", "file", "2", "")]);
        commit_package(r, &mut state, &target("other"), &other).unwrap();
        let err = type_changes(r, &state, &leaving, &[("app.x86_64".into(), v2())]).unwrap_err();
        assert_eq!(json::kind_of(&err), Some(Kind::Conflict));
        assert!(err.to_string().contains("other-1-1.x86_64"), "{err}");
    }

    #[test]
    fn update_and_remove_clean_up_files() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let mut state = State::default();
        for p in ["usr/bin/tool", "usr/share/tool/old.dat", "usr/share/tool/keep.dat"] {
            fs::create_dir_all(r.join(p).parent().unwrap()).unwrap();
            fs::write(r.join(p), "x").unwrap();
        }
        let v1 = contents(
            "tool-1-1.x86_64",
            vec![
                entry("/usr/bin/tool", "file", "1", ""),
                entry("/usr/share/tool", "dir", "", ""),
                entry("/usr/share/tool/old.dat", "file", "2", ""),
                entry("/usr/share/tool/keep.dat", "file", "3", ""),
            ],
        );
        commit_package(r, &mut state, &target("tool"), &v1).unwrap();

        let v2 = contents(
            "tool-2-1.x86_64",
            vec![
                entry("/usr/bin/tool", "file", "1", ""),
                entry("/usr/share/tool", "dir", "", ""),
                entry("/usr/share/tool/keep.dat", "file", "3", ""),
            ],
        );
        commit_package(r, &mut state, &target("tool"), &v2).unwrap();
        assert!(!r.join("usr/share/tool/old.dat").exists());
        assert!(r.join("usr/share/tool/keep.dat").exists());

        remove_package(r, &mut state, "tool.x86_64").unwrap();
        assert!(!r.join("usr/bin/tool").exists());
        assert!(!r.join("usr/share/tool").exists());
        assert!(state.packages.is_empty());
    }

    #[test]
    fn shared_files_survive_removal_of_one_owner() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir_all(r.join("usr/share/licenses")).unwrap();
        fs::write(r.join("usr/share/licenses/COMMON"), "x").unwrap();
        let mut state = State::default();
        let f = || vec![entry("/usr/share/licenses/COMMON", "file", "c", "")];
        commit_package(r, &mut state, &target("a"), &contents("a", f())).unwrap();
        commit_package(r, &mut state, &target("b"), &contents("b", f())).unwrap();
        remove_package(r, &mut state, "a.x86_64").unwrap();
        assert!(r.join("usr/share/licenses/COMMON").exists());
        remove_package(r, &mut state, "b.x86_64").unwrap();
        assert!(!r.join("usr/share/licenses/COMMON").exists());
    }

    #[test]
    fn absolute_symlinks_follow_ownership() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir_all(r.join("usr/bin")).unwrap();
        fs::create_dir_all(r.join("usr/share/code/bin")).unwrap();
        fs::write(r.join("usr/share/code/bin/code"), "#!/bin/sh\n").unwrap();
        symlink("/usr/share/code/bin/code", r.join("usr/bin/code")).unwrap();
        symlink("/usr/bin/bash", r.join("usr/bin/mysh")).unwrap();

        let mut state = State::default();
        let pkg = contents(
            "code-1-1.x86_64",
            vec![
                entry("/usr/bin/code", "symlink", "", "/usr/share/code/bin/code"),
                entry("/usr/bin/mysh", "symlink", "", "/usr/bin/bash"),
                entry("/usr/share/code/bin/code", "file", "z", ""),
            ],
        );
        commit_package(r, &mut state, &target("code"), &pkg).unwrap();
        fix_symlinks(r, &state).unwrap();
        assert_eq!(fs::read_link(r.join("usr/bin/code")).unwrap(), r.join("usr/share/code/bin/code"));
        // Target provided by the system: untouched.
        assert_eq!(fs::read_link(r.join("usr/bin/mysh")).unwrap(), PathBuf::from("/usr/bin/bash"));

        // Target disappears from the root: link goes back to the system path.
        fs::remove_file(r.join("usr/share/code/bin/code")).unwrap();
        fix_symlinks(r, &state).unwrap();
        assert_eq!(fs::read_link(r.join("usr/bin/code")).unwrap(), PathBuf::from("/usr/share/code/bin/code"));
    }
}
