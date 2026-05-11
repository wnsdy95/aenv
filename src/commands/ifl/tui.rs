//! ratatui TUI for `aenv ifl`. Two screens, drill-in/out, state-
//! preserving across navigation, sticky footer with submit hint.
//!
//! Layout (verified pattern: `Layout::vertical([Min(0), Length(N)])`
//! mirroring lazygit's status bar and fzf's `--preview` split):
//!   ┌── body ──────────────────────────────┐
//!   │  scrolling list                      │
//!   │  ...                                 │
//!   ├──────────────────────────────────────┤
//!   │  hints (sticky footer, fixed height) │
//!   └──────────────────────────────────────┘
//!
//! Multi-select pattern: ratatui `List` is single-select, so we keep
//! checked-item state (`HashSet<String>` per (env, kind)) and render
//! `[x] name` / `[ ] name` ourselves. Same approach the ratatui list
//! example documents.

use std::collections::{BTreeMap, HashSet};
use std::io;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

use super::{Plan, Source};

/// Top-level TUI entry. Returns the assembled Plan or an empty Plan
/// if the user quit. Always restores terminal state via the RAII
/// `TerminalGuard` (handles panic + Ctrl-C path).
///
/// `target_state` is the set of items currently in the target env's
/// manifest. The TUI uses it to:
///   - pre-check items already in the target (so re-importing is a
///     no-op rather than a duplicate-add error),
///   - render an `(in target)` marker so the user can tell at a
///     glance what's already pinned,
///   - emit `Plan::remove_*` entries when the user explicitly
///     unchecks a pre-checked item (= "I don't want this in the
///     manifest anymore"). New shape: ifl is bidirectional, not
///     just import.
pub fn run_tui(sources: &[Source], target_state: TargetState) -> Result<Plan> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("init terminal")?;

    let mut app = App::new(sources, target_state);
    let outcome = event_loop(&mut terminal, &mut app)?;
    Ok(if outcome == Outcome::Submit {
        app.into_plan()
    } else {
        Plan::default()
    })
}

/// Names of items currently in the target env's manifest. Used by
/// the TUI to pre-check existing items and emit removes when the
/// user unchecks them.
#[derive(Debug, Clone, Default)]
pub struct TargetState {
    pub plugins: HashSet<String>,
    pub skills: HashSet<String>,
    pub mcps: HashSet<String>,
}

impl TargetState {
    fn contains(&self, kind: Kind, name: &str) -> bool {
        match kind {
            Kind::Plugin => self.plugins.contains(name),
            Kind::Skill => self.skills.contains(name),
            Kind::Mcp => self.mcps.contains(name),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Submit,
    Cancel,
}

// =====================================================================
//   App state
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Kind {
    Plugin,
    Skill,
    Mcp,
}

#[derive(Debug, Clone, Copy)]
enum Screen {
    EnvList,
    EnvDetail { env_idx: usize },
}

/// One row in the env-detail screen — either a section header or a
/// selectable item. Headers are non-selectable (skipped on cursor
/// movement).
#[derive(Debug, Clone)]
enum DetailRow {
    Header(String),
    Item { kind: Kind, name: String },
    Done, // sentinel "submit / back" row at the end of the list
}

struct App<'a> {
    sources: &'a [Source],
    screen_stack: Vec<Screen>,
    env_list_state: ListState,
    detail_list_state: ListState,
    /// Cached row layout for the current detail screen.
    detail_rows: Vec<DetailRow>,
    /// Per (env_name, kind) → set of checked item names. Persists
    /// across drill-in/drill-out so the user never loses a selection.
    /// Pre-populated with target_state so items already in the
    /// target render as checked from first paint.
    checked: BTreeMap<(String, Kind), HashSet<String>>,
    /// What's currently in the target's manifest. Used for the
    /// `(in target)` marker and for remove inference at submit time.
    target: TargetState,
}

impl<'a> App<'a> {
    fn new(sources: &'a [Source], target: TargetState) -> Self {
        let mut env_state = ListState::default();
        if !sources.is_empty() {
            env_state.select(Some(0));
        }
        // Pre-check items that are already in the target manifest, in
        // every source where they appear. The user sees "[x] foo (in
        // target)" from first paint and can either leave it (no-op)
        // or uncheck (= remove from target on submit).
        let mut checked: BTreeMap<(String, Kind), HashSet<String>> = BTreeMap::new();
        for src in sources {
            for (kind, names) in [
                (Kind::Plugin, src.plugin_names()),
                (Kind::Skill, src.skill_names()),
                (Kind::Mcp, src.mcp_names()),
            ] {
                for name in names {
                    if target.contains(kind, &name) {
                        checked
                            .entry((src.name.clone(), kind))
                            .or_default()
                            .insert(name);
                    }
                }
            }
        }
        Self {
            sources,
            screen_stack: vec![Screen::EnvList],
            env_list_state: env_state,
            detail_list_state: ListState::default(),
            detail_rows: Vec::new(),
            checked,
            target,
        }
    }

