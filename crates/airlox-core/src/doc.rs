//! Shared docs: a title, ordered tabs, and for each tab a chain of whole-text
//! versions (`docs/design/docs.md` §0).
//!
//! A doc is a set of `doc` items. Which version of each chain is current is
//! decided from plaintext headers alone, in relay order, so the relay and
//! every client agree on it, including clients that never had old blobs:
//!
//! - `root` (refs `[]`) creates the doc; its id is the doc id and it is the
//!   first version of the doc's *meta* chain (title and tab order).
//! - `meta` (refs `[root, base]`) is a new meta version; `base` must be the
//!   meta chain's head.
//! - `tab` with refs `[root]` creates a tab; its id is the tab id.
//! - `tab` with refs `[root, base]` is a new version of the tab whose head is
//!   `base`.
//!
//! A version whose `base` is not a head is *stale*: the relay refuses it, and
//! a client replaying a log ignores it. Content problems (meta that does not
//! decode) never change which version is current; such a version shows as
//! unreadable.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::encoding::b64d_32;
use crate::item::{is_item_id, ItemHeader, ItemKind, ItemRef};
use crate::keys::PublicKey;
use crate::payload::Meta;
use crate::session::ContentType;
use crate::{Error, Result};

/// The `v` of every doc payload this module writes and reads.
pub const DOC_V: u32 = 1;
pub const MAX_TABS: usize = 64;
pub const MAX_MD_BYTES: usize = 256 * 1024;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_TAB_NAME_CHARS: usize = 64;
pub const MAX_NOTE_CHARS: usize = 512;
/// Versions the relay keeps per chain by default, besides the owner's latest.
pub const DEFAULT_KEEP_VERSIONS: usize = 5;

/// The payload of a `root` or `meta` item.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DocInfo {
    pub v: u32,
    pub title: String,
    /// Tab ids, in display order. Tabs missing from it follow in creation
    /// order; unknown ids are ignored.
    pub order: Vec<String>,
}

/// The payload of a `tab` item: the whole tab.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DocTab {
    pub v: u32,
    pub name: String,
    pub md: String,
    /// A deleted tab keeps its text, so the next version can restore it.
    pub deleted: bool,
    /// What changed, for history and for agents. May be empty.
    pub note: String,
    /// Participants whose listeners this version wakes. Absent, never empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Vec<PublicKey>>,
}

pub fn check_info(d: &DocInfo) -> Result<()> {
    if d.v != DOC_V {
        return Err(Error::invalid("doc", "unsupported doc version"));
    }
    crate::cert::check_display_text("doc title", &d.title, MAX_TITLE_CHARS)?;
    if d.order.len() > MAX_TABS {
        return Err(Error::invalid("doc", "too many tabs in order"));
    }
    let mut seen = HashSet::new();
    for id in &d.order {
        if !is_item_id(id) || !seen.insert(id) {
            return Err(Error::invalid("doc", "order must list distinct tab ids"));
        }
    }
    Ok(())
}

pub fn check_tab(t: &DocTab) -> Result<()> {
    if t.v != DOC_V {
        return Err(Error::invalid("doc", "unsupported doc version"));
    }
    crate::cert::check_display_text("tab name", &t.name, MAX_TAB_NAME_CHARS)?;
    if t.md.len() > MAX_MD_BYTES {
        return Err(Error::invalid("doc", format!("tab text exceeds {} KiB", MAX_MD_BYTES / 1024)));
    }
    if t.md.contains('\0') {
        return Err(Error::invalid("doc", "tab text contains NUL"));
    }
    if !t.note.is_empty() {
        crate::cert::check_display_text("note", &t.note, MAX_NOTE_CHARS)?;
    }
    crate::policy::check_recipients(t.to.as_deref())
}

/// `v<n>·<first 8 hex digits of the blob's SHA-256>`: a version's short name
/// for people and agents. Never compared on the wire, where `base` is a full
/// [`ItemRef`].
pub fn stamp(n: usize, blob_sha256: &str) -> String {
    let hex: String = b64d_32(blob_sha256)
        .map(|d| d[..4].iter().map(|b| format!("{b:02x}")).collect())
        .unwrap_or_else(|_| "????????".into());
    format!("v{n}·{hex}")
}

