//! Sets `cfg(gk_native)` on targets whose kernel provides `eventfd(2)`.
//! There `GkEventFd` is the real kernel object; everywhere else it is a shim.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(gk_native)");
    println!("cargo::rerun-if-changed=build.rs");

    // Linux 2.6.22+, Android, FreeBSD 13+, NetBSD 10+, illumos.
    const NATIVE: &[&str] = &["linux", "android", "freebsd", "netbsd", "illumos"];
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let force_shim = std::env::var_os("CARGO_FEATURE_FORCE_SHIM").is_some();
    if NATIVE.contains(&os.as_str()) && !force_shim {
        println!("cargo::rustc-cfg=gk_native");
    }
}
