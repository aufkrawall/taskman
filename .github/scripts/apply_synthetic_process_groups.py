from pathlib import Path
import re


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def replace_re(text: str, pattern: str, replacement: str, label: str) -> str:
    updated, count = re.subn(pattern, replacement, text, count=1, flags=re.S)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one regex match, found {count}")
    return updated


path = Path("crates/tm-app/src/tabs/processes.rs")
text = path.read_text(encoding="utf-8")

text = replace_once(
    text,
    "//! * Group header counts never depend on expansion state (P0.4). Apps counts\n",
    "//! * Expandable application/family rows are presentation-only aggregate rows:\n//!   they sum CPU, memory, disk and network for the represented members. Once\n//!   expanded, every concrete process — including the former root/head — is a\n//!   child row with only its own resource values.\n//! * Group header counts never depend on expansion state (P0.4). Apps counts\n",
    "module aggregate docs",
)

text = replace_once(
    text,
    "    pub children: bool,\n    /// Exact process identities represented by this visible row for End task.\n",
    "    pub children: bool,\n    /// Presentation-only application/family summary. It deliberately reuses\n    /// the representative process identity as its stable expansion key, but\n    /// is not itself an OS process. When expanded, the concrete representative\n    /// appears below it as an ordinary row with its own values.\n    pub aggregate: bool,\n    /// Exact process identities represented by this visible row for End task.\n",
    "RowData aggregate field",
)

text = replace_once(
    text,
    "    /// A collapsed group contains every hidden member; an expanded group head\n    /// and ordinary rows contain only themselves. Other process actions keep\n",
    "    /// An aggregate group contains every represented member; ordinary process\n    /// rows contain only themselves. Other process actions keep\n",
    "termination target docs",
)

text = replace_once(
    text,
    "fn identity_of(row: &RowData) -> crate::app::ProcessIdentity {\n    crate::app::ProcessIdentity {\n        pid: row.pid,\n        start_epoch_s: row.start_epoch_s,\n    }\n}\n\n",
    "fn identity_of(row: &RowData) -> crate::app::ProcessIdentity {\n    crate::app::ProcessIdentity {\n        pid: row.pid,\n        start_epoch_s: row.start_epoch_s,\n    }\n}\n\nfn row_is_selectable(row: &RowData, expanded: &HashSet<u32>) -> bool {\n    !row.synthetic && (!row.aggregate || !expanded.contains(&row.pid))\n}\n\n",
    "row selectable helper",
)

text = replace_once(
    text,
    "                DisplayRow::Process(row) if !row.synthetic => Some((row.pid, row.name.as_str())),\n",
    "                DisplayRow::Process(row)\n                    if row_is_selectable(row, &app.processes_state.expanded) =>\n                {\n                    Some((row.pid, row.name.as_str()))\n                }\n",
    "typeahead selectable group",
)
text = replace_once(
    text,
    "                DisplayRow::Process(row) if row.pid == pid => Some(row),\n",
    "                DisplayRow::Process(row)\n                    if row.pid == pid\n                        && row_is_selectable(row, &app.processes_state.expanded) =>\n                {\n                    Some(row)\n                }\n",
    "typeahead target selectable",
)
text = replace_once(
    text,
    "            .position(|r| matches!(r, DisplayRow::Process(p) if p.pid == pid))\n",
    "            .position(|r| matches!(r, DisplayRow::Process(p) if p.pid == pid && row_is_selectable(p, &app.processes_state.expanded)))\n",
    "focus row selectable",
)

text = replace_once(
    text,
    "            DisplayRow::Process(row) if !row.synthetic => Some((display_idx, row)),\n",
    "            DisplayRow::Process(row)\n                if row_is_selectable(row, &app.processes_state.expanded) =>\n            {\n                Some((display_idx, row))\n            }\n",
    "keyboard selectable rows",
)
text = replace_once(
    text,
    "        .position(|row| matches!(row, DisplayRow::Process(p) if p.pid == pid))\n",
    "        .position(|row| matches!(row, DisplayRow::Process(p) if p.pid == pid && row_is_selectable(p, &app.processes_state.expanded)))\n",
    "keyboard selected row lookup",
)

