fn main() {
    println!("cargo::rustc-check-cfg=cfg(slirp)");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    match pkg_config::Config::new().atleast_version("4.9").cargo_metadata(false).probe("slirp") {
        Ok(lib) => {
            for dir in &lib.link_paths {
                println!("cargo::rustc-link-search=native={}", dir.display());
            }
            println!("cargo::rustc-link-lib=dylib=slirp");
            println!("cargo::rustc-cfg=slirp");
        }
        Err(_) => println!("cargo::warning=pkg-config found no libslirp, so `--net user` is unavailable; build inside `nix develop` for it"),
    }
}
