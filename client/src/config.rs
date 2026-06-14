use std::collections::HashMap;
use std::path::PathBuf;

/// Load keybinding overrides from a `keybindings.toml` file if it exists.
/// Checks `~/.config/mote/` first, then project root.
pub fn load_keybindings()
-> Option<HashMap<String, crate::tui::keybinding::KeyList>> {
    let path = resolve_config_path("keybindings.toml");
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()?
}

/// Resolve a config file path: check `~/.config/mote/` first, fall back to CWD.
fn resolve_config_path(filename: &str) -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        let p = home.join(".config").join("mote").join(filename);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(filename)
}
