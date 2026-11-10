//! The memory store: typed operations over the VTA's `vta/memory/*` Trust
//! Tasks, at 0.3 (`put`, `delete`, `list`) and `get/0.2` — the versions that
//! carry `purpose`.
//!
//! Every method here is one Trust Task (or one paged sequence of them) plus
//! record encoding. There is no transport code, no hand-rolled retry, and no
//! caching, on purpose:
//!
//! - **Transport** is `VtaClient`'s. The memory methods go through the
//!   generic Trust-Task dispatcher — a real `TrustTask` document, schema-checked
//!   against the published registry before it goes out, carried on TSP, DIDComm
//!   or REST depending on what the two DID documents advertise. Nothing in this
//!   file knows which.
//! - **Retry** has exactly one owner per failure domain, and for operation
//!   completion that owner is [`VtaClient::idempotent`]. The two mutating
//!   methods run inside it; a loop written *around* them here could not hold an
//!   idempotency key stable across attempts, so it would turn one retried
//!   operation into two operations. That matters more at 0.2: `put/0.2` is
//!   classified `Keyed` in `vta_sdk::retry_safety`, because a replayed put takes
//!   a second version and a device syncing the context would see a change that
//!   never happened.
//! - **Caching** would have to answer "stale by how long?" for a store the user
//!   can also write from `pnm`. Every read is a round trip.
//!
//! ## Every read names its purpose
//!
//! The VTA holds every read of memory to the purposes the caller's ACL entry
//! grants, and records the purpose with the read (VTI-ACL-100 – 110). This
//! store names one on every call: [`DEFAULT_PURPOSE`], `personalisation` —
//! "adapt what it does for the principal to the principal: tone, preferences,
//! context carried between sessions", which is what an assistant recalling
//! the user's memories is doing — unless the config names another. A purpose
//! the entry does not grant is refused before anything is read or written, and
//! [`purpose_refused`] turns that refusal into the `pnm` command that grants it.
//!
//! ## Responses are decoded into the published types
//!
//! Each task's response is decoded into the type generated from its
//! specification (`vta_sdk::specs::vta::memory::*`) rather than walked as
//! `serde_json::Value` by hand. Reaching for `resp["items"]` and skipping
//! anything that does not look right means a renamed or re-cased field arrives
//! as **an empty list** — the user is told they have no memories, which is both
//! wrong and believable. Decoding into the typed shape makes it a loud error.
//!
//! The tolerance that *is* deliberate sits one level down: an entry whose
//! **key** is not `<type>/<slug>` belongs to something else writing into the
//! same context and is skipped. A malformed envelope is a bug; an unfamiliar
//! neighbour is not.
//!
//! ## The recall cost
//!
//! `list/0.2` takes a key prefix and pages by cursor, so a `--type` recall asks
//! the VTA for `<type>/` and receives only those; an untyped recall still reads
//! the whole context, a page at a time. One memory by key is a `get`.

use anyhow::Context as _;
use serde_json::{Map, Value, json};
use vta_sdk::client::VtaClient;
use vta_sdk::error::VtaError;
use vta_sdk::specs::vta::memory::{
    delete::v0_3 as delete_spec, get::v0_2 as get_spec, list::v0_3 as list_spec,
    put::v0_3 as put_spec,
};

use crate::record::{Entry, Hit, MemoryKey, MemoryRecord, MemoryType, rank};

/// Items per `list/0.3` page.
const PAGE: u64 = 500;

/// The purpose every memory operation names unless the config names another:
/// an assistant recalling the user's memories adapts what it does to them.
pub const DEFAULT_PURPOSE: &str = "personalisation";

/// A memory store bound to one VTA client and one trust context.
pub struct Store {
    client: VtaClient,
    context_id: String,
    purpose: String,
}

impl Store {
    /// Bind a connected client to a context, reading for [`DEFAULT_PURPOSE`].
    pub fn new(client: VtaClient, context_id: impl Into<String>) -> Self {
        Self {
            client,
            context_id: context_id.into(),
            purpose: DEFAULT_PURPOSE.to_string(),
        }
    }

