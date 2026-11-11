//! Shared by the integration tests: an in-process stand-in for the VTA's
//! memory keyspace, and the identities that write to it.
//!
//! The SDK's `test-loopback` transport answers the Trust-Task surface in the
//! same process, so whatever drives a [`Store`] over [`FakeMemoryKeyspace`]
//! runs the *real* `VtaClient` memory method bodies — the payloads that ship —
//! against the storage semantics the VTA's own `operations::memory` has at
//! `vta/memory/*/0.3` and `get/0.2`:
//!
//! - `put` upserts on `(contextId, key)` and takes the context's next version;
//!   `expectedVersion` makes it conditional, and a failed precondition is
//!   `versionConflict` carrying the current version and value.
//! - `list` returns the context's live items in ascending key order, narrowed
//!   by `prefix`, and never another context's.
//! - `get` answers each key in `items` or `missing`.
//! - `delete` leaves nothing live and answers `existed`.
//! - every operation names a `purpose`, held to the purposes granted.
//!
//! Used by `memory_roundtrip.rs` and the evaluation harness (`eval.rs`), so the
//! harness scores the same store the tests pin.

#![allow(dead_code)] // each test target uses a different subset

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vta_sdk::client::VtaClient;
use vta_sdk::client::loopback::LoopbackSink;
use vta_sdk::error::VtaError;

use vta_agent_memory::authorship::{AuthorKind, Signer};
use vta_agent_memory::store::Store;

/// The memory Trust Tasks, backed by a `BTreeMap` keyed by
/// `(contextId, key)`, so ordering and the context-isolation property are the
/// real ones rather than assumed.
#[derive(Default)]
pub struct FakeMemoryKeyspace {
    /// `(contextId, key)` → `(value, version)`.
    pub entries: Mutex<BTreeMap<(String, String), (String, u64)>>,
    pub counter: Mutex<u64>,
    /// Every `(type_uri, payload)` dispatched, so a test can assert on the wire
    /// shape as well as the outcome.
    pub seen: Mutex<Vec<(String, Value)>>,
    /// The purposes the caller's entry lists; `None` is `any`.
    pub granted: Mutex<Option<Vec<String>>>,
    /// A write another writer makes right after the next `get` is answered:
    /// `(context, key, value)`. Lets a test land a save in the window between a
    /// store's read and its conditional write.
    pub race: Mutex<Option<(String, String, String)>>,
    /// Data Rooms, for recall across them (#34): room id → the entries
    /// `rooms/keys/read/0.2` answers with, each with its `key`.
    pub rooms: Mutex<BTreeMap<String, Vec<Value>>>,
    /// Rooms whose host cannot be reached.
    pub unreachable: Mutex<Vec<String>>,
}

impl FakeMemoryKeyspace {
    pub fn recorded(&self) -> Vec<(String, Value)> {
        self.seen.lock().unwrap().clone()
    }

