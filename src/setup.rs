//! Online setup: turn an existing `pnm` login into a working memory service.
//!
//! Every step here rides a flow the VTI stack already has. Nothing new is
//! invented, and that is the design:
//!
//! | Step | Existing asset |
//! |---|---|
//! | Authenticate as the operator | the `pnm` keyring session (`AgentConnect`'s session rung) |
//! | Find the trust context | `contexts/list/1.0` |
//! | Mint the agent identity | `vta_sdk::provision_client::setup_key::EphemeralSetupKey` |
//! | Authorize it | `acl/create` with role `application`, scoped to one context |
//! | Find the agent's way back in | `vta_sdk::session::resolve_vta_endpoint` |
//! | Prove it works | a real `vta/memory/list/0.1` as the new identity |
//!
//! **Online only.** There is no offline/air-gapped path: setup requires a
//! reachable VTA, because the last step is the one that matters — connecting as
//! the newly-minted identity and actually listing memories. A setup that writes
//! a config it has not exercised is a setup that fails later, in a hook, with
//! no operator watching.
//!
//! # Why a dedicated identity is the default
//!
//! Reusing the operator's `pnm` session is one flag away and stores no key. It
//! is also wrong for anything long-running: the memory service would
//! authenticate as the operator, inheriting every context and every capability
//! that login has, and revoking it would mean revoking the operator's own
//! login. The dedicated path mints a `did:key` whose ACL entry names exactly
//! one context and the least-privileged role that can reach memory, so
//! `pnm acl delete --did <agent>` is a complete off switch.

use anyhow::{Context as _, bail};
use vta_sdk::agent_connect::AgentConnect;
use vta_sdk::client::VtaClient;
use vta_sdk::prelude::CreateAclRequest;
use vta_sdk::provision_client::setup_key::EphemeralSetupKey;
use vta_sdk::session::{SessionStore, VtaEndpoint, resolve_vta_endpoint};

use crate::config::{CONFIG_VERSION, Config, Identity};

/// The role granted to a dedicated memory agent.
///
/// `application` is the least-privileged role that still satisfies the memory
/// tasks' gate. Those are gated on `require_context(payload.contextId)` and on
/// no capability at all, so what matters is context access, and the role only
/// has to be one whose non-empty context list means "these contexts". It must
/// **not** be `admin`: for `admin` an empty context list means *unrestricted*,
/// so an admin entry that ever lost its context scoping would silently reach
/// every context in the VTA. It must not be `monitor` either — that role is
/// authorized nowhere.
pub const AGENT_ROLE: &str = "application";

/// What `setup` decided, for printing.
pub struct SetupOutcome {
    pub config_path: std::path::PathBuf,
    pub context_id: String,
    pub identity_label: String,
    pub agent_did: Option<String>,
    pub memories_found: usize,
}

/// Inputs, straight off the CLI.
pub struct SetupArgs {
    /// `pnm` slug of the operator login to bootstrap from.
    pub vta: String,
    /// Service name the session is stored under.
    pub service_name: Option<String>,
    /// Trust context for memories. Resolved from the VTA when absent.
    pub context: Option<String>,
    /// Reuse the operator session instead of minting a dedicated agent.
    pub use_session: bool,
    /// Where to write the config.
    pub config_path: std::path::PathBuf,
    /// Overwrite an existing config.
    pub force: bool,
}

/// Run setup end to end.
pub async fn run(args: SetupArgs) -> anyhow::Result<SetupOutcome> {
    if args.config_path.exists() && !args.force {
        bail!(
            "{} already exists — pass --force to replace it (this discards the current agent \
             identity; revoke it afterwards with `pnm acl delete`)",
            args.config_path.display()
        );
    }

    // 1. Authenticate as the operator, over whatever transport the VTA
    //    advertises. This is the only privileged connection setup makes.
    let operator = AgentConnect {
        session_key: Some(args.vta.clone()),
        service_name: args.service_name.clone(),
        ..Default::default()
    }
    .connect()
    .await
    .with_context(|| {
        format!(
            "connecting to VTA `{}` with the stored pnm session — run `pnm auth status` to check \
             it, or `pnm setup` if you have not logged in on this machine",
            args.vta
        )
    })?;

    // Everything from here can fail, and the operator connection holds a live
    // mediator socket that Drop cannot close. Run the body, then shut down
    // unconditionally before propagating — a bare `?` would leak the socket and
    // leave it duelling the mediator's one-per-DID slot on reconnect.
    let result = setup_body(&operator, &args).await;
    operator.shutdown().await;
    result
}

