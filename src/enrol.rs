//! Enrolment: mint a throwaway identity, get it granted, then rotate it away.
//!
//! # The pattern, and why it is not a local invention
//!
//! `vti_secrets::IntegrationOnboarding` packages the
//! **ephemeral `did:key` → ACL grant → auto-rotate on first connect** flow that
//! the mediator, PNM and the DID-hosting services already use. This module
//! drives it; it does not re-derive it.
//!
//! 1. **`init`** — mint a throwaway `did:key`, park it in the session store as
//!    *pending rotation*, and print the grant it needs. No credential is
//!    required and nothing authenticated is called, so this runs on a machine
//!    that has never spoken to the VTA.
//! 2. *(out of band)* somebody holding admin runs the grant. **On any
//!    machine** — another laptop, CI, a colleague reading it off a ticket.
//! 3. **`connect`** — on the first successful authentication the session store
//!    **atomically swaps the throwaway key for a fresh one**, mirrors the ACL
//!    entry onto the new DID, and drops the temp entry.
//!
//! # Why step 3 matters
//!
//! The DID from step 1 gets pasted into a ticket, a chat message, or a terminal
//! somebody else is screen-sharing. Rotation is what stops that DID from
//! remaining a live authenticator afterwards. An enrolment flow that mints a
//! long-term key and asks you to paste *its* DID around has quietly made the
//! copy-paste channel part of its trust boundary.
//!
//! It also means **no private key is ever written to this crate's config**.
//! The key lives in the session backend — the OS keyring, normally — and the
//! config records only which session to open.
//!
//! # What `init` needs
//!
//! The VTA's **DID** and the trust **context**. Nothing else: the mediator is
//! read from the VTA's DID document at connect time, which is a public lookup.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use vti_secrets::onboarding::IntegrationOnboarding;

use crate::config::{CONFIG_VERSION, Config, Identity, write_owner_only};
use crate::setup::{AGENT_ROLE, SetupOutcome, verify};

/// Pending-file schema version.
const PENDING_VERSION: u8 = 1;

/// Service namespace this crate stores its own sessions under. Distinct from
/// `pnm-cli` so a memory agent and an operator login never collide.
pub const SERVICE_NAME: &str = "vta-agent-memory";

/// Where this crate keeps its own session store — beside its config, and by the
/// same rule `pnm` uses, so it is right on every platform rather than on Linux.
pub fn sessions_dir() -> anyhow::Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine the user config directory"))?
        .join(SERVICE_NAME))
}

/// Session key for a memory agent in `context_id`. Keyed by context so one
/// machine can hold memories in several, each separately revocable.
pub fn session_key_for(context_id: &str) -> String {
    format!("agent:{context_id}")
}

/// What `init` records for `connect` to pick up.
///
/// **Holds no key** — the throwaway is already in the session store. It is
/// still written owner-only, because it names an identity awaiting a grant and
/// that is nobody else's business.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingEnrolment {
    pub version: u8,
    /// The throwaway `did:key` awaiting a grant — the DID that goes in the
    /// `pnm acl create`, and the one that stops working after `connect`.
    pub ephemeral_did: String,
    /// The VTA to authenticate to.
    pub vta_did: String,
    /// The trust context the grant must name.
    pub context_id: String,
    /// Session store coordinates, so `connect` opens exactly what `init` wrote.
    pub session_key: String,
    pub sessions_dir: PathBuf,
    /// Mediator hint, for a VTA whose DID document does not advertise one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mediator_did: Option<String>,
    /// REST URL override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl PendingEnrolment {
    /// The commands an operator runs to authorize this enrolment, printed
    /// verbatim so they can be pasted — or dropped into a ticket — unedited.
    ///
    /// Two of them, because the context may not exist yet and `pnm contexts
    /// create` takes both an `--id` and a `--name`. The role is `application`,
    /// never `admin`: an empty `allowed_contexts` means *unrestricted* for
    /// admin and *authorized nowhere* for every other role, so an admin grant
    /// is one bad edit away from reaching the whole VTA.
    pub fn grant_commands(&self) -> Vec<String> {
        vec![
            format!(
                "pnm contexts create --id {ctx} --name {ctx}    # skip if it exists",
                ctx = self.context_id
            ),
            format!(
                "pnm acl create --did {} --role {AGENT_ROLE} --contexts {} --label vta-agent-memory",
                self.ephemeral_did, self.context_id
            ),
        ]
    }

    /// Where the pending file lives, beside the config it will become.
    pub fn path_beside(config_path: &Path) -> PathBuf {
        config_path.with_file_name("pending-enrolment.json")
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_owner_only(path, &serde_json::to_string_pretty(self)?)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!(
                "nothing is waiting to be connected at {} ({e}).\n\nStart with:\n  \
                 vta-agent-memory init --vta-did <did:…> --context <id>",
                path.display()
            )
        })?;
        let p: PendingEnrolment =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if p.version > PENDING_VERSION {
            bail!(
                "{} was written by a newer vta-agent-memory — upgrade the binary",
                path.display()
            );
        }
        Ok(p)
    }

    /// The onboarding driver for this enrolment.
    pub fn onboarding(&self) -> IntegrationOnboarding {
        IntegrationOnboarding::with_default_backend(
            SERVICE_NAME,
            self.sessions_dir.clone(),
            self.session_key.clone(),
        )
    }
}

