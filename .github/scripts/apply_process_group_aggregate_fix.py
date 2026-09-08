from pathlib import Path

path = Path("crates/tm-app/src/tabs/processes.rs")
text = path.read_text(encoding="utf-8")


def replace_once(old: str, new: str, label: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    text = text.replace(old, new, 1)


# A node whose children were already finalized used to be mistaken for a leaf.
# It must still run Combine so completed child rollups are added to the parent.
replace_once(
    "                    if pending.is_empty() {\n",
    "                    if kids.is_empty() {\n",
    "order-independent subtree combine",
)

# The synthetic group row should be visibly tied to its explicit membership,
# rather than indirectly depending on the cached recursive rollup.
replace_once(
    "            values: subtree.values(root.pid),\n",
    "            values: family_values(&members),\n",
    "explicit app-group aggregate values",
)

# Strengthen the existing aggregate regression: real snapshots are not
# guaranteed to enumerate parents before children.
replace_once(
    "        child.mem_bytes = 200;\n        let snap = snap_of(vec![root, child]);\n",
    "        child.mem_bytes = 200;\n        // Snapshot order is arbitrary: a child may precede its parent.\n        let snap = snap_of(vec![child, root]);\n",
    "child-before-parent aggregate regression",
)

# Counts were removed in the previous dead-code cleanup; this stale test-only
# assertion would otherwise break cargo test even though release builds pass.
replace_once(
    "        assert_eq!((st.count(3), st.count(2), st.count(1)), (1, 2, 3));\n",
    "",
    "stale subtree count assertion",
)

# The helper no longer computes counts, so keep its name accurate.
text = text.replace("subtree_values_and_counts", "subtree_rollups")

path.write_text(text, encoding="utf-8")
