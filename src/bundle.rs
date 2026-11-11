//! A signed, verifiable bundle of memories (#36): what `export` writes and
//! `import` reads.
//!
//! Memory is portable across AI providers because it lives in the user's VTA;
//! a bundle makes it portable *out of* one — to another VTA, another context,
//! an archive, or a system that is not a VTA at all. The format is documented
//! for those readers in `docs/memory-bundle.md`; this module is its one
//! implementation.
//!
//! # Two layers of signature
//!
//! - **Each record keeps its author's own proof**, verbatim, as it was stored.
//!   Exporting does not re-author anything: a receiver can check every memory
//!   against the DID that wrote it, independently of who exported it.
//! - **The bundle carries the exporter's proof** over all of it — the record
//!   list, the source context and the filter. That is what makes a bundle a
//!   unit: a record dropped from it, added to it, or moved between bundles
//!   fails the bundle's proof even though each record's own proof still holds.
//!
//! Opening a bundle checks both, and is all or nothing: a bundle with one bad
//! record is refused whole, because a bundle somebody tampered with is not one
//! to take any part of on trust.
//!
//! # What is not exported
//!
//! Records that are refused (their signature does not hold) and records that
//! are unattributed (written before signing existed) — neither has an author
//! a receiver could check. The export report lists them; re-saving an
//! unattributed memory signs it. History is not exported either: a bundle is
//! the current version of each memory, which is what a receiver can use.

use serde_json::{Map, Value, json};
use vta_sdk::trust_task_proof::{ProofPurpose, TrustTaskVmResolver, verify_proof_set};
use vta_sdk::trust_task_sign::sign_object_for_assertion;

use crate::authorship::{Authorship, Signer, verify};
use crate::lifecycle::Status;
use crate::record::{MemoryKey, MemoryRecord, MemoryType};

/// The bundle's `type`.
pub const BUNDLE_TYPE: &str = "VtaAgentMemoryBundle";
/// The bundle format version this crate writes and reads.
pub const BUNDLE_VERSION: u64 = 1;
/// Largest bundle `import` reads, in bytes. A context's memories are bounded
/// notes; a bundle far past this is not one this crate wrote.
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024 * 1024;

/// Which memories to carry: each list narrows, an empty list does not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Memory types.
    pub types: Vec<MemoryType>,
    /// Bases — `declared`, `observed`, `verified`, `inferred`, or `unstated`
    /// for a record that names none.
    pub basis: Vec<String>,
    /// Statuses — `active`, `deprecated`, `disputed`, `retracted`.
    pub status: Vec<Status>,
}

