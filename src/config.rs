//! On-disk configuration: what `setup` writes and every other subcommand reads.
//!
//! Written once by `vta-agent-memory setup`, so the MCP server, the hooks, and
//! the slash commands all reach the same VTA and the same trust context without
//! any of them carrying flags. The file is the reason a hook can be a bare
//! `vta-agent-memory recall` with no arguments.
//!
//! **It holds a private key** when setup mints a dedicated agent identity, so
//! it is written `0600` and nothing here ever logs a field it has not been
//! asked to. That is also why setup offers to reuse the operator's `pnm`
//! session instead: that path stores no key at all.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Config-file schema version.
pub const CONFIG_VERSION: u8 = 1;

/// How this machine authenticates to the VTA.
///
/// Mirrors the rungs of `vta_sdk::agent_connect::AgentConnect`, narrowed to the
/// two a setup flow can actually produce. The bearer-token rung is reachable
/// from the environment (`VTA_URL` + `VTA_TOKEN`) and deliberately has no
/// on-disk form — a token is short-lived and does not belong in a config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Identity {
    /// A dedicated agent `did:key`, scoped by its own ACL entry to one context.
    /// The recommended shape: revoking the agent is `pnm acl delete`, and it
    /// cannot reach anything the operator's own login can.
    #[serde(rename_all = "camelCase")]
    Agent {
        /// The agent's `did:key`.
        did: String,
        /// Its Ed25519 signing key, multibase. Never leaves this process.
        private_key_multibase: String,
        /// The VTA this agent authenticates to.
        vta_did: String,
        /// The mediator it routes through.
        mediator_did: String,
        /// REST fallback URL, when the VTA advertises one.
        #[serde(skip_serializing_if = "Option::is_none")]
        rest_url: Option<String>,
    },
    /// Reuse an existing `pnm` login from the keyring. Stores no key material —
    /// but authenticates as the *operator*, so the agent inherits the
    /// operator's whole reach.
    #[serde(rename_all = "camelCase")]
    PnmSession {
        /// The **keyring key**, `vta:<slug>` — not the bare slug. `pnm` stores
        /// sessions under that prefix and no session backend adds it, so the
        /// bare slug finds nothing and reports an authentication failure to
        /// somebody who is authenticated. Resolved once at setup and stored
        /// whole, so nothing downstream has to remember the rule.
        session_key: String,
        /// The VTA's DID. Not used to connect — the session carries that — but
        /// recorded so `doctor` can say *which* VTA these memories are in
        /// without the reader having to decode a local nickname.
        vta_did: String,
        /// The service name sessions were stored under.
        #[serde(skip_serializing_if = "Option::is_none")]
        service_name: Option<String>,
    },
}

impl Identity {
    /// The VTA these memories live in, whichever identity is configured.
    ///
    /// Both variants record it, so diagnostics can name the VTA by the
    /// identifier that means something everywhere rather than by a local
    /// nickname the reader may never have seen.
    pub fn vta_did(&self) -> &str {
        match self {
            Identity::Agent { vta_did, .. } => vta_did,
            Identity::PnmSession { vta_did, .. } => vta_did,
        }
    }

    /// Short label for diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            Identity::Agent { .. } => "dedicated agent did:key",
            Identity::PnmSession { .. } => "pnm session (operator identity)",
        }
    }
}

/// Write `contents` to `path`, creating parents, readable only by its owner.
///
/// The permissions are set on the file **before** anything goes into it, not
/// after: a create-then-chmod leaves a window where a freshly-minted agent key
/// is world-readable, and that window is exactly when the file is most
/// interesting. Shared with the pending-enrolment file, which holds the same
/// key before the config does.
pub fn write_owner_only(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents.as_bytes())?;
    }
    Ok(())
}

/// The whole config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Schema version.
    pub version: u8,
    /// The trust context memories live in. **This is the isolation boundary** —
    /// the VTA gates every `vta/memory/*` call on access to it, so a context-A
    /// agent cannot read context-B memory. One context per project (or per
    /// domain) is the intended shape.
    pub context_id: String,
    /// How to authenticate.
    pub identity: Identity,
    /// How many memories a bare `recall` returns. Kept small: a session-start
    /// hook that pastes fifty memories into the context window has made the
    /// session worse, not better.
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
}

fn default_recall_limit() -> usize {
    8
}

