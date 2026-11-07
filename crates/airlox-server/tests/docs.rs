//! Shared docs on the relay: first save wins, kept versions, runners shut out.

mod common;

use airlox_core::cert::{issue_cert, sign_join, ClientKind, JoinRequest, RunnerInfo, SandboxKind};
use airlox_core::doc::{DocInfo, DocTab};
use airlox_core::item::{new_item_id, seal, ItemKind, ItemRef, NewItem, SealedItem};
use airlox_core::keys::Keypair;
use airlox_core::payload::{Meta, Payload};
use airlox_core::session::JobKind;
use airlox_core::session::{ContentType, Mode, Role};
use common::*;
use reqwest::StatusCode;
use serde_json::Value;

fn doc_session(mode: Mode) -> Sess {
    new_session_with(mode, Some(3_600_000), |p| p.allowed_types.push(ContentType::Doc))
}

fn seal_doc(s: &Sess, m: &Member, sub: ContentType, refs: Vec<ItemRef>, meta: Meta) -> SealedItem {
    seal(
        &s.secrets.session_key(),
        &m.kp,
        NewItem {
            id: new_item_id(),
            session_id: s.id.clone(),
            kind: ItemKind::Doc,
            subkind: Some(sub),
            created_at_ms: now_ms(),
            refs,
        },
        &Payload { meta, body: vec![] }.encode().unwrap(),
    )
    .unwrap()
}

fn root(s: &Sess, m: &Member) -> SealedItem {
    seal_doc(
        s,
        m,
        ContentType::Root,
        vec![],
        Meta::DocRoot(DocInfo { v: 1, title: "Plan".into(), order: vec![] }),
    )
}

fn tab(s: &Sess, m: &Member, refs: Vec<ItemRef>, md: &str) -> SealedItem {
    let t =
        DocTab { v: 1, name: "Main".into(), md: md.into(), deleted: false, note: String::new(), to: None };
    seal_doc(s, m, ContentType::Tab, refs, Meta::DocTab(t))
}

async fn join_runner(env: &Env, s: &Sess) -> Member {
    let kp = Keypair::generate();
    let cert = issue_cert(&s.secrets.role_keypair(Role::Runner), &s.id, Role::Runner, kp.public(), now_ms())
        .unwrap();
    let runner = RunnerInfo {
        name: "box".into(),
        labels: vec![],
        host: "h".into(),
        sandbox: SandboxKind::Docker,
        network_unblocked: false,
        version: None,
        python: None,
        kinds: vec![JobKind::Python],
        shell: None,
        slots: 1,
    };
    let j = JoinRequest {
        v: 1,
        session_id: s.id.clone(),
        cert,
        display_name: "box".into(),
        description: None,
        kind: ClientKind::Runner,
        runner: Some(runner),
        created_at_ms: now_ms(),
    };
    let r = post_join(env, s, canon(&sign_join(&kp, j).unwrap())).await;
    assert_eq!(r.status(), StatusCode::CREATED, "{}", r.text().await.unwrap());
    let v: Value = r.json().await.unwrap();
    Member { kp, token: v["token"].as_str().unwrap().to_string() }
}

async fn present(env: &Env, s: &Sess, m: &Member, i: &SealedItem) -> bool {
    blob(env, s, m, &i.header().id).await.status() == StatusCode::OK
}

async fn ok(env: &Env, s: &Sess, m: &Member, i: &SealedItem) {
    let r = post(env, s, m, frame(i)).await;
    assert_eq!(r.status(), StatusCode::CREATED, "{}", r.text().await.unwrap());
}

#[tokio::test]
async fn first_save_wins_and_the_loser_learns_the_head() {
    let env = Env::new().await;
    let s = doc_session(Mode::Relay);
    assert_eq!(create(&env, &s).await.status(), StatusCode::CREATED);
    let owner = join(&env, &s, Role::Owner, "owner").await;
    let a = join(&env, &s, Role::Sender, "a").await;
    let b = join(&env, &s, Role::Sender, "b").await;

    let r = root(&s, &owner);
    ok(&env, &s, &owner, &r).await;
    let main = tab(&s, &owner, vec![ItemRef::to(&r)], "v1");
    ok(&env, &s, &owner, &main).await;

    let x = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(&main)], "a's edit");
    let y = tab(&s, &b, vec![ItemRef::to(&r), ItemRef::to(&main)], "b's edit");
    ok(&env, &s, &a, &x).await;
    let resp = post(&env, &s, &b, frame(&y)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"], "stale_base");
    assert_eq!(v["head"]["id"], x.header().id.as_str());
    assert!(v["head_stamp"].as_str().unwrap().starts_with("v2·"));
    // Nothing of the loser was stored.
    assert_eq!(blob(&env, &s, &b, &y.header().id).await.status(), StatusCode::NOT_FOUND);

    // A stray tab for a doc that does not exist, and a base from nowhere.
    let other = root(&s, &owner);
    let stray = tab(&s, &a, vec![ItemRef::to(&other)], "x");
    assert_eq!(error_code(post(&env, &s, &a, frame(&stray)).await).await.1, "unknown_doc");
    let nowhere = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(&other)], "x");
    assert_eq!(error_code(post(&env, &s, &a, frame(&nowhere)).await).await.1, "unknown_base");
}

