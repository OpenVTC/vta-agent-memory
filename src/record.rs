//! The memory record: what goes in the VTA's `value`, and how recall finds it
//! again.
//!
//! # Why there is a record shape at all
//!
//! `vta/memory/*` (put/delete/list 0.2, get 0.1) stores a `(contextId, key) -> value`
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

use crate::authorship::Authorship;
use crate::fence::Fence;
use crate::lifecycle::Standing;
use crate::provenance::{self, Basis, Evidence};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Wire version of [`MemoryRecord`]. Bump only for a shape change readers
/// cannot absorb; new optional fields do not need it.
///
/// 2 is the signed record (#31): it names its `author` and the `(contextId,
/// key)` it was written to, and carries a `proof` over all of it. A reader
/// treats a v2 record *without* a proof as tampered with, which is why this is
/// a version and not just new optional fields — see [`crate::authorship`].
pub const RECORD_VERSION: u8 = 2;

// Limits on a memory a caller saves. A memory is a note, not a document, and
// whatever one save stores can be pasted into every later session's context,
// so these bound that. They are enforced where a record comes in
// (`memory_save`), not on decode: an entry stored before the limits existed, or
// by another tool, stays readable and forgettable.

/// Longest `name`, in characters.
pub const MAX_NAME_CHARS: usize = 120;
/// Longest `description`, in characters. Recall returns it, so it should be one
/// line, not a paragraph.
pub const MAX_DESCRIPTION_CHARS: usize = 300;
/// Largest `body`, in bytes (16 KiB).
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// Most `links` one memory can carry.
pub const MAX_LINKS: usize = 32;
/// Longest single link, in characters.
pub const MAX_LINK_CHARS: usize = 120;

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

    // ---- provenance (#32): what kind of claim this is. See `crate::provenance`.
    // Strings and raw values, not the typed forms, so a basis or citation kind a
    // later version adds does not make the whole record undecodable; the types
    // are enforced where a record comes in (`check_limits`).
    /// `declared`, `observed`, `verified` or `inferred`. Absent on records that
    /// predate provenance, which read as `unstated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    /// The author's own estimate, 0 to 1, that this is correct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// What it cites in support: [`crate::provenance::Evidence`] items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<serde_json::Value>,

    // ---- lifecycle (#33): the record's standing. See `crate::lifecycle`.
    /// `active` (absent means active), `deprecated`, `disputed`, `retracted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Why the status is what it is — required for anything but active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// RFC 3339. After it the memory has expired and recall leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<String>,
    /// RFC 3339. After it the memory is flagged for review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_after: Option<String>,
    /// `{ "key": <history key>, "version": n }` — the version this one
    /// replaced, kept verbatim under its history key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<serde_json::Value>,

    // ---- v2: authorship. Set by `authorship::Signer::sign`, never by a caller,
    // and only believed once `authorship::verify` has checked them against the
    // proof and the place the record was found.
    /// The writer's DID — the proof's signer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// `operator` or `agent`, as the writer stated it. A string, not an enum,
    /// so a kind a later version adds does not make the whole record undecodable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_kind: Option<String>,
    /// The trust context the record was written to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// The storage key the record was written to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The writer's Data Integrity proof over everything above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<serde_json::Value>,
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
            basis: None,
            confidence: None,
            evidence: Vec::new(),
            status: None,
            status_reason: None,
            valid_until: None,
            review_after: None,
            supersedes: None,
            author: None,
            author_kind: None,
            context_id: None,
            key: None,
            proof: None,
        }
    }

    /// State what kind of claim this is.
    pub fn with_provenance(
        mut self,
        basis: Basis,
        confidence: Option<f64>,
        evidence: Vec<Evidence>,
    ) -> Self {
        self.basis = Some(basis.as_str().to_string());
        self.confidence = confidence;
        self.evidence = evidence
            .iter()
            .map(|e| serde_json::to_value(e).expect("evidence serialises"))
            .collect();
        self
    }

    /// The basis, if it is one this version knows.
    pub fn basis(&self) -> Option<Basis> {
        self.basis.as_deref().and_then(Basis::parse)
    }

    /// How much this record's basis counts in ranking.
    pub fn basis_weight(&self) -> u32 {
        provenance::weight(self.basis(), !self.evidence.is_empty())
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
                basis: None,
                confidence: None,
                evidence: Vec::new(),
                status: None,
                status_reason: None,
                valid_until: None,
                review_after: None,
                supersedes: None,
                author: None,
                author_kind: None,
                context_id: None,
                key: None,
                proof: None,
            },
        }
    }

    /// Encode for storage.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Check the caller-supplied fields against the `MAX_*` limits, naming the
    /// first limit exceeded.
    ///
    /// The one-line fields are measured in characters, so a name in a
    /// non-Latin script gets the same allowance. The body is measured in bytes,
    /// because what that limit bounds is how much text can reach a context
    /// window.
    pub fn check_limits(&self) -> Result<(), String> {
        let name = self.name.chars().count();
        if name > MAX_NAME_CHARS {
            return Err(format!(
                "`name` is {name} characters; the limit is {MAX_NAME_CHARS}"
            ));
        }
        let description = self.description.chars().count();
        if description > MAX_DESCRIPTION_CHARS {
            return Err(format!(
                "`description` is {description} characters; the limit is \
                 {MAX_DESCRIPTION_CHARS} — it should be one line"
            ));
        }
        if self.body.len() > MAX_BODY_BYTES {
            return Err(format!(
                "`body` is {} bytes; the limit is {MAX_BODY_BYTES} (16 KiB) — keep the \
                 essentials, or save a pointer to where the rest lives",
                self.body.len()
            ));
        }
        if self.links.len() > MAX_LINKS {
            return Err(format!(
                "{} `links`; the limit is {MAX_LINKS}",
                self.links.len()
            ));
        }
        if let Some((i, link)) = self
            .links
            .iter()
            .enumerate()
            .find(|(_, l)| l.chars().count() > MAX_LINK_CHARS)
        {
            return Err(format!(
                "link {} is {} characters; the limit for each link is {MAX_LINK_CHARS}",
                i + 1,
                link.chars().count()
            ));
        }
        // Provenance is checked only where it is stated. `memory_save` always
        // states it; a caller that does not is building a record by hand.
        if let Some(raw) = &self.basis {
            let basis = Basis::parse(raw).ok_or_else(|| {
                format!(
                    "unknown `basis` `{raw}`; expected declared, observed, verified or inferred"
                )
            })?;
            let evidence: Vec<Evidence> = self
                .evidence
                .iter()
                .map(|v| serde_json::from_value(v.clone()))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("malformed `evidence`: {e}"))?;
            provenance::check(basis, self.confidence, &evidence)?;
        }
        // Lifecycle: a status other than active must say why, and dates must
        // be dates (stored normalised, so they compare).
        let status = crate::lifecycle::Status::parse(self.status.as_deref()).ok_or_else(|| {
            format!(
                "unknown `status` `{}`",
                self.status.as_deref().unwrap_or("")
            )
        })?;
        match (&self.status_reason, status) {
            (Some(r), _) => crate::lifecycle::check_reason(r)?,
            (None, crate::lifecycle::Status::Active) => {}
            (None, s) => return Err(format!("a `{}` memory needs a reason", s.as_str())),
        }
        for (field, value) in [
            ("validUntil", &self.valid_until),
            ("reviewAfter", &self.review_after),
        ] {
            if let Some(v) = value
                && crate::lifecycle::normalise_date(field, v)? != *v
            {
                return Err(format!("`{field}` is not normalised RFC 3339: `{v}`"));
            }
        }
        Ok(())
    }

    /// The compact form `memory_recall` returns: enough to decide, not enough
    /// to cost.
    ///
    /// Author-supplied strings (name, description, links) are passed through
    /// [`Fence::sanitize`]: a JSON field is still text once a model reads it,
    /// so a field carrying a delimiter shape could otherwise appear to close
    /// the fence the caller wrapped this payload in. Sanitizing at the
    /// projection means every consumer of `summary`/`full` inherits it. (F8.)
    pub fn summary(&self, key: &MemoryKey) -> serde_json::Value {
        let links: Vec<String> = self.links.iter().map(|l| Fence::sanitize(l)).collect();
        let mut v = serde_json::json!({
            "key": key.to_string(),
            "name": Fence::sanitize(&self.name),
            "type": self.kind.as_str(),
            "description": Fence::sanitize(&self.description),
            "links": links,
            "updatedAt": self.updated_at,
            // Stated on every projection so a reader never has to infer it.
            "trust": "untrusted-data",
        });
        let prov = provenance::to_json(self.basis.as_deref(), self.confidence, &self.evidence);
        if let (Some(v), Some(p)) = (v.as_object_mut(), prov.as_object()) {
            v.extend(p.clone());
        }
        v
    }

    /// The full form `memory_get` returns: the summary, the body, and what the
    /// memory cites. Citations are identifiers, never fetched.
    pub fn full(&self, key: &MemoryKey) -> serde_json::Value {
        let mut v = self.summary(key);
        v["body"] = serde_json::Value::String(Fence::sanitize(&self.body));
        if !self.evidence.is_empty() {
            v["evidence"] = serde_json::Value::Array(self.evidence.clone());
        }
        v
    }

    /// A short note on what kind of claim this is, for a rendered listing.
    pub fn basis_note(&self) -> Option<String> {
        provenance::note(
            self.basis.as_deref(),
            self.confidence,
            !self.evidence.is_empty(),
        )
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

/// One entry as it came back from the VTA, and who it can be attributed to.
///
/// Only entries that passed [`crate::authorship::verify`] become an `Entry`;
/// one that failed is a [`crate::authorship::Refused`] instead, so holding an
/// `Entry` means its `authorship` has been checked.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: MemoryKey,
    pub record: MemoryRecord,
    pub authorship: Authorship,
    /// The version the VTA holds it at — what an update passes back as
    /// `expectedVersion`. 0 for an entry that did not come from a VTA.
    pub version: u64,
    /// Its status, expiry and review state, judged when it was read.
    pub standing: Standing,
}

