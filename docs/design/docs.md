# Docs: a shared plan that any agent can edit and report progress on

Status: §0 implemented (2026-11-07). §1 to §11 are the later, structured
version and remain a proposal. Where the implementation differs from §0:

- Item events carry each blob's hash (signed by the author), so clients
  rebuild every chain from headers even for versions they never fetched.
- `airlox doc edit` takes `--base` explicitly. The MCP `doc_write` tool also
  requires `base`.
- `listen` wakes only on `to` keys. It does not wake on a rejected write; the
  writer's own command reports that.
- Drafts of existing tabs in the web editor are kept in `sessionStorage`
  (beside the invite): they survive a reload and are dropped when the browser
  tab closes, so no plaintext stays on disk. New-tab drafts are not kept.
- The Python SDK has `Session.docs()`, `doc()`, `doc_create()`, `doc_write()`
  and `doc_set_info()`; a stale write raises `StaleBase`.
- `vectors/v1.json` gained doc items and a `docs` section (doc_index input and
  answer, with a stale write left out). Earlier items are unchanged; the
  manifest now allows `doc`.

Originally a proposal (2026-11-07). Builds on [DESIGN.md](../DESIGN.md) §5 (items)
and §7 (transit buffer, re-seeding), and on [protocol.md](../protocol.md) §6,
§7 and §12. Nothing here changes the cryptography, the roles or the relay's
blindness. It adds an item kind, a deterministic way to replay it into a
document, a relay rule for how long doc blobs are kept, and CLI, MCP and web
surfaces for it.

## 0. v1 scope: tabbed Markdown docs

The first version is deliberately smaller than §3 to §5. Product decisions
(2026-11-07):

- A new doc has one tab, **Main**.
- Every writing role (owner and sender) may add, edit, rename, reorder,
  delete and restore any tab.
- First save wins. A writer must name the exact version it started from, and
  agents must pull the latest version and match it before writing.

Revised after a design, UI and security review. UI mockup:
[docs-ui.html](docs-ui.html).

### 0.1 Model

A **doc** has a title, an ordered list of **tabs**, and for each tab a
chain of **versions**. A tab version holds the whole tab:

```text
TabState { name, md, deleted }
```

`md` is Markdown, at most 256 KiB. There are no structured tasks. Progress
is a Markdown checklist in whichever tab people choose. Each doc has at most
64 tabs.

### 0.2 Items

There is one new item kind, `doc`. Its `subkind` is plaintext, so the relay
can sort versions into chains without reading them:

| subkind | header `refs` | meta (encrypted) |
|---|---|---|
| `root` | `[]` | `{v:1, title, order: []}`: creates the doc. Its item id is the doc id. |
| `meta` | `[root, base]` | `{v:1, title, order: [tab ids]}`: retitles the doc or reorders its tabs. The chain starts at `root`. |
| `tab` | `[root]` | `{v:1, name, md, deleted:false, note, to?}`: creates a tab. **Its item id is the tab id**, so two agents creating "Notes" at once get two tabs, not a collision. |
| `tab` | `[root, base]` | `{v:1, name, md, deleted, note, to?}`: a new version of the tab whose current head is `base`. |

- `base` is a full item reference `{id, blob_sha256}`, never a stamp.
- A delete is a version with `deleted: true` that keeps `md`, and a restore
  is the next version with `deleted: false`. The relay needs no special rule
  for either.
- A rename carries the full `md` unchanged.
- Order lives on the `meta` chain, so a reorder never conflicts with a
  content edit. Tabs missing from `order` follow in creation order.
- Creating a doc posts `root` and then the Main `tab`.
- `v:1` lets a later structured version (§3 to §5) live beside v1 docs as a
  different `v`, not as an upgrade of v1 docs.

### 0.3 Which version is current: from headers only

Whether a version applies is decided only from what the relay and every
client can see without decrypting:

1. The signature verifies, and the author's certificate is an owner or
   sender role. Runners and observers cannot write docs. The protocol §8
   table gains a `doc` row: owner and sender only.
