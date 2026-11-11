//! A memory's standing over time (#33): status, expiry, review, and what it
//! replaced.
//!
//! Before this, a memory had two states — there, or hard-deleted — and a save
//! silently overwrote whatever it replaced. So a preference from last year had
//! the same standing as one from today, a wrong memory could only be erased
//! (not marked wrong), and two writers saving at once lost one of the saves
//! without anybody knowing. Data Rooms already solved the same problems for
//! shared records (`status`, `expectedVersion`, tombstones); this is the
//! personal-memory counterpart.
//!
//! - **`status`** — `active` (the default), `deprecated` (superseded or out of
//!   date, kept for reference), `disputed` (somebody says it is wrong), or
//!   `retracted` (withdrawn — a tombstone). Recall leaves retracted memories
//!   out and ranks deprecated and disputed ones lower, saying why.
//! - **`validUntil`** — after it, the memory has expired and recall leaves it
//!   out. For facts with a shelf life: "on leave until 2026-12-01".
//! - **`reviewAfter`** — after it, the memory still recalls, but is flagged
//!   `stale` so somebody confirms it. For preferences that drift.
//! - **`supersedes`** — every save that replaces a memory first copies the
//!   version it replaces, verbatim and still signed by its own author, to a
//!   history key, and links to it. Nothing a save replaces is lost.
//!
//! All of it is inside the record's signature. Changing a status is a new
//! version, signed by whoever changed it — so a disputed memory's `author` is
//! the disputer, and the original author's signed text is one `supersedes`
//! hop away in the history.
//!
//! # History keys
//!
//! `archive/<type>/<slug>/v<version>` — deliberately *not* `<type>/…`, so a
//! typed recall's prefix never reaches history, and an untyped list skips it as
//! a key shape it does not own. At most [`MAX_HISTORY`] versions are kept per
//! memory; `memory_forget` removes the history with the memory, because
//! "forget" has to mean forget.

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

use crate::record::{MemoryKey, MemoryRecord};

/// Versions of one memory kept in its history, newest first.
pub const MAX_HISTORY: usize = 5;
/// Longest status reason, in characters.
pub const MAX_REASON_CHARS: usize = 300;

/// Where a memory stands.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Current. What a memory is unless it says otherwise.
    Active,
    /// Out of date or replaced, kept for reference.
    Deprecated,
    /// Somebody says it is wrong; the reason says why.
    Disputed,
    /// Withdrawn. Its content is gone; that it existed is not.
    Retracted,
}

impl Status {
    /// The wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Deprecated => "deprecated",
            Status::Disputed => "disputed",
            Status::Retracted => "retracted",
        }
    }

    /// Parse the wire form. An absent status is `active`; one this version
    /// does not know is `None`, and is treated as disputed — standing nobody
    /// has vouched for.
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("active") => Some(Status::Active),
            Some("deprecated") => Some(Status::Deprecated),
            Some("disputed") => Some(Status::Disputed),
            Some("retracted") => Some(Status::Retracted),
            Some(_) => None,
        }
    }
}

/// How much a memory's status counts in ranking, out of [`FULL_WEIGHT`].
pub fn weight(status: Option<Status>) -> u32 {
    match status {
        Some(Status::Active) => FULL_WEIGHT,
        Some(Status::Deprecated) | Some(Status::Disputed) | None => 5,
        Some(Status::Retracted) => 0,
    }
}

/// The weight of an active memory.
pub const FULL_WEIGHT: u32 = 10;

/// The history key for `key` at `version`.
pub fn archive_key(key: &MemoryKey, version: u64) -> String {
    format!("{}v{version}", archive_prefix(key))
}

/// The prefix every history key of `key` starts with.
pub fn archive_prefix(key: &MemoryKey) -> String {
    format!("archive/{key}/")
}

/// The version a history key names, if it is one of `key`'s.
pub fn archived_version(key: &MemoryKey, raw: &str) -> Option<u64> {
    raw.strip_prefix(&archive_prefix(key))?
        .strip_prefix('v')?
        .parse()
        .ok()
}

/// Parse a date a caller gave: RFC 3339, or a bare `YYYY-MM-DD` meaning the
/// start of that day in UTC. Returned in RFC 3339, so stored dates compare.
pub fn normalise_date(field: &str, raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(raw) {
        return Ok(t.with_timezone(&Utc).to_rfc3339());
    }
    if let Ok(d) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let t = d.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc();
        return Ok(t.to_rfc3339());
    }
    Err(format!(
        "`{field}` is `{raw}`; expected a date (`2026-12-01`) or an RFC 3339 time"
    ))
}

/// `when` has passed at `now`. An unparseable date has not: a malformed
/// expiry must not make a memory vanish.
fn passed(when: Option<&str>, now: DateTime<Utc>) -> bool {
    when.and_then(|w| DateTime::parse_from_rfc3339(w).ok())
        .is_some_and(|t| t <= now)
}

/// What recall needs to know about a record's standing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    /// `None` for a status this version does not know.
    pub status: Option<Status>,
    /// Past `validUntil`.
    pub expired: bool,
    /// Past `reviewAfter`.
    pub stale: bool,
}

impl Standing {
    /// The standing of `record` at `now`.
    pub fn of(record: &MemoryRecord, now: DateTime<Utc>) -> Self {
        Self {
            status: Status::parse(record.status.as_deref()),
            expired: passed(record.valid_until.as_deref(), now),
            stale: passed(record.review_after.as_deref(), now),
        }
    }

