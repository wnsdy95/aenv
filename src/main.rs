use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

mod audit;
mod backend;
mod cli;
mod commands;
mod env;
mod error;
mod install;
mod mcp_import;
mod paths;
mod resolve;
mod secrets;
mod shell;
mod skills;
mod store;
mod tx;

use cli::{Cli, Command};

fn main() -> ExitCode {
    init_tracing();

    let argv0 = std::env::args().next().unwrap_or_default();
    let stem = PathBuf::from(&argv0)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    print_env_banner();

    // Multi-call binary: when invoked as a registered backend's tool name
    // (claude, codex, …), dispatch into shim mode for that backend.
    // file_stem() strips any `.exe` suffix on Windows so this matches on
    // both `claude` (Unix) and `claude.exe` (Windows).
    let result = if let Some(backend) = backend::for_argv0(&stem) {
        backend::dispatch_shim(backend, std::env::args().skip(1).collect())
    } else {
        let cli = Cli::parse();
        run(cli)
    };

    match result {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("aenv: {err}");
            for cause in err.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("AENV_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .try_init();
}

/// Print `[<env>]` to stderr at the top of every aenv/claude invocation so
/// the user always knows which environment they're operating on. Skipped
/// for `--help` / `--version` (clean output for tooling) and when the user
/// has set `AENV_QUIET=1` (scripted usage).
fn print_env_banner() {
    if std::env::var_os("AENV_QUIET").is_some() {
        return;
    }
    if std::env::args().skip(1).any(|a| {
        matches!(
            a.as_str(),
            "--help" | "-h" | "--version" | "-V" | "help" | "shell-init" | "current"
        )
    }) {
        return;
    }
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return,
    };
    // resolve() always returns Some now (falls back to `global`),
    // but we keep the Err arm just in case a future change relaxes
    // that.
    let label = match resolve::resolve(&cwd) {
        Ok(Some(r)) => r.name,
        Ok(None) | Err(_) => "global".to_string(),
    };
    // Suppress the banner under `global` — the user's real ~/.claude
    // is exactly what they'd see without aenv installed, so adding a
    // `[global]` line on every invocation would just be noise. Mirrors
    // the shell-init `_aenv_prompt` rule that clears AENV_PROMPT when
    // the active env is global.
    if label == "global" {
        return;
    }
    eprintln!("[{label}]");
}

fn run(cli: Cli) -> anyhow::Result<u8> {
    match cli.command {
        Command::Init(args) => commands::init::run(args),
        Command::New(args) => commands::new_env::run(args),
        Command::List(args) => commands::list::run(args),
        Command::Remove(args) => commands::remove::run(args),
        Command::Use(args) => commands::use_cmd::run(args),
        Command::Current(args) => commands::current::run(args),
        Command::Status(args) => commands::status::run(args),
        Command::Exec(args) => commands::exec::run(args),
        Command::Shell(args) => commands::shell_cmd::run(args),
        Command::Which(args) => commands::which::run(args),
        Command::Doctor(args) => commands::doctor::run(args),
        Command::ImportGlobal(args) => commands::import_global::run(args),
        Command::Install(args) => commands::install::run(args),
        Command::Lock(args) => commands::lock_cmd::run(args),
        Command::Sync(args) => commands::sync_cmd::run(args),
        Command::Add(args) => commands::add::run(args),
        Command::Rm(args) => commands::rm::run(args),
        Command::Secrets(args) => commands::secrets_cmd::run(args),
        Command::ExportProfile(args) => commands::export_profile::run(args),
        Command::ImportProfile(args) => commands::import_profile::run(args),
        Command::Rollback(args) => commands::rollback::run(args),
        Command::History(args) => commands::history::run(args),
        Command::Prune(args) => commands::prune::run(args),
        Command::Audit(args) => commands::audit_cmd::run(args),
        Command::Ifl(args) => commands::ifl::run(args),
        Command::Quit => {
            eprintln!(
                "aenv quit: this command must be run as a shell function. \
                 Add `eval \"$(aenv shell-init zsh)\"` (or your shell) to \
                 your rc file and source it, then `aenv quit` will \
                 `unset $AENV` and `export AENV_OVERRIDE=global` in your \
                 current shell — matching Python venv's `deactivate` shape."
            );
            Ok(2)
        }
        Command::Run(args) => commands::run_cmd::run(args),
        Command::Upgrade(args) => commands::upgrade::run(args),
    }
}
