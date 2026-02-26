use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub(crate) struct ThreadTreeItem {
    pub(crate) id: ThreadId,
    pub(crate) forked_from_id: Option<ThreadId>,
    pub(crate) label: String,
}

/// Build a deterministic, tree-indented display list for thread branches.
///
/// Any node whose `forked_from_id` is `None` (or points at a missing node) is treated as a root.
pub(crate) fn build_thread_tree_rows(items: &[ThreadTreeItem]) -> Vec<(ThreadId, String)> {
    let mut by_id: HashMap<ThreadId, &ThreadTreeItem> = HashMap::with_capacity(items.len());
    for item in items {
        by_id.insert(item.id, item);
    }

    let mut children: HashMap<ThreadId, Vec<ThreadId>> = HashMap::new();
    let mut roots: Vec<ThreadId> = Vec::new();

    for item in items {
        if let Some(parent) = item.forked_from_id {
            if by_id.contains_key(&parent) {
                children.entry(parent).or_default().push(item.id);
                continue;
            }
        }
        roots.push(item.id);
    }

    let sort_ids = |ids: &mut Vec<ThreadId>| {
        ids.sort_by(|a, b| {
            let la = by_id.get(a).map(|it| it.label.as_str()).unwrap_or("");
            let lb = by_id.get(b).map(|it| it.label.as_str()).unwrap_or("");
            match la.cmp(lb) {
                std::cmp::Ordering::Equal => a.to_string().cmp(&b.to_string()),
                other => other,
            }
        });
    };

    sort_ids(&mut roots);
    for ids in children.values_mut() {
        sort_ids(ids);
    }

    let mut out: Vec<(ThreadId, String)> = Vec::with_capacity(items.len());
    let mut visited: HashSet<ThreadId> = HashSet::with_capacity(items.len());

    fn walk(
        node: ThreadId,
        by_id: &HashMap<ThreadId, &ThreadTreeItem>,
        children: &HashMap<ThreadId, Vec<ThreadId>>,
        ancestor_last: &mut Vec<bool>,
        out: &mut Vec<(ThreadId, String)>,
        visited: &mut HashSet<ThreadId>,
        is_last: bool,
    ) {
        if !visited.insert(node) {
            return;
        }

        let label = by_id
            .get(&node)
            .map(|it| it.label.as_str())
            .unwrap_or("<unknown>");

        let prefix = if ancestor_last.is_empty() {
            String::new()
        } else {
            let mut s = String::new();
            for last in ancestor_last.iter().copied().take(ancestor_last.len() - 1) {
                if last {
                    s.push_str("   ");
                } else {
                    s.push_str("│  ");
                }
            }
            s.push_str(if is_last { "└─ " } else { "├─ " });
            s
        };

        out.push((node, format!("{prefix}{label}")));

        let kids = children.get(&node).map(Vec::as_slice).unwrap_or(&[]);
        for (idx, child) in kids.iter().copied().enumerate() {
            let child_is_last = idx + 1 == kids.len();
            ancestor_last.push(is_last);
            walk(
                child,
                by_id,
                children,
                ancestor_last,
                out,
                visited,
                child_is_last,
            );
            ancestor_last.pop();
        }
    }

    for (idx, root) in roots.iter().copied().enumerate() {
        let is_last = idx + 1 == roots.len();
        walk(
            root,
            &by_id,
            &children,
            &mut Vec::new(),
            &mut out,
            &mut visited,
            is_last,
        );
    }

    // Ensure every item appears even if parent pointers create a cycle.
    if visited.len() < items.len() {
        let mut remaining: Vec<ThreadId> = items
            .iter()
            .map(|it| it.id)
            .filter(|id| !visited.contains(id))
            .collect();
        remaining.sort_by_key(ToString::to_string);
        for id in remaining {
            out.push((
                id,
                by_id
                    .get(&id)
                    .map(|it| it.label.clone())
                    .unwrap_or_else(|| id.to_string()),
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn id(uuid: &str) -> ThreadId {
        ThreadId::from_string(uuid).expect("valid thread id")
    }

    #[test]
    fn builds_simple_tree_order_and_indentation() {
        let a = id("00000000-0000-0000-0000-000000000001");
        let b = id("00000000-0000-0000-0000-000000000002");
        let c = id("00000000-0000-0000-0000-000000000003");
        let d = id("00000000-0000-0000-0000-000000000004");

        let items = vec![
            ThreadTreeItem {
                id: a,
                forked_from_id: None,
                label: "main".to_string(),
            },
            ThreadTreeItem {
                id: b,
                forked_from_id: Some(a),
                label: "branch-b".to_string(),
            },
            ThreadTreeItem {
                id: c,
                forked_from_id: Some(a),
                label: "branch-c".to_string(),
            },
            ThreadTreeItem {
                id: d,
                forked_from_id: Some(b),
                label: "branch-d".to_string(),
            },
        ];

        let rows = build_thread_tree_rows(&items)
            .into_iter()
            .map(|(_, s)| s)
            .collect::<Vec<_>>();

        assert_eq!(
            rows,
            vec![
                "main".to_string(),
                "├─ branch-b".to_string(),
                "│  └─ branch-d".to_string(),
                "└─ branch-c".to_string(),
            ]
        );
    }

    #[test]
    fn treats_missing_parent_as_root() {
        let orphan = id("00000000-0000-0000-0000-000000000010");
        let missing = id("00000000-0000-0000-0000-000000000011");
        let items = vec![ThreadTreeItem {
            id: orphan,
            forked_from_id: Some(missing),
            label: "orphan".to_string(),
        }];
        let rows = build_thread_tree_rows(&items)
            .into_iter()
            .map(|(_, s)| s)
            .collect::<Vec<_>>();
        assert_eq!(rows, vec!["orphan".to_string()]);
    }
}