#[tokio::test]
async fn sessions_must_allow_docs_and_observers_cannot_write() {
    let env = Env::new().await;
    let s = new_session(Mode::Relay, 3_600_000);
    create(&env, &s).await;
    let owner = join(&env, &s, Role::Owner, "owner").await;
    assert_eq!(post(&env, &s, &owner, frame(&root(&s, &owner))).await.status(), StatusCode::FORBIDDEN);

    let s = doc_session(Mode::Relay);
    create(&env, &s).await;
    let obs = join(&env, &s, Role::Observer, "obs").await;
    assert_eq!(post(&env, &s, &obs, frame(&root(&s, &obs))).await.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn keeps_recent_versions_and_the_owners_latest_across_restarts() {
    let mut env = Env::with(|c| {
        c.blob_hold_ms = 300;
        c.doc_keep_versions = 2;
    })
    .await;
    let s = doc_session(Mode::Relay);
    create(&env, &s).await;
    let owner = join(&env, &s, Role::Owner, "owner").await;
    let a = join(&env, &s, Role::Sender, "a").await;
    let r = root(&s, &owner);
    ok(&env, &s, &owner, &r).await;
    let first = tab(&s, &owner, vec![ItemRef::to(&r)], "owner's text");
    ok(&env, &s, &owner, &first).await;
    let mut chain = vec![first.clone()];
    for i in 0..4 {
        let next = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(chain.last().unwrap())], &format!("a {i}"));
        ok(&env, &s, &a, &next).await;
        chain.push(next);
    }
    // Long past the hold: the root, the owner's version and the last two stay.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert!(present(&env, &s, &a, &r).await, "the root is the meta chain's only version");
    assert!(present(&env, &s, &a, &first).await, "the owner's latest is pinned");
    assert!(!present(&env, &s, &a, &chain[1]).await && !present(&env, &s, &a, &chain[2]).await, "pushed out");
    assert!(present(&env, &s, &a, &chain[3]).await && present(&env, &s, &a, &chain[4]).await);

    // A restart rebuilds the same chains: the head check still holds.
    env.restart(|c| {
        c.blob_hold_ms = 300;
        c.doc_keep_versions = 2;
    })
    .await;
    let stale = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(&chain[3])], "late");
    assert_eq!(error_code(post(&env, &s, &a, frame(&stale)).await).await.1, "stale_base");
    let next = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(&chain[4])], "next");
    ok(&env, &s, &a, &next).await;
    assert!(present(&env, &s, &a, &chain[4]).await);
}

