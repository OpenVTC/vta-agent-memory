//! Recall across personal memory and the Data Rooms the user belongs to (#34).
//!
//! An agent's knowledge is split between personal memory (`vta/memory/*`) and
//! the rooms its user is a member of. This module reads a room's records
//! through the user's own VTA — `rooms/keys/browse` for the keys,
//! `rooms/keys/read` for the bodies, which the VTA opens with the member's
//! group keys — checks each one, and ranks room records together with
//! personal memories on one scale (`crate::record::relevance` × basis
//! weight × status weight).
//!
//! # What is checked
//!
//! The VTA reports what it checked about the *host* (`verification`); the
//! *author's* signature, inside the sealed body, is the reader's to check
//! (`rooms/keys/read`: "the recipient verifies the author's proof"). So for a
//! sealed record (`attributed` and `private` rooms) this does what `pnm rooms
//! read` does, by the same rules as `vtc_client::rooms::body::verify_record_body`:
//!
//! 1. the body carries a Data Integrity proof (`eddsa-jcs-2022` or
//!    `ecdsa-jcs-2019`) that verifies for `assertionMethod`;
//! 2. its `author` is the signer;
//! 3. it names the room, key and version it was read from — the version it was
//!    *sealed* at, which a curation can leave behind the record's own;
//! 4. on an `attributed` room, its author is the one the host recorded.
//!
//! A record failing any of these is **refused** — left out of recall and
//! reported with why — exactly as a personal memory whose signature fails is.
//!
//! An `open` room's record has no in-body signature: its body is cleartext the
//! host can read and serves as-is. It is recalled, attributed to the author
//! the host recorded, and marked `hostReported` — nothing but the host vouches
//! for it.
//!
//! # Resolving authors
//!
//! Personal memory is verified `did:key`-only, so a stored record can never
//! make recall fetch anything. Room members are often `did:webvh`, so room
//! records take a resolver from the caller: the CLI and server pass the SDK's
//! shared resolver, the tests a `did:key`-only one. The DIDs it resolves are
//! room members' — the VTA would not open the record for a non-member — and
//! resolution happens only after the cheap checks (location, host author).
//!
//! # What recall does not do
//!
//! Write. Saving to a room is an explicit act of the user's, never a side
//! effect of remembering something.

use std::collections::HashMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vta_sdk::client::{ROOM_READ_MAX_KEYS, RoomBrowse, VtaClient};
use vta_sdk::trust_task_proof::{ProofPurpose, PurposeVmResolver, verify_proof_set};

use crate::authorship::Refused;
use crate::fence::Fence;
use crate::provenance::{self, Basis};

/// Suites a room record body may be signed with.
const ACCEPTED_SUITES: [&str; 2] = ["eddsa-jcs-2022", "ecdsa-jcs-2019"];

/// Most records read from one room per recall: the newest, by version. A room
/// is meant to be small; one that is not costs a read per record, and recall
/// is not where to pay for that.
pub const MAX_ROOM_RECORDS: usize = 256;

/// One room recall reads, as the config names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomSource {
    /// The room's identifier (its DID).
    pub room_id: String,
    /// The host's DID — where the VTA fetches the records from.
    pub host: String,
    /// Which member to read as, where the VTA holds the room for more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// A name to show for the room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Who wrote a room record, as far as anything checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomAuthorship {
    /// A sealed record whose in-body signature holds for `author` where it was
    /// read.
    Verified { author: String },
    /// An `open` room's record: no in-body signature, so the author is only
    /// what the host recorded, if anything.
    HostReported { author: Option<String> },
}

impl RoomAuthorship {
    fn to_json(&self, member: Option<&str>) -> Value {
        match self {
            RoomAuthorship::Verified { author } => json!({
                "status": "verified",
                "did": author,
                // Whether the signer was a person or an agent acting for one
                // is not in the record: `SealedRecordBody` carries the signing
                // DID only (data-rooms.md §6.1 plans it via attenuated VACs).
                // Said, rather than guessed.
                "kind": "unstated",
                "thisMember": member == Some(author.as_str()),
            }),
            RoomAuthorship::HostReported { author } => json!({
                "status": "hostReported",
                "did": author,
                "kind": "unstated",
            }),
        }
    }
}

/// A room record that passed its checks and can be recalled.
#[derive(Debug, Clone)]
pub struct RoomRecord {
    pub room_id: String,
    pub label: Option<String>,
    /// The member read as, if the config named one.
    pub member: Option<String>,
    pub key: String,
    pub version: u64,
    pub title: Option<String>,
    pub description: String,
    pub body: String,
    /// `active` or `deprecated` (retracted records are never returned).
    pub status: String,
    pub basis: Option<String>,
    pub confidence: Option<f64>,
    pub evidence: Vec<Value>,
    pub updated_at: String,
    pub authorship: RoomAuthorship,
}

