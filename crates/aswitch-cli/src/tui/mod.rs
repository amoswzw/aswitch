use std::collections::{BTreeMap, HashSet};
use std::io::{self, IsTerminal};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use aswitch_core::account::{self, AccountRecord, PluginStatusRow};
use aswitch_core::paths::AswitchPaths;
use aswitch_core::switch;
use aswitch_core::usage::{
    self, CollectUsageOptions, UsageSelection, UsageSnapshot, UsageSourceSummary, UsageWindow,
};
use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Terminal;
use serde_json::Value;

use crate::cmd::{accounts, live, scope, usage as usage_cmd};

pub fn run(paths: &AswitchPaths) -> Result<()> {
    if !is_terminal_session() {
        eprintln!("aswitch tui must be run in a terminal");
        std::process::exit(2);
    }

    let mut app = App::load(paths)?;
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    loop {
        terminal.draw(|frame| app.render(frame))?;
        if app.should_quit {
            break;
        }

        let poll_ms = if app.quota_is_loading() { 80 } else { 200 };
        if event::poll(Duration::from_millis(poll_ms))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(paths, key.code)?;
                }
            }
        }
        app.tick();
    }

    Ok(())
}

pub fn is_terminal_session() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal()
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Switch,
    Plugins,
}

#[derive(Clone, Copy)]
struct Theme {
    text: Color,
    muted: Color,
    border: Color,
    accent: Color,
    accent_soft: Color,
    success: Color,
    warning: Color,
    surface: Color,
    surface_alt: Color,
    header_bg: Color,
    selected_bg: Color,
    selected_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            text: Color::Rgb(235, 239, 245),
            muted: Color::Rgb(137, 148, 168),
            border: Color::Rgb(73, 89, 113),
            accent: Color::Rgb(84, 184, 255),
            accent_soft: Color::Rgb(255, 205, 112),
            success: Color::Rgb(132, 214, 144),
            warning: Color::Rgb(255, 150, 124),
            surface: Color::Rgb(10, 16, 25),
            surface_alt: Color::Rgb(18, 29, 45),
            header_bg: Color::Rgb(18, 27, 39),
            selected_bg: Color::Rgb(31, 54, 83),
            selected_fg: Color::Rgb(247, 250, 255),
        }
    }
}

struct App {
    panel: Panel,
    accounts: Vec<AccountRecord>,
    account_quota: Vec<AccountQuotaSummary>,
    selected: usize,
    usage_window: UsageWindow,
    usage_source: UsageSelection,
    usage: Option<UsageSnapshot>,
    plugins: Vec<PluginStatusRow>,
    status: String,
    help_open: bool,
    should_quit: bool,
    quota_loader: Option<QuotaLoader>,
    spinner_tick: usize,
}

enum QuotaEvent {
    Identities {
        emails: BTreeMap<(String, String), Option<String>>,
        org_names: BTreeMap<(String, String), Option<String>>,
        plans: BTreeMap<(String, String), Option<String>>,
    },
    Quota {
        index: usize,
        summary: AccountQuotaSummary,
    },
    SelectedUsage {
        index: usize,
        snapshot: UsageSnapshot,
    },
    Done,
}

struct QuotaLoader {
    rx: mpsc::Receiver<QuotaEvent>,
    pending: HashSet<usize>,
    selected_index: usize,
    selected_pending: bool,
    finished: bool,
    _handle: JoinHandle<()>,
}

impl QuotaLoader {
    fn spawn(
        paths: &AswitchPaths,
        accounts: &[AccountRecord],
        window: UsageWindow,
        source: UsageSelection,
        selected: usize,
        refresh: bool,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let pending: HashSet<usize> = (0..accounts.len()).collect();
        let paths_clone = paths.clone();
        let accounts_clone: Vec<(String, String)> = accounts
            .iter()
            .map(|a| (a.plugin_id.clone(), a.alias.clone()))
            .collect();
        let handle = thread::spawn(move || {
            run_quota_worker(
                tx,
                paths_clone,
                accounts_clone,
                window,
                source,
                selected,
                refresh,
            )
        });
        Self {
            rx,
            pending,
            selected_index: selected,
            selected_pending: !accounts.is_empty(),
            finished: accounts.is_empty(),
            _handle: handle,
        }
    }

    fn drain(
        &mut self,
        account_quota: &mut [AccountQuotaSummary],
        usage: &mut Option<UsageSnapshot>,
        accounts: &mut [AccountRecord],
    ) -> bool {
        let mut updated = false;
        loop {
            match self.rx.try_recv() {
                Ok(QuotaEvent::Identities {
                    emails,
                    org_names,
                    plans,
                }) => {
                    for account in accounts.iter_mut() {
                        let key = (account.plugin_id.clone(), account.alias.clone());
                        if let Some(value) = emails.get(&key).cloned() {
                            if account.email.is_none() {
                                account.email = value;
                            }
                        }
                        if let Some(value) = org_names.get(&key).cloned() {
                            if account.org_name.is_none() {
                                account.org_name = value;
                            }
                        }
                        if let Some(value) = plans.get(&key).cloned() {
                            if account.plan.is_none() {
                                account.plan = value;
                            }
                        }
                    }
                    updated = true;
                }
                Ok(QuotaEvent::Quota { index, summary }) => {
                    if let Some(slot) = account_quota.get_mut(index) {
                        *slot = summary;
                    }
                    self.pending.remove(&index);
                    updated = true;
                }
                Ok(QuotaEvent::SelectedUsage { index, snapshot }) => {
                    if index == self.selected_index {
                        *usage = Some(snapshot);
                        self.selected_pending = false;
                        updated = true;
                    }
                }
                Ok(QuotaEvent::Done) => {
                    self.finished = true;
                    self.selected_pending = false;
                    updated = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.finished = true;
                    self.selected_pending = false;
                    break;
                }
            }
        }
        updated
    }

