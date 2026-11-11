//! Recall across personal memory and Data Rooms (#34): one ranking, a source
//! and an author on every result, and a room record whose signature does not
//! hold left out and reported.
//!
//! The fake VTA answers `rooms/keys/browse/0.4` and `rooms/keys/read/0.2` as a
//! member's VTA does once it has fetched from the host and opened the bodies,
//! so what is exercised is this crate's checks on what comes back.

mod common;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};
use vta_sdk::trust_task_proof::TrustTaskVmResolver;

use vta_agent_memory::provenance::Basis;
use vta_agent_memory::record::{MemoryRecord, MemoryType};
use vta_agent_memory::rooms::{self, Found, RoomSource, Sources};

const DEALS: &str = "did:webvh:QmRoom:rooms.example:deals";
const WIKI: &str = "did:webvh:QmRoom:rooms.example:wiki";

fn source(room: &str, label: &str) -> RoomSource {
    RoomSource {
        room_id: room.into(),
        host: "did:web:host.example".into(),
        member: None,
        label: Some(label.into()),
    }
}

fn resolver() -> TrustTaskVmResolver {
    TrustTaskVmResolver::did_key_only()
}

/// A sealed record in `room` at `version`, written by `author`, with the host
/// recording that author (an `attributed` room).
async fn sealed(
    author: (&str, &str),
    room: &str,
    key: &str,
    version: u64,
    status: &str,
    fields: Value,
) -> Value {
    json!({
        "key": key, "version": version, "status": status,
        "updatedAt": format!("2026-11-0{version}T00:00:00Z"),
        "author": author.0,
        "plaintext": sealed_body(author, room, key, version, fields).await,
    })
}

fn names(found: &[Found]) -> Vec<String> {
    found
        .iter()
        .map(|f| f.summary("home")["name"].as_str().unwrap().to_string())
        .collect()
}

/// Acceptance: recall across one personal context and two rooms, each result
/// saying where it came from, who wrote it, its status and its basis.
#[tokio::test]
async fn recall_spans_personal_memory_and_rooms_with_provenance_on_each() {
    let (store, vta) = store_with("home");
    store
        .save(
            &MemoryRecord::new(
                MemoryType::Project,
                "Northwind pricing",
                "the user thinks Northwind is price-sensitive",
                "",
                vec![],
            )
            .with_provenance(Basis::Inferred, Some(0.5), vec![]),
        )
        .await
        .unwrap();

    let (alice, alice_key) = did_key();
    vta.room_record(
        DEALS,
        sealed(
            (&alice, &alice_key),
            DEALS,
            "k7Q",
            3,
            "active",
            json!({
                "title": "Northwind pricing holds",
                "description": "Northwind accepted the pricing in the term sheet",
                "body": "Signed 2026-10-08.",
                "basis": "observed",
            }),
        )
        .await,
    );
    // An `open` room: cleartext, no in-body signature.
    vta.room_record(
        WIKI,
        json!({
            "key": "northwind", "version": 1, "status": "active",
            "updatedAt": "2026-11-01T00:00:00Z", "author": "did:key:z6MkWikiEditor",
            "cleartext": { "title": "Northwind", "body": "A customer. Pricing via the deals room." },
        }),
    );

    let sources = Sources::select(None, &[source(DEALS, "deals"), source(WIKI, "wiki")]).unwrap();
    let got = rooms::recall(&store, &sources, "northwind pricing", None, 10, &resolver())
        .await
        .unwrap();
    assert!(got.refused.is_empty(), "{:?}", got.refused);
    assert!(got.unavailable.is_empty(), "{:?}", got.unavailable);
    assert_eq!(
        names(&got.found),
        [
            "Northwind pricing holds", // observed, in title and description
            "Northwind pricing",       // inferred: weighs less
            "Northwind",
        ]
    );

    let room = got.found[0].summary("home");
    assert_eq!(
        room["source"],
        json!({ "kind": "room", "roomId": DEALS, "label": "deals" })
    );
    assert_eq!(room["author"]["status"], "verified");
    assert_eq!(room["author"]["did"], alice.as_str());
    assert_eq!(room["status"], "active");
    assert_eq!(room["basis"], "observed");
    assert_eq!(room["trust"], "untrusted-data");
    assert!(
        room.get("body").is_none(),
        "recall returns summaries, not bodies"
    );

    let mine = got.found[1].summary("home");
    assert_eq!(
        mine["source"],
        json!({ "kind": "personal", "contextId": "home" })
    );
    assert_eq!(mine["author"]["thisAgent"], true);
    assert_eq!(mine["basis"], "inferred");

    let wiki = got.found[2].summary("home");
    assert_eq!(wiki["author"]["status"], "hostReported");
    assert_eq!(wiki["author"]["did"], "did:key:z6MkWikiEditor");

    // Recall only reads: nothing but browse and read reached the rooms.
    for (uri, _) in vta.recorded() {
        if uri.contains("/rooms/") {
            assert!(
                uri.ends_with("/rooms/keys/browse/0.4") || uri.ends_with("/rooms/keys/read/0.2"),
                "recall sent {uri}"
            );
        }
    }
}

