//! Copies the Steam runtime DLLs next to the built exe (target/<profile>/).
//! Source: env STEAM_DLL_DIR, default the known-good Porthole set.

use std::path::{Path, PathBuf};

const DLLS: [&str; 4] = ["steam_api64.dll", "steamclient64.dll", "tier0_s64.dll", "vstdlib_s64.dll"];

fn main() {
    println!("cargo:rerun-if-env-changed=STEAM_DLL_DIR");
    let src = std::env::var("STEAM_DLL_DIR")
        .unwrap_or_else(|_| r"D:\Steam\steamapps\common\porthole".to_string());
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dst = out.ancestors().nth(3).expect("profile dir");
    for d in DLLS {
        let from = Path::new(&src).join(d);
        println!("cargo:rerun-if-changed={}", from.display());
        if let Err(e) = std::fs::copy(&from, dst.join(d)) {
            println!("cargo:warning=copy {} failed: {e}", from.display());
        }
    }
}