    /// Live memories, not counting history copies.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap()
            .keys()
            .filter(|(_, k)| !k.starts_with("archive/"))
            .count()
    }

    /// History copies held for `key` in `context`.
    pub fn history_len(&self, context: &str, key: &str) -> usize {
        let prefix = format!("archive/{key}/");
        self.entries
            .lock()
            .unwrap()
            .keys()
            .filter(|(c, k)| c == context && k.starts_with(&prefix))
            .count()
    }

    pub fn record(context_id: &str, key: &str, value: &str, version: u64) -> Value {
        json!({
            "contextId": context_id, "key": key, "value": value, "version": version,
            "valueBytes": value.len(), "deleted": false,
            "updatedAt": "2026-11-09T00:00:00Z",
        })
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

        if type_uri.contains("/rooms/keys/") {
            return self.room_task(type_uri, payload);
        }

        let context_id = field(payload, "contextId")?;
        if let Some(granted) = self.granted.lock().unwrap().as_ref() {
            let purpose = payload.get("purpose").and_then(Value::as_str);
            let ok = match purpose {
                Some(p) => granted.iter().any(|g| g == p),
                None => granted.len() == 1,
            };
            if !ok {
                let op = type_uri.rsplit('/').nth(1).unwrap_or_default().to_string();
                return Err(VtaError::Rejected {
                    code: format!("vta/memory/{op}:purposeNotGranted"),
                    message: "this entry does not grant the purpose".into(),
                    details: Some(json!({ "purposeNamed": purpose.is_some(), "purpose": purpose })),
                });
            }
        }

        match type_uri {
            u if u.ends_with("/vta/memory/put/0.3") => {
                let key = field(payload, "key")?;
                let value = field(payload, "value")?;
                // `expectedVersion`: 0 is create-only, n requires version n.
                // A failed precondition is `versionConflict` with the current
                // version and value, as the VTA sends it.
                if let Some(expected) = payload.get("expectedVersion").and_then(Value::as_u64) {
                    let current = self
                        .entries
                        .lock()
                        .unwrap()
                        .get(&(context_id.to_string(), key.to_string()))
                        .cloned();
                    let reason = match (&current, expected) {
                        (None, 0) => None,
                        (Some(_), 0) => Some("recordExists"),
                        (None, _) => Some("recordAbsent"),
                        (Some((_, v)), e) if *v != e => Some("versionMismatch"),
                        _ => None,
                    };
                    if let Some(reason) = reason {
                        let mut details = json!({ "reason": reason });
                        if let Some((v, ver)) = current {
                            details["currentVersion"] = json!(ver);
                            details["currentValue"] = json!(v);
                        }
                        return Err(VtaError::Rejected {
                            code: "vta/memory/put:versionConflict".into(),
                            message: "precondition failed".into(),
                            details: Some(details),
                        });
                    }
                }
                let mut counter = self.counter.lock().unwrap();
                *counter += 1;
                let created = self
                    .entries
                    .lock()
                    .unwrap()
                    .insert(
                        (context_id.to_string(), key.to_string()),
                        (value.to_string(), *counter),
                    )
                    .is_none();
                Ok(json!({
                    "contextId": context_id, "key": key, "version": *counter,
                    "wasCreated": created, "updatedAt": "2026-11-09T00:00:00Z",
                    "valueBytes": value.len(),
                }))
            }
            u if u.ends_with("/vta/memory/list/0.3") => {
                let prefix = payload.get("prefix").and_then(Value::as_str).unwrap_or("");
                let items: Vec<Value> = self
                    .entries
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|((c, k), _)| c == context_id && k.starts_with(prefix))
                    .map(|((c, k), (v, ver))| Self::record(c, k, v, *ver))
                    .collect();
                Ok(json!({ "items": items, "highWatermark": *self.counter.lock().unwrap() }))
            }
            u if u.ends_with("/vta/memory/get/0.2") => {
                let race = self.race.lock().unwrap().take();
                let entries = self.entries.lock().unwrap();
                let (mut items, mut missing) = (Vec::new(), Vec::new());
                for k in payload["keys"].as_array().into_iter().flatten() {
                    let k = k.as_str().unwrap_or_default();
                    match entries.get(&(context_id.to_string(), k.to_string())) {
                        Some((v, ver)) => items.push(Self::record(context_id, k, v, *ver)),
                        None => missing.push(json!(k)),
                    }
                }
                drop(entries);
                if let Some((c, k, v)) = race {
                    self.plant(&c, &k, v);
                }
                Ok(json!({ "items": items, "missing": missing }))
            }
            u if u.ends_with("/vta/memory/delete/0.3") => {
                let key = field(payload, "key")?;
                let existed = self
                    .entries
                    .lock()
                    .unwrap()
                    .remove(&(context_id.to_string(), key.to_string()))
                    .is_some();
                Ok(json!({ "contextId": context_id, "key": key, "existed": existed }))
            }
            other => Err(VtaError::Validation(format!(
                "the memory store must not dispatch `{other}`"
            ))),
        }
    }
}

