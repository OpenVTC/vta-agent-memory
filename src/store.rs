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
//! ## Every record is signed, and checked on the way back
//!
//! [`save`](Store::save) signs the record with this process's own key, bound to
//! the `(contextId, key)` it is written to; every read checks that signature
//! before the record is returned as memory. A record that fails is returned as
//! a [`Refused`] — its key and the reason — never as an [`Entry`]. Unsigned
//! records from before signing existed come back marked unattributed. See
//! [`crate::authorship`].
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

use crate::authorship::{Refused, Signer, verify};
use crate::lifecycle::{self, Standing, Status};
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
    /// The key saves are signed with. A store without one can read but not
    /// save: an unsigned save would be indistinguishable from a forged one.
    signer: Option<Signer>,
}

/// What a listing found: the entries returned as memory, and the ones refused.
#[derive(Debug, Default)]
pub struct Listing {
    /// Entries whose authorship checked out (or that predate signing).
    pub entries: Vec<Entry>,
    /// Entries that are not returned as memory, with why.
    pub refused: Vec<Refused>,
}

/// What a recall found: ranked hits, and every entry in scope that was refused.
#[derive(Debug, Default)]
pub struct Recall {
    /// The ranked matches, best first, at most the limit asked for.
    pub hits: Vec<Hit>,
    /// Entries in the recalled scope that were refused, whether or not they
    /// would have matched — a refused record is worth knowing about either way.
    pub refused: Vec<Refused>,
}

impl Store {
    /// Bind a connected client to a context, reading for [`DEFAULT_PURPOSE`].
    pub fn new(client: VtaClient, context_id: impl Into<String>) -> Self {
        Self {
            client,
            context_id: context_id.into(),
            purpose: DEFAULT_PURPOSE.to_string(),
            signer: None,
        }
    }

    /// Sign every save with `signer`.
    pub fn with_signer(mut self, signer: Signer) -> Self {
        self.signer = Some(signer);
        self
    }

    /// The key saves are signed with, if any.
    pub fn signer(&self) -> Option<&Signer> {
        self.signer.as_ref()
    }

    /// The DID this store signs as, to tell its own records apart.
    fn own_did(&self) -> Option<&str> {
        self.signer.as_ref().map(Signer::did)
    }

    /// Check one stored value found at `key`.
    async fn check(&self, key: MemoryKey, value: &str, version: u64) -> Result<Entry, Refused> {
        let key_str = key.to_string();
        match verify(value, &self.context_id, &key_str, self.own_did()).await {
            Ok(authorship) => {
                let record = MemoryRecord::decode(&key, value);
                Ok(Entry {
                    standing: Standing::of(&record, chrono::Utc::now()),
                    record,
                    key,
                    authorship,
                    version,
                })
            }
            Err(reason) => Err(Refused {
                key: key_str,
                reason,
            }),
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

    /// Save one memory, replacing any earlier version under the same
    /// `(type, name)`. Shorthand for [`save_versioned`](Self::save_versioned)
    /// with no expected version, where a lost race is an error.
    pub async fn save(&self, record: &MemoryRecord) -> anyhow::Result<MemoryKey> {
        match self.save_versioned(record, None).await? {
            SaveOutcome::Saved { key, .. } => Ok(key),
            SaveOutcome::Conflict { key, conflict } => anyhow::bail!(
                "memory `{key}` was changed by another writer while saving ({}); nothing was \
                 overwritten",
                conflict.reason
            ),
        }
    }

    /// Save one memory without losing what it replaces or what somebody else
    /// wrote meanwhile.
    ///
    /// 1. The current version, if any, is read.
    /// 2. If the caller says which version it last read (`expected`; 0 for "I
    ///    expect none") and that is not what is there now, nothing is written:
    ///    the outcome is a [`Conflict`] carrying what is there.
    /// 3. The current version is copied, verbatim and still signed by its own
    ///    author, to its history key, and the new record names it in
    ///    `supersedes`.
    /// 4. The new record is signed and written **conditionally** on the version
    ///    read in step 1 — so a writer that got in between steps 1 and 4 wins,
    ///    and this save comes back as a [`Conflict`] naming that winner rather
    ///    than silently overwriting it.
    /// 5. History beyond [`lifecycle::MAX_HISTORY`] versions is pruned.
    ///
    /// Every write runs under [`VtaClient::idempotent`], so a transient fault
    /// retries with one stable idempotency key and cannot apply twice.
    pub async fn save_versioned(
        &self,
        record: &MemoryRecord,
        expected: Option<u64>,
    ) -> anyhow::Result<SaveOutcome> {
        let key = MemoryKey::new(record.kind, &record.name)?;
        let key_str = key.to_string();
        let signer = self.signer.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "cannot save `{key}`: this memory store has no signing key, and memory records \
                 are signed by their writer"
            )
        })?;