old_left = '''    } else if left {\n        if current.children && app.processes_state.expanded.contains(&pid) {\n            app.processes_state.toggle_expanded(pid);\n        } else if current.depth > 0\n            && let Some(parent) = rows[..display_idx].iter().rev().find_map(|row| match row {\n                DisplayRow::Process(parent)\n                    if !parent.synthetic && parent.depth < current.depth =>\n                {\n                    Some(parent)\n                }\n                _ => None,\n            })\n        {\n            select_row(app, parent);\n        }\n    }\n'''
new_left = '''    } else if left {\n        if current.children && app.processes_state.expanded.contains(&pid) {\n            app.processes_state.toggle_expanded(pid);\n        } else if current.depth == 1 {\n            // The virtual aggregate is the parent of every first-level real\n            // process. It is deliberately not selectable while expanded; a\n            // Left key collapses it instead of moving selection onto a row\n            // that does not represent an OS process.\n            let group = rows[..display_idx]\n                .iter()\n                .rev()\n                .find(|row| matches!(row, DisplayRow::Process(parent) if parent.depth == 0))\n                .and_then(|row| match row {\n                    DisplayRow::Process(parent) if parent.aggregate => Some(parent.pid),\n                    _ => None,\n                });\n            if let Some(group_pid) = group {\n                app.processes_state.toggle_expanded(group_pid);\n            }\n        } else if current.depth > 1\n            && let Some(parent) = rows[..display_idx].iter().rev().find_map(|row| match row {\n                DisplayRow::Process(parent)\n                    if !parent.synthetic && !parent.aggregate && parent.depth < current.depth =>\n                {\n                    Some(parent)\n                }\n                _ => None,\n            })\n        {\n            select_row(app, parent);\n        }\n    }\n'''
text = replace_once(text, old_left, new_left, "keyboard left aggregate behavior")

old_selectable = '''fn selectable_identities(rows: &[DisplayRow]) -> Vec<crate::app::ProcessIdentity> {\n    rows.iter()\n        .filter_map(|row| match row {\n            DisplayRow::Process(row) if !row.synthetic => Some(identity_of(row)),\n            _ => None,\n        })\n        .collect()\n}\n'''
new_selectable = '''fn selectable_identities(rows: &[DisplayRow]) -> Vec<crate::app::ProcessIdentity> {\n    let mut seen = HashSet::new();\n    rows.iter()\n        .filter_map(|row| match row {\n            DisplayRow::Process(row) if !row.synthetic => {\n                let identity = identity_of(row);\n                seen.insert(identity.clone()).then_some(identity)\n            }\n            _ => None,\n        })\n        .collect()\n}\n'''
text = replace_once(text, old_selectable, new_selectable, "deduplicate aggregate identity")

text = replace_once(
    text,
    "        let identity = identity_of(&row);\n        if selected.contains(&identity) {\n",
    "        if row.aggregate && app.processes_state.expanded.contains(&row.pid) {\n            continue;\n        }\n        let identity = identity_of(&row);\n        if selected.contains(&identity) {\n",
    "termination selection skips expanded aggregate",
)

text = replace_once(
    text,
    "    let selected = app.selection.contains_pid(row.pid);\n    let (rect, resp) = table.row(ui, pal, selected, (row.pid, row.start_epoch_s));\n\n    // Chevron + icon + name.\n    let expanded = app.processes_state.expanded.contains(&row.pid);\n",
    "    let expanded = app.processes_state.expanded.contains(&row.pid);\n    let selectable = row_is_selectable(row, &app.processes_state.expanded);\n    let selected = selectable && app.selection.contains_pid(row.pid);\n    let (rect, resp) = table.row(\n        ui,\n        pal,\n        selected,\n        (row.pid, row.start_epoch_s, row.aggregate),\n    );\n\n    // Chevron + icon + name.\n",
    "row ui aggregate identity",
)

