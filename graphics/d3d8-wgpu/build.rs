use std::path::Path;

fn fingerprint(path: &Path, root: &Path, hash: &mut u64) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for entry in entries {
            fingerprint(&entry, root, hash);
        }
    } else {
        let name = path.strip_prefix(root).unwrap().to_string_lossy();
        let contents = std::fs::read(path).unwrap();
        for byte in name
            .bytes()
            .chain([0])
            .chain((contents.len() as u64).to_le_bytes())
            .chain(contents)
        {
            *hash = (*hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
}

fn main() {
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut hash = 0xcbf29ce484222325;
    for input in ["src", "build.rs", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={input}");
        fingerprint(&root.join(input), &root, &mut hash);
    }
    println!(
        "cargo:rustc-env=RECOMP_RENDERER_BUILD={hash:016x}-{}-{}",
        std::env::var("TARGET").unwrap(),
        std::env::var("PROFILE").unwrap()
    );
    cxx_build::bridge("src/bridge.rs").compile("d3d8-wgpu-cxx");
}
