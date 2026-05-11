use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "aenv",
    version,
    about = "Per-environment isolation for AI coding CLIs (Claude Code, Codex, ...)",
    long_about = "aenv creates isolated environments for AI coding CLIs and \
                  dispatches them via per-tool shims. Each backend uses its \
                  tool's native config-dir env var (CLAUDE_CONFIG_DIR, \
                  CODEX_HOME, ...) so isolation is real, not a wrapper trick. \
                  Universal core (manifest, lockfile, store, secrets) plus a \
                  thin per-tool backend module that contains the tool-specific \
                  bits."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
// AddArgs is intentionally wide to express the union of plugin/skill/mcp
// flags + claude-mcp-add-compatible (transport/json/from/argv/header) +
// legacy (--command/--arg/--env-var) flags in a single Args struct.
// Boxing the variant adds an indirection that obscures the dispatch
// without buying performance — Add is used once per CLI invocation.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Install shim, set up shell init hook, and create default env if missing.
    Init(InitArgs),
    /// Create a new env.
    New(NewArgs),
    /// List envs.
    List(ListArgs),
    /// Remove an env.
    Remove(RemoveArgs),
    /// Pin the env for the current directory (writes .aenv-version) or set the global default.
    Use(UseArgs),
    /// Print the active env name (resolved from cwd).
    Current(CurrentArgs),
    /// Detailed status of the active env (plugins, MCPs, paths).
    Status(StatusArgs),
    /// Run a one-shot command inside an env's context.
    Exec(ExecArgs),
    /// Print shell init script (eval "$(aenv shell-init <shell>)").
    #[command(name = "shell-init")]
    Shell(ShellArgs),
    /// Print resolved path of an env or the real claude binary.
    Which(WhichArgs),
    /// Diagnose isolation leaks and missing pieces.
    Doctor(DoctorArgs),
    /// Import the user's existing ~/.claude into an env (so plugins/skills/MCPs persist).
    #[command(name = "import-global")]
    ImportGlobal(ImportGlobalArgs),
    /// Resolve manifest into the env: fetch missing plugins/skills, materialize, write lockfile.
    Install(InstallArgs),
    /// Re-generate aenv.lock from the manifest without applying.
    Lock(LockArgs),
    /// Force the env to match aenv.lock (re-materialize everything).
    Sync(SyncArgs),
    /// Add an MCP/plugin/skill entry to the manifest.
    Add(AddArgs),
    /// Remove an MCP/plugin/skill entry from the manifest.
    #[command(name = "rm")]
    Rm(RmArgs),
    /// Manage env-scoped secrets (stored in OS keyring).
    Secrets(SecretsArgs),
    /// Export an env (manifest + lockfile, no secrets/sessions/cache) as a tar.gz bundle.
    #[command(name = "export-profile")]
    ExportProfile(ExportProfileArgs),
    /// Import a previously exported env bundle.
    #[command(name = "import-profile")]
    ImportProfile(ImportProfileArgs),
    /// Restore the most recent committed transaction.
    Rollback(RollbackArgs),
    /// Show recent transactions (snapshots in ~/.aenv/state/).
    History(HistoryArgs),
    /// Delete old transaction snapshots.
    Prune(PruneArgs),
    /// Show recent audit log entries.
    Audit(AuditArgs),
    /// Interactive multi-source import: pick plugins/skills/MCPs from
    /// other envs into the current env (TUI). Pass `--env <name>` +
    /// `--plugin/--skill/--mcp` for the non-interactive form (CI / scripts).
    Ifl(IflArgs),
    /// Drop any shell-set env override (`$AENV` / `$AENV_OVERRIDE`),
    /// letting cwd-resolved env or the global default take over.
    /// Implemented as a shell function via `aenv shell-init`; this
    /// subcommand only fires when shell-init isn't loaded, in which
    /// case it prints instructions instead of silently no-op'ing.
    Quit,
    /// Internal: launch claude under the active env via the same path
    /// the shim takes. Hidden — exists for tests and debugging.
    #[command(hide = true)]
    Run(RunArgs),
    /// Upgrade aenv: rebuild the binary from source via `cargo install`,
    /// then refresh the shim. Single-shot path — users opt in
    /// explicitly so an upgrade never surprises them.
    Upgrade(UpgradeArgs),
}