/// Split a stamp into its number and hex part. Accepts `·`, `-` or `.` as
/// the separator (the middle dot is hard to type) and any case.
pub fn parse_stamp(s: &str) -> Option<(usize, String)> {
    let s = s.trim();
    let rest = s.strip_prefix('v').or_else(|| s.strip_prefix('V'))?;
    let (n, hex) = rest.split_once(['·', '-', '.'])?;
    let n: usize = n.parse().ok()?;
    let hex = hex.to_ascii_lowercase();
    (n >= 1 && hex.len() == 8 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some((n, hex))
}

/// One version in a chain, as headers describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub item: ItemRef,
    pub author: PublicKey,
    /// Whether the author was the owner (the relay keeps the owner's latest).
    pub owner: bool,
    pub seq: u64,
    pub created_at_ms: u64,
}

/// A chain of versions: the doc's meta chain (starting with the root) or a
/// tab (starting with the item that created it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chain {
    /// The first version's item id: the doc id for the meta chain, the tab
    /// id for a tab.
    pub id: String,
    pub versions: Vec<Version>,
}

impl Chain {
    pub fn head(&self) -> &Version {
        self.versions.last().expect("a chain has at least one version")
    }

    /// The stamp of the version at `index` (0-based).
    pub fn stamp_at(&self, index: usize) -> String {
        stamp(index + 1, &self.versions[index].item.blob_sha256)
    }

    pub fn head_stamp(&self) -> String {
        self.stamp_at(self.versions.len() - 1)
    }

    /// The versions whose blobs the relay keeps: the newest `keep`, and the
    /// newest one the owner wrote.
    pub fn kept(&self, keep: usize) -> HashSet<&str> {
        let mut out: HashSet<&str> =
            self.versions.iter().rev().take(keep.max(1)).map(|v| v.item.id.as_str()).collect();
        if let Some(v) = self.versions.iter().rev().find(|v| v.owner) {
            out.insert(&v.item.id);
        }
        out
    }

    /// Find a version by its stamp.
    pub fn find_stamp(&self, s: &str) -> Option<&Version> {
        let (n, hex) = parse_stamp(s)?;
        let v = self.versions.get(n.checked_sub(1)?)?;
        (stamp(n, &v.item.blob_sha256).ends_with(&hex)).then_some(v)
    }
}

/// A doc as headers describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doc {
    pub meta: Chain,
    /// In creation order.
    pub tabs: Vec<Chain>,
}

impl Doc {
    pub fn id(&self) -> &str {
        &self.meta.id
    }

    pub fn root(&self) -> &ItemRef {
        &self.meta.versions[0].item
    }

    pub fn tab(&self, id: &str) -> Option<&Chain> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn chains(&self) -> impl Iterator<Item = &Chain> {
        std::iter::once(&self.meta).chain(self.tabs.iter())
    }
}

/// Where a doc item goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Root,
    Meta { doc: String },
    NewTab { doc: String },
    Tab { doc: String, tab: String },
}

impl Place {
    pub fn doc(&self) -> Option<&str> {
        match self {
            Place::Root => None,
            Place::Meta { doc } | Place::NewTab { doc } | Place::Tab { doc, .. } => Some(doc),
        }
    }

    /// The chain the item extends (or starts): its id.
    pub fn chain<'a>(&'a self, item_id: &'a str) -> &'a str {
        match self {
            Place::Root | Place::NewTab { .. } => item_id,
            Place::Meta { doc } => doc,
            Place::Tab { tab, .. } => tab,
        }
    }
}

/// Why a doc item does not apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reject {
    /// `refs[0]` is not the root of a doc in this session.
    UnknownDoc,
    /// `base` is an older version of a chain: someone saved first. Carries
    /// that chain's current head.
    Stale {
        chain: String,
        head: ItemRef,
        head_stamp: String,
    },
    /// `base` is not a version in this doc (or not of this subkind).
    UnknownBase,
    TooManyTabs,
}

impl Reject {
    pub fn message(&self) -> String {
        match self {
            Reject::UnknownDoc => "no such doc in this session".into(),
            Reject::Stale { head_stamp, .. } => {
                format!("stale base: the current version is {head_stamp}; read it, merge, and retry")
            }
            Reject::UnknownBase => "base is not a version of this doc".into(),
            Reject::TooManyTabs => format!("a doc has at most {MAX_TABS} tabs"),
        }
    }
}

