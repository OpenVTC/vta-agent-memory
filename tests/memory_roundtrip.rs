//! End-to-end tests for [`Store`] against an in-process stand-in for the VTA's
//! memory keyspace.
//!
//! The SDK's `test-loopback` transport answers the Trust-Task surface in the
//! same process, ahead of any real transport. So these tests run the *real*
//! `VtaClient::memory_{put,list,delete}` bodies — the same payload-building code
//! that ships — and hand what they emit to [`FakeMemoryKeyspace`], which
//! implements the storage semantics the VTA's own `operations::memory` does:
//!
//! - `put` upserts on `(contextId, key)`.
//! - `list` returns every entry in the context, ascending key order, and never
//!   another context's entries.
//! - `delete` of an absent key is `not found`.
//!
//! What this cannot cover is framing — TSP sealing, DIDComm authcrypt, mediator
//! routing all sit below the loopback point. It covers the layer where the
//! behaviour of *this* crate lives: the payloads sent, and what the store does
//! with what comes back.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vta_sdk::client::VtaClient;
use vta_sdk::client::loopback::LoopbackSink;
use vta_sdk::error::VtaError;

use vta_agent_memory::record::{MemoryKey, MemoryRecord, MemoryType};
use vta_agent_memory::store::Store;

/// The three memory Trust Tasks, backed by a `BTreeMap` keyed the way the VTA
/// keys its own store (`mem:<contextId>:<key>`), so ordering and the
/// context-isolation property are the real ones rather than assumed.
#[derive(Default)]
struct FakeMemoryKeyspace {
    entries: Mutex<BTreeMap<String, (String, String)>>,
    /// Every `(type_uri, payload)` dispatched, so a test can assert on the wire
    /// shape as well as the outcome.
    seen: Mutex<Vec<(String, Value)>>,
}

impl FakeMemoryKeyspace {
    fn storage_key(context_id: &str, key: &str) -> String {
        format!("mem:{context_id}:{key}")
    }

    fn recorded(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

fn field<'a>(payload: &'a Value, name: &str) -> Result<&'a str, VtaError> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| VtaError::Validation(format!("payload is missing `{name}`: {payload}")))
}

impl LoopbackSink for FakeMemoryKeyspace {
    fn dispatch(&self, type_uri: &str, payload: &Value) -> Result<Value, VtaError> {
        self.seen
            .lock()
            .unwrap()
            .push((type_uri.to_string(), payload.clone()));

        let context_id = field(payload, "contextId")?;

        match type_uri {
            u if u.ends_with("/vta/memory/put/0.1") => {
                let key = field(payload, "key")?;
                let value = field(payload, "value")?;
                self.entries.lock().unwrap().insert(
                    Self::storage_key(context_id, key),
                    (key.to_string(), value.to_string()),
                );
                Ok(json!({ "key": key }))
            }
            u if u.ends_with("/vta/memory/list/0.1") => {
                let prefix = format!("mem:{context_id}:");
                let items: Vec<Value> = self
                    .entries
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(sk, _)| sk.starts_with(&prefix))
                    .map(|(_, (k, v))| json!({ "key": k, "value": v }))
                    .collect();
                Ok(json!({ "items": items }))
            }
            u if u.ends_with("/vta/memory/delete/0.1") => {
                let key = field(payload, "key")?;
                let removed = self
                    .entries
                    .lock()
                    .unwrap()
                    .remove(&Self::storage_key(context_id, key));
                match removed {
                    Some(_) => Ok(json!({ "key": key })),
                    None => Err(VtaError::NotFound(format!(
                        "memory entry `{key}` not found in context `{context_id}`"
                    ))),
                }
            }
            other => Err(VtaError::Validation(format!(
                "the memory store must not dispatch `{other}`"
            ))),
        }
    }
}

fn store_with(context: &str) -> (Store, Arc<FakeMemoryKeyspace>) {
    let vta = Arc::new(FakeMemoryKeyspace::default());
    let client = VtaClient::loopback(vta.clone());
    (Store::new(client, context), vta)
}

fn record(kind: MemoryType, name: &str, description: &str, body: &str) -> MemoryRecord {
    MemoryRecord::new(kind, name, description, body, vec![])
}