impl RoomRecord {
    /// Ranking weight beyond relevance, on the same scale as a memory's:
    /// basis weight × status weight (a deprecated record counts half).
    pub fn weight(&self) -> u32 {
        let basis = provenance::weight(
            self.basis.as_deref().and_then(Basis::parse),
            !self.evidence.is_empty(),
        );
        let status = if self.status == "active" {
            crate::lifecycle::FULL_WEIGHT
        } else {
            crate::lifecycle::FULL_WEIGHT / 2
        };
        basis * status
    }

    /// The name to rank on and show: the title, or the key for an untitled one.
    pub fn name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.key)
    }

    /// What recall returns: no body, like a memory's summary.
    pub fn summary(&self) -> Value {
        let mut v = json!({
            "source": self.source_json(),
            "key": Fence::sanitize(&self.key),
            "name": Fence::sanitize(self.name()),
            "description": Fence::sanitize(&self.description),
            "updatedAt": self.updated_at,
            "version": self.version,
            "status": self.status,
            "author": self.authorship.to_json(self.member.as_deref()),
            "trust": "untrusted-data",
        });
        let prov = provenance::to_json(self.basis.as_deref(), self.confidence, &self.evidence);
        if let (Some(v), Some(p)) = (v.as_object_mut(), prov.as_object()) {
            v.extend(p.clone());
        }
        v
    }

    /// The summary and the body — what `memory_get` with a `room` returns.
    pub fn full(&self) -> Value {
        let mut v = self.summary();
        v["body"] = Value::String(Fence::sanitize(&self.body));
        if !self.evidence.is_empty() {
            v["evidence"] = Value::Array(self.evidence.clone());
        }
        v
    }

    fn source_json(&self) -> Value {
        let mut s = json!({ "kind": "room", "roomId": self.room_id });
        if let Some(l) = &self.label {
            s["label"] = Value::String(Fence::sanitize(l));
        }
        s
    }
}

/// What reading one room found.
#[derive(Debug, Default)]
pub struct RoomRead {
    pub records: Vec<RoomRecord>,
    /// Records left out, with why — a signature that does not hold, or a body
    /// the VTA could not open.
    pub refused: Vec<Refused>,
}

/// Read every record of `source` that can be recalled, checking each.
///
/// `Err` only when the room could not be read at all (not a member, host
/// unreachable); a record that fails is a [`Refused`] beside the rest.
pub async fn read_room(
    client: &VtaClient,
    source: &RoomSource,
    resolver: &(dyn PurposeVmResolver + '_),
) -> anyhow::Result<RoomRead> {
    let member = source.member.as_deref();
    let listing = client
        .room_browse(
            &source.room_id,
            &source.host,
            &RoomBrowse::default(),
            member,
        )
        .await
        .map_err(|e| anyhow::anyhow!("listing room `{}`: {e}", source.room_id))?;
    let listing = serde_json::to_value(listing)?;
    let mut rows: Vec<(String, u64)> = listing["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["status"].as_str() != Some("retracted"))
        .filter_map(|r| Some((r["key"].as_str()?.to_string(), r["version"].as_u64()?)))
        .collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
    rows.truncate(MAX_ROOM_RECORDS);

    let mut read = RoomRead::default();
    for chunk in rows.chunks(ROOM_READ_MAX_KEYS) {
        let keys: Vec<&str> = chunk.iter().map(|(k, _)| k.as_str()).collect();
        let answer = client
            .room_read_batch(&source.room_id, &source.host, &keys, member)
            .await
            .map_err(|e| anyhow::anyhow!("reading room `{}`: {e}", source.room_id))?;
        let answer = serde_json::to_value(answer)?;
        let sealed_at: HashMap<String, u64> = answer
            .pointer("/ext/org.openvtc.rooms/sealedAt")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_u64()?)))
                    .collect()
            })
            .unwrap_or_default();
        for result in answer["results"].as_array().into_iter().flatten() {
            let key = result["key"].as_str().unwrap_or("?").to_string();
            match check(source, result, sealed_at.get(&key).copied(), resolver).await {
                Ok(Some(record)) => read.records.push(record),
                Ok(None) => {}
                Err(reason) => read.refused.push(Refused {
                    key: format!("room {} / {key}", source.room_id),
                    reason,
                }),
            }
        }
    }
    Ok(read)
}

