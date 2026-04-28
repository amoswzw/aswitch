use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use aswitch_core::account::{self, AccountRecord, AddAccountReport, PluginStatusRow, StatusReport};
use aswitch_core::paths::AswitchPaths;
use aswitch_core::plugin::{load_all, LoadedManifest};
use aswitch_core::switch;
use aswitch_core::usage::{self, CollectUsageOptions, UsageSelection, UsageWindow};
use chrono::{DateTime, Local, Utc};
use clap::{Args, ValueEnum};
use serde::Serialize;

use super::prompt::{
    ensure_interactive_terminal, prompt_confirmation, prompt_option,
    prompt_required_value_in_terminal, InteractiveOption,
};
use super::{live, project, scope, shell, usage as usage_cmd};

#[derive(Debug, Args)]
#[command(about = "Save the current active credentials; prompts when arguments are omitted")]
pub struct AddArgs {
    #[arg(value_name = "ACCOUNT", help = "<plugin>/<alias> or alias")]
    pub target: Option<String>,
    #[arg(long, value_name = "ID", help = "Plugin id; prompts when omitted")]
    pub plugin: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(about = "Switch an account on, or turn a scope off")]
pub struct UseArgs {
    #[arg(
        value_name = "ACCOUNT",
        help = "<plugin>/<alias>, alias, or row number"
    )]
    pub selector: Option<String>,
    #[arg(long, value_name = "ID")]
    pub plugin: Option<String>,
    #[arg(long, value_enum, default_value_t = UseScope::Shell)]
    pub scope: UseScope,
    #[arg(long)]
    pub off: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(about = "List saved accounts, the current effective scope, project binding, or status")]
pub struct ListArgs {
    #[arg(long, value_enum, default_value_t = ListView::Saved)]
    pub view: ListView,
    #[arg(long, value_name = "ID")]
    pub plugin: Option<String>,
    #[arg(long)]
    pub explain: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(about = "Remove a saved account")]
pub struct RemoveArgs {
    pub selector: String,
    #[arg(long, value_name = "ID")]
    pub plugin: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum UseScope {
    Shell,
    Project,
    Global,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ListView {
    Saved,
    Current,
    Project,
    Status,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ListedAccount {
    pub row: usize,
    #[serde(flatten)]
    pub account: AccountRecord,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedAccount {
    pub plugin_id: String,
    pub alias: String,
}

pub fn add(paths: &AswitchPaths, args: AddArgs) -> Result<()> {
    if args.json
        && (args.target.is_none() || args.plugin.is_none() && !has_explicit_plugin(&args.target))
    {
        bail!("Interactive save does not support --json; pass <plugin>/<alias> or alias --plugin <id>");
    }

    let resolved = match resolve_add_target(paths, &args)? {
        Some(resolved) => resolved,
        None => {
            println!("Save cancelled.");
            return Ok(());
        }
    };

    let report = account::add_account_with_config_dir(
        &resolved.plugin_id,
        &resolved.alias,
        args.force,
        Some(config_dir(paths)),
    )?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_add_report(&report);
    }

    Ok(())
}

pub fn use_account(paths: &AswitchPaths, args: UseArgs) -> Result<()> {
    if args.off {
        if args.selector.is_some() {
            bail!("ACCOUNT is not accepted with --off; use --plugin when needed");
        }

        return match args.scope {
            UseScope::Shell => shell::direct_clear_requires_shell(),
            UseScope::Project => project::run(
                paths,
                project::ProjectCommand::Clear(project::ProjectClearArgs {
                    plugin: args.plugin,
                    json: args.json,
                }),
            ),
            UseScope::Global => shell::clear_global(paths, args.plugin, args.json),
        };
    }

    let resolved = match args.selector.as_deref() {
        Some(selector) => Some(resolve_selector(paths, selector, args.plugin.as_deref())?),
        None => {
            if args.json {
                bail!("Interactive switching does not support --json; pass <plugin>/<alias>");
            }
            prompt_for_account(paths)?
        }
    };
    let Some(resolved) = resolved else {
        println!("Switch cancelled.");
        return Ok(());
    };

    match args.scope {
        UseScope::Shell => shell::direct_use_requires_shell(),
        UseScope::Project => project::run(
            paths,
            project::ProjectCommand::Use(project::ProjectUseArgs {
                selector: Some(format!("{}/{}", resolved.plugin_id, resolved.alias)),
                plugin: None,
                json: args.json,
            }),
        ),
        UseScope::Global => {
            let report = switch::use_account_with_config_dir(
                &resolved.plugin_id,
                &resolved.alias,
                Some(config_dir(paths)),
            )?;

            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Switched to {}/{}", report.plugin_id, report.alias);
                println!("Restart the corresponding client for the new account to take effect.");
            }

            Ok(())
        }
    }
}

pub fn list(paths: &AswitchPaths, args: ListArgs) -> Result<()> {
    if args.explain && args.view != ListView::Current {
        bail!("--explain is only supported with `aswitch ls --view current`");
    }

    match args.view {
        ListView::Saved => list_saved(paths, &args),
        ListView::Current => list_current(paths, &args),
        ListView::Project => list_project(&args),
        ListView::Status => list_status(paths, &args),
    }
}

pub fn remove(paths: &AswitchPaths, args: RemoveArgs) -> Result<()> {
    let resolved = resolve_selector(paths, &args.selector, args.plugin.as_deref())?;
    let report = account::remove_account_with_config_dir(
        &resolved.plugin_id,
        &resolved.alias,
        args.force,
        Some(config_dir(paths)),
    )?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Removed {}/{}", report.plugin_id, report.alias);
        if report.was_active {
            println!("The live credentials remain in the original client location, but the registry no longer tracks them.");
        }
    }

    Ok(())
}

fn list_saved(paths: &AswitchPaths, args: &ListArgs) -> Result<()> {
    let mut accounts =
        account::list_accounts_with_config_dir(Some(config_dir(paths)), args.plugin.as_deref())?;
    let effective = scope::effective_accounts(paths, args.plugin.as_deref())?;
    live::enrich_saved_accounts(&mut accounts, &effective);
    let rows = enumerate_accounts(accounts);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_account_rows(paths, &rows)?;
    }