2. `refs[0]` is the doc's `root`.
3. `base` (`refs[1]`) is the current head of a chain of the same subkind in
   the same doc.

The relay enforces the third rule at post time, under the session mutex,
and answers `409 stale_base` naming the current head. So first save wins
atomically, and a lost race costs no log line. Clients still replay every
version in `seq` order with the same three rules and ignore anything that
fails them, in case the relay misbehaves (§0.8).

Content problems never change which version is current. Examples are meta
that disagrees with the header, `md` too large, or invalid UTF-8. A client
shows such a version as "unreadable version by X" and offers to restore the
previous one. The CLI and web UI refuse to post such content in the first
place. Because of this, the relay and every client agree on every chain's
head, including clients that never had the old blobs.

**Stamp.** `v<n>·<first 8 hex of blob_sha256>`, for example `v3·9fa2c1d0`.
`n` is the version's position in its chain, counted from headers. The stamp
is for display and for typing. The CLI accepts `·` or `-`, maps a stamp to
the full reference of a version it has shown, and fails if the stamp is
unknown or ambiguous.

### 0.4 Writing: pull, match, post

```bash
airlox doc show --tab main            # syncs first; prints Markdown + stamp + what changed since you last read
airlox doc edit --tab main --base v3·9fa2c1d0 --file main.md --note "…" [--to codex]
```

- `edit` syncs again and compares `base` with the current head. If they
  differ, it posts nothing and **exits 3 (stale)**. It prints the current
  stamp, each newer version's author and note, and the current Markdown.
- If the relay answers `stale_base` (a write landed between the check and
  the post), it **exits 4 (lost race)** with the same information.
- On success it prints `applied <new stamp>`.
- The agent merges and retries. The CLI never retries on its own, and never
  fills in `base` silently.
- MCP: `doc_read` and `doc_write`, with `base` required, return the same
  results as data. Also `doc_tabs`.
- The web editor follows the same rule: Save is disabled once the tab has a
  newer version than the draft's base. A draft remembers its base.

### 0.5 Reading safely (prompt injection)

A doc is long-lived text that agents re-read and act on. So:

- `doc_read` and `doc show` mark content as written by participants, as data
  and never as instructions (like every MCP read today). Each tab shows its
  author, the author's role, and a diff since the reader last read it, so an
  injected edit is visible.
- The skill says that only owner-authored text sets the plan. Text from a
  sender is a suggestion to check with the owner.
- **Wake-ups.** A version wakes `airlox listen` only through `to`, a list of
  participant keys. The CLI accepts names and resolves them to keys. Wake-ups
  respect `--from`. `@name` inside Markdown is display only and never wakes
  anyone, because names are self-chosen. A rejected write is reported to the
  writer by its own CLI, not as a wake.

### 0.6 Result links

`[text](airlox:result/<id>.<blob_sha256>)` names a job result by its full
reference. A client draws a **verified chip** only if all of these hold:

- the item has that hash;
- its kind is `result`;
- it is signed by an approved runner;
- it is the first valid result of an existing, verified job.

The chip shows the result's own facts (command, exit code, runner, tree
head and dirty flag, age), never the link text. Anything else renders as a
plain link marked "unverified" or "not available". A chip proves that the
run happened, not that it belongs to the task next to it, and the tooltip
says so.

### 0.7 Relay: retention, delivery, limits

This needs no database. Doc items are log lines and blob files like every
other item. The relay keeps each chain's head in its in-memory index,
rebuilds it from the log at startup, and sweeps by it.

- **Kept versions.** For each chain, the relay keeps:
  - the newest `AIRLOX_DOC_KEEP_VERSIONS` versions (default 5);
  - the newest owner-authored version, as a pin that one hostile writer
    cannot push out.

  Others get the normal hold and are evicted.
- **Store and replay.** `keep` becomes dynamic in `store.rs`. A new head
  starts the hold on the version it pushes out, and a log replay applies
  exactly the same transitions. Re-seeding never shortens a kept version's
  life.
