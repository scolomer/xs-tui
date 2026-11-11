//! Compile le plugin zellij (`plugin/`) en wasm pour l'embarquer dans xst (SPEC.md § Plugin
//! zellij), et publie l'empreinte de son contenu : xst l'écrit sous un nom qui en dépend, car
//! zellij garde en mémoire, par chemin, le code d'un plugin déjà chargé.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=plugin/src");
    println!("cargo:rerun-if-changed=plugin/Cargo.toml");
    println!("cargo:rerun-if-changed=plugin/Cargo.lock");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target_dir = out.join("plugin-target");
    let mut cargo = Command::new(env::var_os("CARGO").expect("CARGO"));
    cargo
        .args(["build", "--release", "--locked", "--target", "wasm32-wasip1", "--manifest-path", "plugin/Cargo.toml"])
        .arg("--target-dir")
        .arg(&target_dir);
    // Réglages de la compilation d'xst (clippy, drapeaux de l'hôte) : pas pour le plugin.
    for var in ["RUSTC_WORKSPACE_WRAPPER", "RUSTC_WRAPPER", "CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS", "CARGO_TARGET_DIR"] {
        cargo.env_remove(var);
    }
    let status = cargo.status().expect("cargo pour le plugin");
    assert!(status.success(), "compilation du plugin zellij : `rustup target add wasm32-wasip1` ?");
    let wasm = target_dir.join("wasm32-wasip1/release/xs-tui-zellij.wasm");
    let bytes = std::fs::read(&wasm).expect("plugin wasm");
    std::fs::write(out.join("xs-tui.wasm"), &bytes).expect("copie du plugin");
    println!("cargo:rustc-env=XS_TUI_PLUGIN_HASH={:016x}", fnv1a(&bytes));
}

/// FNV-1a 64 bits : un nom de fichier par version du plugin, sans dépendance.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}
