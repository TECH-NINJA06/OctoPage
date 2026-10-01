use std::path::Path;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it");
    let dir = Path::new(&dir);
    // For the tests, which compile a C program for the same target.
    for (name, var) in [("OCTOPAGE_TARGET", "TARGET"), ("OCTOPAGE_HOST", "HOST")] {
        println!(
            "cargo:rustc-env={name}={}",
            std::env::var(var).expect("cargo sets it")
        );
    }
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");
    let config = cbindgen::Config::from_file(dir.join("cbindgen.toml")).expect("cbindgen.toml");
    let bindings = cbindgen::Builder::new()
        .with_config(config)
        .with_src(dir.join("src").join("lib.rs"))
        .generate()
        .expect("the C ABI generates a header");
    // Written only when it changes, so the header's timestamp says when the ABI last changed.
    bindings.write_to_file(dir.join("include").join("octopage.h"));
}