impl Entry {
    /// An entry that did not come from a VTA: unattributed, version 0, judged
    /// now. For tests and local ranking.
    pub fn local(key: MemoryKey, record: MemoryRecord) -> Self {
        let standing = Standing::of(&record, chrono::Utc::now());
        Self {
            key,
            record,
            authorship: Authorship::Unattributed,
            version: 0,
            standing,
        }
    }

    /// [`MemoryRecord::summary`] plus who wrote it, its version and its
    /// standing — what recall returns.
    pub fn summary(&self) -> serde_json::Value {
        let mut v = self.record.summary(&self.key);
        self.annotate(&mut v);
        v
    }

    /// [`MemoryRecord::full`] plus the same — what get returns.
    pub fn full(&self) -> serde_json::Value {
        let mut v = self.record.full(&self.key);
        self.annotate(&mut v);
        v
    }

    fn annotate(&self, v: &mut serde_json::Value) {
        v["author"] = self.authorship.to_json();
        v["version"] = serde_json::json!(self.version);
        let life = crate::lifecycle::to_json(&self.record, self.standing);
        if let (Some(v), Some(l)) = (v.as_object_mut(), life.as_object()) {
            v.extend(l.clone());
        }
    }

    /// Notes for a rendered listing: status, basis, authorship.
    pub fn notes(&self) -> Vec<String> {
        [
            crate::lifecycle::note(&self.record, self.standing),
            self.record.basis_note(),
            self.authorship.note(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// How much this entry counts in ranking beyond its relevance: basis
    /// weight times standing weight.
    pub fn weight(&self) -> u32 {
        self.record.basis_weight() * self.standing.weight()
    }
}

/// A ranked recall hit.
#[derive(Debug, Clone)]
pub struct Hit {
    pub entry: Entry,
    pub score: u32,
}

/// Words that carry no topic, dropped from a query before it is matched.
///
/// Without this, "what is the deploy for" scored every memory containing
/// "the" or "for" — which is every memory — and a query of nothing but these
/// words returned the context instead of nothing. Kept short and English on
/// purpose: a word missing from here costs some precision, while a content word
/// wrongly in here makes memories unfindable. Negations ("not", "never", "no")
/// are deliberately absent — they change what a memory means.
const STOPWORDS: &[&str] = &[
    "a", "about", "am", "an", "and", "any", "are", "as", "at", "be", "been", "by", "can", "could",
    "did", "do", "does", "for", "from", "had", "has", "have", "how", "i", "if", "in", "into", "is",
    "it", "its", "me", "my", "of", "on", "or", "our", "should", "so", "than", "that", "the",
    "their", "them", "then", "there", "these", "they", "this", "those", "to", "us", "was", "we",
    "were", "what", "when", "where", "which", "who", "whom", "why", "will", "with", "would", "you",
    "your",
];

/// Split `s` into its words: runs of letters and digits, in any script,
/// lowercased and reduced to a singular form (see [`singular`]).
fn tokens(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| singular(&t.to_lowercase()))
}

/// The set of words in a field, for whole-word matching.
fn words(s: &str) -> std::collections::HashSet<String> {
    tokens(s).collect()
}

/// The terms a query is matched on: its words, less [`STOPWORDS`] and
/// single characters, each once.
fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for t in tokens(query) {
        if t.chars().count() > 1 && !STOPWORDS.contains(&t.as_str()) && !terms.contains(&t) {
            terms.push(t);
        }
    }
    terms
}

/// A plural reduced to its singular, by rule — so "tests" finds "test" and
/// "policies" finds "policy".
///
/// Plurals only, and deliberately so. A stemmer that also strips "-ing" and
/// "-ed" has to guess ("named" → "nam"?), and a guess that is wrong makes a
/// memory unfindable in a way nobody can predict; a plural rule is wrong rarely
/// and visibly. Applied to both sides, so a mistake is at least symmetric.
fn singular(w: &str) -> String {
    let n = w.chars().count();
    if n > 4 && w.ends_with("ies") {
        return format!("{}y", &w[..w.len() - 3]);
    }
    if n > 4
        && w.ends_with("es")
        && ["sses", "xes", "ches", "shes", "zes"]
            .iter()
            .any(|e| w.ends_with(e))
    {
        return w[..w.len() - 2].to_string();
    }
    if n > 3 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") && !w.ends_with("is") {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

/// Rank `entries` against a free-text query.
///
/// Scoring is deliberately dumb — whole-word matching, weighted by field — and
/// that is a decision, not a shortcut. The alternative (embeddings) needs a
/// model, a vector index, and somewhere to keep it; the VTA offers none of the
/// three, and a scoring function nobody can predict is worse than a crude one
/// everybody can. Weights: name 4, description 2, body 1, so a memory *about*
/// the query outranks one that merely mentions it.
///
/// Relevance is then multiplied by the record's basis weight
/// ([`provenance::weight`]): a stated preference outranks an inference that
/// matches the query equally well, but a strong match on an inference still
/// beats a passing mention of a stated fact. `confidence` is never ranked on —
/// two authors' estimates are not on one scale. It is multiplied again by the
/// entry's standing ([`crate::lifecycle::weight`]): a deprecated or disputed
/// memory counts half. (Retracted and expired memories never reach ranking —
/// `Store::recall` leaves them out.)
///
/// An empty query scores everything 0, which makes recall degrade into list —
/// the right behaviour for a session-start hook that just wants what is there.
/// Ties break on basis weight, then `updated_at` descending, then key, so what
/// the user stated survives a limit ahead of what was inferred, and output is
/// stable.
pub fn rank(entries: Vec<Entry>, query: &str) -> Vec<Hit> {
    let terms = query_terms(query);
    let mut hits: Vec<Hit> = entries
        .into_iter()
        .map(|entry| {
            let score = if terms.is_empty() {
                0
            } else {
                let name = words(&entry.record.name);
                let desc = words(&entry.record.description);
                let body = words(&entry.record.body);
                let relevance: u32 = terms
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
                    .sum();
                relevance * entry.weight()
            };
            Hit { entry, score }
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.entry.weight().cmp(&a.entry.weight()))
            .then_with(|| b.entry.record.updated_at.cmp(&a.entry.record.updated_at))
            .then_with(|| a.entry.key.to_string().cmp(&b.entry.key.to_string()))
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: MemoryType, name: &str, desc: &str, body: &str) -> Entry {
        Entry::local(
            MemoryKey::new(kind, name).unwrap(),
            MemoryRecord::new(kind, name, desc, body, vec![]),
        )
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

    fn with_basis(mut e: Entry, basis: Basis, updated_at: &str) -> Entry {
        e.record = e.record.with_provenance(basis, None, vec![]);
        e.record.updated_at = updated_at.to_string();
        e
    }

    #[test]
    fn a_statement_outranks_an_equally_relevant_inference_even_a_newer_one() {
        let stated = with_basis(
            entry(
                MemoryType::User,
                "Short answers",
                "prefers short answers",
                "x",
            ),
            Basis::Declared,
            "2026-01-01T00:00:00Z",
        );
        let guessed = with_basis(
            entry(
                MemoryType::User,
                "Short replies",
                "prefers short answers",
                "x",
            ),
            Basis::Inferred,
            "2026-11-01T00:00:00Z",
        );
        let hits = rank(vec![guessed, stated], "short answers");
        assert_eq!(hits[0].entry.record.name, "Short answers");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn basis_is_a_nudge_not_an_override() {
        // A memory *about* the query, inferred, still beats a stated memory
        // that merely mentions it in passing.
        let about = with_basis(
            entry(MemoryType::Project, "Gateway", "the gateway rollout", "x"),
            Basis::Inferred,
            "2026-01-01T00:00:00Z",
        );
        let mentions = with_basis(
            entry(
                MemoryType::Project,
                "Other",
                "something else",
                "the gateway",
            ),
            Basis::Declared,
            "2026-01-01T00:00:00Z",
        );
        let hits = rank(vec![mentions, about], "gateway");
        assert_eq!(hits[0].entry.record.name, "Gateway");
    }

    #[test]
    fn with_no_query_stated_memories_survive_a_limit_ahead_of_inferred_ones() {
        let stated = with_basis(
            entry(MemoryType::User, "A", "a", "a"),
            Basis::Declared,
            "2026-01-01T00:00:00Z",
        );
        let guessed = with_basis(
            entry(MemoryType::User, "B", "b", "b"),
            Basis::Inferred,
            "2026-11-01T00:00:00Z",
        );
        let hits = rank(vec![guessed, stated], "");
        assert!(hits.iter().all(|h| h.score == 0));
        assert_eq!(hits[0].entry.record.name, "A");
    }

    #[test]
    fn verified_without_evidence_ranks_as_an_inference() {
        let mut unsupported = entry(MemoryType::User, "X", "x", "x");
        unsupported.record.basis = Some("verified".into());
        let inferred = with_basis(entry(MemoryType::User, "Y", "y", "y"), Basis::Inferred, "");
        assert_eq!(
            unsupported.record.basis_weight(),
            inferred.record.basis_weight()
        );
    }

    #[test]
    fn a_record_from_before_provenance_reads_as_unstated() {
        let key = MemoryKey::parse("user/old").unwrap();
        let r = MemoryRecord::decode(
            &key,
            r#"{"v":1,"name":"Old","type":"user","description":"d","body":"b","updatedAt":""}"#,
        );
        assert_eq!(r.basis(), None);
        assert_eq!(r.summary(&key)["basis"], "unstated");
        assert_eq!(r.basis_note().as_deref(), Some("basis unstated"));
    }

    #[test]
    fn an_unknown_basis_from_a_later_writer_does_not_break_decoding() {
        let key = MemoryKey::parse("user/new").unwrap();
        let r = MemoryRecord::decode(
            &key,
            r#"{"v":2,"name":"New","type":"user","description":"d","body":"b","updatedAt":"",
                "basis":"testified","evidence":[{"kind":"notarised","ref":"x"}]}"#,
        );
        assert_eq!(r.name, "New", "decoded as a record, not a foreign value");
        assert_eq!(r.basis(), None);
        assert_eq!(r.full(&key)["evidence"][0]["kind"], "notarised");
    }

    #[test]
    fn terms_match_whole_words_not_fragments() {
        // "run" is not in "runner", and "test" is not in "latest".
        let hits = rank(
            vec![
                entry(MemoryType::Feedback, "Test runner", "executed by CI", ""),
                entry(MemoryType::Project, "Latest release", "shipped", ""),
            ],
            "run",
        );
        assert!(hits.iter().all(|h| h.score == 0), "{hits:?}");
        let hits = rank(
            vec![entry(MemoryType::Project, "Latest release", "shipped", "")],
            "test",
        );
        assert_eq!(hits[0].score, 0);
    }

    #[test]
    fn plurals_match_their_singular_both_ways() {
        for (query, text) in [
            ("tests", "how to run the test"),
            ("test", "the tests live here"),
            ("policy", "retention policies"),
            ("box", "inboxes and boxes"),
            ("push", "no pushes to main"),
        ] {
            let hits = rank(vec![entry(MemoryType::User, "x", text, "")], query);
            assert!(hits[0].score > 0, "{query:?} should find {text:?}");
        }
    }

    #[test]
    fn words_ending_in_s_that_are_not_plurals_are_left_alone() {
        for w in ["status", "analysis", "process", "bus", "gas"] {
            assert_eq!(singular(w), w, "{w}");
        }
    }

    #[test]
    fn a_query_of_only_stopwords_has_no_terms() {
        assert!(query_terms("what is the for").is_empty());
        assert_eq!(query_terms("what is the deploy for"), vec!["deploy"]);
        // Negations are content: they change what is meant.
        assert_eq!(query_terms("never push"), vec!["never", "push"]);
    }

    #[test]
    fn non_latin_words_match() {
        let hits = rank(
            vec![entry(
                MemoryType::User,
                "Café",
                "prefers the café on Straße",
                "",
            )],
            "CAFÉ straße",
        );
        assert_eq!(
            hits[0].score,
            (4 + 2 + 2) * hits[0].entry.weight(),
            "café in name and description, straße in description"
        );
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
