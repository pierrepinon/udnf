fn main() {
    // Only the include paths come from pkg-config: libdnf5.pc lists its private
    // dependencies (libsolv, librepo, glib...) which we never call directly.
    let dnf = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libdnf5")
        .expect("libdnf5-devel is required (build inside the toolbox)");
    let rpm = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("rpm")
        .expect("rpm-devel is required (build inside the toolbox)");

    cxx_build::bridge("src/ffi.rs")
        .file("shim/udnf.cpp")
        .includes(dnf.include_paths.iter().chain(rpm.include_paths.iter()))
        .std("c++20")
        .flag_if_supported("-Wno-deprecated-declarations")
        // Spurious warnings in cxx-generated code.
        .flag_if_supported("-Wno-maybe-uninitialized")
        .compile("udnf-shim");

    // fmt: libdnf5 headers instantiate fmt templates inline.
    for lib in ["dnf5", "rpm", "rpmio", "fmt"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    println!("cargo:rerun-if-changed=shim/udnf.cpp");
    println!("cargo:rerun-if-changed=shim/udnf.hpp");
    println!("cargo:rerun-if-changed=src/ffi.rs");
}
