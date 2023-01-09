extern crate cc;

fn main() {
    if cfg!(target_os = "macos") {
        println!("cargo:rerun-if-changed=objc/pmstub.m");
        cc::Build::new()
            .file("objc/pmstub.m")
            .flag("-fmodules")
            .warnings(false)
            .compile("pmstub");
    }
}