    Ok(())
}

fn list_current(paths: &AswitchPaths, args: &ListArgs) -> Result<()> {
    let rows = scope::effective_accounts(paths, args.plugin.as_deref())?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print_current_rows(&rows, args.explain);
    }

    Ok(())
}

fn list_project(args: &ListArgs) -> Result<()> {
    let binding =
        filter_project_binding(project::current_project_binding()?, args.plugin.as_deref());

    if args.json {
        println!("{}", serde_json::to_string_pretty(&binding)?);
    } else {
        print_project_binding(binding);
    }

    Ok(())
}

fn list_status(paths: &AswitchPaths, args: &ListArgs) -> Result<()> {
    let report = filter_status_report(
        account::status_with_config_dir(Some(config_dir(paths)))?,
        args.plugin.as_deref(),
    );

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_status_report(&report);
    }

    Ok(())
}

fn filter_project_binding(
    binding: Option<project::ProjectBinding>,
    plugin_filter: Option<&str>,
) -> Option<project::ProjectBinding> {
    let Some(mut binding) = binding else {
        return None;
    };

    if let Some(plugin_id) = plugin_filter {
        binding.accounts.retain(|current, _| current == plugin_id);
    }

    Some(binding)
}

fn print_project_binding(binding: Option<project::ProjectBinding>) {
    let Some(binding) = binding else {
        println!("No project binding found.");
        return;
    };

    println!("Project binding: {}", binding.path.display());
    if binding.accounts.is_empty() {
        println!("Accounts: -");
        return;
    }

    println!("Accounts:");
    for (plugin_id, alias) in binding.accounts {
        println!("- {plugin_id}/{alias}");
    }
}

fn filter_status_report(mut report: StatusReport, plugin_filter: Option<&str>) -> StatusReport {
    let Some(plugin_id) = plugin_filter else {
        return report;
    };

    report.plugins.retain(|row| row.plugin_id == plugin_id);
    report
        .errors
        .retain(|error| error.path.to_string_lossy().contains(plugin_id));
    report
}