old_click = '''    if resp.clicked() {\n        if row.synthetic {\n            // A pseudo-row owns no process; selecting it would arm the\n            // toolbar for a target that cannot be acted on.\n            app.selection.clear();\n        } else {\n            let kind =\n                crate::selection::ClickKind::from_modifiers(&ui.input(|input| input.modifiers));\n            // Materialized here rather than once per frame: a Shift range is\n            // the only thing that needs it, and this runs on a click.\n            app.selection\n                .click(kind, identity_of(row), &selectable_identities(all_rows));\n        }\n    }\n    // A right-click inside an existing multi-selection keeps it, so the menu\n    // can act on the whole set; outside it the selection follows the row.\n    if resp.secondary_clicked() && !row.synthetic && !app.selection.contains_pid(row.pid) {\n        app.selection.select_single(identity_of(row));\n    }\n'''
new_click = '''    if resp.clicked() {\n        if row.synthetic {\n            // A pseudo-row owns no process; selecting it would arm the\n            // toolbar for a target that cannot be acted on.\n            app.selection.clear();\n        } else if row.aggregate && expanded {\n            // Once open, the aggregate is a pure virtual parent. Clicking its\n            // body behaves like its chevron instead of selecting the concrete\n            // root process that is now visible directly below it.\n            app.processes_state.toggle_expanded(row.pid);\n        } else if selectable {\n            let kind =\n                crate::selection::ClickKind::from_modifiers(&ui.input(|input| input.modifiers));\n            // Materialized here rather than once per frame: a Shift range is\n            // the only thing that needs it, and this runs on a click.\n            app.selection\n                .click(kind, identity_of(row), &selectable_identities(all_rows));\n        }\n    }\n    // A right-click inside an existing multi-selection keeps it, so the menu\n    // can act on the whole set; outside it the selection follows the row.\n    if resp.secondary_clicked() && selectable && !app.selection.contains_pid(row.pid) {\n        app.selection.select_single(identity_of(row));\n    }\n'''
text = replace_once(text, old_click, new_click, "row click aggregate behavior")
text = replace_once(
    text,
    "            && app.selection.primary().is_some_and(|primary| {\n",
    "            && selectable\n            && app.selection.primary().is_some_and(|primary| {\n",
    "keyboard context skips expanded aggregate",
)

old_search = '''    if !q.is_empty() {\n        let mut matched: Vec<&ProcessEntry> = all\n            .iter()\n            .copied()\n            .filter(|p| q.matches_process(p))\n            .collect();\n        sort_entries(&mut matched, sort_col, ascending, &subtree.values);\n        for p in matched {\n            out.push(make_flat_row(p, &subtree));\n        }\n        normalize_heat(&mut out);\n        return out;\n    }\n'''
new_search = '''    if !q.is_empty() {\n        let mut matched: Vec<&ProcessEntry> = all\n            .iter()\n            .copied()\n            .filter(|p| q.matches_process(p))\n            .collect();\n        // Search is a flat process view: concrete process rows must show and\n        // sort by their OWN values, never by hidden descendants. Aggregation\n        // belongs exclusively to the explicit virtual group rows below.\n        let own = matched\n            .iter()\n            .map(|process| (process.pid, own_values(process)))\n            .collect::<HashMap<_, _>>();\n        sort_entries(&mut matched, sort_col, ascending, &own);\n        for p in matched {\n            out.push(make_flat_row(p));\n        }\n        normalize_heat(&mut out);\n        return out;\n    }\n'''
text = replace_once(text, old_search, new_search, "search own values")

