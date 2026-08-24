//! Connecting to the VTA on first use, not at launch.
//!
//! Claude Code starts an MCP server when a session starts and takes what it
//! gets. A server that exits before speaking MCP does not appear as a broken
//! memory service — it does not appear at all: the six tools are simply absent,
//! and the model has no way to tell anyone why, because the thing it would use
//! to say so is the thing that is missing.
//!
//! That is what connecting in `main` produced. An unreachable VTA, a laptop on
//! a train, a revoked ACL grant, or a machine where `setup` was never run all
//! ended the process with a message on stderr that nobody reads.
//!
//! So the server starts unconditionally and this type defers the work:
//!
//! - **The config is loaded on demand**, so `setup` can be run *during* a
//!   session and the next tool call picks it up. No restart.
//! - **The connection is made on demand**, and — because
//!   [`OnceCell::get_or_try_init`] does not cache errors — a failure is
//!   retried on the next call rather than latched for the life of the session.
//!   A VTA that comes back stops being a problem without anyone intervening.
//! - **Failures reach the model as tool errors**, which it can relay: "your
//!   memory service can't reach the VTA" is a useful thing to be told, and
//!   "run `vta-agent-memory setup`" is an actionable one.
//!
//! The cost is that the first memory call in a session pays the DIDComm
//! handshake. That is the right place for it — a session that never touches
//! memory now never opens a mediator socket at all.

use tokio::sync::OnceCell;

use crate::config::Config;
use crate::store::Store;

/// A store that connects the first time it is actually needed.
pub struct LazyStore {
    config_path: std::path::PathBuf,
    config: OnceCell<Config>,
    store: OnceCell<Store>,
}

impl LazyStore {
    /// Wrap a config path. Does no I/O — deliberately: the whole point is that
    /// construction cannot fail.
    pub fn new(config_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            config: OnceCell::new(),
            store: OnceCell::new(),
        }
    }

    /// The loaded config. Cheap — a local file read, no network.
    ///
    /// Kept separate from [`store`](Self::store) so diagnostics can answer
    /// "which VTA and which context?" while the VTA itself is unreachable.
    /// That is precisely when someone asks.
    pub async fn config(&self) -> anyhow::Result<&Config> {
        self.config
            .get_or_try_init(|| async { Config::load(&self.config_path) })
            .await
    }

    /// The connected store, connecting on first use.
    ///
    /// Errors are **not** cached: `get_or_try_init` only stores a success, so a
    /// transient failure is retried on the next call.
    pub async fn store(&self) -> anyhow::Result<&Store> {
        self.store
            .get_or_try_init(|| async {
                let cfg = self.config().await?.clone();
                let context_id = cfg.context_id.clone();

                // The connect future is **not `Send`**: the session rung goes
                // through `SessionStore::connect_with_transport`, which returns
                // `Box<dyn Error>` and holds one across an await. An MCP tool
                // handler requires a `Send` future, so awaiting it directly
                // here does not compile.
                //
                // Driving it on a blocking-pool thread with `block_on` is the
                // fix that does not require changing the SDK: the future never
                // migrates between threads, so `Send` is not needed, and the
                // `VtaClient` it produces *is* `Send + Sync` (vta-mcp already
                // shares one across an rmcp handler). `block_on` from the
                // blocking pool is supported — what is forbidden is blocking
                // inside an async task, which this deliberately is not.
                let client = tokio::task::spawn_blocking(move || {
                    tokio::runtime::Handle::current().block_on(cfg.to_agent_connect().connect())
                })
                .await
                .map_err(|e| anyhow::anyhow!("the VTA connection task failed: {e}"))??;

                Ok::<_, anyhow::Error>(Store::new(client, context_id))
            })
            .await
    }

    /// Whether a connection has actually been established.
    pub fn is_connected(&self) -> bool {
        self.store.initialized()
    }

    /// Close the transport if one was ever opened.
    ///
    /// A DIDComm or TSP client owns a live, auto-reconnecting mediator socket
    /// that `Drop` cannot close, and the mediator permits one per DID — so a
    /// leaked client duels the next connection for that slot. A session that
    /// never touched memory has nothing to close, which is the other half of
    /// why connecting lazily is worth it.
    pub async fn shutdown(&self) {
        if let Some(store) = self.store.get() {
            store.client().shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing() -> LazyStore {
        LazyStore::new("/nonexistent/vta-agent-memory/config.json")
    }

    #[tokio::test]
    async fn construction_never_fails_and_never_connects() {
        // The property the whole module exists for: whatever the state of the
        // config or the network, there is always a server to serve tools.
        let lazy = missing();
        assert!(!lazy.is_connected());
    }

    #[tokio::test]
    async fn a_missing_config_surfaces_as_an_error_the_model_can_relay() {
        // `Store` is not `Debug`, so `unwrap_err` is unavailable — match.
        let Err(err) = missing().store().await else {
            panic!("a missing config cannot produce a connected store");
        };
        assert!(
            format!("{err:#}").contains("vta-agent-memory setup"),
            "the error must name the fix, since it reaches a person via the model: {err:#}"
        );
    }

    #[tokio::test]
    async fn failures_are_retried_rather_than_latched() {
        // `get_or_try_init` caches successes only. A VTA that comes back must
        // stop being a problem without restarting the session.
        let lazy = missing();
        assert!(lazy.store().await.is_err());
        assert!(!lazy.is_connected(), "a failed attempt must not be cached");
        assert!(lazy.store().await.is_err(), "and it is tried again");
        assert!(!lazy.is_connected());
    }

    #[tokio::test]
    async fn shutting_down_an_unconnected_store_is_a_no_op() {
        missing().shutdown().await;
    }

    #[tokio::test]
    async fn config_is_readable_without_a_connection() {
        // Diagnostics have to work while the VTA is unreachable — that is when
        // they are asked for.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"version":1,"contextId":"proj",
                "identity":{"kind":"pnmSession","sessionKey":"vta:x","vtaDid":"did:key:zV"}}"#,
        )
        .unwrap();

        let lazy = LazyStore::new(&path);
        let cfg = lazy.config().await.unwrap();
        assert_eq!(cfg.context_id, "proj");
        assert_eq!(cfg.identity.vta_did(), "did:key:zV");
        assert!(!lazy.is_connected(), "reading config must not connect");
    }
}
