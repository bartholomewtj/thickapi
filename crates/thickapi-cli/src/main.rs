use anyhow::Result;
use clap::{Parser, Subcommand};

use thickapi_cli::{commands, log_filter};

#[derive(Parser)]
#[command(
    name = "thickapi",
    about = "Local Rust daemon for subscription-backed coding agents",
    // `thickapi 0.15.0 (<git sha>)` (SPEC §11); the same pair `GET /health` reports.
    version = thickapi_api::VERSION_LINE
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Serve {
        /// Write the `tools` array of the first thin `/v1/chat/completions` request to PATH, then clear.
        #[arg(long, value_name = "PATH")]
        dump_tools: Option<std::path::PathBuf>,
    },
    Doctor {
        #[arg(long)]
        offline: bool,
        #[command(subcommand)]
        action: Option<DoctorCommands>,
    },
    Turn {
        #[arg(long, default_value = ".")]
        cwd: std::path::PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        combo: Option<String>,
        #[arg(long)]
        allow_paid: bool,
        #[arg(long)]
        stream: bool,
        prompt: Option<String>,
    },
    SyncPi {
        #[arg(long)]
        key: Option<String>,
    },
    #[command(
        about = "Catalogue rows, and whether each still names the vendor's newest version (SPEC 10.3)"
    )]
    Models {
        #[command(subcommand)]
        command: ModelsCommands,
    },
    Keys {
        #[command(subcommand)]
        command: KeysCommands,
    },
    Reap {
        #[arg(long, default_value_t = 7)]
        days: u64,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(
        about = "Proposal-only MCP server for the Claude bridge (SPEC §19.7); spawned by the daemon, not by hand",
        hide = true
    )]
    McpProposal {
        #[arg(long)]
        catalog: std::path::PathBuf,
        #[arg(long)]
        ready: std::path::PathBuf,
        #[arg(long)]
        violation: std::path::PathBuf,
    },
    #[command(
        about = "Record a vendor's live wire exchange as a token-redacted fixture set (SPEC §19.11)",
        long_about = "Run one text turn and one tool turn against the vendor through the same thin adapter `serve` uses, and write the redacted request, response head, body frames and resulting events to <out>/<vendor>/<cli-version>/{text,tool}/.\n\nVendors: claude (OAuth-direct), claude-bridge, grok, agy, cursor.\nThe adapter mapper tests replay the newest set; a wire change is fixed by: capture, watch tests fail, fix, tests pass.

--check captures to a temp directory instead, runs the leak check and the replay there, prints a diff against the newest committed set under <out>, and never writes under <out>."
    )]
    Capture {
        #[arg(required_unless_present = "all")]
        vendor: Option<String>,
        /// Catalogue row to drive (default: the first enabled thin row for the vendor).
        #[arg(long)]
        model: Option<String>,
        /// Fixture root (the repo's `fixtures/` when run from a checkout).
        #[arg(long, default_value = "fixtures")]
        out: std::path::PathBuf,
        /// Only one scenario: `text`, `tool`, bridge `image` / `image-tool` / `dialect` /
        /// `todo` (merged into an existing set), or (Cursor-only, opt-in, not in `--all`)
        /// `sdlc`, `create`, `create-fnf`, `quiet`, `mcp-repeat` (F1 #181; merged into an
        /// existing set).
        #[arg(long)]
        scenario: Option<String>,
        /// Run every vendor (claude, claude-bridge, grok, agy, cursor).
        #[arg(long)]
        all: bool,
        /// Leave the temp fixture tree on disk when a scenario fails.
        #[arg(long)]
        keep_failed: bool,
        /// Capture to a temp dir, leak-check and replay it, diff it against the newest
        /// committed set; never promote.
        #[arg(long)]
        check: bool,
    },
    #[command(
        about = "Register thickapi-serve Windows scheduled task at logon",
        long_about = "Register a Windows Task Scheduler job named thickapi-serve that runs `thickapi serve` when the current user logs on, plus a daily `reap` task. Pass --weekly-capture to also register a weekly `thickapi capture --all` (default off; spends vendor quota).\n\nRemove the task with:\n  schtasks /Delete /TN thickapi-serve /F"
    )]
    InstallService {
        /// Also register weekly `thickapi capture --all` (default off).
        #[arg(long)]
        weekly_capture: bool,
    },
}

#[derive(Subcommand)]
enum DoctorCommands {
    ClearHealth { account_id: String },
    ClearSession { session_id: String },
}

#[derive(Subcommand)]
enum ModelsCommands {
    /// Print every models.yaml row with its wire id, transport and aliases.
    List,
    /// Fetch each vendor's live model list and fail when a row is behind or gone.
    Check,
}

