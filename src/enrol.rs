//! Two-phase enrolment: this machine mints its own identity and asks to be
//! granted access, rather than holding an operator credential.
//!
//! # Why this is the primary path
//!
//! The `setup` flow bootstraps from a `pnm` login, which is convenient and
//! wrong for the general case. It requires the machine that will hold the
//! memories to also hold an **operator** credential with admin over the
//! context — a credential far more powerful than anything a memory service
//! should be near, on a laptop that may be nobody's admin workstation.
//!
//! This is the shape the rest of the VTI stack already uses for exactly that
//! reason. From the workspace's own guidance: *"VTC deliberately never holds a
//! VTA admin credential (no self-grant), same as mediator / did-hosting."* The
//! mediator, the DID-hosting daemon, and the VTC all provision this way:
//!
//! 1. **`init`** — mint an agent `did:key` here, persist it `0600`, and print
//!    the exact `pnm acl create` the grant needs. Nothing privileged happens,
//!    and nothing reaches the VTA.
//! 2. *(out of band)* somebody holding admin runs that grant. **On any
//!    machine** — that is the whole point. It can be a different laptop, a CI
//!    job, or a person in another timezone reading it off a ticket.
//! 3. **`connect`** — authenticate with the now-authorised key, prove it can
//!    actually reach the context, and write the config.
//!
//! The two halves are joined by a pending file, not by a session: nothing about
//! phase 1 assumes phase 2 happens soon, on the same day, or with the same
//! person watching.
//!
//! # What phase 1 needs
//!
//! The VTA's **DID** and the trust **context**. Everything else — the mediator,
//! the REST fallback — is read out of the VTA's DID document, so the operator
//! is not asked for endpoints the network can already answer for. Overrides
//! exist for VTAs that cannot advertise (a `did:key` VTA, an airgapped one),
//! because for those the network cannot.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use vta_sdk::provision_client::setup_key::EphemeralSetupKey;

use crate::config::{CONFIG_VERSION, Config, Identity, write_owner_only};
use crate::setup::{AGENT_ROLE, SetupOutcome, discover_didcomm_endpoint, verify};

/// Pending-file schema version.
const PENDING_VERSION: u8 = 1;

/// What `init` mints and `connect` picks up.
///
/// Holds a private key, so it is written `0600` — and deleted once `connect`
/// has folded it into the real config, so the key exists in one place rather
/// than two.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingAgent {
    pub version: u8,
    /// The agent `did:key` awaiting a grant. This is the DID that goes in the
    /// `pnm acl create`.
    pub agent_did: String,
    /// Its Ed25519 signing key, multibase. Never leaves this machine.
    pub private_key_multibase: String,
    /// The VTA to authenticate to.
    pub vta_did: String,
    /// The trust context the grant must name.
    pub context_id: String,
    /// The mediator, resolved at `init` so `connect` needs no network beyond
    /// the connection itself.
    pub mediator_did: String,
    /// REST fallback, when the VTA advertises or was configured with one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rest_url: Option<String>,
}

impl PendingAgent {
    /// The `pnm` command that grants this agent what it needs — printed
    /// verbatim so it can be pasted, or copied into a ticket, unedited.
    ///
    /// `application`, never `admin`: an empty `allowed_contexts` means
    /// *unrestricted* for admin and *authorized nowhere* for every other role,
    /// so an admin grant is one bad edit away from reaching the whole VTA.
    pub fn grant_command(&self) -> String {
        format!(
            "pnm acl create --did {} --role {AGENT_ROLE} --contexts {} --label vta-agent-memory",
            self.agent_did, self.context_id
        )
    }

    /// Where the pending file lives, beside the config it will become.
    pub fn path_beside(config_path: &Path) -> PathBuf {
        config_path.with_file_name("pending-agent.json")
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
        let p: PendingAgent =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if p.version > PENDING_VERSION {
            bail!(
                "{} was written by a newer vta-agent-memory — upgrade the binary",
                path.display()
            );
        }
        Ok(p)
    }
}

