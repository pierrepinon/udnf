#!/bin/bash
# udnf against real packages from the real repositories (network needed, run on the host).
# Each case gets a fresh isolated HOME; repo metadata is cached in target/real-cache.
# Usage: tests/real-packages.sh [case...]    cases: htop npm ansible maven docker vscode-user (default: all)
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
udnf=$(realpath "$here/../target/debug/udnf")
target=$(realpath "$here/../target")
cache="$target/real-cache"
mkdir -p "$cache"

failures=()
current=""
fail() { echo "  ÉCHEC: $*" >&2; failures+=("$current: $*"); return 1; }

# Fresh HOME for one case.
setup() {
    current=$1
    echo "== $1"
    export HOME="$target/real-home-$1"
    chmod -R u+w "$HOME" 2>/dev/null; rm -rf "$HOME"
    mkdir -p "$HOME"
    export XDG_DATA_HOME="$HOME/.local/share" XDG_CONFIG_HOME="$HOME/.config" XDG_CACHE_HOME="$cache" \
        XDG_STATE_HOME="$HOME/.local/state" XDG_BIN_HOME="$HOME/.local/bin"
    root="$XDG_DATA_HOME/udnf/root"
    bin="$XDG_BIN_HOME"
}

teardown() {
    chmod -R u+w "$HOME" 2>/dev/null; rm -rf "$HOME"
}

# install must succeed; then removal must leave nothing behind.
expect_installed() {
    "$udnf" install -y "$@" >"$HOME/install.log" 2>&1 || { fail "install $*: $(tail -3 "$HOME/install.log")"; return 1; }
}
expect_clean_removal() {
    "$udnf" remove -y "$@" >/dev/null 2>&1 || { fail "remove $*"; return 1; }
    local left
    left=$(find "$root" "$bin" -not -type d 2>/dev/null)
    [[ -z $left ]] || fail "fichiers restants après remove: $(head -3 <<<"$left")"
}
# install must be refused because of scriptlets, naming `$1`, and change nothing.
expect_refused() {
    local culprit=$1; shift
    local out
    out=$("$udnf" install -y "$@" 2>&1) && { fail "install $* aurait dû être refusé"; return 1; }
    grep -q "utilisent des scriptlets" <<<"$out" || { fail "refus attendu pour scriptlets: $(tail -3 <<<"$out")"; return 1; }
    grep -q "$culprit" <<<"$out" || fail "$culprit absent du message: $(tail -5 <<<"$out")"
    [[ -z $(find "$root" -not -type d 2>/dev/null) ]] || fail "le root a été modifié"
    [[ -z $(ls "$cache/udnf/packages" 2>/dev/null) ]] || fail "RPM laissés dans le cache"
}

case_htop() {
    setup htop
    expect_installed htop || return
    [[ $("$bin/htop" --version) == "htop 3."* ]] || fail "htop --version"
    expect_clean_removal htop
    teardown
}

case_npm() {
    setup npm
    expect_installed npm || return
    grep -q "nodejs24-libs" "$XDG_DATA_HOME/udnf/state.json" || fail "nodejs24-libs devrait venir de udnf"
    [[ $("$bin/npm" --version) =~ ^[0-9]+\.[0-9]+ ]] || fail "npm --version"
    [[ $("$bin/npx" --version) =~ ^[0-9]+\.[0-9]+ ]] || fail "npx --version"
    [[ $("$bin/node" -e 'console.log(6*7)') == 42 ]] || fail "node -e"
    head -1 "$root/usr/lib/node_modules_24/npm/bin/npm-cli.js" | grep -q "^#!$XDG_DATA_HOME/udnf/exec/usr/bin/node" \
        || fail "shebang de npm-cli.js non réécrit"
    expect_clean_removal npm
    teardown
}

# Python application on the system interpreter (#!/usr/bin/python3), its modules and the
# collections coming from the udnf root through the wrapper's PYTHONPATH.
case_ansible() {
    setup ansible
    expect_installed ansible || return
    local out
    out=$("$bin/ansible" --version 2>&1)
    grep -q "ansible python module location = $root/" <<<"$out" || fail "modules ansible hors du root: $out"
    "$bin/ansible" localhost -c local -m ping 2>&1 | grep -q '"ping": "pong"' || fail "ansible -m ping"
    cat > "$HOME/playbook.yml" <<'YAML'
- hosts: localhost
  connection: local
  gather_facts: false
  tasks:
    - ansible.builtin.copy:
        dest: "{{ lookup('env', 'HOME') }}/out.json"
        content: "{{ 'x' | community.general.dict_kv('k') | to_json }}"
YAML
    "$bin/ansible-playbook" -i localhost, "$HOME/playbook.yml" >"$HOME/playbook.log" 2>&1 \
        || fail "ansible-playbook: $(tail -3 "$HOME/playbook.log")"
    [[ $(cat "$HOME/out.json" 2>/dev/null) == '{"k": "x"}' ]] || fail "collection community.general"
    expect_clean_removal ansible
    teardown
}

case_maven() {
    setup maven
    # alternatives (/usr/bin/mvn, /usr/bin/java) are set up by %post scriptlets.
    expect_refused "maven-.* (%post" maven
    teardown
}

case_docker() {
    setup docker
    expect_refused "moby-engine-.* (%post" docker
    teardown
}

# Repo declared only in the user's HOME, key fetched over https by udnf (no `sudo rpm --import`).
case_vscode-user() {
    setup vscode-user
    mkdir -p "$XDG_CONFIG_HOME/udnf/repos.d"
    cat > "$XDG_CONFIG_HOME/udnf/repos.d/vscode.repo" <<'REPO'
[vscode-user]
name=Visual Studio Code (user)
baseurl=https://packages.microsoft.com/yumrepos/vscode
enabled=1
autorefresh=1
type=rpm-md
gpgcheck=1
gpgkey=https://packages.microsoft.com/keys/microsoft.asc
REPO
    local out
    out=$("$udnf" repolist -v --repo vscode-user 2>&1)
    grep -qE '^vscode-user +activé' <<<"$out" || fail "dépôt utilisateur non chargé: $out"
    grep -q "fichier: $XDG_CONFIG_HOME/udnf/repos.d/vscode.repo" <<<"$out" || fail "origine du dépôt"
    # code is installed on the host, code-insiders is not: it comes from the user repo.
    "$udnf" install --dry-run --repo vscode-user code-insiders 2>&1 | grep -q "code-insiders-.*vscode-user" \
        || fail "code-insiders non résolu depuis vscode-user"
    # The Microsoft key is fetched and imported into the user keystore, then the package is
    # refused for its %post: VS Code from RPM is out of scope for udnf.
    expect_refused "code-insiders-.* (%post" --repo vscode-user code-insiders
    ls "$XDG_DATA_HOME/udnf/pubkeys" | grep -q bc528686b50d79e339d3721ceb3e94adbe1229cf \
        || fail "clé Microsoft absente du keystore utilisateur"
    teardown
}

cases=("$@")
[[ ${#cases[@]} -gt 0 ]] || cases=(htop npm ansible maven docker vscode-user)
for c in "${cases[@]}"; do
    declare -F "case_$c" >/dev/null || { echo "cas inconnu: $c" >&2; exit 2; }
    "case_$c"
done

if [[ ${#failures[@]} -gt 0 ]]; then
    printf 'ÉCHECS:\n'; printf '  %s\n' "${failures[@]}"
    exit 1
fi
echo "OK: ${cases[*]}"