old_emit_tree_pattern = r'''fn emit_tree<'a>\(\n.*?\n\}\n\n/// Per-pid subtree rollups shared by every Processes row builder\.'''
new_emit_tree = '''fn emit_tree<'a>(\n    out: &mut Vec<DisplayRow>,\n    roots: &[&'a ProcessEntry],\n    children: &HashMap<u32, Vec<&'a ProcessEntry>>,\n    subtree: &Subtree,\n    sort_col: usize,\n    ascending: bool,\n    expanded: &HashSet<u32>,\n) {\n    let mut sorted_roots: Vec<&ProcessEntry> = roots.to_vec();\n    sort_entries(&mut sorted_roots, sort_col, ascending, &subtree.values);\n\n    for root in sorted_roots {\n        let members = tree_group_members(root, children);\n        if members.len() <= 1 {\n            out.push(make_own_row(root, 0));\n            continue;\n        }\n        let net_available = members\n            .iter()\n            .any(|member| member.net_recv_bps.is_some() || member.net_sent_bps.is_some());\n        out.push(DisplayRow::Process(RowData {\n            pid: root.pid,\n            start_epoch_s: root.start_epoch_s,\n            depth: 0,\n            name: format!("{} [{}]", process_display_name(root), members.len()),\n            icon_path: root\n                .exe_path\n                .as_ref()\n                .map(|path| path.to_string_lossy().into_owned()),\n            children: true,\n            aggregate: true,\n            termination_targets: members\n                .iter()\n                .filter(|member| !member.synthetic)\n                .map(|member| process_identity(member))\n                .collect(),\n            values: subtree.values(root.pid),\n            heat: [0.0; 4],\n            net_available,\n            status: subtree.status(root.pid),\n            power_throttled: subtree.efficiency(root.pid),\n            synthetic: false,\n            tooltip: None,\n        }));\n\n        if expanded.contains(&root.pid) {\n            emit_expanded_tree_members(\n                out, root, children, &members, sort_col, ascending,\n            );\n        }\n    }\n}\n\nfn tree_group_members<'a>(\n    root: &'a ProcessEntry,\n    children: &HashMap<u32, Vec<&'a ProcessEntry>>,\n) -> Vec<&'a ProcessEntry> {\n    let mut members = Vec::new();\n    let mut seen = HashSet::new();\n    let mut stack = vec![root];\n    while let Some(process) = stack.pop() {\n        if !seen.insert(process.pid) {\n            continue;\n        }\n        members.push(process);\n        if let Some(kids) = children.get(&process.pid) {\n            stack.extend(kids.iter().copied());\n        }\n    }\n    members\n}\n\nfn emit_expanded_tree_members<'a>(\n    out: &mut Vec<DisplayRow>,\n    root: &'a ProcessEntry,\n    children: &HashMap<u32, Vec<&'a ProcessEntry>>,\n    members: &[&'a ProcessEntry],\n    sort_col: usize,\n    ascending: bool,\n) {\n    let own = members\n        .iter()\n        .map(|process| (process.pid, own_values(process)))\n        .collect::<HashMap<_, _>>();\n    let member_pids = members.iter().map(|process| process.pid).collect::<HashSet<_>>();\n    let mut seen = HashSet::new();\n    let mut stack = vec![(root, 1usize)];\n    while let Some((process, depth)) = stack.pop() {\n        if !seen.insert(process.pid) {\n            continue;\n        }\n        out.push(make_own_row(process, depth));\n        let mut kids = children\n            .get(&process.pid)\n            .cloned()\n            .unwrap_or_default()\n            .into_iter()\n            .filter(|child| member_pids.contains(&child.pid))\n            .collect::<Vec<_>>();\n        sort_entries(&mut kids, sort_col, ascending, &own);\n        for child in kids.into_iter().rev() {\n            stack.push((child, depth + 1));\n        }\n    }\n}\n\n/// Per-pid subtree rollups shared by every Processes row builder.'''
text = replace_re(text, old_emit_tree_pattern, new_emit_tree, "replace app tree emitter")