    fn is_loading(&self, index: usize) -> bool {
        self.pending.contains(&index)
    }

    fn is_active(&self) -> bool {
        !self.finished
    }
}

fn run_quota_worker(
    tx: mpsc::Sender<QuotaEvent>,
    paths: AswitchPaths,
    accounts: Vec<(String, String)>,
    window: UsageWindow,
    source: UsageSelection,
    selected: usize,
    refresh: bool,
) {
    // Live identity (Claude info / Gemini Code Assist) is the first thing
    // the background does, so rows pick up email/plan as soon as possible.
    if let Ok(effective_live) = scope::effective_accounts(&paths, None) {
        let mut saved_rows = match account::list_accounts_with_config_dir(
            Some(accounts::config_dir(&paths)),
            None,
        ) {
            Ok(rows) => rows,
            Err(_) => Vec::new(),
        };
        live::enrich_saved_accounts(&mut saved_rows, &effective_live);

        let mut emails = BTreeMap::new();
        let mut org_names = BTreeMap::new();
        let mut plans = BTreeMap::new();
        for row in &saved_rows {
            let key = (row.plugin_id.clone(), row.alias.clone());
            emails.insert(key.clone(), row.email.clone());
            org_names.insert(key.clone(), row.org_name.clone());
            plans.insert(key, row.plan.clone());
        }
        if tx
            .send(QuotaEvent::Identities {
                emails,
                org_names,
                plans,
            })
            .is_err()
        {
            return;
        }
    }

    let effective = scope::effective_targets_offline(&paths, None).unwrap_or_default();
    // Process the selected account first so the details panel populates quickly.
    let order: Vec<usize> = std::iter::once(selected)
        .filter(|i| *i < accounts.len())
        .chain((0..accounts.len()).filter(|i| *i != selected))
        .collect();

    let config_dir = accounts::config_dir(&paths);
    for index in order {
        let (plugin_id, alias) = &accounts[index];
        let snapshot = usage::collect_usage_with_config_dir(
            plugin_id,
            alias,
            Some(config_dir.clone()),
            CollectUsageOptions {
                window: Some(window),
                source: Some(source),
                refresh,
            },
        );

        let summary = match snapshot {
            Ok(mut snapshot) => {
                let is_effective = effective.iter().any(|(p, a)| p == plugin_id && a == alias);
                live::enrich_usage_snapshot(&mut snapshot, Some(config_dir.clone()), is_effective);
                let summary = quota_summary_from_snapshot(&snapshot);
                if index == selected {
                    if tx
                        .send(QuotaEvent::SelectedUsage {
                            index,
                            snapshot: snapshot.clone(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                summary
            }
            Err(_) => AccountQuotaSummary::default(),
        };

        if tx.send(QuotaEvent::Quota { index, summary }).is_err() {
            return;
        }
    }
    let _ = tx.send(QuotaEvent::Done);
}

#[derive(Clone, Copy)]
enum AccountTableLayout {
    Compact,
    Wide,
}

#[derive(Clone, Debug, Default)]
struct AccountQuotaSummary {
    remaining_percent: Option<f64>,
    used_percent: Option<f64>,
    reset_time: Option<String>,
    session_remaining_percent: Option<f64>,
    weekly_remaining_percent: Option<f64>,
}

impl AccountQuotaSummary {
    fn has_display(&self) -> bool {
        self.remaining_percent.is_some()
            || self.used_percent.is_some()
            || self.reset_time.is_some()
            || self.session_remaining_percent.is_some()
            || self.weekly_remaining_percent.is_some()
    }
}

impl App {
    fn load(paths: &AswitchPaths) -> Result<Self> {
        // Keep startup synchronous-only: no live API calls. The background
        // loader picks up live identity + quota right after first render.
        let accounts =
            account::list_accounts_with_config_dir(Some(accounts::config_dir(paths)), None)?;
        let plugins = account::status_with_config_dir(Some(accounts::config_dir(paths)))?.plugins;
        let account_quota = vec![AccountQuotaSummary::default(); accounts.len()];
        let usage_window = UsageWindow::CurrentMonth;
        let usage_source = UsageSelection::Both;
        let selected = 0usize;
        let quota_loader = if accounts.is_empty() {
            None
        } else {
            Some(QuotaLoader::spawn(
                paths,
                &accounts,
                usage_window,
                usage_source,
                selected,
                false,
            ))
        };
        let status = if quota_loader.is_some() {
            "Loading usage in the background. Enter switches; w/s/R update usage.".to_string()
        } else {
            "Select an account. Enter switches globally; w/s/R update usage.".to_string()
        };
        Ok(Self {
            panel: Panel::Switch,
            accounts,
            account_quota,
            selected,
            usage_window,
            usage_source,
            usage: None,
            plugins,
            status,
            help_open: false,
            should_quit: false,
            quota_loader,
            spinner_tick: 0,
        })
    }

    fn restart_quota_loader(&mut self, paths: &AswitchPaths, refresh: bool) {
        if self.accounts.is_empty() {
            self.quota_loader = None;
            self.account_quota.clear();
            self.usage = None;
            return;
        }
        self.account_quota = vec![AccountQuotaSummary::default(); self.accounts.len()];
        self.usage = None;
        self.quota_loader = Some(QuotaLoader::spawn(
            paths,
            &self.accounts,
            self.usage_window,
            self.usage_source,
            self.selected,
            refresh,
        ));
    }

    fn drain_quota_events(&mut self) -> bool {
        let Some(loader) = self.quota_loader.as_mut() else {
            return false;
        };
        let updated = loader.drain(&mut self.account_quota, &mut self.usage, &mut self.accounts);
        if !loader.is_active() {
            self.quota_loader = None;
        }
        updated
    }

    fn quota_is_loading_for(&self, index: usize) -> bool {
        self.quota_loader
            .as_ref()
            .map(|loader| loader.is_loading(index))
            .unwrap_or(false)
    }

    fn quota_is_loading(&self) -> bool {
        self.quota_loader
            .as_ref()
            .map(QuotaLoader::is_active)
            .unwrap_or(false)
    }

    fn tick(&mut self) {
        let was_loading = self.quota_is_loading();
        let _ = self.drain_quota_events();
        if was_loading {
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
            if !self.quota_is_loading() {
                self.status = "Usage ready.".to_string();
            }
        }
    }

    fn spinner_glyph(&self) -> &'static str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[self.spinner_tick % FRAMES.len()]
    }

    fn render(&self, frame: &mut ratatui::Frame) {
        let theme = Theme::default();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Min(1),
                Constraint::Length(4),
            ])
            .split(frame.area());

        self.render_header(frame, chunks[0], theme);

        match self.panel {
            Panel::Switch => self.render_switch(frame, chunks[1], theme),
            Panel::Plugins => self.render_plugins(frame, chunks[1], theme),
        }

        self.render_footer(frame, chunks[2], theme);

        if self.help_open {
            self.render_help(frame, theme);
        }
    }

    fn render_header(&self, frame: &mut ratatui::Frame, area: Rect, theme: Theme) {
        let active_accounts = self
            .accounts
            .iter()
            .filter(|account| account.active)
            .count();
        let loaded_plugins = self.plugins.iter().filter(|plugin| plugin.loaded).count();
        let top_line = Line::from(vec![
            badge("ASWITCH", theme.text, theme.accent),
            Span::raw("  "),
            Span::styled(
                format!("{} accounts", self.accounts.len()),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{} live", active_accounts),
                Style::default().fg(theme.success),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{} plugins", loaded_plugins),
                Style::default().fg(theme.muted),
            ),
            Span::raw("   "),
            nav_pill("1 Switch", self.panel == Panel::Switch, theme),
            Span::raw(" "),
            nav_pill("2 Plugins", self.panel == Panel::Plugins, theme),
            Span::raw("  "),
            if self.quota_is_loading() {
                Span::styled(
                    format!("{} loading usage…", self.spinner_glyph()),
                    Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            },
        ]);
        let bottom_line = header_selected_line(
            theme,
            self.accounts.get(self.selected),
            self.selected_quota_summary().as_ref(),
            area.width.saturating_sub(4) as usize,
        );

        frame.render_widget(
            Paragraph::new(vec![top_line, bottom_line])
                .style(Style::default().fg(theme.text).bg(theme.header_bg))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme.border))
                        .style(Style::default().bg(theme.header_bg)),
                )
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_footer(&self, frame: &mut ratatui::Frame, area: Rect, theme: Theme) {
        let status = truncate_text(&self.status, area.width.saturating_sub(12) as usize);
        let lines = vec![
            Line::from(vec![
                Span::styled(
                    "Keys  ",
                    Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.shortcut_text(), Style::default().fg(theme.text)),
            ]),
            Line::from(vec![
                Span::styled(
                    "Status ",
                    Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(status, status_style(theme, &self.status)),
            ]),
        ];

        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(panel_block(theme, "Hints")),
            area,
        );
    }

    fn render_switch(&self, frame: &mut ratatui::Frame, area: Rect, theme: Theme) {
        let sections = if area.width >= 118 {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
                .split(area)
        } else {
            let detail_height = area.height.saturating_sub(8).clamp(9, 11);
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(7), Constraint::Length(detail_height)])
                .split(area)
        };

        self.render_accounts_table(frame, sections[0], theme);
        self.render_selection_details(frame, sections[1], theme);
    }

    fn render_accounts_table(&self, frame: &mut ratatui::Frame, area: Rect, theme: Theme) {
        if self.accounts.is_empty() {
            frame.render_widget(
                Paragraph::new("No saved accounts yet. Use `aswitch save` to capture one.")
                    .wrap(Wrap { trim: true })
                    .block(panel_block(theme, "Accounts")),
                area,
            );
            return;
        }

        let layout = detect_account_table_layout(&self.accounts, area.width);
        let title = format!(
            "Accounts {} saved / {} live",
            self.accounts.len(),
            self.accounts
                .iter()
                .filter(|account| account.active)
                .count()
        );

        let rows = self.accounts.iter().enumerate().map(|(index, account)| {
            let base_style = if index == self.selected {
                Style::default()
                    .fg(theme.selected_fg)
                    .bg(theme.selected_bg)
                    .add_modifier(Modifier::BOLD)
            } else if index % 2 == 0 {
                Style::default().fg(theme.text)
            } else {
                Style::default().fg(theme.muted)
            };

            let marker = Cell::from(Span::styled(
                if index == self.selected { ">" } else { " " },
                if index == self.selected {
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.selected_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.border)
                },
            ));

            let state = Cell::from(Span::styled(
                if account.active { "LIVE" } else { "saved" },
                if account.active {
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.muted)
                },
            ));
            let quota = self.account_quota_for_row(index);
            let pending = self.quota_is_loading_for(index);
            let (left, reset) = if pending && !quota.has_display() {
                (
                    pending_cell(theme, self.spinner_glyph(), index == self.selected),
                    pending_cell(theme, "loading", index == self.selected),
                )
            } else {
                (
                    quota_left_cell(theme, &quota, index == self.selected),
                    quota_reset_cell(theme, &quota, index == self.selected),
                )
            };

            let row = match layout {
                AccountTableLayout::Compact => Row::new(vec![
                    marker,
                    Cell::from(truncate_text(
                        &format!("{}/{}", account.plugin_id, account.alias),
                        28,
                    )),
                    Cell::from(truncate_text(account_identity(account), 28)),
                    left,
                    reset,
                    state,
                ]),
                AccountTableLayout::Wide => Row::new(vec![
                    marker,
                    Cell::from(account.plugin_id.clone()),
                    Cell::from(account.alias.clone()),
                    Cell::from(account_identity(account).to_string()),
                    left,
                    reset,
                    state,
                ]),
            };

            row.style(base_style)
        });

        let (header, widths) = match layout {
            AccountTableLayout::Compact => (
                account_table_header(
                    theme,
                    vec!["", "ACCOUNT", "IDENTITY", "LEFT now/wk", "RESET", "STATE"],
                ),
                vec![
                    Constraint::Length(2),
                    Constraint::Length(28),
                    Constraint::Min(14),
                    Constraint::Length(11),
                    Constraint::Length(12),
                    Constraint::Length(8),
                ],
            ),
            AccountTableLayout::Wide => (
                account_table_header(
                    theme,
                    vec![
                        "",
                        "PLUGIN",
                        "ALIAS",
                        "IDENTITY",
                        "LEFT now/wk",
                        "RESET",
                        "STATE",
                    ],
                ),
                vec![
                    Constraint::Length(2),
                    Constraint::Length(14),
                    Constraint::Length(16),
                    Constraint::Min(18),
                    Constraint::Length(11),
                    Constraint::Length(12),
                    Constraint::Length(8),
                ],
            ),
        };

        frame.render_widget(
            Table::new(rows, widths)
                .header(header)
                .block(panel_block(theme, title.as_str())),
            area,
        );
    }

    fn render_selection_details(&self, frame: &mut ratatui::Frame, area: Rect, theme: Theme) {
        let body = if let Some(account) = self.accounts.get(self.selected) {
            let last_used = account
                .last_used_at
                .as_ref()
                .map(|time| {
                    time.with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| "-".to_string());
            let compact = area.height <= 10;
            let meta = account_metadata_summary(account, area.width.saturating_sub(12) as usize);

            if compact {
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled(
                            format!("{}/{}", account.plugin_id, account.alias),
                            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        badge(
                            if account.active { "ACTIVE" } else { "saved" },
                            theme.text,
                            if account.active {
                                theme.success
                            } else {
                                theme.border
                            },
                        ),
                    ]),
                    detail_line(theme, "Who", account_identity(account)),
                ];

                if meta != "-" {
                    lines.push(detail_line(theme, "Meta", meta.as_str()));
                }

                if let Some(snapshot) = &self.usage {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "Last  ",
                            Style::default()
                                .fg(theme.accent_soft)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(last_used, Style::default().fg(theme.text)),
                        Span::raw(" "),
                        badge(snapshot.window.as_str(), theme.text, theme.border),
                    ]));
                    lines.push(metric_line(
                        theme,
                        [
                            ("Left", quota_left_summary(snapshot)),
                            ("Reset", quota_reset_brief(snapshot)),
                            (
                                "Mode",
                                format!(
                                    "{}{}",
                                    snapshot.source.as_str(),
                                    if snapshot.cached { " cached" } else { "" }
                                ),
                            ),
                        ],
                    ));
                    lines.push(metric_line(
                        theme,
                        [
                            ("Req", display_metric(snapshot.metrics.requests)),
                            ("In", display_metric(snapshot.metrics.tokens_in)),
                            ("Out", display_metric(snapshot.metrics.tokens_out)),
                        ],
                    ));
                    lines.push(metric_line(
                        theme,
                        [
                            ("Cost", display_cost(snapshot.metrics.cost_usd)),
                            (
                                "Quota",
                                truncate_text(
                                    &usage_cmd::format_quota(&snapshot.quota),
                                    area.width.saturating_sub(24) as usize,
                                ),
                            ),
                            ("Srcs", snapshot.sources.len().to_string()),
                        ],
                    ));
                    lines.push(detail_line(
                        theme,
                        "Source",
                        format_source_summary(
                            &snapshot.sources,
                            area.width.saturating_sub(12) as usize,
                        )
                        .as_str(),
                    ));
                    let footer = if snapshot.warnings.is_empty() {
                        format_extra_metrics(
                            &snapshot.metrics.extra,
                            area.width.saturating_sub(12) as usize,
                        )
                    } else {
                        format_notes(&snapshot.warnings, area.width.saturating_sub(12) as usize)
                    };
                    lines.push(detail_line(
                        theme,
                        if snapshot.warnings.is_empty() {
                            "Extra"
                        } else {
                            "Notes"
                        },
                        footer.as_str(),
                    ));
                } else {
                    lines.push(detail_line(theme, "Last", last_used.as_str()));
                    lines.push(detail_line(theme, "Usage", "No usage snapshot loaded"));
                }

                lines
            } else {
                let mut lines = vec![Line::from(vec![
                    Span::styled(
                        format!("{}/{}", account.plugin_id, account.alias),
                        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    badge(
                        if account.active { "ACTIVE" } else { "saved" },
                        theme.text,
                        if account.active {
                            theme.success
                        } else {
                            theme.border
                        },
                    ),
                ])];

                for line in account_profile_lines(theme, account) {
                    lines.push(line);
                }

                if let Some(snapshot) = &self.usage {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "Last  ",
                            Style::default()
                                .fg(theme.accent_soft)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(last_used, Style::default().fg(theme.text)),
                        Span::raw("  "),
                        badge(snapshot.window.as_str(), theme.text, theme.border),
                        Span::raw(" "),
                        badge(snapshot.source.as_str(), theme.text, theme.accent),
                        Span::raw(" "),
                        badge(
                            if snapshot.cached { "cached" } else { "live" },
                            theme.text,
                            if snapshot.cached {
                                theme.accent_soft
                            } else {
                                theme.success
                            },
                        ),
                    ]));
                    lines.push(metric_line(
                        theme,
                        [
                            ("Left", quota_left_summary(snapshot)),
                            ("Reset", quota_reset_brief(snapshot)),
                            (
                                "Srcs",
                                if snapshot.sources.is_empty() {
                                    "0".to_string()
                                } else {
                                    snapshot.sources.len().to_string()
                                },
                            ),
                        ],
                    ));
                    lines.push(metric_line(
                        theme,
                        [
                            ("Req", display_metric(snapshot.metrics.requests)),
                            ("In", display_metric(snapshot.metrics.tokens_in)),
                            ("Out", display_metric(snapshot.metrics.tokens_out)),
                        ],
                    ));
                    lines.push(metric_line(
                        theme,
                        [
                            ("Cost", display_cost(snapshot.metrics.cost_usd)),
                            (
                                "Quota",
                                truncate_text(
                                    &usage_cmd::format_quota(&snapshot.quota),
                                    area.width.saturating_sub(28) as usize,
                                ),
                            ),
                            ("Mode", snapshot.source.as_str().to_string()),
                        ],
                    ));
                    lines.push(detail_line(
                        theme,
                        "Extra",
                        format_extra_metrics(
                            &snapshot.metrics.extra,
                            area.width.saturating_sub(12) as usize,
                        )
                        .as_str(),
                    ));
                    lines.push(detail_line(
                        theme,
                        "Source",
                        format_source_summary(
                            &snapshot.sources,
                            area.width.saturating_sub(12) as usize,
                        )
                        .as_str(),
                    ));
                    lines.push(detail_line(
                        theme,
                        "Notes",
                        format_notes(&snapshot.warnings, area.width.saturating_sub(12) as usize)
                            .as_str(),
                    ));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "Last  ",
                            Style::default()
                                .fg(theme.accent_soft)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(last_used, Style::default().fg(theme.text)),
                    ]));
                    lines.push(detail_line(theme, "Usage", "No usage snapshot loaded"));
                }

                lines
            }
        } else {
            vec![Line::from(Span::styled(
                "No saved accounts.",
                Style::default().fg(theme.muted),
            ))]
        };

        frame.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: true })
                .block(panel_block(theme, "Selected Account")),
            area,
        );
    }

    fn render_plugins(&self, frame: &mut ratatui::Frame, area: Rect, theme: Theme) {
        if self.plugins.is_empty() {
            frame.render_widget(
                Paragraph::new("No plugins discovered.")
                    .wrap(Wrap { trim: true })
                    .block(panel_block(theme, "Plugins")),
                area,
            );
            return;
        }

        let compact = area.width < 96;
        let title = format!(
            "Plugins {} discovered / {} loaded",
            self.plugins.len(),
            self.plugins.iter().filter(|plugin| plugin.loaded).count()
        );
        let rows = self.plugins.iter().enumerate().map(|(index, plugin)| {
            let row_style = if index % 2 == 0 {
                Style::default().fg(theme.text)
            } else {
                Style::default().fg(theme.muted)
            };
            let (status_label, status_style) = plugin_status(plugin, theme);

            let row = if compact {
                Row::new(vec![
                    Cell::from(plugin.plugin_id.clone()),
                    Cell::from(
                        plugin
                            .active_alias
                            .clone()
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    Cell::from(plugin.account_count.to_string()),
                    Cell::from(Span::styled(status_label, status_style)),
                ])
            } else {
                Row::new(vec![
                    Cell::from(plugin.plugin_id.clone()),
                    Cell::from(
                        plugin
                            .display_name
                            .clone()
                            .unwrap_or_else(|| plugin.plugin_id.clone()),
                    ),
                    Cell::from(
                        plugin
                            .active_alias
                            .clone()
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    Cell::from(plugin.account_count.to_string()),
                    Cell::from(plugin.source.clone().unwrap_or_else(|| "-".to_string())),
                    Cell::from(plugin.version.clone().unwrap_or_else(|| "-".to_string())),
                    Cell::from(Span::styled(status_label, status_style)),
                ])
            };

            row.style(row_style)
        });

        let (header, widths) = if compact {
            (
                Row::new(vec!["ID", "ACTIVE", "ACCTS", "STATUS"]).style(
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.surface_alt)
                        .add_modifier(Modifier::BOLD),
                ),
                vec![
                    Constraint::Length(16),
                    Constraint::Length(16),
                    Constraint::Length(7),
                    Constraint::Length(10),
                ],
            )
        } else {
            (
                Row::new(vec![
                    "ID", "NAME", "ACTIVE", "ACCTS", "SOURCE", "VERSION", "STATUS",
                ])
                .style(
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.surface_alt)
                        .add_modifier(Modifier::BOLD),
                ),
                vec![
                    Constraint::Length(14),
                    Constraint::Length(22),
                    Constraint::Length(16),
                    Constraint::Length(7),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(10),
                ],
            )
        };

        frame.render_widget(
            Table::new(rows, widths)
                .header(header)
                .block(panel_block(theme, title.as_str())),
            area,
        );
    }

    fn render_help(&self, frame: &mut ratatui::Frame, theme: Theme) {
        let area = centered_rect(78, 58, frame.area());
        let lines = vec![
            Line::from(vec![badge("Global", theme.text, theme.accent)]),
            Line::from("q quit | ? close help | Tab or 1/2 switch panels"),
            Line::from(""),
            Line::from(vec![badge("Switch", theme.text, theme.accent_soft)]),
            Line::from("j/k or Up/Down move selection"),
            Line::from("Enter switches the selected account globally"),
            Line::from("w changes usage window | s changes usage source | R refreshes usage"),
            Line::from(""),
            Line::from(vec![badge("Plugins", theme.text, theme.border)]),
            Line::from("Read-only view of loaded plugin manifests and active aliases"),
        ];

        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(panel_block(theme, "Help")),
            area,
        );
    }

    fn handle_key(&mut self, paths: &AswitchPaths, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Char('?') => {
                self.help_open = !self.help_open;
                self.status = if self.help_open {
                    "Help is open. Press ? to close it.".to_string()
                } else {
                    "Help closed.".to_string()
                };
            }
            KeyCode::Char('1') => {
                self.panel = Panel::Switch;
                self.status = "Switched to Switch.".to_string();
            }
            KeyCode::Char('2') => {
                self.panel = Panel::Plugins;
                self.status = "Switched to Plugins.".to_string();
            }
            KeyCode::Tab => {
                self.panel = match self.panel {
                    Panel::Switch => Panel::Plugins,
                    Panel::Plugins => Panel::Switch,
                };
                self.status = format!("Switched to {}.", self.panel_name());
            }
            KeyCode::Down | KeyCode::Char('j') if self.panel == Panel::Switch => {
                if !self.accounts.is_empty() {
                    let new_selected =
                        (self.selected + 1).min(self.accounts.len().saturating_sub(1));
                    if new_selected != self.selected {
                        self.selected = new_selected;
                        self.on_selection_changed(paths);
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') if self.panel == Panel::Switch => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.on_selection_changed(paths);
                }
            }
            KeyCode::Enter if self.panel == Panel::Switch => {
                if let Some(account) = self.accounts.get(self.selected) {
                    let plugin_id = account.plugin_id.clone();
                    let alias = account.alias.clone();
                    switch::use_account_with_config_dir(
                        &plugin_id,
                        &alias,
                        Some(accounts::config_dir(paths)),
                    )?;
                    self.accounts = account::list_accounts_with_config_dir(
                        Some(accounts::config_dir(paths)),
                        None,
                    )?;
                    self.plugins =
                        account::status_with_config_dir(Some(accounts::config_dir(paths)))?.plugins;
                    self.status = format!(
                        "Switched to {plugin_id}/{alias}. Restart the corresponding client."
                    );
                    self.restart_quota_loader(paths, true);
                }
            }
            KeyCode::Char('w') if self.panel == Panel::Switch => {
                self.usage_window = match self.usage_window {
                    UsageWindow::Today => UsageWindow::Last24h,
                    UsageWindow::Last24h => UsageWindow::Last7d,
                    UsageWindow::Last7d => UsageWindow::CurrentMonth,
                    UsageWindow::CurrentMonth => UsageWindow::Last30d,
                    UsageWindow::Last30d => UsageWindow::All,
                    UsageWindow::All => UsageWindow::Today,
                };
                self.status = format!("Usage window: {}", self.usage_window.as_str());
                self.restart_quota_loader(paths, false);
            }
            KeyCode::Char('s') if self.panel == Panel::Switch => {
                self.usage_source = match self.usage_source {
                    UsageSelection::Local => UsageSelection::Api,
                    UsageSelection::Api => UsageSelection::Both,
                    UsageSelection::Both => UsageSelection::Local,
                };
                self.status = format!("Usage source: {}", self.usage_source.as_str());
                self.restart_quota_loader(paths, false);
            }
            KeyCode::Char('R') if self.panel == Panel::Switch => {
                self.status = "Refreshing usage…".to_string();
                self.restart_quota_loader(paths, true);
            }
            _ => {}
        }

        Ok(())
    }

    fn on_selection_changed(&mut self, paths: &AswitchPaths) {
        self.status = format!("Selected {}.", self.selected_account_label());
        // Drop the previous selected snapshot; it belonged to a different account.
        self.usage = None;
        // Try to fast-path from cache without enrichment so the details panel
        // shows tokens immediately. Provider-API enrichment is deferred to the
        // background loader, which we kick off again so the `selected` target
        // gets the live quota.
        if let Some(account) = self.accounts.get(self.selected) {
            if let Ok(snapshot) = usage::collect_usage_with_config_dir(
                &account.plugin_id,
                &account.alias,
                Some(accounts::config_dir(paths)),
                CollectUsageOptions {
                    window: Some(self.usage_window),
                    source: Some(self.usage_source),
                    refresh: false,
                },
            ) {
                self.usage = Some(snapshot);
            }
        }
        self.restart_quota_loader(paths, false);
    }

    fn account_quota_for_row(&self, index: usize) -> AccountQuotaSummary {
        self.account_quota.get(index).cloned().unwrap_or_default()
    }

    fn selected_quota_summary(&self) -> Option<AccountQuotaSummary> {
        self.usage
            .as_ref()
            .map(quota_summary_from_snapshot)
            .filter(AccountQuotaSummary::has_display)
            .or_else(|| {
                self.account_quota
                    .get(self.selected)
                    .cloned()
                    .filter(AccountQuotaSummary::has_display)
            })
    }

    fn shortcut_text(&self) -> &'static str {
        match self.panel {
            Panel::Switch => "q quit | ? help | Tab panel | j/k move | Enter switch | w/s/R usage",
            Panel::Plugins => "q quit | ? help | Tab panel | 1/2 jump panels",
        }
    }

    fn panel_name(&self) -> &'static str {
        match self.panel {
            Panel::Switch => "Switch",
            Panel::Plugins => "Plugins",
        }
    }

    fn selected_account_label(&self) -> String {
        self.accounts
            .get(self.selected)
            .map(|account| format!("{}/{}", account.plugin_id, account.alias))
            .unwrap_or_else(|| "no account".to_string())
    }
}

