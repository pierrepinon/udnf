#!/bin/bash
# End-to-end test of udnf against a local repository, with an isolated HOME.
# Usage: tests/integration.sh [path/to/udnf]
# Fixtures are built with rpmbuild/createrepo_c, through scripts/dev.sh when the host lacks them.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
udnf=$(realpath "${1:-$here/../target/debug/udnf}")
mkdir -p "$here/../target"
work=$(mktemp -d "$here/../target/it.XXXXXX")
trap 'chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"' EXIT
real_home=$HOME

in_build_env() {
    if command -v rpmbuild >/dev/null && command -v createrepo_c >/dev/null; then
        "$@"
    else
        # The build container mounts the repository: $work lives under target/.
        env -u XDG_DATA_HOME -u XDG_CONFIG_HOME -u XDG_CACHE_HOME -u XDG_STATE_HOME -u XDG_BIN_HOME \
            HOME="$real_home" "$here/../scripts/dev.sh" "$@"
    fi
}

build_rpm() { # spec [rpmbuild args...]
    in_build_env rpmbuild -bb --quiet --define "_topdir $work/rpmbuild" --define "_rpmdir $work/repo" \
        "${@:2}" "$here/fixtures/$1" >/dev/null 2>&1 || { echo "rpmbuild $1 failed"; exit 1; }
}

fail() { echo "ÉCHEC: $*" >&2; exit 1; }
step() { echo "== $*"; }

mkdir -p "$work/repo" "$work/home"
build_rpm libudnftest.spec --define "ver 1.0"
build_rpm udnftest-bin.spec
build_rpm udnftest-scriptlet.spec
in_build_env createrepo_c -q "$work/repo"

export HOME="$work/home"
export XDG_DATA_HOME="$HOME/.local/share" XDG_CONFIG_HOME="$HOME/.config" XDG_CACHE_HOME="$HOME/.cache" \
    XDG_STATE_HOME="$HOME/.local/state" XDG_BIN_HOME="$HOME/.local/bin"
mkdir -p "$HOME/.config/udnf/repos.d" "$HOME/.config/udnf/vars"
# The baseurl goes through a user variable: proves ~/.config/udnf/vars is loaded.
printf '%s' "$work/repo" > "$HOME/.config/udnf/vars/udnftestdir"
cat > "$HOME/.config/udnf/repos.d/udnftest.repo" <<REPO
[udnftest]
name=udnf test repo
baseurl=file://\$udnftestdir
gpgcheck=0
metadata_expire=0
REPO
root="$XDG_DATA_HOME/udnf/root"
run() { "$udnf" --repo udnftest "$@"; }

step "repolist voit le dépôt utilisateur"
run repolist | grep -q '^udnftest ' || fail "dépôt udnftest absent"

step "install --dry-run n'installe rien et ne propose pas glibc"
out=$(run install --dry-run udnftest-bin)
echo "$out"
grep -q 'libudnftest-1.0' <<<"$out" || fail "libudnftest absent du plan"
grep -q glibc <<<"$out" && fail "glibc ne doit pas être installé"
[[ ! -e $root ]] || fail "dry-run a écrit dans le root"