    fn current_screen(&self) -> Screen {
        *self.screen_stack.last().unwrap_or(&Screen::EnvList)
    }

    fn selected_count_for(&self, env_name: &str) -> usize {
        let mut total = 0;
        for kind in [Kind::Plugin, Kind::Skill, Kind::Mcp] {
            if let Some(set) = self.checked.get(&(env_name.to_string(), kind)) {
                total += set.len();
            }
        }
        total
    }

    fn total_selected(&self) -> usize {
        self.checked.values().map(|s| s.len()).sum()
    }

    fn rebuild_detail_rows(&mut self, env_idx: usize) {
        let src = &self.sources[env_idx];
        let mut rows: Vec<DetailRow> = Vec::new();
        let plugins = src.plugin_names();
        if !plugins.is_empty() {
            rows.push(DetailRow::Header("Plugins".into()));
            for name in plugins {
                rows.push(DetailRow::Item {
                    kind: Kind::Plugin,
                    name,
                });
            }
        }
        let skills = src.skill_names();
        if !skills.is_empty() {
            rows.push(DetailRow::Header("Skills".into()));
            for name in skills {
                rows.push(DetailRow::Item {
                    kind: Kind::Skill,
                    name,
                });
            }
        }
        let mcps = src.mcp_names();
        if !mcps.is_empty() {
            rows.push(DetailRow::Header("MCPs".into()));
            for name in mcps {
                rows.push(DetailRow::Item {
                    kind: Kind::Mcp,
                    name,
                });
            }
        }
        // "← Back to env list" sentinel — explicit affordance for
        // users who don't know `←` yet. Mirrors the `[submit]` row
        // in the env-list screen.
        rows.push(DetailRow::Done);
        self.detail_rows = rows;
        // Place cursor on the first selectable row.
        let first_item = self
            .detail_rows
            .iter()
            .position(|r| matches!(r, DetailRow::Item { .. }))
            .or_else(|| Some(self.detail_rows.len().saturating_sub(1)));
        self.detail_list_state.select(first_item);
    }

