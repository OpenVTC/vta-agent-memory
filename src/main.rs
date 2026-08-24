//! `vta-agent-memory` — durable agent memory backed by a Verifiable Trust
//! Agent.
//!
//! One binary, two front ends, because a Claude Code plugin needs both:
//!
//! - **`serve`** — an MCP server over stdio. What the plugin's `.mcp.json`
//!   launches; gives the model `memory_save` / `memory_recall` / … as tools.
//! - **`recall` / `list` / `forget` / `doctor`** — one-shot commands. Hooks are
//!   shell commands, not MCP calls, so a `SessionStart` hook that pre-loads the
//!   user's memories needs something it can execute and read stdout from. That
//!   is the difference between memory the model has to remember to ask for and
//!   memory that is simply there.
//!
//! Plus **`setup`**, which is what makes the other two need no flags.
//!
//! Logging goes to stderr throughout: in `serve` mode stdout is the MCP
//! JSON-RPC channel, and in `recall` mode stdout is the hook's context payload.
//! Either way, one stray `println!` corrupts it.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::stdio;

use vta_agent_memory::config::Config;
use vta_agent_memory::record::{self, MemoryKey, MemoryType};
use vta_agent_memory::server::MemoryMcp;
use vta_agent_memory::setup;
use vta_agent_memory::store::Store;

#[derive(Parser, Debug)]
#[command(
    name = "vta-agent-memory",
    version,
    about = "Agent memory stored in your own Verifiable Trust Agent"
)]
struct Cli {
    /// Path to the memory config (default: `~/.config/vta-agent-memory/config.json`).
    #[arg(long, global = true, env = "VTA_AGENT_MEMORY_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve the memory tools over MCP on stdio. The default.
    Serve,

    /// Phase 1 of enrolment: mint this machine's agent identity and print the
    /// grant it needs. Needs no credential and no `pnm` — the grant is run by
    /// somebody holding admin, on any machine.
    Init {
        /// The VTA's DID.
        #[arg(long, value_name = "DID")]
        vta_did: String,
        /// The trust context memories will live in. This is the isolation
        /// boundary: an agent scoped elsewhere cannot see them.
        #[arg(long, value_name = "ID")]
        context: String,
        /// Mediator DID, for a VTA whose DID document does not advertise one
        /// (a `did:key` VTA, an airgapped deployment).
        #[arg(long, value_name = "DID")]
        mediator_did: Option<String>,
        /// REST URL, likewise.
        #[arg(long)]
        url: Option<String>,
        /// Replace a pending enrolment or an existing config.
        #[arg(long)]
        force: bool,
    },

    /// Phase 2: connect with the granted identity and keep it. Run this once
    /// somebody has executed the grant `init` printed.
    Connect,

    /// Do both phases at once, using a `pnm` login on *this* machine to make
    /// the grant itself. Convenient, but needs an operator credential here —
    /// prefer `init` + `connect` otherwise.
    Setup {
        /// Which VTA — its DID (`did:webvh:…`), or the local `pnm` name.
        /// Omit to use whichever VTA `pnm` treats as the default.
        ///
        /// Prefer the DID: a `pnm` name is a nickname chosen on this machine
        /// and means nothing on any other, so a setup instruction written with
        /// one cannot be copied into a runbook.
        #[arg(long, env = "VTA_AGENT_MEMORY_VTA", value_name = "DID_OR_NAME")]
        vta: Option<String>,
        /// Service name the session is stored under (default `pnm-cli`).
        #[arg(long)]
        service_name: Option<String>,
        /// Trust context to keep memories in. Required when the VTA has more
        /// than one.
        #[arg(long)]
        context: Option<String>,
        /// Authenticate as the operator's own `pnm` session instead of minting
        /// a dedicated, context-scoped agent identity. Stores no key, but the
        /// memory service then inherits the operator's whole reach.
        #[arg(long)]
        use_session: bool,
        /// Replace an existing config.
        #[arg(long)]
        force: bool,
    },

    /// Print stored memories for a session-start hook to load.
    Recall {
        /// What to look for. Omit for the most recently updated memories.
        #[arg(long)]
        query: Option<String>,
        /// Narrow to one type: user, feedback, project, reference.
        #[arg(long = "type")]
        kind: Option<String>,
        /// Maximum memories to print.
        #[arg(long)]
        limit: Option<usize>,
        /// `text` (default, human/markdown) or `json` (a Claude Code
        /// `hookSpecificOutput.additionalContext` envelope).
        #[arg(long, default_value = "text")]
        format: OutputFormat,
        /// Include each memory's full body, not just its description.
        #[arg(long)]
        full: bool,
    },

