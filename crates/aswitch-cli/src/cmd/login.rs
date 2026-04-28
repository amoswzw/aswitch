use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use aswitch_core::account;
use aswitch_core::paths::{self, AswitchPaths};
use aswitch_core::plugin::{load_manifest, LoadedManifest, ReadyMarkerKind};
use aswitch_core::store::{self, CredentialStore};
use clap::Args;
use serde::Serialize;

use super::accounts;
use super::prompt::ensure_interactive_terminal;

#[derive(Debug, Args)]
#[command(
    about = "Run the native login flow and save the account; prompts when arguments are omitted"
)]
pub struct LoginArgs {
    #[arg(value_name = "TARGET", help = "<plugin> or <plugin>/<alias>")]
    pub target: Option<String>,
    #[arg(long = "as", value_name = "ALIAS", help = "Alias to save after login")]
    pub alias: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct LoginReport {
    plugin_id: String,
    alias: String,
    added: account::AddAccountReport,
}

pub fn run(paths: &AswitchPaths, args: LoginArgs) -> Result<()> {
    let resolved = match resolve_login_target(paths, &args)? {
        Some(resolved) => resolved,
        None => {
            println!("Login cancelled.");
            return Ok(());
        }
    };

    let manifest = load_plugin_manifest(paths, &resolved.plugin_id)?;
    let mut child = spawn_login_process(&manifest)?;
    wait_for_ready_marker(&manifest, &mut child)?;

    let alias = if let Some(alias) = resolved.alias {
        alias
    } else {
        match prompt_alias()? {
            Some(alias) => alias,
            None => {
                println!("Login completed, but it was not saved as an aswitch account.");
                return Ok(());
            }
        }
    };
    let added = account::add_account_with_config_dir(
        &resolved.plugin_id,
        &alias,
        args.force,
        Some(accounts::config_dir(paths)),
    )?;
    let report = LoginReport {
        plugin_id: resolved.plugin_id,
        alias,
        added,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Login completed and saved as {}/{}",
            report.plugin_id, report.alias
        );
    }

    Ok(())
}

#[derive(Debug)]
struct ResolvedLoginTarget {
    plugin_id: String,
    alias: Option<String>,
}

fn resolve_login_target(
    paths: &AswitchPaths,
    args: &LoginArgs,
) -> Result<Option<ResolvedLoginTarget>> {
    let parsed_target = parse_login_target(args.target.as_deref())?;
    let (plugin_from_target, alias_from_target) = match parsed_target {
        Some((plugin_id, alias)) => (Some(plugin_id), alias),
        None => (None, None),
    };
    let plugin_id = match plugin_from_target {
        Some(plugin_id) => plugin_id,
        None => {
            if args.json {
                bail!(
                    "Interactive login does not support --json; pass <plugin> or <plugin>/<alias>"
                );
            }
            match accounts::prompt_for_plugin(paths, "aswitch login")? {
                Some(plugin_id) => plugin_id,
                None => return Ok(None),
            }
        }
    };

    let alias = merge_login_alias(alias_from_target, args.alias.as_deref())?;
    if args.json && alias.is_none() {
        bail!("--json requires an explicit alias; use <plugin>/<alias> or --as <alias>");
    }

    Ok(Some(ResolvedLoginTarget { plugin_id, alias }))
}

fn parse_login_target(value: Option<&str>) -> Result<Option<(String, Option<String>)>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if let Some((plugin_id, alias)) = value.split_once('/') {
        if plugin_id.is_empty() || alias.is_empty() {
            bail!("TARGET must be <plugin> or <plugin>/<alias>");
        }
        return Ok(Some((plugin_id.to_string(), Some(alias.to_string()))));
    }

    if value.is_empty() {
        bail!("TARGET must not be empty");
    }

    Ok(Some((value.to_string(), None)))
}

fn merge_login_alias(
    alias_from_target: Option<String>,
    alias_flag: Option<&str>,
) -> Result<Option<String>> {
    match (alias_from_target, alias_flag) {
        (Some(target_alias), Some(flag_alias)) if target_alias != flag_alias => {
            bail!("Conflicting aliases: TARGET and --as do not match")
        }
        (Some(target_alias), _) => Ok(Some(target_alias)),
        (None, Some(flag_alias)) => Ok(Some(flag_alias.to_string())),
        (None, None) => Ok(None),
    }
}

fn load_plugin_manifest(paths: &AswitchPaths, plugin_id: &str) -> Result<LoadedManifest> {
    let manifest_path = paths.plugins_dir.join(plugin_id).join("plugin.toml");
    load_manifest(&manifest_path)
        .with_context(|| format!("failed to load manifest for plugin {plugin_id}"))
}