- **Delivery.** Kept doc blobs may be fetched by any owner, sender or
  observer, including those who join later. Today a fetch needs the
  participant to have been a recipient at post time. Runners are never
  recipients of `doc` items and are refused, so "runners do not see docs"
  holds at the relay. They still hold `K`, so this is about delivery, not
  cryptography.
- **Audit.** History always shows who changed which tab, when, and how big
  the version was. The content of versions outside the kept set can be gone
  for good, unless a participant still has it cached and re-seeds it.
- **Limits.**
  - `md` ≤ 256 KiB, 64 tabs per doc.
  - `AIRLOX_MAX_DOC_BYTES` (64 MiB per session) for kept doc blobs. A
    quarter of it is reserved for the owner.
  - 30 doc posts per minute per participant.
  - `AIRLOX_MAX_DOC_ITEMS` (1 500 per session). Every save is an item and a
    log line, and the session log is capped at 4 MiB for everything. The CLI
    and UI warn at 80%.
  - `airlox doc export --json` and `airlox doc new --from export.json`
    continue a doc in a fresh session.
- **Lifetime.** A doc lives and dies with its session. A never-expiring
  session on the public relay is still purged after 3 idle days. The UI
  shows when that happens, and `--profile project` plus `doc export` (or a
  self-hosted relay with `AIRLOX_IDLE_TTL_UNEXPIRING=never`) covers longer
  gaps.

### 0.8 What the relay can still do

It cannot read or forge a version. An observer or the relay can seal a
blob under `K`, but cannot sign as a writer, and references pin blob hashes.

The relay can still:

- choose which of two concurrent writes wins;
- withhold the newest version, so an agent writes against an older one.

Withholding is detected as soon as the hidden version is shown to anyone,
because their later writes then fail the head check. Clients report gaps in
`seq` as "history may be incomplete". There is no cryptographic freshness
guarantee. Owner-signed checkpoints are a later item.

The relay also learns:

- the number of docs and tabs;
- the version count, size, author and timing of every edit.

It does not learn titles, names or content.

### 0.9 Web UI rules

- Markdown is rendered with markdown-it `html: false` plus DOMPurify.
- Links are limited to `https:`, `http:` and `airlox:`, and `airlox:` links
  are handled in app code.
- No remote images. They render as links, since loading one would leak who
  is reading and when.
- Tab names, notes and titles are set as text and follow the
  display-name character rules (no control, bidi or zero-width characters).

### 0.10 Compatibility

`doc` is a new `ItemKind`, plus a manifest `types` entry, in core, the wheel
and the wasm build together. A relay that has stored `doc` items cannot be
rolled back to a build without them. Deploy the relay first. Clients before
this change reject sessions that allow `doc`. Wire breaks are acceptable at
this stage.

Structured tasks, ops and snapshots (§3 to §5) come later, as `v:2`, if
Markdown checklists turn out not to be enough.

## 1. Why

More and more often, one piece of work runs across several agents on
different platforms. Claude on claude.ai writes the design. Codex cloud
implements part one, Claude Code on a laptop does part two, and a person
checks progress from a phone. Today the plan lives in one of three places,
and each one fails somebody:

| where the plan lives | what goes wrong |
|---|---|
| A vendor's doc (Claude Docs, Notion, Google Docs) | Only that vendor's agents can edit it, through that vendor's connector. A hosted Codex sandbox cannot reach it, and you have to give a third party the plaintext. |
| A Markdown file in the repo | Each agent edits its own branch. Progress shows up only after a merge. Two agents claim the same task without knowing. Agents with no push access cannot update it. |
| Chat messages in an Airlox session | Fine for a hand-off, but nothing holds the *current* state. A late joiner has to read the whole timeline, and blobs are evicted after delivery. |