old_flat_pattern = r'''fn emit_flat_with_family_groups\(\n.*?\n\}\n\nfn derive_display_groups'''
new_flat = '''fn emit_flat_with_family_groups(\n    out: &mut Vec<DisplayRow>,\n    members: &[&ProcessEntry],\n    children: &HashMap<u32, Vec<&ProcessEntry>>,\n    sort_col: usize,\n    ascending: bool,\n    expanded: &HashSet<u32>,\n) {\n    let mut family_heads: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();\n    let mut swallowed: HashSet<u32> = HashSet::new();\n    for p in members {\n        if swallowed.contains(&p.pid) {\n            continue;\n        }\n        let fam = application_family(p, children);\n        if fam.len() > 1 {\n            for member in fam.iter().skip(1) {\n                swallowed.insert(member.pid);\n            }\n            family_heads.insert(p.pid, fam);\n        }\n    }\n\n    let mut runs: HashMap<(u32, String), Vec<&ProcessEntry>> = HashMap::new();\n    for p in members {\n        if swallowed.contains(&p.pid) || family_heads.contains_key(&p.pid) {\n            continue;\n        }\n        if let Some(key) = sibling_run_key(p) {\n            runs.entry(key).or_default().push(p);\n        }\n    }\n    for (_, mut run) in runs {\n        if run.len() < 2 {\n            continue;\n        }\n        run.sort_by_key(|p| p.pid);\n        let head = run[0].pid;\n        for member in run.iter().skip(1) {\n            swallowed.insert(member.pid);\n        }\n        family_heads.insert(head, run);\n    }\n\n    let mut repr: HashMap<u32, [f64; 4]> = HashMap::with_capacity(members.len());\n    for p in members {\n        if let Some(fam) = family_heads.get(&p.pid) {\n            repr.insert(p.pid, family_values(fam));\n        } else if !swallowed.contains(&p.pid) {\n            repr.insert(p.pid, own_values(p));\n        }\n    }\n    let mut display: Vec<&ProcessEntry> = members\n        .iter()\n        .copied()\n        .filter(|p| !swallowed.contains(&p.pid))\n        .collect();\n    sort_entries(&mut display, sort_col, ascending, &repr);\n\n    for p in display {\n        if let Some(fam) = family_heads.get(&p.pid) {\n            let net_available = fam\n                .iter()\n                .any(|member| member.net_recv_bps.is_some() || member.net_sent_bps.is_some());\n            out.push(DisplayRow::Process(RowData {\n                pid: p.pid,\n                start_epoch_s: p.start_epoch_s,\n                depth: 0,\n                name: format!("{} [{}]", process_display_name(p), fam.len()),\n                icon_path: p\n                    .exe_path\n                    .as_ref()\n                    .map(|path| path.to_string_lossy().into_owned()),\n                children: true,\n                aggregate: true,\n                termination_targets: fam\n                    .iter()\n                    .filter(|member| !member.synthetic)\n                    .map(|member| process_identity(member))\n                    .collect(),\n                values: repr.get(&p.pid).copied().unwrap_or([0.0; 4]),\n                heat: [0.0; 4],\n                net_available,\n                status: if fam\n                    .iter()\n                    .any(|member| member.status == ProcStatus::NotResponding)\n                {\n                    ProcStatus::NotResponding\n                } else if fam\n                    .iter()\n                    .any(|member| member.status == ProcStatus::Suspended)\n                {\n                    ProcStatus::Suspended\n                } else {\n                    p.status\n                },\n                power_throttled: fam\n                    .iter()\n                    .any(|member| member.power_throttled == Some(true)),\n                synthetic: false,\n                tooltip: None,\n            }));\n            if expanded.contains(&p.pid) {\n                // The virtual row owns the aggregate; every real member,\n                // including the representative/root, is rendered below it\n                // with its own metrics. Keep the representative first, then\n                // sort the remaining members by the active column.\n                out.push(make_own_row(p, 1));\n                let mut rest = fam\n                    .iter()\n                    .copied()\n                    .filter(|member| member.pid != p.pid)\n                    .collect::<Vec<_>>();\n                let own = rest\n                    .iter()\n                    .map(|member| (member.pid, own_values(member)))\n                    .collect::<HashMap<_, _>>();\n                sort_entries(&mut rest, sort_col, ascending, &own);\n                for member in rest {\n                    out.push(make_own_row(member, 1));\n                }\n            }\n        } else {\n            out.push(make_own_row(p, 0));\n        }\n    }\n}\n\nfn derive_display_groups'''
text = replace_re(text, old_flat_pattern, new_flat, "replace flat family emitter")