impl Filter {
    /// Build a filter from the words a person typed, naming the first one that
    /// is not a type, basis or status.
    pub fn parse(types: &[String], basis: &[String], status: &[String]) -> Result<Self, String> {
        let split = |items: &[String]| -> Vec<String> {
            items
                .iter()
                .flat_map(|s| s.split(','))
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let types = split(types)
            .into_iter()
            .map(|t| {
                MemoryType::parse(&t).ok_or_else(|| {
                    format!("unknown type `{t}`; expected user, feedback, project or reference")
                })
            })
            .collect::<Result<_, _>>()?;
        let basis = split(basis)
            .into_iter()
            .map(|b| match b.as_str() {
                "declared" | "observed" | "verified" | "inferred" | "unstated" => Ok(b),
                _ => Err(format!(
                    "unknown basis `{b}`; expected declared, observed, verified, inferred or \
                     unstated"
                )),
            })
            .collect::<Result<_, _>>()?;
        let status = split(status)
            .into_iter()
            .map(|s| {
                Status::parse(Some(&s)).ok_or_else(|| {
                    format!(
                        "unknown status `{s}`; expected active, deprecated, disputed or retracted"
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            types,
            basis,
            status,
        })
    }

    /// Whether `record` is carried.
    pub fn matches(&self, record: &MemoryRecord) -> bool {
        let basis = record.basis.as_deref().unwrap_or("unstated");
        (self.types.is_empty() || self.types.contains(&record.kind))
            && (self.basis.is_empty() || self.basis.iter().any(|b| b == basis))
            && (self.status.is_empty()
                || Status::parse(record.status.as_deref())
                    .is_some_and(|s| self.status.contains(&s)))
    }

    /// The `filter` member of a bundle: what it was narrowed to, so a receiver
    /// knows it is a subset.
    pub fn to_json(&self) -> Value {
        let mut v = Map::new();
        if !self.types.is_empty() {
            v.insert(
                "types".into(),
                json!(self.types.iter().map(|t| t.as_str()).collect::<Vec<_>>()),
            );
        }
        if !self.basis.is_empty() {
            v.insert("basis".into(), json!(self.basis));
        }
        if !self.status.is_empty() {
            v.insert(
                "status".into(),
                json!(self.status.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
            );
        }
        Value::Object(v)
    }
}

/// One memory as it goes into a bundle: its key, the version it was stored at,
/// and the stored record, verbatim.
#[derive(Debug, Clone)]
pub struct Item {
    pub key: MemoryKey,
    pub version: u64,
    pub record: Value,
}

/// Seal `items` from `source_context` into a bundle signed by `signer`.
pub async fn seal(
    signer: &Signer,
    source_context: &str,
    filter: &Filter,
    items: &[Item],
) -> anyhow::Result<Value> {
    let mut bundle = json!({
        "type": BUNDLE_TYPE,
        "bundleVersion": BUNDLE_VERSION,
        "exportedBy": signer.did(),
        "exportedAt": chrono::Utc::now().to_rfc3339(),
        "sourceContext": source_context,
        "filter": filter.to_json(),
        "records": items
            .iter()
            .map(|i| json!({ "key": i.key.to_string(), "version": i.version, "record": i.record }))
            .collect::<Vec<_>>(),
    });
    sign_object_for_assertion(&mut bundle, signer.holder_key())
        .await
        .map_err(|e| anyhow::anyhow!("signing the bundle: {e}"))?;
    Ok(bundle)
}

/// A bundle whose signatures all hold.
#[derive(Debug)]
pub struct Opened {
    /// The DID that signed the bundle.
    pub exported_by: String,
    pub exported_at: String,
    /// The context the memories were exported from.
    pub source_context: String,
    /// The filter the exporter applied.
    pub filter: Value,
    /// Each memory, checked against its own author.
    pub memories: Vec<Memory>,
}

/// One memory from an opened bundle.
#[derive(Debug)]
pub struct Memory {
    pub key: MemoryKey,
    /// The record as stored in the source context.
    pub record: MemoryRecord,
    /// Who wrote it — the original author, also for a record that was itself
    /// imported into the source context.
    pub authorship: Authorship,
    /// The original as its author signed it — what an import carries as
    /// `importedFrom`. For a record imported into the source context, that
    /// import's own original: imports flatten rather than nest.
    pub origin: Value,
}

/// Check a bundle: its type and version, the exporter's proof, and every
/// record's own proof at the key and context it names. `Err` with why, for the
/// whole bundle — see the module docs for why it is all or nothing.
pub async fn open(raw: &str) -> Result<Opened, String> {
    if raw.len() > MAX_BUNDLE_BYTES {
        return Err(format!(
            "the bundle is {} bytes; the limit is {MAX_BUNDLE_BYTES}",
            raw.len()
        ));
    }
    let bundle: Value =
        serde_json::from_str(raw).map_err(|e| format!("the bundle is not JSON ({e})"))?;
    let field = |name: &str| bundle.get(name).and_then(Value::as_str);
    if field("type") != Some(BUNDLE_TYPE) {
        return Err(format!(
            "it is not a memory bundle (`type` is not `{BUNDLE_TYPE}`)"
        ));
    }
    match bundle.get("bundleVersion").and_then(Value::as_u64) {
        Some(BUNDLE_VERSION) => {}
        Some(v) => {
            return Err(format!(
                "it is bundle format {v}; this version reads format {BUNDLE_VERSION}"
            ));
        }
        None => return Err("it names no `bundleVersion`".to_string()),
    }

    if bundle.get("proof").is_none() {
        return Err("it is not signed, so nothing says who exported it".to_string());
    }
    let signer = verify_proof_set(
        &bundle,
        ProofPurpose::AssertionMethod,
        &TrustTaskVmResolver::did_key_only(),
    )
    .await
    .map_err(|e| {
        format!("its signature does not verify ({e}) — it was altered after it was exported")
    })?
    .into_signer();
    let exported_by = field("exportedBy").unwrap_or_default();
    if exported_by != signer {
        return Err(format!(
            "it claims to be exported by `{exported_by}` but was signed by `{signer}`"
        ));
    }
    let source_context = field("sourceContext")
        .ok_or("it names no `sourceContext`")?
        .to_string();
    let records = bundle
        .get("records")
        .and_then(Value::as_array)
        .ok_or("it has no `records` list")?;

    let mut memories = Vec::with_capacity(records.len());
    let mut seen = std::collections::HashSet::new();
    for (i, item) in records.iter().enumerate() {
        let memory = open_item(item, &source_context)
            .await
            .map_err(|why| format!("record {i} is refused: {why}"))?;
        if !seen.insert(memory.key.to_string()) {
            return Err(format!("it holds `{}` twice", memory.key));
        }
        memories.push(memory);
    }

    Ok(Opened {
        exported_by: signer,
        exported_at: field("exportedAt").unwrap_or_default().to_string(),
        source_context,
        filter: bundle.get("filter").cloned().unwrap_or(Value::Null),
        memories,
    })
}

async fn open_item(item: &Value, source_context: &str) -> Result<Memory, String> {
    let raw_key = item
        .get("key")
        .and_then(Value::as_str)
        .ok_or("it names no `key`")?;
    let key = MemoryKey::parse(raw_key).map_err(|e| e.to_string())?;
    let record_value = item.get("record").ok_or("it has no `record`")?;
    let stored = serde_json::to_string(record_value).map_err(|e| e.to_string())?;

    let authorship = verify(&stored, source_context, raw_key, None)
        .await
        .map_err(|why| format!("`{key}`: {why}"))?;
    if authorship == Authorship::Unattributed {
        return Err(format!(
            "`{key}` is not signed, so nothing says who wrote it"
        ));
    }
    let record: MemoryRecord = serde_json::from_value(record_value.clone())
        .map_err(|e| format!("`{key}` is not a memory record ({e})"))?;
    // Where an import will write it: the key its own name and type address.
    if MemoryKey::new(record.kind, &record.name).ok().as_ref() != Some(&key) {
        return Err(format!(
            "`{key}` holds a record named `{}`, which is stored elsewhere",
            record.name
        ));
    }
    record
        .check_limits()
        .map_err(|why| format!("`{key}`: {why}"))?;
    let origin = record
        .imported_from
        .clone()
        .unwrap_or_else(|| record_value.clone());
    Ok(Memory {
        key,
        record,
        authorship,
        origin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_parses_commas_and_repeats_and_names_a_bad_word() {
        let f = Filter::parse(
            &["user,feedback".into()],
            &["declared".into(), "unstated".into()],
            &["active".into()],
        )
        .unwrap();
        assert_eq!(f.types, vec![MemoryType::User, MemoryType::Feedback]);
        assert_eq!(f.basis, vec!["declared", "unstated"]);
        assert_eq!(f.status, vec![Status::Active]);

        let err = Filter::parse(&[], &["guessed".into()], &[]).unwrap_err();
        assert!(err.contains("guessed"), "{err}");
    }

    #[test]
    fn a_filter_narrows_on_every_list_it_has() {
        let mut r = MemoryRecord::new(MemoryType::User, "n", "d", "b", vec![]);
        assert!(Filter::default().matches(&r));
        let declared = Filter::parse(&[], &["declared".into()], &[]).unwrap();
        assert!(!declared.matches(&r), "no basis is `unstated`");
        r.basis = Some("declared".into());
        assert!(declared.matches(&r));
        let retracted = Filter::parse(&[], &[], &["retracted".into()]).unwrap();
        assert!(!retracted.matches(&r), "no status is active");
        let projects = Filter::parse(&["project".into()], &[], &[]).unwrap();
        assert!(!projects.matches(&r));
    }
}