pub(crate) fn resolve_selector(
    paths: &AswitchPaths,
    selector: &str,
    plugin_hint: Option<&str>,
) -> Result<ResolvedAccount> {
    let accounts = account::list_accounts_with_config_dir(Some(config_dir(paths)), plugin_hint)?;
    let rows = enumerate_accounts(accounts);

    if let Ok(row) = selector.parse::<usize>() {
        if row == 0 {
            bail!("Row numbers start at 1");
        }

        let selected = rows
            .into_iter()
            .find(|item| item.row == row)
            .ok_or_else(|| anyhow::anyhow!("Row {row} does not exist"))?;
        return Ok(ResolvedAccount {
            plugin_id: selected.account.plugin_id,
            alias: selected.account.alias,
        });
    }

    if let Some((plugin_id, alias)) = selector.split_once('/') {
        if let Some(plugin_hint) = plugin_hint {
            if plugin_hint != plugin_id {
                bail!("Conflicting arguments: the positional plugin does not match --plugin");
            }
        }
        return Ok(ResolvedAccount {
            plugin_id: plugin_id.to_string(),
            alias: alias.to_string(),
        });
    }

    let mut matches = rows
        .into_iter()
        .filter(|item| item.account.alias == selector)
        .collect::<Vec<_>>();

    match matches.len() {
        0 => bail!("Account {selector} does not exist"),
        1 => {
            let selected = matches.remove(0);
            Ok(ResolvedAccount {
                plugin_id: selected.account.plugin_id,
                alias: selected.account.alias,
            })
        }
        _ => bail!("Alias {selector} exists under multiple plugins; use <plugin>/<alias>"),
    }
}

pub(crate) fn enumerate_accounts(accounts: Vec<AccountRecord>) -> Vec<ListedAccount> {
    accounts
        .into_iter()
        .enumerate()
        .map(|(index, account)| ListedAccount {
            row: index + 1,
            account,
        })
        .collect()
}

pub(crate) fn prompt_for_account(paths: &AswitchPaths) -> Result<Option<ResolvedAccount>> {
    ensure_interactive_terminal("aswitch use")?;

    let rows = enumerate_accounts(account::list_accounts_with_config_dir(
        Some(config_dir(paths)),
        None,
    )?);
    if rows.is_empty() {
        bail!("No saved accounts found; run aswitch save or aswitch login first");
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stderr();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    prompt_for_account_from_rows(&rows, &mut input, &mut output)
}

fn prompt_for_account_from_rows<R, W>(
    rows: &[ListedAccount],
    input: &mut R,
    output: &mut W,
) -> Result<Option<ResolvedAccount>>
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    let grouped = group_accounts_by_plugin(rows);
    let plugin_ids = grouped.keys().cloned().collect::<Vec<_>>();

    let plugin_id = if plugin_ids.len() == 1 {
        let plugin_id = plugin_ids[0].clone();
        writeln!(output, "plugin: {plugin_id}")?;
        plugin_id
    } else {
        let options = plugin_ids
            .iter()
            .map(|plugin_id| InteractiveOption {
                label: plugin_label(plugin_id, grouped.get(plugin_id).expect("grouped plugin")),
                value: plugin_id.clone(),
                keys: vec![plugin_id.clone()],
            })
            .collect::<Vec<_>>();

        match prompt_option(
            input,
            output,
            "Available plugins:",
            "Select a plugin",
            &options,
        )? {
            Some(plugin_id) => plugin_id,
            None => return Ok(None),
        }
    };

    let accounts = grouped.get(&plugin_id).expect("selected plugin");
    let account = if accounts.len() == 1 {
        let selected = accounts[0];
        writeln!(
            output,
            "Account: {}/{}",
            selected.account.plugin_id, selected.account.alias
        )?;
        ResolvedAccount {
            plugin_id: selected.account.plugin_id.clone(),
            alias: selected.account.alias.clone(),
        }
    } else {
        let options = accounts
            .iter()
            .map(|item| InteractiveOption {
                label: account_label(item),
                value: ResolvedAccount {
                    plugin_id: item.account.plugin_id.clone(),
                    alias: item.account.alias.clone(),
                },
                keys: vec![
                    item.account.alias.clone(),
                    format!("{}/{}", item.account.plugin_id, item.account.alias),
                ],
            })
            .collect::<Vec<_>>();

        match prompt_option(
            input,
            output,
            "Available accounts:",
            "Select an account",
            &options,
        )? {
            Some(account) => account,
            None => return Ok(None),
        }
    };

    if !prompt_confirmation(
        input,
        output,
        &format!("Switch to {}/{}", account.plugin_id, account.alias),
    )? {
        return Ok(None);
    }

    Ok(Some(account))
}