impl Config {
    /// `$VTA_AGENT_MEMORY_CONFIG`, else `~/.config/vta-agent-memory/config.json`.
    pub fn default_path() -> anyhow::Result<PathBuf> {
        if let Ok(p) = std::env::var("VTA_AGENT_MEMORY_CONFIG")
            && !p.is_empty()
        {
            return Ok(PathBuf::from(p));
        }
        let base = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("no config directory; set VTA_AGENT_MEMORY_CONFIG"))?;
        Ok(base.join("vta-agent-memory").join("config.json"))
    }

    /// Load from `path`, with an error that names the fix rather than the file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!(
                "no memory configuration at {} ({e}).\n\nRun:\n  vta-agent-memory setup --vta <did:… or pnm name>\n\n(or just `vta-agent-memory setup`, to use your default `pnm` VTA)",
                path.display()
            )
        })?;
        let cfg: Config = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        if cfg.version > CONFIG_VERSION {
            anyhow::bail!(
                "{} was written by a newer vta-agent-memory (config version {}, this build understands {CONFIG_VERSION}) — upgrade the binary",
                path.display(),
                cfg.version
            );
        }
        Ok(cfg)
    }

    /// Write to `path`, creating parents, owner-only. See [`write_owner_only`].
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_owner_only(path, &serde_json::to_string_pretty(self)?)
    }

    /// Turn the stored identity into the SDK's connect ladder.
    pub fn to_agent_connect(&self) -> vta_sdk::agent_connect::AgentConnect {
        use vta_sdk::agent_connect::AgentConnect;
        match &self.identity {
            Identity::Agent {
                did,
                private_key_multibase,
                vta_did,
                mediator_did,
                rest_url,
            } => AgentConnect {
                agent_did: Some(did.clone()),
                agent_key: Some(private_key_multibase.clone()),
                vta_did: Some(vta_did.clone()),
                mediator_did: Some(mediator_did.clone()),
                url: rest_url.clone(),
                ..Default::default()
            },
            Identity::PnmSession {
                session_key,
                service_name,
                ..
            } => AgentConnect {
                session_key: Some(session_key.clone()),
                service_name: service_name.clone(),
                // `pnm` keeps its sessions beside its config; the SDK's default
                // is the same path, but saying so keeps the two from drifting.
                sessions_dir: crate::pnm::PnmConfig::default_dir().ok(),
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_config() -> Config {
        Config {
            version: CONFIG_VERSION,
            context_id: "my-project".into(),
            identity: Identity::Agent {
                did: "did:key:zAgent".into(),
                private_key_multibase: "zSecret".into(),
                vta_did: "did:webvh:abc:example.com:vta".into(),
                mediator_did: "did:web:mediator.example".into(),
                rest_url: Some("https://vta.example".into()),
            },
            recall_limit: 8,
        }
    }

    #[test]
    fn config_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");
        agent_config().save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(back.context_id, "my-project");
        assert!(matches!(back.identity, Identity::Agent { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn the_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        agent_config().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "it holds an agent private key");
    }

    #[test]
    fn a_missing_config_points_at_setup() {
        let err = Config::load(Path::new("/nonexistent/vta-agent-memory.json")).unwrap_err();
        assert!(err.to_string().contains("vta-agent-memory setup"), "{err}");
    }

    #[test]
    fn a_newer_config_version_is_refused_rather_than_half_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = agent_config();
        cfg.version = CONFIG_VERSION + 1;
        cfg.save(&path).unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("upgrade the binary"), "{err}");
    }

    #[test]
    fn the_agent_identity_maps_onto_the_didkey_rung() {
        use vta_sdk::agent_connect::ConnectMode;
        let mode = agent_config().to_agent_connect().mode().unwrap();
        assert_eq!(
            mode,
            ConnectMode::DidKey {
                agent_did: "did:key:zAgent".into(),
                mediator_did: "did:web:mediator.example".into(),
            }
        );
    }

    #[test]
    fn the_session_identity_maps_onto_the_session_rung() {
        use vta_sdk::agent_connect::ConnectMode;
        let cfg = Config {
            identity: Identity::PnmSession {
                session_key: "vta:my-vta".into(),
                vta_did: "did:webvh:abc:example.com:vta".into(),
                service_name: None,
            },
            ..agent_config()
        };
        assert_eq!(
            cfg.to_agent_connect().mode().unwrap(),
            ConnectMode::Session {
                key: "vta:my-vta".into()
            }
        );
    }

    /// Session mode must always carry an explicit `sessions_dir`.
    ///
    /// `vta_sdk`'s default was `~/.config/pnm` until VTI #1087 — wrong on macOS
    /// and Windows, where `pnm` writes to `dirs::config_dir()`. This crate was
    /// never exposed to that because it resolves the directory itself, and this
    /// pins the invariant: a future session-mode path that leaves the field
    /// `None` would silently find no session on two platforms and report it as
    /// an authentication failure.
    #[test]
    fn session_mode_never_leaves_the_sessions_dir_to_a_default() {
        let cfg = Config {
            identity: Identity::PnmSession {
                session_key: "vta:my-vta".into(),
                vta_did: "did:key:zV".into(),
                service_name: None,
            },
            ..agent_config()
        };
        let connect = cfg.to_agent_connect();
        let dir = connect
            .sessions_dir
            .expect("session mode must resolve the directory itself");
        assert_eq!(dir.file_name().expect("pnm"), "pnm");
        assert_eq!(dir.parent().expect("parent"), dirs::config_dir().unwrap());
    }

    #[test]
    fn recall_limit_defaults_when_absent() {
        let cfg: Config = serde_json::from_str(
            r#"{"version":1,"contextId":"c","identity":{"kind":"pnmSession","sessionKey":"vta:v","vtaDid":"did:key:zV"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.recall_limit, 8);
    }
}
