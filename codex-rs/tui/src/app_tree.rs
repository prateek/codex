//! `/tree` command state and flow handling.
//!
//! `/tree` lets users save named branch points and later jump back by forking
//! from those saved nodes. Labels are stored as immutable rollout snapshots so
//! jumping does not mutate the saved node.

use std::path::PathBuf;

use crate::app::App;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::chatwidget::ChatWidget;
use crate::tui;
use codex_core::protocol::Op;
use tracing::warn;

#[derive(Debug, Clone)]
pub(crate) struct TreeLabelSnapshot {
    pub(crate) rollout_path: PathBuf,
}

fn normalize_tree_label(raw_label: &str) -> Option<String> {
    let label = raw_label.trim();
    (!label.is_empty()).then(|| label.to_string())
}

impl App {
    pub(crate) fn clear_tree_labels(&mut self) {
        self.tree_labels.clear();
    }

    pub(crate) fn open_tree_menu(&mut self) {
        let mut items = vec![SelectionItem {
            name: "Set label for current node".to_string(),
            description: Some("Create or update a /tree label at this point.".to_string()),
            selected_description: Some(
                "Create a named branch point for returning here later.".to_string(),
            ),
            actions: vec![Box::new(|tx: &AppEventSender| {
                tx.send(AppEvent::PromptTreeLabel);
            })],
            dismiss_on_select: true,
            search_value: Some("set label create update".to_string()),
            ..Default::default()
        }];

        if self.tree_labels.is_empty() {
            items.push(SelectionItem {
                name: "No saved labels yet".to_string(),
                description: Some("Choose the first option to create one.".to_string()),
                is_disabled: true,
                disabled_reason: Some("Create a label first.".to_string()),
                search_value: Some("no labels".to_string()),
                ..Default::default()
            });
        } else {
            for (label, snapshot) in &self.tree_labels {
                let label_for_action = label.clone();
                let label_for_name = label.clone();
                let path_display = snapshot.rollout_path.display().to_string();
                let search_value = format!("{label} {path_display}");
                let selected_description = format!("Snapshot: {path_display}");
                items.push(SelectionItem {
                    name: format!("Jump to {label_for_name}"),
                    description: Some("Create a new branch from this saved label.".to_string()),
                    selected_description: Some(selected_description),
                    actions: vec![Box::new(move |tx: &AppEventSender| {
                        tx.send(AppEvent::JumpToTreeLabel {
                            label: label_for_action.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    search_value: Some(search_value),
                    ..Default::default()
                });
            }
        }

        self.chat_widget.show_selection_view(SelectionViewParams {
            title: Some("Conversation tree".to_string()),
            subtitle: Some("Label current node or jump to a saved label".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: true,
            search_placeholder: Some("Type to search tree labels".to_string()),
            ..Default::default()
        });
    }

    pub(crate) async fn save_tree_label(&mut self, raw_label: String) {
        let Some(label) = normalize_tree_label(&raw_label) else {
            self.chat_widget
                .add_error_message("Tree label cannot be empty.".to_string());
            return;
        };

        let Some(path) = self.chat_widget.rollout_path() else {
            self.chat_widget.add_error_message(
                "A thread must contain at least one turn before it can be labeled.".to_string(),
            );
            return;
        };
        if !path.exists() {
            self.chat_widget.add_error_message(
                "A thread must contain at least one turn before it can be labeled.".to_string(),
            );
            return;
        }

        match self
            .server
            .fork_thread(usize::MAX, self.config.clone(), path.clone(), false)
            .await
        {
            Ok(snapshot) => {
                let Some(snapshot_path) = snapshot.thread.rollout_path() else {
                    if let Err(err) = snapshot.thread.submit(Op::Shutdown).await {
                        warn!(%err, "failed to shut down /tree snapshot thread");
                    }
                    self.server.remove_thread(&snapshot.thread_id).await;
                    self.chat_widget
                        .add_error_message("Failed to capture tree label snapshot.".to_string());
                    return;
                };
                if let Err(err) = snapshot.thread.submit(Op::Shutdown).await {
                    warn!(%err, "failed to shut down /tree snapshot thread");
                }
                self.server.remove_thread(&snapshot.thread_id).await;

                let replaced = self
                    .tree_labels
                    .insert(
                        label.clone(),
                        TreeLabelSnapshot {
                            rollout_path: snapshot_path.clone(),
                        },
                    )
                    .is_some();
                let status = if replaced { "Updated" } else { "Saved" };
                self.chat_widget.add_info_message(
                    format!("{status} /tree label '{label}'."),
                    Some("Run /tree to jump back to this node.".to_string()),
                );
            }
            Err(err) => {
                let path_display = path.display();
                self.chat_widget.add_error_message(format!(
                    "Failed to save /tree label '{label}' from {path_display}: {err}"
                ));
            }
        }
    }

    pub(crate) async fn jump_to_tree_label(&mut self, tui: &mut tui::Tui, raw_label: String) {
        let Some(label) = normalize_tree_label(&raw_label) else {
            self.chat_widget
                .add_error_message("Tree label cannot be empty.".to_string());
            return;
        };

        let Some(snapshot) = self.tree_labels.get(&label).cloned() else {
            self.chat_widget
                .add_error_message(format!("No /tree label named '{label}'."));
            return;
        };
        if !snapshot.rollout_path.exists() {
            let path_display = snapshot.rollout_path.display();
            self.chat_widget.add_error_message(format!(
                "Tree label '{label}' points to missing snapshot path: {path_display}"
            ));
            return;
        }

        match self
            .server
            .fork_thread(
                usize::MAX,
                self.config.clone(),
                snapshot.rollout_path.clone(),
                false,
            )
            .await
        {
            Ok(forked) => {
                self.shutdown_current_thread().await;
                let init =
                    self.chatwidget_init_for_forked_or_resumed_thread(tui, self.config.clone());
                self.chat_widget =
                    ChatWidget::new_from_existing(init, forked.thread, forked.session_configured);
                self.reset_thread_event_state();
                self.chat_widget
                    .add_info_message(format!("Jumped to /tree label '{label}'."), None);
            }
            Err(err) => {
                let path_display = snapshot.rollout_path.display();
                self.chat_widget.add_error_message(format!(
                    "Failed to jump to /tree label '{label}' from {path_display}: {err}"
                ));
            }
        }

        tui.frame_requester().schedule_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_tree_label;
    use pretty_assertions::assert_eq;

    #[test]
    fn normalize_tree_label_trims_whitespace() {
        assert_eq!(normalize_tree_label("  root  "), Some("root".to_string()));
    }

    #[test]
    fn normalize_tree_label_rejects_empty() {
        assert_eq!(normalize_tree_label("   "), None);
    }
}
