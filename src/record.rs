//! The memory record: what goes in the VTA's `value`, and how recall finds it
//! again.
//!
//! # Why there is a record shape at all
//!
//! `vta/memory/{put,list,delete}/0.1` stores a `(contextId, key) -> value`
//! string and nothing else. `list` returns **every entry in the context, in
//! ascending key order — no prefix, no cursor, no search** (see
//! `docs/05-design-notes/appstate-store.md` upstream, which cites exactly this
//! as why application state got its own family). So every affordance a memory
//! service needs — a type, a one-line summary to rank on, links between
//! entries, a timestamp — has to be carried *inside* the value, and retrieval
//! has to happen on this side of the wire.
//!
//! Two consequences shape everything below:
//!
//! 1. **The key is structured**, `<type>/<slug>`, so a prefix filter can do the
//!    work the store won't. [`MemoryKey`] owns that encoding.
//! 2. **Recall ranks on `description`, not `body`.** A context of a few hundred
//!    memories is a few hundred bodies; returning them all is how a memory
//!    service quietly eats the caller's context window. [`rank`] scores against
//!    name and description, and the caller fetches bodies for the handful it
//!    actually wants.
//!
//! # What this is not
//!
//! Not durable application state. A user asking an agent to forget everything
//! must be a safe thing to do, which it stops being the moment account state
//! lives here. That belongs in `vta/app-state/*/1.0`, which is versioned,
//! namespaced, and has a change feed.

use crate::fence::Fence;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Wire version of [`MemoryRecord`]. Bump only for a shape change readers
/// cannot absorb; new optional fields do not need it.
pub const RECORD_VERSION: u8 = 1;

/// What kind of thing a memory is. Deliberately the same four-way split Claude
/// Code's own file-based memory uses, so a project moving onto the VTA keeps
/// its taxonomy (and its habits about what is worth saving).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// Who the user is — role, expertise, standing preferences.
    User,
    /// Guidance the user has given on how to work. Carries a why.
    Feedback,
    /// Ongoing work, goals, constraints not derivable from the code.
    Project,
    /// Pointers to external resources — URLs, dashboards, tickets.
    Reference,
}

impl MemoryType {
    /// Every variant, in the order they are presented to a caller.
    pub const ALL: [MemoryType; 4] = [
        MemoryType::User,
        MemoryType::Feedback,
        MemoryType::Project,
        MemoryType::Reference,
    ];

    /// The lowercase wire/key form.
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::User => "user",
            MemoryType::Feedback => "feedback",
            MemoryType::Project => "project",
            MemoryType::Reference => "reference",
        }
    }

    /// Parse from the wire/key form. Case-insensitive, since it arrives from a
    /// model as often as from a config file.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Some(MemoryType::User),
            "feedback" => Some(MemoryType::Feedback),
            "project" => Some(MemoryType::Project),
            "reference" => Some(MemoryType::Reference),
            _ => None,
        }
    }
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A storage key, `<type>/<slug>`.
///
/// The `/` is not decoration. `memory/list` has no prefix parameter, but the
/// *client* can filter on one, and a key that carries its type means filtering
/// by type costs no extra round trip and no extra field to keep consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryKey {
    pub kind: MemoryType,
    pub slug: String,
}

impl MemoryKey {
    /// Build a key, normalising `name` into a slug.
    pub fn new(kind: MemoryType, name: &str) -> Result<Self, KeyError> {
        let slug = slugify(name);
        if slug.is_empty() {
            return Err(KeyError::EmptySlug(name.to_string()));
        }
        Ok(Self { kind, slug })
    }

    /// Parse a stored key back into its parts.
    pub fn parse(raw: &str) -> Result<Self, KeyError> {
        let (kind, slug) = raw
            .split_once('/')
            .ok_or_else(|| KeyError::Malformed(raw.to_string()))?;
        let kind =
            MemoryType::parse(kind).ok_or_else(|| KeyError::UnknownType(kind.to_string()))?;
        if slug.is_empty() {
            return Err(KeyError::EmptySlug(raw.to_string()));
        }
        Ok(Self {
            kind,
            slug: slug.to_string(),
        })
    }