old_make_flat_pattern = r'''fn make_flat_row\(p: &ProcessEntry, subtree: &Subtree\) -> DisplayRow \{\n.*?\n\}\n\n/// The process's own resource values'''
new_make_flat = '''fn make_flat_row(p: &ProcessEntry) -> DisplayRow {\n    DisplayRow::Process(RowData {\n        pid: p.pid,\n        start_epoch_s: p.start_epoch_s,\n        depth: 0,\n        name: process_display_name(p),\n        icon_path: p\n            .exe_path\n            .as_ref()\n            .map(|path| path.to_string_lossy().into_owned()),\n        children: false,\n        aggregate: false,\n        termination_targets: vec![process_identity(p)],\n        values: own_values(p),\n        heat: [0.0; 4],\n        net_available: p.net_recv_bps.is_some() || p.net_sent_bps.is_some(),\n        status: p.status,\n        power_throttled: p.power_throttled == Some(true),\n        synthetic: p.synthetic,\n        tooltip: synthetic_tooltip(p),\n    })\n}\n\n/// The process's own resource values'''
text = replace_re(text, old_make_flat_pattern, new_make_flat, "flat row own values")
text = replace_once(
    text,
    "        children: false,\n        termination_targets: vec![process_identity(p)],\n        values: own_values(p),\n",
    "        children: false,\n        aggregate: false,\n        termination_targets: vec![process_identity(p)],\n        values: own_values(p),\n",
    "own row aggregate false",
)

# Tests: pin the new virtual-parent semantics and update former-head expectations.
text = replace_once(
    text,
    '''        let root = rows\n            .iter()\n            .find_map(|row| match row {\n                DisplayRow::Process(row) if row.pid == 1 => Some(row),\n                _ => None,\n            })\n            .expect("expanded root");\n        assert_eq!(\n            root.termination_targets\n                .iter()\n                .map(|identity| identity.pid)\n                .collect::<Vec<_>>(),\n            [1]\n        );\n        let child = rows\n            .iter()\n            .find_map(|row| match row {\n                DisplayRow::Process(row) if row.pid == 2 => Some(row),\n                _ => None,\n            })\n            .expect("collapsed child");\n        let mut child_targets = child\n            .termination_targets\n            .iter()\n            .map(|identity| identity.pid)\n            .collect::<Vec<_>>();\n        child_targets.sort_unstable();\n        assert_eq!(child_targets, [2, 3]);\n''',
    '''        let aggregate = rows\n            .iter()\n            .find_map(|row| match row {\n                DisplayRow::Process(row) if row.pid == 1 && row.aggregate => Some(row),\n                _ => None,\n            })\n            .expect("expanded aggregate");\n        assert_eq!(\n            aggregate\n                .termination_targets\n                .iter()\n                .map(|identity| identity.pid)\n                .collect::<Vec<_>>(),\n            [1, 2, 3]\n        );\n        let root = rows\n            .iter()\n            .find_map(|row| match row {\n                DisplayRow::Process(row) if row.pid == 1 && !row.aggregate => Some(row),\n                _ => None,\n            })\n            .expect("expanded concrete root");\n        assert_eq!(root.termination_targets.iter().map(|identity| identity.pid).collect::<Vec<_>>(), [1]);\n        let child = rows\n            .iter()\n            .find_map(|row| match row {\n                DisplayRow::Process(row) if row.pid == 2 && !row.aggregate => Some(row),\n                _ => None,\n            })\n            .expect("expanded child");\n        assert_eq!(child.termination_targets.iter().map(|identity| identity.pid).collect::<Vec<_>>(), [2]);\n''',
    "termination test expanded semantics",
)

text = replace_once(
    text,
    "        assert_eq!(depths, vec![0, 1, 2, 3]);\n",
    "        assert_eq!(depths, vec![0, 1, 2, 3, 4]);\n",
    "deep tree depths",
)

