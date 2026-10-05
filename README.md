# udnf — User DNF

[![CI](https://github.com/pierrepinon/udnf/actions/workflows/ci.yml/badge.svg)](https://github.com/pierrepinon/udnf/actions/workflows/ci.yml)

**Rootless RPM package manager for your home directory, built on libdnf5.**

udnf installs RPM packages into `$HOME` without root. It uses your system's dnf repositories and
treats everything already installed on the system as available. That makes it a good fit for
image-based systems such as Fedora Silverblue and Kinoite: you get a tool without
`rpm-ostree install`, without a reboot, and without a container.

```console
$ udnf install htop
Installation:
  + htop-3.4.1-3.fc44.x86_64            fedora      203.6 KiB
  + hwloc-libs-2.14.0-1.fc44.x86_64     updates       2.1 MiB
Continuer ? [o/N] o
...
$ htop --version
htop 3.4.1
```

> **Status:** early (0.1). Developed and tested on Fedora 44 (Silverblue and Workstation),
> x86_64. The command-line messages are currently in French.

## Features

- **Reuses the system.** libdnf5 resolves dependencies against the system rpmdb, so only what is
  missing gets installed. udnf never modifies system packages: it refuses any transaction that
  would upgrade or replace one.
- **Your dnf configuration, plus yours.** udnf reads the full dnf5 configuration of the system:
  repo files, `repos.override.d`, vars and `libdnf5.conf.d` drop-ins. On top of that comes a user
  layer in `~/.config/udnf/` for your own repos, overrides and settings.
- **Nothing to configure.** Programs are exposed as wrappers in `~/.local/bin`, which set
  `LD_LIBRARY_PATH`, `XDG_DATA_DIRS`, `PYTHONPATH` and similar variables per program, never
  globally. Desktop entries, icons, fonts and man pages land in `~/.local/share`. Scripts whose
  interpreter is itself installed by udnf (e.g. `npm` → `node`) are rewritten to use it.
- **Signed packages.** Signatures are checked against a per-user keystore, because ostree systems
  ship no keys in their rpmdb. System-provided keys (`file://`) are imported automatically; remote
  keys need your confirmation.
- **No scriptlets, ever.** Packages with `%pre`, `%post`, triggers and the like are refused before
  anything is changed. Only `%pretrans` is ignored, never run.
- **GNOME Software plugin.** Optional: install, update and launch apps from *Software*, as a
  "udnf (utilisateur)" source next to Flatpak and system packages.

## Installation

udnf links against the system's `libdnf5` and `librpm`, so build it for the Fedora release you
run. To build the RPMs (needs podman):

```sh
git clone https://github.com/pierrepinon/udnf && cd udnf
scripts/build-rpm.sh
```

The RPMs land in `target/rpm/`. Install them:

```sh
# Fedora Workstation
sudo dnf install target/rpm/x86_64/udnf-0.1.0-*.x86_64.rpm \
                 target/rpm/x86_64/udnf-gnome-software-0.1.0-*.x86_64.rpm   # optional

# Fedora Silverblue / Kinoite (one-time layering, then reboot)
rpm-ostree install target/rpm/x86_64/udnf-0.1.0-*.x86_64.rpm \
                   target/rpm/x86_64/udnf-gnome-software-0.1.0-*.x86_64.rpm   # optional
```

`~/.local/bin` must be in your `PATH`. Fedora's default `~/.bashrc` already puts it there.

## Usage

```sh
udnf install ripgrep jq         # -y: don't ask; --dry-run: show the plan only
udnf remove jq                  # also removes dependencies nothing needs anymore
udnf upgrade [name...]          # upgrade everything, or only the named packages
udnf sync                       # after `rpm-ostree upgrade`: recompute what udnf must provide
udnf list [--available] [pattern...]
udnf search <term...>
udnf info <package...>
udnf repolist [--all] [-v]      # -v: origin file and user overrides of each repo
udnf makecache | clean
```

Global options: `--repo ID`, `--enablerepo ID`, `--disablerepo ID` (globs), `-C` (cache only),
`-v`, `--json` (see [Machine interface](#machine-interface)). With `install`, `--repo` only
restricts the packages being requested. Packages you already installed are still resolved against
every enabled repo.

### Adding a repository for yourself

Drop a `.repo` file in `~/.config/udnf/repos.d/`. No root and no `rpm --import` are needed:

```ini
# ~/.config/udnf/repos.d/vscodium.repo
[vscodium]
name=VSCodium
baseurl=https://download.vscodium.com/rpms/
gpgcheck=1
gpgkey=https://gitlab.com/paulcarroty/vscodium-deb-rpm-repo/raw/master/pub.gpg
```

The first install from that repo shows the key's fingerprint and asks before importing it into
your keystore.

## GNOME Software

The `udnf-gnome-software` package adds a plugin to GNOME Software. Every app of the system
AppStream catalog that udnf can install gets a **"udnf (utilisateur)"** source on its page, next
to Flatpak and the system packages (rpm-ostree on Silverblue, dnf5 on Workstation). udnf apps also
show up in search results.

- Install and remove apps without a password. udnf errors, such as a package with scriptlets or a
  remote key to import, are shown as notifications. A refused app is not offered again until its
  version changes.
- The **Installed** tab lists the apps you asked for (not their dependencies), and you can launch
  them from there.
- The **Updates** tab lists udnf apps, plus a "Composants udnf" entry for libraries and other
  dependencies.
- The plugin only runs the `udnf` command. It shares no library with GNOME Software and only uses
  the network when metadata is refreshed. It never imports a remote GPG key on its own: import it
  once with `udnf` on the command line.
- The plugin is tied to the plugin API of the GNOME Software version it was built against
  (`libgnomesoftware.so.23` for GNOME Software 50). Rebuild it when that API changes.

## How it works

- **World model.** udnf records the packages you asked for. Each operation resolves that whole set
  again with libdnf5, with the system rpmdb loaded as "installed", and the difference with what
  udnf installed is the plan. Installed versions stay pinned, except during `upgrade`.
- **Relocated root.** Package payloads are unpacked with librpm into `~/.local/share/udnf/root/`,
  so `/usr/bin/x` ends up at `root/usr/bin/x`. setuid bits are dropped. Absolute symlinks and
  shebangs that point at files installed by udnf are redirected into the root, and restored once
  the system provides those files again.
- **Exports.** Wrappers, desktop entries, D-Bus services, icons, fonts and man pages are generated
  from what is installed. udnf never overwrites a file it did not create. The per-user caches
  (`update-desktop-database`, `gtk-update-icon-cache`, `fc-cache`, `glib-compile-schemas`) are
  refreshed in place of the scriptlets.

| What | Where |
|---|---|
| Relocated root | `~/.local/share/udnf/root/` |
| State (requested packages, installed files, exports) | `~/.local/share/udnf/state.json` |
| GPG keystore | `~/.local/share/udnf/pubkeys/` |
| Metadata and package cache | `~/.cache/udnf/` |
| Settings (`[main]`, applied after `/etc/dnf/dnf.conf`) | `~/.config/udnf/udnf.conf` |
| Repos, overrides, vars | `~/.config/udnf/{repos.d,repos.override.d,vars}/` |

udnf sets `install_weak_deps=False` by default to keep the root small; you can change it in
`udnf.conf`.

## Limitations

- **Packages with scriptlets are refused.** That excludes anything relying on `alternatives`
  (Java, Maven), on services or system users (Docker), and some fonts. udnf targets developer
  tools: for runtimes such as Node.js or Java, a dedicated manager (nvm, sdkman…) is often the
  better choice. In tests, htop, npm, ansible and 11 out of 12 popular GUI apps installed fine.
- **Hard-coded paths.** A program that opens `/usr/share/...` by absolute path won't find its data.
  meld, for instance, installs but doesn't start. Tested and working: gnome-mahjongg, gnome-chess,
  gnome-sudoku, d-spy, ghex, geany.
- A wrapper's `LD_LIBRARY_PATH` is inherited by the processes the program starts. After an
  `rpm-ostree upgrade`, run `udnf sync` so that libraries now provided by the system are dropped
  from the root.
- No setuid, no systemd services, no multilib, no install-only packages (kernels).

## Machine interface

With `--json`, udnf prints one JSON value per line. The GNOME Software plugin uses it, and you can
script against it:

- `udnf --json status`: installed packages, with `requested` and `desktop_ids`.
- `udnf --json available [names...]`: latest installable version of packages not provided by the
  system.
- `udnf --json upgrade --dry-run`: a `plan` event listing the updates.
- `udnf --json -y install|remove|upgrade ...`: the events `plan`, `download`, `unpack`, `done`,
  or `error` with a `kind` among `resolve`, `system`, `scriptlets`, `key`, `signature`,
  `conflict`.

## Development

udnf is written in Rust. A small C++ shim (`shim/`, bridged with [cxx](https://cxx.rs)) is the only
code that talks to libdnf5 and librpm. The GNOME Software plugin is C, in
`gnome-software-plugin/`. Everything builds in a podman container matching the host's Fedora
release. `.devcontainer/` also works with VS Code Dev Containers.

```sh
scripts/dev.sh cargo build       # build in the dev container
scripts/dev.sh cargo test        # unit tests
tests/integration.sh             # end to end: local test repo, isolated HOME, runs on the host
tests/real-packages.sh [case...] # real repos (network): htop npm ansible maven docker vscode-user
tests/gnome-software.sh [rpm...] # GNOME Software plugin in a Fedora container, via gnome-software-cmd
scripts/build-rpm.sh             # SRPM + RPMs into target/rpm/ (vendored crates: builds offline)
```

The version comes from `Cargo.toml` and must match `Version:` in `udnf.spec`.

## License

GPL-2.0-or-later, see [LICENSE](LICENSE).