    /// The `<type>/` prefix matching every key of one type — and *only* that
    /// type: the trailing `/` is what stops `user` from also matching a
    /// hypothetical `userdata`. Kept as the single definition of the prefix so
    /// a caller filtering raw keys (before decoding them) agrees with
    /// [`MemoryKey::parse`] by construction.
    pub fn prefix(kind: MemoryType) -> String {
        format!("{}/", kind.as_str())
    }

    /// Whether a raw, still-undecoded storage key belongs to `kind`.
    pub fn raw_key_has_type(raw: &str, kind: MemoryType) -> bool {
        raw.starts_with(&Self::prefix(kind))
    }
}

impl fmt::Display for MemoryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.kind.as_str(), self.slug)
    }
}

/// Why a key could not be built or read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// The name reduced to nothing once slugified (e.g. all punctuation).
    EmptySlug(String),
    /// No `/` separator.
    Malformed(String),
    /// The part before `/` is not a known [`MemoryType`].
    UnknownType(String),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::EmptySlug(n) => write!(
                f,
                "`{n}` contains no characters usable in a memory name (letters, digits, `-`)"
            ),
            KeyError::Malformed(k) => write!(
                f,
                "memory key `{k}` is not `<type>/<name>`; types are: user, feedback, project, reference"
            ),
            KeyError::UnknownType(t) => write!(
                f,
                "unknown memory type `{t}`; expected one of: user, feedback, project, reference"
            ),
        }
    }
}

impl std::error::Error for KeyError {}

/// Lowercase, hyphen-separated, `[a-z0-9-]` only, no leading/trailing or
/// repeated hyphens.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_sep = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            // Any run of non-alphanumerics collapses to a single separator,
            // and a trailing run produces none at all.
            pending_sep = true;
        }
    }
    out
}

/// One memory, as stored in the VTA's opaque `value` string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    /// Wire version. See [`RECORD_VERSION`].
    pub v: u8,
    /// Human-facing name. The slug in the key is derived from this; keeping the
    /// original means display never has to un-slugify.
    pub name: String,
    /// What kind of memory this is. Duplicated from the key deliberately: a
    /// record read on its own is still self-describing.
    #[serde(rename = "type")]
    pub kind: MemoryType,
    /// One line. **This is what recall ranks and returns** — write it as the
    /// answer to "would I want this loaded right now?".
    pub description: String,
    /// The memory itself.
    pub body: String,
    /// Names of related memories. Free-form: a link to a memory that does not
    /// exist yet marks something worth writing, it is not an error.
    #[serde(default)]
    pub links: Vec<String>,
    /// RFC 3339, set on every write.
    pub updated_at: String,
}