old_grouped_test = r'''    #\[test\]\n    fn grouped_label_counts_entire_subtree\(\) \{\n.*?\n    \}\n\n    #\[test\]\n    fn subtree_aggregation_counts_all_descendants'''
new_grouped_test = '''    #[test]\n    fn grouped_label_counts_entire_subtree() {\n        let mut snap_procs = vec![\n            proc(1, None, "Brave", ProcCategory::App),\n            proc(2, Some(1), "Child", ProcCategory::App),\n            proc(3, Some(2), "GC", ProcCategory::App),\n            proc(4, Some(3), "GGC", ProcCategory::App),\n        ];\n        for p in &mut snap_procs {\n            p.cpu_pct = 0.0;\n        }\n        let snap = snap_of(snap_procs);\n        let groups = [false; 3];\n        let collapsed = build_display_rows(&snap, "", 0, true, &HashSet::new(), &groups);\n        let apps = rows_in_group(&collapsed, 0);\n        assert_eq!(apps.len(), 1);\n        assert_eq!(apps[0].name, "Brave [4]");\n        assert!(apps[0].aggregate);\n\n        let expanded = HashSet::from([1u32]);\n        let open = build_display_rows(&snap, "", 0, true, &expanded, &groups);\n        let apps = rows_in_group(&open, 0);\n        assert_eq!(apps.len(), 5, "virtual group plus all four real processes");\n        assert_eq!(apps[0].name, "Brave [4]");\n        assert!(apps[0].aggregate);\n        assert_eq!((apps[1].pid, apps[1].depth, apps[1].aggregate), (1, 1, false));\n        assert_eq!((apps[2].pid, apps[2].depth), (2, 2));\n        assert_eq!((apps[3].pid, apps[3].depth), (3, 3));\n        assert_eq!((apps[4].pid, apps[4].depth), (4, 4));\n    }\n\n    #[test]\n    fn app_group_aggregate_is_separate_from_the_real_root_values() {\n        let mut root = proc(1, None, "app.exe", ProcCategory::App);\n        root.has_window = true;\n        root.cpu_pct = 1.0;\n        root.mem_bytes = 100;\n        let mut child = proc(2, Some(1), "app.exe", ProcCategory::App);\n        child.cpu_pct = 2.0;\n        child.mem_bytes = 200;\n        let snap = snap_of(vec![root, child]);\n\n        let collapsed = build_display_rows(&snap, "", 0, true, &HashSet::new(), &[false; 3]);\n        let apps = rows_in_group(&collapsed, 0);\n        assert_eq!(apps.len(), 1);\n        assert!(apps[0].aggregate);\n        assert_eq!(apps[0].values[0], 3.0);\n        assert_eq!(apps[0].values[1], 300.0);\n\n        let open = build_display_rows(&snap, "", 0, true, &HashSet::from([1]), &[false; 3]);\n        let apps = rows_in_group(&open, 0);\n        assert_eq!(apps.len(), 3);\n        assert!(apps[0].aggregate);\n        assert_eq!(apps[0].values[0], 3.0);\n        assert_eq!((apps[1].pid, apps[1].values[0], apps[1].values[1]), (1, 1.0, 100.0));\n        assert_eq!((apps[2].pid, apps[2].values[0], apps[2].values[1]), (2, 2.0, 200.0));\n    }\n\n    #[test]\n    fn subtree_aggregation_counts_all_descendants'''
text = replace_re(text, old_grouped_test, new_grouped_test, "grouped label tests")

text = replace_once(
    text,
    "        assert_eq!(bg_open.len(), 4, \"group row plus members\");\n        assert_eq!(bg_open[0].name, \"Dropbox.exe [4]\");\n        assert!(bg_open[1..].iter().all(|r| r.depth == 1));\n        assert!(bg_open[1..].iter().all(|r| !r.children));\n",
    "        assert_eq!(bg_open.len(), 5, \"virtual group row plus all four real members\");\n        assert_eq!(bg_open[0].name, \"Dropbox.exe [4]\");\n        assert!(bg_open[0].aggregate);\n        assert_eq!(bg_open[1].pid, 1, \"the concrete former head is the first child\");\n        assert!(bg_open[1..].iter().all(|r| r.depth == 1 && !r.aggregate));\n        assert!(bg_open[1..].iter().all(|r| !r.children));\n",
    "background expanded members",
)
text = replace_once(
    text,
    "        assert_eq!(sys.len(), 5, \"expanded group exposes every host\");\n        assert_eq!(sys.iter().filter(|row| row.depth == 1).count(), 3);\n",
    "        assert_eq!(sys.len(), 6, \"services.exe + virtual group + every host\");\n        assert_eq!(sys.iter().filter(|row| row.depth == 1).count(), 4);\n",
    "svchost expanded count",
)
text = replace_once(
    text,
    "        assert_eq!(apps.len(), 2, \"family row plus expandable child\");\n",
    "        assert_eq!(apps.len(), 3, \"virtual family row plus both real processes\");\n",
    "busy same image expanded count",
)