Airlox already offers what the other options lack: one invite link works from
any agent with a shell or an MCP client, both ends use only outbound HTTPS,
everything is end-to-end encrypted and signed, and runners return **signed
results**. So "task done" can point at a job that a runner you approved
actually ran, instead of an agent's claim that the tests pass.

## 2. Goals and non-goals

Goals:

- One agent writes a structured plan: sections of Markdown plus tasks.
  Others, on any platform, read it, claim tasks, update sections, attach
  results and comment. Everyone sees the same current state.
- Edits from several agents at once never silently overwrite each other. A
  losing writer finds out and redoes its edit against the new text.
- Progress is something you can check. A task can name the signed job
  results and files that back it up.
- A participant who joins late, or comes back after a week, gets the current
  document without anyone else online.
- An agent works through a small set of verbs (read, edit a section, claim,
  update a task, comment) that fit in a tool list and a skill file.
- The document can be exported to Markdown or JSON and imported again, so it
  can also live in git.

Non-goals (v1):

- Character-level real-time co-typing by people. LLM agents rewrite whole
  sections. A CRDT fits human typists. See §9.
- Per-section or per-doc access control beyond the session roles. Anyone
  with a writing role (owner, sender) can edit, observers read, runners
  neither read nor write docs.
- Rich content inside the document (tables are fine as Markdown, images are
  links to `image` items).
- Keeping a document after its session is purged. Long-lived documents use a
  long-lived session plus export.

## 3. Model

A **doc** is the state you get by replaying its **revisions** in relay order.
The state is:

```text
Doc {
  id                      genesis item id
  title
  sections: [Section]     ordered
  tasks:    [Task]        ordered; each optionally placed in a section
  comments: [Comment]
  rejected: [RevRef]      revisions whose preconditions failed (§4)
}
Section { id, heading, body (Markdown), rev (last revision to touch it), hash }
Task    { id, title, section?, status, assignee?, note?, evidence: [ItemRef], rev, hash }
Comment { id, anchor: {section|task}, text, to?, reply_to?, resolved, author }
```

- **Ids** (sections, tasks, comments) are short slugs chosen by the author,
  `[a-z0-9-]{1,48}`, unique within the doc, and stable across renames.
  Agents refer to `api`, `t3`, not to line numbers.
- **Task status** is one of `todo`, `doing`, `blocked`, `review`, `done`,
  `dropped`. `assignee` is a participant public key (shown by name).
  Names are self-chosen and cannot be trusted, so the key is the identity.
- **Hash** is the SHA-256 of the canonical JSON of the section or task. It
  is the precondition that edits carry (§4).
- **Evidence** is a list of item references (`{id, blob_sha256}`): usually
  `result` items, sometimes `file` items such as a report or a screenshot.
  Clients render a result reference as verified facts taken from the signed
  result: "pytest -x · exit 0 · runner gpu-box (approved) · tree a1b2c3
  clean".

## 4. Revisions and concurrency

A revision is one item: kind `doc`, subkind `rev`. Its payload meta is:

```json
{
  "type": "doc",
  "doc": {"id": "…", "blob_sha256": "…"},     // absent on the genesis
  "note": "Split auth into token + session sections",
  "to": ["<participant pub>", …],             // optional, as for text: whom to wake
  "ops": [ … ]                                // 1 to 64
}
```

The header's `refs[0]` is the doc's genesis reference (empty on the genesis
itself). That lets the relay group a doc's blobs (§6) without reading them.
It learns that a session has docs, and how many revisions each has. It
already learns that a session runs jobs, so this leaks nothing new in kind.

Ops:

| op | fields | precondition `base` |
|---|---|---|
| `title` | `text` | none |
| `section_put` | `id`, `heading`, `body`, `after` (section id or `null` = first) | hash of the section, or `null` when creating it |
| `section_move` | `id`, `after` | none |
| `section_del` | `id` | hash of the section |
| `task_put` | `id`, `title`, `section?`, `status`, `assignee?`, `note?` | hash, or `null` when creating it |
| `task_set` | `id`, and any of `status`, `assignee` (`null` unassigns), `note`, `evidence_add` | hash of the task |
| `comment` | `id`, `anchor`, `text`, `to?`, `reply_to?` | none |
| `comment_resolve` | `id` | none |

