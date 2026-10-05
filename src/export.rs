//! Makes installed packages usable without any user action: wrappers in `~/.local/bin`,
//! desktop entries, D-Bus services, icons, fonts and man pages under `~/.local/share`.
//! Library and data paths are set per wrapper, never globally.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::install::rooted;
use crate::state::State;

pub const MARKER: &str = "# managed-by: udnf";

const BIN_DIRS: [&str; 4] = ["/usr/bin/", "/usr/sbin/", "/bin/", "/sbin/"];

/// (packaged prefix, destination under XDG_DATA_HOME), exported as symlinks.
const MIRRORS: [(&str, &str); 4] = [
    ("/usr/share/icons/", "icons"),
    ("/usr/share/pixmaps/", "icons"),
    ("/usr/share/fonts/", "fonts/udnf"),
    ("/usr/share/man/", "man"),
];

pub struct ExportDirs {
    pub root: PathBuf,
    pub bin: PathBuf,
    /// XDG_DATA_HOME (`~/.local/share`)
    pub data_home: PathBuf,
}

impl ExportDirs {
    fn applications(&self) -> PathBuf {
        self.data_home.join("applications")
    }
    fn dbus_services(&self) -> PathBuf {
        self.data_home.join("dbus-1/services")
    }
    /// Wrappers for programs launched from desktop/D-Bus files outside of the bin dirs.
    fn exec_dir(&self) -> PathBuf {
        self.root.with_file_name("exec")
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Export {
    File { content: String, executable: bool, kind: &'static str },
    Link { target: PathBuf, kind: &'static str },
}

impl Export {
    fn kind(&self) -> &'static str {
        match self {
            Export::File { kind, .. } | Export::Link { kind, .. } => kind,
        }
    }
}

/// Environment a wrapper sets before running a program from the root.
#[derive(Debug, Default, PartialEq)]
pub struct WrapperEnv {
    pub path: Vec<PathBuf>,
    pub lib_dirs: Vec<PathBuf>,
    pub data_dirs: Vec<PathBuf>,
    pub typelib_dirs: Vec<PathBuf>,
    pub python_dirs: Vec<PathBuf>,
    pub qt_plugin_dirs: Vec<PathBuf>,
}

fn has_shared_object(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".so") || name.contains(".so.")
        })
    })
}

/// Library directories, following ldconfig: the standard dirs when they hold shared objects,
/// plus the dirs listed in the packages' `/etc/ld.so.conf.d/*.conf`, rewritten into the root.
pub fn library_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for std_dir in ["usr/lib64", "usr/lib"] {
        let d = root.join(std_dir);
        if has_shared_object(&d) {
            dirs.push(d);
        }
    }
    let conf_d = root.join("etc/ld.so.conf.d");
    let mut confs: Vec<PathBuf> = fs::read_dir(&conf_d)
        .map(|it| it.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "conf")).collect())
        .unwrap_or_default();
    confs.sort();
    for conf in confs {
        let Ok(text) = fs::read_to_string(&conf) else { continue };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with("include") || !line.starts_with('/') {
                continue;
            }
            let d = rooted(root, line);
            if d.is_dir() && !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    dirs
}

fn existing(root: &Path, rels: &[&str]) -> Vec<PathBuf> {
    rels.iter().map(|r| root.join(r)).filter(|p| p.is_dir()).collect()
}

fn python_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for lib in ["usr/lib64", "usr/lib"] {
        let Ok(entries) = fs::read_dir(root.join(lib)) else { continue };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("python3"))
            .map(|e| e.path().join("site-packages"))
            .filter(|p| p.is_dir())
            .collect();
        found.sort();
        out.extend(found);
    }
    out
}

