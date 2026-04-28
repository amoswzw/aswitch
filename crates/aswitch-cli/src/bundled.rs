use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use aswitch_core::paths::AswitchPaths;

include!(concat!(env!("OUT_DIR"), "/bundled_plugins.rs"));

pub fn ensure_bundled_plugins(paths: &AswitchPaths) -> Result<()> {
    for (plugin_id, manifest) in BUNDLED_PLUGINS {
        let plugin_dir = paths.plugins_dir.join(plugin_id);
        let manifest_path = plugin_dir.join("plugin.toml");
        if manifest_path.is_file() {
            continue;
        }

        fs::create_dir_all(&plugin_dir)?;
        write_file(&manifest_path, manifest.as_bytes())?;
    }

    Ok(())
}

fn write_file(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    Ok(())
}