    fn into_plan(self) -> Plan {
        // Iterate sources in order so "first checked wins" matches the
        // visual order — env list is alphabetical-ish (Env::list sorts
        // by name) which is intuitive.
        let mut plan = Plan::default();

        // Adds: every checked item that's NOT already in the target.
        // Items that are checked AND already in the target round-trip
        // as no-ops (apply skips them). This avoids surfacing
        // "imported 0 items" when the user explicitly chose to keep
        // an existing pin checked.
        for src in self.sources {
            for (kind, set) in [
                (
                    Kind::Plugin,
                    self.checked.get(&(src.name.clone(), Kind::Plugin)),
                ),
                (
                    Kind::Skill,
                    self.checked.get(&(src.name.clone(), Kind::Skill)),
                ),
                (Kind::Mcp, self.checked.get(&(src.name.clone(), Kind::Mcp))),
            ] {
                let Some(set) = set else { continue };
                for name in set {
                    if self.target.contains(kind, name) {
                        continue;
                    }
                    match kind {
                        Kind::Plugin => plan.add_plugin(name.clone(), &src.name),
                        Kind::Skill => plan.add_skill(name.clone(), &src.name),
                        Kind::Mcp => plan.add_mcp(name.clone(), &src.name),
                    }
                }
            }
        }

        // Removes: only items that were *actually rendered* in the
        // TUI (= appeared in at least one source's detail screen)
        // AND are no longer checked anywhere. Target-only items —
        // pinned in the target's manifest but absent from every
        // source — must NOT be removed silently: the user never saw
        // them, never had a chance to keep them, so inferring intent
        // from "not in still_checked" would silently drop unrelated
        // pins on submit. To remove a target-only item, the user
        // runs `aenv rm <kind> <name>` (the explicit destructive
        // command path), or imports the same item into a side env
        // first so it shows up in ifl.
        let rendered: HashSet<(Kind, String)> = self
            .sources
            .iter()
            .flat_map(|src| {
                let plugins = src.plugin_names().into_iter().map(|n| (Kind::Plugin, n));
                let skills = src.skill_names().into_iter().map(|n| (Kind::Skill, n));
                let mcps = src.mcp_names().into_iter().map(|n| (Kind::Mcp, n));
                plugins.chain(skills).chain(mcps)
            })
            .collect();
        let still_checked: HashSet<(Kind, String)> = self
            .checked
            .iter()
            .flat_map(|((_, kind), names)| names.iter().map(move |n| (*kind, n.clone())))
            .collect();
        for name in &self.target.plugins {
            let key = (Kind::Plugin, name.clone());
            if rendered.contains(&key) && !still_checked.contains(&key) {
                plan.remove_plugin(name.clone());
            }
        }
        for name in &self.target.skills {
            let key = (Kind::Skill, name.clone());
            if rendered.contains(&key) && !still_checked.contains(&key) {
                plan.remove_skill(name.clone());
            }
        }
        for name in &self.target.mcps {
            let key = (Kind::Mcp, name.clone());
            if rendered.contains(&key) && !still_checked.contains(&key) {
                plan.remove_mcp(name.clone());
            }
        }
        plan
    }
}

// =====================================================================
//   Event loop
// =====================================================================

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<Outcome> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Ctrl-C → cancel everywhere
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(Outcome::Cancel);
        }
        match app.current_screen() {
            Screen::EnvList => {
                if let Some(out) = handle_env_list_key(app, key.code) {
                    return Ok(out);
                }
            }
            Screen::EnvDetail { env_idx } => {
                if let Some(out) = handle_detail_key(app, env_idx, key.code) {
                    return Ok(out);
                }
            }
        }
    }
}

/// Returns Some(outcome) if the key terminates the loop.
fn handle_env_list_key(app: &mut App, code: KeyCode) -> Option<Outcome> {
    // Env list has N envs followed by a [submit] sentinel — total = N + 1.
    let env_count = app.sources.len();
    let total = env_count + 1;
    let on_submit = app
        .env_list_state
        .selected()
        .is_some_and(|i| i == env_count);
    match code {
        KeyCode::Char('q') | KeyCode::Esc => Some(Outcome::Cancel),
        KeyCode::Char('s') | KeyCode::Char('S') => Some(Outcome::Submit),
        KeyCode::Down | KeyCode::Char('j') => {
            move_cursor(&mut app.env_list_state, total, 1);
            None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_cursor(&mut app.env_list_state, total, -1);
            None
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            if on_submit {
                Some(Outcome::Submit)
            } else if let Some(env_idx) = app.env_list_state.selected() {
                if env_idx < env_count {
                    app.screen_stack.push(Screen::EnvDetail { env_idx });
                    app.rebuild_detail_rows(env_idx);
                }
                None
            } else {
                None
            }
        }
        _ => None,
    }
}

fn handle_detail_key(app: &mut App, env_idx: usize, code: KeyCode) -> Option<Outcome> {
    match code {
        KeyCode::Char('q') => Some(Outcome::Cancel),
        KeyCode::Char('s') | KeyCode::Char('S') => Some(Outcome::Submit),
        KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
            // Drill back, preserving selections.
            app.screen_stack.pop();
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_to_next_selectable(app, 1);
            None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_to_next_selectable(app, -1);
            None
        }
        KeyCode::Char(' ') => {
            toggle_current(app, env_idx);
            None
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            toggle_all_in_section(app, env_idx);
            None
        }
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            // On the [Done] row, return to env list. On an item row,
            // toggle (matches lazygit/gum's "either action moves
            // selection forward").
            let i = app.detail_list_state.selected().unwrap_or(0);
            match app.detail_rows.get(i) {
                Some(DetailRow::Done) => {
                    app.screen_stack.pop();
                }
                Some(DetailRow::Item { .. }) => toggle_current(app, env_idx),
                _ => {}
            }
            None
        }
        _ => None,
    }
}

fn move_cursor(state: &mut ListState, total: usize, delta: i32) {
    if total == 0 {
        return;
    }
    let cur = state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(total as i32) as usize;
    state.select(Some(next));
}

