#!/bin/bash
# End-to-end test of the gnome-software plugin, in a Fedora container (network needed):
# gnome-software + AppStream catalog + udnf + libgs_plugin_udnf.so, driven by gnome-software-cmd
# as a non-root user. Uses the dev build (target/debug/udnf, target/gs-plugin) unless RPMs are
# given: tests/gnome-software.sh [udnf.rpm udnf-gnome-software.rpm]
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
image=localhost/udnf-dev:latest
rpms=("$@")

mounts=(-v "$repo/target:/target:ro,z")
for r in "${rpms[@]}"; do mounts+=(-v "$(realpath "$r"):/rpms/$(basename "$r"):ro,z"); done

exec podman run --rm -i "${mounts[@]}" "$image" bash -s <<'EOF'
set -uo pipefail
fail() { echo "ÉCHEC: $*" >&2; exit 1; }
step() { echo "== $*"; }

dnf -y -q install appstream-data dbus-daemon xorg-x11-server-Xvfb setpriv procps-ng >/dev/null 2>&1 || fail "dnf install"
if ls /rpms/*.rpm >/dev/null 2>&1; then
    dnf -y -q install /rpms/*.rpm >/dev/null 2>&1 || fail "installation des RPM udnf"
else
    install -m755 /target/debug/udnf /usr/bin/udnf
    install -m755 /target/gs-plugin/libgs_plugin_udnf.so "$(pkg-config --variable=plugindir gnome-software)/"
fi
useradd -m dev
mkdir -p /run/dbus && dbus-daemon --system --fork

# gnome-software-cmd as the user, with only the appstream and udnf plugins.
gs() {
    setpriv --reuid=dev --regid=dev --init-groups env HOME=/home/dev DISPLAY=:99 \
        dbus-run-session -- /usr/libexec/gnome-software-cmd --plugin-allowlist=appstream,udnf "$@" 2>&1 \
        | grep -v '^MESA\|^$'
}
as_dev() { setpriv --reuid=dev --regid=dev --init-groups env HOME=/home/dev "$@"; }
Xvfb :99 >/dev/null 2>&1 &
sleep 1

app=org.gnome.Meld.desktop
step "alternative udnf pour $app"
out=$(gs --show-results get-alternates $app)
grep -q "unique-id: *user/package/fedora/$app" <<<"$out" || fail "pas d'alternative udnf: $out"
grep -q "management-plugin: *udnf" <<<"$out" || fail "management-plugin: $out"
grep -q "state: *available" <<<"$out" || fail "état available attendu: $out"
grep -q "origin-ui: *udnf (utilisateur)" <<<"$out" || fail "origin-ui: $out"

step "installation de $app"
out=$(gs install meld)
as_dev test -x /home/dev/.local/bin/meld || fail "wrapper meld absent: $out"
as_dev test -f /home/dev/.local/share/applications/$app || fail "desktop absent"
out=$(gs --show-results installed)
grep -q "unique-id: *user/package/fedora/$app" <<<"$out" || fail "absent de la liste installée: $out"
grep -q "state: *installed" <<<"$out" || fail "état installed attendu: $out"

# meld itself can't run from udnf: it opens /usr/share/meld/... by absolute path (README).
step "lancement d'une app installée (gnome-mahjongg)"
gs install gnome-mahjongg >/dev/null
# gnome-software-cmd's "launch" passes a bare app no plugin manages; the UI passes the udnf app,
# whose launch delegates to gnome-software's helper: GDesktopAppInfo lookup, as gtk4-launch does.
# Checked inside the same session bus: the app dies with it.
as_dev env DISPLAY=:99 dbus-run-session -- sh -c \
    'gtk4-launch org.gnome.Mahjongg >/dev/null 2>&1; sleep 5; pgrep -f "udnf/root/usr/bin/gnome-mahjongg"' >/dev/null \
    || fail "gnome-mahjongg n'a pas démarré"
gs remove gnome-mahjongg >/dev/null

step "mise à jour détectée"
gs remove meld >/dev/null
as_dev udnf -y --disablerepo=updates install meld >/dev/null 2>&1 || fail "install meld 3.23 (CLI)"
out=$(gs --show-results updates)
grep -q "unique-id: *user/package/fedora/$app" <<<"$out" || fail "mise à jour de meld absente: $out"
grep -q "state: *updatable-live" <<<"$out" || fail "état updatable-live attendu: $out"
grep -q "update-version: *3.24" <<<"$out" || fail "version de mise à jour: $out"

step "désinstallation de $app"
out=$(gs remove meld)
as_dev test -e /home/dev/.local/bin/meld && fail "wrapper restant: $out"
out=$(gs --show-results installed)
grep -q "user/package/fedora/$app" <<<"$out" && fail "toujours listé installé: $out"

step "app refusée (scriptlets) : erreur remontée à l'utilisateur"
# Plugin errors reach the user as events (notifications); gnome-software-cmd only logs them.
out=$(gs --verbose install inkscape)
grep -q "udnf: udnf n'installe pas les paquets qui utilisent des scriptlets" <<<"$out" || fail "erreur scriptlets: $out"
grep -q "Adding event" <<<"$out" || fail "aucun événement émis"
as_dev test -e /home/dev/.local/bin/inkscape && fail "inkscape installé"
out=$(gs --show-results get-alternates org.inkscape.Inkscape)
grep -q "management-plugin: *udnf" <<<"$out" && fail "inkscape encore proposé après refus"

echo "OK: plugin gnome-software"
EOF