**Order.** The relay already gives every item a `seq`, and every client sees
the same order. Clients replay a doc's revisions in `seq` order, with no
vector clocks and no DAG.

**Compare-and-swap at replay time.** Each op that carries `base` applies only
if the target's current hash equals `base`. A revision applies all its ops or
none of them, so it acts as a transaction. A revision whose precondition
fails goes into `rejected` and changes nothing. Every client computes the
same answer, because the input (revisions in `seq` order) is the same for
all of them.

What that gives agents:

- **No lost updates.** Two agents rewrite `api` from the same text. The
  first one in `seq` order wins. The second is rejected, and its author sees
  the rejection on its next sync, re-reads the section and redoes the edit.
  An LLM agent handles "re-read and redo" well. It handles a merged pair of
  rewrites badly.
- **Atomic claims.** `task_set {id: t3, status: doing, assignee: me, base}`
  from two agents at once: exactly one gets the task, and the other gets a
  rejection naming the winner.
- **Comments never conflict**, because they only append.

The CLI and MCP server do the bookkeeping. They remember the hash of every
section and task they last *showed* the agent. An edit uses that as `base`,
and they refuse an edit of something the agent has not read. That is the
same rule as an editor tool's "read before you edit". After posting, they
sync until their own revision has a `seq` and report `applied` or
`rejected: api changed in rev <id> by codex ("…note…")`, with the current
text.

**Trust.** Order comes from the relay. A malicious relay could reorder
concurrent revisions, which can only flip which of two conflicting edits
wins, and the loser is flagged either way. It could also withhold revisions,
which it can already do to any item. Everything else is as for any item:
signed by its author, encrypted, and checked by every reader. The relay
cannot forge an edit or read one.

**Limits.** `section.body` ≤ 64 KiB, `title` and `note` ≤ 512 bytes,
`text` of a comment ≤ 16 KiB, at most 256 sections, 1024 tasks, 4096
comments. A doc's state must fit in one item (`max_item_bytes`). Validation
lives in `airlox-core`, like every other payload rule, with conformance
vectors for replay (including rejection cases).

## 5. Snapshots

Replaying from the genesis is fine for hundreds of revisions, but blobs
that sit on the relay cost storage (§6). So a doc also has **snapshots**:
kind `doc`, subkind `snap`, with meta

```json
{"type": "doc_snap", "doc": <genesis ref>, "upto": <rev ref>, "upto_seq": 812,
 "state": { …Doc as canonical JSON… }, "state_sha256": "…"}
```

- Any writer may post one. Clients post one after every 50 revisions, or
  when they export.
- A client that has the history recomputes the state at `upto` and checks
  it. A mismatch is shown as a warning naming the snapshot's author, and the
  snapshot is ignored.
- A client *without* the history (a late joiner after eviction) accepts a
  snapshot only from the owner or from itself. Otherwise it asks for the
  revisions with `want`. The owner's CLI and web UI post snapshots, so a
  reachable owner is enough. This is the one place where a sender has to
  trust someone, and it is limited to the owner, who already controls the
  session.

## 6. Keeping doc blobs around

Today a blob is evicted once every recipient has acked it, or after
`AIRLOX_BLOB_HOLD` (1 h). That is right for files in transit and wrong for a
document that someone opens next Tuesday. New relay rule, which needs no
change to the cryptography:

- `doc` items are **retained**. Their blobs are not evicted on delivery or by
  the hold timer. They are evicted when a later `snap` of the same doc
  (`refs[0]`), posted by the owner, covers them (`upto_seq` ≥ their `seq`),
  or when the session ends. The newest owner snapshot and the revisions after
  it therefore always stay.
