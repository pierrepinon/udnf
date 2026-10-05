#include "udnf/shim/udnf.hpp"

#include "udnf/src/ffi.rs.h"

#include <libdnf5/base/goal.hpp>
#include <libdnf5/base/transaction.hpp>
#include <libdnf5/base/transaction_package.hpp>
#include <libdnf5/conf/config_parser.hpp>
#include <libdnf5/conf/const.hpp>
#include <libdnf5/repo/download_callbacks.hpp>
#include <libdnf5/repo/package_downloader.hpp>
#include <libdnf5/repo/repo_query.hpp>
#include <libdnf5/rpm/package_query.hpp>
#include <libdnf5/rpm/package_set.hpp>
#include <libdnf5/rpm/rpm_signature.hpp>
#include <libdnf5/transaction/transaction_item_action.hpp>
#include <libdnf5/transaction/transaction_item_reason.hpp>
#include <rpm/header.h>
#include <rpm/rpmarchive.h>
#include <rpm/rpmfiles.h>
#include <rpm/rpmio.h>
#include <rpm/rpmlib.h>
#include <rpm/rpmmacro.h>
#include <rpm/rpmtag.h>
#include <rpm/rpmts.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cstring>
#include <filesystem>
#include <iostream>
#include <set>
#include <stdexcept>

namespace fs = std::filesystem;
using Priority = libdnf5::Option::Priority;

