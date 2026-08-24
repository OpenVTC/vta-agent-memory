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
/// Always a **session in a session store** — never a private key in this file.
/// The key lives in the OS keyring (or whichever backend the SDK selected), and
/// for a dedicated agent it is one the VTA has never seen in a paste-able form:
/// see `crate::enrol` for the rotation that makes that true.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// Service namespace the session is stored under.
    pub service_name: String,
    /// Key within that namespace.
    pub session_key: String,
    /// Directory the session backend was opened against. Stored rather than
    /// derived: the one bug this crate keeps meeting is a path that looked
    /// obvious and was wrong on somebody else's platform.
    pub sessions_dir: PathBuf,
    /// The VTA's DID — the identifier worth writing down, and the only one
    /// that means anything on another machine.
    pub vta_did: String,
    /// True when this reuses the **operator's own** `pnm` login rather than a
    /// dedicated, context-scoped agent identity. Convenient on a workstation,
    /// and far more reach than a memory service should have anywhere else.
    #[serde(default)]
    pub operator_login: bool,
}

impl Identity {
    /// The VTA these memories live in.
    pub fn vta_did(&self) -> &str {
        &self.vta_did
    }

    /// Short label for diagnostics.
    pub fn label(&self) -> &'static str {
        if self.operator_login {
            "pnm session (operator identity)"
        } else {
            "dedicated agent (rotated)"
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
    ///
    /// Always the session rung, and always with an explicit `sessions_dir` —
    /// the SDK's default was wrong on macOS and Windows until VTI #1087, and
    /// resolving it here means this crate never depended on that being right.
    pub fn to_agent_connect(&self) -> vta_sdk::agent_connect::AgentConnect {
        vta_sdk::agent_connect::AgentConnect {
            session_key: Some(self.identity.session_key.clone()),
            service_name: Some(self.identity.service_name.clone()),
            sessions_dir: Some(self.identity.sessions_dir.clone()),
            ..Default::default()
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
            identity: Identity {
                service_name: "vta-agent-memory".into(),
                session_key: "agent:my-project".into(),
                sessions_dir: PathBuf::from("/tmp/vam-sessions"),
                vta_did: "did:webvh:abc:example.com:vta".into(),
                operator_login: false,
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
        assert_eq!(back.identity.session_key, "agent:my-project");
        assert!(!back.identity.operator_login);
    }

    #[test]
    fn the_config_holds_no_key_material() {
        // The whole point of routing enrolment through the session store: the
        // key lives in the OS keyring, and this file only says which session to
        // open. A regression here would put a private key back in plain JSON.
        let json = serde_json::to_string(&agent_config()).unwrap();
        for forbidden in ["privateKey", "private_key", "multibase", "secret"] {
            assert!(!json.contains(forbidden), "`{forbidden}` in config: {json}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_config_is_owner_only() {
        // It no longer holds a key, but it names an identity and a VTA, and
        // there is no reason for that to be world-readable.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        agent_config().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
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
    fn every_identity_connects_through_the_session_rung() {
        use vta_sdk::agent_connect::ConnectMode;
        assert_eq!(
            agent_config().to_agent_connect().mode().unwrap(),
            ConnectMode::Session {
                key: "agent:my-project".into()
            }
        );
    }

    /// Session mode must always carry an explicit `sessions_dir`.
    ///
    /// `vta_sdk`'s default was `~/.config/pnm` until VTI #1087 — wrong on macOS
    /// and Windows. This crate resolves the directory itself and records it, so
    /// it never depended on that being right. Pinned, because a path left to a
    /// default would find no session on two platforms and report it as an
    /// authentication failure.
    #[test]
    fn the_sessions_dir_is_never_left_to_a_default() {
        let connect = agent_config().to_agent_connect();
        assert_eq!(
            connect.sessions_dir.expect("recorded, not derived"),
            PathBuf::from("/tmp/vam-sessions")
        );
    }

    #[test]
    fn an_operator_login_is_labelled_as_one() {
        // It is the higher-privilege choice, so diagnostics must not present it
        // as equivalent to a scoped agent.
        let mut cfg = agent_config();
        cfg.identity.operator_login = true;
        assert!(cfg.identity.label().contains("operator"));
        assert!(!agent_config().identity.label().contains("operator"));
    }

    #[test]
    fn recall_limit_defaults_when_absent() {
        let cfg: Config = serde_json::from_str(
            r#"{"version":1,"contextId":"c","identity":{"serviceName":"s","sessionKey":"k",
                "sessionsDir":"/tmp","vtaDid":"did:key:zV"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.recall_limit, 8);
        assert!(
            !cfg.identity.operator_login,
            "absent means the safer reading"
        );
    }
}
