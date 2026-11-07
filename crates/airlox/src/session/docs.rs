//! Shared docs on the client: read the current state from the items this
//! session holds, and write with the base check (`docs/design/docs.md` §0).
//!
//! Which version is current comes from item headers in relay order
//! ([`DocIndex`]); content comes from the items this client could open. A
//! write names the version it started from; it is refused here when that is
//! no longer the head, and by the relay when another write lands first.

use std::collections::HashMap;
use std::time::Duration;

use airlox_core::doc::{resolve, Chain, Doc, DocIndex, DocInfo, DocState, DocTab, TabState, DOC_V};
use airlox_core::encoding::parse_canonical;
use airlox_core::item::{ItemEnvelope, ItemRef};
use airlox_core::payload::Meta;
use anyhow::{bail, Context, Result};

use super::{Session, Status};
use crate::client::RelayError;

/// A tab write: a new version of `tab` based on `base`, or a new tab.
#[derive(Clone, Debug)]
pub struct DocWrite {
    pub doc: String,
    /// The tab to write; `None` creates a tab.
    pub tab: Option<String>,
    /// The version the text was based on; required for an existing tab.
    pub base: Option<ItemRef>,
    pub name: String,
    pub md: String,
    pub deleted: bool,
    pub note: String,
    /// Participant keys to wake.
    pub to: Vec<String>,
}

/// A write refused because the tab moved on: before posting (`lost_race`
/// false) or by the relay, because another write landed first.
#[derive(Debug)]
pub struct StaleBase {
    pub lost_race: bool,
    pub tab: String,
    pub base: String,
    pub current: String,
    /// The versions after the base: `stamp  author  “note”`.
    pub newer: Vec<String>,
    /// The current text, to merge into.
    pub current_md: Option<String>,
}

impl std::fmt::Display for StaleBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.lost_race {
            write!(
                f,
                "another write to {} landed first; it is now at {}. Nothing of yours was stored.",
                self.tab, self.current
            )
        } else {
            write!(
                f,
                "stale: {} is at {}, your base is {}. Nothing was posted.",
                self.tab, self.current, self.base
            )
        }
    }
}

impl std::error::Error for StaleBase {}

impl Session {
    /// Every doc in the session, from item headers in relay order. Items
    /// from anyone but the owner or a sender are left out.
    pub fn doc_index(&self) -> DocIndex {
        let mut ix = DocIndex::default();
        for it in self.items.iter().filter(|i| i.kind == "doc") {
            if !matches!(it.author_role.as_deref(), Some("owner" | "sender")) {
                continue;
            }
            let Some(env) = self.envelopes.get(&it.id) else { continue };
            let Ok(env) = parse_canonical::<ItemEnvelope>(env, "item envelope") else { continue };
            let Some(sha) = it.item_ref["blob_sha256"].as_str() else { continue };
            let _ = ix.apply(&env.header, sha, it.author_role.as_deref() == Some("owner"), it.seq);
        }
        ix
    }

    /// The checked content of a doc version this client holds.
    pub fn doc_content(&self, r: &ItemRef) -> Option<Meta> {
        let it = self.get(&r.id)?;
        if it.status != Status::Ok || it.item_ref["blob_sha256"].as_str() != Some(&r.blob_sha256) {
            return None;
        }
        serde_json::from_value(it.meta.clone()?).ok()
    }

    /// A tab version's content, if this client holds it.
    pub fn doc_tab_version(&self, r: &ItemRef) -> Option<DocTab> {
        match self.doc_content(r)? {
            Meta::DocTab(t) => Some(t),
            _ => None,
        }
    }

    pub fn doc_state(&self, doc: &Doc) -> DocState {
        resolve(doc, |r| self.doc_content(r))
    }

    /// A doc by id (or a prefix of at least 4 characters) or title (any
    /// case). Without one: the only doc, or the most recently changed.
    pub fn doc_find(&self, spec: Option<&str>) -> Result<(Doc, DocState)> {
        let ix = self.doc_index();
        let docs: Vec<&Doc> = ix.docs().collect();
        if docs.is_empty() {
            bail!("this session has no docs yet; create one with `airlox doc new \"Title\"`");
        }
        let doc = match spec {
            None => *docs
                .iter()
                .max_by_key(|d| d.chains().map(|c| c.head().seq).max().unwrap_or(0))
                .expect("not empty"),
            Some(s) => {
                let by_title: Vec<&&Doc> = docs
                    .iter()
                    .filter(|d| self.doc_state(d).title.is_some_and(|t| t.eq_ignore_ascii_case(s)))
                    .collect();
                let by_id: Vec<&&Doc> =
                    docs.iter().filter(|d| s.len() >= 4 && d.id().starts_with(s)).collect();
                match (by_id.as_slice(), by_title.as_slice()) {
                    ([d], _) | ([], [d]) => **d,
                    ([], []) => bail!("no doc {s:?} in this session (airlox doc ls lists them)"),
                    _ => bail!("{s:?} matches several docs; use its id"),
                }
            }
        };
        Ok((doc.clone(), self.doc_state(doc)))
    }