impl MemoryRecord {
    /// Build a record, stamping `updated_at` now.
    pub fn new(
        kind: MemoryType,
        name: impl Into<String>,
        description: impl Into<String>,
        body: impl Into<String>,
        links: Vec<String>,
    ) -> Self {
        Self {
            v: RECORD_VERSION,
            name: name.into(),
            kind,
            description: description.into(),
            body: body.into(),
            links,
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Decode a stored value.
    ///
    /// Values written by something other than this tool — or by hand — are not
    /// an error: they come back as a `Reference` record whose body is the raw
    /// string, so a mixed context stays listable and forgettable rather than
    /// failing the whole `list`. The alternative, erroring, means one stray
    /// entry makes every recall fail.
    pub fn decode(key: &MemoryKey, value: &str) -> Self {
        match serde_json::from_str::<MemoryRecord>(value) {
            Ok(r) => r,
            Err(_) => Self {
                v: 0,
                name: key.slug.clone(),
                kind: key.kind,
                description: first_line(value),
                body: value.to_string(),
                links: Vec::new(),
                updated_at: String::new(),
            },
        }
    }

    /// Encode for storage.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// The compact form recall returns: enough to decide, not enough to cost.
    /// The compact form `memory_recall` returns.
    ///
    /// Author-supplied strings are passed through [`Fence::sanitize`]: a JSON
    /// field is still text once a model reads it, so a `description` carrying
    /// a delimiter shape could otherwise appear to close the fence the caller
    /// wrapped this payload in. Sanitizing at the projection means every
    /// consumer of `summary`/`full` inherits it. (F8.)
    pub fn summary(&self, key: &MemoryKey) -> serde_json::Value {
        serde_json::json!({
            "key": key.to_string(),
            "name": Fence::sanitize(&self.name),
            "type": self.kind.as_str(),
            "description": Fence::sanitize(&self.description),
            "links": self.links,
            "updatedAt": self.updated_at,
            // Stated on every projection so a reader never has to infer it.
            "trust": "untrusted-data",
        })
    }

    /// The full form `memory_get` returns.
    pub fn full(&self, key: &MemoryKey) -> serde_json::Value {
        let mut v = self.summary(key);
        v["body"] = serde_json::Value::String(Fence::sanitize(&self.body));
        v
    }
}

/// First line of `s`, trimmed and capped — a stand-in description for a value
/// this tool did not write.
fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > 120 {
        let cut: String = line.chars().take(117).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// One entry as it came back from the VTA.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: MemoryKey,
    pub record: MemoryRecord,
}

/// A ranked recall hit.
#[derive(Debug, Clone)]
pub struct Hit {
    pub entry: Entry,
    pub score: u32,
}

/// Rank `entries` against a free-text query.
///
/// Scoring is deliberately dumb — token containment, weighted by field — and
/// that is a decision, not a shortcut. The alternative (embeddings) needs a
/// model, a vector index, and somewhere to keep it; the VTA offers none of the
/// three, and a scoring function nobody can predict is worse than a crude one
/// everybody can. Weights: name 4, description 2, body 1, so a memory *about*
/// the query outranks one that merely mentions it.
///
/// An empty query ranks everything equally, which makes recall degrade into
/// list — the right behaviour for a session-start hook that just wants what is
/// there. Ties break on `updated_at` descending, then key, so output is stable.
pub fn rank(entries: Vec<Entry>, query: &str) -> Vec<Hit> {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| t.to_ascii_lowercase())
        .collect();

    let mut hits: Vec<Hit> = entries
        .into_iter()
        .map(|entry| {
            let score = if terms.is_empty() {
                0
            } else {
                let name = entry.record.name.to_ascii_lowercase();
                let desc = entry.record.description.to_ascii_lowercase();
                let body = entry.record.body.to_ascii_lowercase();
                terms
                    .iter()
                    .map(|t| {
                        let mut s = 0;
                        if name.contains(t) {
                            s += 4;
                        }
                        if desc.contains(t) {
                            s += 2;
                        }
                        if body.contains(t) {
                            s += 1;
                        }
                        s
                    })
                    .sum()
            };
            Hit { entry, score }
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.entry.record.updated_at.cmp(&a.entry.record.updated_at))
            .then_with(|| a.entry.key.to_string().cmp(&b.entry.key.to_string()))
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: MemoryType, name: &str, desc: &str, body: &str) -> Entry {
        Entry {
            key: MemoryKey::new(kind, name).unwrap(),
            record: MemoryRecord::new(kind, name, desc, body, vec![]),
        }
    }

    #[test]
    fn slugify_collapses_punctuation_runs() {
        assert_eq!(slugify("Cierge  Gateway!! (v2)"), "cierge-gateway-v2");
        assert_eq!(slugify("--leading and trailing--"), "leading-and-trailing");
        assert_eq!(slugify("already-fine"), "already-fine");
    }

    #[test]
    fn a_name_with_no_usable_characters_is_refused() {
        // Better here than as an unaddressable `feedback/` key in the store.
        assert!(matches!(
            MemoryKey::new(MemoryType::Feedback, "!!! ???"),
            Err(KeyError::EmptySlug(_))
        ));
    }

    #[test]
    fn keys_round_trip() {
        let k = MemoryKey::new(MemoryType::Project, "Cierge Gateway").unwrap();
        assert_eq!(k.to_string(), "project/cierge-gateway");
        assert_eq!(MemoryKey::parse("project/cierge-gateway").unwrap(), k);
    }

    #[test]
    fn key_errors_name_the_valid_types() {
        assert!(
            MemoryKey::parse("no-slash")
                .unwrap_err()
                .to_string()
                .contains("user, feedback, project, reference")
        );
        assert!(
            MemoryKey::parse("nonsense/x")
                .unwrap_err()
                .to_string()
                .contains("unknown memory type")
        );
    }

    #[test]
    fn type_prefix_is_exact() {
        // `user/` must not be a prefix of anything else; the trailing slash is
        // what guarantees it.
        assert_eq!(MemoryKey::prefix(MemoryType::User), "user/");
        assert!(!"userdata/x".starts_with(&MemoryKey::prefix(MemoryType::User)));
    }

    #[test]
    fn records_round_trip_through_the_value_string() {
        let r = MemoryRecord::new(
            MemoryType::Feedback,
            "No PR attribution",
            "Don't add Claude trailers to commits",
            "Because the commit log is the team's, not the tool's.",
            vec!["cierge-use-prs".into()],
        );
        let key = MemoryKey::new(MemoryType::Feedback, &r.name).unwrap();
        let back = MemoryRecord::decode(&key, &r.encode().unwrap());
        assert_eq!(back.name, r.name);
        assert_eq!(back.kind, MemoryType::Feedback);
        assert_eq!(back.links, vec!["cierge-use-prs".to_string()]);
        assert_eq!(back.v, RECORD_VERSION);
    }

    #[test]
    fn a_foreign_value_decodes_instead_of_failing() {
        // One entry written by another tool must not break `list` for the rest.
        let key = MemoryKey::parse("reference/hand-written").unwrap();
        let r = MemoryRecord::decode(&key, "just some text\nmore text");
        assert_eq!(r.v, 0, "flagged as not written by this tool");
        assert_eq!(r.description, "just some text");
        assert_eq!(r.body, "just some text\nmore text");
    }

    #[test]
    fn a_long_foreign_value_gets_a_truncated_description() {
        let key = MemoryKey::parse("reference/long").unwrap();
        let r = MemoryRecord::decode(&key, &"x".repeat(500));
        assert_eq!(r.description.chars().count(), 118, "117 + ellipsis");
        assert!(r.description.ends_with('…'));
        assert_eq!(r.body.len(), 500, "the body is never truncated");
    }

    #[test]
    fn a_name_match_outranks_a_body_mention() {
        let hits = rank(
            vec![
                entry(
                    MemoryType::Project,
                    "Unrelated",
                    "other",
                    "mentions gateway",
                ),
                entry(MemoryType::Project, "Gateway", "the thing", "unrelated"),
            ],
            "gateway",
        );
        assert_eq!(hits[0].entry.record.name, "Gateway");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn an_empty_query_keeps_everything_at_equal_score() {
        // Recall degrades into list, which is what a session-start hook wants.
        let hits = rank(
            vec![
                entry(MemoryType::User, "A", "a", "a"),
                entry(MemoryType::User, "B", "b", "b"),
            ],
            "",
        );
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.score == 0));
    }

    #[test]
    fn single_character_query_tokens_are_dropped() {
        // "a gateway" must not score every memory containing the letter a.
        let hits = rank(
            vec![entry(MemoryType::Project, "Zeta", "a thing", "a body")],
            "a",
        );
        assert_eq!(hits[0].score, 0);
    }

    #[test]
    fn ranking_is_stable_for_equal_scores() {
        let a = entry(MemoryType::User, "A", "x", "x");
        let b = entry(MemoryType::User, "B", "x", "x");
        let first = rank(vec![a.clone(), b.clone()], "nomatch");
        let second = rank(vec![b, a], "nomatch");
        assert_eq!(
            first[0].entry.key.to_string(),
            second[0].entry.key.to_string()
        );
    }
}