        let current = self.get_raw(&key_str).await?;
        let current_version = current.as_ref().map(|(_, v)| *v).unwrap_or(0);
        if let Some(exp) = expected
            && exp != current_version
        {
            let current = match current {
                Some((value, version)) => Some(self.check(key.clone(), &value, version).await),
                None => None,
            };
            return Ok(SaveOutcome::Conflict {
                key,
                conflict: Box::new(Conflict {
                    reason: format!(
                        "expected version {exp}, but it is at {}",
                        if current_version == 0 {
                            "no version (it does not exist)".to_string()
                        } else {
                            current_version.to_string()
                        }
                    ),
                    current_version: (current_version > 0).then_some(current_version),
                    current,
                }),
            });
        }

        let mut record = record.clone();
        if let Some((value, version)) = &current {
            let archived = lifecycle::archive_key(&key, *version);
            // Create-only: a history version is immutable, and one already
            // there (an earlier attempt of this save got this far) is the same
            // bytes.
            match self.put(&archived, value, Some(0)).await? {
                Ok(_) => {}
                Err(c) if c.reason == "recordExists" => {}
                Err(c) => anyhow::bail!("archiving `{key}` v{version}: {}", c.reason),
            }
            record.supersedes = Some(json!({ "key": archived, "version": version }));
        }
        let value = signer.sign(&record, &self.context_id, &key).await?;