old_resort = '''        // fam.exe family aggregates to 30 % → first block; its expanded\n        // worker rides along; other.exe (10 %) follows. Headers are gone\n        // in this view.\n        assert_eq!(pids, vec![1, 2, 3]);\n        let DisplayRow::Process(head_row) = &rows[0] else {\n            panic!("first row must be a process row");\n        };\n        assert!(head_row.children, "family head stays expandable");\n'''
new_resort = '''        // fam.exe family aggregates to 30 % → first block. The virtual\n        // aggregate is followed by BOTH real family members; other.exe\n        // (10 %) follows. Headers are gone in this view.\n        assert_eq!(pids, vec![1, 1, 2, 3]);\n        let DisplayRow::Process(head_row) = &rows[0] else {\n            panic!("first row must be a process row");\n        };\n        assert!(head_row.children && head_row.aggregate, "first row is the virtual family aggregate");\n        let DisplayRow::Process(real_root) = &rows[1] else {\n            panic!("second row must be the real root");\n        };\n        assert!(!real_root.aggregate);\n        assert_eq!(real_root.values[0], 0.0, "real root keeps only its own CPU");\n'''
text = replace_once(text, old_resort, new_resort, "resource resort group semantics")

path.write_text(text, encoding="utf-8")

# Durable docs.
path = Path("llm-wiki/repo-map.md")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    "  A group's aggregate is\n  `family_values` — the MEMBERS' own values, not the subtree's, because a\n",
    "  Expandable application/family rows are virtual presentation parents: the\n  virtual row owns the aggregate, while expansion reveals every concrete member\n  (including the former head/root) as a child with only its own metrics. Flat\n  Background/Windows family aggregates use `family_values` — the MEMBERS' own\n  values, not the subtree's, because a\n",
    "repo map virtual group semantics",
)
path.write_text(text, encoding="utf-8")

path = Path("llm-wiki/current.md")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    "- **Application grouping on Processes** keeps foreground executables visible unless\n",
    "- **Application grouping on Processes** uses explicit virtual aggregate rows: a\n  collapsed group reports the summed CPU/memory/disk/network of all represented\n  processes, while expansion shows every real process — including the former\n  root/head — underneath with only its own values. It keeps foreground executables visible unless\n",
    "current virtual group summary",
)
path.write_text(text, encoding="utf-8")

path = Path("llm-wiki/log/recent.md")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    "# Recent Activity\n\n",
    "# Recent Activity\n\n## 2026-09-08 — Process groups use virtual aggregate parents\n\n1. **Group totals are no longer disguised as a real process.** Every expandable\n   Apps/Background/Windows family has a presentation-only parent row whose CPU,\n   memory, disk and network values are the sum of its represented members.\n2. **Expansion reveals every concrete process.** The process that previously\n   doubled as the group header is now the first real child; all real rows show\n   only their own resource values. App descendants retain hierarchy indentation,\n   while flat Background/Windows families remain one level deep.\n3. **Search is unambiguously per-process.** Search results are flat concrete rows\n   and therefore sort/render by each process's own metrics rather than a hidden\n   subtree aggregate.\n4. Regression coverage pins aggregate-vs-root resource values, full expanded\n   membership (including svchost groups), hierarchy depth, resource-sorted block\n   attachment and collapsed End-task membership.\n\n",
    "recent virtual group log",
)
path.write_text(text, encoding="utf-8")