fn move_to_next_selectable(app: &mut App, delta: i32) {
    if app.detail_rows.is_empty() {
        return;
    }
    let total = app.detail_rows.len() as i32;
    let mut cur = app.detail_list_state.selected().unwrap_or(0) as i32;
    for _ in 0..total {
        cur = (cur + delta).rem_euclid(total);
        let row = &app.detail_rows[cur as usize];
        if matches!(row, DetailRow::Item { .. } | DetailRow::Done) {
            app.detail_list_state.select(Some(cur as usize));
            return;
        }
    }
}

fn toggle_current(app: &mut App, env_idx: usize) {
    let i = match app.detail_list_state.selected() {
        Some(i) => i,
        None => return,
    };
    let (kind, name) = match app.detail_rows.get(i) {
        Some(DetailRow::Item { kind, name }) => (*kind, name.clone()),
        _ => return,
    };
    toggle_item(app, env_idx, kind, &name);
}

/// Toggle one (kind, name). For items the target manifest already
/// pins, the toggle is a *global* membership flip — every source
/// detail screen that surfaces the same name reflects the new
/// state, so unchecking once removes from the target regardless of
/// which source's row the user clicked. For items not in the
/// target, toggle is source-specific (= "import from THIS source"),
/// which is what the first-wins add semantics expects.
///
/// Without the global path, a target plugin shared between two
/// source envs (a common case) wouldn't be removable: unchecking
/// in source A leaves it pre-checked in source B, `still_checked`
/// stays non-empty for that name, and `into_plan` skips the remove.
/// The footer's "uncheck (in target) row → remove" promise needs
/// this synchronisation to hold for shared shapes.
fn toggle_item(app: &mut App, env_idx: usize, kind: Kind, name: &str) {
    if app.target.contains(kind, name) {
        // Decide direction by looking at any source's current state
        // (they're kept in sync, so the env_idx row's state is the
        // canonical answer).
        let env_name = app.sources[env_idx].name.clone();
        let currently_checked = app
            .checked
            .get(&(env_name, kind))
            .map(|s| s.contains(name))
            .unwrap_or(false);
        let new_state = !currently_checked;
        for src in app.sources {
            let surfaces = match kind {
                Kind::Plugin => src.plugin_names().iter().any(|n| n == name),
                Kind::Skill => src.skill_names().iter().any(|n| n == name),
                Kind::Mcp => src.mcp_names().iter().any(|n| n == name),
            };
            if !surfaces {
                continue;
            }
            let entry = app.checked.entry((src.name.clone(), kind)).or_default();
            if new_state {
                entry.insert(name.to_string());
            } else {
                entry.remove(name);
            }
        }
    } else {
        // Source-specific toggle for items not in the target.
        let env_name = app.sources[env_idx].name.clone();
        let key = (env_name, kind);
        let entry = app.checked.entry(key).or_default();
        if !entry.insert(name.to_string()) {
            entry.remove(name);
        }
    }
}