/// Every doc in a session, built from item headers in relay order.
#[derive(Clone, Debug, Default)]
pub struct DocIndex {
    docs: BTreeMap<String, Doc>,
    /// Ids of `doc` items that did not apply, in order. The relay stores
    /// only items that apply, so on a client any entry here means a save it
    /// was not shown (or one the relay should have refused): its history may
    /// be incomplete (`docs/design/docs.md` §0.8).
    pub skipped: Vec<String>,
}

impl DocIndex {
    pub fn docs(&self) -> impl Iterator<Item = &Doc> {
        self.docs.values()
    }

    pub fn get(&self, id: &str) -> Option<&Doc> {
        self.docs.get(id)
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Where `h` (a `doc` item) would go, or why it does not apply.
    pub fn place(&self, h: &ItemHeader) -> std::result::Result<Place, Reject> {
        debug_assert_eq!(h.kind, ItemKind::Doc);
        if h.subkind == Some(ContentType::Root) {
            return Ok(Place::Root);
        }
        let doc = h
            .refs
            .first()
            .and_then(|r| self.docs.get(&r.id).filter(|d| d.root() == r))
            .ok_or(Reject::UnknownDoc)?;
        let doc_id = doc.id().to_string();
        let base = h.refs.get(1);
        let check = |c: &Chain, base: &ItemRef| -> Option<std::result::Result<(), Reject>> {
            if c.head().item == *base {
                Some(Ok(()))
            } else if c.versions.iter().any(|v| v.item == *base) {
                Some(Err(Reject::Stale {
                    chain: c.id.clone(),
                    head: c.head().item.clone(),
                    head_stamp: c.head_stamp(),
                }))
            } else {
                None
            }
        };
        match (h.subkind, base) {
            (Some(ContentType::Meta), Some(base)) => match check(&doc.meta, base) {
                Some(Ok(())) => Ok(Place::Meta { doc: doc_id }),
                Some(Err(e)) => Err(e),
                None => Err(Reject::UnknownBase),
            },
            (Some(ContentType::Tab), None) => {
                if doc.tabs.len() >= MAX_TABS {
                    Err(Reject::TooManyTabs)
                } else {
                    Ok(Place::NewTab { doc: doc_id })
                }
            }
            (Some(ContentType::Tab), Some(base)) => {
                for t in &doc.tabs {
                    match check(t, base) {
                        Some(Ok(())) => return Ok(Place::Tab { doc: doc_id, tab: t.id.clone() }),
                        Some(Err(e)) => return Err(e),
                        None => {}
                    }
                }
                Err(Reject::UnknownBase)
            }
            _ => Err(Reject::UnknownBase),
        }
    }

    /// Record `h` (whose blob hashes to `blob_sha256`, base64url) if it
    /// applies. Returns where it went.
    pub fn apply(
        &mut self,
        h: &ItemHeader,
        blob_sha256: &str,
        owner: bool,
        seq: u64,
    ) -> std::result::Result<Place, Reject> {
        let place = match self.place(h) {
            Ok(p) => p,
            Err(e) => {
                self.skipped.push(h.id.clone());
                return Err(e);
            }
        };
        let v = Version {
            item: ItemRef { id: h.id.clone(), blob_sha256: blob_sha256.to_string() },
            author: h.author,
            owner,
            seq,
            created_at_ms: h.created_at_ms,
        };
        match &place {
            Place::Root => {
                self.docs.insert(
                    h.id.clone(),
                    Doc { meta: Chain { id: h.id.clone(), versions: vec![v] }, tabs: vec![] },
                );
            }
            Place::Meta { doc } => self.docs.get_mut(doc).expect("placed").meta.versions.push(v),
            Place::NewTab { doc } => self
                .docs
                .get_mut(doc)
                .expect("placed")
                .tabs
                .push(Chain { id: h.id.clone(), versions: vec![v] }),
            Place::Tab { doc, tab } => self
                .docs
                .get_mut(doc)
                .expect("placed")
                .tabs
                .iter_mut()
                .find(|t| t.id == *tab)
                .expect("placed")
                .versions
                .push(v),
        }
        Ok(place)
    }

    /// The chain `id` (a doc id for its meta chain, or a tab id).
    pub fn chain(&self, doc: &str, id: &str) -> Option<&Chain> {
        let d = self.docs.get(doc)?;
        if d.id() == id {
            Some(&d.meta)
        } else {
            d.tab(id)
        }
    }
}

/// The readable state of one tab: its head version's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabState {
    pub id: String,
    /// `None` when the head's content is not available (evicted and never
    /// cached) or does not decode.
    pub content: Option<DocTab>,
    pub head: Version,
    pub stamp: String,
    pub versions: usize,
}

