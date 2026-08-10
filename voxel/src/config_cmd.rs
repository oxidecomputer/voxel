//! `voxel config` - show / get / set / load the `voxel.toml`.

use anyhow::{Context, anyhow};
use camino::Utf8Path;
use std::fs;
use voxel_config::{VoxelConfig, config as vcfg};

use crate::{ConfigCmd, config_text, load_config};

pub(crate) fn cmd_config(
    path: &Utf8Path,
    cmd: &ConfigCmd,
) -> anyhow::Result<()> {
    match cmd {
        ConfigCmd::Show => {
            print!("{}", load_config(path)?.to_toml());
        }
        ConfigCmd::Get { key } => {
            let text = config_text(path)?;
            match vcfg::get(&text, key).map_err(|e| anyhow!(e))? {
                Some(v) => println!("{v}"),
                None => return Err(anyhow!("no such key: {key}")),
            }
        }
        ConfigCmd::Set { key, value } => {
            // Seed the file with defaults if it doesn't exist yet, so edits stick.
            let text = config_text(path)?;
            let updated =
                vcfg::set(&text, key, value).map_err(|e| anyhow!(e))?;
            ensure_parent_dir(path)?;
            fs::write(path, &updated)
                .with_context(|| format!("write {}", path))?;
            println!("{key} = {value}");
        }
        ConfigCmd::Load { file } => {
            let text = fs::read_to_string(file)
                .with_context(|| format!("read {}", file))?;
            VoxelConfig::from_toml(&text)
                .map_err(|e| anyhow!("invalid config {}: {e}", file))?;
            ensure_parent_dir(path)?;
            fs::write(path, &text)
                .with_context(|| format!("write {}", path))?;
            println!("loaded {} -> {}", file, path);
        }
    }
    Ok(())
}

/// Create the config file's parent directory if needed - the default lives at
/// `~/.config/voxel/voxel.toml`, whose directory may not exist on a fresh box.
fn ensure_parent_dir(path: &Utf8Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir))?;
    }
    Ok(())
}
