//! Build script: records the target triple so `sentinel version` can report
//! which architecture-specific binary this is (SPEC.md §41).

fn main() {
    println!(
        "cargo:rustc-env=SENTINEL_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".into())
    );
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=migrations");
}