fn panel_block<'a>(theme: Theme, title: &'a str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.surface))
        .title(Span::styled(
            format!(" {} ", title),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
}

fn account_table_header<'a>(theme: Theme, cells: Vec<&'a str>) -> Row<'a> {
    Row::new(cells).style(
        Style::default()
            .fg(theme.accent_soft)
            .bg(theme.surface_alt)
            .add_modifier(Modifier::BOLD),
    )
}

fn badge(label: impl Into<String>, fg: Color, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", label.into()),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

fn nav_pill(label: &str, selected: bool, theme: Theme) -> Span<'static> {
    if selected {
        badge(label, theme.text, theme.selected_bg)
    } else {
        Span::styled(
            format!(" {} ", label),
            Style::default().fg(theme.muted).bg(theme.header_bg),
        )
    }
}

fn header_selected_line(
    theme: Theme,
    account: Option<&AccountRecord>,
    quota: Option<&AccountQuotaSummary>,
    max_chars: usize,
) -> Line<'static> {
    let Some(account) = account else {
        return Line::from(vec![
            badge("Selected", theme.text, theme.surface_alt),
            Span::raw(" "),
            Span::styled("No saved accounts", Style::default().fg(theme.muted)),
        ]);
    };

    let mut summary = format!("{}/{}", account.plugin_id, account.alias);
    let identity = account_identity(account);
    if identity != "-" {
        summary.push_str("  ");
        summary.push_str(identity);
    }
    let summary = truncate_text(&summary, max_chars.saturating_sub(18));

    let mut spans = vec![
        badge("Selected", theme.text, theme.surface_alt),
        Span::raw(" "),
        Span::styled(summary, Style::default().fg(theme.text)),
    ];

    if let Some(summary) = quota {
        if let Some(left) = quota_left_badge(summary) {
            spans.push(Span::raw(" "));
            spans.push(badge(
                left,
                theme.text,
                quota_usage_color(theme, quota_used_from_summary(summary).unwrap_or(0.0)),
            ));
        }

        if let Some(reset_time) = summary.reset_time.as_deref() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("reset {reset_time}"),
                Style::default().fg(theme.muted),
            ));
        }
    }

    Line::from(spans)
}