namespace udnf {

namespace {

// Plugins whose job only makes sense for a root-run system transaction.
const std::vector<std::string> ROOT_ONLY_PLUGINS{"expired-pgp-keys", "systemd-inhibit"};

// Prints one line per started download on stderr (metadata and packages).
class StderrDownloadCallbacks : public libdnf5::repo::DownloadCallbacks {
public:
    void * add_new_download(void *, const char * description, double) override {
        std::cerr << "  ↓ " << description << std::endl;
        return nullptr;
    }
    int end(void *, TransferStatus status, const char * msg) override {
        if (status == TransferStatus::ERROR && msg) {
            std::cerr << "  ✗ " << msg << std::endl;
        }
        return ReturnCode::OK;
    }
};

PackageInfo to_package_info(const libdnf5::rpm::Package & pkg) {
    PackageInfo info;
    info.name = pkg.get_name();
    info.nevra = pkg.get_nevra();
    info.evr = pkg.get_evr();
    info.arch = pkg.get_arch();
    info.repo_id = pkg.get_repo_id();
    info.summary = pkg.get_summary();
    info.description = pkg.get_description();
    info.url = pkg.get_url();
    info.license = pkg.get_license();
    info.download_size = pkg.get_download_size();
    info.install_size = pkg.get_install_size();
    info.installed = pkg.is_installed();
    return info;
}

}  // namespace

Session::Session(const OpenOptions & opts) {
    auto & cfg = base.get_config();

    // Standard chain: /etc/dnf/dnf.conf + /usr/share/dnf5/libdnf.conf.d + /etc/dnf/libdnf5.conf.d.
    base.load_config();

    // udnf default, applied over the system config but under the user layer.
    cfg.get_install_weak_deps_option().set(Priority::MAINCONFIG, false);

    // User layer: ~/.config/udnf/udnf.conf.
    const std::string user_config(opts.user_config);
    if (!user_config.empty() && fs::exists(user_config)) {
        libdnf5::ConfigParser parser;
        parser.read(user_config);
        if (parser.has_section("main")) {
            cfg.load_from_parser(parser, "main", *base.get_vars(), *base.get_logger(), Priority::AUTOMATICCONFIG);
        }
    }

    // Forced: everything a non-root user cannot write.
    cfg.get_installroot_option().set(Priority::RUNTIME, "/");
    cfg.get_cachedir_option().set(Priority::RUNTIME, std::string(opts.cachedir));
    cfg.get_system_cachedir_option().set(Priority::RUNTIME, std::string(opts.cachedir));
    cfg.get_persistdir_option().set(Priority::RUNTIME, std::string(opts.persistdir));
    cfg.get_logdir_option().set(Priority::RUNTIME, std::string(opts.logdir));
    if (opts.cacheonly) {
        cfg.get_cacheonly_option().set(Priority::RUNTIME, "all");
    }

    // Keep libdnf5's directories, append the user ones.
    auto reposdir = cfg.get_reposdir_option().get_value();
    reposdir.emplace_back(std::string(opts.user_reposdir));
    cfg.get_reposdir_option().set(Priority::RUNTIME, reposdir);
    auto varsdir = cfg.get_varsdir_option().get_value();
    varsdir.emplace_back(std::string(opts.user_varsdir));
    cfg.get_varsdir_option().set(Priority::RUNTIME, varsdir);

    // Keys live in a user keystore; rpm's default is the (root-owned) rpmdb.
    const std::string keyring(opts.keyring_dir);
    fs::create_directories(keyring);
    rpmPushMacro(nullptr, "_keyring", nullptr, "fs", RMIL_CMDLINE);
    rpmPushMacro(nullptr, "_keyringpath", nullptr, (keyring + "/").c_str(), RMIL_CMDLINE);
    // Importing a key takes rpm's transaction lock, by default in the read-only system rpmdb.
    const std::string lock = (fs::path(keyring).parent_path() / "rpm.lock").string();
    rpmPushMacro(nullptr, "_rpmlock_path", nullptr, lock.c_str(), RMIL_CMDLINE);

    base.enable_disable_plugins(ROOT_ONLY_PLUGINS, false);
    base.set_download_callbacks(std::make_unique<StderrDownloadCallbacks>());
    base.setup();

    // dnf repo files + /usr/share/dnf5/repos.override.d + /etc/dnf/repos.override.d.
    auto repo_sack = base.get_repo_sack();
    repo_sack->create_repos_from_system_configuration();
    apply_user_overrides(std::string(opts.user_overridedir));
    apply_cli_repo_selection(opts);
}

// Same semantics as the system override dirs: files sorted by name, sections are repo id
// globs, later values win. libdnf5 hardcodes its override dirs, so this layer is ours.
void Session::apply_user_overrides(const std::string & dir) {
    if (dir.empty() || !fs::is_directory(dir)) {
        return;
    }
    std::vector<fs::path> files;
    for (const auto & entry : fs::directory_iterator(dir)) {
        if (entry.is_regular_file() && entry.path().extension() == ".repo") {
            files.push_back(entry.path());
        }
    }
    std::sort(files.begin(), files.end());

    for (const auto & file : files) {
        libdnf5::ConfigParser parser;
        parser.read(file);
        for (const auto & [section, items] : parser.get_data()) {
            if (section.empty()) {
                continue;
            }
            libdnf5::repo::RepoQuery repos(base);
            repos.filter_id(section, libdnf5::sack::QueryCmp::GLOB);
            for (auto & repo : repos) {
                auto & binds = repo->get_config().opt_binds();
                for (const auto & [key, value] : items) {
                    if (key.empty() || key[0] == '#') {
                        continue;
                    }
                    auto bind = binds.find(key);
                    if (bind == binds.end()) {
                        std::cerr << "udnf: " << file.string() << ": option inconnue '" << key << "'" << std::endl;
                        continue;
                    }
                    bind->second.new_string(Priority::REPOCONFIG, value);
                    user_overrides[repo->get_id()].push_back(file.filename().string() + ": " + key + "=" + value);
                }
            }
        }
    }
}

void Session::apply_cli_repo_selection(const OpenOptions & opts) {
    if (!opts.only_repos.empty()) {
        libdnf5::repo::RepoQuery all(base);
        for (auto & repo : all) {
            repo->disable();
        }
        for (const auto & glob : opts.only_repos) {
            libdnf5::repo::RepoQuery repos(base);
            repos.filter_id(std::string(glob), libdnf5::sack::QueryCmp::GLOB);
            for (auto & repo : repos) {
                repo->enable();
            }
        }
    }
    for (const auto & glob : opts.disable_repos) {
        libdnf5::repo::RepoQuery repos(base);
        repos.filter_id(std::string(glob), libdnf5::sack::QueryCmp::GLOB);
        for (auto & repo : repos) {
            repo->disable();
        }
    }
    for (const auto & glob : opts.enable_repos) {
        libdnf5::repo::RepoQuery repos(base);
        repos.filter_id(std::string(glob), libdnf5::sack::QueryCmp::GLOB);
        for (auto & repo : repos) {
            repo->enable();
        }
    }
}

void Session::load_repos(bool available) {
    auto repo_sack = base.get_repo_sack();
    if (available) {
        repo_sack->load_repos();
    } else {
        repo_sack->load_repos(libdnf5::repo::Repo::Type::SYSTEM);
    }
}

rust::Vec<RepoInfo> Session::repolist(bool all) const {
    rust::Vec<RepoInfo> out;
    libdnf5::repo::RepoQuery repos(base);
    repos.filter_type(libdnf5::repo::Repo::Type::AVAILABLE);
    std::vector<libdnf5::repo::RepoWeakPtr> sorted(repos.begin(), repos.end());
    std::sort(sorted.begin(), sorted.end(), [](const auto & a, const auto & b) { return a->get_id() < b->get_id(); });
    for (auto & repo : sorted) {
        if (!all && !repo->is_enabled()) {
            continue;
        }
        auto & cfg = repo->get_config();
        RepoInfo info;
        info.id = repo->get_id();
        info.name = repo->get_name();
        info.enabled = repo->is_enabled();
        info.origin = repo->get_repo_file_path();
        for (const auto & url : cfg.get_baseurl_option().get_value()) {
            info.baseurl.push_back(url);
        }
        info.metalink = cfg.get_metalink_option().empty() ? "" : cfg.get_metalink_option().get_value();
        info.mirrorlist = cfg.get_mirrorlist_option().empty() ? "" : cfg.get_mirrorlist_option().get_value();
        info.pkg_gpgcheck = cfg.get_pkg_gpgcheck_option().get_value();
        auto it = user_overrides.find(info.id.operator std::string());
        if (it != user_overrides.end()) {
            for (const auto & o : it->second) {
                info.overrides.push_back(o);
            }
        }
        out.push_back(std::move(info));
    }
    return out;
}

Resolution Session::resolve(const ResolveRequest & req) {
    auto sack = base.get_rpm_package_sack();

    // Pin by exclusion: hide the other versions of a pinned name.arch, never force-install it,
    // so packages no longer needed still drop out of the solution.
    libdnf5::rpm::PackageSet excludes(base);
    for (const auto & pin : req.pins) {
        libdnf5::rpm::PackageQuery exact(base);
        exact.filter_available();
        exact.filter_nevra(std::string(pin));
        if (exact.empty()) {
            continue;
        }
        const auto pkg = *exact.begin();
        libdnf5::rpm::PackageQuery others(base);
        others.filter_available();
        others.filter_name(pkg.get_name());
        others.filter_arch(pkg.get_arch());
        others -= exact;
        excludes |= others;
    }
    sack->set_user_excludes(excludes);

    Resolution res;
    res.ok = true;

    libdnf5::ResolveSpecSettings spec_settings;
    spec_settings.set_with_nevra(true);
    spec_settings.set_with_provides(true);
    spec_settings.set_with_filenames(true);
    spec_settings.set_with_binaries(true);

    // `--repo` globs -> ids, for the specs restricted to them.
    std::vector<std::string> repo_ids;
    for (const auto & glob : req.repos) {
        libdnf5::repo::RepoQuery repos(base);
        repos.filter_enabled(true);
        repos.filter_id(std::string(glob), libdnf5::sack::QueryCmp::GLOB);
        if (repos.empty()) {
            throw std::runtime_error("aucun dépôt activé ne correspond à " + std::string(glob));
        }
        for (const auto & repo : repos) {
            repo_ids.push_back(repo->get_id());
        }
    }
    std::set<std::string> restricted;
    for (const auto & spec : req.restricted_specs) {
        restricted.insert(std::string(spec));
    }
    std::set<std::string> best;
    for (const auto & spec : req.best_specs) {
        best.insert(std::string(spec));
    }

    libdnf5::Goal goal(base);
    goal.set_allow_erasing(false);
    for (const auto & spec : req.specs) {
        const std::string s(spec);
        // A spec the system already satisfies needs nothing from us.
        libdnf5::rpm::PackageQuery installed(base);
        installed.filter_installed();
        if (installed.resolve_pkg_spec(s, spec_settings, false).first && !installed.empty()) {
            res.system_specs.push_back(s);
            continue;
        }
        libdnf5::GoalJobSettings job;
        if (restricted.contains(s)) {
            job.set_to_repo_ids(repo_ids);
        }
        if (best.contains(s)) {
            job.set_best(libdnf5::GoalSetting::SET_TRUE);
        }
        goal.add_install(s, job);
    }

    auto tx = goal.resolve();
    if (tx.get_problems() != libdnf5::GoalProblem::NO_PROBLEM) {
        res.ok = false;
    }
    for (const auto & msg : tx.get_resolve_logs_as_strings()) {
        res.problems.push_back(msg);
    }

    resolved.clear();
    for (const auto & tpkg : tx.get_transaction_packages()) {
        const auto pkg = tpkg.get_package();
        ResolvedPackage rp;
        rp.nevra = pkg.get_nevra();
        rp.name = pkg.get_name();
        rp.arch = pkg.get_arch();
        rp.evr = pkg.get_evr();
        rp.repo_id = pkg.get_repo_id();
        rp.download_size = pkg.get_download_size();
        rp.install_size = pkg.get_install_size();
        rp.action = libdnf5::transaction::transaction_item_action_to_string(tpkg.get_action());
        rp.reason = libdnf5::transaction::transaction_item_reason_to_string(tpkg.get_reason());
        rp.system = pkg.is_installed();
        resolved.emplace(pkg.get_nevra(), pkg);
        res.packages.push_back(std::move(rp));
    }
    return res;
}

rust::Vec<DownloadedPackage> Session::download(const rust::Vec<rust::String> & nevras, rust::Str dest_dir) {
    const std::string dest(dest_dir);
    fs::create_directories(dest);
    libdnf5::repo::PackageDownloader downloader(base);
    downloader.force_keep_packages(true);
    rust::Vec<DownloadedPackage> out;
    for (const auto & nevra : nevras) {
        auto it = resolved.find(std::string(nevra));
        if (it == resolved.end()) {
            throw std::runtime_error("paquet absent de la résolution: " + std::string(nevra));
        }
        const auto & pkg = it->second;
        downloader.add(pkg, dest);
        DownloadedPackage d;
        d.nevra = pkg.get_nevra();
        d.path = (fs::path(dest) / fs::path(pkg.get_location()).filename()).string();
        d.repo_id = pkg.get_repo_id();
        out.push_back(std::move(d));
    }
    downloader.download();
    return out;
}

SignatureCheck Session::check_signature(rust::Str path, rust::Str repo_id) {
    const std::string id(repo_id);
    SignatureCheck out{true, false, ""};
    libdnf5::repo::RepoQuery repos(base);
    repos.filter_id(id);
    if (repos.empty()) {
        return {false, false, "dépôt inconnu: " + id};
    }
    if (!(*repos.begin())->get_config().get_pkg_gpgcheck_option().get_value()) {
        return out;
    }
    libdnf5::rpm::RpmSignature sig(base);
    using Result = libdnf5::rpm::RpmSignature::CheckResult;
    const auto result = sig.check_package_signature(std::string(path));
    if (result == Result::OK || result == Result::SKIPPED) {
        return out;
    }
    out.ok = false;
    out.key_missing = result == Result::FAILED_KEY_MISSING;
    out.message = libdnf5::rpm::RpmSignature::check_result_to_string(result);
    return out;
}

const std::vector<libdnf5::rpm::KeyInfo> & Session::keys_of(const std::string & repo_id) {
    auto cached = repo_key_cache.find(repo_id);
    if (cached != repo_key_cache.end()) {
        return cached->second;
    }
    libdnf5::repo::RepoQuery repos(base);
    repos.filter_id(repo_id);
    if (repos.empty()) {
        throw std::runtime_error("dépôt inconnu: " + repo_id);
    }
    std::vector<libdnf5::rpm::KeyInfo> keys;
    libdnf5::rpm::RpmSignature sig(base);
    for (const auto & url : (*repos.begin())->get_config().get_gpgkey_option().get_value()) {
        for (auto & key : sig.parse_key_file(url)) {
            keys.push_back(std::move(key));
        }
    }
    return repo_key_cache.emplace(repo_id, std::move(keys)).first->second;
}

rust::Vec<KeyInfo> Session::repo_keys(rust::Str repo_id) {
    libdnf5::rpm::RpmSignature sig(base);
    rust::Vec<KeyInfo> out;
    for (const auto & key : keys_of(std::string(repo_id))) {
        KeyInfo info;
        info.key_id = key.get_key_id();
        info.fingerprint = key.get_fingerprint();
        for (const auto & uid : key.get_user_ids()) {
            info.user_ids.push_back(uid);
        }
        info.url = key.get_url();
        info.present = sig.key_present(key);
        out.push_back(std::move(info));
    }
    return out;
}

void Session::import_key(rust::Str repo_id, rust::Str fingerprint) {
    libdnf5::rpm::RpmSignature sig(base);
    for (const auto & key : keys_of(std::string(repo_id))) {
        if (key.get_fingerprint() == std::string(fingerprint)) {
            if (!sig.import_key(key)) {
                throw std::runtime_error("import de la clé " + key.get_short_key_id() + " échoué");
            }
            return;
        }
    }
    throw std::runtime_error("clé inconnue pour " + std::string(repo_id) + ": " + std::string(fingerprint));
}

rust::Vec<PackageInfo> Session::query(
    const rust::Vec<rust::String> & patterns, bool installed, bool available, bool latest) const {
    libdnf5::rpm::PackageQuery q(base);
    if (installed && !available) {
        q.filter_installed();
    } else if (available && !installed) {
        q.filter_available();
    }
    if (!patterns.empty()) {
        libdnf5::rpm::PackageQuery matched(base, libdnf5::sack::ExcludeFlags::APPLY_EXCLUDES, true);
        libdnf5::ResolveSpecSettings settings;
        settings.set_with_provides(false);
        settings.set_with_filenames(false);
        settings.set_with_binaries(false);
        for (const auto & p : patterns) {
            libdnf5::rpm::PackageQuery one(q);
            one.resolve_pkg_spec(std::string(p), settings, false);
            matched |= one;
        }
        q = matched;
    }
    if (latest) {
        q.filter_latest_evr();
    }
    rust::Vec<PackageInfo> out;
    for (const auto & pkg : q) {
        out.push_back(to_package_info(pkg));
    }
    return out;
}

rust::Vec<PackageInfo> Session::search(const rust::Vec<rust::String> & terms) const {
    libdnf5::rpm::PackageQuery base_q(base);
    base_q.filter_available();
    base_q.filter_latest_evr();
    libdnf5::rpm::PackageQuery result(base, libdnf5::sack::ExcludeFlags::APPLY_EXCLUDES, true);
    for (const auto & t : terms) {
        const std::string term(t);
        libdnf5::rpm::PackageQuery by_name(base_q);
        by_name.filter_name(term, libdnf5::sack::QueryCmp::ICONTAINS);
        libdnf5::rpm::PackageQuery by_summary(base_q);
        by_summary.filter_summary(term, libdnf5::sack::QueryCmp::ICONTAINS);
        result |= by_name;
        result |= by_summary;
    }
    rust::Vec<PackageInfo> out;
    for (const auto & pkg : result) {
        out.push_back(to_package_info(pkg));
    }
    return out;
}

std::unique_ptr<Session> open_session(const OpenOptions & opts) {
    return std::make_unique<Session>(opts);
}

// ---------------------------------------------------------------------------------------------
// RPM payload handling (librpm)
// ---------------------------------------------------------------------------------------------

namespace {

struct RpmHandle {
    FD_t fd = nullptr;
    rpmts ts = nullptr;
    Header h = nullptr;
    rpmfiles files = nullptr;

