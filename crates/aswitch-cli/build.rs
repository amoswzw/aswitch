use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let assets_dir = manifest_dir.join("../../assets/bundled-plugins");
    println!("cargo:rerun-if-changed={}", assets_dir.display());

    let mut entries = fs::read_dir(&assets_dir)
        .expect("read bundled plugins")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    let mut generated = String::from("pub const BUNDLED_PLUGINS: &[(&str, &str)] = &[\n");
    for entry in entries {
        let id = entry.file_name().to_string_lossy().to_string();
        let manifest_path = entry.path().join("plugin.toml");
        let contents = fs::read_to_string(&manifest_path).expect("read plugin manifest");
        generated.push_str(&format!("    ({id:?}, {contents:?}),\n",));
    }
    generated.push_str("];\n");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    fs::write(out_dir.join("bundled_plugins.rs"), generated).expect("write bundled plugins");
}
