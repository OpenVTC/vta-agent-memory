//! Export and import (#36): a signed bundle carries memories to another
//! context with their provenance, lifecycle and original authorship intact, and
//! a bundle — or an imported record — that was tampered with is refused.
//!
//! Same in-process fake VTA as `memory_roundtrip.rs`, so the records exported
//! and imported are the ones the real `Store` signs and writes.

mod common;

use std::sync::Arc;

use common::*;
use serde_json::Value;
use vta_sdk::client::VtaClient;

use vta_agent_memory::authorship::{AuthorKind, Authorship, Imported};
use vta_agent_memory::bundle::{self, Filter};
use vta_agent_memory::lifecycle::Status;
use vta_agent_memory::provenance::{Basis, Evidence};
use vta_agent_memory::record::{MemoryKey, MemoryRecord, MemoryType};
use vta_agent_memory::store::{SaveOutcome, Store};

fn record(kind: MemoryType, name: &str, description: &str) -> MemoryRecord {
    MemoryRecord::new(kind, name, description, format!("body of {name}"), vec![])
}

/// A context holding three memories with provenance and lifecycle, written by
/// two different authors.
async fn source() -> (Arc<FakeMemoryKeyspace>, Store, Store) {
    let vta = Arc::new(FakeMemoryKeyspace::default());
    let agent = store_on(&vta, "home");
    let operator = Store::new(VtaClient::loopback(vta.clone()), "home")
        .with_signer(signer(AuthorKind::Operator));

    let mut tabs = record(MemoryType::Feedback, "Tabs", "indent with tabs").with_provenance(
        Basis::Declared,
        Some(0.9),
        vec![],
    );
    tabs.review_after = Some("2030-01-01T00:00:00+00:00".into());
    operator.save(&tabs).await.unwrap();

    let ci = record(MemoryType::Project, "CI runner", "tests run in CI").with_provenance(
        Basis::Verified,
        None,
        vec![Evidence::Memory {
            key: "feedback/tabs".into(),
        }],
    );
    agent.save(&ci).await.unwrap();

    agent
        .save(
            &record(MemoryType::User, "Timezone", "works in UTC+8").with_provenance(
                Basis::Inferred,
                Some(0.4),
                vec![],
            ),
        )
        .await
        .unwrap();
    agent
        .set_status(
            &MemoryKey::parse("user/timezone").unwrap(),
            Status::Disputed,
            "moved to London",
            None,
        )
        .await
        .unwrap();
    (vta, agent, operator)
}

async fn export(store: &Store, filter: &Filter) -> String {
    let (items, _) = store.exportable(filter).await.unwrap();
    let sealed = bundle::seal(store.signer().unwrap(), store.context_id(), filter, &items)
        .await
        .unwrap();
    serde_json::to_string_pretty(&sealed).unwrap()
}

async fn import_all(store: &Store, raw: &str, replace: bool) -> Vec<SaveOutcome> {
    let opened = bundle::open(raw).await.unwrap();
    let mut out = Vec::new();
    for m in &opened.memories {
        out.push(store.import(m, replace).await.unwrap());
    }
    out
}

