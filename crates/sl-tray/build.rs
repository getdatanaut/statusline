//! Embed `packaging/Info.plist` into the binary's `__TEXT,__info_plist`
//! section, so a bare Mach-O (no .app bundle) still has a bundle identifier.
//! `UNUserNotificationCenter` aborts the process without one, and with one it
//! files the tray's notifications under its own name in System Settings.

#[expect(clippy::expect_used, reason = "a build script fails the build loudly")]
fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let plist = format!("{manifest_dir}/packaging/Info.plist");
    println!("cargo:rerun-if-changed={plist}");
    // `-bins` only: test harnesses do not need a second copy of the plist.
    println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{plist}");
}