/// Acceptance: a room record whose signature fails is excluded and reported —
/// whether its text was altered, it was moved to another key, or it names an
/// author the host did not record.
#[tokio::test]
async fn a_room_record_whose_signature_fails_is_excluded() {
    let (store, vta) = store_with("home");
    let (alice, alice_key) = did_key();
    let fields = json!({ "title": "Deploy freeze", "description": "no deploys in December" });

    // Altered after signing.
    let mut altered = sealed(
        (&alice, &alice_key),
        DEALS,
        "a1",
        1,
        "active",
        fields.clone(),
    )
    .await;
    let mut body: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(altered["plaintext"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    body["description"] = "deploys in December are fine".into();
    altered["plaintext"] = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&body).unwrap())
        .into();
    vta.room_record(DEALS, altered);

    // Signed for `b1`, served as `b2`.
    let mut moved = sealed(
        (&alice, &alice_key),
        DEALS,
        "b1",
        2,
        "active",
        fields.clone(),
    )
    .await;
    moved["key"] = "b2".into();
    vta.room_record(DEALS, moved);

    // Signed by Mallory, the host recorded Alice.
    let (mallory, mallory_key) = did_key();
    let mut forged = sealed(
        (&mallory, &mallory_key),
        DEALS,
        "c1",
        3,
        "active",
        fields.clone(),
    )
    .await;
    forged["author"] = alice.clone().into();
    vta.room_record(DEALS, forged);

    // And one good one, which still recalls.
    vta.room_record(
        DEALS,
        sealed((&alice, &alice_key), DEALS, "d1", 4, "active", fields).await,
    );

    let sources = Sources::select(Some(&["rooms".into()]), &[source(DEALS, "deals")]).unwrap();
    let got = rooms::recall(&store, &sources, "deploys december", None, 10, &resolver())
        .await
        .unwrap();
    let keys: Vec<_> = got
        .found
        .iter()
        .map(|f| f.summary("home")["key"].clone())
        .collect();
    assert_eq!(keys, [json!("d1")]);

    let reason = |k: &str| {
        got.refused
            .iter()
            .find(|r| r.key.ends_with(&format!("/ {k}")))
            .unwrap_or_else(|| panic!("{k} refused: {:?}", got.refused))
            .reason
            .clone()
    };
    assert!(reason("a1").contains("does not verify"), "{}", reason("a1"));
    assert!(reason("b2").contains("copied or moved"), "{}", reason("b2"));
    assert!(reason("c1").contains("host recorded"), "{}", reason("c1"));

    // And the same with no query — what the session-start path would show.
    let all = rooms::recall(&store, &sources, "", None, 10, &resolver())
        .await
        .unwrap();
    assert_eq!(all.found.len(), 1);
    assert_eq!(all.refused.len(), 3);
}

