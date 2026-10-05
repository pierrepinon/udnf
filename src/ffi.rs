//! Bridge to the C++ shim (`shim/udnf.cpp`), the only code touching libdnf5/librpm.

#[cxx::bridge(namespace = "udnf")]
pub mod bridge {
    /// Paths and overrides applied on top of the standard libdnf5 configuration.
    pub struct OpenOptions {
        pub cachedir: String,
        pub persistdir: String,
        pub logdir: String,
        /// `~/.config/udnf/udnf.conf`, loaded after `/etc/dnf/dnf.conf` and its drop-ins.
        pub user_config: String,
        /// Appended to the default `reposdir`.
        pub user_reposdir: String,
        /// Applied after `/usr/share/dnf5/repos.override.d` and `/etc/dnf/repos.override.d`.
        pub user_overridedir: String,
        /// Appended to the default `varsdir`.
        pub user_varsdir: String,
        /// Repo id globs from `--enablerepo` / `--disablerepo` / `--repo`.
        pub enable_repos: Vec<String>,
        pub disable_repos: Vec<String>,
        pub only_repos: Vec<String>,
        pub cacheonly: bool,
        /// User keystore (`fs` backend of rpm): the system rpmdb holds no keys on ostree.
        pub keyring_dir: String,
    }

    pub struct RepoInfo {
        pub id: String,
        pub name: String,
        pub enabled: bool,
        pub origin: String,
        pub baseurl: Vec<String>,
        pub metalink: String,
        pub mirrorlist: String,
        pub pkg_gpgcheck: bool,
        pub overrides: Vec<String>,
    }

    pub struct ResolveRequest {
        /// Every spec of the world set.
        pub specs: Vec<String>,
        /// NEVRAs to keep: other versions of the same name.arch are excluded when the exact one exists.
        pub pins: Vec<String>,
        /// Specs that may only come from `repos` (`udnf install --repo X spec`); the rest of the
        /// world is still resolved against every enabled repo.
        pub restricted_specs: Vec<String>,
        /// Repo id globs for `restricted_specs`.
        pub repos: Vec<String>,
        /// Specs that must resolve to their best (latest) version (`udnf upgrade <names>`).
        pub best_specs: Vec<String>,
    }

    pub struct ResolvedPackage {
        pub nevra: String,
        pub name: String,
        pub arch: String,
        pub evr: String,
        pub repo_id: String,
        pub download_size: u64,
        pub install_size: u64,
        pub action: String,
        pub reason: String,
        /// The package comes from the system rpmdb.
        pub system: bool,
    }

    pub struct Resolution {
        pub ok: bool,
        pub problems: Vec<String>,
        pub packages: Vec<ResolvedPackage>,
        /// World specs already provided by the system rpmdb.
        pub system_specs: Vec<String>,
    }

    pub struct PackageInfo {
        pub name: String,
        pub nevra: String,
        pub evr: String,
        pub arch: String,
        pub repo_id: String,
        pub summary: String,
        pub description: String,
        pub url: String,
        pub license: String,
        pub download_size: u64,
        pub install_size: u64,
        pub installed: bool,
    }

    pub struct SignatureCheck {
        pub ok: bool,
        /// The repo key is not in the user keystore yet: import it then check again.
        pub key_missing: bool,
        pub message: String,
    }

    /// A key listed in a repo's `gpgkey`.
    pub struct KeyInfo {
        pub key_id: String,
        pub fingerprint: String,
        pub user_ids: Vec<String>,
        pub url: String,
        /// Already in the user keystore.
        pub present: bool,
    }

    pub struct DownloadedPackage {
        pub nevra: String,
        pub path: String,
        pub repo_id: String,
    }

    /// One entry of an RPM file list.
    pub struct FileEntry {
        /// Absolute path inside the package (`/usr/bin/foo`).
        pub path: String,
        pub mode: u32,
        /// One of `file`, `dir`, `symlink`, `other`.
        pub kind: String,
        pub symlink_target: String,
        pub digest: String,
        pub size: u64,
        pub config: bool,
        pub ghost: bool,
        pub doc: bool,
    }

    pub struct RpmContents {
        pub nevra: String,
        pub files: Vec<FileEntry>,
        /// Names of scriptlets/triggers present in the header: such packages are refused.
        pub scriptlets: Vec<String>,
    }

    unsafe extern "C++" {
        include!("udnf/shim/udnf.hpp");

        type Session;

        fn open_session(opts: &OpenOptions) -> Result<UniquePtr<Session>>;
        fn load_repos(self: Pin<&mut Session>, available: bool) -> Result<()>;
        fn repolist(self: &Session, all: bool) -> Vec<RepoInfo>;
        fn resolve(self: Pin<&mut Session>, req: &ResolveRequest) -> Result<Resolution>;
        fn download(self: Pin<&mut Session>, nevras: &Vec<String>, dest_dir: &str) -> Result<Vec<DownloadedPackage>>;
        fn check_signature(self: Pin<&mut Session>, path: &str, repo_id: &str) -> Result<SignatureCheck>;
        fn repo_keys(self: Pin<&mut Session>, repo_id: &str) -> Result<Vec<KeyInfo>>;
        fn import_key(self: Pin<&mut Session>, repo_id: &str, fingerprint: &str) -> Result<()>;
        fn query(
            self: &Session,
            patterns: &Vec<String>,
            installed: bool,
            available: bool,
            latest: bool,
        ) -> Result<Vec<PackageInfo>>;
        fn search(self: &Session, terms: &Vec<String>) -> Result<Vec<PackageInfo>>;

        fn read_rpm(path: &str) -> Result<RpmContents>;
        /// Extracts the payload under `dest_root`; returns the package contents.
        fn extract_rpm(path: &str, dest_root: &str, skip_docs: bool) -> Result<RpmContents>;
    }
}