pub fn wrapper_env(root: &Path) -> WrapperEnv {
    WrapperEnv {
        path: existing(root, &["usr/bin", "usr/sbin"]),
        lib_dirs: library_dirs(root),
        data_dirs: existing(root, &["usr/share"]),
        typelib_dirs: existing(root, &["usr/lib64/girepository-1.0", "usr/lib/girepository-1.0"]),
        python_dirs: python_dirs(root),
        qt_plugin_dirs: existing(root, &["usr/lib64/qt6/plugins", "usr/lib64/qt5/plugins"]),
    }
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn join(dirs: &[PathBuf]) -> String {
    dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(":")
}

pub fn wrapper_script(program: &Path, env: &WrapperEnv) -> String {
    let mut s = format!("#!/bin/sh\n{MARKER}\n");
    // Prepend to a variable, keeping what the caller had.
    let mut prepend = |var: &str, dirs: &[PathBuf], default: Option<&str>| {
        if dirs.is_empty() {
            return;
        }
        let value = shell_quote(&join(dirs));
        match default {
            Some(def) => s.push_str(&format!("export {var}={value}\":${{{var}:-{def}}}\"\n")),
            None => s.push_str(&format!("export {var}={value}\"${{{var}:+:${var}}}\"\n")),
        }
    };
    prepend("PATH", &env.path, None);
    prepend("LD_LIBRARY_PATH", &env.lib_dirs, None);
    prepend("XDG_DATA_DIRS", &env.data_dirs, Some("/usr/local/share:/usr/share"));
    prepend("GI_TYPELIB_PATH", &env.typelib_dirs, None);
    prepend("PYTHONPATH", &env.python_dirs, None);
    prepend("QT_PLUGIN_PATH", &env.qt_plugin_dirs, None);
    s.push_str(&format!("exec {} \"$@\"\n", shell_quote(&program.display().to_string())));
    s
}

/// Splits a desktop-entry Exec value into tokens, keeping quotes as written.
fn split_exec(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quoted => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Rewrites the program of an Exec/TryExec value; `resolve` maps a program to its replacement.
pub fn rewrite_exec(value: &str, resolve: &dyn Fn(&str) -> Option<String>) -> String {
    let mut tokens = split_exec(value);
    let mut i = 0;
    if tokens.first().is_some_and(|t| t == "env" || t == "/usr/bin/env") {
        i = 1;
        while i < tokens.len() && tokens[i].contains('=') && !tokens[i].starts_with('"') {
            i += 1;
        }
    }
    if let Some(tok) = tokens.get(i) {
        let unquoted = tok.trim_matches('"');
        if let Some(new) = resolve(unquoted) {
            tokens[i] = if new.contains(char::is_whitespace) { format!("\"{new}\"") } else { new };
        }
    }
    tokens.join(" ")
}

/// Wrapper for a program of the root launched by absolute path. It mirrors the packaged path
/// (`/usr/bin/node-24` -> `exec/usr/bin/node-24`), so the original path can always be read back
/// from the wrapper path: no state needed to undo a rewrite.
fn exec_wrapper(exec_dir: &Path, packaged: &str) -> PathBuf {
    exec_dir.join(packaged.trim_start_matches('/'))
}

/// Inverse of [`exec_wrapper`] applied to a shebang line: `<exec_dir>/usr/bin/node-24 -x`
/// gives back `/usr/bin/node-24 -x`; any other line is already the original.
fn original_shebang(line: &str, exec_dir: &Path) -> String {
    let prefix = exec_dir.display().to_string();
    match line.strip_prefix(prefix.as_str()) {
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        _ => line.to_string(),
    }
}

/// Rewrites a desktop entry or D-Bus service file so programs of the root always start
/// through a wrapper (and thus with their environment). Programs outside the bin dirs get a
/// wrapper in `exec_dir`, recorded in `extra` (wrapper -> program).
pub fn rewrite_entry(
    content: &str,
    root: &Path,
    wrappers: &BTreeMap<String, PathBuf>,
    exec_dir: &Path,
    extra: &RefCell<BTreeMap<PathBuf, PathBuf>>,
) -> String {
    let resolve = |prog: &str| -> Option<String> {
        if prog.starts_with('/') {
            if BIN_DIRS.iter().any(|d| prog.starts_with(d)) {
                let name = Path::new(prog).file_name()?.to_string_lossy().to_string();
                if let Some(w) = wrappers.get(&name) {
                    return Some(w.display().to_string());
                }
            }
            let inside = rooted(root, prog);
            if !is_executable(&inside) {
                return None;
            }
            let wrapper = exec_wrapper(exec_dir, prog);
            extra.borrow_mut().insert(wrapper.clone(), inside);
            return Some(wrapper.display().to_string());
        }
        wrappers.get(prog).map(|w| w.display().to_string())
    };
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        let rewritten = if let Some(v) = line.strip_prefix("Exec=") {
            format!("Exec={}", rewrite_exec(v, &resolve))
        } else if let Some(v) = line.strip_prefix("TryExec=") {
            format!("TryExec={}", rewrite_exec(v, &resolve))
        } else if let Some(v) = line.strip_prefix("Icon=").filter(|v| v.starts_with('/')) {
            let inside = rooted(root, v);
            if inside.exists() { format!("Icon={}", inside.display()) } else { line.to_string() }
        } else {
            line.to_string()
        };
        out.push_str(&rewritten);
        out.push('\n');
    }
    out
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Every export the current state calls for, by destination path.
fn desired_exports(dirs: &ExportDirs, state: &State) -> BTreeMap<PathBuf, Export> {
    let root = &dirs.root;
    let env = wrapper_env(root);
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
    let mut out = BTreeMap::new();

    let mut paths: BTreeSet<&str> = BTreeSet::new();
    for pkg in state.packages.values() {
        for f in pkg.files.iter().filter(|f| f.kind == "file" || f.kind == "symlink") {
            paths.insert(f.path.as_str());
        }
    }

    // Wrappers for executables of the root (symlinks back to the system are not wrapped).
    let mut wrappers = BTreeMap::new();
    for p in &paths {
        // Direct children of a bin dir only (`/usr/bin/foo/bar` is not a command).
        let Some(name) = BIN_DIRS.iter().find_map(|d| p.strip_prefix(d)).filter(|n| !n.contains('/')) else {
            continue;
        };
        let name = name.to_string();
        let program = rooted(root, p);
        let inside = fs::canonicalize(&program).is_ok_and(|c| c.starts_with(&canonical_root));
        if !inside || !is_executable(&program) || wrappers.contains_key(&name) {
            continue;
        }
        let dest = dirs.bin.join(&name);
        out.insert(
            dest.clone(),
            Export::File { content: wrapper_script(&program, &env), executable: true, kind: "wrapper" },
        );
        wrappers.insert(name, dest);
    }

    // Desktop entries and D-Bus session services.
    let extra = RefCell::new(BTreeMap::new());
    for (prefix, dest_dir, kind, ext) in [
        ("/usr/share/applications/", dirs.applications(), "desktop", ".desktop"),
        ("/usr/share/dbus-1/services/", dirs.dbus_services(), "dbus", ".service"),
    ] {
        for p in paths.iter().filter(|p| p.starts_with(prefix) && p.ends_with(ext)) {
            let rel = &p[prefix.len()..];
            if rel.contains('/') {
                continue;
            }
            let Ok(content) = fs::read_to_string(rooted(root, p)) else { continue };
            // The marker (a comment, valid in both formats) makes the file recognisable as ours
            // even if state.json never recorded it.
            let content = format!("{MARKER}\n{}", rewrite_entry(&content, root, &wrappers, &dirs.exec_dir(), &extra));
            out.insert(dest_dir.join(rel), Export::File { content, executable: false, kind });
        }
    }
    // Interpreters of the root used in shebangs (`#!/usr/bin/node-24`).
    for (_, original) in scripts(dirs, state) {
        let Some(interp) = original.split_whitespace().next() else { continue };
        if interp.starts_with('/') && is_executable(&rooted(root, interp)) {
            extra.borrow_mut().insert(exec_wrapper(&dirs.exec_dir(), interp), rooted(root, interp));
        }
    }
    for (wrapper, program) in extra.into_inner() {
        out.insert(
            wrapper,
            Export::File { content: wrapper_script(&program, &env), executable: true, kind: "wrapper" },
        );
    }

    // Icons, fonts, man pages: symlinks into the root.
    for (prefix, dest) in MIRRORS {
        for p in paths.iter().filter(|p| p.starts_with(prefix)) {
            let rel = &p[prefix.len()..];
            out.entry(dirs.data_home.join(dest).join(rel))
                .or_insert(Export::Link { target: rooted(root, p), kind: "link" });
        }
    }
    out
}

fn is_ours(path: &Path, export_kind: Option<&str>, root: &Path) -> bool {
    if export_kind.is_some() {
        return true;
    }
    // Not recorded (state lost?): recognise our own artifacts.
    if let Ok(target) = fs::read_link(path) {
        return target.starts_with(root);
    }
    fs::read_to_string(path).is_ok_and(|c| c.lines().take(2).any(|l| l == MARKER))
}

fn write_export(dest: &Path, export: &Export) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp =
        dest.with_file_name(format!(".udnf-tmp.{}", dest.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    let _ = fs::remove_file(&tmp);
    match export {
        Export::File { content, executable, .. } => {
            fs::write(&tmp, content)?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(if *executable { 0o755 } else { 0o644 }))?;
        }
        Export::Link { target, .. } => symlink(target, &tmp)?,
    }
    fs::rename(&tmp, dest).with_context(|| format!("écriture de {}", dest.display()))?;
    Ok(())
}

fn unchanged(dest: &Path, export: &Export) -> bool {
    match export {
        Export::File { content, .. } => fs::read_to_string(dest).is_ok_and(|c| &c == content),
        Export::Link { target, .. } => fs::read_link(dest).is_ok_and(|t| &t == target),
    }
}

/// Removes now-empty directories between `path` and `stop` (exclusive).
fn prune_empty_parents(path: &Path, stop: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == stop || !d.starts_with(stop) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// The `#!` line of a file, without `#!` and the newline.
fn read_shebang(path: &Path) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = File::open(path).ok()?.read(&mut buf).ok()?;
    let line = buf[..n].strip_prefix(b"#!")?;
    let end = line.iter().position(|&b| b == b'\n')?;
    Some(String::from_utf8_lossy(&line[..end]).trim().to_string())
}

/// Executable files of the root starting with `#!`, with their original shebang line.
fn scripts(dirs: &ExportDirs, state: &State) -> Vec<(PathBuf, String)> {
    let exec_dir = dirs.exec_dir();
    state
        .packages
        .values()
        .flat_map(|p| &p.files)
        .filter(|f| f.kind == "file")
        .map(|f| rooted(&dirs.root, &f.path))
        .filter(|path| is_executable(path))
        .filter_map(|path| read_shebang(&path).map(|line| (path, original_shebang(&line, &exec_dir))))
        .collect()
}

/// Points shebangs at interpreters of the root to their wrapper (the interpreter needs its
/// environment, e.g. `node-24` and `libnode.so`); restores the original once the interpreter is
/// gone from the root. Run after `sync`, which creates the wrappers.
pub fn fix_shebangs(dirs: &ExportDirs, state: &State) -> Result<()> {
    for (path, original) in scripts(dirs, state) {
        let (interp, rest) = original.split_once(char::is_whitespace).unwrap_or((original.as_str(), ""));
        let wanted = if interp.starts_with('/') && is_executable(&rooted(&dirs.root, interp)) {
            let wrapper = exec_wrapper(&dirs.exec_dir(), interp).display().to_string();
            if rest.is_empty() { wrapper } else { format!("{wrapper} {rest}") }
        } else {
            original.clone()
        };
        if read_shebang(&path).is_none_or(|current| current == wanted) {
            continue;
        }
        let content = fs::read(&path)?;
        let body = &content[content.iter().position(|&b| b == b'\n').unwrap_or(content.len())..];
        let mut new = format!("#!{wanted}").into_bytes();
        new.extend_from_slice(body);
        let tmp = path
            .with_file_name(format!(".udnf-tmp.{}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
        fs::write(&tmp, new)?;
        fs::set_permissions(&tmp, fs::metadata(&path)?.permissions())?;
        fs::rename(&tmp, &path).with_context(|| format!("réécriture du shebang de {}", path.display()))?;
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct Report {
    pub written: usize,
    pub removed: usize,
    pub skipped: Vec<String>,
    pub changed_kinds: BTreeSet<&'static str>,
    pub fonts_changed: bool,
    pub icons_changed: bool,
}

/// Brings the exports in line with the state; records them in `state.exports`.
pub fn sync(dirs: &ExportDirs, state: &mut State) -> Result<Report> {
    let desired = desired_exports(dirs, state);
    let mut report = Report::default();
    let font_dir = dirs.data_home.join("fonts/udnf");
    let icon_dir = dirs.data_home.join("icons");

    // Drop exports that are no longer wanted.
    let previous: Vec<(String, String)> = state.exports.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (path, kind) in previous {
        let p = PathBuf::from(&path);
        if desired.contains_key(&p) {
            continue;
        }
        if fs::symlink_metadata(&p).is_ok() {
            fs::remove_file(&p).with_context(|| format!("suppression de {}", p.display()))?;
            report.removed += 1;
        }
        report.fonts_changed |= p.starts_with(&font_dir);
        report.icons_changed |= p.starts_with(&icon_dir);
        if kind == "desktop" || kind == "dbus" {
            report.changed_kinds.insert(if kind == "desktop" { "desktop" } else { "dbus" });
        }
        prune_empty_parents(&p, &dirs.data_home);
        state.exports.remove(&path);
    }

    for (dest, export) in &desired {
        let key = dest.display().to_string();
        let exists = fs::symlink_metadata(dest).is_ok();
        if exists && !is_ours(dest, state.exports.get(&key).map(String::as_str), &dirs.root) {
            report.skipped.push(format!("{} existe déjà et n'appartient pas à udnf", dest.display()));
            continue;
        }
        if !(exists && unchanged(dest, export)) {
            write_export(dest, export)?;
            report.written += 1;
            report.changed_kinds.insert(export.kind());
            report.fonts_changed |= dest.starts_with(&font_dir);
            report.icons_changed |= dest.starts_with(&icon_dir);
        }
        state.exports.insert(key, export.kind().to_string());
    }
    Ok(report)
}

fn run_quiet(program: &str, args: &[&str]) {
    // Missing tools are fine: these only refresh caches.
    let _ = Command::new(program).args(args).output();
}

/// Replacements for the scriptlets udnf never runs: refresh the per-user caches.
pub fn run_triggers(dirs: &ExportDirs, report: &Report) {
    if report.changed_kinds.contains("desktop") {
        run_quiet("update-desktop-database", &["-q", &dirs.applications().display().to_string()]);
    }
    let hicolor = dirs.data_home.join("icons/hicolor");
    if report.icons_changed && hicolor.is_dir() {
        run_quiet("gtk-update-icon-cache", &["-q", "-t", "-f", &hicolor.display().to_string()]);
    }
    if report.fonts_changed {
        run_quiet("fc-cache", &["-f", &dirs.data_home.join("fonts").display().to_string()]);
    }
    let schemas = dirs.root.join("usr/share/glib-2.0/schemas");
    if schemas.is_dir() {
        let has_xml = fs::read_dir(&schemas)
            .is_ok_and(|it| it.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".gschema.xml")));
        if has_xml {
            run_quiet("glib-compile-schemas", &[&schemas.display().to_string()]);
        } else {
            let _ = fs::remove_file(schemas.join("gschemas.compiled"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{InstalledFile, InstalledPackage};

    fn file(path: &str, kind: &str) -> InstalledFile {
        InstalledFile { path: path.into(), kind: kind.into(), ..Default::default() }
    }

    fn put(root: &Path, rel: &str, content: &str, mode: u32) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn library_dirs_follow_ldconfig_rules() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        put(root, "usr/lib64/libfoo.so.1", "", 0o755);
        put(root, "usr/lib64/foo/libbar.so", "", 0o755);
        put(root, "usr/lib/README", "", 0o644);
        put(root, "etc/ld.so.conf.d/foo.conf", "# comment\n/usr/lib64/foo\n/nonexistent\n", 0o644);
        assert_eq!(library_dirs(root), vec![root.join("usr/lib64"), root.join("usr/lib64/foo")]);
    }

    #[test]
    fn wrapper_sets_env_only_when_needed() {
        let empty = WrapperEnv::default();
        let s = wrapper_script(Path::new("/r/usr/bin/x"), &empty);
        assert_eq!(s, format!("#!/bin/sh\n{MARKER}\nexec '/r/usr/bin/x' \"$@\"\n"));

        let env = WrapperEnv {
            lib_dirs: vec![PathBuf::from("/r/usr/lib64")],
            data_dirs: vec![PathBuf::from("/r/usr/share")],
            ..Default::default()
        };
        let s = wrapper_script(Path::new("/r/usr/bin/x"), &env);
        assert!(s.contains("export LD_LIBRARY_PATH='/r/usr/lib64'\"${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}\"\n"));
        assert!(s.contains("export XDG_DATA_DIRS='/r/usr/share'\":${XDG_DATA_DIRS:-/usr/local/share:/usr/share}\"\n"));
    }

    #[test]
    fn wrapper_runs_with_env() {
        let t = tempfile::tempdir().unwrap();
        let prog = t.path().join("prog");
        put(t.path(), "prog", "#!/bin/sh\necho \"$LD_LIBRARY_PATH|$1\"\n", 0o755);
        let env = WrapperEnv { lib_dirs: vec![PathBuf::from("/a b")], ..Default::default() };
        put(t.path(), "w", &wrapper_script(&prog, &env), 0o755);
        let out = Command::new(t.path().join("w")).arg("x y").env("LD_LIBRARY_PATH", "/sys").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "/a b:/sys|x y\n");
        let out = Command::new(t.path().join("w")).arg("z").env_remove("LD_LIBRARY_PATH").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "/a b|z\n");
    }

    #[test]
    fn exec_rewriting() {
        let wrappers = BTreeMap::from([("code".to_string(), PathBuf::from("/home/u/.local/bin/code"))]);
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "usr/share/code/code", "", 0o755);
        put(t.path(), "usr/share/pixmaps/code.png", "", 0o644);
        let entry = "[Desktop Entry]\nExec=/usr/share/code/code %F\nTryExec=code\nIcon=/usr/share/pixmaps/code.png\n\
                     [Desktop Action new]\nExec=env A=1 /usr/bin/code --new-window\nIcon=vscode\n";
        let extra = RefCell::new(BTreeMap::new());
        let out = rewrite_entry(entry, t.path(), &wrappers, Path::new("/x/exec"), &extra);
        let r = t.path().display();
        assert_eq!(
            out,
            format!(
                "[Desktop Entry]\nExec=/x/exec/usr/share/code/code %F\nTryExec=/home/u/.local/bin/code\n\
                 Icon={r}/usr/share/pixmaps/code.png\n[Desktop Action new]\n\
                 Exec=env A=1 /home/u/.local/bin/code --new-window\nIcon=vscode\n"
            )
        );
        // Programs outside of the bin dirs get their own wrapper.
        assert_eq!(
            extra.into_inner(),
            BTreeMap::from([(PathBuf::from("/x/exec/usr/share/code/code"), t.path().join("usr/share/code/code"))])
        );
        // System programs stay as they are.
        assert_eq!(rewrite_exec("/usr/bin/bash -c \"x y\"", &|_| None), "/usr/bin/bash -c \"x y\"");
    }

    fn setup() -> (tempfile::TempDir, ExportDirs, State) {
        let t = tempfile::tempdir().unwrap();
        let dirs =
            ExportDirs { root: t.path().join("root"), bin: t.path().join("bin"), data_home: t.path().join("share") };
        put(&dirs.root, "usr/bin/tool", "#!/bin/sh\n", 0o755);
        put(&dirs.root, "usr/lib64/libtool.so.1", "", 0o755);
        put(&dirs.root, "usr/share/applications/tool.desktop", "[Desktop Entry]\nExec=tool %U\n", 0o644);
        put(&dirs.root, "usr/share/icons/hicolor/48x48/apps/tool.png", "", 0o644);
        fs::create_dir_all(dirs.root.join("usr/bin/notacommand")).unwrap();
        fs::create_dir_all(dirs.root.join("usr/bin")).unwrap();
        symlink("/usr/bin/bash", dirs.root.join("usr/bin/sysbash")).unwrap();
        let mut state = State::default();
        state.packages.insert(
            "tool.x86_64".into(),
            InstalledPackage {
                nevra: "tool-1-1.x86_64".into(),
                name: "tool".into(),
                arch: "x86_64".into(),
                evr: "1-1".into(),
                repo_id: "t".into(),
                files: vec![
                    file("/usr/bin/tool", "file"),
                    file("/usr/bin/sysbash", "symlink"),
                    file("/usr/lib64/libtool.so.1", "file"),
                    file("/usr/share/applications/tool.desktop", "file"),
                    file("/usr/share/icons/hicolor/48x48/apps/tool.png", "file"),
                ],
            },
        );
        (t, dirs, state)
    }

    #[test]
    fn sync_creates_and_removes_exports() {
        let (_t, dirs, mut state) = setup();
        let report = sync(&dirs, &mut state).unwrap();
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);

        let wrapper = fs::read_to_string(dirs.bin.join("tool")).unwrap();
        assert!(wrapper.contains(&format!("{}/usr/lib64", dirs.root.display())));
        assert!(!dirs.bin.join("sysbash").exists(), "symlinks to the system are not wrapped");
        let desktop = fs::read_to_string(dirs.data_home.join("applications/tool.desktop")).unwrap();
        assert!(desktop.contains(&format!("Exec={} %U", dirs.bin.join("tool").display())));
        let icon = dirs.data_home.join("icons/hicolor/48x48/apps/tool.png");
        assert_eq!(fs::read_link(&icon).unwrap(), dirs.root.join("usr/share/icons/hicolor/48x48/apps/tool.png"));
        assert_eq!(state.exports.len(), 3);

        // Idempotent.
        let again = sync(&dirs, &mut state).unwrap();
        assert_eq!(again.written, 0);

        // Exports written but never recorded (interrupted run): still recognised as ours.
        state.exports.clear();
        let again = sync(&dirs, &mut state).unwrap();
        assert!(again.skipped.is_empty(), "{:?}", again.skipped);
        assert_eq!(state.exports.len(), 3);

        state.packages.clear();
        let report = sync(&dirs, &mut state).unwrap();
        assert_eq!(report.removed, 3);
        assert!(!dirs.bin.join("tool").exists());
        assert!(!dirs.data_home.join("icons/hicolor").exists(), "empty icon dirs pruned");
        assert!(state.exports.is_empty());
    }

    #[test]
    fn original_shebang_is_read_back_from_the_wrapper_path() {
        let exec = Path::new("/h/.local/share/udnf/exec");
        assert_eq!(original_shebang("/h/.local/share/udnf/exec/usr/bin/node-24 -x", exec), "/usr/bin/node-24 -x");
        assert_eq!(original_shebang("/usr/bin/python3 -s", exec), "/usr/bin/python3 -s");
        assert_eq!(original_shebang("/h/.local/share/udnf/exec2/x", exec), "/h/.local/share/udnf/exec2/x");
    }

    #[test]
    fn shebangs_follow_interpreters_of_the_root() {
        let (_t, dirs, mut state) = setup();
        put(&dirs.root, "usr/bin/interp", "#!/bin/sh\necho \"interp $1\"\n", 0o755);
        put(&dirs.root, "usr/lib/app/cli.js", "#!/usr/bin/interp -x\nbody\n", 0o755);
        put(&dirs.root, "usr/lib/app/sys.sh", "#!/bin/sh\necho sys\n", 0o755);
        let pkg = state.packages.get_mut("tool.x86_64").unwrap();
        for p in ["/usr/bin/interp", "/usr/lib/app/cli.js", "/usr/lib/app/sys.sh"] {
            pkg.files.push(file(p, "file"));
        }
        sync(&dirs, &mut state).unwrap();
        fix_shebangs(&dirs, &state).unwrap();

        let wrapper = dirs.exec_dir().join("usr/bin/interp");
        let cli = dirs.root.join("usr/lib/app/cli.js");
        assert_eq!(fs::read_to_string(&cli).unwrap(), format!("#!{} -x\nbody\n", wrapper.display()));
        assert!(fs::metadata(&cli).unwrap().permissions().mode() & 0o111 != 0);
        assert_eq!(fs::read_to_string(dirs.root.join("usr/lib/app/sys.sh")).unwrap(), "#!/bin/sh\necho sys\n");
        let out = Command::new(&cli).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "interp -x\n");

        // Interpreter gone (now provided by the system): original shebang restored.
        fs::remove_file(dirs.root.join("usr/bin/interp")).unwrap();
        sync(&dirs, &mut state).unwrap();
        fix_shebangs(&dirs, &state).unwrap();
        assert_eq!(fs::read_to_string(&cli).unwrap(), "#!/usr/bin/interp -x\nbody\n");
        assert!(!wrapper.exists());
    }

    #[test]
    fn foreign_files_are_never_overwritten() {
        let (_t, dirs, mut state) = setup();
        put(&dirs.bin, "tool", "#!/bin/sh\necho mine\n", 0o755);
        let report = sync(&dirs, &mut state).unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(fs::read_to_string(dirs.bin.join("tool")).unwrap(), "#!/bin/sh\necho mine\n");
        assert!(!state.exports.contains_key(&dirs.bin.join("tool").display().to_string()));
    }
}