step "install"
run install -y udnftest-bin
[[ -f $root/usr/lib64/libudnftest.so.1 ]] || fail "lib non installée"
[[ $("$HOME/.local/bin/udnftest") == "udnftest 10" ]] || fail "le wrapper ne trouve pas la lib"
[[ $("$HOME/.local/bin/udnftest-helper") == "helper" ]] || fail "symlink absolu non relocalisé"
[[ $(readlink "$root/usr/bin/udnftest-helper") == "$root/usr/libexec/udnftest/helper" ]] || fail "cible du symlink"
grep -q "^Exec=$HOME/.local/bin/udnftest %U" "$XDG_DATA_HOME/applications/udnftest.desktop" || fail "desktop"
[[ -L $XDG_DATA_HOME/icons/hicolor/scalable/apps/udnftest.svg ]] || fail "icône"
out=$("$HOME/.local/bin/udnftest-script" 2>&1) || true; [[ $out == "interp env udnftest-script" ]] || fail "shebang vers un interpréteur du root: $out / $(head -1 "$root/usr/lib/udnftest/script.js") / $(ls "$XDG_DATA_HOME/udnf/exec")"
head -1 "$root/usr/lib/udnftest/script.js" | grep -q "^#!$XDG_DATA_HOME/udnf/exec/usr/bin/udnftest-interp\$" || fail "shebang non réécrit"
helper_wrapper="$XDG_DATA_HOME/udnf/exec/usr/libexec/udnftest/helper"
grep -q "^Exec=$helper_wrapper\$" "$XDG_DATA_HOME/applications/udnftest.desktop" || fail "Exec hors bin non wrappé"
[[ $("$helper_wrapper") == "helper" ]] || fail "wrapper exec"
grep -q LD_LIBRARY_PATH "$helper_wrapper" || fail "wrapper exec sans environnement"

step "un paquet avec scriptlet est refusé et rien n'est modifié"
before=$(cat "$XDG_DATA_HOME/udnf/state.json")
out=$(run install -y udnftest-scriptlet 2>&1) && fail "udnftest-scriptlet aurait dû être refusé"
grep -q "udnftest-scriptlet-1.0-1.noarch (%post)" <<<"$out" || fail "message attendu: $out"
[[ $(cat "$XDG_DATA_HOME/udnf/state.json") == "$before" ]] || fail "état modifié"
[[ ! -e $root/usr/share/udnftest-scriptlet ]] || fail "fichiers extraits"
[[ -z $(ls "$XDG_CACHE_HOME/udnf/packages" 2>/dev/null) ]] || fail "RPM téléchargés laissés dans le cache"

step "les paquets déjà présents sur le système ne sont pas installés"
out=$("$udnf" install --dry-run bash)
grep -q 'Déjà fourni par le système: bash' <<<"$out" || fail "bash devrait être fourni par le système"

step "un fichier étranger dans ~/.local/bin n'est jamais écrasé"
"$udnf" remove -y udnftest-bin
printf '#!/bin/sh\necho mine\n' > "$HOME/.local/bin/udnftest"; chmod +x "$HOME/.local/bin/udnftest"
out=$(run install -y udnftest-bin 2>&1)
grep -q "n'appartient pas à udnf" <<<"$out" || fail "pas d'avertissement: $out"
[[ $("$HOME/.local/bin/udnftest") == "mine" ]] || fail "fichier étranger écrasé"
rm "$HOME/.local/bin/udnftest"
"$udnf" sync -y >/dev/null
[[ $("$HOME/.local/bin/udnftest") == "udnftest 10" ]] || fail "sync n'a pas recréé le wrapper"

step "install d'un autre paquet ne met pas à jour les versions installées (épinglage)"
build_rpm libudnftest.spec --define "ver 2.0"
in_build_env createrepo_c -q --update "$work/repo"
out=$("$udnf" sync --dry-run)
grep -q 'Rien à faire' <<<"$out" || fail "sync ne devrait pas mettre à jour: $out"

step "upgrade d'un paquet nommé qui exige une dépendance plus récente"
# libudnftest 1.0 is pinned and still in the repo; udnftest-bin 2.0 needs libudnftest >= 2.0.
build_rpm udnftest-bin.spec --define "ver 2.0"
in_build_env createrepo_c -q --update "$work/repo"
"$udnf" upgrade -y udnftest-bin >/dev/null || fail "upgrade udnftest-bin"
[[ $("$HOME/.local/bin/udnftest") == "udnftest 20" ]] || fail "upgrade non appliqué"
grep -q '"libudnftest-2.0-1.x86_64"' "$XDG_DATA_HOME/udnf/state.json" || fail "état non mis à jour"
grep -q '"udnftest-bin-2.0-1.x86_64"' "$XDG_DATA_HOME/udnf/state.json" || fail "udnftest-bin non mis à jour"