    /// List every stored memory.
    List {
        /// Narrow to one type.
        #[arg(long = "type")]
        kind: Option<String>,
    },

    /// Permanently delete one memory by key.
    Forget {
        /// The memory key, e.g. `feedback/no-pr-attribution`.
        key: String,
    },

    /// Check the configured connection and context access.
    Doctor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Pin rustls to aws-lc-rs before any TLS object exists. Without it,
    // rustls 0.23 panics on backend auto-detection when both backends are
    // compiled in — which they are, via the resolver's transitive deps.
    vta_sdk::crypto_init::install_default_crypto_provider();
    // The keyring backend the `pnm` session store reads from needs its
    // platform store installed once, before any entry is opened. A failure
    // here is not fatal: only the session rung reads the keyring, so a
    // dedicated-agent config still works on a machine with no usable keyring
    // (a headless box, a locked login session). Warn and carry on — the
    // session rung will fail later with an error that names what it wanted.
    if let Err(e) = vta_sdk::keyring_init::install_default_store() {
        tracing::warn!(error = %e, "no OS keyring available; `pnm` session reuse will not work");
    }

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let config_path = match &cli.config {
        Some(p) => p.clone(),
        None => Config::default_path()?,
    };

    match cli.command.unwrap_or(Command::Serve) {
        Command::Init {
            vta_did,
            context,
            mediator_did,
            url,
            force,
        } => {
            let pending = vta_agent_memory::enrol::init(vta_agent_memory::enrol::InitArgs {
                vta_did,
                context_id: context,
                mediator_did,
                url,
                config_path,
                force,
            })
            .await?;
            print_pending(&pending);
            Ok(())
        }
        Command::Connect => {
            let outcome = vta_agent_memory::enrol::connect(vta_agent_memory::enrol::ConnectArgs {
                config_path,
                recall_limit: 8,
            })
            .await?;
            print_setup_outcome(&outcome);
            Ok(())
        }
        Command::Setup {
            vta,
            service_name,
            context,
            use_session,
            force,
        } => {
            let outcome = setup::run(setup::SetupArgs {
                vta,
                service_name,
                context,
                use_session,
                config_path,
                force,
            })
            .await?;
            print_setup_outcome(&outcome);
            Ok(())
        }
        Command::Serve => serve(&config_path).await,
        Command::Recall {
            query,
            kind,
            limit,
            format,
            full,
        } => recall(&config_path, query, kind, limit, format, full).await,
        Command::List { kind } => list(&config_path, kind).await,
        Command::Forget { key } => forget(&config_path, &key).await,
        Command::Doctor => doctor(&config_path).await,
    }
}

/// Open the configured store. Every non-setup subcommand starts here.
async fn open(config_path: &std::path::Path) -> anyhow::Result<(Config, Store)> {
    let cfg = Config::load(config_path)?;
    let client = cfg.to_agent_connect().connect().await?;
    let store = Store::new(client, cfg.context_id.clone());
    Ok((cfg, store))
}

async fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    // Serve unconditionally. Claude Code starts this at session start and takes
    // what it gets: a process that exits before speaking MCP does not show up as
    // a broken memory service, it shows up as no memory tools at all, and the
    // model then has no way to tell anyone why. Connecting happens on first use.
    let lazy = Arc::new(vta_agent_memory::lazy::LazyStore::new(config_path));
    tracing::info!(
        config = %config_path.display(),
        "serving MCP over stdio; the VTA is contacted on the first memory call"
    );

    let mcp = MemoryMcp::new(lazy.clone());

    // Serve, wait, then shut the transport down *however* serving ended. A
    // bare `?` on `waiting()` would skip teardown on the ordinary
    // EOF/disconnect path, leaving a live mediator socket behind that
    // auto-reconnects and holds the one-per-DID slot.
    let served = async {
        let service = mcp.serve(stdio()).await?;
        service.waiting().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    lazy.shutdown().await;
    served
}