        match self.put(&key_str, &value, Some(current_version)).await? {
            Ok(version) => {
                if current.is_some()
                    && let Err(e) = self.prune_history(&key).await
                {
                    // The save landed; old history outliving its bound is not
                    // worth failing it over.
                    tracing::warn!(key = %key, error = %format!("{e:#}"), "could not prune memory history");
                }
                Ok(SaveOutcome::Saved {
                    key,
                    version,
                    superseded: current.map(|(_, v)| v),
                })
            }
            Err(raw) => {
                let current = match (raw.current_value, raw.current_version) {
                    (Some(value), Some(version)) => {
                        Some(self.check(key.clone(), &value, version).await)
                    }
                    _ => None,
                };
                Ok(SaveOutcome::Conflict {
                    key,
                    conflict: Box::new(Conflict {
                        reason: format!("another writer saved it first ({})", raw.reason),
                        current_version: raw.current_version,
                        current,
                    }),
                })
            }
        }
    }

    /// Change a memory's status, as a new version signed by this store.
    ///
    /// `deprecated` and `disputed` keep the memory's content and say why.
    /// `retracted` is a tombstone: the content is withdrawn — the new version
    /// keeps the name and type and says why, and the content stays only in the
    /// history the save writes — so it never recalls again. A refused record
    /// (one whose signature does not hold) can be retracted but nothing else,
    /// since anything else would re-sign content nobody vouches for. Like a
    /// save, conditional on `expected` if given.
    pub async fn set_status(
        &self,
        key: &MemoryKey,
        status: Status,
        reason: &str,
        expected: Option<u64>,
    ) -> anyhow::Result<SaveOutcome> {
        lifecycle::check_reason(reason).map_err(|e| anyhow::anyhow!(e))?;
        let key_str = key.to_string();
        let Some((value, version)) = self.get_raw(&key_str).await? else {
            anyhow::bail!("memory `{key}` not found in context `{}`", self.context_id);
        };
        let checked = self.check(key.clone(), &value, version).await;
        let mut record = match (status, checked) {
            (Status::Retracted, current) => {
                let name = current
                    .as_ref()
                    .map(|e| e.record.name.clone())
                    .unwrap_or_else(|_| key.slug.clone());
                MemoryRecord::new(key.kind, name, format!("Retracted: {reason}"), "", vec![])
            }
            (_, Ok(entry)) => {
                let mut r = entry.record;
                r.updated_at = chrono::Utc::now().to_rfc3339();
                r
            }
            (_, Err(refused)) => anyhow::bail!(
                "memory `{key}` is refused ({}), so it can only be retracted or forgotten",
                refused.reason
            ),
        };
        // The record's own name must address the same key, or the save would
        // land elsewhere. A foreign value's name is its slug, which does.
        if MemoryKey::new(record.kind, &record.name).ok().as_ref() != Some(key) {
            record.name = key.slug.clone();
        }
        record.status = (status != Status::Active).then(|| status.as_str().to_string());
        record.status_reason = Some(reason.to_string());
        self.save_versioned(&record, Some(expected.unwrap_or(version)))
            .await
    }

    /// Earlier versions of `key`, newest first, each checked against the key
    /// it was written to.
    pub async fn history(&self, key: &MemoryKey) -> anyhow::Result<Vec<Entry>> {
        let key_str = key.to_string();
        let mut out = Vec::new();
        for (raw, value, _) in self.raw_list(&lifecycle::archive_prefix(key)).await? {
            let Some(version) = lifecycle::archived_version(key, &raw) else {
                continue;
            };
            // Verified against the key it was *written* to: a history copy is
            // the original bytes, signature and all.
            match verify(&value, &self.context_id, &key_str, self.own_did()).await {
                Ok(authorship) => {
                    let record = MemoryRecord::decode(key, &value);
                    out.push(Entry {
                        standing: Standing::of(&record, chrono::Utc::now()),
                        key: key.clone(),
                        record,
                        authorship,
                        version,
                    })
                }
                Err(reason) => {
                    tracing::warn!(key = %raw, reason = %reason, "refusing archived memory version")
                }
            }
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.version));
        Ok(out)
    }

    /// Delete history beyond [`lifecycle::MAX_HISTORY`] versions.
    async fn prune_history(&self, key: &MemoryKey) -> anyhow::Result<()> {
        let mut versions: Vec<(u64, String)> = self
            .raw_list(&lifecycle::archive_prefix(key))
            .await?
            .into_iter()
            .filter_map(|(raw, _, _)| lifecycle::archived_version(key, &raw).map(|v| (v, raw)))
            .collect();
        versions.sort_by_key(|v| std::cmp::Reverse(v.0));
        for (_, raw) in versions.into_iter().skip(lifecycle::MAX_HISTORY) {
            self.delete(&raw).await?;
        }
        Ok(())
    }

    /// One `put/0.3`: `Ok(Ok(version))` written, `Ok(Err(conflict))` the
    /// precondition failed, `Err` anything else.
    async fn put(
        &self,
        key: &str,
        value: &str,
        expected: Option<u64>,
    ) -> anyhow::Result<Result<u64, RawConflict>> {
        let resp = self
            .client
            .idempotent(|| {
                self.client.memory_put_versioned_for(
                    &self.context_id,
                    key,
                    value,
                    expected,
                    Some(&self.purpose),
                )
            })
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                if let Some(c) = RawConflict::from_error(&e) {
                    return Ok(Err(c));
                }
                return Err(self.failed(
                    e,
                    format!("saving memory `{key}` in context `{}`", self.context_id),
                ));
            }
        };
        let decoded: put_spec::Response = serde_json::from_value(resp)
            .with_context(|| format!("decoding the response to saving `{key}`"))?;
        // The VTA echoes the key it stored. A mismatch means the value landed
        // somewhere this store cannot address, which is worse than a failure —
        // recall would never find it and the user would think it saved.
        if decoded.key.as_str() != key {
            anyhow::bail!(
                "the VTA stored memory `{}` when `{key}` was requested",
                decoded.key.as_str()
            );
        }
        Ok(Ok(decoded.version.0.get()))
    }

    /// One unconditional `delete/0.3`, returning whether anything was there.
    async fn delete(&self, key: &str) -> anyhow::Result<bool> {
        let resp = self
            .client
            .idempotent(|| {
                self.client.memory_delete_versioned_for(
                    &self.context_id,
                    key,
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
        if decoded.key.as_str() != key {
            anyhow::bail!(
                "the VTA deleted memory `{}` when `{key}` was requested",
                decoded.key.as_str()
            );
        }
        Ok(decoded.existed)
    }

    /// Every memory in the context, a page at a time.
    pub async fn list(&self) -> anyhow::Result<Listing> {
        self.list_of_type(None).await
    }

    /// As [`list`](Self::list), narrowed to one type — by the VTA, which is
    /// asked only for keys under `<type>/`.
    ///
    /// Entries whose keys this tool did not write are skipped rather than
    /// erroring, so a context shared with another writer stays usable. That
    /// includes history keys (`archive/…`), which are reached through
    /// [`history`](Self::history).
    pub async fn list_of_type(&self, kind: Option<MemoryType>) -> anyhow::Result<Listing> {
        let prefix = kind.map(MemoryKey::prefix).unwrap_or_default();
        let mut listing = Listing::default();
        for (raw, value, version) in self.raw_list(&prefix).await? {
            // One level down from the envelope, tolerance is right: a key that
            // is not `<type>/<slug>` belongs to something else writing into the
            // same context, and skipping it keeps `list` total.
            let Ok(key) = MemoryKey::parse(&raw) else {
                tracing::debug!(key = %raw, "skipping entry with an unrecognised key shape");
                continue;
            };
            match self.check(key, &value, version).await {
                Ok(entry) => listing.entries.push(entry),
                Err(refused) => {
                    tracing::warn!(key = %refused.key, reason = %refused.reason, "refusing stored memory");
                    listing.refused.push(refused);
                }
            }
        }
        Ok(listing)
    }

    /// Every live `(key, value, version)` under `prefix`, a page at a time.
    async fn raw_list(&self, prefix: &str) -> anyhow::Result<Vec<(String, String, u64)>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = Map::new();
            query.insert("limit".into(), json!(PAGE));
            if !prefix.is_empty() {
                query.insert("prefix".into(), json!(prefix));
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
            out.extend(page);
            match next {
                Some(n) => cursor = Some(n),
                None => break,
            }
        }
        Ok(out)
    }

    /// Ranked recall: list, optionally narrow by type, leave out what is
    /// retracted or expired, rank against `query`, take `limit`.
    pub async fn recall(
        &self,
        query: &str,
        kind: Option<MemoryType>,
        limit: usize,
    ) -> anyhow::Result<Recall> {
        let Listing { entries, refused } = self.list_of_type(kind).await?;
        let entries = entries
            .into_iter()
            .filter(|e| e.standing.recallable())
            .collect();
        let mut hits = rank(entries, query);
        // A non-empty query that matched nothing should say so rather than
        // hand back the `limit` least-irrelevant memories in the context —
        // a confident wrong answer is worse than an empty one.
        if !query.trim().is_empty() {
            hits.retain(|h| h.score > 0);
        }
        hits.truncate(limit);
        Ok(Recall { hits, refused })
    }

    /// One memory by key, or `None` if absent — one `vta/memory/get/0.2`.
    ///
    /// A record that fails its authorship check is an error naming why, not
    /// the record: it is not returned as memory. A retracted or expired memory
    /// *is* returned — asked for by key, it says what it is.
    pub async fn get(&self, key: &MemoryKey) -> anyhow::Result<Option<Entry>> {
        let Some((value, version)) = self.get_raw(&key.to_string()).await? else {
            return Ok(None);
        };
        match self.check(key.clone(), &value, version).await {
            Ok(entry) => Ok(Some(entry)),
            Err(refused) => anyhow::bail!(
                "memory `{key}` is stored but refused: {}. It is not returned as memory; \
                 `memory_forget` removes it",
                refused.reason
            ),
        }
    }

    /// The live value and version at `key`, unchecked.
    async fn get_raw(&self, key: &str) -> anyhow::Result<Option<(String, u64)>> {
        let resp = self
            .client
            .memory_get_for(&self.context_id, &[key], Some(&self.purpose))
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
            (item.key.as_str() == key).then(|| (value.as_str().to_string(), item.version.0.get()))
        }))
    }

    /// Delete one memory and its history. An absent key is an error, as it
    /// always has been.
    ///
    /// The hard-delete path: unlike retracting, nothing is kept — the history
    /// a save leaves behind goes too, because "forget" has to mean forget.
    /// `delete/0.3` leaves a VTA-level tombstone (no content), so the user's
    /// other devices learn of the deletion. It answers a key holding nothing
    /// with `existed: false`; this store refuses that case, since "forget X"
    /// for an X that is not there is more likely a mistyped name than a
    /// success. Under [`VtaClient::idempotent`], a retry of a delete that did
    /// land is answered with that delete's own response.
    pub async fn forget(&self, key: &MemoryKey) -> anyhow::Result<()> {
        let existed = self.delete(&key.to_string()).await?;
        let history = self.raw_list(&lifecycle::archive_prefix(key)).await?;
        for (raw, _, _) in &history {
            self.delete(raw).await?;
        }
        if !existed && history.is_empty() {
            anyhow::bail!(
                "memory `{key}` not found in context `{}` — nothing to forget",
                self.context_id
            );
        }
        Ok(())
    }
}