#[tokio::test]
async fn a_saved_memory_comes_back_from_recall_and_get() {
    let (store, _vta) = store_with("proj");

    let key = store
        .save(&record(
            MemoryType::Feedback,
            "No PR attribution",
            "Don't add Claude trailers to commits or PRs",
            "The commit log is the team's record, not the tool's.",
        ))
        .await
        .unwrap();
    assert_eq!(key.to_string(), "feedback/no-pr-attribution");

    let hits = store.recall("attribution", None, 8).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.record.name, "No PR attribution");

    let full = store.get(&key).await.unwrap().expect("present");
    assert!(full.record.body.contains("the team's record"));
}

#[tokio::test]
async fn saving_the_same_name_replaces_rather_than_duplicates() {
    let (store, vta) = store_with("proj");

    store
        .save(&record(MemoryType::Project, "Gateway", "first", "one"))
        .await
        .unwrap();
    store
        .save(&record(MemoryType::Project, "Gateway", "second", "two"))
        .await
        .unwrap();

    assert_eq!(vta.len(), 1, "a re-save must upsert, not append");
    let entries = store.list().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].record.description, "second");
}

#[tokio::test]
async fn names_that_differ_only_in_punctuation_are_the_same_memory() {
    // The slug is the identity. "Gateway (v2)" and "gateway v2" must not become
    // two memories that disagree with each other.
    let (store, vta) = store_with("proj");
    store
        .save(&record(MemoryType::Project, "Gateway (v2)", "a", "x"))
        .await
        .unwrap();
    store
        .save(&record(MemoryType::Project, "gateway  v2", "b", "y"))
        .await
        .unwrap();
    assert_eq!(vta.len(), 1);
}

#[tokio::test]
async fn recall_returns_nothing_for_a_query_that_matches_nothing() {
    let (store, _vta) = store_with("proj");
    store
        .save(&record(
            MemoryType::User,
            "Prefers Rust",
            "works in Rust",
            "…",
        ))
        .await
        .unwrap();

    assert!(
        store
            .recall("kubernetes", None, 8)
            .await
            .unwrap()
            .is_empty(),
        "a near-miss must not be passed off as an answer"
    );
}

#[tokio::test]
async fn recall_with_no_query_returns_the_context_up_to_the_limit() {
    let (store, _vta) = store_with("proj");
    for i in 0..5 {
        store
            .save(&record(
                MemoryType::Project,
                &format!("Memory {i}"),
                "d",
                "b",
            ))
            .await
            .unwrap();
    }
    assert_eq!(store.recall("", None, 3).await.unwrap().len(), 3);
    assert_eq!(store.recall("", None, 99).await.unwrap().len(), 5);
}

#[tokio::test]
async fn the_type_filter_narrows_before_decoding() {
    let (store, _vta) = store_with("proj");
    store
        .save(&record(MemoryType::Feedback, "Rule", "a rule", "x"))
        .await
        .unwrap();
    store
        .save(&record(
            MemoryType::Project,
            "Rule of thumb",
            "a project",
            "x",
        ))
        .await
        .unwrap();

    let only_feedback = store
        .list_of_type(Some(MemoryType::Feedback))
        .await
        .unwrap();
    assert_eq!(only_feedback.len(), 1);
    assert_eq!(only_feedback[0].key.kind, MemoryType::Feedback);
    assert_eq!(store.list().await.unwrap().len(), 2);
}

#[tokio::test]
async fn forgetting_removes_it_and_forgetting_again_reports_not_found() {
    let (store, _vta) = store_with("proj");
    let key = store
        .save(&record(MemoryType::Reference, "Dashboard", "a link", "…"))
        .await
        .unwrap();

    store.forget(&key).await.unwrap();
    assert!(store.get(&key).await.unwrap().is_none());

    let err = store.forget(&key).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("not found"),
        "the VTA's not-found must survive to the caller: {err:#}"
    );
}