step "liens durs et répertoire devenu symlink après la mise à jour"
for f in a b c; do
    [[ $(cat "$root/usr/share/udnftest/$f") == 2.0 ]] || fail "lien dur $f: contenu de l'ancienne version"
done
[[ -L $root/usr/share/udnftest/compat ]] || fail "compat devrait être un symlink"
[[ $(cat "$root/usr/share/udnftest/compat/data") == new ]] || fail "compat/data"

step "remove retire aussi les dépendances et les exports"
"$udnf" remove -y udnftest-bin
[[ ! -e $root/usr/lib64/libudnftest.so.1 ]] || fail "lib restante"
[[ ! -e $HOME/.local/bin/udnftest ]] || fail "wrapper restant"
[[ ! -e $XDG_DATA_HOME/applications/udnftest.desktop ]] || fail "desktop restant"
[[ ! -e $helper_wrapper ]] || fail "wrapper exec restant"
[[ -z $(find "$root" -type f) ]] || fail "fichiers restants: $(find "$root" -type f)"

step "un nom qui n'a pas été demandé ne peut pas être retiré"
out=$("$udnf" remove -y libudnftest 2>&1) && fail "remove aurait dû échouer"
grep -q "pas été demandé" <<<"$out" || fail "message attendu: $out"

step "les overrides utilisateur s'appliquent aux dépôts"
cat > "$HOME/.config/udnf/repos.d/udnftest-off.repo" <<REPO
[udnftest-off]
name=udnf test repo (disabled)
baseurl=file://$work/repo
enabled=0
gpgcheck=0
REPO
"$udnf" repolist --all | grep -qE '^udnftest-off +désactivé' || fail "udnftest-off devrait être désactivé"
mkdir -p "$HOME/.config/udnf/repos.override.d"
printf '[udnftest-off]\nenabled=1\n' > "$HOME/.config/udnf/repos.override.d/50-test.repo"
out=$("$udnf" repolist --all -v)
grep -qE '^udnftest-off +activé' <<<"$out" || fail "override utilisateur ignoré"
grep -q 'override udnf: 50-test.repo: enabled=1' <<<"$out" || fail "override non affiché par repolist -v"
rm "$HOME/.config/udnf/repos.override.d/50-test.repo" "$HOME/.config/udnf/repos.d/udnftest-off.repo"

step "udnf.conf [main] est appliqué"
printf '[main]\nexcludepkgs=udnftest-bin\n' > "$HOME/.config/udnf/udnf.conf"
out=$(run install --dry-run udnftest-bin 2>&1) && fail "udnftest-bin devrait être exclu"
grep -q "matches only excluded packages" <<<"$out" || fail "message attendu: $out"
rm "$HOME/.config/udnf/udnf.conf"

step "interface JSON (plugin gnome-software)"
out=$("$udnf" --json available udnftest-bin libudnftest bash)
grep -q '"name":"udnftest-bin"' <<<"$out" || fail "available: udnftest-bin absent: $out"
grep -q '"name":"bash"' <<<"$out" && fail "available: bash est fourni par le système"
out=$(run --json -y install udnftest-bin)
for ev in plan download unpack done; do grep -q "\"event\":\"$ev\"" <<<"$out" || fail "événement $ev absent: $out"; done
out=$("$udnf" --json status)
grep -q '"name":"udnftest-bin".*"requested":true.*"desktop_ids":\["udnftest.desktop"\]' <<<"$out" || fail "status: $out"
grep -q '"name":"libudnftest".*"requested":false' <<<"$out" || fail "status dépendance: $out"
out=$(run --json -y install udnftest-scriptlet 2>/dev/null) && fail "udnftest-scriptlet aurait dû être refusé"
grep -q '"event":"error","kind":"scriptlets"' <<<"$out" || fail "erreur scriptlets attendue: $out"
out=$("$udnf" --json -y remove udnftest-bin)
grep -q '"event":"done"' <<<"$out" || fail "remove json: $out"

echo "OK: tous les tests d'intégration passent"