/// Phase-1 inputs.
pub struct InitArgs {
    pub vta_did: String,
    pub context_id: String,
    pub mediator_did: Option<String>,
    pub url: Option<String>,
    pub config_path: PathBuf,
    pub force: bool,
}

/// Mint the throwaway identity and describe the grant it needs.
pub fn init(args: InitArgs) -> anyhow::Result<PendingEnrolment> {
    let pending_path = PendingEnrolment::path_beside(&args.config_path);
    if pending_path.exists() && !args.force {
        let existing = PendingEnrolment::load(&pending_path)?;
        bail!(
            "an enrolment is already pending for {} in context `{}`.\n\nGrant it:\n{}\n\nThen:\n  \
             vta-agent-memory connect\n\nOr start over with --force.",
            existing.ephemeral_did,
            existing.context_id,
            existing
                .grant_commands()
                .iter()
                .map(|c| format!("  {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    if args.config_path.exists() && !args.force {
        bail!(
            "{} already exists — this machine is already enrolled.\n\nPass --force to replace it, \
             and revoke the old agent afterwards with `pnm acl delete`.",
            args.config_path.display()
        );
    }

    let sessions_dir = sessions_dir()?;
    let session_key = session_key_for(&args.context_id);
    let onboarding = IntegrationOnboarding::with_default_backend(
        SERVICE_NAME,
        sessions_dir.clone(),
        session_key.clone(),
    );

    // Mints the throwaway key and parks it as pending-rotation. The private
    // half goes straight into the session store and is never returned here, so
    // it cannot end up in a log, a config file, or an error message.
    let ticket = onboarding
        .begin(&args.vta_did)
        .map_err(|e| anyhow::anyhow!("minting the enrolment identity: {e}"))?;

    let pending = PendingEnrolment {
        version: PENDING_VERSION,
        ephemeral_did: ticket.ephemeral_did().to_string(),
        vta_did: ticket.vta_did().to_string(),
        context_id: args.context_id,
        session_key,
        sessions_dir,
        mediator_did: args.mediator_did,
        url: args.url,
    };
    pending
        .save(&pending_path)
        .with_context(|| format!("writing {}", pending_path.display()))?;
    Ok(pending)
}

/// Phase-2 inputs.
pub struct ConnectArgs {
    pub config_path: PathBuf,
    pub recall_limit: usize,
}

/// Authenticate with the granted identity, **rotate off it**, prove the result
/// reaches the context, and keep it.
///
/// The proof is a real `vta/memory/list/0.1`, not a cheaper `whoami`: listing is
/// the operation gated on exactly what the grant was supposed to arrange, and a
/// `whoami` that succeeds tells you the key authenticates and nothing about
/// whether it can reach the memories.
pub async fn connect(args: ConnectArgs) -> anyhow::Result<SetupOutcome> {
    let pending_path = PendingEnrolment::path_beside(&args.config_path);
    let pending = PendingEnrolment::load(&pending_path)?;
    let onboarding = pending.onboarding();

    // Rotation happens inside this call, on first successful auth.
    let client = onboarding
        .connect(pending.url.as_deref(), pending.mediator_did.as_deref())
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "connecting as {} failed: {e}\n\nIf the grant has not been made yet, run this \
                 where you hold VTA admin:\n{}\n\nthen try again.",
                pending.ephemeral_did,
                pending
                    .grant_commands()
                    .iter()
                    .map(|c| format!("  {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })?;

    let cfg = Config {
        version: CONFIG_VERSION,
        context_id: pending.context_id.clone(),
        identity: Identity {
            service_name: SERVICE_NAME.to_string(),
            session_key: pending.session_key.clone(),
            sessions_dir: pending.sessions_dir.clone(),
            vta_did: pending.vta_did.clone(),
            operator_login: false,
        },
        recall_limit: args.recall_limit,
    };

    // Verify over the connection just made, then close it — a DIDComm client
    // owns a mediator socket `Drop` cannot release, and the mediator allows one
    // per DID, so leaving it open would duel the next connect.
    let store = crate::store::Store::new(client, pending.context_id.clone());
    let listed = store.list().await;
    store.shutdown().await;
    let memories_found = listed
        .with_context(|| {
            format!(
                "authenticated, but cannot read memories in context `{}` — check `pnm acl list` \
                 shows this agent with access to it",
                pending.context_id
            )
        })?
        .len();

    cfg.save(&args.config_path)
        .with_context(|| format!("writing {}", args.config_path.display()))?;
    let _ = std::fs::remove_file(&pending_path);

    // The DID that reached the operator is gone; report the one now in use.
    let rotated_did = onboarding
        .session_store()
        .loaded_session(&pending.session_key)
        .map(|s| s.client_did);

    Ok(SetupOutcome {
        config_path: args.config_path,
        vta_did: pending.vta_did,
        context_id: pending.context_id,
        identity_label: cfg.identity.label().to_string(),
        agent_did: rotated_did,
        memories_found,
    })
}

/// Verify a config by connecting as it. Re-exported for `setup`, which builds
/// the same shape by a different route.
pub async fn verify_config(cfg: &Config) -> anyhow::Result<usize> {
    verify(cfg).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingEnrolment {
        PendingEnrolment {
            version: PENDING_VERSION,
            ephemeral_did: "did:key:z6MkTemp".into(),
            vta_did: "did:webvh:abc:example.com:vta".into(),
            context_id: "agent-memory".into(),
            session_key: session_key_for("agent-memory"),
            sessions_dir: PathBuf::from("/tmp/sessions"),
            mediator_did: None,
            url: None,
        }
    }

    #[test]
    fn the_grant_commands_match_the_real_pnm_flags() {
        // `pnm contexts create` takes --id AND --name; getting this wrong hands
        // somebody a command that fails, in a ticket, with no way to tell
        // whether the tool or the instruction is broken.
        let cmds = pending().grant_commands();
        assert!(
            cmds[0].starts_with("pnm contexts create --id agent-memory --name agent-memory"),
            "{}",
            cmds[0]
        );
        assert_eq!(
            cmds[1],
            "pnm acl create --did did:key:z6MkTemp --role application \
             --contexts agent-memory --label vta-agent-memory"
        );
    }

    #[test]
    fn the_grant_is_never_admin() {
        // For `admin` an empty context list means *unrestricted*.
        assert!(
            pending()
                .grant_commands()
                .iter()
                .all(|c| !c.contains("admin"))
        );
    }

    #[test]
    fn session_keys_are_scoped_per_context() {
        // So one machine can hold memories in several contexts, each revocable
        // on its own.
        assert_ne!(session_key_for("work"), session_key_for("personal"));
        assert_eq!(session_key_for("work"), "agent:work");
    }

    #[test]
    fn our_sessions_never_collide_with_pnms() {
        assert_ne!(SERVICE_NAME, "pnm-cli");
        assert!(!session_key_for("x").starts_with("vta:"));
    }

    #[test]
    fn the_pending_file_carries_no_key_material() {
        // The throwaway private key belongs in the session store, not here.
        let json = serde_json::to_string(&pending()).unwrap();
        assert!(!json.contains("privateKey"), "{json}");
        assert!(!json.contains("private_key"), "{json}");
    }

    #[test]
    fn pending_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-enrolment.json");
        pending().save(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let back = PendingEnrolment::load(&path).unwrap();
        assert_eq!(back.ephemeral_did, "did:key:z6MkTemp");
        assert_eq!(back.session_key, "agent:agent-memory");
    }

    #[test]
    fn a_missing_pending_file_points_at_init() {
        let err = PendingEnrolment::load(Path::new("/nonexistent/pending.json")).unwrap_err();
        assert!(err.to_string().contains("vta-agent-memory init"), "{err}");
    }

    #[test]
    fn init_refuses_to_silently_replace_a_pending_enrolment() {
        // Overwriting would strand an already-granted DID in the ACL with
        // nothing left on disk that names it.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        pending()
            .save(&PendingEnrolment::path_beside(&config_path))
            .unwrap();

        let err = init(InitArgs {
            vta_did: "did:webvh:abc:example.com:vta".into(),
            context_id: "agent-memory".into(),
            mediator_did: None,
            url: None,
            config_path,
            force: false,
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already pending"), "{msg}");
        assert!(
            msg.contains("pnm acl create"),
            "it reprints the grant: {msg}"
        );
    }

    #[test]
    fn init_rejects_a_vta_did_that_is_not_a_did() {
        let dir = tempfile::tempdir().unwrap();
        let err = init(InitArgs {
            vta_did: "vta.example.com".into(),
            context_id: "ctx".into(),
            mediator_did: None,
            url: None,
            config_path: dir.path().join("config.json"),
            force: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("did:"), "{err}");
    }
}
