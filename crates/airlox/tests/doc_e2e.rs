//! `airlox doc` against an in-process relay: two writers race on one
//! version, the loser is told and merges, and a participant who joins after
//! old versions were evicted still sees the same chain and can write.

use std::path::Path;
use std::process::Output;

use airlox_core::invite::Invite;
use airlox_core::session::{ContentType, Mode, Role};
use serde_json::Value;

#[path = "../../airlox-server/tests/common/mod.rs"]
mod common;
use common::*;

fn airlox(invite: &str, cache: &Path, name: &str, args: &[&str], stdin: Option<&str>) -> Output {
    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_airlox"))
        .args(args)
        .env("AIRLOX_INVITE", invite)
        .env("AIRLOX_CACHE_DIR", cache)
        .env("AIRLOX_NAME", name)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.unwrap_or("").as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn json_of(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|_| {
        panic!("{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
    })
}

fn stamp(invite: &str, cache: &Path, name: &str) -> String {
    let o = airlox(invite, cache, name, &["doc", "show", "--json"], None);
    json_of(&o)["tabs"][0]["stamp"].as_str().unwrap().to_string()
}

fn edit(invite: &str, cache: &Path, name: &str, base: &str, text: &str) -> Output {
    airlox(
        invite,
        cache,
        name,
        &["doc", "edit", "--tab", "main", "--base", base, "--file", "-", "--json"],
        Some(text),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn writers_race_and_late_joiners_agree() {
    let env = Env::with(|c| {
        c.blob_hold_ms = 1500;
        c.blob_grace_ms = 200;
        c.doc_keep_versions = 2;
    })
    .await;
    let s = new_session_with(Mode::Relay, Some(3_600_000), |p| p.allowed_types.push(ContentType::Doc));
    create(&env, &s).await;
    let owner = Invite::owner(&env.base, &s.secrets).unwrap().to_url();
    let sender = Invite::member(&env.base, &s.secrets, Role::Sender).unwrap().to_url();
    let dir = tempfile::tempdir().unwrap();
    let (c_owner, c_a, c_b, c_late) =
        (dir.path().join("o"), dir.path().join("a"), dir.path().join("b"), dir.path().join("late"));

    let (owner2, c_owner2) = (owner.clone(), c_owner.clone());
    let new = tokio::task::spawn_blocking(move || {
        airlox(&owner2, &c_owner2, "owner", &["doc", "new", "Plan", "--from", "-", "--json"], Some("start\n"))
    })
    .await
    .unwrap();
    assert!(new.status.success(), "{}", String::from_utf8_lossy(&new.stderr));

    let (sender2, c_a2, c_b2) = (sender.clone(), c_a.clone(), c_b.clone());
    tokio::task::spawn_blocking(move || {
        // Both read v1, both write: the first wins, the second posts nothing.
        let base_a = stamp(&sender2, &c_a2, "a");
        let base_b = stamp(&sender2, &c_b2, "b");
        assert_eq!(base_a, base_b);
        let won = edit(&sender2, &c_a2, "a", &base_a, "start\nfrom a\n");
        assert!(won.status.success(), "{}", String::from_utf8_lossy(&won.stdout));
        let lost = edit(&sender2, &c_b2, "b", &base_b, "start\nfrom b\n");
        assert_eq!(lost.status.code(), Some(3));
        let v = json_of(&lost);
        assert_eq!(v["reason"], "stale");
        assert_eq!(v["current_md"], "start\nfrom a\n");
        // b merges and writes on the current version.
        let merged = edit(&sender2, &c_b2, "b", v["current"].as_str().unwrap(), "start\nfrom a\nfrom b\n");
        assert!(merged.status.success());
        // A stamp that names no version is refused before anything is posted.
        let bogus = edit(&sender2, &c_b2, "b", "v9·00000000", "x");
        assert!(!bogus.status.success());
        // More versions, so the early ones leave the relay.
        for i in 0..3 {
            let base = stamp(&sender2, &c_a2, "a");
            assert!(edit(&sender2, &c_a2, "a", &base, &format!("edit {i}\n")).status.success());
        }
    })
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
    tokio::task::spawn_blocking(move || {
        let show = json_of(&airlox(&sender, &c_late, "late", &["doc", "show", "--json"], None));
        let tab = &show["tabs"][0];
        assert_eq!(tab["versions"], 6, "the late joiner rebuilds the whole chain from headers");
        assert_eq!(tab["md"], "edit 2\n");
        let log = json_of(&airlox(&sender, &c_late, "late", &["doc", "log", "--json"], None));
        let available: Vec<bool> =
            log["versions"].as_array().unwrap().iter().map(|v| v["available"].as_bool().unwrap()).collect();
        // Newest first: the last two, then evicted ones, then the owner's pinned first version.
        assert_eq!(available, [true, true, false, false, false, true]);
        let base = tab["stamp"].as_str().unwrap();
        let w = edit(&sender, &c_late, "late", base, "late\n");
        assert!(w.status.success(), "{}", String::from_utf8_lossy(&w.stdout));
    })
    .await
    .unwrap();
}