fn group_accounts_by_plugin<'a>(
    rows: &'a [ListedAccount],
) -> BTreeMap<String, Vec<&'a ListedAccount>> {
    let mut grouped = BTreeMap::new();
    for row in rows {
        grouped
            .entry(row.account.plugin_id.clone())
            .or_insert_with(Vec::new)
            .push(row);
    }
    grouped
}

fn plugin_label(plugin_id: &str, rows: &[&ListedAccount]) -> String {
    let active_alias = rows
        .iter()
        .find(|item| item.account.active)
        .map(|item| item.account.alias.as_str())
        .unwrap_or("-");
    format!(
        "{plugin_id} ({} accounts, active: {active_alias})",
        rows.len()
    )
}

fn account_label(row: &ListedAccount) -> String {
    let active = if row.account.active { " [active]" } else { "" };
    format!(
        "{}{}  email={}  plan={}",
        row.account.alias,
        active,
        display_opt(row.account.email.as_deref()),
        display_opt(row.account.plan.as_deref())
    )
}

fn print_add_report(report: &AddAccountReport) {
    let status = if report.overwritten {
        "Updated"
    } else {
        "Saved"
    };
    println!("{status} {}/{}", report.plugin_id, report.alias);
    if let Some(email) = report.identity.email.as_deref() {
        println!("email: {email}");
    }
    if let Some(plan) = report.identity.plan.as_deref() {
        println!("plan: {plan}");
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}

#[derive(Clone, Debug)]
struct AccountListDisplayRow {
    plugin_alias: String,
    email: String,
    quota: String,
    usage: String,
    weekly_usage: String,
    next_refresh: String,
    active: String,
}

#[derive(Clone, Debug)]
struct UsageDisplay {
    snapshot: Option<usage::UsageSnapshot>,
    cache_status: Option<usage::UsageCacheStatus>,
    error: Option<String>,
}

fn print_account_rows(paths: &AswitchPaths, rows: &[ListedAccount]) -> Result<()> {
    if rows.is_empty() {
        println!("No saved accounts.");
        return Ok(());
    }

    let effective_targets = scope::effective_targets(paths, None)?;
    println!("Saved accounts: {}", rows.len());

    for (index, row) in rows.iter().enumerate() {
        let display = build_account_list_display_row(paths, row, &effective_targets);
        if index > 0 {
            println!();
        }
        println!("[{}] {}", row.row, display.plugin_alias);
        println!("    Email: {}", display.email);
        println!("    Quota: {}", display.quota);
        println!("    Monthly Usage: {}", display.usage);
        println!("    Weekly Usage : {}", display.weekly_usage);
        println!("    Next Refresh : {}", display.next_refresh);
        println!("    Active       : {}", display.active);
    }

    Ok(())
}

fn print_current_rows(rows: &[scope::EffectiveAccountRecord], explain: bool) {
    if rows.is_empty() {
        println!("No plugins or active accounts.");
        return;
    }

    println!(
        "{:<16} {:<16} {:<28} {:<16} {:<8} SCOPE",
        "PLUGIN", "ALIAS", "EMAIL", "ORG", "PLAN"
    );

    for row in rows {
        println!(
            "{:<16} {:<16} {:<28} {:<16} {:<8} {}",
            row.plugin_id,
            display_opt(row.alias.as_deref()),
            display_opt(row.email.as_deref()),
            display_opt(row.org_name.as_deref()),
            display_opt(row.plan.as_deref()),
            display_scope(&row.scope)
        );
        if explain {
            println!(
                "detail [{}]: {}",
                row.plugin_id,
                row.detail.as_deref().unwrap_or("-")
            );
        }
    }
}

fn print_status_report(report: &StatusReport) {
    println!("registry version: {}", report.registry_version);
    println!("last switch: {}", display_time(report.last_switch_at));
    println!();
    println!(
        "{:<16} {:<8} {:<8} {:<16} {:<8} LAST_USED",
        "PLUGIN", "STATUS", "SOURCE", "ACTIVE", "COUNT"
    );

    for row in &report.plugins {
        print_status_row(row);
    }

    if !report.warnings.is_empty() {
        println!();
        println!("warnings:");
        for warning in &report.warnings {
            println!("- {warning}");
        }
    }

    if !report.errors.is_empty() {
        println!();
        println!("errors:");
        for error in &report.errors {
            println!("- {}: {}", error.path.display(), error.error);
        }
    }
}

fn print_status_row(row: &PluginStatusRow) {
    println!(
        "{:<16} {:<8} {:<8} {:<16} {:<8} {}",
        row.plugin_id,
        if row.loaded { "ok" } else { "missing" },
        display_opt(row.source.as_deref()),
        display_opt(row.active_alias.as_deref()),
        row.account_count,
        display_time(row.last_used_at)
    );

    for warning in &row.warnings {
        println!("warning [{}]: {warning}", row.plugin_id);
    }
}

fn display_opt(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

fn build_account_list_display_row(
    paths: &AswitchPaths,
    row: &ListedAccount,
    effective_targets: &[(String, String)],
) -> AccountListDisplayRow {
    let is_effective = effective_targets.iter().any(|(plugin_id, alias)| {
        plugin_id == &row.account.plugin_id && alias == &row.account.alias
    });

    let current_usage =
        load_usage_display(paths, &row.account, UsageWindow::CurrentMonth, is_effective);
    let weekly_usage = load_usage_display(paths, &row.account, UsageWindow::Last7d, is_effective);

    AccountListDisplayRow {
        plugin_alias: format!("{}/{}", row.account.plugin_id, row.account.alias),
        email: display_opt(row.account.email.as_deref()).to_string(),
        quota: format_quota_cell(&current_usage),
        usage: format_usage_cell(&current_usage),
        weekly_usage: format_usage_cell(&weekly_usage),
        next_refresh: format_next_refresh(
            current_usage
                .cache_status
                .as_ref()
                .map(|status| status.expires_at),
            weekly_usage
                .cache_status
                .as_ref()
                .map(|status| status.expires_at),
        ),
        active: if row.account.active {
            "yes".to_string()
        } else {
            "-".to_string()
        },
    }
}

fn load_usage_display(
    paths: &AswitchPaths,
    account: &AccountRecord,
    window: UsageWindow,
    is_effective: bool,
) -> UsageDisplay {
    let mut snapshot = match usage::collect_usage_with_config_dir(
        &account.plugin_id,
        &account.alias,
        Some(config_dir(paths)),
        CollectUsageOptions {
            window: Some(window),
            source: Some(UsageSelection::Both),
            refresh: false,
        },
    ) {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            return UsageDisplay {
                snapshot: None,
                cache_status: usage::inspect_cache_with_config_dir(
                    &account.plugin_id,
                    &account.alias,
                    Some(config_dir(paths)),
                    CollectUsageOptions {
                        window: Some(window),
                        source: Some(UsageSelection::Both),
                        refresh: false,
                    },
                )
                .ok()
                .flatten(),
                error: Some(error.to_string()),
            };
        }
    };

    if let Some(snapshot_ref) = snapshot.as_mut() {
        live::enrich_usage_snapshot(snapshot_ref, Some(config_dir(paths)), is_effective);
    }

    UsageDisplay {
        cache_status: usage::inspect_cache_with_config_dir(
            &account.plugin_id,
            &account.alias,
            Some(config_dir(paths)),
            CollectUsageOptions {
                window: Some(window),
                source: Some(UsageSelection::Both),
                refresh: false,
            },
        )
        .ok()
        .flatten(),
        snapshot,
        error: None,
    }
}

fn format_quota_cell(display: &UsageDisplay) -> String {
    if let Some(snapshot) = display.snapshot.as_ref() {
        let quota = usage_cmd::format_quota(&snapshot.quota);
        if quota != "-" {
            return quota;
        }
    }

    if display.error.is_some() {
        "error".to_string()
    } else {
        "-".to_string()
    }
}

fn format_usage_cell(display: &UsageDisplay) -> String {
    let Some(snapshot) = display.snapshot.as_ref() else {
        return if display.error.is_some() {
            "error".to_string()
        } else {
            "-".to_string()
        };
    };

    if snapshot.metrics.requests.is_none()
        && snapshot.metrics.tokens_in.is_none()
        && snapshot.metrics.tokens_out.is_none()
        && snapshot.metrics.cost_usd.is_none()
    {
        return "-".to_string();
    }

    format!(
        "req {} | in {} | out {}",
        compact_metric(snapshot.metrics.requests),
        compact_metric(snapshot.metrics.tokens_in),
        compact_metric(snapshot.metrics.tokens_out)
    )
}

fn format_next_refresh(
    current_month: Option<DateTime<Utc>>,
    weekly: Option<DateTime<Utc>>,
) -> String {
    let next_refresh = match (current_month, weekly) {
        (Some(current_month), Some(weekly)) => Some(current_month.min(weekly)),
        (Some(current_month), None) => Some(current_month),
        (None, Some(weekly)) => Some(weekly),
        (None, None) => None,
    };

    match next_refresh {
        Some(next_refresh) if next_refresh <= Utc::now() => "now".to_string(),
        Some(next_refresh) => next_refresh
            .with_timezone(&Local)
            .format("%m-%d %H:%M")
            .to_string(),
        None => "-".to_string(),
    }
}

fn compact_metric(value: Option<f64>) -> String {
    match value {
        Some(value) => compact_number(value),
        None => "-".to_string(),
    }
}

fn compact_number(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1_000_000_000.0 {
        return format_scaled(value, 1_000_000_000.0, "B");
    }
    if abs >= 1_000_000.0 {
        return format_scaled(value, 1_000_000.0, "M");
    }
    if abs >= 1_000.0 {
        return format_scaled(value, 1_000.0, "k");
    }
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value:.1}")
    }
}