/// The readable state of a doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocState {
    pub id: String,
    pub title: Option<String>,
    pub meta_head: Version,
    pub meta_stamp: String,
    /// Tabs in display order, deleted ones included (check `content`).
    pub tabs: Vec<TabState>,
}

/// Resolve a doc's current state. `content` returns the decrypted, checked
/// payload of a version when it is available.
pub fn resolve(doc: &Doc, content: impl Fn(&ItemRef) -> Option<Meta>) -> DocState {
    let info = match content(&doc.meta.head().item) {
        Some(Meta::DocRoot(d) | Meta::DocMeta(d)) => Some(d),
        _ => None,
    };
    let mut order: Vec<&Chain> = Vec::new();
    if let Some(info) = &info {
        for id in &info.order {
            if let Some(t) = doc.tab(id) {
                if !order.iter().any(|o| o.id == t.id) {
                    order.push(t);
                }
            }
        }
    }
    for t in &doc.tabs {
        if !order.iter().any(|o| o.id == t.id) {
            order.push(t);
        }
    }
    let tabs = order
        .into_iter()
        .map(|t| TabState {
            id: t.id.clone(),
            content: match content(&t.head().item) {
                Some(Meta::DocTab(d)) => Some(d),
                _ => None,
            },
            head: t.head().clone(),
            stamp: t.head_stamp(),
            versions: t.versions.len(),
        })
        .collect();
    DocState {
        id: doc.id().to_string(),
        title: info.map(|i| i.title),
        meta_head: doc.meta.head().clone(),
        meta_stamp: doc.meta.head_stamp(),
        tabs,
    }
}