fn detail_line(theme: Theme, label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<6}"),
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_string(), Style::default().fg(theme.text)),
    ])
}

fn account_identity(account: &AccountRecord) -> &str {
    account
        .email
        .as_deref()
        .or(account.plan.as_deref())
        .unwrap_or("-")
}

fn account_profile_lines(theme: Theme, account: &AccountRecord) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if let Some(email) = account.email.as_deref() {
        lines.push(detail_line(theme, "Email", email));
    }
    if let Some(plan) = account.plan.as_deref() {
        lines.push(detail_line(theme, "Plan", plan));
    }

    if lines.is_empty() {
        lines.push(detail_line(theme, "Profile", "No identity metadata"));
    }

    lines
}

fn metric_line<const N: usize>(theme: Theme, items: [(&str, String); N]) -> Line<'static> {
    let mut spans = Vec::with_capacity(N * 4);
    for (index, (label, value)) in items.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" | ", Style::default().fg(theme.border)));
        }
        spans.push(Span::styled(
            format!("{label} "),
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(value, Style::default().fg(theme.text)));
    }
    Line::from(spans)
}

fn plugin_status(plugin: &PluginStatusRow, theme: Theme) -> (&'static str, Style) {
    if !plugin.loaded {
        (
            "missing",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )
    } else if !plugin.warnings.is_empty() {
        (
            "warn",
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "ok",
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
        )
    }
}