fn spawn_login_process(manifest: &LoadedManifest) -> Result<std::process::Child> {
    let (program, args) = manifest
        .manifest
        .login
        .cmd
        .split_first()
        .context("login.cmd must not be empty")?;

    Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn login command {}",
                manifest.manifest.login.cmd.join(" ")
            )
        })
}

fn wait_for_ready_marker(manifest: &LoadedManifest, child: &mut std::process::Child) -> Result<()> {
    let deadline =
        Instant::now() + Duration::from_secs(manifest.manifest.login.ready_marker_timeout_s);

    loop {
        if ready_marker_is_present(manifest)? {
            let status = child.wait().context("failed to wait for login process")?;
            ensure_success(status, &manifest.manifest.id)?;
            return Ok(());
        }

        if let Some(status) = child
            .try_wait()
            .context("failed to inspect login process")?
        {
            if status.success() {
                if Instant::now() >= deadline {
                    bail!("login completed but ready marker did not appear before timeout");
                }
            } else {
                ensure_success(status, &manifest.manifest.id)?;
            }
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "timed out after {}s waiting for login marker for {}",
                manifest.manifest.login.ready_marker_timeout_s,
                manifest.manifest.id
            );
        }

        thread::sleep(Duration::from_millis(500));
    }
}

fn ready_marker_is_present(manifest: &LoadedManifest) -> Result<bool> {
    match manifest.manifest.login.ready_marker_kind {
        ReadyMarkerKind::File => {
            let path = manifest
                .manifest
                .login
                .ready_marker_path
                .as_deref()
                .context("file ready marker requires login.ready_marker_path")?;
            Ok(paths::expand_user_path(path)?.exists())
        }
        ReadyMarkerKind::Keychain => {
            let store = store::resolve_active_store(&manifest.manifest.credential_store)?;
            Ok(store.exists()? || store.allows_missing_active())
        }
    }
}

fn ensure_success(status: ExitStatus, plugin_id: &str) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    bail!("login command for {plugin_id} exited with status {status}");
}

fn prompt_alias() -> Result<Option<String>> {
    ensure_interactive_terminal("aswitch login")?;
    accounts::prompt_for_alias("Enter an account alias")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use aswitch_core::plugin::{
        CredentialStore, CredentialStoreKind, LoadedManifest, LoginConfig, Manifest, Platform,
        PluginSource, ReadyMarkerKind,
    };

    use super::{merge_login_alias, parse_login_target, ready_marker_is_present};

    fn file_manifest(path: String) -> LoadedManifest {
        LoadedManifest {
            path: PathBuf::from("/tmp/demo/plugin.toml"),
            source: PluginSource::User,
            warnings: Vec::new(),
            manifest: Manifest {
                id: "demo".to_string(),
                display_name: "Demo".to_string(),
                version: "1.0.0".to_string(),
                author: "tests".to_string(),
                description: "tests".to_string(),
                platforms: vec![Platform::Macos, Platform::Linux],
                credential_store: CredentialStore {
                    kind: CredentialStoreKind::File,
                    path: Some(path.clone()),
                    permissions: Some(0o600),
                    macos_service: None,
                    macos_account: None,
                    linux_schema: None,
                    linux_attributes: Default::default(),
                    linux_fallback_kind: None,
                    linux_fallback_path: None,
                    linux_fallback_permissions: None,
                    allow_empty_active: false,
                },
                session_activation: None,
                aux_files: Vec::new(),
                identity_extract: Vec::new(),
                login: LoginConfig {
                    cmd: vec!["demo".to_string(), "login".to_string()],
                    ready_marker_kind: ReadyMarkerKind::File,
                    ready_marker_timeout_s: 1,
                    ready_marker_path: Some(path),
                },
                usage_source: Vec::new(),
            },
        }
    }

    #[test]
    fn file_ready_marker_detects_existing_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let marker = temp_dir.path().join("ready.txt");
        fs::write(&marker, "ok").expect("marker");

        let manifest = file_manifest(marker.display().to_string());
        assert!(ready_marker_is_present(&manifest).expect("ready marker"));
    }

    #[test]
    fn file_ready_marker_detects_missing_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let marker = temp_dir.path().join("missing.txt");

        let manifest = file_manifest(marker.display().to_string());
        assert!(!ready_marker_is_present(&manifest).expect("ready marker"));
    }

    #[test]
    fn parse_login_target_supports_plugin_alias() {
        let parsed = parse_login_target(Some("codex/work"))
            .expect("parse target")
            .expect("parsed value");

        assert_eq!(parsed.0, "codex");
        assert_eq!(parsed.1.as_deref(), Some("work"));
    }

    #[test]
    fn merge_login_alias_rejects_conflicts() {
        let error = merge_login_alias(Some("work".to_string()), Some("personal"))
            .expect_err("should reject conflicting aliases");

        assert!(error.to_string().contains("--as"));
    }
}