fn toggle_all_in_section(app: &mut App, env_idx: usize) {
    let cur = match app.detail_list_state.selected() {
        Some(i) => i,
        None => return,
    };
    // Find the section header above the cursor; collect every item
    // between this header and the next header (or end).
    let mut section_kind: Option<Kind> = None;
    let mut start = 0usize;
    for i in (0..=cur).rev() {
        if let DetailRow::Header(_) = &app.detail_rows[i] {
            // Look at first item after this header to determine kind.
            if let Some(DetailRow::Item { kind, .. }) = app.detail_rows.get(i + 1) {
                section_kind = Some(*kind);
                start = i + 1;
            }
            break;
        }
    }
    let Some(kind) = section_kind else { return };
    let mut end = app.detail_rows.len();
    for i in start..app.detail_rows.len() {
        if matches!(app.detail_rows[i], DetailRow::Header(_) | DetailRow::Done) {
            end = i;
            break;
        }
    }
    // Decide direction off this source's current state — same lookup
    // single-row toggle uses, so the "all checked → uncheck all"
    // heuristic stays accurate even when target items are in the
    // mix (their state is identical across sources by construction).
    let env_name = app.sources[env_idx].name.clone();
    let all_checked = (start..end).all(|i| match &app.detail_rows[i] {
        DetailRow::Item { name, .. } => app
            .checked
            .get(&(env_name.clone(), kind))
            .map(|s| s.contains(name))
            .unwrap_or(false),
        _ => true,
    });
    // Collect names first (to avoid mutable-borrow conflicts with
    // `toggle_item`), then toggle each via the global path so target
    // items propagate to every source row.
    let names: Vec<String> = (start..end)
        .filter_map(|i| match &app.detail_rows[i] {
            DetailRow::Item { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    for name in names {
        let is_checked = app
            .checked
            .get(&(env_name.clone(), kind))
            .map(|s| s.contains(&name))
            .unwrap_or(false);
        // Only toggle when it would change the row in the desired
        // direction. Calling toggle_item unconditionally would flip
        // already-correct rows the wrong way for mixed-state sections.
        let need_toggle = if all_checked { is_checked } else { !is_checked };
        if need_toggle {
            toggle_item(app, env_idx, kind, &name);
        }
    }
}

// =====================================================================
//   Render
// =====================================================================

fn draw(frame: &mut ratatui::Frame, app: &App) {
    // Body + sticky footer (hints). Footer height is fixed at 2 lines:
    // one for the keybinding hint, one for the running total.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(frame.area());
    let body = chunks[0];
    let footer = chunks[1];

    match app.current_screen() {
        Screen::EnvList => draw_env_list(frame, body, app),
        Screen::EnvDetail { env_idx } => draw_detail(frame, body, app, env_idx),
    }
    draw_footer(frame, footer, app);
}

fn draw_env_list(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let mut items: Vec<ListItem> = app
        .sources
        .iter()
        .map(|s| {
            let count = app.selected_count_for(&s.name);
            let badge = if count > 0 {
                format!("  · {count} selected")
            } else {
                String::new()
            };
            let line = if count > 0 {
                Line::from(vec![
                    Span::raw(s.name.clone()),
                    Span::styled(badge, Style::new().add_modifier(Modifier::DIM)),
                ])
            } else {
                Line::from(s.name.clone())
            };
            ListItem::new(line)
        })
        .collect();
    // Sticky `[submit]` row at the end. Always reachable by ↓ past
    // the last env. Visually distinct so users know it's special.
    let total = app.total_selected();
    let submit_label = if total > 0 {
        format!(
            "[submit] ({total} item{} total)",
            if total == 1 { "" } else { "s" }
        )
    } else {
        "[submit] (no items selected)".into()
    };
    items.push(ListItem::new(Line::from(Span::styled(
        submit_label,
        Style::new().add_modifier(Modifier::BOLD),
    ))));

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Import from list — pick envs to drill into "),
        )
        .highlight_style(highlight_style())
        .highlight_symbol("▸ ");
    let mut state = app.env_list_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App, env_idx: usize) {
    let env_name = &app.sources[env_idx].name;
    let items: Vec<ListItem> = app
        .detail_rows
        .iter()
        .map(|row| match row {
            DetailRow::Header(title) => ListItem::new(Line::from(Span::styled(
                title.clone(),
                Style::new().add_modifier(Modifier::BOLD).underlined(),
            ))),
            DetailRow::Item { kind, name } => {
                let checked = app
                    .checked
                    .get(&(env_name.clone(), *kind))
                    .map(|s| s.contains(name))
                    .unwrap_or(false);
                let mark = if checked { "[x]" } else { "[ ]" };
                let in_target = app.target.contains(*kind, name);
                if in_target {
                    // Dim trailing tag so the user can tell at a
                    // glance which rows are "already pinned" — and
                    // therefore that unchecking them removes them.
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("  {mark} {name}")),
                        Span::styled("  (in target)", Style::new().add_modifier(Modifier::DIM)),
                    ]))
                } else {
                    ListItem::new(Line::from(format!("  {mark} {name}")))
                }
            }
            DetailRow::Done => ListItem::new(Line::from(Span::styled(
                "  ← back to env list",
                Style::new().add_modifier(Modifier::DIM),
            ))),
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {env_name} — pick items ")),
        )
        .highlight_style(highlight_style())
        .highlight_symbol("▸ ");
    let mut state = app.detail_list_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_footer(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &App) {
    let hint = match app.current_screen() {
        Screen::EnvList => "↑↓ move  ⏎ open  s submit  q quit",
        Screen::EnvDetail { .. } => {
            "↑↓ move  Space toggle (uncheck (in target) row → remove)  a toggle-all  ← back  s submit  q quit"
        }
    };
    let total = app.total_selected();
    let totals = format!("Total checked: {total}");
    let para = Paragraph::new(vec![
        Line::from(Span::styled(hint, Style::new().add_modifier(Modifier::DIM))),
        Line::from(Span::styled(
            totals,
            Style::new().add_modifier(Modifier::BOLD),
        )),
    ]);
    frame.render_widget(para, area);
}