/// One record from a `rooms/keys/read` result: `Ok(None)` for one recall
/// leaves out by design (retracted, expired, a profile), `Err` for one that is
/// refused.
pub async fn check(
    source: &RoomSource,
    result: &Value,
    sealed_at: Option<u64>,
    resolver: &(dyn PurposeVmResolver + '_),
) -> Result<Option<RoomRecord>, String> {
    let key = result["key"].as_str().unwrap_or("?");
    if let Some(err) = result.get("error") {
        return Err(format!(
            "the VTA could not read it ({})",
            err["code"].as_str().unwrap_or("unknown error")
        ));
    }
    let status = result["status"].as_str().unwrap_or("active");
    if status == "retracted" {
        return Ok(None);
    }
    let version = result["version"].as_u64().unwrap_or(0);
    let host_author = result
        .get("author")
        .or_else(|| result.get("createdBy"))
        .and_then(Value::as_str);

    let (body, authorship) = if let Some(cleartext) = result.get("cleartext") {
        let author = host_author.map(str::to_string);
        (cleartext.clone(), RoomAuthorship::HostReported { author })
    } else if let Some(plaintext) = result["plaintext"].as_str() {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(plaintext)
            .map_err(|e| format!("the VTA returned a body that is not base64url ({e})"))?;
        let at = sealed_at.unwrap_or(version);
        if at > version {
            return Err(format!(
                "it was opened at version {at}, past the record's own {version}"
            ));
        }
        let body = verify_body(
            &bytes,
            &source.room_id,
            key,
            at,
            result.get("author").and_then(Value::as_str),
            resolver,
        )
        .await?;
        let author = body["author"].as_str().unwrap_or_default().to_string();
        (body, RoomAuthorship::Verified { author })
    } else {
        // Neither: the room's own profile, or something this version does not
        // read. Not memory, not a fault.
        return Ok(None);
    };

    let text = |name: &str| body.get(name).and_then(Value::as_str).map(str::to_string);
    let valid_until = text("validUntil");
    if valid_until
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|t| t <= chrono::Utc::now())
    {
        return Ok(None); // the author no longer stands behind it
    }
    Ok(Some(RoomRecord {
        room_id: source.room_id.clone(),
        label: source.label.clone(),
        member: source.member.clone(),
        key: key.to_string(),
        version,
        title: text("title"),
        description: text("description").unwrap_or_default(),
        body: text("body").unwrap_or_default(),
        status: status.to_string(),
        basis: text("basis"),
        confidence: body.get("confidence").and_then(Value::as_f64),
        evidence: body
            .get("evidence")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        updated_at: result["updatedAt"].as_str().unwrap_or_default().to_string(),
        authorship,
    }))
}

/// Check a sealed record's body, opened by the VTA, and return it without its
/// proof. The rules are in the module docs.
async fn verify_body(
    bytes: &[u8],
    room_id: &str,
    key: &str,
    version: u64,
    host_author: Option<&str>,
    resolver: &(dyn PurposeVmResolver + '_),
) -> Result<Value, String> {
    let signed: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("its body is not JSON ({e})"))?;
    let Some(obj) = signed.as_object() else {
        return Err("its body is not an object".to_string());
    };
    let proofs = match obj.get("proof") {
        Some(Value::Array(a)) => a.clone(),
        Some(p @ Value::Object(_)) => vec![p.clone()],
        _ => return Err("its body is not signed".to_string()),
    };
    for p in &proofs {
        let suite = p["cryptosuite"].as_str().unwrap_or("none");
        if !ACCEPTED_SUITES.contains(&suite) {
            return Err(format!(
                "its body is signed with {suite}; a record body takes {}",
                ACCEPTED_SUITES.join(" or ")
            ));
        }
    }

    // The cheap checks first: no author is resolved for a body that names
    // another location or another author than the host recorded.
    let field = |name: &str| obj.get(name).and_then(Value::as_str);
    let signed_at = (
        field("roomId"),
        field("key"),
        obj.get("version").and_then(Value::as_u64),
    );
    if signed_at != (Some(room_id), Some(key), Some(version)) {
        return Err(format!(
            "its body was signed for {} / {} @ v{}, but was read from {room_id} / {key} @ \
             v{version} — it was copied or moved",
            clip(signed_at.0.unwrap_or("<none>")),
            clip(signed_at.1.unwrap_or("<none>")),
            signed_at.2.unwrap_or(0),
        ));
    }
    let author = field("author").ok_or("its body names no author")?;
    if let Some(recorded) = host_author
        && recorded != author
    {
        return Err(format!(
            "its body names {} as author, but the host recorded {}",
            clip(author),
            clip(recorded)
        ));
    }

    let signer = verify_proof_set(&signed, ProofPurpose::AssertionMethod, resolver)
        .await
        .map_err(|e| format!("its signature does not verify ({e}) — it was altered"))?
        .into_signer();
    if signer != author {
        return Err(format!(
            "its body names {} as author but was signed by {}",
            clip(author),
            clip(&signer)
        ));
    }
    let mut body = signed.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("proof");
    }
    Ok(body)
}