#[tokio::test]
async fn late_joiners_read_docs_and_runners_do_not() {
    let env = Env::new().await;
    let s = doc_session(Mode::Execute);
    create(&env, &s).await;
    let owner = join(&env, &s, Role::Owner, "owner").await;
    let r = root(&s, &owner);
    ok(&env, &s, &owner, &r).await;
    let late = join(&env, &s, Role::Observer, "late").await;
    assert_eq!(blob(&env, &s, &late, &r.header().id).await.status(), StatusCode::OK);
    let runner = join_runner(&env, &s).await;
    assert_eq!(blob(&env, &s, &runner, &r.header().id).await.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn doc_limits() {
    let env = Env::with(|c| {
        c.max_doc_items = 3;
        c.doc_posts_per_minute = 100;
    })
    .await;
    let s = doc_session(Mode::Relay);
    create(&env, &s).await;
    let owner = join(&env, &s, Role::Owner, "owner").await;
    let r = root(&s, &owner);
    ok(&env, &s, &owner, &r).await;
    let t1 = tab(&s, &owner, vec![ItemRef::to(&r)], "1");
    ok(&env, &s, &owner, &t1).await;
    let t2 = tab(&s, &owner, vec![ItemRef::to(&r), ItemRef::to(&t1)], "2");
    ok(&env, &s, &owner, &t2).await;
    let t3 = tab(&s, &owner, vec![ItemRef::to(&r), ItemRef::to(&t2)], "3");
    assert_eq!(error_code(post(&env, &s, &owner, frame(&t3)).await).await.1, "session_full");

    let env = Env::with(|c| c.doc_posts_per_minute = 2).await;
    let s = doc_session(Mode::Relay);
    create(&env, &s).await;
    let owner = join(&env, &s, Role::Owner, "owner").await;
    let r = root(&s, &owner);
    ok(&env, &s, &owner, &r).await;
    ok(&env, &s, &owner, &tab(&s, &owner, vec![ItemRef::to(&r)], "1")).await;
    let third = tab(&s, &owner, vec![ItemRef::to(&r)], "2");
    assert_eq!(error_code(post(&env, &s, &owner, frame(&third)).await).await.1, "limit");
}

#[tokio::test]
async fn runners_get_no_doc_wants_or_usage() {
    let env = Env::new().await;
    let s = doc_session(Mode::Execute);
    create(&env, &s).await;
    let owner = join(&env, &s, Role::Owner, "owner").await;
    let r = root(&s, &owner);
    ok(&env, &s, &owner, &r).await;
    let runner = join_runner(&env, &s).await;
    assert_eq!(want(&env, &s, &runner, &r.header().id).await.status(), StatusCode::FORBIDDEN);
    let usage = |m: &Member| {
        env.http.get(env.url(&format!("/v1/sessions/{}/usage", s.id))).bearer_auth(&m.token).send()
    };
    assert_eq!(usage(&runner).await.unwrap().status(), StatusCode::FORBIDDEN);
    let u: Value = usage(&owner).await.unwrap().json().await.unwrap();
    assert_eq!(u["doc_items"], 1);
    assert!(u["max_doc_items"].as_u64().unwrap() > 0 && u["idle_ttl_ms"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn the_owner_keeps_a_share_of_doc_saves_and_items_are_capped() {
    let env = Env::with(|c| {
        c.max_doc_items = 10;
        c.doc_posts_per_minute = 100;
    })
    .await;
    // Files may be large here; doc items still may not.
    let s = new_session_with(Mode::Relay, Some(3_600_000), |p| {
        p.allowed_types.push(ContentType::Doc);
        p.max_item_bytes = 4 * 1024 * 1024;
    });
    create(&env, &s).await;
    let owner = join(&env, &s, Role::Owner, "owner").await;
    let a = join(&env, &s, Role::Sender, "a").await;
    let r = root(&s, &owner);
    ok(&env, &s, &owner, &r).await;
    // A doc item larger than a tab can be is refused, whatever it holds.
    let big = tab(&s, &a, vec![ItemRef::to(&r)], &"x".repeat(1100 * 1024));
    let (status, _) = error_code(post(&env, &s, &a, frame(&big)).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Senders may use 9 of the 10 saves; the last is the owner's.
    let mut head = tab(&s, &a, vec![ItemRef::to(&r)], "0");
    ok(&env, &s, &a, &head).await;
    for i in 1..8 {
        let next = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(&head)], &i.to_string());
        ok(&env, &s, &a, &next).await;
        head = next;
    }
    let refused = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(&head)], "a");
    assert_eq!(error_code(post(&env, &s, &a, frame(&refused)).await).await.1, "session_full");
    ok(&env, &s, &owner, &tab(&s, &owner, vec![ItemRef::to(&r), ItemRef::to(&head)], "owner")).await;
}

#[tokio::test]
async fn a_save_that_frees_what_it_adds_is_not_refused_for_size() {
    let blob_len = |i: &SealedItem| i.header().blob_len().unwrap();
    let env0 = Env::new().await;
    let s0 = doc_session(Mode::Relay);
    create(&env0, &s0).await;
    let o0 = join(&env0, &s0, Role::Owner, "owner").await;
    let r0 = root(&s0, &o0);
    let sample = tab(&s0, &o0, vec![ItemRef::to(&r0)], &"y".repeat(1000));
    // Senders get three quarters: room for the root and one and a half tab
    // versions, so every save below fits only once the one it replaces goes.
    let cap = (blob_len(&r0) + blob_len(&sample) * 3 / 2) * 4 / 3 + 4;
    let env = Env::with(|c| {
        c.max_doc_bytes = cap;
        c.doc_keep_versions = 1;
        c.doc_posts_per_minute = 100;
    })
    .await;
    let s = doc_session(Mode::Relay);
    create(&env, &s).await;
    let owner = join(&env, &s, Role::Owner, "owner").await;
    let a = join(&env, &s, Role::Sender, "a").await;
    let r = root(&s, &owner);
    ok(&env, &s, &owner, &r).await;
    let first = tab(&s, &a, vec![ItemRef::to(&r)], &"y".repeat(1000));
    ok(&env, &s, &a, &first).await;
    let mut head = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(&first)], &"z".repeat(1000));
    ok(&env, &s, &a, &head).await;
    // At the cap, each save pushes the previous one out: still accepted.
    for i in 0..3 {
        let next = tab(&s, &a, vec![ItemRef::to(&r), ItemRef::to(&head)], &format!("{i}").repeat(1000));
        ok(&env, &s, &a, &next).await;
        head = next;
    }
}
