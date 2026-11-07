"""Shared docs through the SDK: create, read, write with a base, the stale
check, tabs, and a late joiner after old versions left the relay."""

import time

import pytest

from airlox import Session, StaleBase
from airlox.docs import line_diff, parse_stamp

from conftest import Relay


def test_write_read_and_stale(relay, caches):
    owner = Session.create(relay.url, cache_dir=caches("owner"))
    a = Session.join(owner.invites["sender"], display_name="a", cache_dir=caches("a"))
    b = Session.join(owner.invites["sender"], display_name="b", cache_dir=caches("b"))

    doc = owner.doc_create("Plan", "# Plan\n- [ ] step 1\n", note="first draft")
    assert doc.title == "Plan"
    assert [t.name for t in doc.tabs] == ["Main"]
    assert parse_stamp(doc.tabs[0].stamp)[0] == 1

    ta, tb = a.doc("Plan").tab("main"), b.doc("Plan").tab()
    assert ta.stamp == tb.stamp and ta.md == "# Plan\n- [ ] step 1\n"
    assert ta.head.owner and ta.author_name

    won = a.doc_write("Plan", "main", base=ta.stamp, md="# Plan\n- [x] step 1\n", note="done")
    assert parse_stamp(won.stamp)[0] == 2

    # b read v1 too: refused before posting, with what to merge into.
    with pytest.raises(StaleBase) as e:
        b.doc_write("Plan", "main", base=tb.stamp, md="# Plan\n- [ ] step 1\n- [ ] step 2\n")
    assert not e.value.lost_race
    assert e.value.current == won.stamp
    assert e.value.current_md == "# Plan\n- [x] step 1\n"
    assert e.value.newer and "a" in e.value.newer[0] and "done" in e.value.newer[0]
    merged = b.doc_write("Plan", "main", base=e.value.current, md="# Plan\n- [x] step 1\n- [ ] step 2\n")
    assert parse_stamp(merged.stamp)[0] == 3

    # A stamp that names no version is refused.
    with pytest.raises(ValueError):
        b.doc_write("Plan", "main", base="v9·00000000", md="x")

    assert ("+", "- [ ] step 2") in line_diff(won.md, merged.md)
    assert [v.stamp for v in owner.doc().tab().versions][-1] == merged.stamp

    # Observers can read but not write.
    obs = Session.join(owner.invites["observer"], cache_dir=caches("obs"))
    assert obs.doc().tab().md == merged.md
    with pytest.raises(Exception):
        obs.doc_write(None, "main", base=merged.stamp, md="no")


def test_tabs_and_info(relay, caches):
    owner = Session.create(relay.url, cache_dir=caches("owner"))
    agent = Session.join(owner.invites["sender"], display_name="worker", cache_dir=caches("agent"))
    doc = owner.doc_create("Spec", "intro\n")

    progress = agent.doc_write(doc.id, name="Progress", md="- started\n", to=["role:owner"])
    assert progress.name == "Progress" and progress.versions[0].author_name == "worker"
    renamed = agent.doc_write(doc.id, progress, base=progress.stamp, name="Status")
    assert renamed.name == "Status" and renamed.md == "- started\n"
    gone = agent.doc_write(doc.id, "Status", base=renamed.stamp, deleted=True)
    assert gone.deleted and gone.md == "- started\n"
    back = owner.doc_write(doc.id, "Status", base=gone.stamp, deleted=False)
    assert not back.deleted

    d = owner.doc(doc.id)
    order = [t.id for t in reversed(d.tabs)]
    d2 = owner.doc_set_info(d, base=d.stamp, title="Spec v2", order=order)
    assert d2.title == "Spec v2" and [t.id for t in d2.tabs] == order
    with pytest.raises(StaleBase):
        agent.doc_set_info(d2.id, base=d.stamp, title="old")
    assert [x.title for x in agent.docs()] == ["Spec v2"]


def test_late_joiner_rebuilds_chain_after_eviction(tmp_path, caches):
    relay = Relay(tmp_path / "relay", blob_hold="1500ms", doc_keep_versions=2).start()
    try:
        owner = Session.create(relay.url, cache_dir=caches("owner"))
        a = Session.join(owner.invites["sender"], display_name="a", cache_dir=caches("a"))
        owner.doc_create("Log", "v1\n")
        for i in range(4):
            t = a.doc().tab()
            a.doc_write(None, t, base=t.stamp, md=f"edit {i}\n")
        time.sleep(3)
        late = Session.join(owner.invites["sender"], display_name="late", cache_dir=caches("late"))
        t = late.doc().tab()
        assert len(t.versions) == 5 and t.md == "edit 3\n"
        # The newest two, then evicted ones, then the owner's first version.
        assert [v.available for v in reversed(t.versions)] == [True, True, False, False, True]
        w = late.doc_write(None, t, base=t.stamp, md="late\n")
        assert parse_stamp(w.stamp)[0] == 6
    finally:
        relay.stop()