/// Highlight style honors NO_COLOR per <https://no-color.org/> —
/// without color the user still gets reverse-video so the cursor
/// position is visible.
fn highlight_style() -> Style {
    if std::env::var_os("NO_COLOR").is_some() {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::REVERSED).bold()
    }
}

// =====================================================================
//   RAII terminal guard
// =====================================================================

/// Enters raw mode + alternate screen on construction; restores on
/// drop (handles panics + Ctrl-C path). Without this the user's
/// terminal is left in raw mode if anything panics during the loop.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        execute!(io::stdout(), EnterAlternateScreen).context("enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod plan_inference_tests {
    //! `into_plan` is the entire state→intent translator the TUI
    //! relies on. These tests exercise it directly (no event loop)
    //! to lock the contract that submit-time inference matches what
    //! the user actually saw on screen.

    use super::*;
    use crate::env::manifest::{
        EnvMeta, Hooks, Manifest, McpSpec, PlatformsBlock, PluginRef, PluginSpec, PluginsBlock,
        SkillsBlock, SCHEMA_VERSION,
    };
    use std::collections::BTreeMap;

    fn manifest_with(name: &str, plugins: &[&str], mcps: &[&str]) -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION.to_string(),
            env: EnvMeta {
                name: name.to_string(),
                description: None,
                compat: BTreeMap::new(),
                created: None,
            },
            platforms: PlatformsBlock::default(),
            mcp: mcps
                .iter()
                .map(|n| {
                    (
                        (*n).to_string(),
                        McpSpec {
                            command: Some("noop".into()),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            plugins: PluginsBlock {
                enabled: plugins
                    .iter()
                    .map(|n| {
                        PluginRef::Detailed(PluginSpec {
                            name: (*n).to_string(),
                            version: None,
                            source: Some(format!("npm:{n}")),
                            subpath: None,
                            sha256: None,
                            release_url: None,
                            target_map: BTreeMap::new(),
                        })
                    })
                    .collect(),
            },
            skills: SkillsBlock::default(),
            hooks: Hooks::default(),
        }
    }

    fn src(name: &str, plugins: &[&str], mcps: &[&str]) -> Source {
        Source {
            name: name.to_string(),
            manifest: manifest_with(name, plugins, mcps),
        }
    }

    fn target_state(plugins: &[&str], mcps: &[&str]) -> TargetState {
        TargetState {
            plugins: plugins.iter().map(|s| (*s).to_string()).collect(),
            skills: HashSet::new(),
            mcps: mcps.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn target_only_items_are_never_removed_even_after_submit() {
        // P1 contract: target has `lonely` (no source carries it).
        // Source A has `shared` (also in target) and `imported`.
        // User opens ifl, leaves everything as pre-checked, submits.
        // Expected: zero adds (shared already there), zero removes
        // (lonely was never rendered, so submit must not infer
        // intent for it). `imported` was unchecked from the start
        // (not in target), and the user didn't check it — no add.
        let sources = vec![src("a", &["shared", "imported"], &[])];
        let target = target_state(&["lonely", "shared"], &[]);
        let app = App::new(&sources, target);
        let plan = app.into_plan();
        assert!(plan.plugins.is_empty(), "no adds expected: {plan:?}");
        assert!(
            plan.remove_plugins.is_empty(),
            "target-only `lonely` must not be removed: {:?}",
            plan.remove_plugins
        );
    }

    #[test]
    fn unchecking_a_rendered_target_item_emits_a_remove() {
        // shared is in target AND in source a → rendered + pre-checked.
        // The user unchecks it. submit must infer remove.
        let sources = vec![src("a", &["shared"], &[])];
        let target = target_state(&["shared"], &[]);
        let mut app = App::new(&sources, target);
        // Simulate unchecking by mutating the checked map directly —
        // event_loop reaches the same state via Space toggle.
        app.checked.clear();
        let plan = app.into_plan();
        assert_eq!(
            plan.remove_plugins.iter().collect::<Vec<_>>(),
            vec![&"shared".to_string()],
            "unchecked rendered item must be removed: {plan:?}"
        );
    }

    #[test]
    fn checking_a_new_item_emits_an_add() {
        // foo is in source a, NOT in target. User checks it. submit
        // must infer add against source a.
        let sources = vec![src("a", &["foo"], &[])];
        let target = target_state(&[], &[]);
        let mut app = App::new(&sources, target);
        app.checked
            .entry(("a".to_string(), Kind::Plugin))
            .or_default()
            .insert("foo".into());
        let plan = app.into_plan();
        assert_eq!(plan.plugins.get("foo").map(|s| s.as_str()), Some("a"));
        assert!(plan.remove_plugins.is_empty());
    }

    #[test]
    fn left_checked_target_item_round_trips_as_noop() {
        // target has `kept`, source a has `kept` + `extra`. User
        // does nothing (defaults: kept pre-checked, extra unchecked)
        // and submits. Plan must be entirely empty.
        let sources = vec![src("a", &["kept", "extra"], &[])];
        let target = target_state(&["kept"], &[]);
        let app = App::new(&sources, target);
        let plan = app.into_plan();
        assert!(
            plan.is_empty(),
            "no-op submit must yield empty plan: {plan:?}"
        );
    }

    #[test]
    fn unchecking_target_item_in_one_source_propagates_across_all_sources() {
        // P2 contract: target plugin `shared` lives in two sources.
        // Pre-pivot a single uncheck wouldn't infer remove because
        // the duplicate row in the other source kept it in
        // still_checked. New: target items toggle globally, so one
        // uncheck removes from every source row + emits remove.
        let sources = vec![src("a", &["shared"], &[]), src("b", &["shared"], &[])];
        let target = target_state(&["shared"], &[]);
        let mut app = App::new(&sources, target);
        // Sanity: pre-checked in BOTH sources from `App::new`.
        for env in ["a", "b"] {
            assert!(
                app.checked
                    .get(&(env.to_string(), Kind::Plugin))
                    .is_some_and(|s| s.contains("shared")),
                "expected pre-check in env {env}"
            );
        }
        // Toggle once via the global path (env_idx = 0 = source a).
        toggle_item(&mut app, 0, Kind::Plugin, "shared");
        // Both sources should now show `shared` unchecked.
        for env in ["a", "b"] {
            let entry = app.checked.get(&(env.to_string(), Kind::Plugin));
            let still_in = entry.is_some_and(|s| s.contains("shared"));
            assert!(!still_in, "uncheck must propagate to env {env}: {entry:?}");
        }
        // And submit must infer the remove.
        let plan = app.into_plan();
        assert_eq!(
            plan.remove_plugins.iter().collect::<Vec<_>>(),
            vec![&"shared".to_string()],
            "globalised toggle must emit remove: {plan:?}"
        );
    }

    #[test]
    fn checking_non_target_item_stays_source_specific() {
        // Symmetry check: non-target items (= candidate adds) must
        // NOT propagate. `bar` is in source a and source b, neither
        // in target. User checks in source a → only source a's
        // entry is set, plan picks source a as the canonical
        // (first-wins) origin. Source b stays untouched.
        let sources = vec![src("a", &["bar"], &[]), src("b", &["bar"], &[])];
        let target = target_state(&[], &[]);
        let mut app = App::new(&sources, target);
        toggle_item(&mut app, 0, Kind::Plugin, "bar");
        assert!(
            app.checked
                .get(&("a".to_string(), Kind::Plugin))
                .is_some_and(|s| s.contains("bar")),
            "source a row must be checked"
        );
        assert!(
            !app.checked
                .get(&("b".to_string(), Kind::Plugin))
                .map(|s| s.contains("bar"))
                .unwrap_or(false),
            "source b row must remain unchecked (source-specific add)"
        );
        let plan = app.into_plan();
        assert_eq!(plan.plugins.get("bar").map(|s| s.as_str()), Some("a"));
    }

    #[test]
    fn mcp_target_only_protected_same_way_as_plugins() {
        // Symmetry check: the rendered-set guard must apply to MCPs
        // and skills, not just plugins. Otherwise an MCP-only
        // target-only entry is exposed to the same silent-remove bug.
        let sources = vec![src("a", &["p1"], &["m_in_a"])];
        let target = target_state(&[], &["m_lonely", "m_in_a"]);
        let app = App::new(&sources, target);
        let plan = app.into_plan();
        assert!(
            plan.remove_mcps.is_empty(),
            "target-only MCP `m_lonely` must survive: {:?}",
            plan.remove_mcps
        );
    }
}
