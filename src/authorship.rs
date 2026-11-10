//! Who wrote a memory, and whether that claim holds (#31).
//!
//! A trust context can be granted to more than one DID — the user's other
//! devices, other agents — and the VTA stores whatever any of them puts. Before
//! records were signed, nothing distinguished a memory the user dictated from
//! one another agent saved, or from one injected by a compromised writer.
//!
//! So every record this crate writes carries a Data Integrity proof by the
//! writer's own key — the same `eddsa-jcs-2022` / `ecdsa-jcs-2019` proof, made
//! by the same SDK primitive ([`sign_object_for_assertion`]), that Data Rooms
//! put on a sealed record body. The signed object names its `author` and the
//! `(contextId, key)` it was written to, so a reader can check three things:
//!
//! 1. **The bytes are what the author signed.** A record edited after signing
//!    does not verify.
//! 2. **The author is who signed.** `author` must be the proof's signer, so a
//!    record cannot claim somebody else wrote it.
//! 3. **It is where it was written.** A signed record copied to another key or
//!    another context names the place it came from, not the place it was found.
//!
//! A record failing any of those is **refused**: it is not returned as memory,
//! and the caller is told it was there and why it was refused, as `pnm rooms
//! get` does with a room record whose signature does not hold.
//!
//! # What is not refused
//!
//! Records written before signing existed — `v` 0 or 1, no `proof` — stay
//! readable, marked [`Authorship::Unattributed`]. Refusing them would make
//! every existing memory vanish on upgrade. The cost is that a writer with
//! access to the context can still write an unsigned record; what it can no
//! longer do is make one look *attributed*. A record that says it is signed
//! (`v` ≥ 2) but carries no proof is refused, so stripping a signature does not
//! downgrade a record to merely unattributed.
//!
//! # Verification does no I/O
//!
//! The resolver is `did:key`-only ([`TrustTaskVmResolver::did_key_only`]): the
//! key is in the identifier, so checking a record never fetches anything, and a
//! stored record cannot make recall reach out to a URL of its choosing. Every
//! identity this crate signs with is a session-store `did:key` (enrolment
//! rotates to a fresh one on first connect), so nothing this crate wrote needs
//! more. A record signed by any other DID method is refused, with a reason that
//! says so.

use serde_json::Value;
use vta_sdk::trust_task_proof::{ProofPurpose, TrustTaskVmResolver, verify_proof_set};
use vta_sdk::trust_task_sign::{HolderKey, sign_object_for_assertion};

use crate::record::{MemoryKey, MemoryRecord, RECORD_VERSION};

/// The first record version that carries a signature. A record at or above it
/// with no `proof` has had its signature removed.
pub const FIRST_SIGNED_VERSION: u64 = 2;

/// Which kind of principal a record's author is, as the author stated it.
///
/// Signed, so it cannot be changed after the fact — but asserted by the signer,
/// so it says what the writer *was configured as*, not something the VTA
/// vouched for. A dedicated agent identity writes `agent`; the operator's own
/// `pnm` login (`setup --use-session`) writes `operator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorKind {
    /// The operator's own `pnm` identity.
    Operator,
    /// A dedicated, context-scoped agent identity.
    Agent,
}

impl AuthorKind {
    /// The wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthorKind::Operator => "operator",
            AuthorKind::Agent => "agent",
        }
    }

    /// Parse the wire form. `None` for anything else, which is shown as
    /// unknown rather than failing the record: the kind is descriptive.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "operator" => Some(AuthorKind::Operator),
            "agent" => Some(AuthorKind::Agent),
            _ => None,
        }
    }
}

/// The key this process signs the records it saves with.
pub struct Signer {
    key: HolderKey,
    kind: AuthorKind,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `HolderKey`'s own `Debug` redacts the private half.
        f.debug_struct("Signer")
            .field("did", &self.did())
            .field("kind", &self.kind)
            .finish()
    }
}

impl Signer {
    /// A signer for a `did:key` identity and its private key, as the session
    /// store holds them.
    pub fn from_did_key(
        did: &str,
        private_key_multibase: &str,
        kind: AuthorKind,
    ) -> anyhow::Result<Self> {
        let key = HolderKey::from_did_key(did, private_key_multibase).map_err(|e| {
            anyhow::anyhow!(
                "this identity cannot sign memory records: {e}. Memory records are signed by the \
                 writer's own did:key, which every identity `setup` or `enrol` creates is."
            )
        })?;
        Ok(Self { key, kind })
    }

    /// The DID records are signed as — the `author` they carry.
    pub fn did(&self) -> &str {
        self.key.holder_did()
    }

    /// The kind of author this signer writes as.
    pub fn kind(&self) -> AuthorKind {
        self.kind
    }

    /// The stored value for `record` at `(context_id, key)`: the record with
    /// its author and location filled in, signed.
    ///
    /// Whatever authorship fields `record` already carries are replaced — a
    /// record is always attributed to whoever signs it.
    pub async fn sign(
        &self,
        record: &MemoryRecord,
        context_id: &str,
        key: &MemoryKey,
    ) -> anyhow::Result<String> {
        let mut r = record.clone();
        r.v = RECORD_VERSION;
        r.author = Some(self.did().to_string());
        r.author_kind = Some(self.kind.as_str().to_string());
        r.context_id = Some(context_id.to_string());
        r.key = Some(key.to_string());
        r.proof = None;

        let mut value = serde_json::to_value(&r)?;
        sign_object_for_assertion(&mut value, &self.key)
            .await
            .map_err(|e| anyhow::anyhow!("signing memory `{key}`: {e}"))?;
        Ok(serde_json::to_string(&value)?)
    }
}

