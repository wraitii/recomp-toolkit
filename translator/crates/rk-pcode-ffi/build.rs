fn main() {
    cxx_build::CFG.include_prefix = "rk_pcode";
    let _ = cxx_build::bridge("src/baseline/abi.rs");
    println!("cargo:rerun-if-changed=src/baseline/abi.rs");
}