#[derive(Debug, Args)]
pub struct UpgradeArgs {
    /// Print what would happen without actually invoking cargo. Used
    /// by integration tests so the upgrade flow can be exercised
    /// without network / cargo install side effects.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct IflArgs {
    /// Target env to import INTO (defaults to active env).
    #[arg(long, short = 'E')]
    pub env: Option<String>,
    /// Non-interactive: source env to import FROM (repeatable).
    /// Pairs with `--plugin/--skill/--mcp` to scope individual items;
    /// empty selectors → import every item from the listed sources.
    #[arg(long = "from", value_name = "ENV")]
    pub from_env: Vec<String>,
    /// Non-interactive: import this plugin name from the most-recently
    /// listed `--from` env (repeatable across `--from` blocks).
    #[arg(long = "plugin", value_name = "NAME")]
    pub plugins: Vec<String>,
    /// Non-interactive: import this skill name (same scoping as --plugin).
    #[arg(long = "skill", value_name = "NAME")]
    pub skills: Vec<String>,
    /// Non-interactive: import this MCP name (same scoping as --plugin).
    #[arg(long = "mcp", value_name = "NAME")]
    pub mcps: Vec<String>,
    /// Override TTY detection — force the TUI even when stdin/stdout
    /// aren't TTYs or `CI=true`. Mirrors gh's `GH_FORCE_TTY`.
    #[arg(long)]
    pub force_tty: bool,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Don't create a default env on init.
    #[arg(long)]
    pub no_default: bool,
    /// Force re-link the shim even if it exists.
    #[arg(long)]
    pub force: bool,
    /// Initialize a project-local env in the current directory by writing
    /// `aenv.toml` (and an empty `aenv.lock`). The optional positional
    /// arg sets `[env].name` (defaults to the basename of cwd). Skips
    /// the global setup steps — use this in a project root that already
    /// has aenv installed system-wide.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub here: Option<String>,
    /// Suppress the "Next steps: wire your shell" guidance block.
    /// Used by `aenv upgrade` so re-running init doesn't mislead an
    /// already-wired user into duplicating their .zshrc/.bashrc line.
    #[arg(long)]
    pub no_guidance: bool,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    pub name: String,
    /// Clone an existing env's config as a starting point.
    #[arg(long)]
    pub from: Option<String>,
    /// Don't seed an empty .claude/settings.json.
    #[arg(long)]
    pub bare: bool,
    /// After creating, immediately open `aenv ifl` to import items
    /// (plugins/skills/MCPs) from existing envs in the same step.
    #[arg(long)]
    pub ifl: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Show paths and metadata.
    #[arg(long, short)]
    pub long: bool,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    pub name: String,
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct UseArgs {
    pub name: String,
    /// Set as global default instead of writing .aenv-version in cwd.
    #[arg(long)]
    pub global: bool,
}

#[derive(Debug, Args)]
pub struct CurrentArgs {
    /// Print the resolution chain (which step matched).
    #[arg(long)]
    pub explain: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Output JSON.
    #[arg(long)]
    pub json: bool,
    #[arg(long, short = 'E')]
    pub env: Option<String>,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// The env to run in. Defaults to the resolved active env.
    #[arg(long, short = 'E')]
    pub env: Option<String>,
    /// Command + args. Use `--` to separate from aenv flags.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
    pub argv: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ShellArgs {
    /// Shell name: bash | zsh | fish.
    pub shell: String,
}

#[derive(Debug, Args)]
pub struct WhichArgs {
    /// What to resolve: 'env <name>' | 'claude' | 'shim' | 'home'.
    pub target: String,
    pub arg: Option<String>,
}

#[derive(Debug, Args)]
pub struct ImportGlobalArgs {
    /// Target env name (created if it doesn't exist; overwritten with --force).
    pub name: String,
    /// Overwrite existing env.
    #[arg(long)]
    pub force: bool,
    /// Set this env as the global default after import.
    #[arg(long)]
    pub set_default: bool,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Run inside this env (default: active).
    #[arg(long, short = 'E')]
    pub env: Option<String>,
    /// Don't update aenv.lock; just verify state.
    #[arg(long)]
    pub no_lock: bool,
}

#[derive(Debug, Args)]
pub struct LockArgs {
    #[arg(long, short = 'E')]
    pub env: Option<String>,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[arg(long, short = 'E')]
    pub env: Option<String>,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Kind: mcp | plugin | skill
    pub kind: String,
    /// Name (e.g. "github" for mcp, "engineering@1.4.0" for plugin). For
    /// `mcp --from <source>` (bulk import) the name is derived per-entry
    /// from the imported config and this positional may be empty.
    #[arg(default_value = "")]
    pub name: String,
    /// Source (URL/git/local path) for plugin/skill.
    #[arg(long)]
    pub source: Option<String>,
    /// Plugin root inside --source (for marketplace repos with many plugins).
    #[arg(long, value_name = "PATH")]
    pub subpath: Option<String>,
    /// (mcp legacy) Command to run. Prefer the `-- <cmd> [args...]` form.
    #[arg(long)]
    pub command: Option<String>,
    /// (mcp legacy) Command argument (repeatable).
    /// Use `--arg=-y` or `--arg=-flag` for hyphen-prefixed values.
    #[arg(long = "arg", allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// (mcp) KEY=VALUE env var (repeatable). Short alias `-e`.
    #[arg(long = "env-var", short = 'e')]
    pub env_var: Vec<String>,
    /// (mcp) Transport: stdio (default) | http | sse.
    /// Mirrors `claude mcp add --transport`.
    #[arg(long, value_name = "TRANSPORT")]
    pub transport: Option<String>,
    /// (mcp) Inline JSON config (claude_desktop_config.json shape).
    #[arg(long, value_name = "JSON")]
    pub json: Option<String>,
    /// (mcp) Bulk-import from an existing config:
    /// `claude-desktop` / `claude-code` / `cursor` / `cursor-deeplink:<url>` /
    /// `vscode-deeplink:<url>` / a path to a `mcpServers`-shaped JSON file.
    #[arg(long, value_name = "SOURCE")]
    pub from: Option<String>,
    /// (mcp http transport) Server URL when transport=http|sse.
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
    /// (mcp http transport) `Name: Value` HTTP header (repeatable).
    #[arg(long, value_name = "HEADER")]
    pub header: Vec<String>,
    /// Target env (defaults to active env or project mode's env).
    #[arg(long, short = 'E')]
    pub env: Option<String>,
    /// (mcp) Trailing argv after `--`: command + args for stdio transport.
    /// Example: `aenv add mcp github -- npx -y @scope/server-github`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub argv: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RmArgs {
    /// Kind: mcp | plugin | skill
    pub kind: String,
    pub name: String,
    #[arg(long, short = 'E')]
    pub env: Option<String>,
}

#[derive(Debug, Args)]
pub struct SecretsArgs {
    #[command(subcommand)]
    pub command: SecretsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Add or update a secret. Value read from stdin if --value not given.
    Add {
        key: String,
        #[arg(long)]
        value: Option<String>,
        #[arg(long, short = 'E')]
        env: Option<String>,
    },
    /// List secret keys (values are never displayed).
    List {
        #[arg(long, short = 'E')]
        env: Option<String>,
    },
    /// Remove a secret.
    #[command(name = "rm")]
    Remove {
        key: String,
        #[arg(long, short = 'E')]
        env: Option<String>,
    },
    /// Rotate a secret (replace value).
    Rotate {
        key: String,
        #[arg(long)]
        value: Option<String>,
        #[arg(long, short = 'E')]
        env: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct ExportProfileArgs {
    /// Env to export (default: active).
    #[arg(long, short = 'E')]
    pub env: Option<String>,
    /// Output file (defaults to <name>.aenv.tar.gz in cwd).
    #[arg(long, short)]
    pub output: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
pub struct ImportProfileArgs {
    /// Path to .aenv.tar.gz bundle.
    pub file: std::path::PathBuf,
    /// Override env name on import.
    #[arg(long)]
    pub name: Option<String>,
    /// Overwrite existing env.
    #[arg(long)]
    pub force: bool,
    /// Allow importing a manifest that declares `hooks.pre_activate`.
    /// Without this flag, an imported hook command would silently execute
    /// `sh -c <attacker-controlled>` on every claude launch in the env.
    /// Required even for trusted sources to make the trust decision explicit.
    #[arg(long)]
    pub trust_hooks: bool,
}

#[derive(Debug, Args)]
pub struct RollbackArgs {
    /// Roll back the most recent transaction even if it's still in Pending
    /// state. Use this to recover after a process was killed mid-install.
    #[arg(long)]
    pub pending: bool,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Keep at most this many snapshots.
    #[arg(long, default_value_t = 100)]
    pub keep_count: usize,
    /// Keep snapshots newer than N days.
    #[arg(long, default_value_t = 30)]
    pub keep_days: i64,
}

#[derive(Debug, Args)]
pub struct AuditArgs {
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Specific env to check (defaults to active).
    pub env: Option<String>,
    /// Output JSON for CI.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Override the env name to run with.
    #[arg(long)]
    pub env: Option<String>,
    /// Path to the real claude binary (skip auto-resolve).
    #[arg(long)]
    pub claude_bin: Option<PathBuf>,
    /// Args to forward to claude.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub argv: Vec<String>,
}
