mod export;
mod ffi;
mod install;
mod json;
mod paths;
mod plan;
mod state;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use ffi::bridge as dnf;
use paths::Paths;
use state::{Lock, State};

/// udnf — gestionnaire de paquets RPM sans root, installé dans $HOME, basé sur libdnf5.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Args)]
struct Global {
    /// Répondre oui à toutes les questions.
    #[arg(short = 'y', long, global = true)]
    assumeyes: bool,
    /// N'utiliser que le cache local des métadonnées.
    #[arg(short = 'C', long, global = true)]
    cacheonly: bool,
    /// N'activer que ces dépôts (glob, répétable).
    #[arg(long = "repo", global = true, value_name = "ID")]
    repo: Vec<String>,
    /// Activer ces dépôts en plus (glob, répétable).
    #[arg(long, global = true, value_name = "ID")]
    enablerepo: Vec<String>,
    /// Désactiver ces dépôts (glob, répétable).
    #[arg(long, global = true, value_name = "ID")]
    disablerepo: Vec<String>,
    /// Affichage détaillé.
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Sortie machine : une valeur JSON par ligne (interface du plugin gnome-software).
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Installer des paquets (seules les dépendances absentes du système sont installées).
    Install {
        #[arg(required = true)]
        specs: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Désinstaller des paquets et les dépendances devenues inutiles.
    Remove {
        #[arg(required = true)]
        specs: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Mettre à jour les paquets installés (tous, ou seulement ceux nommés).
    Upgrade {
        names: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Recalculer l'installation après une mise à jour du système (rpm-ostree upgrade).
    Sync {
        #[arg(long)]
        dry_run: bool,
    },
    /// Lister les paquets installés par udnf, ou disponibles.
    List {
        patterns: Vec<String>,
        #[arg(long, conflicts_with = "installed")]
        available: bool,
        #[arg(long)]
        installed: bool,
    },
    /// Paquets installés par udnf (avec --json : interface machine).
    Status,
    /// Paquets installables (non fournis par le système), dernière version.
    Available {
        /// Limiter à ces noms de paquets.
        names: Vec<String>,
    },
    /// Chercher dans les noms et résumés des paquets.
    Search {
        #[arg(required = true)]
        terms: Vec<String>,
    },
    /// Afficher les détails de paquets.
    Info {
        #[arg(required = true)]
        specs: Vec<String>,
    },
    /// Lister les dépôts configurés.
    Repolist {
        /// Inclure les dépôts désactivés.
        #[arg(long)]
        all: bool,
    },
    /// Télécharger les métadonnées des dépôts.
    Makecache,
    /// Vider le cache de udnf.
    Clean,
}

struct Ctx {
    paths: Paths,
    global: Global,
    out: json::Reporter,
}

impl Ctx {
    /// `restrict_repos`: apply `--repo` by disabling the other repos (queries). Transactions
    /// pass `false` and restrict only the requested specs, so installed packages stay resolvable.
    fn open(&self, available: bool, restrict_repos: bool) -> Result<cxx::UniquePtr<dnf::Session>> {
        let p = &self.paths;
        let opts = dnf::OpenOptions {
            cachedir: p.cache.display().to_string(),
            persistdir: p.persist.display().to_string(),
            logdir: p.logs.display().to_string(),
            user_config: p.user_config.display().to_string(),
            user_reposdir: p.repos_d.display().to_string(),
            user_overridedir: p.repos_override_d.display().to_string(),
            user_varsdir: p.vars.display().to_string(),
            enable_repos: self.global.enablerepo.clone(),
            disable_repos: self.global.disablerepo.clone(),
            only_repos: if restrict_repos { self.global.repo.clone() } else { vec![] },
            cacheonly: self.global.cacheonly,
            keyring_dir: p.keyring.display().to_string(),
        };
        let mut session = dnf::open_session(&opts).context("initialisation de libdnf5")?;
        session.pin_mut().load_repos(available).context("chargement des dépôts")?;
        Ok(session)
    }

    fn export_dirs(&self) -> export::ExportDirs {
        export::ExportDirs {
            root: self.paths.root.clone(),
            bin: self.paths.bin.clone(),
            data_home: self.paths.data_home.clone(),
        }
    }
}

enum Mode {
    Install(Vec<String>),
    Remove(Vec<String>),
    Upgrade(Vec<String>),
    Sync,
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() || std::env::args().any(|a| a == "--json") {
        bail!("confirmation requise: relancez avec -y");
    }
    print!("{question} [o/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "o" | "oui" | "y" | "yes"))
}

fn transaction(ctx: &Ctx, mode: Mode, dry_run: bool) -> Result<()> {
    let paths = &ctx.paths;
    let _lock = Lock::acquire(&paths.lock_file)?;
    let mut state = State::load(&paths.state_file)?;

    let mut world = state.world.clone();
    match &mode {
        Mode::Install(specs) => world.extend(specs.iter().cloned()),
        Mode::Remove(specs) => {
            for s in specs {
                if !world.remove(s) {
                    let known = state.world.iter().cloned().collect::<Vec<_>>().join(", ");
                    bail!("« {s} » n'a pas été demandé explicitement (demandés: {known})");
                }
            }
        }
        Mode::Upgrade(_) | Mode::Sync => {}
    }

    // Pin installed versions, except for what is being upgraded.
    let pins: Vec<String> = match &mode {
        Mode::Upgrade(names) if names.is_empty() => vec![],
        Mode::Upgrade(names) => {
            for n in names {
                if !state.packages.values().any(|p| &p.name == n) {
                    bail!("« {n} » n'est pas installé par udnf");
                }
            }
            state.packages.values().filter(|p| !names.contains(&p.name)).map(|p| p.nevra.clone()).collect()
        }
        _ => state.packages.values().map(|p| p.nevra.clone()).collect(),
    };

    let mut session = ctx.open(true, false)?;
    let restricted_specs = match &mode {
        Mode::Install(specs) if !ctx.global.repo.is_empty() => specs.clone(),
        _ if !ctx.global.repo.is_empty() => {
            bail!("--repo ne s'utilise qu'avec install, list, search, info et repolist")
        }
        _ => vec![],
    };
    let mut request = dnf::ResolveRequest {
        specs: world.iter().cloned().collect(),
        pins,
        restricted_specs,
        repos: ctx.global.repo.clone(),
        // An upgraded package must move to its latest version, or the solver may silently keep
        // the installed one when a pinned dependency is too old (then the pins are relaxed below).
        best_specs: match &mode {
            Mode::Upgrade(names) => world
                .iter()
                .filter(|spec| state.packages.values().any(|p| names.contains(&p.name) && &&p.name == spec))
                .cloned()
                .collect(),
            _ => vec![],
        },
    };
    let mut resolution = session.pin_mut().resolve(&request)?;
    // Pinned versions may not fit: after a system update (sync), or when the upgraded packages
    // need newer dependencies (upgrade <names>). Retry with fewer pins.
    if !resolution.ok && !request.pins.is_empty() {
        let relaxed: Option<Vec<String>> = match &mode {
            Mode::Sync => Some(vec![]),
            // Keep only the other explicitly requested packages: dependencies may move.
            Mode::Upgrade(names) if !names.is_empty() => Some(
                state
                    .packages
                    .values()
                    .filter(|p| state.world.contains(&p.name) && !names.contains(&p.name))
                    .map(|p| p.nevra.clone())
                    .collect(),
            ),
            _ => None,
        };
        if let Some(pins) = relaxed {
            ctx.out.say(
                "Les versions épinglées ne permettent pas de résoudre, nouvelle résolution avec moins d'épinglage.",
            );
            request.pins = pins;
            resolution = session.pin_mut().resolve(&request)?;
        }
    }
    if ctx.global.verbose {
        for p in &resolution.problems {
            eprintln!("libdnf5: {p}");
        }
    }
    let plan = plan::build(&resolution, &state)?;
    if !ctx.out.json {
        print!("{}", plan::render(&plan, &state));
    }
    ctx.out.event(&plan_event(&plan, &state));

    if plan.is_empty() {
        ctx.out.say("Rien à faire.");
        if !dry_run {
            state.world = world;
            finish(ctx, &mut state)?;
            ctx.out.event(&json::Event::Done);
        }
        return Ok(());
    }
    if dry_run {
        return Ok(());
    }
    if !ctx.global.assumeyes && !confirm("Continuer ?")? {
        bail!("opération annulée");
    }

    // Fetch and verify everything before touching the root.
    let fetch = plan.to_fetch();
    let nevras: Vec<String> = fetch.iter().map(|t| t.nevra.clone()).collect();
    let downloaded = if nevras.is_empty() {
        vec![]
    } else {
        ctx.out.say("Téléchargement des paquets…");
        ctx.out.event(&json::Event::Download {
            packages: fetch.len(),
            bytes: fetch.iter().map(|t| t.download_size).sum(),
        });
        session.pin_mut().download(&nevras, &paths.packages.display().to_string())?
    };
    verify_signatures(ctx, &mut session, &downloaded)?;
    let mut incoming = Vec::new();
    for (t, d) in fetch.iter().zip(&downloaded) {
        incoming.push((t.key.clone(), dnf::read_rpm(&d.path)?));
    }
    if let Err(e) = install::check_no_scriptlets(&incoming) {
        for d in &downloaded {
            let _ = fs::remove_file(&d.path);
        }
        return Err(e);
    }
    let leaving: BTreeSet<String> =
        plan.remove.iter().cloned().chain(plan.update.iter().map(|(_, t)| t.key.clone())).collect();
    install::check_conflicts(&state, &leaving, &incoming)?;
    let retyped = install::type_changes(&paths.root, &state, &leaving, &incoming)?;

    for key in &plan.remove {
        if ctx.global.verbose {
            println!("  - {key}");
        }
        install::remove_package(&paths.root, &mut state, key)?;
        state.save(&paths.state_file)?;
    }
    install::clear_type_changes(&retyped)?;
    for (i, (t, d)) in fetch.iter().zip(&downloaded).enumerate() {
        ctx.out.say(format!("  + {}", t.nevra));
        ctx.out.event(&json::Event::Unpack { nevra: t.nevra.clone(), index: i + 1, total: fetch.len() });
        let contents = dnf::extract_rpm(&d.path, &paths.root.display().to_string(), false)
            .with_context(|| format!("extraction de {}", t.nevra))?;
        install::commit_package(&paths.root, &mut state, t, &contents)?;
        state.save(&paths.state_file)?;
    }
    for d in &downloaded {
        let _ = fs::remove_file(&d.path);
    }

    state.world = world;
    finish(ctx, &mut state)?;
    ctx.out.say("Terminé.");
    ctx.out.event(&json::Event::Done);
    Ok(())
}

fn plan_event(plan: &plan::Plan, state: &State) -> json::Event {
    json::Event::Plan {
        install: plan
            .install
            .iter()
            .map(|t| json::PlanPackage {
                name: t.name.clone(),
                arch: t.arch.clone(),
                evr: t.evr.clone(),
                nevra: t.nevra.clone(),
                repo_id: t.repo_id.clone(),
                download_size: t.download_size,
                install_size: t.install_size,
            })
            .collect(),
        update: plan
            .update
            .iter()
            .map(|(from, t)| json::PlanUpdate {
                name: t.name.clone(),
                arch: t.arch.clone(),
                from: state.packages.get(&t.key).map_or_else(|| from.clone(), |p| p.evr.clone()),
                to: t.evr.clone(),
                repo_id: t.repo_id.clone(),
                download_size: t.download_size,
            })
            .collect(),
        remove: plan
            .remove
            .iter()
            .filter_map(|k| state.packages.get(k))
            .map(|p| json::PlanRemove { name: p.name.clone(), arch: p.arch.clone(), nevra: p.nevra.clone() })
            .collect(),
        system: plan.system_specs.clone(),
    }
}

/// Checks every package signature against the user keystore. A missing key is imported from
/// the repo's `gpgkey`: silently when the system ships it (`file://`), after confirmation
/// otherwise, as dnf does.
fn verify_signatures(
    ctx: &Ctx,
    session: &mut cxx::UniquePtr<dnf::Session>,
    downloaded: &[dnf::DownloadedPackage],
) -> Result<()> {
    let mut imported_for = BTreeSet::new();
    for d in downloaded {
        let mut check = session.pin_mut().check_signature(&d.path, &d.repo_id)?;
        if !check.ok && check.key_missing && imported_for.insert(d.repo_id.clone()) {
            import_repo_keys(ctx, session, &d.repo_id)?;
            check = session.pin_mut().check_signature(&d.path, &d.repo_id)?;
        }
        if !check.ok {
            return Err(json::error(
                json::Kind::Signature,
                format!("signature invalide pour {} ({}): {}", d.nevra, d.repo_id, check.message),
            ));
        }
    }
    Ok(())
}

fn import_repo_keys(ctx: &Ctx, session: &mut cxx::UniquePtr<dnf::Session>, repo_id: &str) -> Result<()> {
    let keys = session.pin_mut().repo_keys(repo_id).with_context(|| format!("clés du dépôt {repo_id}"))?;
    if keys.is_empty() {
        bail!("le dépôt {repo_id} exige des signatures mais ne déclare aucune clé (gpgkey)");
    }
    for k in keys.iter().filter(|k| !k.present) {
        let shipped_by_system = k.url.starts_with("file://");
        // The machine interface never trusts a remote key on its own, even with -y.
        if !shipped_by_system && ctx.out.json {
            return Err(json::error(
                json::Kind::Key,
                format!(
                    "la clé {} du dépôt {repo_id} ({}, empreinte {}) doit être importée avec udnf en ligne de commande",
                    k.key_id, k.url, k.fingerprint
                ),
            ));
        }
        ctx.out.say(format!("Import de la clé {} pour le dépôt {repo_id}", k.key_id));
        for uid in &k.user_ids {
            ctx.out.say(format!("  utilisateur : {uid}"));
        }
        ctx.out.say(format!("  empreinte   : {}", k.fingerprint));
        ctx.out.say(format!("  source      : {}", k.url));
        if !shipped_by_system && !ctx.global.assumeyes && !confirm("Importer cette clé ?")? {
            return Err(json::error(json::Kind::Key, "clé refusée"));
        }
        session.pin_mut().import_key(repo_id, &k.fingerprint)?;
    }
    Ok(())
}

/// Symlinks, exports and cache triggers, then the final state save.
fn finish(ctx: &Ctx, state: &mut State) -> Result<()> {
    install::fix_symlinks(&ctx.paths.root, state)?;
    let dirs = ctx.export_dirs();
    let report = export::sync(&dirs, state);
    // Exports written before a failure are already in state.exports: record them regardless.
    state.save(&ctx.paths.state_file)?;
    let report = report?;
    export::fix_shebangs(&dirs, state)?;
    export::run_triggers(&dirs, &report);
    for s in &report.skipped {
        eprintln!("attention: {s}");
    }
    Ok(())
}

fn status(ctx: &Ctx) -> Result<()> {
    let state = State::load(&ctx.paths.state_file)?;
    for p in state.packages.values() {
        let requested = state.world.contains(&p.name) || state.world.contains(&p.nevra);
        if ctx.out.json {
            let desktop_ids = p
                .files
                .iter()
                .filter_map(|f| f.path.strip_prefix("/usr/share/applications/"))
                .filter(|n| n.ends_with(".desktop") && !n.contains('/'))
                .map(str::to_owned)
                .collect();
            json::line(&json::Installed {
                name: p.name.clone(),
                arch: p.arch.clone(),
                evr: p.evr.clone(),
                nevra: p.nevra.clone(),
                repo_id: p.repo_id.clone(),
                requested,
                desktop_ids,
            });
        } else {
            let mark = if requested { "*" } else { " " };
            println!("{mark} {:<48} {:<30} {}", format!("{}.{}", p.name, p.arch), p.evr, p.repo_id);
        }
    }
    Ok(())
}

/// Latest installable version of each package name not provided by the system. When several
/// arches exist (multilib), the native one wins.
fn available(ctx: &Ctx, names: &[String]) -> Result<()> {
    let session = ctx.open(true, true)?;
    let system: BTreeSet<String> = session.query(&vec![], true, false, false)?.into_iter().map(|p| p.name).collect();
    let mut best: BTreeMap<String, dnf::PackageInfo> = BTreeMap::new();
    for p in session.query(&names.to_vec(), false, true, true)? {
        if system.contains(&p.name) {
            continue;
        }
        let multilib = |arch: &str| arch == "i686";
        match best.get(&p.name) {
            Some(cur) if !multilib(&cur.arch) || multilib(&p.arch) => {}
            _ => {
                best.insert(p.name.clone(), p);
            }
        }
    }
    for p in best.into_values() {
        if ctx.out.json {
            json::line(&json::Available {
                name: p.name,
                arch: p.arch,
                evr: p.evr,
                repo_id: p.repo_id,
                summary: p.summary,
                download_size: p.download_size,
                install_size: p.install_size,
            });
        } else {
            println!("{:<50} {:<30} {}", format!("{}.{}", p.name, p.arch), p.evr, p.repo_id);
        }
    }
    Ok(())
}

fn print_packages(pkgs: &[dnf::PackageInfo]) {
    for p in pkgs {
        println!("{:<50} {:<30} {}", format!("{}.{}", p.name, p.arch), p.evr, p.repo_id);
    }
}

fn run(cli: Cli) -> Result<()> {
    let out = json::Reporter { json: cli.global.json };
    let ctx = Ctx { paths: Paths::from_env()?, global: cli.global, out };
    match cli.command {
        Cmd::Install { specs, dry_run } => transaction(&ctx, Mode::Install(specs), dry_run),
        Cmd::Remove { specs, dry_run } => transaction(&ctx, Mode::Remove(specs), dry_run),
        Cmd::Upgrade { names, dry_run } => transaction(&ctx, Mode::Upgrade(names), dry_run),
        Cmd::Sync { dry_run } => transaction(&ctx, Mode::Sync, dry_run),
        Cmd::List { patterns, available, installed: _ } => {
            if available {
                let session = ctx.open(true, true)?;
                print_packages(&session.query(&patterns, false, true, true)?);
            } else {
                let state = State::load(&ctx.paths.state_file)?;
                for p in state.packages.values() {
                    if !patterns.is_empty() && !patterns.iter().any(|pat| p.name.contains(pat.as_str())) {
                        continue;
                    }
                    let mark = if state.world.contains(&p.name) { "*" } else { " " };
                    println!("{mark} {:<48} {:<30} {}", format!("{}.{}", p.name, p.arch), p.evr, p.repo_id);
                }
            }
            Ok(())
        }
        Cmd::Status => status(&ctx),
        Cmd::Available { names } => available(&ctx, &names),
        Cmd::Search { terms } => {
            let session = ctx.open(true, true)?;
            for p in session.search(&terms)? {
                println!("{:<40} {}", format!("{}.{}", p.name, p.arch), p.summary);
            }
            Ok(())
        }
        Cmd::Info { specs } => {
            let session = ctx.open(true, true)?;
            let pkgs = session.query(&specs, true, true, true)?;
            if pkgs.is_empty() {
                bail!("aucun paquet ne correspond");
            }
            for p in pkgs {
                println!("Nom         : {}", p.name);
                println!("Version     : {}", p.evr);
                println!("Arch        : {}", p.arch);
                println!("Dépôt       : {}{}", p.repo_id, if p.installed { " (système)" } else { "" });
                println!("Taille      : {}", plan::human_size(p.install_size));
                println!("URL         : {}", p.url);
                println!("Licence     : {}", p.license);
                println!("Résumé      : {}", p.summary);
                println!("Description : {}\n", p.description.replace('\n', "\n              "));
            }
            Ok(())
        }
        Cmd::Repolist { all } => {
            let session = ctx.open(false, true)?;
            for r in session.repolist(all) {
                let status = if r.enabled { "activé" } else { "désactivé" };
                println!("{:<45} {:<10} {}", r.id, status, r.name);
                if ctx.global.verbose {
                    println!("    fichier: {}", r.origin);
                    for o in &r.overrides {
                        println!("    override udnf: {o}");
                    }
                }
            }
            Ok(())
        }
        Cmd::Makecache => {
            ctx.open(true, true)?;
            ctx.out.say("Métadonnées à jour.");
            ctx.out.event(&json::Event::Done);
            Ok(())
        }
        Cmd::Clean => {
            if ctx.paths.cache.exists() {
                fs::remove_dir_all(&ctx.paths.cache)?;
            }
            println!("Cache supprimé: {}", ctx.paths.cache.display());
            Ok(())
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let json = cli.global.json;
    if let Err(e) = run(cli) {
        if json {
            json::line(&json::Event::Error { kind: json::kind_of(&e), message: format!("{e:#}") });
        }
        eprintln!("udnf: {e:#}");
        std::process::exit(1);
    }
}