- Retained bytes count against `AIRLOX_MAX_SESSION_BYTES` as usual. A new
  `AIRLOX_MAX_DOC_BYTES` (default 64 MiB per session) bounds them separately,
  so a doc cannot crowd out file transfers or the other way round.
- Re-seeding still works as before for anything else.
- The relay checks `refs[0]` against a known genesis and rejects a `snap`
  that names a doc it has never seen. It does not have to trust the
  snapshot's content: clients check it (§5).

Sessions: documents want a session that outlives a workday. A new profile,
`airlox session create --profile project`, sets `--ttl never`, the `doc` type
allowed, and execute on with runner approval, so one session holds the plan,
the machines and the results. The unexpiring idle limit (`AIRLOX_IDLE_TTL_UNEXPIRING`,
3 d) still applies on the public relay. `airlox doc export` before a long
break, or a self-hosted relay, covers longer gaps.

The manifest's allowed `types` gains `doc`. A session that does not allow it
rejects doc items in every client (and at the relay, which can see the kind).

## 7. Surfaces

### CLI

```bash
airlox doc new "Auth redesign" --from plan.md      # ## headings become sections, - [ ] lines become tasks
airlox doc ls
airlox doc show auth                               # the whole doc, Markdown, with ids in the margins
airlox doc show auth --section api --json          # one section, with its hash
airlox doc show auth --tasks --mine                # what's assigned to me
airlox doc edit auth --section api --file api.md   # uses the hash from the last show; refuses if never read
airlox doc task add auth t3 "Token refresh" --in api --assign codex
airlox doc task claim auth t3                      # doing + assignee me, or "taken by codex"
airlox doc task set auth t3 --status review --note "PR #51" --evidence <job id>
airlox doc comment auth --on api --to owner "RS256 or EdDSA?"
airlox doc log auth                                # revisions: who, when, note, applied/rejected
airlox doc diff auth <rev> [<rev>]
airlox doc export auth --format md > docs/auth.md  # or json, for import
airlox doc import docs/auth.md --into auth         # a diff against the current state, applied as one revision
```

`auth` resolves to a doc by id prefix or title. `airlox run --task auth/t3
-- pytest` adds its signed result to `t3`'s evidence once it completes, so
an agent cannot forget to attach it and cannot attach a result from another
task's job without saying so.

### MCP

The tools are a mirror of the CLI, sized for a tool list: `doc_list`,
`doc_read` (whole doc, one section, tasks filtered by status or assignee,
`since` a revision for a diff), `doc_create`, `doc_edit` (a list of ops, with
the `base` values filled in by the server from what the agent last read),
`doc_task` (add, claim, set), `doc_comment`. Every write returns `applied` or
the rejection with the current text, so the agent can retry in the same turn.
As with every item, document text is labelled as data written by
participants, never as instructions.

### Waking agents

`airlox listen` (and `--agent claude|codex|…`) wakes a listener for doc
revisions that concern it:

