use aswitch_core::session;
use serde::Serialize;

use super::accounts;

#[derive(Debug, Serialize)]
pub(crate) struct ActivationOutput {
    #[serde(flatten)]
    report: session::SessionActivationReport,
    pub(crate) shell: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeactivationOutput {
    plugin_id: String,
    env_var: String,
    pub(crate) shell: String,
}

pub(crate) fn build_activation_output(
    paths: &aswitch_core::paths::AswitchPaths,
    plugin_id: &str,
    alias: &str,
) -> anyhow::Result<ActivationOutput> {
    let report = session::prepare_session_activation_with_config_dir(
        plugin_id,
        alias,
        Some(accounts::config_dir(paths)),
    )?;
    let shell = format!(
        "export {}={}",
        report.env_var,
        shell_quote(report.runtime_home.to_string_lossy().as_ref())
    );

    Ok(ActivationOutput { report, shell })
}

pub(crate) fn build_deactivation_output(
    paths: &aswitch_core::paths::AswitchPaths,
    plugin_id: &str,
) -> anyhow::Result<DeactivationOutput> {
    let env_var =
        session::session_env_var_with_config_dir(plugin_id, Some(accounts::config_dir(paths)))?;
    let shell = format!("unset {env_var}");

    Ok(DeactivationOutput {
        plugin_id: plugin_id.to_string(),
        env_var,
        shell,
    })
}

pub(crate) fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("/tmp/plain"), "'/tmp/plain'");
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\"'\"'s'");
    }
}