fn status_style(theme: Theme, message: &str) -> Style {
    let lowercase = message.to_ascii_lowercase();
    if lowercase.contains("failed")
        || lowercase.contains("error")
        || lowercase.contains("unavailable")
    {
        Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD)
    } else if lowercase.contains("switched")
        || lowercase.contains("refreshed")
        || lowercase.contains("selected")
    {
        Style::default()
            .fg(theme.success)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    }
}

fn display_metric(value: Option<f64>) -> String {
    value.map(compact_number).unwrap_or_else(|| "-".to_string())
}

fn display_cost(value: Option<f64>) -> String {
    value
        .map(|value| {
            if value.abs() >= 1000.0 {
                format!("${}", compact_number(value))
            } else {
                format!("${value:.2}")
            }
        })
        .unwrap_or_else(|| "-".to_string())
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
        format!("{value:.2}")
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

fn display_percent(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}%", value as i64)
    } else {
        format!("{value:.1}%")
    }
}

fn quota_summary_from_snapshot(snapshot: &UsageSnapshot) -> AccountQuotaSummary {
    AccountQuotaSummary {
        remaining_percent: quota_remaining_percent(snapshot),
        used_percent: quota_used_percent(snapshot),
        reset_time: quota_reset_summary(snapshot),
        session_remaining_percent: window_remaining_percent(snapshot, "session"),
        weekly_remaining_percent: window_remaining_percent(snapshot, "weekly"),
    }
}