impl FakeMemoryKeyspace {
    /// `rooms/keys/browse/0.4` and `rooms/keys/read/0.2`, as the member's VTA
    /// answers them once it has fetched from the host and opened the bodies.
    fn room_task(&self, type_uri: &str, payload: &Value) -> Result<Value, VtaError> {
        let room_id = field(payload, "roomId")?;
        if self
            .unreachable
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == room_id)
        {
            return Err(VtaError::Rejected {
                code: "rooms/keys/read:hostUnreachable".into(),
                message: "the host did not answer".into(),
                details: None,
            });
        }
        let rooms = self.rooms.lock().unwrap();
        let Some(records) = rooms.get(room_id) else {
            return Err(VtaError::Rejected {
                code: "rooms/keys/read:notAMember".into(),
                message: "no group state for this room".into(),
                details: None,
            });
        };
        let verification = json!({ "priorRoots": "notChecked" });
        if type_uri.ends_with("/rooms/keys/browse/0.4") {
            let rows: Vec<Value> = records
                .iter()
                .map(|r| {
                    json!({
                        "key": r["key"], "version": r["version"], "updatedAt": r["updatedAt"],
                        "status": r.get("status").cloned().unwrap_or(json!("active")),
                    })
                })
                .collect();
            return Ok(json!({
                "roomId": room_id, "records": rows, "verification": verification, "complete": true,
            }));
        }
        if type_uri.ends_with("/rooms/keys/read/0.2") {
            let keys: Vec<&str> = payload["keys"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut sealed_at = serde_json::Map::new();
            let results: Vec<Value> = keys
                .iter()
                .map(|k| match records.iter().find(|r| r["key"] == *k) {
                    Some(r) => {
                        let mut r = r.clone();
                        if let Some(at) = r.as_object_mut().and_then(|o| o.remove("sealedAt")) {
                            sealed_at.insert(k.to_string(), at);
                        }
                        r["verification"] = verification.clone();
                        r
                    }
                    None => json!({ "key": k, "error": {
                        "code": "rooms/keys/read:cannotOpen", "message": "no such record",
                        "retryable": false,
                    }}),
                })
                .collect();
            let mut out = json!({ "roomId": room_id, "results": results });
            if !sealed_at.is_empty() {
                out["ext"] = json!({ "org.openvtc.rooms": { "sealedAt": sealed_at } });
            }
            return Ok(out);
        }
        Err(VtaError::Validation(format!(
            "the rooms fake does not answer `{type_uri}`"
        )))
    }

    /// Put a record in `room`, as `rooms/keys/read` will answer it.
    pub fn room_record(&self, room: &str, entry: Value) {
        self.rooms
            .lock()
            .unwrap()
            .entry(room.to_string())
            .or_default()
            .push(entry);
    }
}

/// A sealed room record body signed by `(did, private)` for `(room, key,
/// version)`, as `rooms/keys/read` returns it opened: base64url `plaintext`.
pub async fn sealed_body(
    (did, private): (&str, &str),
    room: &str,
    key: &str,
    version: u64,
    fields: Value,
) -> String {
    use base64::Engine as _;
    let mut body = json!({ "roomId": room, "key": key, "version": version, "author": did });
    if let (Some(b), Some(f)) = (body.as_object_mut(), fields.as_object()) {
        b.extend(f.clone());
    }
    let holder = vta_sdk::trust_task_sign::HolderKey::from_did_key(did, private).unwrap();
    vta_sdk::trust_task_sign::sign_object_for_assertion(&mut body, &holder)
        .await
        .unwrap();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).unwrap())
}

/// A fresh `did:key` signer, as the session store would hold for an agent.
pub fn signer(kind: AuthorKind) -> Signer {
    let (did, private) = did_key();
    Signer::from_did_key(&did, &private, kind).unwrap()
}

/// A fresh Ed25519 `did:key` and its private key, multibase.
pub fn did_key() -> (String, String) {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    let public = ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .to_bytes();
    let did = format!(
        "did:key:{}",
        vta_sdk::did_key::ed25519_multibase_pubkey(&public)
    );
    let private = multibase::encode(multibase::Base::Base58Btc, seed);
    (did, private)
}

/// A store over `vta` in `context`, signing as a fresh agent.
pub fn store_on(vta: &Arc<FakeMemoryKeyspace>, context: &str) -> Store {
    Store::new(VtaClient::loopback(vta.clone()), context).with_signer(signer(AuthorKind::Agent))
}

pub fn store_with(context: &str) -> (Store, Arc<FakeMemoryKeyspace>) {
    let vta = Arc::new(FakeMemoryKeyspace::default());
    (store_on(&vta, context), vta)
}

impl FakeMemoryKeyspace {
    /// The stored value at `(context, key)`.
    pub fn value(&self, context: &str, key: &str) -> String {
        self.entries.lock().unwrap()[&(context.to_string(), key.to_string())]
            .0
            .clone()
    }

    /// Store `value` at `(context, key)` directly, as another writer with
    /// access to the context could.
    pub fn plant(&self, context: &str, key: &str, value: impl Into<String>) {
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;
        self.entries
            .lock()
            .unwrap()
            .insert((context.into(), key.into()), (value.into(), *counter));
    }
}