    /// A tab by id (or a prefix of at least 4 characters) or name (any
    /// case); without one, the first tab that is not deleted.
    pub fn tab_find<'a>(st: &'a DocState, spec: Option<&str>) -> Result<&'a TabState> {
        let live = |t: &&TabState| t.content.as_ref().is_none_or(|c| !c.deleted);
        let Some(s) = spec else {
            return st.tabs.iter().find(live).context("this doc has no tabs");
        };
        let by_name: Vec<&TabState> = st
            .tabs
            .iter()
            .filter(|t| t.content.as_ref().is_some_and(|c| c.name.eq_ignore_ascii_case(s)))
            .collect();
        let by_id: Vec<&TabState> = st.tabs.iter().filter(|t| s.len() >= 4 && t.id.starts_with(s)).collect();
        match (by_id.as_slice(), by_name.as_slice()) {
            ([t], _) | ([], [t]) => Ok(t),
            ([], []) => bail!("no tab {s:?} in this doc"),
            (_, names) => {
                // Prefer a live tab when a deleted one has the same name.
                let mut live_names = names.iter().filter(|t| live(t));
                match (live_names.next(), live_names.next()) {
                    (Some(t), None) => Ok(t),
                    _ => bail!("{s:?} matches several tabs; use its id"),
                }
            }
        }
    }

    /// The version of a chain this participant last read, if any.
    pub fn doc_last_read(&self, chain: &str) -> Option<String> {
        self.cache.docs_read(self.role.as_str()).remove(chain)
    }

    /// Remember that this participant has read `item` of `chain`.
    pub fn mark_doc_read(&self, chain: &str, item: &str) -> Result<()> {
        let mut read = self.cache.docs_read(self.role.as_str());
        read.insert(chain.to_string(), item.to_string());
        self.cache.save_docs_read(self.role.as_str(), &read)
    }

    /// Post a doc item and wait until the relay lists it, so the index
    /// includes it. Returns its id.
    async fn post_doc(&mut self, subkind: &str, refs: Vec<ItemRef>, meta: Meta) -> Result<String> {
        let refs = refs.iter().map(serde_json::to_value).collect::<std::result::Result<Vec<_>, _>>()?;
        let r = self.send("doc", Some(subkind), serde_json::to_value(&meta)?, vec![], refs).await?;
        let id = r["id"].as_str().context("relay gave no item id")?.to_string();
        for _ in 0..50 {
            self.sync().await?;
            if self.get(&id).is_some() {
                return Ok(id);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("posted {id}, but the relay has not listed it yet; run `airlox doc show` again")
    }

    /// Create a doc titled `title` with one tab, Main. Returns the doc id.
    pub async fn doc_create(&mut self, title: &str, main_md: &str, note: &str) -> Result<String> {
        let info = DocInfo { v: DOC_V, title: title.to_string(), order: vec![] };
        airlox_core::doc::check_info(&info)?;
        let root = self.post_doc("root", vec![], Meta::DocRoot(info)).await?;
        let root_ref = self.item_ref_of(&root)?;
        self.doc_write(DocWrite {
            doc: root_ref.id.clone(),
            tab: None,
            base: None,
            name: "Main".into(),
            md: main_md.to_string(),
            deleted: false,
            note: note.to_string(),
            to: vec![],
        })
        .await?;
        Ok(root)
    }

    fn item_ref_of(&self, id: &str) -> Result<ItemRef> {
        let it = self.get(id).with_context(|| format!("no item {id}"))?;
        Ok(serde_json::from_value(it.item_ref.clone())?)
    }

    /// Write a tab: pull, check that `base` is still the head, post. Fails
    /// with [`StaleBase`] when it is not, before posting or because another
    /// write landed first. Returns the new version's stamp.
    pub async fn doc_write(&mut self, w: DocWrite) -> Result<(String, String)> {
        let tab = DocTab {
            v: DOC_V,
            name: w.name.clone(),
            md: w.md.clone(),
            deleted: w.deleted,
            note: w.note.clone(),
            to: (!w.to.is_empty())
                .then(|| w.to.iter().map(|k| airlox_core::keys::PublicKey::from_b64(k)).collect())
                .transpose()?,
        };
        airlox_core::doc::check_tab(&tab)?;
        self.sync().await?;
        let ix = self.doc_index();
        let doc = ix.get(&w.doc).context("no such doc in this session")?;
        let mut refs = vec![doc.root().clone()];
        if let Some(t) = &w.tab {
            let chain = doc.tab(t).context("no such tab in this doc")?;
            let base = w.base.as_ref().context("a write to an existing tab needs its base version")?;
            if chain.head().item != *base {
                return Err(self.stale(chain, base, false).into());
            }
            refs.push(base.clone());
        }
        let posted = match self.post_doc("tab", refs, Meta::DocTab(tab)).await {
            Ok(id) => id,
            Err(e) if e.downcast_ref::<RelayError>().is_some_and(|r| r.code == "stale_base") => {
                self.sync().await?;
                let ix = self.doc_index();
                let (Some(chain), Some(base)) =
                    (w.tab.as_ref().and_then(|t| ix.get(&w.doc)?.tab(t)), w.base.as_ref())
                else {
                    return Err(e);
                };
                return Err(self.stale(chain, base, true).into());
            }
            Err(e) => return Err(e),
        };
        let ix = self.doc_index();
        let chain = ix
            .get(&w.doc)
            .and_then(|d| d.tabs.iter().find(|c| c.versions.iter().any(|v| v.item.id == posted)))
            .context("the relay stored the version but it is not in the doc; was the doc rewritten?")?;
        let stamp = chain.head_stamp();
        let _ = self.mark_doc_read(&chain.id, &posted);
        Ok((chain.id.clone(), stamp))
    }

    /// Retitle the doc or reorder its tabs, from the meta version `base`.
    pub async fn doc_set_info(
        &mut self,
        doc: &str,
        base: &ItemRef,
        title: &str,
        order: Vec<String>,
    ) -> Result<String> {
        let info = DocInfo { v: DOC_V, title: title.to_string(), order };
        airlox_core::doc::check_info(&info)?;
        self.sync().await?;
        let ix = self.doc_index();
        let d = ix.get(doc).context("no such doc in this session")?;
        if d.meta.head().item != *base {
            return Err(self.stale(&d.meta, base, false).into());
        }
        let refs = vec![d.root().clone(), base.clone()];
        match self.post_doc("meta", refs, Meta::DocMeta(info)).await {
            Ok(_) => {}
            Err(e) if e.downcast_ref::<RelayError>().is_some_and(|r| r.code == "stale_base") => {
                self.sync().await?;
                let ix = self.doc_index();
                let d = ix.get(doc).context("no such doc")?;
                return Err(self.stale(&d.meta, base, true).into());
            }
            Err(e) => return Err(e),
        }
        let ix = self.doc_index();
        Ok(ix.get(doc).context("no such doc")?.meta.head_stamp())
    }

    fn stale(&self, chain: &Chain, base: &ItemRef, lost_race: bool) -> StaleBase {
        let at = chain.versions.iter().position(|v| v.item == *base);
        let name = |i: usize| -> String {
            match self.doc_content(&chain.versions[i].item) {
                Some(Meta::DocTab(t)) => t.name,
                Some(Meta::DocRoot(d) | Meta::DocMeta(d)) => d.title,
                _ => chain.id.clone(),
            }
        };
        let newer = chain
            .versions
            .iter()
            .enumerate()
            .skip(at.map_or(0, |i| i + 1))
            .map(|(i, v)| {
                let who = self.participant_name(&v.author.to_b64()).unwrap_or_else(|| "?".into());
                let note = match self.doc_content(&v.item) {
                    Some(Meta::DocTab(t)) if !t.note.is_empty() => format!("  “{}”", t.note),
                    _ => String::new(),
                };
                format!("{}  {who}{note}", chain.stamp_at(i))
            })
            .collect();
        StaleBase {
            lost_race,
            tab: name(chain.versions.len() - 1),
            base: at.map_or_else(|| "an unknown version".into(), |i| chain.stamp_at(i)),
            current: chain.head_stamp(),
            newer,
            current_md: self.doc_tab_version(&chain.head().item).map(|t| t.md),
        }
    }

    /// Unread doc versions per chain, for `listen` and `doc ls`: chain id →
    /// how many versions came after the one this participant last read.
    pub fn doc_unread(&self, doc: &Doc) -> HashMap<String, usize> {
        let read = self.cache.docs_read(self.role.as_str());
        doc.chains()
            .filter_map(|c| {
                let after = match read.get(&c.id) {
                    Some(id) => c
                        .versions
                        .iter()
                        .position(|v| v.item.id == *id)
                        .map_or(c.versions.len(), |i| c.versions.len() - 1 - i),
                    None => c.versions.len(),
                };
                (after > 0).then(|| (c.id.clone(), after))
            })
            .collect()
    }
}