/// Who wrote a record that was returned as memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorship {
    /// Signed, and the signature holds for this author at this location.
    Verified {
        /// The signer's DID.
        author: String,
        /// The kind of principal the author said it was; `None` if it named
        /// one this version does not know.
        kind: Option<AuthorKind>,
        /// Whether the author is the identity this process signs as — "I wrote
        /// this" rather than "another writer on this context did".
        by_this_agent: bool,
    },
    /// Written before records were signed, or by something that does not sign.
    /// Readable, but nothing vouches for who wrote it.
    Unattributed,
}

impl Authorship {
    /// The `author` block recall and get return.
    pub fn to_json(&self) -> Value {
        match self {
            Authorship::Verified {
                author,
                kind,
                by_this_agent,
            } => serde_json::json!({
                "status": "verified",
                "did": author,
                "kind": kind.map(|k| k.as_str()).unwrap_or("unknown"),
                "thisAgent": by_this_agent,
            }),
            Authorship::Unattributed => serde_json::json!({ "status": "unattributed" }),
        }
    }

    /// A short note for a rendered listing, or `None` when there is nothing
    /// worth saying (this agent wrote it).
    pub fn note(&self) -> Option<String> {
        match self {
            Authorship::Verified {
                by_this_agent: true,
                ..
            } => None,
            Authorship::Verified { author, kind, .. } => Some(format!(
                "written by {} {}",
                kind.map(|k| k.as_str()).unwrap_or("writer"),
                abbreviate_did(author)
            )),
            Authorship::Unattributed => Some("unattributed".to_string()),
        }
    }
}

/// A stored entry that is not returned as memory, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The storage key it was found under.
    pub key: String,
    /// Why it was refused, in words a person can act on.
    pub reason: String,
}

impl Refused {
    /// The form recall and list return.
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "key": self.key, "reason": self.reason })
    }
}

/// Check the stored `value` found at `(context_id, key)`.
///
/// `Ok` with how it is attributed, or `Err` with why it is refused. `own_did`
/// is the DID this process signs as, if any, to tell its own records apart.
pub async fn verify(
    value: &str,
    context_id: &str,
    key: &str,
    own_did: Option<&str>,
) -> Result<Authorship, String> {
    // A value that is not a JSON object was not written by this crate at any
    // version; `MemoryRecord::decode` already reads it as a foreign reference.
    let Ok(raw) = serde_json::from_str::<Value>(value) else {
        return Ok(Authorship::Unattributed);
    };
    let Some(obj) = raw.as_object() else {
        return Ok(Authorship::Unattributed);
    };

    if !obj.contains_key("proof") {
        let v = obj.get("v").and_then(Value::as_u64).unwrap_or(0);
        if v >= FIRST_SIGNED_VERSION {
            return Err(format!(
                "it is a signed-format record (v{v}) with no signature — the signature was \
                 removed after it was written"
            ));
        }
        return Ok(Authorship::Unattributed);
    }

    // Verified over the value exactly as stored, not a re-serialisation of the
    // decoded struct: a field this version does not know is still signed, and
    // dropping it would fail a record a newer writer made correctly.
    let signer = verify_proof_set(
        &raw,
        ProofPurpose::AssertionMethod,
        &TrustTaskVmResolver::did_key_only(),
    )
    .await
    .map_err(|e| {
        format!("its signature does not verify ({e}) — it was altered after it was signed")
    })?
    .into_signer();

    let field = |name: &str| obj.get(name).and_then(Value::as_str);
    match field("author") {
        Some(author) if author == signer => {}
        Some(author) => {
            return Err(format!(
                "it claims to be written by {} but was signed by {}",
                clip(author),
                clip(&signer)
            ));
        }
        None => {
            return Err(format!(
                "it is signed by {} but names no author",
                clip(&signer)
            ));
        }
    }
    let (signed_context, signed_key) = (field("contextId"), field("key"));
    if signed_context != Some(context_id) || signed_key != Some(key) {
        return Err(format!(
            "it was signed for `{}` in context `{}`, not where it was found — it was copied or \
             moved here",
            clip(signed_key.unwrap_or("<none>")),
            clip(signed_context.unwrap_or("<none>")),
        ));
    }

    Ok(Authorship::Verified {
        by_this_agent: own_did == Some(signer.as_str()),
        kind: field("authorKind").and_then(AuthorKind::parse),
        author: signer,
    })
}

/// A string taken from a refused record, cut short for its refusal reason.
///
/// Everything a refused record says about itself is attacker-controlled — that
/// is why it was refused — and the reason is shown to a model and a person. So
/// it gets a bound, on top of the fence sanitising every rendered field.
fn clip(s: &str) -> String {
    const MAX: usize = 100;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX - 1).collect();
    format!("{cut}…")
}

/// `did:key:z6Mk…wxyz` — enough to tell writers apart in a listing.
fn abbreviate_did(did: &str) -> String {
    let chars: Vec<char> = did.chars().collect();
    if chars.len() <= 24 {
        return did.to_string();
    }
    let head: String = chars[..14].iter().collect();
    let tail: String = chars[chars.len() - 6..].iter().collect();
    format!("{head}…{tail}")
}

/// A throwaway `did:key` signer, for this crate's unit tests.
#[cfg(test)]
pub(crate) fn test_signer(kind: AuthorKind) -> Signer {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("OS RNG");
    let public = ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .to_bytes();
    let did = format!(
        "did:key:{}",
        vta_sdk::did_key::ed25519_multibase_pubkey(&public)
    );
    let private = multibase::encode(multibase::Base::Base58Btc, seed);
    Signer::from_did_key(&did, &private, kind).expect("a did:key signer")
}