fn window_remaining_percent(snapshot: &UsageSnapshot, prefix: &str) -> Option<f64> {
    let remaining_key = format!("{prefix}_remaining_percent");
    snapshot
        .quota
        .get(&remaining_key)
        .and_then(Value::as_f64)
        .or_else(|| {
            let used_key = format!("{prefix}_used_percent");
            snapshot
                .quota
                .get(&used_key)
                .and_then(Value::as_f64)
                .map(|used| (100.0 - used).clamp(0.0, 100.0))
        })
}

fn quota_remaining_percent(snapshot: &UsageSnapshot) -> Option<f64> {
    snapshot
        .quota
        .get("remaining_percent")
        .and_then(Value::as_f64)
        .or_else(|| {
            snapshot
                .quota
                .get("used_percent")
                .and_then(Value::as_f64)
                .map(|used| 100.0 - used)
        })
}

fn quota_used_percent(snapshot: &UsageSnapshot) -> Option<f64> {
    snapshot
        .quota
        .get("used_percent")
        .and_then(Value::as_f64)
        .or_else(|| {
            snapshot
                .quota
                .get("remaining_percent")
                .and_then(Value::as_f64)
                .map(|remaining| 100.0 - remaining)
        })
}

fn quota_used_from_summary(summary: &AccountQuotaSummary) -> Option<f64> {
    summary
        .used_percent
        .or_else(|| summary.remaining_percent.map(|remaining| 100.0 - remaining))
}

