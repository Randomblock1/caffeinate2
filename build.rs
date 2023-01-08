extern crate cc;

fn main() {
    if cfg!(target_os = "macos") {
        println!("cargo:rerun-if-changed=objc/caffeinate2.m");
        cc::Build::new()
            .file("objc/caffeinate2.m")
            .flag("-fmodules")
            .warnings(false)
            .compile("caffeinate2");
    }
}
