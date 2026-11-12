fn main() {
    // khronos-egl is used with `no-pkg-config` (pkg-config cannot see the NDK
    // sysroot), so it emits no link directive: link Android's libEGL here, or
    // the library loads with unresolved egl* symbols.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=EGL");
    }
}
