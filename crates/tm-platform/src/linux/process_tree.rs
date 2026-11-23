//! Identity-checked Linux process-tree termination.
//!
//! `/proc` parent links identify a tree only when they are paired with process
//! creation ticks. A recorded PPID can outlive its parent and later name an
//! unrelated process with the recycled PID; descendants are therefore accepted
//! only when they are not older than the process currently reached by the walk.

use std::collections::{HashMap, VecDeque};

use tm_core::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TreeLink {
    pid: u32,
    parent_pid: u32,
    start_ticks: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Descendant {
    link: TreeLink,
    parent_start_ticks: i64,
}

pub(super) fn kill_process(pid: u32, expected_start_ticks: Option<i64>, tree: bool) -> Result<()> {
    if !tree {
        return super::send_signal_checked(pid, expected_start_ticks, libc::SIGTERM);
    }
    if pid == 0 {
        return Err(tm_core::TmError::platform("kill", "system process"));
    }
    let expected = expected_start_ticks.ok_or_else(|| {
        tm_core::TmError::platform("kill", "process creation identity unavailable")
    })?;
    super::verify_process_identity(pid, Some(expected))?;

    let table = process_table()?;
    let Some(root) = table.get(&pid) else {
        // Surface a vanished/recycled root as ProcessNotFound; only a live but
        // unreadable root is an inconclusive tree-enumeration failure.
        super::verify_process_identity(pid, Some(expected))?;
        return Err(tm_core::TmError::platform(
            "terminate process tree",
            "could not establish the root process identity",
        ));
    };
    if root.start_ticks != expected {
        return Err(tm_core::TmError::ProcessNotFound { pid });
    }
    let descendants = descendants(pid, expected, &table);

    // The root must still be the selected process before any descendant is
    // touched. A vanished/reused root aborts the action without guessing.
    super::verify_process_identity(pid, Some(expected))?;

    // `/proc` is not one atomic process table. Re-read each candidate's own
    // identity and its parent generation before any signal. If an ancestor's
    // relation was invalidated, exclude its whole subtree rather than guessing.
    let mut valid_parents = std::collections::HashSet::from([pid]);
    let mut safe_descendants = Vec::new();
    for descendant in descendants {
        if !valid_parents.contains(&descendant.link.parent_pid) {
            continue;
        }
        let current = relationship_is_current(&descendant)?;
        if current {
            valid_parents.insert(descendant.link.pid);
            safe_descendants.push(descendant);
        }
    }

    let mut child_error = None;
    // BFS puts every parent before its children. Reversing it is child-first:
    // no descendant can keep a parent busy with waiting or shutdown work.
    for child in safe_descendants.iter().rev() {
        // Match the Windows action contract: never include the process running
        // the action merely because it happens to sit below the selected root.
        if child.link.pid == std::process::id() {
            continue;
        }
        match child_identity_is_current(child) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                tracing::debug!(
                    child = child.link.pid,
                    error = %error,
                    "tree-kill child identity could not be revalidated"
                );
                child_error.get_or_insert(error);
                continue;
            }
        }
        match super::send_signal_checked(
            child.link.pid,
            Some(child.link.start_ticks),
            libc::SIGTERM,
        ) {
            Ok(()) | Err(tm_core::TmError::ProcessNotFound { .. }) => {}
            Err(error) => {
                tracing::debug!(
                    child = child.link.pid,
                    error = %error,
                    "tree-kill child failed"
                );
                child_error.get_or_insert(error);
            }
        }
    }

    let root_result = super::send_signal_checked(pid, Some(expected), libc::SIGTERM);
    root_result.and(child_error.map_or(Ok(()), Err))
}

fn process_table() -> Result<HashMap<u32, TreeLink>> {
    let entries = std::fs::read_dir("/proc").map_err(|error| {
        tm_core::TmError::platform("read /proc", format!("process table: {error}"))
    })?;
    let mut table = HashMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            tm_core::TmError::platform(
                "terminate process tree",
                format!("could not enumerate /proc: {error}"),
            )
        })?;
        let Some(pid) = entry.file_name().to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        let link = match read_tree_link(pid) {
            Ok(Some(link)) => link,
            Ok(None) => {
                // A process that disappeared during the walk is no longer a
                // target. Permission, I/O, and parse failures are different:
                // they mean the tree cannot be proven complete.
                continue;
            }
            Err(error) => return Err(error),
        };
        table.insert(pid, link);
    }
    Ok(table)
}

fn read_tree_link(pid: u32) -> Result<Option<TreeLink>> {
    let path = format!("/proc/{pid}/stat");
    let stat = match std::fs::read(&path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(tm_core::TmError::platform(
                "terminate process tree",
                format!("{path}: {error}"),
            ));
        }
    };
    parse_tree_link(pid, &stat).map(Some).ok_or_else(|| {
        tm_core::TmError::platform(
            "terminate process tree",
            format!("{path}: malformed process record"),
        )
    })
}

fn parse_tree_link(pid: u32, stat: &[u8]) -> Option<TreeLink> {
    // Field 2 (`comm`) can contain spaces, parentheses, and non-UTF-8 bytes.
    // Split after its final `)` instead of breaking the line on whitespace.
    let after_comm = stat.iter().rposition(|&byte| byte == b')')? + 1;
    let mut fields = stat[after_comm..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|part| !part.is_empty());
    fields.next()?; // field 3: state
    let parent_pid = std::str::from_utf8(fields.next()?)
        .ok()?
        .parse::<u32>()
        .ok()?;
    let start_ticks = std::str::from_utf8(fields.nth(17)?)
        .ok()?
        .parse::<i64>()
        .ok()?;
    Some(TreeLink {
        pid,
        parent_pid,
        start_ticks,
    })
}