/// What a save did.
#[derive(Debug)]
pub enum SaveOutcome {
    /// Written.
    Saved {
        key: MemoryKey,
        /// The version it took — what the next update passes as expected.
        version: u64,
        /// The version it replaced, now in history; `None` for a new memory.
        superseded: Option<u64>,
    },
    /// Not written: somebody else's version is there.
    Conflict {
        key: MemoryKey,
        conflict: Box<Conflict>,
    },
}

/// A save that lost, and what won.
#[derive(Debug)]
pub struct Conflict {
    /// Why, in words.
    pub reason: String,
    /// The version there now, if any.
    pub current_version: Option<u64>,
    /// What is there now — checked like any read, so a forged winner is a
    /// [`Refused`], not an [`Entry`].
    pub current: Option<Result<Entry, Refused>>,
}

/// A `vta/memory/put:versionConflict`, as the VTA sent it.
#[derive(Debug)]
struct RawConflict {
    reason: String,
    current_version: Option<u64>,
    current_value: Option<String>,
}

impl RawConflict {
    fn from_error(e: &VtaError) -> Option<Self> {
        let VtaError::Rejected { code, details, .. } = e else {
            return None;
        };
        if !(code.starts_with("vta/memory/put") && code.ends_with(":versionConflict")) {
            return None;
        }
        let d = details.clone().unwrap_or_default();
        let deleted = d["currentDeleted"].as_bool().unwrap_or(false);
        Some(Self {
            reason: d["reason"]
                .as_str()
                .unwrap_or("versionConflict")
                .to_string(),
            current_version: d["currentVersion"].as_u64(),
            current_value: d["currentValue"]
                .as_str()
                .filter(|_| !deleted)
                .map(str::to_string),
        })
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

/// One decoded `list/0.3` page: the live `(key, value, version)` triples,
/// unparsed and unchecked, and the cursor for the next page.
type Page = (Vec<(String, String, u64)>, Option<String>);

/// Decode one `list/0.3` response into a [`Page`].
fn decode_page(resp: Value) -> anyhow::Result<Page> {
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
        entries.push((
            item.key.as_str().to_string(),
            value.as_str().to_string(),
            item.version.0.get(),
        ));
    }
    Ok((entries, page.next_cursor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::MemoryRecord;

    fn entry(kind: MemoryType, name: &str, desc: &str) -> Entry {
        Entry::local(
            MemoryKey::new(kind, name).unwrap(),
            MemoryRecord::new(kind, name, desc, "body", vec![]),
        )
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