fn format_scaled(value: f64, scale: f64, suffix: &str) -> String {
    let scaled = value / scale;
    let mut text = format!("{scaled:.1}");
    if text.ends_with(".0") {
        text.truncate(text.len() - 2);
    }
    format!("{text}{suffix}")
}

fn display_time(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

fn display_scope(scope: &scope::ScopeSource) -> &'static str {
    match scope {
        scope::ScopeSource::Local => "shell",
        scope::ScopeSource::Project => "project",
        scope::ScopeSource::Global => "global",
        scope::ScopeSource::None => "-",
    }
}

pub(crate) fn config_dir(paths: &AswitchPaths) -> PathBuf {
    paths.root.clone()
}

pub(crate) fn prompt_for_plugin(paths: &AswitchPaths, action: &str) -> Result<Option<String>> {
    ensure_interactive_terminal(action)?;

    let catalog = load_all(&paths.plugins_dir)?;
    if catalog.plugins.is_empty() {
        bail!("No plugins are loaded; install one or check ~/.aswitch/plugins");
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stderr();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    prompt_for_plugin_from_catalog(&catalog.plugins, &mut input, &mut output)
}

pub(crate) fn prompt_for_alias(prompt: &str) -> Result<Option<String>> {
    prompt_required_value_in_terminal(prompt)
}

fn resolve_add_target(paths: &AswitchPaths, args: &AddArgs) -> Result<Option<ResolvedAccount>> {
    let parsed_target = args.target.as_deref().and_then(parse_plugin_alias);

    if let Some((plugin_id, alias)) = parsed_target.as_ref() {
        if let Some(plugin) = args.plugin.as_deref() {
            if plugin != plugin_id {
                bail!("Conflicting arguments: the positional plugin does not match --plugin");
            }
        }

        return Ok(Some(ResolvedAccount {
            plugin_id: plugin_id.clone(),
            alias: alias.clone(),
        }));
    }

    let plugin_id = match args.plugin.clone() {
        Some(plugin_id) => plugin_id,
        None => match prompt_for_plugin(paths, "aswitch save")? {
            Some(plugin_id) => plugin_id,
            None => return Ok(None),
        },
    };

    let alias = match args.target.clone() {
        Some(alias) => alias,
        None => match prompt_for_alias("Enter an account alias")? {
            Some(alias) => alias,
            None => return Ok(None),
        },
    };

    Ok(Some(ResolvedAccount { plugin_id, alias }))
}

fn prompt_for_plugin_from_catalog<R, W>(
    plugins: &[LoadedManifest],
    input: &mut R,
    output: &mut W,
) -> Result<Option<String>>
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    if plugins.len() == 1 {
        let plugin_id = plugins[0].manifest.id.clone();
        writeln!(output, "plugin: {plugin_id}")?;
        return Ok(Some(plugin_id));
    }

    let options = plugins
        .iter()
        .map(|plugin| InteractiveOption {
            label: format!("{} ({})", plugin.manifest.id, plugin.manifest.display_name),
            value: plugin.manifest.id.clone(),
            keys: vec![
                plugin.manifest.id.clone(),
                plugin.manifest.display_name.clone(),
            ],
        })
        .collect::<Vec<_>>();

    prompt_option(
        input,
        output,
        "Available plugins:",
        "Select a plugin",
        &options,
    )
}

