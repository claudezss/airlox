// Shared docs through the wasm binding: chains from headers, first save wins.
// Run: npm test  (after `npm run build`)
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const ac = require("../pkg/node/airlox_core_wasm.js");

const SID = "ax_aaaaaaaaaaaaaaaaaaaaaaaaaa";
const KEY = new Uint8Array(32).fill(1);
const SEED = new Uint8Array(32).fill(2);

function doc(subkind, refs, meta) {
  const item = { id: ac.newItemId(), session_id: SID, kind: "doc", subkind, created_at_ms: 1, refs };
  const out = ac.sealItem(KEY, SEED, JSON.stringify(item), ac.encodePayload(JSON.stringify(meta), new Uint8Array()));
  return { envelope: out.envelope, ref: JSON.parse(ac.itemRef(out.envelope, out.blob)) };
}
const tab = (md) => ({ type: "doc_tab", v: 1, name: "Main", md, deleted: false, note: "" });

test("docIndex keeps the first save and drops the stale one", () => {
  const root = doc("root", [], { type: "doc_root", v: 1, title: "Plan", order: [] });
  const main = doc("tab", [root.ref], tab("v1"));
  const a = doc("tab", [root.ref, main.ref], tab("a"));
  const b = doc("tab", [root.ref, main.ref], tab("b"));
  const entries = [root, main, a, b].map((x, i) => ({ envelope: x.envelope, blob_sha256: x.ref.blob_sha256, owner: i < 2, seq: i + 1 }));
  const { docs: [d], skipped } = JSON.parse(ac.docIndex(JSON.stringify(entries)));
  assert.deepEqual(skipped, [b.ref.id]);
  assert.equal(d.id, root.ref.id);
  assert.equal(d.tabs.length, 1);
  const vs = d.tabs[0].versions;
  assert.deepEqual(vs.map((v) => v.id), [main.ref.id, a.ref.id]);
  assert.match(vs[1].stamp, /^v2·[0-9a-f]{8}$/);
});

test("docCheck, lineDiff and digests", () => {
  assert.throws(() => ac.docCheck(JSON.stringify({ ...tab("x"), name: " " })), /ValidationError/);
  ac.docCheck(JSON.stringify(tab("x")));
  assert.deepEqual(JSON.parse(ac.lineDiff("a\nb", "a\nc")), [[" ", "a"], ["-", "b"], ["+", "c"]]);
  const root = doc("root", [], { type: "doc_root", v: 1, title: "Plan", order: [] });
  assert.deepEqual(JSON.parse(ac.itemRefFromDigest(root.envelope, root.ref.blob_sha256)), root.ref);
  const other = doc("root", [], { type: "doc_root", v: 1, title: "Other", order: [] });
  assert.throws(() => ac.itemRefFromDigest(root.envelope, other.ref.blob_sha256), /SignatureError/);
});