fn quota_usage_color(theme: Theme, used_percent: f64) -> Color {
    if used_percent >= 85.0 {
        theme.warning
    } else if used_percent >= 60.0 {
        theme.accent_soft
    } else {
        theme.success
    }
}

fn quota_reset_summary(snapshot: &UsageSnapshot) -> Option<String> {
    let raw = snapshot.quota.get("reset_time")?.as_str()?;
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(
            parsed
                .with_timezone(&Local)
                .format("%m-%d %H:%M")
                .to_string(),
        );
    }

    Some(truncate_text(&usage_cmd::format_local_time(raw), 18))
}

fn quota_left_text(summary: &AccountQuotaSummary) -> String {
    let now = summary
        .session_remaining_percent
        .or(summary.remaining_percent);
    let weekly = summary.weekly_remaining_percent;
    match (now, weekly) {
        (Some(n), Some(w)) => format!(
            "{}/{}",
            display_percent_compact(n),
            display_percent_compact(w)
        ),
        (Some(n), None) => format!("{}/-", display_percent_compact(n)),
        (None, Some(w)) => format!("-/{}", display_percent_compact(w)),
        (None, None) => "-".to_string(),
    }
}

fn display_percent_compact(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}%", value as i64)
    } else {
        format!("{:.0}%", value)
    }
}