#[tokio::test]
async fn one_context_never_sees_anothers_memories() {
    // The privilege boundary is enforced on the VTA, but the store must also
    // never *ask* across contexts — a `list` that returned another context's
    // entries would mean the wrong contextId went on the wire.
    let vta = Arc::new(FakeMemoryKeyspace::default());
    let a = Store::new(VtaClient::loopback(vta.clone()), "ctx-a");
    let b = Store::new(VtaClient::loopback(vta.clone()), "ctx-b");

    a.save(&record(MemoryType::User, "Secret", "a-only", "x"))
        .await
        .unwrap();
    b.save(&record(MemoryType::User, "Secret", "b-only", "y"))
        .await
        .unwrap();

    let a_entries = a.list().await.unwrap();
    assert_eq!(a_entries.len(), 1);
    assert_eq!(a_entries[0].record.description, "a-only");
    assert_eq!(b.list().await.unwrap()[0].record.description, "b-only");
}

#[tokio::test]
async fn a_sibling_context_prefix_does_not_bleed() {
    // `ctx` must not match `ctx-extra`. The VTA guards this with a trailing `:`
    // in its prefix scan; the fake keys entries the same way, so this test
    // fails if the client ever stops sending an exact contextId.
    let vta = Arc::new(FakeMemoryKeyspace::default());
    let short = Store::new(VtaClient::loopback(vta.clone()), "ctx");
    let long = Store::new(VtaClient::loopback(vta.clone()), "ctx-extra");

    short
        .save(&record(MemoryType::User, "K", "short", "x"))
        .await
        .unwrap();
    long.save(&record(MemoryType::User, "K", "long", "y"))
        .await
        .unwrap();

    let entries = short.list().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].record.description, "short");
}

#[tokio::test]
async fn an_entry_written_by_another_tool_is_skipped_not_fatal() {
    // A context shared with something else writing plain keys must stay
    // listable. Erroring here would make one stray entry break every recall.
    let vta = Arc::new(FakeMemoryKeyspace::default());
    vta.entries.lock().unwrap().insert(
        FakeMemoryKeyspace::storage_key("proj", "some-other-tools-key"),
        ("some-other-tools-key".into(), "opaque".into()),
    );
    let store = Store::new(VtaClient::loopback(vta.clone()), "proj");

    store
        .save(&record(MemoryType::User, "Mine", "ours", "x"))
        .await
        .unwrap();

    let entries = store.list().await.unwrap();
    assert_eq!(entries.len(), 1, "the foreign entry is skipped");
    assert_eq!(entries[0].record.name, "Mine");
    assert_eq!(vta.len(), 2, "and is left untouched in the store");
}

#[tokio::test]
async fn the_payloads_on_the_wire_are_camel_case_and_context_scoped() {
    // The recurring defect class in this stack is casing drift on wire types.
    // These payloads are built by the SDK, but this crate is what decides the
    // key and value in them, so pin the whole shape.
    let (store, vta) = store_with("proj");
    let key = store
        .save(&record(MemoryType::User, "Name", "desc", "body"))
        .await
        .unwrap();
    store.forget(&key).await.unwrap();

    let seen = vta.recorded();
    let (put_uri, put) = &seen[0];
    assert!(put_uri.ends_with("/vta/memory/put/0.1"), "{put_uri}");
    assert_eq!(put.get("contextId").unwrap(), "proj");
    assert_eq!(put.get("key").unwrap(), "user/name");
    assert!(
        put.get("context_id").is_none(),
        "snake_case must not appear"
    );

    // The value is the record, and it round-trips.
    let stored: MemoryRecord =
        serde_json::from_str(put.get("value").unwrap().as_str().unwrap()).unwrap();
    assert_eq!(stored.name, "Name");
    assert_eq!(stored.kind, MemoryType::User);
    assert!(!stored.updated_at.is_empty(), "every write is stamped");

    let (del_uri, del) = &seen[seen.len() - 1];
    assert!(del_uri.ends_with("/vta/memory/delete/0.1"), "{del_uri}");
    assert_eq!(del.get("key").unwrap(), "user/name");
}

#[tokio::test]
async fn a_name_with_no_usable_characters_never_reaches_the_vta() {
    let (store, vta) = store_with("proj");
    let err = store
        .save(&record(MemoryType::User, "!!! ???", "d", "b"))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("memory name"), "{err:#}");
    assert!(
        vta.recorded().is_empty(),
        "a key that cannot be addressed must fail before the round trip"
    );
}

#[test]
fn keys_survive_a_round_trip_through_storage() {
    let key = MemoryKey::new(MemoryType::Feedback, "No PR attribution").unwrap();
    assert_eq!(MemoryKey::parse(&key.to_string()).unwrap(), key);
}