async fn setup_body(operator: &VtaClient, args: &SetupArgs) -> anyhow::Result<SetupOutcome> {
    // 2. Resolve the trust context. This is the isolation boundary for every
    //    memory, so it is chosen explicitly rather than defaulted to something
    //    convenient.
    let context_id = resolve_context(operator, args.context.as_deref()).await?;

    // 3 + 4. Identity.
    let identity = if args.use_session {
        Identity::PnmSession {
            vta: args.vta.clone(),
            service_name: args.service_name.clone(),
        }
    } else {
        mint_agent_identity(
            operator,
            &args.vta,
            args.service_name.as_deref(),
            &context_id,
        )
        .await?
    };

    // 5. Prove it. Connect as the identity we just chose and do the real thing.
    let cfg = Config {
        version: CONFIG_VERSION,
        context_id: context_id.clone(),
        identity,
        recall_limit: 8,
    };
    let memories_found = verify(&cfg).await?;

    // 6. Only now does anything land on disk.
    cfg.save(&args.config_path)
        .with_context(|| format!("writing {}", args.config_path.display()))?;

    let agent_did = match &cfg.identity {
        Identity::Agent { did, .. } => Some(did.clone()),
        Identity::PnmSession { .. } => None,
    };
    Ok(SetupOutcome {
        config_path: args.config_path.clone(),
        context_id,
        identity_label: cfg.identity.label().to_string(),
        agent_did,
        memories_found,
    })
}

/// Pick the trust context, or explain how to.
///
/// An explicit `--context` is checked against what the VTA actually has, so a
/// typo fails here rather than as a `permissionDenied` on the first save.
async fn resolve_context(client: &VtaClient, requested: Option<&str>) -> anyhow::Result<String> {
    let listed = client
        .list_contexts()
        .await
        .context("listing trust contexts")?;
    let ids: Vec<String> = listed.contexts.iter().map(|c| c.id.clone()).collect();

    match requested {
        Some(id) => {
            if !ids.iter().any(|c| c == id) {
                bail!(
                    "no trust context `{id}` on this VTA.\n\nAvailable: {}\n\nCreate one with:\n  \
                     pnm contexts create --name {id}",
                    render_list(&ids)
                );
            }
            Ok(id.to_string())
        }
        None if ids.len() == 1 => Ok(ids[0].clone()),
        None if ids.is_empty() => bail!(
            "this VTA has no trust contexts yet. Create one for your memories:\n  \
             pnm contexts create --name memory"
        ),
        None => bail!(
            "several trust contexts exist — choose which one holds these memories with \
             --context <id>.\n\nAvailable: {}",
            render_list(&ids)
        ),
    }
}

fn render_list(ids: &[String]) -> String {
    if ids.is_empty() {
        "(none)".to_string()
    } else {
        ids.join(", ")
    }
}

