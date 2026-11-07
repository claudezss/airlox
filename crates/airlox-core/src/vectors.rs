//! Deterministic test vectors (`vectors/v1.json`).
//!
//! Every binding must reproduce the derived values and pass the verification
//! steps using only the inputs in the file. Regenerate with:
//!
//! ```text
//! cargo run -p airlox-core --example gen_vectors > crates/airlox-core/vectors/v1.json
//! ```

use serde_json::{json, Value};

use crate::cert::{issue_cert, sign_join, ClientKind, JoinRequest, RunnerInfo, SandboxKind};
use crate::doc::{DocInfo, DocTab, DOC_V};
use crate::encoding::{b64e, canonical_json};
use crate::invite::Invite;
use crate::item::{item_key, seal_with_nonce, ItemKind, ItemRef, NewItem, SealedItem};
use crate::keys::Keypair;
use crate::payload::{JobResult, JobSpec, JobStatus, Meta, OutFile, Payload, Target};
use crate::session::{ContentType, JobKind, JobLimits, Mode, Role, SessionParams, SessionSecrets};
use crate::Result;

pub const BASE_URL: &str = "https://airlox.example";

fn s(b: Vec<u8>) -> String {
    String::from_utf8(b).expect("utf-8")
}

fn framed(meta: &[u8]) -> Vec<u8> {
    let mut v = (meta.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(meta);
    v
}

fn negative_invites(secrets: &SessionSecrets) -> Result<Value> {
    let good = Invite::member(BASE_URL, secrets, Role::Sender)?.to_url();
    let owner = Invite::owner(BASE_URL, secrets)?.to_url();
    let other = SessionSecrets::from_owner_seed([0xEE; 32]);
    let forged_owner =
        Invite::owner(BASE_URL, &other)?.to_url().replace(&other.session_id(), &secrets.session_id());
    Ok(json!([
        {"name": "wrong_version", "url": good.replace("v=1", "v=2")},
        {"name": "duplicate_role", "url": format!("{good}&r=view")},
        {"name": "query_instead_of_fragment", "url": good.replace('#', "?")},
        {"name": "owner_link_with_member_key", "url": format!("{owner}&k=AAAA")},
        {"name": "owner_seed_for_other_session", "url": forged_owner},
        {"name": "short_session_id", "url": good.replace(&secrets.session_id(), "ax_aaaaaaaaaaaa")},
    ]))
}

pub fn generate() -> Result<Value> {
    let owner_seed: [u8; 32] = std::array::from_fn(|i| i as u8);
    let secrets = SessionSecrets::from_owner_seed(owner_seed);
    let params = SessionParams {
        mode: Mode::Execute,
        created_at_ms: 1_761_400_000_000,
        ttl_ms: Some(3_600_000),
        allowed_types: vec![
            ContentType::Text,
            ContentType::Py,
            ContentType::Md,
            ContentType::Zip,
            ContentType::Image,
            ContentType::Doc,
        ],
        max_item_bytes: 26_214_400,
        job_limits: Some(JobLimits {
            packages: vec!["matplotlib".into(), "numpy".into(), "pandas".into()],
            memory_mb: 512,
            ..JobLimits::default()
        }),
        insecure_http_host: None,
    };
    let signed = secrets.manifest(&params)?;
    let sid = signed.manifest.session_id.clone();
    let key = secrets.session_key();

    let participants = [
        (Role::Owner, [0x33u8; 32]),
        (Role::Sender, [0x11u8; 32]),
        (Role::Runner, [0x22u8; 32]),
        (Role::Observer, [0x44u8; 32]),
    ];
    let mut certs = Vec::new();
    for (role, seed) in &participants {
        let p = Keypair::from_seed(seed);
        let c = issue_cert(&secrets.role_keypair(*role), &sid, *role, p.public(), params.created_at_ms + 1)?;
        certs.push(json!({
            "role": role.as_str(),
            "participant_seed": b64e(seed),
            "participant_pub": p.public().to_b64(),
            "participant_fingerprint": p.public().fingerprint(),
            "canonical": s(canonical_json(&c.cert)?),
            "signed": c,
            "signed_canonical": s(canonical_json(&c)?),
        }));
    }

    let runner_p = Keypair::from_seed(&participants[2].1);
    let runner_cert = issue_cert(
        &secrets.role_keypair(Role::Runner),
        &sid,
        Role::Runner,
        runner_p.public(),
        params.created_at_ms + 1,
    )?;
    let join = sign_join(
        &runner_p,
        JoinRequest {
            v: 1,
            session_id: sid.clone(),
            cert: runner_cert,
            display_name: "runner-01".into(),
            description: None,
            kind: ClientKind::Runner,
            runner: Some(RunnerInfo {
                name: "runner-01".into(),
                labels: vec!["cpu".into(), "py3.12".into()],
                host: "build-box".into(),
                sandbox: SandboxKind::Docker,
                network_unblocked: false,
                version: None,
                python: None,
                kinds: vec![JobKind::Python],
                shell: None,
                slots: 1,
            }),
            created_at_ms: params.created_at_ms + 2,
        },
    )?;

    let sender = Keypair::from_seed(&participants[1].1);
    let mut items = Vec::new();
    let mut sealed_items = Vec::new();
    let mut seal_case = |name: &str,
                         author: &Keypair,
                         role: Role,
                         kind: ItemKind,
                         subkind: Option<ContentType>,
                         refs: Vec<ItemRef>,
                         payload: Payload,
                         id: &str|
     -> Result<SealedItem> {
        let i = items.len();
        let new = NewItem {
            id: id.into(),
            session_id: sid.clone(),
            kind,
            subkind,
            created_at_ms: params.created_at_ms + 10 + i as u64,
            refs,
        };
        let nonce = [0xA0 + i as u8; 12];
        let encoded = payload.encode()?;
        let sealed = seal_with_nonce(&key, author, new.clone(), &encoded, nonce)?;
        items.push(json!({
            "name": name,
            "author_role": role.as_str(),
            "author_seed": b64e(&author.seed()),
            "author_cert_index": participants.iter().position(|(r, _)| *r == role).expect("role"),
            "new_item": new,
            "nonce": b64e(&nonce),
            "payload": b64e(&encoded),
            "payload_meta_canonical": s(canonical_json(&payload.meta)?),
            "item_key": b64e(&item_key(&key, &sid, id)),
            "header_canonical": s(sealed.header().canonical_bytes()?),
            "envelope": sealed.envelope,
            "envelope_canonical": s(canonical_json(&sealed.envelope)?),
            "blob": b64e(&sealed.blob),
            "blob_sha256": b64e(&sealed.blob_sha256()),
        }));
        Ok(sealed)
    };

    let text = seal_case(
        "text",
        &sender,
        Role::Sender,
        ItemKind::Text,
        None,
        vec![],
        Payload::text("Cleaned dataset attached. Ünïcödé, \u{2028}line sep, \"quotes\" and \\ back\\slash."),
        "AAAAAAAAAAAAAAAAAAAAAA",
    )?;
    let readme = seal_case(
        "markdown_file",
        &sender,
        Role::Sender,
        ItemKind::File,
        Some(ContentType::Md),
        vec![ItemRef::to(&text)],
        Payload::file("README.md", "text/markdown", b"# Orders\n\nDeduplicated export.\n".to_vec()),
        "AQEBAQEBAQEBAQEBAQEBAQ",
    )?;
    let job_spec = JobSpec {
        kind: None,
        cwd: None,
        tree: None,
        fresh: false,
        filename: "summarize.py".into(),
        code: "print(sum(range(10)))\n".into(),
        runtime: "python3.12".into(),
        timeout_s: 30,
        memory_mb: None,
        files: vec![ItemRef::to(&readme)],
        requirements: vec!["pandas".into()],
        target: Target::Runner { participant_pub: runner_p.public(), name: "runner-01".into() },
        progress_file: None,
    };
    let job = seal_case(
        "job",
        &sender,
        Role::Sender,
        ItemKind::Job,
        Some(ContentType::Py),
        vec![ItemRef::to(&readme)],
        Payload::job(job_spec.clone()),
        "AwMDAwMDAwMDAwMDAwMDAw",
    )?;
    let result = JobResult {
        job: ItemRef::to(&job),
        exit_code: Some(0),
        status: JobStatus::Completed,
        reason: None,
        wall_ms: 1840,
        peak_mem_mb: 212,
        stdout: "45\n".into(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        // Points at an output item not included in these vectors.
        out_files: vec![OutFile {
            name: "chart.png".into(),
            size: 38_912,
            item: ItemRef { id: "BAQEBAQEBAQEBAQEBAQEBA".into(), blob_sha256: b64e(&[4u8; 32]) },
        }],
        code_sha256: crate::encoding::hex(&crate::encoding::sha256(job_spec.code.as_bytes())),
        input_sha256: crate::encoding::hex(&crate::encoding::sha256(&readme.blob)),
        env: None,
    };
    seal_case(
        "result",
        &runner_p,
        Role::Runner,
        ItemKind::Result,
        None,
        vec![ItemRef::to(&job)],
        Payload::result(result),
        "BQUFBQUFBQUFBQUFBQUFBQ",
    )?;

    // A shared doc: the owner creates it with a Main tab, a sender writes v2
    // and reorders; a second write on v1 comes after v2 and is stale.
    let owner_p = Keypair::from_seed(&participants[0].1);
    let doc = |meta: Meta| Payload { meta, body: vec![] };
    let tab = |md: &str, note: &str, to: Option<Vec<crate::keys::PublicKey>>| {
        doc(Meta::DocTab(DocTab {
            v: DOC_V,
            name: "Main".into(),
            md: md.into(),
            deleted: false,
            note: note.into(),
            to,
        }))
    };
    let root = seal_case(
        "doc_root",
        &owner_p,
        Role::Owner,
        ItemKind::Doc,
        Some(ContentType::Root),
        vec![],
        doc(Meta::DocRoot(DocInfo { v: DOC_V, title: "Orders cleanup".into(), order: vec![] })),
        "BgYGBgYGBgYGBgYGBgYGBg",
    )?;
    let v1 = seal_case(
        "doc_tab_create",
        &owner_p,
        Role::Owner,
        ItemKind::Doc,
        Some(ContentType::Tab),
        vec![ItemRef::to(&root)],
        tab("# Plan\n- [ ] dedupe\n", "first draft", None),
        "BwcHBwcHBwcHBwcHBwcHBw",
    )?;
    seal_case(
        "doc_tab_version",
        &sender,
        Role::Sender,
        ItemKind::Doc,
        Some(ContentType::Tab),
        vec![ItemRef::to(&root), ItemRef::to(&v1)],
        tab("# Plan\n- [x] dedupe\n", "deduped", Some(vec![owner_p.public()])),
        "CAgICAgICAgICAgICAgICA",
    )?;
    seal_case(
        "doc_meta",
        &sender,
        Role::Sender,
        ItemKind::Doc,
        Some(ContentType::Meta),
        vec![ItemRef::to(&root), ItemRef::to(&root)],
        doc(Meta::DocMeta(DocInfo {
            v: DOC_V,
            title: "Orders cleanup (done)".into(),
            order: vec![v1.header().id.clone()],
        })),
        "CQkJCQkJCQkJCQkJCQkJCQ",
    )?;
    seal_case(
        "doc_tab_stale",
        &sender,
        Role::Sender,
        ItemKind::Doc,
        Some(ContentType::Tab),
        vec![ItemRef::to(&root), ItemRef::to(&v1)],
        tab("# Plan\n- [ ] dedupe\n- [ ] export\n", "", None),
        "CgoKCgoKCgoKCgoKCgoKCg",
    )?;
    // Doc items in relay order, as `doc_index` takes them, and its answer:
    // the stale write is left out of `docs` and listed in `skipped`.
    let doc_entries: Vec<Value> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| it["new_item"]["kind"] == "doc")
        .map(|(seq, it)| {
            json!({"envelope": it["envelope_canonical"], "blob_sha256": it["blob_sha256"],
                   "owner": it["author_role"] == "owner", "seq": seq})
        })
        .collect();
    let doc_index: Value =
        serde_json::from_str(&crate::api::doc_index(&serde_json::to_string(&doc_entries)?)?)?;
    sealed_items.push(text);

    // Negative cases built from the first item.
    let base = sealed_items.remove(0);
    let mut flipped = base.blob.clone();
    *flipped.last_mut().expect("non-empty") ^= 0x01;
    let mut relabeled = base.envelope.clone();
    relabeled.header.created_at_ms += 1;

    Ok(json!({
        "version": 1,
        "description": "Airlox protocol v1 test vectors. All binary values are base64url without padding.",
        "inputs": {
            "owner_seed": b64e(&owner_seed),
            "base_url": BASE_URL,
            "session_params": params,
        },
        "session": {
            "session_id": sid,
            "session_key": b64e(&key),
            "owner_pub": secrets.owner().public().to_b64(),
            "role_seeds": {
                "sender": b64e(&secrets.role_seed(Role::Sender)),
                "runner": b64e(&secrets.role_seed(Role::Runner)),
                "observer": b64e(&secrets.role_seed(Role::Observer)),
            },
        },
        "manifest": {
            "canonical": s(signed.manifest.canonical_bytes()?),
            "signed": signed,
            "signed_canonical": s(canonical_json(&signed)?),
        },
        "invites": {
            "owner": Invite::owner(BASE_URL, &secrets)?.to_url(),
            "sender": Invite::member(BASE_URL, &secrets, Role::Sender)?.to_url(),
            "runner": Invite::member(BASE_URL, &secrets, Role::Runner)?.to_url(),
            "observer": Invite::member(BASE_URL, &secrets, Role::Observer)?.to_url(),
        },
        "certs": certs,
        "join": {
            "canonical": s(canonical_json(&join.join)?),
            "signed": join,
            "signed_canonical": s(canonical_json(&join)?),
        },
        "items": items,
        "docs": {"entries": doc_entries, "index": doc_index},
        "negative_items": [
            {"name": "flipped_blob_byte", "envelope_canonical": s(canonical_json(&base.envelope)?),
             "blob": b64e(&flipped), "expect": "signature"},
            {"name": "header_changed_after_signing", "envelope_canonical": s(canonical_json(&relabeled)?),
             "blob": b64e(&base.blob), "expect": "signature"},
            {"name": "wrong_session_key", "envelope_canonical": s(canonical_json(&base.envelope)?),
             "blob": b64e(&base.blob), "session_key": b64e(&[0u8; 32]), "expect": "decrypt"},
            {"name": "non_canonical_envelope", "envelope_canonical": serde_json::to_string_pretty(&base.envelope)?,
             "blob": b64e(&base.blob), "expect": "invalid"},
            {"name": "missing_null_subkind", "envelope_canonical": s(canonical_json(&base.envelope)?).replace("\"subkind\":null,", ""),
             "blob": b64e(&base.blob), "expect": "invalid"},
        ],
        "negative_invites": negative_invites(&secrets)?,
        // Links that parse but that the manifest above refuses: it names no
        // host for plain http.
        "negative_invites_for_manifest": [
            {"name": "plain_http", "url": Invite::member(BASE_URL, &secrets, Role::Sender)?.to_url().replace("https://", "http://")},
        ],
        "negative_payloads": [
            {"name": "truncated", "payload": b64e(&[0, 0, 0])},
            {"name": "meta_length_past_end", "payload": b64e(&[0, 0, 0, 9, b'{', b'}'])},
            {"name": "non_canonical_meta", "payload": b64e(&framed(br#"{"text":"hi", "type":"text"}"#))},
            {"name": "body_on_text", "payload": b64e(&[framed(br#"{"text":"hi","type":"text"}"#), b"x".to_vec()].concat())},
            {"name": "float_in_meta", "payload": b64e(&framed(br#"{"text":"hi","type":"text","x":1.5}"#))},
        ],
        "negative_canonical": [
            {"name": "pretty_printed_manifest", "signed_manifest": serde_json::to_string_pretty(&signed)?},
            {"name": "space_after_colon", "signed_manifest": s(canonical_json(&signed)?).replacen("\"v\":1", "\"v\": 1", 1)},
        ],
    }))
}