async fn recall(
    config_path: &std::path::Path,
    query: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
    format: OutputFormat,
    full: bool,
) -> anyhow::Result<()> {
    // `--format json` is the hook path, and a hook has a different contract
    // from a command a person ran. Two consequences, both deliberate:
    //
    // 1. **Never fail the session.** An unreachable VTA, an expired grant, a
    //    machine that has not been set up — none of those are reasons to put an
    //    error in front of someone who just opened a terminal. Report to stderr
    //    and exit 0 with no context.
    // 2. **Say nothing when there is nothing.** Injecting "no memories stored"
    //    into every session is noise that never becomes signal.
    //
    // The text path keeps ordinary CLI behaviour: a person who ran `recall`
    // wants to know it failed, and wants to be told the context is empty.
    let hook_mode = format == OutputFormat::Json;

    let outcome = async {
        let (cfg, store) = open(config_path).await?;
        let kind = parse_kind(kind.as_deref())?;
        let limit = limit.unwrap_or(cfg.recall_limit);
        let result = store.recall(&query.unwrap_or_default(), kind, limit).await;
        let hits = finish(store, result).await?;
        let entries: Vec<_> = hits.iter().map(|h| &h.entry).collect();
        let empty = entries.is_empty();
        Ok::<_, anyhow::Error>((render_memories(&cfg.context_id, entries, full), empty))
    }
    .await;

    let (body, empty) = match outcome {
        Ok(v) => v,
        Err(e) if hook_mode => {
            tracing::warn!(error = %format!("{e:#}"), "memory recall unavailable; starting without it");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    match format {
        OutputFormat::Text => println!("{body}"),
        OutputFormat::Json if empty => {}
        OutputFormat::Json => {
            // The envelope Claude Code reads for SessionStart hooks.
            let out = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": body,
                }
            });
            println!("{}", serde_json::to_string(&out)?);
        }
    }
    Ok(())
}

async fn list(config_path: &std::path::Path, kind: Option<String>) -> anyhow::Result<()> {
    let (cfg, store) = open(config_path).await?;
    let kind = parse_kind(kind.as_deref())?;
    let result = store.list_of_type(kind).await;
    let mut entries = finish(store, result).await?;
    entries.sort_by_key(|e| e.key.to_string());
    println!(
        "{}",
        render_memories(&cfg.context_id, entries.iter().collect::<Vec<_>>(), false)
    );
    Ok(())
}

async fn forget(config_path: &std::path::Path, key: &str) -> anyhow::Result<()> {
    let (_cfg, store) = open(config_path).await?;
    let parsed = MemoryKey::parse(key)?;
    let result = store.forget(&parsed).await;
    finish(store, result).await?;
    println!("Forgot {parsed}.");
    Ok(())
}

async fn doctor(config_path: &std::path::Path) -> anyhow::Result<()> {
    let cfg = Config::load(config_path)?;
    println!("config      {}", config_path.display());
    println!("vta         {}", cfg.identity.vta_did());
    println!("context     {}", cfg.context_id);
    println!("identity    {}", cfg.identity.label());

    let client = cfg.to_agent_connect().connect().await?;
    println!("transport   {:?}", client.trust_task_transport());

    let store = Store::new(client, cfg.context_id.clone());
    let result = store.list().await;
    match finish(store, result).await {
        Ok(entries) => {
            println!("memories    {}", entries.len());
            println!("\nOK — the memory service can read and write this context.");
            Ok(())
        }
        Err(e) => {
            println!("memories    unavailable");
            Err(e)
        }
    }
}

/// Shut the transport down, then yield the result.
///
/// Every command needs this and none of them can use `?` before it: a DIDComm
/// or TSP client owns a live mediator socket with no `Drop` teardown, so an
/// early return leaks it.
async fn finish<T>(store: Store, result: anyhow::Result<T>) -> anyhow::Result<T> {
    store.shutdown().await;
    result
}

fn parse_kind(raw: Option<&str>) -> anyhow::Result<Option<MemoryType>> {
    match raw {
        None => Ok(None),
        Some(s) => MemoryType::parse(s).map(Some).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown memory type `{s}`; expected user, feedback, project or reference"
            )
        }),
    }
}