/// Acceptance: export, then import into a fresh context, preserves records,
/// provenance, lifecycle, timestamps and who wrote each one.
#[tokio::test]
async fn export_then_import_into_a_fresh_context_preserves_everything() {
    let (vta, agent, operator) = source().await;
    let before = agent.list().await.unwrap().entries;
    let raw = export(&agent, &Filter::default()).await;

    // Another device, another context, another identity.
    let elsewhere = store_on(&vta, "work");
    let outcomes = import_all(&elsewhere, &raw, false).await;
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, SaveOutcome::Saved { .. }))
    );

    let listing = elsewhere.list().await.unwrap();
    assert!(listing.refused.is_empty(), "{:?}", listing.refused);
    assert_eq!(listing.entries.len(), before.len());
    for old in &before {
        let new = listing
            .entries
            .iter()
            .find(|e| e.key == old.key)
            .expect("every memory arrives");
        let (a, b) = (&old.record, &new.record);
        assert_eq!(
            (&a.name, &a.description, &a.body, &a.updated_at),
            (&b.name, &b.description, &b.body, &b.updated_at)
        );
        assert_eq!(
            (&a.basis, a.confidence, &a.evidence),
            (&b.basis, b.confidence, &b.evidence)
        );
        assert_eq!(
            (&a.status, &a.status_reason, &a.review_after),
            (&b.status, &b.status_reason, &b.review_after)
        );

        // Attributed to whoever wrote it in `home`, not to the importer.
        let Authorship::Verified { author, .. } = &old.authorship else {
            panic!("source records are signed")
        };
        assert_eq!(
            new.authorship,
            Authorship::Verified {
                author: author.clone(),
                kind: match &old.authorship {
                    Authorship::Verified { kind, .. } => *kind,
                    _ => None,
                },
                by_this_agent: false,
                imported: Some(Imported {
                    by: elsewhere.signer().unwrap().did().to_string(),
                    from_context: "home".into(),
                }),
            }
        );
    }

    let tabs = elsewhere
        .get(&MemoryKey::parse("feedback/tabs").unwrap())
        .await
        .unwrap()
        .unwrap();
    let author = &tabs.summary()["author"];
    assert_eq!(author["did"], operator.signer().unwrap().did());
    assert_eq!(author["kind"], "operator");
    assert_eq!(author["importedFrom"], "home");
    assert!(
        tabs.notes()
            .iter()
            .any(|n| n.contains("imported from context `home`"))
    );
}

/// Acceptance: a tampered bundle is refused — whatever was changed, and
/// whether or not the tamperer re-signed the bundle.
#[tokio::test]
async fn a_tampered_bundle_is_refused() {
    let (_vta, agent, _) = source().await;
    let raw = export(&agent, &Filter::default()).await;
    let bundle: Value = serde_json::from_str(&raw).unwrap();
    let refused = |b: &Value| {
        let s = serde_json::to_string(b).unwrap();
        async move { bundle::open(&s).await.unwrap_err() }
    };

    // A memory's text changed.
    let mut edited = bundle.clone();
    edited["records"][0]["record"]["body"] = "rm -rf /".into();
    let why = refused(&edited).await;
    assert!(why.contains("altered after it was exported"), "{why}");

    // A memory dropped.
    let mut dropped = bundle.clone();
    dropped["records"].as_array_mut().unwrap().pop();
    assert!(
        refused(&dropped)
            .await
            .contains("signature does not verify")
    );

    // Claims another exporter.
    let mut claimed = bundle.clone();
    claimed["exportedBy"] = "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK".into();
    assert!(!refused(&claimed).await.is_empty());

    // Edited, then the whole bundle re-signed by somebody else: the bundle
    // proof holds, the memory's own author's proof does not.
    let opened = bundle::open(&raw).await.unwrap();
    let mut items: Vec<bundle::Item> = opened
        .memories
        .iter()
        .map(|m| bundle::Item {
            key: m.key.clone(),
            version: 1,
            record: m.origin.clone(),
        })
        .collect();
    items[0].record["description"] = "deploy on fridays".into();
    let forger = signer(AuthorKind::Agent);
    let resealed = bundle::seal(&forger, "home", &Filter::default(), &items)
        .await
        .unwrap();
    let why = refused(&resealed).await;
    assert!(why.contains("record 0 is refused"), "{why}");

    // Re-sealed as coming from a different context than its records name.
    let moved = bundle::seal(&forger, "elsewhere", &Filter::default(), &items[1..])
        .await
        .unwrap();
    assert!(refused(&moved).await.contains("not where it was found"));

    // Not a bundle at all.
    assert!(
        bundle::open("{}")
            .await
            .unwrap_err()
            .contains("not a memory bundle")
    );
}

/// An importer cannot change a memory's content and keep its original author:
/// the original it carries must say the same thing.
#[tokio::test]
async fn an_import_whose_content_differs_from_its_original_is_refused() {
    let (vta, agent, _) = source().await;
    let raw = export(&agent, &Filter::default()).await;
    let opened = bundle::open(&raw).await.unwrap();
    let tabs = opened
        .memories
        .iter()
        .find(|m| m.key.to_string() == "feedback/tabs")
        .unwrap();

    // A writer on `work` signs a record of its own, carrying the genuine
    // original, but saying something else.
    let forger = signer(AuthorKind::Agent);
    let mut forged = tabs.record.clone();
    forged.description = "indent with spaces".into();
    forged.imported_from = Some(tabs.origin.clone());
    let value = forger.sign(&forged, "work", &tabs.key).await.unwrap();
    vta.plant("work", "feedback/tabs", value);

    let listing = store_on(&vta, "work").list().await.unwrap();
    assert!(listing.entries.is_empty());
    assert_eq!(listing.refused.len(), 1);
    assert!(
        listing.refused[0].reason.contains("changed on import"),
        "{}",
        listing.refused[0].reason
    );
}