fn quota_left_badge(summary: &AccountQuotaSummary) -> Option<String> {
    let now = summary
        .session_remaining_percent
        .or(summary.remaining_percent);
    let weekly = summary.weekly_remaining_percent;
    match (now, weekly) {
        (Some(n), Some(w)) => Some(format!(
            "{} now / {} wk",
            display_percent(n),
            display_percent(w)
        )),
        (Some(n), None) => Some(format!("{} left", display_percent(n))),
        (None, Some(w)) => Some(format!("{} weekly", display_percent(w))),
        (None, None) => None,
    }
}

fn quota_left_summary(snapshot: &UsageSnapshot) -> String {
    let summary = quota_summary_from_snapshot(snapshot);
    quota_left_text(&summary)
}

fn quota_reset_brief(snapshot: &UsageSnapshot) -> String {
    quota_summary_from_snapshot(snapshot)
        .reset_time
        .unwrap_or_else(|| "-".to_string())
}

fn pending_cell(theme: Theme, text: &str, selected: bool) -> Cell<'static> {
    let style = if selected {
        Style::default()
            .fg(theme.selected_fg)
            .bg(theme.selected_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.accent_soft)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from(Span::styled(text.to_string(), style))
}

fn quota_left_cell(theme: Theme, summary: &AccountQuotaSummary, selected: bool) -> Cell<'static> {
    let style = if selected {
        Style::default()
            .fg(theme.selected_fg)
            .bg(theme.selected_bg)
            .add_modifier(Modifier::BOLD)
    } else if let Some(used_percent) = quota_used_from_summary(summary) {
        Style::default()
            .fg(quota_usage_color(theme, used_percent))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    };

    Cell::from(Span::styled(quota_left_text(summary), style))
}

fn quota_reset_cell(theme: Theme, summary: &AccountQuotaSummary, selected: bool) -> Cell<'static> {
    let value = summary
        .reset_time
        .clone()
        .unwrap_or_else(|| "-".to_string());
    let style = if selected {
        Style::default()
            .fg(theme.selected_fg)
            .bg(theme.selected_bg)
            .add_modifier(Modifier::BOLD)
    } else if summary.reset_time.is_some() {
        Style::default().fg(theme.text)
    } else {
        Style::default().fg(theme.muted)
    };

    Cell::from(Span::styled(value, style))
}

fn format_extra_metrics(metrics: &BTreeMap<String, f64>, max_chars: usize) -> String {
    if metrics.is_empty() {
        return "-".to_string();
    }

    let text = metrics
        .iter()
        .take(2)
        .map(|(name, value)| format!("{} {}", shorten_extra_name(name), compact_number(*value)))
        .collect::<Vec<_>>()
        .join(" | ");

    truncate_text(&text, max_chars)
}

fn shorten_extra_name(name: &str) -> String {
    match name {
        "cache_creation_tokens" => "cache_write".to_string(),
        "cache_read_tokens" => "cache_read".to_string(),
        other => other.replace('_', " "),
    }
}

fn format_source_summary(sources: &[UsageSourceSummary], max_chars: usize) -> String {
    let Some(first) = sources.first() else {
        return "-".to_string();
    };

    let mut text = match first.path_or_url.as_deref() {
        Some(path_or_url) => format!("{} {}", first.kind, path_or_url),
        None => first.kind.clone(),
    };
    if sources.len() > 1 {
        text.push_str(&format!(" +{}", sources.len() - 1));
    }

    truncate_text(&text, max_chars)
}

fn format_notes(warnings: &[String], max_chars: usize) -> String {
    if warnings.is_empty() {
        return "clean".to_string();
    }

    truncate_text(&warnings.join(" | "), max_chars)
}

fn account_metadata_summary(account: &AccountRecord, max_chars: usize) -> String {
    let mut items = Vec::new();

    if let Some(plan) = account.plan.as_deref() {
        items.push(plan.to_string());
    }

    if items.is_empty() {
        return "-".to_string();
    }

    truncate_text(&items.join(" | "), max_chars)
}

fn detect_account_table_layout(accounts: &[AccountRecord], width: u16) -> AccountTableLayout {
    let _ = accounts;
    if width < 104 {
        AccountTableLayout::Compact
    } else {
        AccountTableLayout::Wide
    }
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let length = value.chars().count();
    if length <= max_chars {
        return value.to_string();
    }

    if max_chars <= 3 {
        return value.chars().take(max_chars).collect();
    }

    let kept = max_chars - 3;
    let mut output = value.chars().take(kept).collect::<String>();
    output.push_str("...");
    output
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::{compact_number, truncate_text};

    #[test]
    fn compact_number_scales_large_values() {
        assert_eq!(compact_number(8_524.0), "8.5k");
        assert_eq!(compact_number(100_490.0), "100.5k");
        assert_eq!(compact_number(7_977_315.0), "8M");
    }

    #[test]
    fn truncate_text_adds_ellipsis() {
        assert_eq!(truncate_text("abcdefghij", 7), "abcd...");
        assert_eq!(truncate_text("short", 10), "short");
    }
}
