from pathlib import Path

path = Path("crates/tm-app/src/tabs/processes.rs")
text = path.read_text(encoding="utf-8")


def replace_once(old: str, new: str, label: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    text = text.replace(old, new, 1)


replace_once(
    '''/// Exact identities hidden behind one collapsed Apps-tree row. This walk is
/// only performed for rows that are actually emitted while collapsed; an
/// expanded branch stores single-process targets and lets its visible child
/// rows carry their own scope. The total retained target set therefore stays
/// linear in the visible process model rather than duplicating every subtree.
fn collapsed_tree_targets<'a>(
    root: &'a ProcessEntry,
    children: &HashMap<u32, Vec<&'a ProcessEntry>>,
) -> Vec<crate::app::ProcessIdentity> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(process) = stack.pop() {
        if !seen.insert(process.pid) {
            continue;
        }
        if !process.synthetic {
            targets.push(process_identity(process));
        }
        if let Some(kids) = children.get(&process.pid) {
            for child in kids.iter().rev() {
                stack.push(*child);
            }
        }
    }
    targets
}

''',
    '',
    'obsolete collapsed-tree target helper',
)
replace_once('    counts: HashMap<u32, u32>,\n', '', 'Subtree.counts field')
replace_once(
    '''    fn count(&self, pid: u32) -> u32 {
        self.counts.get(&pid).copied().unwrap_or(1)
    }

''',
    '',
    'Subtree::count method',
)
replace_once(
    '    let mut counts: HashMap<u32, u32> = HashMap::with_capacity(all.len());\n',
    '',
    'counts accumulator',
)
replace_once('                    let mut cnt: u32 = 1;\n', '', 'combine count seed')
replace_once(
    '                            cnt += counts.get(&k.pid).copied().unwrap_or(1);\n',
    '',
    'child count aggregation',
)
replace_once('                    counts.insert(pid, cnt);\n', '', 'combined count insert')
replace_once('                        counts.insert(pid, 1);\n', '', 'leaf count insert')
replace_once('        counts.entry(p.pid).or_insert(1);\n', '', 'fallback count insert')
replace_once('        counts,\n', '', 'Subtree counts initializer')

path.write_text(text, encoding="utf-8")