    /// Whether recall returns it at all.
    pub fn recallable(&self) -> bool {
        !self.expired && self.status != Some(Status::Retracted)
    }

    /// Ranking weight, out of [`FULL_WEIGHT`].
    pub fn weight(&self) -> u32 {
        weight(self.status)
    }
}

/// The lifecycle fields recall, get and list return.
pub fn to_json(record: &MemoryRecord, standing: Standing) -> Value {
    let mut v = serde_json::json!({
        "status": standing.status.map(|s| s.as_str()).unwrap_or("unknown"),
    });
    if let Some(r) = &record.status_reason {
        v["statusReason"] = Value::String(crate::fence::Fence::sanitize(r));
    }
    if let Some(d) = &record.valid_until {
        v["validUntil"] = Value::String(d.clone());
    }
    if let Some(d) = &record.review_after {
        v["reviewAfter"] = Value::String(d.clone());
    }
    if standing.expired {
        v["expired"] = Value::Bool(true);
    }
    if standing.stale {
        v["stale"] = Value::Bool(true);
    }
    if let Some(s) = &record.supersedes {
        v["supersedes"] = s.clone();
    }
    v
}

/// A short note for a rendered listing, or `None` for an active, current memory.
pub fn note(record: &MemoryRecord, standing: Standing) -> Option<String> {
    let mut parts = Vec::new();
    match standing.status {
        Some(Status::Active) => {}
        Some(s) => parts.push(match &record.status_reason {
            Some(r) => format!("{}: {r}", s.as_str()),
            None => s.as_str().to_string(),
        }),
        None => parts.push("status unknown".to_string()),
    }
    if standing.expired {
        parts.push("expired".to_string());
    }
    if standing.stale {
        parts.push("due for review".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Check a status reason a caller gave.
pub fn check_reason(reason: &str) -> Result<(), String> {
    if reason.trim().is_empty() {
        return Err("a reason is required".to_string());
    }
    let n = reason.chars().count();
    if n > MAX_REASON_CHARS {
        return Err(format!(
            "the reason is {n} characters; the limit is {MAX_REASON_CHARS} — it should be one line"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::MemoryType;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn record() -> MemoryRecord {
        MemoryRecord::new(MemoryType::User, "n", "d", "b", vec![])
    }

    #[test]
    fn dates_accept_a_day_or_a_time_and_refuse_anything_else() {
        assert_eq!(
            normalise_date("validUntil", "2026-12-01").unwrap(),
            "2026-12-01T00:00:00+00:00"
        );
        assert_eq!(
            normalise_date("validUntil", "2026-12-01T10:00:00+02:00").unwrap(),
            "2026-12-01T08:00:00+00:00"
        );
        let err = normalise_date("reviewAfter", "next tuesday").unwrap_err();
        assert!(err.contains("reviewAfter"), "{err}");
    }

    #[test]
    fn expiry_and_review_are_judged_against_now() {
        let mut r = record();
        r.valid_until = Some("2026-12-01T00:00:00+00:00".into());
        r.review_after = Some("2026-06-01T00:00:00+00:00".into());
        let before = Standing::of(&r, at("2026-05-01T00:00:00Z"));
        assert!(!before.expired && !before.stale && before.recallable());
        let between = Standing::of(&r, at("2026-07-01T00:00:00Z"));
        assert!(!between.expired && between.stale && between.recallable());
        let after = Standing::of(&r, at("2027-01-01T00:00:00Z"));
        assert!(after.expired && !after.recallable());
    }

    #[test]
    fn a_malformed_expiry_does_not_make_a_memory_vanish() {
        let mut r = record();
        r.valid_until = Some("whenever".into());
        assert!(Standing::of(&r, Utc::now()).recallable());
    }

    #[test]
    fn retracted_is_not_recalled_and_an_unknown_status_is_demoted() {
        let mut r = record();
        r.status = Some("retracted".into());
        assert!(!Standing::of(&r, Utc::now()).recallable());
        r.status = Some("quarantined".into());
        let s = Standing::of(&r, Utc::now());
        assert!(s.recallable());
        assert!(s.weight() < FULL_WEIGHT);
        assert_eq!(to_json(&r, s)["status"], "unknown");
    }

    #[test]
    fn notes_say_what_is_unusual_and_nothing_otherwise() {
        let mut r = record();
        let now = at("2027-01-01T00:00:00Z");
        assert_eq!(note(&r, Standing::of(&r, now)), None);
        r.status = Some("disputed".into());
        r.status_reason = Some("user says spaces".into());
        r.review_after = Some("2026-06-01T00:00:00+00:00".into());
        assert_eq!(
            note(&r, Standing::of(&r, now)).as_deref(),
            Some("disputed: user says spaces, due for review")
        );
    }

    #[test]
    fn history_keys_are_outside_every_type_prefix() {
        let key = MemoryKey::parse("user/name").unwrap();
        let archived = archive_key(&key, 7);
        assert_eq!(archived, "archive/user/name/v7");
        assert!(MemoryKey::parse(&archived).is_err(), "list must skip it");
        for kind in crate::record::MemoryType::ALL {
            assert!(!MemoryKey::raw_key_has_type(&archived, kind));
        }
        assert_eq!(archived_version(&key, &archived), Some(7));
        let other = MemoryKey::parse("user/name-two").unwrap();
        assert_eq!(archived_version(&other, &archived), None);
    }
}