    explicit RpmHandle(const std::string & path) {
        fd = Fopen(path.c_str(), "r.ufdio");
        if (!fd || Ferror(fd)) {
            throw std::runtime_error("impossible d'ouvrir " + path);
        }
        ts = rpmtsCreate();
        // Signatures are checked separately (Session::check_signature); keep digests for integrity.
        rpmtsSetVSFlags(ts, RPMVSF_MASK_NOSIGNATURES);
        rpmRC rc = rpmReadPackageFile(ts, fd, path.c_str(), &h);
        if (rc != RPMRC_OK && rc != RPMRC_NOTTRUSTED && rc != RPMRC_NOKEY) {
            throw std::runtime_error("paquet RPM invalide: " + path);
        }
        files = rpmfilesNew(nullptr, h, 0, RPMFI_KEEPHEADER);
    }
    ~RpmHandle() {
        if (files) rpmfilesFree(files);
        if (h) headerFree(h);
        if (ts) rpmtsFree(ts);
        if (fd) Fclose(fd);
    }
    RpmHandle(const RpmHandle &) = delete;
    RpmHandle & operator=(const RpmHandle &) = delete;
};

std::string hex(const unsigned char * data, size_t len) {
    static const char * digits = "0123456789abcdef";
    std::string out;
    out.reserve(len * 2);
    for (size_t i = 0; i < len; ++i) {
        out.push_back(digits[data[i] >> 4]);
        out.push_back(digits[data[i] & 0xf]);
    }
    return out;
}

RpmContents contents_of(RpmHandle & rpm) {
    RpmContents out;
    char * nevra = headerGetAsString(rpm.h, RPMTAG_NEVRA);
    out.nevra = nevra ? nevra : "";
    free(nevra);

    const int count = static_cast<int>(rpmfilesFC(rpm.files));
    for (int ix = 0; ix < count; ++ix) {
        FileEntry e;
        char * fn = rpmfilesFN(rpm.files, ix);
        e.path = fn;
        free(fn);
        const auto mode = rpmfilesFMode(rpm.files, ix);
        e.mode = mode;
        if (S_ISREG(mode)) {
            e.kind = "file";
        } else if (S_ISDIR(mode)) {
            e.kind = "dir";
        } else if (S_ISLNK(mode)) {
            e.kind = "symlink";
            const char * link = rpmfilesFLink(rpm.files, ix);
            e.symlink_target = link ? link : "";
        } else {
            e.kind = "other";
        }
        int algo = 0;
        size_t len = 0;
        const unsigned char * digest = rpmfilesFDigest(rpm.files, ix, &algo, &len);
        if (digest && len) {
            e.digest = hex(digest, len);
        }
        e.size = rpmfilesFSize(rpm.files, ix);
        const auto flags = rpmfilesFFlags(rpm.files, ix);
        e.config = flags & RPMFILE_CONFIG;
        e.ghost = flags & RPMFILE_GHOST;
        e.doc = flags & (RPMFILE_DOC | RPMFILE_LICENSE | RPMFILE_README);
        out.files.push_back(std::move(e));
    }

    // Script body tag, interpreter tag (`%post -p /sbin/ldconfig` only sets the latter), name.
    const struct {
        rpmTagVal script;
        rpmTagVal prog;
        const char * name;
    } scriptlets[] = {
        {RPMTAG_PRETRANS, RPMTAG_PRETRANSPROG, "%pretrans"},
        {RPMTAG_PREIN, RPMTAG_PREINPROG, "%pre"},
        {RPMTAG_POSTIN, RPMTAG_POSTINPROG, "%post"},
        {RPMTAG_PREUN, RPMTAG_PREUNPROG, "%preun"},
        {RPMTAG_POSTUN, RPMTAG_POSTUNPROG, "%postun"},
        {RPMTAG_POSTTRANS, RPMTAG_POSTTRANSPROG, "%posttrans"},
        {RPMTAG_PREUNTRANS, RPMTAG_PREUNTRANSPROG, "%preuntrans"},
        {RPMTAG_POSTUNTRANS, RPMTAG_POSTUNTRANSPROG, "%postuntrans"},
        {RPMTAG_TRIGGERSCRIPTS, RPMTAG_TRIGGERSCRIPTPROG, "%trigger"},
        {RPMTAG_FILETRIGGERSCRIPTS, RPMTAG_FILETRIGGERSCRIPTPROG, "%filetrigger"},
        {RPMTAG_TRANSFILETRIGGERSCRIPTS, RPMTAG_TRANSFILETRIGGERSCRIPTPROG, "%transfiletrigger"},
    };
    for (const auto & s : scriptlets) {
        if (headerIsEntry(rpm.h, s.script) || headerIsEntry(rpm.h, s.prog)) {
            out.scriptlets.push_back(s.name);
        }
    }
    return out;
}

// Owner always gets rwx on dirs / rw on files so udnf can update and remove them later;
// setuid/setgid/sticky are dropped (no privilege in userland).
mode_t user_mode(mode_t mode) {
    mode_t perm = mode & 0777;
    perm |= S_ISDIR(mode) ? 0700 : 0600;
    return perm;
}

void mkdirs(const fs::path & dir) {
    std::error_code ec;
    fs::create_directories(dir, ec);
    if (ec) {
        throw std::runtime_error("mkdir " + dir.string() + ": " + ec.message());
    }
}

void set_mtime(const fs::path & path, time_t mtime, bool nofollow) {
    struct timespec times[2];
    times[0].tv_sec = mtime;
    times[0].tv_nsec = 0;
    times[1] = times[0];
    utimensat(AT_FDCWD, path.c_str(), times, nofollow ? AT_SYMLINK_NOFOLLOW : 0);
}

}  // namespace

RpmContents read_rpm(rust::Str path) {
    RpmHandle rpm{std::string(path)};
    return contents_of(rpm);
}

RpmContents extract_rpm(rust::Str path, rust::Str dest_root, bool skip_docs) {
    const std::string rpm_path(path);
    const fs::path root(std::string{dest_root});
    RpmHandle rpm{rpm_path};
    RpmContents out = contents_of(rpm);

    const char * compr = headerGetString(rpm.h, RPMTAG_PAYLOADCOMPRESSOR);
    const std::string io_flags = std::string("r.") + (compr ? compr : "gzip");
    FD_t payload = Fdopen(fdDup(Fileno(rpm.fd)), io_flags.c_str());
    if (!payload || Ferror(payload)) {
        throw std::runtime_error("payload illisible: " + rpm_path);
    }

    rpmfi fi = rpmfiNewArchiveReader(payload, rpm.files, RPMFI_ITER_READ_ARCHIVE_CONTENT_FIRST);
    // Directories get their final mode last, so read-only dirs don't block their own content.
    std::vector<std::pair<fs::path, mode_t>> dirs;
    // Members of this package whose content was written in this run (file index -> path).
    // Hardlink siblings must copy from here, never from a file left by a previous version.
    std::map<int, fs::path> written;
    std::string error;
    int ix;
    while ((ix = rpmfiNext(fi)) >= 0) {
        const auto flags = rpmfilesFFlags(rpm.files, ix);
        if (flags & RPMFILE_GHOST) {
            continue;
        }
        if (skip_docs && (flags & RPMFILE_DOC)) {
            continue;
        }
        char * fn = rpmfilesFN(rpm.files, ix);
        const fs::path dest = root / fs::path(fn).relative_path();
        free(fn);
        const mode_t mode = rpmfilesFMode(rpm.files, ix);
        const time_t mtime = rpmfilesFMtime(rpm.files, ix);
        mkdirs(dest.parent_path());

        if (S_ISDIR(mode)) {
            mkdirs(dest);
            chmod(dest.c_str(), 0700);
            dirs.emplace_back(dest, user_mode(mode));
            continue;
        }

        // Write to a temp name then rename: replaces running binaries safely on update.
        const fs::path tmp = dest.parent_path() / (".udnf-tmp." + dest.filename().string());
        std::error_code ec;
        fs::remove(tmp, ec);

        if (S_ISLNK(mode)) {
            if (symlink(rpmfilesFLink(rpm.files, ix), tmp.c_str()) != 0) {
                error = "symlink " + dest.string() + ": " + std::strerror(errno);
                break;
            }
            set_mtime(tmp, mtime, true);
        } else if (S_ISREG(mode)) {
            if (rpmfiArchiveHasContent(fi)) {
                FD_t outfd = Fopen(tmp.c_str(), "w.ufdio");
                if (!outfd || Ferror(outfd)) {
                    error = "écriture " + dest.string() + " impossible";
                    break;
                }
                int rc = rpmfiArchiveReadToFile(fi, outfd, 0);
                Fclose(outfd);
                if (rc) {
                    error = "extraction de " + dest.string() + " échouée (rc=" + std::to_string(rc) + ")";
                    break;
                }
            } else {
                // Hardlink set: the content came with the member extracted first
                // (RPMFI_ITER_READ_ARCHIVE_CONTENT_FIRST).
                const int * links = nullptr;
                const uint32_t nlinks = rpmfilesFLinks(rpm.files, ix, &links);
                bool done = false;
                for (uint32_t i = 0; i < nlinks && !done; ++i) {
                    auto src = written.find(links[i]);
                    if (links[i] == ix || src == written.end()) continue;
                    fs::copy_file(src->second, tmp, fs::copy_options::overwrite_existing, ec);
                    done = !ec;
                }
                if (!done) {
                    error = "lien dur sans contenu: " + dest.string();
                    break;
                }
            }
            chmod(tmp.c_str(), user_mode(mode));
            set_mtime(tmp, mtime, false);
        } else {
            // Device nodes, fifos, sockets: meaningless without root.
            continue;
        }

        if (fs::is_directory(fs::symlink_status(dest))) {
            fs::remove(tmp, ec);
            error = "conflit: " + dest.string() + " est un répertoire";
            break;
        }
        fs::rename(tmp, dest, ec);
        if (ec) {
            error = "rename " + dest.string() + ": " + ec.message();
            break;
        }
        if (S_ISREG(mode)) {
            written[ix] = dest;
        }
    }
    if (error.empty() && ix != RPMERR_ITER_END) {
        error = "archive corrompue: " + rpm_path + " (rc=" + std::to_string(ix) + ")";
    }
    rpmfiFree(fi);
    Fclose(payload);

    for (auto it = dirs.rbegin(); it != dirs.rend(); ++it) {
        chmod(it->first.c_str(), it->second);
    }
    if (!error.empty()) {
        throw std::runtime_error(error);
    }
    return out;
}

}  // namespace udnf
