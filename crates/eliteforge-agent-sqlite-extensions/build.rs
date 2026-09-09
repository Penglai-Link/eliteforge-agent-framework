use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_BUNDLED_NATIVE");
    if env::var_os("CARGO_FEATURE_BUNDLED_NATIVE").is_none() {
        return;
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("resources");
    println!("cargo:rerun-if-changed=resources/SHA256SUMS");
    let manifest = fs::read_to_string(root.join("SHA256SUMS")).expect("native resource checksums");
    for line in manifest.lines() {
        let (expected, name) = line.split_once("  ").expect("SHA256SUMS entry");
        println!("cargo:rerun-if-changed=resources/{name}");
        let bytes = fs::read(root.join(name)).unwrap_or_else(|e| panic!("missing {name}: {e}"));
        assert!(
            !bytes.starts_with(b"version https://git-lfs.github.com/spec/v1"),
            "{name} is a Git LFS pointer, not a native resource"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            expected,
            "resource checksum mismatch: {name}"
        );
    }
}