#[derive(Subcommand)]
enum KeysCommands {
    Create {
        #[arg(long)]
        label: String,
        /// `--scope` is repeatable. Default when absent: chat:write, models:read (thin). An
        /// unknown scope is a clap error, not a silent typo (D3 deliverable 1).
        #[arg(long = "scope", value_enum)]
        scope: Vec<KeyScopeArg>,
    },
    /// id, scopes, created — never hashes (D3 deliverable 2).
    List,
    /// Drop every key with this id; takes effect without a restart (`keys.yaml` mtime).
    Revoke { id: String },
}

/// CLI spelling of the scope strings in `thickapi_core::config::keys` (D3 deliverable 1).
#[derive(Clone, Debug, clap::ValueEnum)]
enum KeyScopeArg {
    #[value(name = "chat:write")]
    ChatWrite,
    #[value(name = "turn:write")]
    TurnWrite,
    #[value(name = "models:read")]
    ModelsRead,
}

impl KeyScopeArg {
    fn as_str(&self) -> &'static str {
        use thickapi_core::config::keys::{SCOPE_CHAT_WRITE, SCOPE_MODELS_READ, SCOPE_TURN_WRITE};
        match self {
            KeyScopeArg::ChatWrite => SCOPE_CHAT_WRITE,
            KeyScopeArg::TurnWrite => SCOPE_TURN_WRITE,
            KeyScopeArg::ModelsRead => SCOPE_MODELS_READ,
        }
    }
}

fn config_dir(cli: &Cli) -> std::path::PathBuf {
    cli.config
        .clone()
        .unwrap_or_else(thickapi_core::default_config_dir)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // The proposal server speaks JSON-RPC on stdout: no config load, tracing to stderr only.
    if let Commands::McpProposal {
        catalog,
        ready,
        violation,
    } = &cli.command
    {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(log_filter::env_filter().add_directive("thickapi=warn".parse()?))
            .init();
        let code = commands::mcp_proposal::run(catalog, ready, violation);
        std::process::exit(code);
    }

    let cfg_dir = config_dir(&cli);
    match &cli.command {
        Commands::Serve { dump_tools } => {
            commands::serve::run(&cfg_dir, dump_tools.clone()).await?;
            return Ok(());
        }
        _ => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(log_filter::env_filter())
                .init();
        }
    }

    match cli.command {
        Commands::McpProposal { .. } => unreachable!("handled above"),
        Commands::Serve { .. } => unreachable!("handled above"),
        Commands::Doctor { offline, action } => match action {
            Some(DoctorCommands::ClearHealth { account_id }) => {
                let code = commands::doctor::clear_health(&cfg_dir, &account_id)?;
                std::process::exit(code);
            }
            Some(DoctorCommands::ClearSession { session_id }) => {
                let code = commands::doctor::clear_session(&cfg_dir, &session_id)?;
                std::process::exit(code);
            }
            None => {
                let code = commands::doctor::run(&cfg_dir, offline).await?;
                std::process::exit(code);
            }
        },
        Commands::Reap { days, dry_run } => {
            commands::reap::run_from_config(&cfg_dir, days, dry_run)?;
        }
        Commands::InstallService { weekly_capture } => {
            commands::install_service::run(&cfg_dir, weekly_capture)?
        }
        Commands::Capture {
            vendor,
            model,
            out,
            scenario,
            all,
            keep_failed,
            check,
        } => {
            if vendor.as_deref() == Some("claude-tools") {
                commands::claude_tools::run(commands::claude_tools::ClaudeToolsArgs { out, check })
                    .await?;
            } else {
                commands::capture::run(
                    &cfg_dir,
                    commands::capture::CaptureArgs {
                        vendor,
                        model,
                        out,
                        scenario,
                        all,
                        keep_failed,
                        check,
                    },
                )
                .await?
            }
        }
        Commands::Turn {
            cwd,
            model,
            combo,
            allow_paid,
            stream,
            prompt,
        } => {
            commands::turn::run(&cfg_dir, cwd, model, combo, allow_paid, stream, prompt).await?;
        }
        Commands::SyncPi { key } => commands::sync_pi::run(&cfg_dir, key)?,
        Commands::Models { command } => {
            let code = match command {
                ModelsCommands::List => commands::models::list(&cfg_dir)?,
                ModelsCommands::Check => commands::models::check(&cfg_dir).await?,
            };
            std::process::exit(code);
        }
        Commands::Keys { command } => match command {
            KeysCommands::Create { label, scope } => {
                let scopes: Vec<&str> = scope.iter().map(KeyScopeArg::as_str).collect();
                commands::keys::create(&cfg_dir, label, &scopes)?
            }
            KeysCommands::List => commands::keys::list(&cfg_dir)?,
            KeysCommands::Revoke { id } => commands::keys::revoke(&cfg_dir, &id)?,
        },
    }
    Ok(())
}