/// Mint a `did:key`, grant it least-privilege access to one context, and work
/// out how it reaches the VTA on its own.
async fn mint_agent_identity(
    operator: &VtaClient,
    vta_slug: &str,
    service_name: Option<&str>,
    context_id: &str,
) -> anyhow::Result<Identity> {
    // The same helper every VTI two-phase setup uses to mint a provisioning
    // key — an Ed25519 did:key with its private half in multibase.
    let key = EphemeralSetupKey::generate().context("minting the agent did:key")?;

    // Least privilege, and remote-first: the ACL entry is created before
    // anything about this identity is written down. If the process dies on the
    // next line, the operator has an unused ACL entry to delete, which is
    // visible in `pnm acl list`. The other ordering leaves a config file
    // holding a key that authorizes nothing, which looks like a bug in the
    // agent rather than an incomplete setup.
    operator
        .create_acl(CreateAclRequest {
            did: key.did.clone(),
            role: AGENT_ROLE.to_string(),
            label: Some("vta-agent-memory".to_string()),
            allowed_contexts: vec![context_id.to_string()],
            expires_at: None,
            step_up_approver: None,
            step_up_require: None,
            approve_all_contexts: false,
            approve_contexts: Vec::new(),
            allowed_keys: None,
        })
        .await
        .with_context(|| {
            format!(
                "granting the memory agent `{}` access to context `{context_id}` \
                 (needs an operator login with admin over that context)",
                key.did
            )
        })?;

    // Where does this identity connect to? The operator session knows the VTA
    // DID; the VTA's own DID document knows the rest. Reading it here — rather
    // than asking the operator for a mediator DID — is what keeps setup to one
    // flag.
    let sessions = SessionStore::new(
        service_name.unwrap_or(vta_sdk::agent_connect::DEFAULT_SERVICE_NAME),
        default_sessions_dir()?,
    );
    let vta_did = sessions
        .loaded_session(vta_slug)
        .and_then(|s| s.vta_did)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the stored pnm session `{vta_slug}` has no VTA DID yet — finish `pnm setup` \
                 before provisioning a memory agent"
            )
        })?;

    let (mediator_did, rest_url) = match resolve_vta_endpoint(&vta_did)
        .await
        .map_err(|e| anyhow::anyhow!("resolving the VTA's DID document ({vta_did}): {e}"))?
    {
        // A dedicated agent authenticates over DIDComm, so it needs a mediator.
        // A TSP-advertising VTA still has to give us its DIDComm mediator: the
        // client's TSP leg rides an existing DIDComm session, so there is no
        // TSP-only way in for this identity.
        VtaEndpoint::DIDComm {
            mediator_did,
            rest_url,
            ..
        } => (mediator_did, rest_url),
        VtaEndpoint::Tsp {
            didcomm_mediator_did: Some(mediator_did),
            rest_url,
            ..
        } => (mediator_did, rest_url),
        VtaEndpoint::Tsp { rest_url, .. } => bail!(
            "this VTA advertises TSP but no DIDComm mediator, so a dedicated agent identity has \
             no way to connect{}.\n\nUse the operator session instead:\n  \
             vta-agent-memory setup --vta {vta_slug} --use-session",
            rest_url
                .as_deref()
                .map(|u| format!(
                    " (REST is available at {u}, but a did:key agent authenticates over DIDComm)"
                ))
                .unwrap_or_default()
        ),
        VtaEndpoint::Rest { url } => bail!(
            "this VTA advertises only REST ({url}), so a dedicated agent identity cannot \
             authenticate over DIDComm.\n\nUse the operator session instead:\n  \
             vta-agent-memory setup --vta {vta_slug} --use-session"
        ),
        // `VtaEndpoint` is `#[non_exhaustive]`: a transport added upstream
        // arrives here as an unknown variant. Refusing beats guessing — a
        // dedicated identity needs a mediator DID this arm cannot supply.
        _ => bail!(
            "this VTA advertises a transport this build does not know how to give a dedicated \
             agent identity.\n\nUse the operator session instead:\n  \
             vta-agent-memory setup --vta {vta_slug} --use-session"
        ),
    };

    Ok(Identity::Agent {
        did: key.did.clone(),
        private_key_multibase: key.private_key_multibase().to_string(),
        vta_did,
        mediator_did,
        rest_url,
    })
}

/// Connect as the configured identity and list memories.
///
/// The probe is `memory/list/0.1` and not a cheaper `whoami`, because listing
/// is the operation gated on exactly what setup just arranged: access to the
/// chosen context. A successful `whoami` proves the key authenticates and
/// nothing about whether it can reach the memories.
async fn verify(cfg: &Config) -> anyhow::Result<usize> {
    let client =
        cfg.to_agent_connect().connect().await.with_context(|| {
            format!("connecting as the {} just configured", cfg.identity.label())
        })?;
    let store = crate::store::Store::new(client, cfg.context_id.clone());
    let result = store.list().await.with_context(|| {
        format!(
            "the identity connected but cannot read memories in context `{}` — check \
             `pnm acl list` shows it with access to that context",
            cfg.context_id
        )
    });
    let count = match result {
        Ok(entries) => entries.len(),
        Err(e) => {
            store.shutdown().await;
            return Err(e);
        }
    };
    store.shutdown().await;
    Ok(count)
}

/// `~/.config/pnm`, matching where `pnm` writes sessions.
fn default_sessions_dir() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| anyhow::anyhow!("HOME is not set"))?;
    Ok(std::path::PathBuf::from(home).join(".config").join("pnm"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_role_is_never_admin() {
        // For `admin` an empty context list means *unrestricted*, so an admin
        // entry that ever lost its scoping would reach every context on the
        // VTA. A memory agent must not be one bad edit away from that.
        assert_ne!(AGENT_ROLE, "admin");
        assert_eq!(AGENT_ROLE, "application");
    }

    #[test]
    fn empty_context_lists_render_readably() {
        assert_eq!(render_list(&[]), "(none)");
        assert_eq!(render_list(&["a".into(), "b".into()]), "a, b");
    }
}
