// Thin C++ layer between udnf (Rust) and libdnf5/librpm.
// Everything here is plain data in, plain data out: Rust never sees libdnf5 types.

#pragma once

#include "rust/cxx.h"

#include <libdnf5/base/base.hpp>
#include <libdnf5/rpm/package.hpp>
#include <libdnf5/rpm/rpm_signature.hpp>

#include <map>
#include <memory>
#include <string>
#include <vector>

namespace udnf {

struct OpenOptions;
struct RepoInfo;
struct ResolveRequest;
struct Resolution;
struct PackageInfo;
struct DownloadedPackage;
struct SignatureCheck;
struct KeyInfo;
struct RpmContents;

class Session {
public:
    explicit Session(const OpenOptions & opts);

    void load_repos(bool available);
    rust::Vec<RepoInfo> repolist(bool all) const;
    Resolution resolve(const ResolveRequest & req);
    rust::Vec<DownloadedPackage> download(const rust::Vec<rust::String> & nevras, rust::Str dest_dir);
    SignatureCheck check_signature(rust::Str path, rust::Str repo_id);
    rust::Vec<KeyInfo> repo_keys(rust::Str repo_id);
    void import_key(rust::Str repo_id, rust::Str fingerprint);
    rust::Vec<PackageInfo> query(
        const rust::Vec<rust::String> & patterns, bool installed, bool available, bool latest) const;
    rust::Vec<PackageInfo> search(const rust::Vec<rust::String> & terms) const;

private:
    void apply_user_overrides(const std::string & dir);
    void apply_cli_repo_selection(const OpenOptions & opts);
    const std::vector<libdnf5::rpm::KeyInfo> & keys_of(const std::string & repo_id);

    // `mutable`: libdnf5 queries take a non-const Base even for read-only work.
    mutable libdnf5::Base base;
    // Packages of the last resolution, by NEVRA, for download().
    std::map<std::string, libdnf5::rpm::Package> resolved;
    // repo id -> "file: key=value" overrides applied from the user layer (for repolist -v).
    std::map<std::string, std::vector<std::string>> user_overrides;
    // repo id -> keys parsed from its gpgkey URLs (downloaded once per run).
    std::map<std::string, std::vector<libdnf5::rpm::KeyInfo>> repo_key_cache;
};

std::unique_ptr<Session> open_session(const OpenOptions & opts);

RpmContents read_rpm(rust::Str path);
RpmContents extract_rpm(rust::Str path, rust::Str dest_root, bool skip_docs);

}  // namespace udnf
