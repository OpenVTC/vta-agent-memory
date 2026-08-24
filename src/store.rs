//! The memory store: typed operations over the VTA's `vta/memory/*` Trust
//! Tasks.
//!
//! Every method here is one Trust Task plus record encoding. There is no
//! transport code, no hand-rolled retry, and no caching, on purpose:
//!
//! - **Transport** is `VtaClient`'s. `memory_{put,list,delete}` go through the
//!   generic Trust-Task dispatcher — a real `TrustTask` document, schema-checked
//!   against the published registry before it goes out, carried on TSP, DIDComm
//!   or REST depending on what the two DID documents advertise. Nothing in this
//!   file knows which.
//! - **Retry** has exactly one owner per failure domain, and for operation
//!   completion that owner is [`VtaClient::idempotent`]. The two mutating
//!   methods run inside it (both tasks are classified `RetrySafe` in
//!   `vta_sdk::retry_safety`); a loop written *around* them here could not hold
//!   an idempotency key stable across attempts, so it would turn one retried
//!   operation into two operations.
//! - **Caching** would have to answer "stale by how long?" for a store the user
//!   can also write from `pnm`. Every read is a round trip.
//!
//! ## Responses are decoded into the published types
//!
//! Each task's request and response shape is generated from the upstream spec
//! (`trust_tasks_rs::specs::vta::memory::*`) and re-exported by the SDK as
//! [`MemoryListResponse`] and friends. This module deserializes into those
//! rather than walking `serde_json::Value` by hand.
//!
//! That is not tidiness. Reaching for `resp["items"]` and skipping anything that
//! does not look right means a renamed or re-cased field arrives as **an empty
//! list** — the user is told they have no memories, which is both wrong and
//! believable. Casing drift on wire types is the recurring defect class in this
//! stack; decoding into the typed shape is what makes it a loud error instead of
//! a quiet one.
//!
//! The tolerance that *is* deliberate sits one level down: an entry whose
//! **key** is not `<type>/<slug>` belongs to something else writing into the
//! same context and is skipped. A malformed envelope is a bug; an unfamiliar
//! neighbour is not.
//!
//! ## The recall cost
//!
//! `memory/list/0.1` returns the whole context and nothing narrower exists, so
//! [`Store::list`] fetches everything and the filtering happens here. That is a
//! real cost, and [`Store::list_of_type`] is the single place to change if the
//! upstream task ever grows a prefix or a cursor.

use anyhow::Context as _;
use vta_sdk::client::VtaClient;
use vta_sdk::protocols::memory::{MemoryDeleteResponse, MemoryListResponse, MemoryPutResponse};

use crate::record::{Entry, Hit, MemoryKey, MemoryRecord, MemoryType, rank};

/// A memory store bound to one VTA client and one trust context.
pub struct Store {
    client: VtaClient,
    context_id: String,
}

impl Store {
    /// Bind a connected client to a context.
    pub fn new(client: VtaClient, context_id: impl Into<String>) -> Self {
        Self {
            client,
            context_id: context_id.into(),
        }
    }

    /// The context every operation is scoped to.
    pub fn context_id(&self) -> &str {
        &self.context_id
    }

    /// The underlying client, for callers that need the VTA directly.
    pub fn client(&self) -> &VtaClient {
        &self.client
    }

    /// Close the transport. A DIDComm or TSP client holds a live mediator
    /// socket that `Drop` cannot close; leaving it open duels the mediator's
    /// one-socket-per-DID slot on reconnect. Idempotent, and a no-op on REST.
    pub async fn shutdown(self) {
        self.client.shutdown().await;
    }

    /// Upsert one memory. A second save of the same `(type, name)` replaces it.
    ///
    /// Runs under [`VtaClient::idempotent`], which retries transient faults with
    /// one stable idempotency key. `vta/memory/put/0.1` is classified
    /// `RetrySafe`, so a replay is a re-upsert of the same value — the point of
    /// the wrapper is that a websocket blip mid-session does not silently lose
    /// something the user asked to be remembered.
    pub async fn save(&self, record: &MemoryRecord) -> anyhow::Result<MemoryKey> {
        let key = MemoryKey::new(record.kind, &record.name)?;
        let key_str = key.to_string();
        let value = record.encode().context("encoding memory record")?;

        let resp = self
            .client
            .idempotent(|| self.client.memory_put(&self.context_id, &key_str, &value))
            .await
            .with_context(|| format!("saving memory `{key}` in context `{}`", self.context_id))?;

        let decoded: MemoryPutResponse = serde_json::from_value(resp)
            .with_context(|| format!("decoding the response to saving `{key}`"))?;
        // The VTA echoes the key it stored. A mismatch means the value landed
        // somewhere this store cannot address, which is worse than a failure —
        // recall would never find it and the user would think it saved.
        if decoded.key != key_str {
            anyhow::bail!(
                "the VTA stored memory `{}` when `{key_str}` was requested",
                decoded.key
            );
        }
        Ok(key)
    }

    /// Every memory in the context.
    ///
    /// This is a full read of the context on every call, because
    /// `vta/memory/list/0.1` returns "every entry in the context, in ascending
    /// key order" and takes no prefix and no cursor. Fine at the hundreds of
    /// entries a person accumulates; not a query engine.
    pub async fn list(&self) -> anyhow::Result<Vec<Entry>> {
        self.list_of_type(None).await
    }