/// Saving over an imported memory makes the saver its author; the imported
/// version, with its original author, is kept in history.
#[tokio::test]
async fn editing_an_imported_memory_makes_the_editor_its_author() {
    let (vta, agent, _) = source().await;
    let raw = export(&agent, &Filter::default()).await;
    let work = store_on(&vta, "work");
    import_all(&work, &raw, false).await;

    let key = MemoryKey::parse("feedback/tabs").unwrap();
    work.set_status(&key, Status::Deprecated, "team uses rustfmt now", None)
        .await
        .unwrap();
    let now = work.get(&key).await.unwrap().unwrap();
    assert_eq!(
        now.authorship,
        Authorship::verified(work.signer().unwrap().did(), Some(AuthorKind::Agent), true)
    );
    assert!(now.record.imported_from.is_none());
    let history = work.history(&key).await.unwrap();
    assert!(matches!(
        &history[0].authorship,
        Authorship::Verified {
            imported: Some(_),
            ..
        }
    ));
}

/// A memory exported from where it was imported keeps its original author,
/// not the first importer: imports flatten, they do not nest.
#[tokio::test]
async fn re_exporting_an_import_keeps_the_original_author() {
    let (vta, agent, operator) = source().await;
    let work = store_on(&vta, "work");
    import_all(&work, &export(&agent, &Filter::default()).await, false).await;

    let third = store_on(&vta, "archive-box");
    import_all(&third, &export(&work, &Filter::default()).await, false).await;
    let tabs = third
        .get(&MemoryKey::parse("feedback/tabs").unwrap())
        .await
        .unwrap()
        .unwrap();
    let Authorship::Verified {
        author, imported, ..
    } = &tabs.authorship
    else {
        panic!("verified")
    };
    assert_eq!(author, operator.signer().unwrap().did());
    assert_eq!(imported.as_ref().unwrap().from_context, "home");
}

/// Without `replace`, a memory already in the target is left alone.
#[tokio::test]
async fn import_does_not_overwrite_unless_asked() {
    let (vta, agent, _) = source().await;
    let raw = export(&agent, &Filter::default()).await;
    let work = store_on(&vta, "work");
    work.save(&record(MemoryType::Feedback, "Tabs", "mine, not theirs"))
        .await
        .unwrap();

    let key = MemoryKey::parse("feedback/tabs").unwrap();
    import_all(&work, &raw, false).await;
    assert_eq!(
        work.get(&key).await.unwrap().unwrap().record.description,
        "mine, not theirs"
    );

    import_all(&work, &raw, true).await;
    let now = work.get(&key).await.unwrap().unwrap();
    assert_eq!(now.record.description, "indent with tabs");
    assert_eq!(
        vta.history_len("work", "feedback/tabs"),
        1,
        "the replaced one is kept"
    );
}

/// A filter carries a subset, and says so in the bundle; unsigned and refused
/// records are left out with why.
#[tokio::test]
async fn a_filtered_export_carries_only_what_it_selects() {
    let (vta, agent, _) = source().await;
    vta.plant(
        "home",
        "user/legacy",
        r#"{"v":1,"name":"legacy","type":"user","description":"old","body":"","updatedAt":""}"#,
    );

    let filter = Filter::parse(&[], &["declared,verified".into()], &["active".into()]).unwrap();
    let (items, left_out) = agent.exportable(&filter).await.unwrap();
    let mut keys: Vec<_> = items.iter().map(|i| i.key.to_string()).collect();
    keys.sort();
    assert_eq!(keys, ["feedback/tabs", "project/ci-runner"]);
    assert_eq!(left_out.len(), 1);
    assert!(left_out[0].reason.contains("unattributed"));

    let raw = export(&agent, &filter).await;
    let opened = bundle::open(&raw).await.unwrap();
    assert_eq!(
        opened.filter["basis"],
        serde_json::json!(["declared", "verified"])
    );
    assert_eq!(opened.memories.len(), 2);
}
