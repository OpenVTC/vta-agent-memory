//! What kind of claim a memory is (#32).
//!
//! A memory's type says what it is *about*; its basis says *how anybody came to
//! believe it*. Without one, a behaviour inferred once ("prefers short
//! answers") is indistinguishable from a preference the user stated, and is
//! reused across every later session with the same weight.
//!
//! The vocabulary is the one Data Rooms publish for a record's provenance
//! (`RecordBasis`, `confidence`, `evidence` in `rooms/records/put/0.5`), so a
//! record moved between a person's memory and a room keeps its meaning:
//!
//! - **`declared`** — the user's own statement: a decision, a preference, an
//!   intention. True because they said so.
//! - **`observed`** — something seen happen or measured, recorded as found.
//! - **`verified`** — checked against evidence somebody else can check again,
//!   such as a credential. Must cite that evidence; a `verified` record that
//!   cites none is an unsupported claim to have checked, and is read as one.
//! - **`inferred`** — a conclusion: an agent's reading of the situation, a
//!   generalisation, a guess. What an agent writes when the user did not say it.
//!
//! All three fields sit inside the record's signature, so they cannot be
//! upgraded after the fact. They are still the *author's* account: nobody
//! checks them, and `inferred` can be right while `observed` is wrong.
//!
//! # Confidence is shown, not ranked
//!
//! `confidence` is the author's own estimate, 0 to 1. Two writers' numbers are
//! not on one scale — the rooms schema says a reader MUST NOT rank by it across
//! authors — so recall returns it and never sorts on it. [`Basis`] is what
//! ranking weighs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::record::MemoryKey;

/// Most `evidence` items one memory can cite.
pub const MAX_EVIDENCE: usize = 8;
/// Longest single identifier inside an evidence item, in characters.
pub const MAX_EVIDENCE_ID_CHARS: usize = 200;

/// What kind of claim a memory is, as its author states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Basis {
    /// The user said it.
    Declared,
    /// It was seen to happen.
    Observed,
    /// Checked against evidence that is cited.
    Verified,
    /// A conclusion drawn from other information.
    Inferred,
}

impl Basis {
    /// The wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Basis::Declared => "declared",
            Basis::Observed => "observed",
            Basis::Verified => "verified",
            Basis::Inferred => "inferred",
        }
    }

    /// Parse the wire form; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "declared" => Some(Basis::Declared),
            "observed" => Some(Basis::Observed),
            "verified" => Some(Basis::Verified),
            "inferred" => Some(Basis::Inferred),
            _ => None,
        }
    }
}

/// How much a memory's basis counts in ranking, out of [`FULL_WEIGHT`].
///
/// A nudge, not an override: relevance is multiplied by it, so a stated
/// preference outranks an inference *equally relevant to the query*, but a
/// strong match on an inference still beats a passing mention of a stated fact.
/// `basis` is `None` for a record that predates provenance, or names a basis
/// this version does not know — shown as `unstated`, weighted between observed
/// and inferred. `has_evidence` matters only to `verified`, which without
/// evidence is an unsupported claim and weighs as an inference.
pub fn weight(basis: Option<Basis>, has_evidence: bool) -> u32 {
    match basis {
        Some(Basis::Declared) => FULL_WEIGHT,
        Some(Basis::Verified) if has_evidence => FULL_WEIGHT,
        Some(Basis::Observed) => 9,
        None => 8,
        Some(Basis::Inferred) | Some(Basis::Verified) => 6,
    }
}

/// The weight of a declared or evidenced memory.
pub const FULL_WEIGHT: u32 = 10;

/// One thing a memory cites in support of itself.
///
/// The `credential` and `record` shapes are the rooms schema's
/// (`RecordEvidenceCredential`, `RecordEvidenceRecord`); `memory` is the
/// personal-memory counterpart of a same-room `record` citation. All of them
/// are **identifiers, never locators**: nothing here fetches what a memory
/// cites, so citing cannot be used to make a reader reach out to an address of
/// the author's choosing. What a citation proves is only that the author cited
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Evidence {
    /// A verifiable credential, by its `id`.
    #[serde(rename_all = "camelCase")]
    Credential {
        /// The credential's `id`.
        credential_id: String,
        /// Optional: the SHA-256 multihash of the credential's JCS form, so a
        /// reader holding a credential with this id can tell it is the one meant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        digest: Option<String>,
    },
    /// Another memory in this trust context, by key (`<type>/<slug>`).
    Memory {
        /// The cited memory's key.
        key: String,
    },
    /// A record in a data room.
    #[serde(rename_all = "camelCase")]
    Record {
        /// The room's identifier.
        room_id: String,
        /// The record's key within the room.
        key: String,
    },
}

impl Evidence {
    /// Check one item a caller supplied, naming what is wrong with it.
    fn check(&self) -> Result<(), String> {
        let ids: Vec<(&str, &str)> = match self {
            Evidence::Credential {
                credential_id,
                digest,
            } => {
                let mut v = vec![("credentialId", credential_id.as_str())];
                if let Some(d) = digest {
                    v.push(("digest", d.as_str()));
                }
                v
            }
            Evidence::Memory { key } => {
                MemoryKey::parse(key).map_err(|e| format!("evidence cites {e}"))?;
                vec![("key", key.as_str())]
            }
            Evidence::Record { room_id, key } => {
                vec![("roomId", room_id.as_str()), ("key", key.as_str())]
            }
        };
        for (field, id) in ids {
            if id.trim().is_empty() {
                return Err(format!("evidence `{field}` is empty"));
            }
            let n = id.chars().count();
            if n > MAX_EVIDENCE_ID_CHARS {
                return Err(format!(
                    "evidence `{field}` is {n} characters; the limit is {MAX_EVIDENCE_ID_CHARS}"
                ));
            }
        }
        Ok(())
    }
}