/// Phase-1 inputs.
pub struct InitArgs {
    /// The VTA's DID. Required — this is the identifier that travels.
    pub vta_did: String,
    /// The trust context memories will live in.
    pub context_id: String,
    /// Mediator override, for a VTA that cannot advertise one.
    pub mediator_did: Option<String>,
    /// REST URL override.
    pub url: Option<String>,
    /// Where the config will eventually be written.
    pub config_path: PathBuf,
    /// Replace an existing pending enrolment.
    pub force: bool,
}

/// Mint the agent identity and describe the grant it needs.
///
/// Deliberately makes **no authenticated call**. The VTA is contacted only to
/// resolve its DID document, which is public, so this works before anybody has
/// granted this machine anything.
pub async fn init(args: InitArgs) -> anyhow::Result<PendingAgent> {
    let pending_path = PendingAgent::path_beside(&args.config_path);
    if pending_path.exists() && !args.force {
        let existing = PendingAgent::load(&pending_path)?;
        bail!(
            "an enrolment is already pending for {} in context `{}`.\n\nGrant it:\n  {}\n\nThen:\n  \
             vta-agent-memory connect\n\nOr start over with --force (which abandons that DID; \
             delete its ACL entry if it was already granted).",
            existing.agent_did,
            existing.context_id,
            existing.grant_command()
        );
    }
    if args.config_path.exists() && !args.force {
        bail!(
            "{} already exists — this machine is already enrolled.\n\nPass --force to replace it, \
             and revoke the old agent afterwards with `pnm acl delete`.",
            args.config_path.display()
        );
    }

    // The same helper every VTI two-phase setup uses.
    let key = EphemeralSetupKey::generate().context("minting the agent did:key")?;

    let (mediator_did, rest_url) =
        discover_didcomm_endpoint(&args.vta_did, args.mediator_did, args.url).await?;

    let pending = PendingAgent {
        version: PENDING_VERSION,
        agent_did: key.did.clone(),
        private_key_multibase: key.private_key_multibase().to_string(),
        vta_did: args.vta_did,
        context_id: args.context_id,
        mediator_did,
        rest_url,
    };
    pending.save(&pending_path).with_context(|| {
        format!(
            "writing the pending enrolment to {}",
            pending_path.display()
        )
    })?;
    Ok(pending)
}

/// Phase-2 inputs.
pub struct ConnectArgs {
    /// Where the config will be written.
    pub config_path: PathBuf,
    /// How many memories a bare `recall` returns.
    pub recall_limit: usize,
}