    /// Name `purpose` on every operation instead of [`DEFAULT_PURPOSE`].
    pub fn with_purpose(mut self, purpose: impl Into<String>) -> Self {
        self.purpose = purpose.into();
        self
    }

    /// The purpose every operation names.
    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    /// The error for a failed call: a `purposeNotGranted` refusal becomes the
    /// command that fixes it, anything else keeps `doing` as its context.
    fn failed(&self, e: VtaError, doing: String) -> anyhow::Error {
        match purpose_refused(&e, &self.purpose) {
            Some(why) => anyhow::anyhow!(why),
            None => anyhow::Error::new(e).context(doing),
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
    /// Unconditional (`put/0.3` without `expectedVersion`): a save is the user
    /// asking for this to be remembered now, whatever was there. Runs under
    /// [`VtaClient::idempotent`], which retries transient faults with one stable
    /// idempotency key, so a websocket blip mid-session neither loses the save
    /// nor applies it twice.
    pub async fn save(&self, record: &MemoryRecord) -> anyhow::Result<MemoryKey> {
        let key = MemoryKey::new(record.kind, &record.name)?;
        let key_str = key.to_string();
        let value = record.encode().context("encoding memory record")?;

        let resp = self
            .client
            .idempotent(|| {
                self.client.memory_put_versioned_for(
                    &self.context_id,
                    &key_str,
                    &value,
                    None,
                    Some(&self.purpose),
                )
            })
            .await
            .map_err(|e| {
                self.failed(
                    e,
                    format!("saving memory `{key}` in context `{}`", self.context_id),
                )
            })?;

        let decoded: put_spec::Response = serde_json::from_value(resp)
            .with_context(|| format!("decoding the response to saving `{key}`"))?;
        // The VTA echoes the key it stored. A mismatch means the value landed
        // somewhere this store cannot address, which is worse than a failure —
        // recall would never find it and the user would think it saved.
        if decoded.key.as_str() != key_str {
            anyhow::bail!(
                "the VTA stored memory `{}` when `{key_str}` was requested",
                decoded.key.as_str()
            );
        }
        Ok(key)
    }

    /// Every memory in the context, a page at a time.
    pub async fn list(&self) -> anyhow::Result<Vec<Entry>> {
        self.list_of_type(None).await
    }

    /// As [`list`](Self::list), narrowed to one type — by the VTA, which is
    /// asked only for keys under `<type>/`.
    ///
    /// Entries whose keys this tool did not write are skipped rather than
    /// erroring, so a context shared with another writer stays usable.
    pub async fn list_of_type(&self, kind: Option<MemoryType>) -> anyhow::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = Map::new();
            query.insert("limit".into(), json!(PAGE));
            if let Some(k) = kind {
                query.insert("prefix".into(), json!(MemoryKey::prefix(k)));
            }
            if let Some(c) = &cursor {
                query.insert("cursor".into(), json!(c));
            }
            let resp = self
                .client
                .memory_list_page_for(&self.context_id, query, Some(&self.purpose))
                .await
                .map_err(|e| {
                    self.failed(
                        e,
                        format!("listing memories in context `{}`", self.context_id),
                    )
                })?;
            let (page, next) = decode_page(resp).with_context(|| {
                format!(
                    "decoding the memory list for context `{}` — the VTA's response did not \
                     match vta/memory/list/0.3",
                    self.context_id
                )
            })?;
            entries.extend(page);
            match next {
                Some(n) => cursor = Some(n),
                None => break,
            }
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

    /// One memory by key, or `None` if absent — one `vta/memory/get/0.2`.
    pub async fn get(&self, key: &MemoryKey) -> anyhow::Result<Option<Entry>> {
        let key_str = key.to_string();
        let resp = self
            .client
            .memory_get_for(&self.context_id, &[key_str.as_str()], Some(&self.purpose))
            .await
            .map_err(|e| {
                self.failed(
                    e,
                    format!("reading memory `{key}` in context `{}`", self.context_id),
                )
            })?;
        let got: get_spec::Response = serde_json::from_value(resp)
            .with_context(|| format!("decoding the response to reading `{key}`"))?;
        Ok(got.items.into_iter().find_map(|item| {
            let value = item.value.filter(|_| !item.deleted)?;
            (item.key.as_str() == key_str).then(|| Entry {
                key: key.clone(),
                record: MemoryRecord::decode(key, value.as_str()),
            })
        }))
    }

    /// Delete one memory. An absent key is an error, as it always has been.
    ///
    /// `delete/0.3` leaves a tombstone, so the user's other devices learn of the
    /// deletion, and answers a key holding nothing with `existed: false` rather
    /// than an error. This store keeps refusing that case: "forget X" for an X
    /// that is not there is more likely a mistyped name than a success. Under
    /// [`VtaClient::idempotent`], whose idempotency key stays the same across
    /// attempts, so a retry of a delete that did land is answered with that
    /// delete's own response (`existed: true`), not a spurious "not found".
    pub async fn forget(&self, key: &MemoryKey) -> anyhow::Result<()> {
        let key_str = key.to_string();
        let resp = self
            .client
            .idempotent(|| {
                self.client.memory_delete_versioned_for(
                    &self.context_id,
                    &key_str,
                    None,
                    Some(&self.purpose),
                )
            })
            .await
            .map_err(|e| {
                self.failed(
                    e,
                    format!("forgetting memory `{key}` in context `{}`", self.context_id),
                )
            })?;

        let decoded: delete_spec::Response = serde_json::from_value(resp)
            .with_context(|| format!("decoding the response to forgetting `{key}`"))?;
        if decoded.key.as_str() != key_str {
            anyhow::bail!(
                "the VTA deleted memory `{}` when `{key_str}` was requested",
                decoded.key.as_str()
            );
        }
        if !decoded.existed {
            anyhow::bail!(
                "memory `{key}` not found in context `{}` — nothing to forget",
                self.context_id
            );
        }
        Ok(())
    }
}

/// The explanation for a refusal of `purpose` (`vta/memory/*:purposeNotGranted`),
/// naming the `pnm` command that grants it; `None` for any other error.
///
/// The VTA refuses a purpose the caller's ACL entry does not grant, or — at the
/// version this store does not send — none named where the entry lists more
/// than one (VTI-ACL-103, VTI-ACL-104). Nothing was read or written.
pub fn purpose_refused(e: &VtaError, purpose: &str) -> Option<String> {
    let VtaError::Rejected { code, .. } = e else {
        return None;
    };
    if !(code.starts_with("vta/memory/") && code.ends_with(":purposeNotGranted")) {
        return None;
    }
    Some(format!(
        "the VTA refused this memory operation: this agent's access entry does not grant the \
         purpose `{purpose}`, which every memory read names. Nothing was read or written.\n\n\
         Either grant it (keeping any purposes the entry already lists):\n  \
         pnm acl update <this agent's DID> --purposes {purpose}[,<others>]\n\n\
         or name a purpose the entry does grant, as `purpose` in the memory config \
         (`vta-agent-memory doctor` shows the agent's DID and the config path)."
    ))
}

/// One `list/0.3` page: its live entries this tool can read, and the cursor
/// for the next page.
fn decode_page(resp: Value) -> anyhow::Result<(Vec<Entry>, Option<String>)> {
    let page: list_spec::Response = serde_json::from_value(resp)?;
    let mut entries = Vec::with_capacity(page.items.len());
    for item in page.items {
        if item.deleted {
            continue;
        }
        // Values are asked for (the default), so a live item without one is a
        // malformed response, not an entry to skip.
        let Some(value) = item.value else {
            anyhow::bail!(
                "live memory `{}` came back without its value",
                item.key.as_str()
            );
        };
        // One level down from the envelope, tolerance is right: a key that is
        // not `<type>/<slug>` belongs to something else writing into the same
        // context, and skipping it keeps `list` total.
        let Ok(key) = MemoryKey::parse(item.key.as_str()) else {
            tracing::debug!(key = %item.key.as_str(), "skipping entry with an unrecognised key shape");
            continue;
        };
        let record = MemoryRecord::decode(&key, value.as_str());
        entries.push(Entry { key, record });
    }
    Ok((entries, page.next_cursor))
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