/// Render memories as markdown for a hook or a terminal.
fn render_memories(context_id: &str, entries: Vec<&record::Entry>, full: bool) -> String {
    if entries.is_empty() {
        return format!("No memories stored in trust context `{context_id}`.");
    }
    let mut out = format!(
        "# Stored memories ({} in trust context `{context_id}`)\n",
        entries.len()
    );
    for kind in MemoryType::ALL {
        let of_kind: Vec<&&record::Entry> = entries.iter().filter(|e| e.key.kind == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {kind}\n"));
        for e in of_kind {
            out.push_str(&format!(
                "\n- **{}** (`{}`) — {}",
                e.record.name, e.key, e.record.description
            ));
            if full && !e.record.body.is_empty() {
                out.push_str(&format!("\n\n  {}", e.record.body.replace('\n', "\n  ")));
            }
        }
        out.push('\n');
    }
    out
}

/// Phase-1 output. The grant command is the deliverable — it is meant to be
/// copied into a ticket or a chat message and run by somebody else, so it is
/// printed on its own line, unadorned and unwrapped.
fn print_pending(p: &vta_agent_memory::enrol::PendingAgent) {
    println!("This machine's memory agent is ready to be granted access.\n");
    println!("  agent DID   {}", p.agent_did);
    println!("  vta         {}", p.vta_did);
    println!("  context     {}", p.context_id);
    println!("  mediator    {}", p.mediator_did);
    println!("\nRun this wherever you hold VTA admin — it does not have to be this machine:\n");
    println!("  {}\n", p.grant_command());
    println!("Then, back here:\n");
    println!("  vta-agent-memory connect");
}

fn print_setup_outcome(o: &setup::SetupOutcome) {
    println!("Memory service configured.\n");
    println!("  config      {}", o.config_path.display());
    println!("  vta         {}", o.vta_did);
    println!("  context     {}", o.context_id);
    println!("  identity    {}", o.identity_label);
    if let Some(did) = &o.agent_did {
        println!("  agent DID   {did}");
    }
    println!("  memories    {} already stored", o.memories_found);
    println!("\nEnable it in Claude Code:");
    println!("  claude plugin install vta-agent-memory");
    if let Some(did) = &o.agent_did {
        println!("\nTo revoke this machine's access later:");
        println!("  pnm acl delete --did {did}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use record::{Entry, MemoryRecord};

    fn entry(kind: MemoryType, name: &str, desc: &str, body: &str) -> Entry {
        Entry {
            key: MemoryKey::new(kind, name).unwrap(),
            record: MemoryRecord::new(kind, name, desc, body, vec![]),
        }
    }

    #[test]
    fn an_empty_context_renders_as_a_sentence_not_an_empty_heading() {
        // This string is pasted straight into a session's context by the hook;
        // a bare heading with nothing under it reads like a failure.
        let out = render_memories("proj", vec![], false);
        assert!(out.contains("No memories stored"));
        assert!(!out.contains('#'));
    }

    #[test]
    fn memories_are_grouped_by_type_in_a_fixed_order() {
        let a = entry(MemoryType::Project, "P", "a project", "body");
        let b = entry(MemoryType::User, "U", "a user fact", "body");
        let out = render_memories("proj", vec![&a, &b], false);
        let user_at = out.find("## user").expect("user section");
        let project_at = out.find("## project").expect("project section");
        assert!(
            user_at < project_at,
            "MemoryType::ALL order is the render order"
        );
    }

    #[test]
    fn summaries_omit_bodies_unless_asked() {
        let e = entry(MemoryType::User, "U", "one line", "the long body");
        let brief = render_memories("proj", vec![&e], false);
        assert!(brief.contains("one line"));
        assert!(!brief.contains("the long body"), "bodies cost context");
        assert!(render_memories("proj", vec![&e], true).contains("the long body"));
    }

    #[test]
    fn unknown_types_are_rejected_before_any_connection() {
        assert!(parse_kind(Some("secrets")).is_err());
        assert_eq!(parse_kind(None).unwrap(), None);
        assert_eq!(
            parse_kind(Some("Feedback")).unwrap(),
            Some(MemoryType::Feedback)
        );
    }

    #[test]
    fn the_cli_defaults_to_serving() {
        let cli = Cli::parse_from(["vta-agent-memory"]);
        assert!(cli.command.is_none(), "no subcommand means Serve");
    }

    /// `--vta` takes the VTA's DID, its local `pnm` name, or nothing at all
    /// (meaning `pnm`'s default). The DID is the form worth putting in a
    /// runbook — a `pnm` name only means something on the machine that chose
    /// it — so the flag must not insist on either one.
    #[test]
    fn setup_accepts_a_did_a_name_or_nothing() {
        for args in [
            vec!["vta-agent-memory", "setup"],
            vec!["vta-agent-memory", "setup", "--vta", "mine"],
            vec![
                "vta-agent-memory",
                "setup",
                "--vta",
                "did:webvh:abc:example.com:vta",
            ],
        ] {
            assert!(
                Cli::try_parse_from(&args).is_ok(),
                "setup should accept {args:?}"
            );
        }
    }
}