/// Authenticate with the granted key, prove it reaches the context, and keep it.
///
/// The proof is a real `vta/memory/list/0.1`, not a cheaper `whoami`: listing is
/// the operation gated on exactly what the grant was supposed to arrange, and a
/// `whoami` that succeeds tells you the key authenticates and nothing about
/// whether it can reach the memories.
pub async fn connect(args: ConnectArgs) -> anyhow::Result<SetupOutcome> {
    let pending_path = PendingAgent::path_beside(&args.config_path);
    let pending = PendingAgent::load(&pending_path)?;

    let cfg = Config {
        version: CONFIG_VERSION,
        context_id: pending.context_id.clone(),
        identity: Identity::Agent {
            did: pending.agent_did.clone(),
            private_key_multibase: pending.private_key_multibase.clone(),
            vta_did: pending.vta_did.clone(),
            mediator_did: pending.mediator_did.clone(),
            rest_url: pending.rest_url.clone(),
        },
        recall_limit: args.recall_limit,
    };

    let memories_found = verify(&cfg).await.with_context(|| {
        format!(
            "the grant does not appear to have been made yet.\n\nRun this where you hold VTA \
             admin:\n  {}\n\nthen try again",
            pending.grant_command()
        )
    })?;

    cfg.save(&args.config_path)
        .with_context(|| format!("writing {}", args.config_path.display()))?;

    // The key now lives in the config; leaving a second copy on disk is a
    // second thing to leak and a second thing to forget to revoke.
    if let Err(e) = std::fs::remove_file(&pending_path) {
        tracing::warn!(
            path = %pending_path.display(),
            error = %e,
            "could not remove the pending enrolment file; delete it by hand — it holds the agent key"
        );
    }

    Ok(SetupOutcome {
        config_path: args.config_path,
        vta_did: pending.vta_did,
        context_id: pending.context_id,
        identity_label: cfg.identity.label().to_string(),
        agent_did: Some(pending.agent_did),
        memories_found,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingAgent {
        PendingAgent {
            version: PENDING_VERSION,
            agent_did: "did:key:z6MkAgent".into(),
            private_key_multibase: "zSecret".into(),
            vta_did: "did:webvh:abc:example.com:vta".into(),
            context_id: "my-memories".into(),
            mediator_did: "did:web:mediator.example".into(),
            rest_url: None,
        }
    }

    #[test]
    fn the_grant_command_is_pasteable_and_least_privilege() {
        // Printed for a human to run elsewhere, so it has to be correct
        // verbatim — and it must never say `admin`, for which an empty context
        // list means unrestricted.
        let cmd = pending().grant_command();
        assert_eq!(
            cmd,
            "pnm acl create --did did:key:z6MkAgent --role application \
             --contexts my-memories --label vta-agent-memory"
        );
        assert!(!cmd.contains("admin"));
    }

    #[test]
    fn the_pending_file_sits_beside_the_config() {
        let p = PendingAgent::path_beside(Path::new("/x/y/config.json"));
        assert_eq!(p, PathBuf::from("/x/y/pending-agent.json"));
    }

    #[test]
    fn pending_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-agent.json");
        pending().save(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "it holds the agent private key");
        }

        let back = PendingAgent::load(&path).unwrap();
        assert_eq!(back.agent_did, "did:key:z6MkAgent");
        assert_eq!(back.context_id, "my-memories");
    }

    #[test]
    fn a_missing_pending_file_points_at_init() {
        let err = PendingAgent::load(Path::new("/nonexistent/pending-agent.json")).unwrap_err();
        assert!(err.to_string().contains("vta-agent-memory init"), "{err}");
    }

    #[test]
    fn a_newer_pending_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-agent.json");
        let mut p = pending();
        p.version = PENDING_VERSION + 1;
        p.save(&path).unwrap();
        assert!(
            PendingAgent::load(&path)
                .unwrap_err()
                .to_string()
                .contains("upgrade the binary")
        );
    }

    #[tokio::test]
    async fn init_refuses_to_silently_replace_a_pending_enrolment() {
        // Overwriting would strand an already-granted DID in the ACL with
        // nothing left on disk that names it.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        pending()
            .save(&PendingAgent::path_beside(&config_path))
            .unwrap();

        let err = init(InitArgs {
            vta_did: "did:webvh:abc:example.com:vta".into(),
            context_id: "my-memories".into(),
            mediator_did: Some("did:web:mediator.example".into()),
            url: None,
            config_path,
            force: false,
        })
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("already pending"), "{msg}");
        assert!(
            msg.contains("pnm acl create"),
            "it reprints the grant: {msg}"
        );
    }

    #[tokio::test]
    async fn init_needs_no_network_when_the_mediator_is_supplied() {
        // The property that makes phase 1 usable before anyone has granted
        // anything: no authenticated call, and no DID resolution either when
        // the endpoint is already known.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let p = init(InitArgs {
            vta_did: "did:key:zVta".into(),
            context_id: "ctx".into(),
            mediator_did: Some("did:key:zMed".into()),
            url: Some("https://vta.example".into()),
            config_path: config_path.clone(),
            force: false,
        })
        .await
        .unwrap();

        assert!(p.agent_did.starts_with("did:key:"));
        assert_eq!(p.mediator_did, "did:key:zMed");
        assert!(PendingAgent::path_beside(&config_path).exists());
        assert!(!config_path.exists(), "phase 1 writes no config");
    }
}