/// Deprecated room records are demoted, retracted and expired ones are left
/// out, and a curated record verifies at the version it was sealed at.
#[tokio::test]
async fn room_record_standing_is_respected() {
    let (store, vta) = store_with("home");
    let (alice, alice_key) = did_key();
    let about = |title: &str| json!({ "title": title, "description": "release checklist" });
    vta.room_record(
        DEALS,
        sealed(
            (&alice, &alice_key),
            DEALS,
            "old",
            1,
            "deprecated",
            about("Old"),
        )
        .await,
    );
    vta.room_record(
        DEALS,
        sealed(
            (&alice, &alice_key),
            DEALS,
            "new",
            2,
            "active",
            about("New"),
        )
        .await,
    );
    vta.room_record(
        DEALS,
        json!({ "key": "gone", "version": 3, "status": "retracted", "updatedAt": "2026-11-03T00:00:00Z" }),
    );
    let mut expired = about("Expired");
    expired["validUntil"] = "2020-01-01T00:00:00Z".into();
    vta.room_record(
        DEALS,
        sealed((&alice, &alice_key), DEALS, "exp", 4, "active", expired).await,
    );
    // Sealed at 5, deprecated by a curator at 6: the body keeps version 5.
    let mut curated = sealed(
        (&alice, &alice_key),
        DEALS,
        "cur",
        5,
        "deprecated",
        about("Curated"),
    )
    .await;
    curated["version"] = 6.into();
    curated["sealedAt"] = 5.into();
    vta.room_record(DEALS, curated);

    let sources = Sources::select(None, &[source(DEALS, "deals")]).unwrap();
    for query in ["release checklist", ""] {
        let got = rooms::recall(&store, &sources, query, None, 10, &resolver())
            .await
            .unwrap();
        assert!(got.refused.is_empty(), "{query:?}: {:?}", got.refused);
        let n = names(&got.found);
        assert_eq!(n[0], "New", "{query:?}: active first, {n:?}");
        assert!(n.contains(&"Old".to_string()) && n.contains(&"Curated".to_string()));
        assert!(!n.contains(&"Expired".to_string()), "{query:?}: {n:?}");
        assert_eq!(n.len(), 3, "{query:?}: retracted is not recalled, {n:?}");
    }
}

/// A room that cannot be read is reported, and personal memory still answers.
#[tokio::test]
async fn an_unreachable_room_does_not_take_recall_down() {
    let (store, vta) = store_with("home");
    store
        .save(&MemoryRecord::new(
            MemoryType::User,
            "Tabs",
            "indent with tabs",
            "",
            vec![],
        ))
        .await
        .unwrap();
    vta.unreachable.lock().unwrap().push(DEALS.into());

    let sources = Sources::select(None, &[source(DEALS, "deals")]).unwrap();
    let got = rooms::recall(&store, &sources, "tabs", None, 10, &resolver())
        .await
        .unwrap();
    assert_eq!(names(&got.found), ["Tabs"]);
    assert_eq!(got.unavailable.len(), 1);
    assert!(
        got.unavailable[0].1.contains("hostUnreachable"),
        "{:?}",
        got.unavailable
    );
}

/// `sources` narrows, and a memory type leaves rooms out (they are untyped).
#[tokio::test]
async fn sources_and_type_narrow_what_is_read() {
    let (store, vta) = store_with("home");
    let (alice, alice_key) = did_key();
    vta.room_record(
        DEALS,
        sealed(
            (&alice, &alice_key),
            DEALS,
            "k",
            1,
            "active",
            json!({ "title": "Tabs" }),
        )
        .await,
    );
    store
        .save(&MemoryRecord::new(
            MemoryType::User,
            "Tabs",
            "indent with tabs",
            "",
            vec![],
        ))
        .await
        .unwrap();
    let configured = [source(DEALS, "deals")];

    let personal = Sources::select(Some(&["personal".into()]), &configured).unwrap();
    let got = rooms::recall(&store, &personal, "tabs", None, 10, &resolver())
        .await
        .unwrap();
    assert_eq!(got.found.len(), 1);
    assert!(!vta.recorded().iter().any(|(u, _)| u.contains("/rooms/")));

    let all = Sources::select(None, &configured).unwrap();
    let typed = rooms::recall(
        &store,
        &all,
        "tabs",
        Some(MemoryType::User),
        10,
        &resolver(),
    )
    .await
    .unwrap();
    assert_eq!(typed.found.len(), 1);
    assert!(matches!(typed.found[0], Found::Memory(_)));

    let both = rooms::recall(&store, &all, "tabs", None, 10, &resolver())
        .await
        .unwrap();
    assert_eq!(both.found.len(), 2);
}