/// Bound an attacker-controlled string for a refusal reason.
fn clip(s: &str) -> String {
    const MAX: usize = 100;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    format!("{}…", s.chars().take(MAX - 1).collect::<String>())
}

/// Which sources a recall reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    pub personal: bool,
    pub rooms: Vec<RoomSource>,
}

impl Sources {
    /// The sources `requested` names among those `configured`. `None` is all
    /// of them. Each entry is `personal`, `rooms` (every configured room), or a
    /// configured room's id or label.
    pub fn select(requested: Option<&[String]>, configured: &[RoomSource]) -> Result<Self, String> {
        let Some(requested) = requested.filter(|r| !r.is_empty()) else {
            return Ok(Self {
                personal: true,
                rooms: configured.to_vec(),
            });
        };
        let mut out = Self {
            personal: false,
            rooms: Vec::new(),
        };
        for want in requested {
            match want.trim() {
                "personal" => out.personal = true,
                "rooms" => {
                    for r in configured {
                        if !out.rooms.contains(r) {
                            out.rooms.push(r.clone());
                        }
                    }
                }
                other => {
                    let room = configured
                        .iter()
                        .find(|r| r.room_id == other || r.label.as_deref() == Some(other))
                        .ok_or_else(|| {
                            format!(
                                "`{other}` is not a source: use `personal`, `rooms`, or one of \
                                 the configured rooms ({})",
                                if configured.is_empty() {
                                    "none are configured".to_string()
                                } else {
                                    configured
                                        .iter()
                                        .map(|r| r.label.clone().unwrap_or(r.room_id.clone()))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                }
                            )
                        })?;
                    if !out.rooms.contains(room) {
                        out.rooms.push(room.clone());
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The resolver room records are verified with: the SDK's shared resolver
/// (`PNM_RESOLVER_URL` if set) when `sources` reads a room, since members are
/// often `did:webvh`; `did:key`-only otherwise, so a personal-only recall never
/// builds one. See the module docs for why rooms differ from personal memory.
pub async fn resolver_for(sources: &Sources) -> vta_sdk::trust_task_proof::TrustTaskVmResolver {
    use vta_sdk::trust_task_proof::TrustTaskVmResolver;
    if sources.rooms.is_empty() {
        return TrustTaskVmResolver::did_key_only();
    }
    match vta_sdk::resolver::shared_did_resolver_from_env().await {
        Ok(r) => TrustTaskVmResolver::new(r),
        Err(e) => {
            tracing::warn!(error = %e, "no DID resolver; room records by non-did:key authors will be refused");
            TrustTaskVmResolver::did_key_only()
        }
    }
}

/// One recall result, from wherever it came.
#[derive(Debug, Clone)]
pub enum Found {
    Memory(Box<crate::record::Hit>),
    Room { record: Box<RoomRecord>, score: u32 },
}

impl Found {
    fn score(&self) -> u32 {
        match self {
            Found::Memory(h) => h.score,
            Found::Room { score, .. } => *score,
        }
    }

    fn weight(&self) -> u32 {
        match self {
            Found::Memory(h) => h.entry.weight(),
            Found::Room { record, .. } => record.weight(),
        }
    }

    fn updated_at(&self) -> &str {
        match self {
            Found::Memory(h) => &h.entry.record.updated_at,
            Found::Room { record, .. } => &record.updated_at,
        }
    }

    /// The summary recall returns, naming its source.
    pub fn summary(&self, context_id: &str) -> Value {
        match self {
            Found::Memory(h) => {
                let mut v = h.entry.summary();
                v["source"] = json!({ "kind": "personal", "contextId": context_id });
                v
            }
            Found::Room { record, .. } => record.summary(),
        }
    }
}

/// What a recall across sources found.
#[derive(Debug, Default)]
pub struct Recalled {
    /// Best first, at most the limit asked for.
    pub found: Vec<Found>,
    /// Refused memories and room records, wherever they were.
    pub refused: Vec<Refused>,
    /// Rooms that could not be read at all, with why. Recall carries on
    /// without them: one unreachable host must not take personal memory down.
    pub unavailable: Vec<(String, String)>,
}

/// Recall `query` across `sources`, ranked on one scale.
///
/// A `kind` narrows personal memory to one type and leaves rooms out, since
/// room records are not typed. A room that cannot be read is reported in
/// `unavailable`; a personal-memory failure is an error, as it always was.
pub async fn recall(
    store: &crate::store::Store,
    sources: &Sources,
    query: &str,
    kind: Option<crate::record::MemoryType>,
    limit: usize,
    resolver: &(dyn PurposeVmResolver + '_),
) -> anyhow::Result<Recalled> {
    let mut out = Recalled::default();
    let mut found = Vec::new();
    if sources.personal {
        // Every hit, not `limit`: the limit applies after the merge.
        let personal = store.recall(query, kind, usize::MAX).await?;
        found.extend(
            personal
                .hits
                .into_iter()
                .map(|h| Found::Memory(Box::new(h))),
        );
        out.refused.extend(personal.refused);
    }
    if kind.is_none() {
        let terms = crate::record::query_terms(query);
        for source in &sources.rooms {
            match read_room(store.client(), source, resolver).await {
                Ok(read) => {
                    out.refused.extend(read.refused);
                    for record in read.records {
                        let score = crate::record::relevance(
                            &terms,
                            record.name(),
                            &record.description,
                            &record.body,
                        ) * record.weight();
                        found.push(Found::Room {
                            record: Box::new(record),
                            score,
                        });
                    }
                }
                Err(e) => out
                    .unavailable
                    .push((source.room_id.clone(), format!("{e:#}"))),
            }
        }
    }
    if !query.trim().is_empty() {
        found.retain(|f| f.score() > 0);
    }
    found.sort_by(|a, b| {
        b.score()
            .cmp(&a.score())
            .then_with(|| b.weight().cmp(&a.weight()))
            .then_with(|| b.updated_at().cmp(a.updated_at()))
    });
    found.truncate(limit);
    out.found = found;
    Ok(out)
}

/// One record of a configured room, by key, checked — for `memory_get`.
pub async fn get(
    client: &VtaClient,
    source: &RoomSource,
    key: &str,
    resolver: &(dyn PurposeVmResolver + '_),
) -> anyhow::Result<Option<RoomRecord>> {
    let answer = client
        .room_read_batch(
            &source.room_id,
            &source.host,
            &[key],
            source.member.as_deref(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("reading `{key}` from room `{}`: {e}", source.room_id))?;
    let answer = serde_json::to_value(answer)?;
    let sealed_at = answer
        .pointer("/ext/org.openvtc.rooms/sealedAt")
        .and_then(|m| m.get(key))
        .and_then(Value::as_u64);
    let Some(result) = answer["results"].get(0) else {
        return Ok(None);
    };
    check(source, result, sealed_at, resolver)
        .await
        .map_err(|why| {
            anyhow::anyhow!(
                "`{key}` in room `{}` is refused: {why}. It is not returned",
                source.room_id
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str, label: Option<&str>) -> RoomSource {
        RoomSource {
            room_id: id.into(),
            host: "did:key:host".into(),
            member: None,
            label: label.map(str::to_string),
        }
    }

    #[test]
    fn sources_default_to_everything_and_name_what_is_unknown() {
        let configured = [room("did:web:r1", Some("deals")), room("did:web:r2", None)];
        let all = Sources::select(None, &configured).unwrap();
        assert!(all.personal);
        assert_eq!(all.rooms.len(), 2);

        let one = Sources::select(Some(&["deals".into()]), &configured).unwrap();
        assert!(!one.personal);
        assert_eq!(one.rooms, [configured[0].clone()]);

        let personal = Sources::select(Some(&["personal".into()]), &configured).unwrap();
        assert!(personal.personal && personal.rooms.is_empty());

        let err = Sources::select(Some(&["nope".into()]), &configured).unwrap_err();
        assert!(err.contains("deals"), "{err}");
    }

    #[test]
    fn a_deprecated_room_record_weighs_half() {
        let mut r = RoomRecord {
            room_id: "r".into(),
            label: None,
            member: None,
            key: "k".into(),
            version: 1,
            title: None,
            description: String::new(),
            body: String::new(),
            status: "active".into(),
            basis: Some("declared".into()),
            confidence: None,
            evidence: vec![],
            updated_at: String::new(),
            authorship: RoomAuthorship::HostReported { author: None },
        };
        let active = r.weight();
        r.status = "deprecated".into();
        assert_eq!(r.weight() * 2, active);
    }
}