/// Check the provenance a caller supplied with a save.
///
/// Enforced where a record comes in, like the size limits: a record stored
/// before these rules, or by a later version, stays readable.
pub fn check(basis: Basis, confidence: Option<f64>, evidence: &[Evidence]) -> Result<(), String> {
    if let Some(c) = confidence
        && !(c.is_finite() && (0.0..=1.0).contains(&c))
    {
        return Err(format!("`confidence` is {c}; it must be between 0 and 1"));
    }
    if evidence.len() > MAX_EVIDENCE {
        return Err(format!(
            "{} `evidence` items; the limit is {MAX_EVIDENCE}",
            evidence.len()
        ));
    }
    for e in evidence {
        e.check()?;
    }
    if basis == Basis::Verified && evidence.is_empty() {
        return Err(
            "`basis: verified` must cite what it was checked against in `evidence` — a \
             credential, another memory, or a room record. If nothing can be cited, the \
             basis is `observed` or `declared`, not `verified`."
                .to_string(),
        );
    }
    Ok(())
}

/// The provenance fields recall and get return for a stored record.
///
/// `evidence` is passed through as stored — only the count is checked here —
/// so a citation kind a later version adds is shown rather than dropped.
pub fn to_json(basis: Option<&str>, confidence: Option<f64>, evidence: &[Value]) -> Value {
    let parsed = basis.and_then(Basis::parse);
    let mut v = serde_json::json!({
        "basis": parsed.map(|b| b.as_str()).unwrap_or("unstated"),
    });
    if let Some(c) = confidence {
        v["confidence"] = serde_json::json!(c);
    }
    if parsed == Some(Basis::Verified) && evidence.is_empty() {
        v["basisNote"] = serde_json::json!("claims to be verified but cites no evidence");
    }
    v
}

/// A short note for a rendered listing: the basis, unless it is `declared`.
pub fn note(basis: Option<&str>, confidence: Option<f64>, has_evidence: bool) -> Option<String> {
    let parsed = basis.and_then(Basis::parse);
    let mut s = match parsed {
        Some(Basis::Declared) => return None,
        Some(Basis::Verified) if !has_evidence => "verified, no evidence cited".to_string(),
        Some(b) => b.as_str().to_string(),
        None => "basis unstated".to_string(),
    };
    if let Some(c) = confidence {
        s.push_str(&format!(", confidence {c:.2}"));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verified_must_cite_something() {
        assert!(check(Basis::Verified, None, &[]).is_err());
        let cited = [Evidence::Credential {
            credential_id: "urn:uuid:1".into(),
            digest: None,
        }];
        assert!(check(Basis::Verified, None, &cited).is_ok());
    }

    #[test]
    fn confidence_is_a_probability() {
        for bad in [-0.1, 1.5, f64::NAN, f64::INFINITY] {
            assert!(check(Basis::Inferred, Some(bad), &[]).is_err(), "{bad}");
        }
        for ok in [0.0, 0.5, 1.0] {
            assert!(check(Basis::Inferred, Some(ok), &[]).is_ok(), "{ok}");
        }
    }

    #[test]
    fn evidence_is_bounded_and_well_formed() {
        let many = vec![
            Evidence::Memory {
                key: "user/x".into()
            };
            MAX_EVIDENCE + 1
        ];
        assert!(check(Basis::Observed, None, &many).is_err());
        let bad_key = [Evidence::Memory {
            key: "nonsense".into(),
        }];
        assert!(check(Basis::Observed, None, &bad_key).is_err());
        let empty = [Evidence::Record {
            room_id: " ".into(),
            key: "k".into(),
        }];
        assert!(check(Basis::Observed, None, &empty).is_err());
    }

    #[test]
    fn evidence_has_the_rooms_wire_shape() {
        let e = Evidence::Credential {
            credential_id: "urn:uuid:1".into(),
            digest: None,
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({ "kind": "credential", "credentialId": "urn:uuid:1" })
        );
        let r = Evidence::Record {
            room_id: "did:web:room".into(),
            key: "k".into(),
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "kind": "record", "roomId": "did:web:room", "key": "k" })
        );
    }

    #[test]
    fn inference_weighs_less_than_a_statement_and_unsupported_verification_is_inference() {
        assert!(weight(Some(Basis::Inferred), false) < weight(Some(Basis::Declared), false));
        assert_eq!(
            weight(Some(Basis::Verified), false),
            weight(Some(Basis::Inferred), false)
        );
        assert_eq!(weight(Some(Basis::Verified), true), FULL_WEIGHT);
        let unstated = weight(None, false);
        assert!(weight(Some(Basis::Inferred), false) < unstated);
        assert!(unstated < weight(Some(Basis::Observed), false));
    }

    #[test]
    fn projection_says_unstated_rather_than_guessing() {
        assert_eq!(to_json(None, None, &[])["basis"], "unstated");
        assert_eq!(to_json(Some("rumour"), None, &[])["basis"], "unstated");
        let v = to_json(Some("verified"), None, &[]);
        assert!(v["basisNote"].as_str().unwrap().contains("no evidence"));
        assert_eq!(to_json(Some("inferred"), Some(0.4), &[])["confidence"], 0.4);
    }
}