fn descendants(
    root: u32,
    root_start_ticks: i64,
    table: &HashMap<u32, TreeLink>,
) -> Vec<Descendant> {
    let mut children_of: HashMap<u32, Vec<TreeLink>> = HashMap::new();
    for link in table.values() {
        // PID 0 names itself as its parent; it must never become a child.
        if link.pid != link.parent_pid {
            children_of.entry(link.parent_pid).or_default().push(*link);
        }
    }
    for children in children_of.values_mut() {
        children.sort_unstable_by_key(|link| (link.start_ticks, link.pid));
    }

    let mut out = Vec::new();
    let mut queue = VecDeque::from([(root, root_start_ticks)]);
    let mut seen = std::collections::HashSet::from([root]);
    while let Some((parent, parent_start_ticks)) = queue.pop_front() {
        for child in children_of.get(&parent).into_iter().flatten() {
            // A child cannot predate its parent. If it does, its recorded PPID
            // belongs to a parent generation that has already exited.
            if child.start_ticks < parent_start_ticks || !seen.insert(child.pid) {
                continue;
            }
            out.push(Descendant {
                link: *child,
                parent_start_ticks,
            });
            queue.push_back((child.pid, child.start_ticks));
        }
    }
    out
}

fn relationship_is_current(descendant: &Descendant) -> Result<bool> {
    let Some(parent) = read_tree_link(descendant.link.parent_pid)? else {
        return Ok(false);
    };
    let Some(child) = read_tree_link(descendant.link.pid)? else {
        return Ok(false);
    };
    Ok(relationship_matches(&parent, &child, descendant))
}

fn child_identity_is_current(descendant: &Descendant) -> Result<bool> {
    Ok(read_tree_link(descendant.link.pid)?
        .is_some_and(|child| child.start_ticks == descendant.link.start_ticks))
}

fn relationship_matches(parent: &TreeLink, child: &TreeLink, expected: &Descendant) -> bool {
    parent.start_ticks == expected.parent_start_ticks
        && child.start_ticks == expected.link.start_ticks
        && child.parent_pid == expected.link.parent_pid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(pid: u32, parent_pid: u32, start_ticks: i64) -> TreeLink {
        TreeLink {
            pid,
            parent_pid,
            start_ticks,
        }
    }

    fn table(links: &[TreeLink]) -> HashMap<u32, TreeLink> {
        links.iter().map(|link| (link.pid, *link)).collect()
    }

    #[test]
    fn proc_stat_parses_parent_and_start_ticks_after_a_hostile_comm() {
        let stat =
            b"42 (worker (x) \xff) S 17 42 42 0 -1 4194304 10 0 2 0 1 0 0 0 20 0 4 0 12345 1000 50";
        assert_eq!(
            parse_tree_link(42, stat),
            Some(TreeLink {
                pid: 42,
                parent_pid: 17,
                start_ticks: 12345,
            })
        );
        assert_eq!(parse_tree_link(42, b"not-a-stat"), None);
    }

    #[test]
    fn descendant_walk_includes_children_and_grandchildren() {
        let table = table(&[
            link(100, 1, 1_000),
            link(200, 100, 1_100),
            link(201, 100, 1_050),
            link(300, 200, 1_200),
            link(400, 999, 1_300),
        ]);
        let pids: Vec<u32> = descendants(100, 1_000, &table)
            .into_iter()
            .map(|descendant| descendant.link.pid)
            .collect();
        assert_eq!(pids, [201, 200, 300]);
    }

    #[test]
    fn descendant_walk_rejects_children_of_a_recycled_parent_pid() {
        let table = table(&[
            link(100, 1, 1_000),   // selected root
            link(200, 100, 500),   // older process naming the recycled PPID
            link(201, 200, 600),   // that stranger's own child
            link(300, 100, 1_500), // genuine child
            link(400, 300, 1_600), // genuine grandchild
            link(500, 300, 1_200), // older than 300: recycled parent link
            link(0, 0, 0),         // idle names itself
        ]);
        let pids: Vec<u32> = descendants(100, 1_000, &table)
            .into_iter()
            .map(|descendant| descendant.link.pid)
            .collect();
        assert_eq!(pids, [300, 400]);
    }

    #[test]
    fn descendant_relation_requires_the_expected_parent_and_child_generations() {
        let expected = Descendant {
            link: link(200, 100, 1_100),
            parent_start_ticks: 1_000,
        };
        assert!(relationship_matches(
            &link(100, 1, 1_000),
            &link(200, 100, 1_100),
            &expected
        ));
        assert!(!relationship_matches(
            &link(100, 1, 2_000),
            &link(200, 100, 1_100),
            &expected
        ));
        assert!(!relationship_matches(
            &link(100, 1, 1_000),
            &link(200, 100, 2_100),
            &expected
        ));
    }

    #[test]
    fn reversed_breadth_first_order_signals_every_child_before_its_parent() {
        let links = [
            link(100, 1, 1_000),
            link(200, 100, 1_100),
            link(201, 100, 1_050),
            link(300, 200, 1_200),
            link(400, 300, 1_300),
        ];
        let tree = table(&links);
        let mut order: Vec<u32> = descendants(100, 1_000, &tree)
            .iter()
            .rev()
            .map(|descendant| descendant.link.pid)
            .collect();
        order.push(100); // the root is signalled after every descendant
        assert_eq!(order, [400, 300, 200, 201, 100]);

        let position = |pid: u32| {
            order
                .iter()
                .position(|&candidate| candidate == pid)
                .expect("target in order")
        };
        for (child, parent) in [(400, 300), (300, 200), (200, 100), (201, 100)] {
            assert!(
                position(child) < position(parent),
                "child {child} must be signalled before parent {parent}"
            );
        }
    }
}