    /// As [`list`](Self::list), narrowed to one type.
    ///
    /// The narrowing happens on the **raw key**, before the record's JSON is
    /// parsed. The wire cost is identical either way — the VTA sends the whole
    /// context regardless — but a `--type feedback` recall in a context of a
    /// thousand memories then decodes only the feedback ones. That is the
    /// entire practical payoff of putting the type in the key.
    ///
    /// Entries whose keys this tool did not write are skipped rather than
    /// erroring, so a context shared with another writer stays usable.
    pub async fn list_of_type(&self, kind: Option<MemoryType>) -> anyhow::Result<Vec<Entry>> {
        let resp = self
            .client
            .memory_list(&self.context_id)
            .await
            .with_context(|| format!("listing memories in context `{}`", self.context_id))?;

        // Decode into the published response type, not by hand. A renamed or
        // re-cased field must fail here; walking the JSON would turn it into an
        // empty list, and "you have no memories" is a wrong answer the user has
        // no way to distinguish from a right one.
        let listed: MemoryListResponse = serde_json::from_value(resp).with_context(|| {
            format!(
                "decoding the memory list for context `{}` — the VTA's response did not match \
                 vta/memory/list/0.1",
                self.context_id
            )
        })?;

        let mut entries = Vec::with_capacity(listed.items.len());
        for item in listed.items {
            if let Some(k) = kind
                && !MemoryKey::raw_key_has_type(&item.key, k)
            {
                continue;
            }
            // One level down from the envelope, tolerance is right: a key that
            // is not `<type>/<slug>` belongs to something else writing into the
            // same context, and skipping it keeps `list` total.
            let Ok(key) = MemoryKey::parse(&item.key) else {
                tracing::debug!(
                    key = %item.key,
                    "skipping entry with an unrecognised key shape"
                );
                continue;
            };
            let record = MemoryRecord::decode(&key, &item.value);
            entries.push(Entry { key, record });
        }
        Ok(entries)
    }

    /// Ranked recall: list, optionally narrow by type, rank against `query`,
    /// take `limit`.
    pub async fn recall(
        &self,
        query: &str,
        kind: Option<MemoryType>,
        limit: usize,
    ) -> anyhow::Result<Vec<Hit>> {
        let entries = self.list_of_type(kind).await?;
        let mut hits = rank(entries, query);
        // A non-empty query that matched nothing should say so rather than
        // hand back the `limit` least-irrelevant memories in the context —
        // a confident wrong answer is worse than an empty one.
        if !query.trim().is_empty() {
            hits.retain(|h| h.score > 0);
        }
        hits.truncate(limit);
        Ok(hits)
    }

    /// One memory by key, or `None` if absent.
    ///
    /// There is no `memory/get` Trust Task, so this is a `list` plus a lookup.
    /// Same cost as recall; stated here so the cost is not a surprise.
    pub async fn get(&self, key: &MemoryKey) -> anyhow::Result<Option<Entry>> {
        Ok(self.list().await?.into_iter().find(|e| &e.key == key))
    }

    /// Delete one memory. Absent keys surface the VTA's own `not found`.
    ///
    /// Also under [`VtaClient::idempotent`] — `vta/memory/delete/0.1` is
    /// `RetrySafe`, and `not found` is not a transient fault, so a replay after
    /// a delete that did land still reports the deletion rather than retrying
    /// into a spurious error.
    pub async fn forget(&self, key: &MemoryKey) -> anyhow::Result<()> {
        let key_str = key.to_string();
        let resp = self
            .client
            .idempotent(|| self.client.memory_delete(&self.context_id, &key_str))
            .await
            .with_context(|| {
                format!("forgetting memory `{key}` in context `{}`", self.context_id)
            })?;

        let decoded: MemoryDeleteResponse = serde_json::from_value(resp)
            .with_context(|| format!("decoding the response to forgetting `{key}`"))?;
        if decoded.key != key_str {
            anyhow::bail!(
                "the VTA deleted memory `{}` when `{key_str}` was requested",
                decoded.key
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::MemoryRecord;

    fn entry(kind: MemoryType, name: &str, desc: &str) -> Entry {
        Entry {
            key: MemoryKey::new(kind, name).unwrap(),
            record: MemoryRecord::new(kind, name, desc, "body", vec![]),
        }
    }

    /// The recall pipeline without a VTA: the filter → rank → drop-zeros →
    /// truncate sequence is where the behaviour lives, and it is worth pinning
    /// independently of the transport.
    fn recall_locally(
        entries: Vec<Entry>,
        query: &str,
        kind: Option<MemoryType>,
        limit: usize,
    ) -> Vec<Hit> {
        let entries = match kind {
            Some(k) => entries.into_iter().filter(|e| e.key.kind == k).collect(),
            None => entries,
        };
        let mut hits = rank(entries, query);
        if !query.trim().is_empty() {
            hits.retain(|h| h.score > 0);
        }
        hits.truncate(limit);
        hits
    }

    #[test]
    fn a_query_that_matches_nothing_returns_nothing() {
        let hits = recall_locally(
            vec![
                entry(MemoryType::Project, "Alpha", "one"),
                entry(MemoryType::Project, "Beta", "two"),
            ],
            "kubernetes",
            None,
            8,
        );
        assert!(hits.is_empty(), "no near-misses passed off as answers");
    }

    #[test]
    fn an_empty_query_returns_the_context_up_to_the_limit() {
        let hits = recall_locally(
            (0..20)
                .map(|i| entry(MemoryType::User, &format!("m{i}"), "d"))
                .collect(),
            "",
            None,
            8,
        );
        assert_eq!(hits.len(), 8);
    }

    #[test]
    fn the_type_filter_applies_before_ranking() {
        let hits = recall_locally(
            vec![
                entry(MemoryType::Project, "Gateway", "the gateway"),
                entry(MemoryType::Feedback, "Gateway rules", "about the gateway"),
            ],
            "gateway",
            Some(MemoryType::Feedback),
            8,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.key.kind, MemoryType::Feedback);
    }
}