/// A line diff from `a` to `b`: `(' ', line)` kept, `('-', line)` removed,
/// `('+', line)` added. Texts too large to compare line by line come back
/// as all of `a` removed and all of `b` added.
pub fn line_diff<'a>(a: &'a str, b: &'a str) -> Vec<(char, &'a str)> {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    // Common prefix and suffix first: most edits touch a few lines.
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out: Vec<(char, &str)> = a[..pre].iter().map(|l| (' ', *l)).collect();
    if am.len().saturating_mul(bm.len()) > 4_000_000 {
        out.extend(am.iter().map(|l| ('-', *l)));
        out.extend(bm.iter().map(|l| ('+', *l)));
    } else {
        let (n, m) = (am.len(), bm.len());
        let mut lcs = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] =
                    if am[i] == bm[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if am[i] == bm[j] {
                out.push((' ', am[i]));
                i += 1;
                j += 1;
            } else if lcs[i + 1][j] >= lcs[i][j + 1] {
                out.push(('-', am[i]));
                i += 1;
            } else {
                out.push(('+', bm[j]));
                j += 1;
            }
        }
        out.extend(am[i..].iter().map(|l| ('-', *l)));
        out.extend(bm[j..].iter().map(|l| ('+', *l)));
    }
    out.extend(a[a.len() - suf..].iter().map(|l| (' ', *l)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::b64e;
    use crate::item::{new_item_id, seal, NewItem, SealedItem};
    use crate::keys::Keypair;
    use crate::payload::Payload;

    const SID: &str = "ax_aaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn item(a: &Keypair, sub: ContentType, refs: Vec<ItemRef>, meta: Meta) -> SealedItem {
        let new = NewItem {
            id: new_item_id(),
            session_id: SID.into(),
            kind: ItemKind::Doc,
            subkind: Some(sub),
            created_at_ms: 1,
            refs,
        };
        seal(&[1u8; 32], a, new, &Payload { meta, body: vec![] }.encode().unwrap()).unwrap()
    }

    fn info(title: &str) -> Meta {
        Meta::DocRoot(DocInfo { v: 1, title: title.into(), order: vec![] })
    }

    fn tab(md: &str) -> Meta {
        Meta::DocTab(DocTab {
            v: 1,
            name: "Main".into(),
            md: md.into(),
            deleted: false,
            note: String::new(),
            to: None,
        })
    }

    fn sha(i: &SealedItem) -> String {
        b64e(&i.blob_sha256())
    }

    #[test]
    fn first_save_wins_and_stale_names_the_head() {
        let a = Keypair::from_seed(&[2u8; 32]);
        let mut ix = DocIndex::default();
        let root = item(&a, ContentType::Root, vec![], info("Plan"));
        assert_eq!(ix.apply(root.header(), &sha(&root), true, 1), Ok(Place::Root));
        let r = ItemRef::to(&root);
        let main = item(&a, ContentType::Tab, vec![r.clone()], tab("v1"));
        let tab_id = main.header().id.clone();
        assert_eq!(
            ix.apply(main.header(), &sha(&main), true, 2),
            Ok(Place::NewTab { doc: root.header().id.clone() })
        );
        let base = ItemRef::to(&main);
        let x = item(&a, ContentType::Tab, vec![r.clone(), base.clone()], tab("x"));
        let y = item(&a, ContentType::Tab, vec![r.clone(), base.clone()], tab("y"));
        assert!(matches!(ix.apply(x.header(), &sha(&x), false, 3), Ok(Place::Tab { .. })));
        match ix.apply(y.header(), &sha(&y), false, 4) {
            Err(Reject::Stale { chain, head, head_stamp }) => {
                assert_eq!(chain, tab_id);
                assert_eq!(head, ItemRef::to(&x));
                assert_eq!(head_stamp, stamp(2, &sha(&x)));
            }
            other => panic!("{other:?}"),
        }
        let d = ix.get(&root.header().id).unwrap();
        assert_eq!(d.tab(&tab_id).unwrap().versions.len(), 2);
    }

    #[test]
    fn unknown_doc_and_base_are_refused() {
        let a = Keypair::from_seed(&[2u8; 32]);
        let mut ix = DocIndex::default();
        let root = item(&a, ContentType::Root, vec![], info("Plan"));
        ix.apply(root.header(), &sha(&root), true, 1).unwrap();
        let other = item(&a, ContentType::Root, vec![], info("Other"));
        let stray = item(&a, ContentType::Tab, vec![ItemRef::to(&other)], tab("x"));
        assert_eq!(ix.place(stray.header()), Err(Reject::UnknownDoc));
        // Right id, wrong hash: not this doc.
        let mut wrong = ItemRef::to(&root);
        wrong.blob_sha256 = b64e(&[0u8; 32]);
        let forged = item(&a, ContentType::Tab, vec![wrong], tab("x"));
        assert_eq!(ix.place(forged.header()), Err(Reject::UnknownDoc));
        // A meta version must extend the meta chain, not a tab.
        let main = item(&a, ContentType::Tab, vec![ItemRef::to(&root)], tab("v1"));
        ix.apply(main.header(), &sha(&main), true, 2).unwrap();
        let bad = item(
            &a,
            ContentType::Meta,
            vec![ItemRef::to(&root), ItemRef::to(&main)],
            Meta::DocMeta(DocInfo { v: 1, title: "x".into(), order: vec![] }),
        );
        assert_eq!(ix.place(bad.header()), Err(Reject::UnknownBase));
        let good = item(
            &a,
            ContentType::Meta,
            vec![ItemRef::to(&root), ItemRef::to(&root)],
            Meta::DocMeta(DocInfo { v: 1, title: "x".into(), order: vec![] }),
        );
        assert!(matches!(ix.place(good.header()), Ok(Place::Meta { .. })));
    }

    #[test]
    fn kept_versions_include_the_owners_latest() {
        let a = Keypair::from_seed(&[2u8; 32]);
        let mut ix = DocIndex::default();
        let root = item(&a, ContentType::Root, vec![], info("Plan"));
        ix.apply(root.header(), &sha(&root), true, 1).unwrap();
        let r = ItemRef::to(&root);
        let first = item(&a, ContentType::Tab, vec![r.clone()], tab("0"));
        ix.apply(first.header(), &sha(&first), true, 2).unwrap();
        let mut base = ItemRef::to(&first);
        for i in 0..8 {
            let next = item(&a, ContentType::Tab, vec![r.clone(), base.clone()], tab(&i.to_string()));
            ix.apply(next.header(), &sha(&next), false, 3 + i).unwrap();
            base = ItemRef::to(&next);
        }
        let chain = &ix.get(&root.header().id).unwrap().tabs[0];
        let kept = chain.kept(5);
        assert_eq!(kept.len(), 6);
        assert!(kept.contains(first.header().id.as_str()), "the owner's latest stays");
    }

    #[test]
    fn stamps_parse_and_resolve() {
        let h = b64e(&[
            0xab, 0xcd, 0x01, 0x23, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
            9, 9, 9, 9,
        ]);
        assert_eq!(stamp(3, &h), "v3·abcd0123");
        assert_eq!(parse_stamp("v3·abcd0123"), Some((3, "abcd0123".into())));
        assert_eq!(parse_stamp("V3-ABCD0123"), Some((3, "abcd0123".into())));
        assert_eq!(parse_stamp("v0·abcd0123"), None);
        assert_eq!(parse_stamp("v3·abcd"), None);
    }

    #[test]
    fn resolve_orders_tabs() {
        let a = Keypair::from_seed(&[2u8; 32]);
        let mut ix = DocIndex::default();
        let root = item(&a, ContentType::Root, vec![], info("Plan"));
        ix.apply(root.header(), &sha(&root), true, 1).unwrap();
        let r = ItemRef::to(&root);
        let t1 = item(&a, ContentType::Tab, vec![r.clone()], tab("one"));
        let t2 = item(&a, ContentType::Tab, vec![r.clone()], tab("two"));
        ix.apply(t1.header(), &sha(&t1), true, 2).unwrap();
        ix.apply(t2.header(), &sha(&t2), true, 3).unwrap();
        let reorder = item(
            &a,
            ContentType::Meta,
            vec![r.clone(), r.clone()],
            Meta::DocMeta(DocInfo { v: 1, title: "Plan".into(), order: vec![t2.header().id.clone()] }),
        );
        ix.apply(reorder.header(), &sha(&reorder), true, 4).unwrap();
        let items = [&root, &t1, &t2, &reorder];
        let st = resolve(ix.get(&root.header().id).unwrap(), |r| {
            let i = items.iter().find(|i| r.matches(i))?;
            Some(Payload::decode(&i.open(&[1u8; 32]).ok()?).ok()?.meta)
        });
        assert_eq!(st.title.as_deref(), Some("Plan"));
        let ids: Vec<&str> = st.tabs.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, [t2.header().id.as_str(), t1.header().id.as_str()]);
        assert_eq!(st.tabs[1].content.as_ref().unwrap().md, "one");
    }

    #[test]
    fn diffs_lines() {
        let d = line_diff("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(d, [(' ', "a"), ('-', "b"), ('+', "B"), (' ', "c"), ('+', "d")]);
        assert!(line_diff("x", "x").iter().all(|(k, _)| *k == ' '));
    }

    #[test]
    fn payload_checks() {
        assert!(check_tab(&DocTab {
            v: 1,
            name: "Main".into(),
            md: "x".into(),
            deleted: false,
            note: "".into(),
            to: None
        })
        .is_ok());
        assert!(check_tab(&DocTab {
            v: 2,
            name: "Main".into(),
            md: "x".into(),
            deleted: false,
            note: "".into(),
            to: None
        })
        .is_err());
        assert!(check_tab(&DocTab {
            v: 1,
            name: " ".into(),
            md: "x".into(),
            deleted: false,
            note: "".into(),
            to: None
        })
        .is_err());
        assert!(check_tab(&DocTab {
            v: 1,
            name: "a\u{202e}b".into(),
            md: "x".into(),
            deleted: false,
            note: "".into(),
            to: None
        })
        .is_err());
        assert!(check_tab(&DocTab {
            v: 1,
            name: "M".into(),
            md: "x".repeat(MAX_MD_BYTES + 1),
            deleted: false,
            note: "".into(),
            to: None
        })
        .is_err());
        assert!(check_info(&DocInfo { v: 1, title: "T".into(), order: vec!["x".into()] }).is_err());
    }
}
