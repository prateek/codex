use std::sync::Arc;

use crate::history_cell::HistoryCell;

/// A named bookmark in the conversation, marking a position that can be
/// branched from or returned to.
#[derive(Clone)]
pub(crate) struct Bookmark {
    pub label: String,
    /// Number of user turns at the time the bookmark was created.
    pub nth_user_turn: usize,
}

/// A saved branch of conversation that was rolled back.  The cells record
/// the work that happened *after* the branch point so it can be displayed
/// in the tree view even though it is no longer active.
#[derive(Clone)]
pub(crate) struct SavedBranch {
    /// Label of the bookmark this branch extends from.
    pub from_label: String,
    /// Optional user-assigned name for this branch.
    pub label: Option<String>,
    /// The transcript cells produced on this branch (after the branch point).
    pub cells: Vec<Arc<dyn HistoryCell>>,
}

/// Manages the tree of conversation branches.
#[derive(Default)]
pub(crate) struct ConversationTree {
    pub bookmarks: Vec<Bookmark>,
    pub branches: Vec<SavedBranch>,
}

impl ConversationTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or update a bookmark at the given user-turn position.
    pub fn set_bookmark(&mut self, label: String, nth_user_turn: usize) {
        if let Some(existing) = self.bookmarks.iter_mut().find(|b| b.label == label) {
            existing.nth_user_turn = nth_user_turn;
        } else {
            self.bookmarks.push(Bookmark {
                label,
                nth_user_turn,
            });
        }
    }

    /// Look up a bookmark by label.
    pub fn find_bookmark(&self, label: &str) -> Option<&Bookmark> {
        self.bookmarks.iter().find(|b| b.label == label)
    }

    /// Save a branch that was produced after a bookmark.
    pub fn save_branch(
        &mut self,
        from_label: String,
        label: Option<String>,
        cells: Vec<Arc<dyn HistoryCell>>,
    ) {
        self.branches.push(SavedBranch {
            from_label,
            label,
            cells,
        });
    }

    /// Build display lines for the tree.
    pub fn display_lines(&self, current_user_turns: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push("Conversation tree:".to_string());
        if self.bookmarks.is_empty() {
            lines.push("  (no bookmarks set — use /tree label <name> to create one)".to_string());
        } else {
            for bookmark in &self.bookmarks {
                let marker = if bookmark.nth_user_turn <= current_user_turns {
                    "●"
                } else {
                    "○"
                };
                lines.push(format!(
                    "  {marker} {} [turn {}]",
                    bookmark.label, bookmark.nth_user_turn
                ));
                let child_branches: Vec<&SavedBranch> = self
                    .branches
                    .iter()
                    .filter(|b| b.from_label == bookmark.label)
                    .collect();
                for (i, branch) in child_branches.iter().enumerate() {
                    let connector = if i == child_branches.len() - 1 {
                        "└─"
                    } else {
                        "├─"
                    };
                    let branch_label = branch
                        .label
                        .as_deref()
                        .unwrap_or("(unnamed)");
                    let user_cells: usize = branch
                        .cells
                        .iter()
                        .filter(|c| {
                            c.as_any()
                                .downcast_ref::<crate::history_cell::UserHistoryCell>()
                                .is_some()
                        })
                        .count();
                    lines.push(format!(
                        "    {connector} branch: {branch_label} ({user_cells} turns)"
                    ));
                }
            }
        }
        lines.push(format!("  ◆ current [turn {current_user_turns}]"));
        lines
    }

    /// Clear all tree state (used on session reset).
    pub fn clear(&mut self) {
        self.bookmarks.clear();
        self.branches.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::AgentMessageCell;
    use crate::history_cell::UserHistoryCell;
    use pretty_assertions::assert_eq;
    use ratatui::text::Line;

    #[test]
    fn set_and_find_bookmark() {
        let mut tree = ConversationTree::new();
        tree.set_bookmark("base".to_string(), 3);
        let bm = tree.find_bookmark("base").unwrap();
        assert_eq!(bm.label, "base");
        assert_eq!(bm.nth_user_turn, 3);
    }

    #[test]
    fn update_existing_bookmark() {
        let mut tree = ConversationTree::new();
        tree.set_bookmark("base".to_string(), 3);
        tree.set_bookmark("base".to_string(), 5);
        assert_eq!(tree.bookmarks.len(), 1);
        assert_eq!(tree.bookmarks[0].nth_user_turn, 5);
    }

    #[test]
    fn save_and_display_branch() {
        let mut tree = ConversationTree::new();
        tree.set_bookmark("base".to_string(), 2);
        let cells: Vec<Arc<dyn HistoryCell>> = vec![
            Arc::new(UserHistoryCell {
                message: "explore".to_string(),
                text_elements: Vec::new(),
                local_image_paths: Vec::new(),
                remote_image_urls: Vec::new(),
            }),
            Arc::new(AgentMessageCell::new(vec![Line::from("result")], false)),
        ];
        tree.save_branch("base".to_string(), Some("explore-a".to_string()), cells);

        let lines = tree.display_lines(2);
        assert_eq!(lines[0], "Conversation tree:");
        assert!(lines[1].contains("base"));
        assert!(lines[1].contains("[turn 2]"));
        assert!(lines[2].contains("explore-a"));
        assert!(lines[2].contains("1 turns"));
        assert!(lines[3].contains("current"));
    }

    #[test]
    fn display_empty_tree() {
        let tree = ConversationTree::new();
        let lines = tree.display_lines(0);
        assert!(lines[1].contains("no bookmarks"));
    }

    #[test]
    fn clear_resets_tree() {
        let mut tree = ConversationTree::new();
        tree.set_bookmark("a".to_string(), 1);
        tree.save_branch("a".to_string(), None, Vec::new());
        tree.clear();
        assert!(tree.bookmarks.is_empty());
        assert!(tree.branches.is_empty());
    }
}