fn parse_plugin_alias(value: &str) -> Option<(String, String)> {
    let (plugin_id, alias) = value.split_once('/')?;
    if plugin_id.is_empty() || alias.is_empty() {
        return None;
    }

    Some((plugin_id.to_string(), alias.to_string()))
}

fn has_explicit_plugin(target: &Option<String>) -> bool {
    target.as_deref().and_then(parse_plugin_alias).is_some()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{prompt_for_account_from_rows, AccountRecord, ListedAccount};

    #[test]
    fn interactive_use_selects_by_numbers() {
        let rows = sample_rows();
        let mut input = std::io::Cursor::new("2\n1\ny\n");
        let mut output = Vec::new();

        let selected = prompt_for_account_from_rows(&rows, &mut input, &mut output)
            .expect("selection succeeds")
            .expect("selection exists");

        assert_eq!(selected.plugin_id, "codex");
        assert_eq!(selected.alias, "work");
    }

    #[test]
    fn interactive_use_accepts_plugin_and_alias_text() {
        let rows = sample_rows();
        let mut input = std::io::Cursor::new("claude-code\npersonal\nyes\n");
        let mut output = Vec::new();

        let selected = prompt_for_account_from_rows(&rows, &mut input, &mut output)
            .expect("selection succeeds")
            .expect("selection exists");

        assert_eq!(selected.plugin_id, "claude-code");
        assert_eq!(selected.alias, "personal");
    }

    #[test]
    fn interactive_use_can_cancel() {
        let rows = sample_rows();
        let mut input = std::io::Cursor::new("q\n");
        let mut output = Vec::new();

        let selected = prompt_for_account_from_rows(&rows, &mut input, &mut output)
            .expect("selection succeeds");

        assert!(selected.is_none());
    }

    fn sample_rows() -> Vec<ListedAccount> {
        vec![
            listed_account(
                1,
                "claude-code",
                "work",
                "work@example.com",
                Some("team"),
                true,
            ),
            listed_account(
                2,
                "claude-code",
                "personal",
                "personal@example.com",
                Some("pro"),
                false,
            ),
            listed_account(3, "codex", "work", "codex@example.com", Some("plus"), false),
        ]
    }

    fn listed_account(
        row: usize,
        plugin_id: &str,
        alias: &str,
        email: &str,
        plan: Option<&str>,
        active: bool,
    ) -> ListedAccount {
        ListedAccount {
            row,
            account: AccountRecord {
                plugin_id: plugin_id.to_string(),
                alias: alias.to_string(),
                email: Some(email.to_string()),
                org_name: None,
                plan: plan.map(str::to_string),
                added_at: Utc::now(),
                last_used_at: None,
                active,
            },
        }
    }
}
