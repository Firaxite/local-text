// Per-project LSP configuration, loaded from `<root>/.vscode/local-text.json`.
//
// Schema (every key optional), keyed by the server *binary name* — the same
// string returned by `lsp_binary(lang)` (e.g. "rust-analyzer"):
//
//   {
//     "rust-analyzer": {
//       "env":     { "CARGO_TARGET_DIR": "/tmp/ra-target", "RUSTUP_TOOLCHAIN": "nightly" },
//       "command": "/custom/path/to/rust-analyzer",
//       "args":    ["--stdio"]
//     }
//   }
//
// `env` is merged over the inherited environment; `command`/`args` override the
// hardcoded defaults. Applies to both local and remote (SSH) servers.

use std::collections::HashMap;

use serde::Deserialize;

use crate::lsp::LspLaunchOverrides;
use crate::vpath::VPath;

pub const CONFIG_REL_PATH: &str = ".vscode/local-text.json";

#[derive(Deserialize, Default)]
struct ServerConfig {
    #[serde(default)]
    env:     HashMap<String, String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args:    Option<Vec<String>>,
}

/// Load the launch overrides for `bin` from the project at `root`. Returns the
/// default (empty) overrides when the file is absent, unreadable, malformed, or
/// has no entry for `bin`.
pub fn load_project_lsp_config(root: &VPath, bin: &str) -> LspLaunchOverrides {
    let Some(text) = read_config_text(root) else { return LspLaunchOverrides::default() };
    let Ok(cfg) = serde_json::from_str::<HashMap<String, ServerConfig>>(&text) else {
        return LspLaunchOverrides::default();
    };
    let Some(server) = cfg.get(bin) else { return LspLaunchOverrides::default() };
    // Sort env for deterministic ordering across runs (HashMap iteration order is random).
    let mut env: Vec<(String, String)> =
        server.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    env.sort();
    LspLaunchOverrides {
        command: server.command.clone(),
        args:    server.args.clone(),
        env,
    }
}

fn read_config_text(root: &VPath) -> Option<String> {
    match root {
        VPath::Local(p) => std::fs::read_to_string(p.join(CONFIG_REL_PATH)).ok(),
        VPath::Remote { host, path } => {
            let remote = format!("{}/{}", path.display(), CONFIG_REL_PATH);
            crate::ssh::read_remote_file_sync(host, &remote)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_local_project_config() {
        let dir = std::env::temp_dir().join(format!("lt-projcfg-test-{}", std::process::id()));
        let vscode = dir.join(".vscode");
        std::fs::create_dir_all(&vscode).unwrap();
        std::fs::write(
            vscode.join("local-text.json"),
            r#"{"rust-analyzer":{"env":{"FOO":"bar","BAZ":"qux"},"command":"/x/ra","args":["--stdio"]}}"#,
        ).unwrap();

        let ov = load_project_lsp_config(&VPath::Local(dir.clone()), "rust-analyzer");
        assert_eq!(ov.command.as_deref(), Some("/x/ra"));
        assert_eq!(ov.args, Some(vec!["--stdio".to_owned()]));
        // env is sorted for determinism
        assert_eq!(ov.env, vec![
            ("BAZ".to_owned(), "qux".to_owned()),
            ("FOO".to_owned(), "bar".to_owned()),
        ]);

        // A binary with no entry → empty defaults.
        let none = load_project_lsp_config(&VPath::Local(dir.clone()), "pylsp");
        assert!(none.command.is_none() && none.args.is_none() && none.env.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_yields_default() {
        let dir = std::env::temp_dir().join(format!("lt-projcfg-absent-{}", std::process::id()));
        let ov = load_project_lsp_config(&VPath::Local(dir), "rust-analyzer");
        assert!(ov.command.is_none() && ov.args.is_none() && ov.env.is_empty());
    }
}