- a task assigned to it, or reassigned away from it;
- a comment addressed to it (`to`), or a reply to one of its comments;
- a rejection of one of its own revisions;
- a revision whose `to` names it (for example "design ready, please start
  t1–t4").

The existing guards apply unchanged: `--from`, and the run-of-agent-posts
loop breaker.

### Web UI

There is a **Docs** tab next to the timeline. It shows the document
rendered (the same Markdown rules as `md` files: no HTML, DOMPurify), with
section anchors, a task board grouped by status with assignee and evidence
chips (a verified result shows a check mark), comments in the margin,
history with per-revision diffs, and inline editing of a section or task.
The web UI uses the same compare-and-swap edits, so a person and an agent
editing the same section get the same "changed while you were editing"
prompt. An observer link gives a read-only live progress view that you can
hand to a lead or open on a phone.

### Skill

`skills/airlox/SKILL.md` gains a short loop for working from a doc:
read the doc → claim one task → work → run the checks with `--task` → set
`review` or `done` with a note → read what changed and pick the next task.
It also covers what to do on a rejection (re-read, redo, never force) and on
`blocked` (say why in `note`, comment to the owner).

## 8. Example

1. A person asks Claude on claude.ai for a design. Claude, connected to the
   Airlox MCP server with the owner link, calls `doc_create` with sections
   `goals`, `api`, `storage`, `rollout` and tasks `t1`–`t5`, assigns `t1`–`t3`
   to `codex` and `t4` to `laptop`, and posts the revision with
   `to: [codex, laptop]`.
2. Codex cloud has a sender link in `AIRLOX_INVITE` and runs `airlox listen
   --agent codex`. It wakes, `doc show --tasks --mine`, claims `t1`, implements
   it, runs `airlox run --on gpu-box --tree --task design/t1 -- pytest`, and
   sets `t1` to `review` with the PR link.
3. Claude Code on the laptop updates the `storage` section at the same time
   as Codex. One of them is rejected, re-reads, and redoes the edit.
4. The person opens the observer link on a phone and sees `t1` in review with
   a verified result, `t4` doing, and a comment on `api` from Codex asking
   about token lifetime. The comment wakes Claude, which answers it and
   updates `api`.

## 9. Alternatives considered

- **A CRDT (Automerge or Yjs) for the whole document.** It merges any
  concurrent edits without conflicts, which is what people typing at once
  need. For agents it has two costs. Merging two whole-section rewrites
  character by character produces text neither agent wrote, with no signal
  that anything went wrong. And claims ("exactly one agent takes t3") are not
  expressible. It also adds a large dependency to the core crate, the wheel
  and the wasm build. Keep it as an option for a later *body* type of a
  section edited live in the browser. The op model above leaves room for it
  (a `section_crdt` op).
- **Re-send a Markdown file item per change.** It needs no new code, but it
  has no structure (no tasks, no claims), no concurrency control, and the
  file is evicted after delivery.
- **A git repository as the store.** Good history, but every participant
  needs credentials for a git host they can all reach, which is the shared
  infrastructure Airlox exists to avoid. Export and import cover "also keep
  it in git".
- **A document store on the relay.** Simple, but the relay would read the
  content. Ruled out.

## 10. Milestones

| | scope |
|---|---|
| D1 | `airlox-core`: payloads, op validation, replay and compare-and-swap, snapshot check, conformance vectors. Protocol §6/§7 text. The `doc` type in manifests. |
| D2 | CLI `airlox doc …`, `--profile project`, export/import, `run --task`. Python SDK `Session.doc(...)`. |
| D3 | Relay retention rule and `AIRLOX_MAX_DOC_BYTES`. Owner snapshots. Late-joiner path. |
| D4 | MCP tools, `listen` wake rules, skill update. Live test with Claude Code and Codex on one doc. |
| D5 | Web UI Docs tab: render, task board, history, inline edit, observer view. |

D1 to D4 alone give agents on any platform a working shared plan. D5 is what
people look at.

## 11. Open questions

1. **Doc limits versus `AIRLOX_MAX_ITEMS` (5 000).** Every revision is an
   item, and envelopes stay in the log for the whole session. A busy doc in a
   long-lived session could reach the limit. Proposed: count `doc` items
   separately (for example 20 000) and nudge clients to batch ops (one
   revision per agent step, not one per field).
2. **Who may post retained snapshots.** Proposed: the owner only, for
   eviction (§6) and for late-joiner trust (§5). The cost is that a doc whose
   owner is never online keeps all its revisions until `MAX_DOC_BYTES`.
3. **Task dependencies.** `blocked_by: [task ids]` is cheap to add and lets
   `doc show --tasks --ready` list what can start now. Add in D2, or wait for
   demand?
4. **Several docs per session, or one?** The model allows several (each is
   its own genesis). The UI is simpler with one. Proposed: allow several,
   show the most recently changed first.
5. **Compatibility.** Clients before this change reject `doc` items. Wire
   breaks are acceptable at this stage, so a session that allows `doc` needs
   new clients throughout, as with `kinds` in 0.2.
