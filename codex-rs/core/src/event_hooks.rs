use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_protocol::protocol::Event;
use tracing::warn;

#[derive(Debug)]
pub(crate) struct EventHookRunner {
    hooks: HashMap<String, Vec<Vec<String>>>,
    cwd: PathBuf,
    seq: AtomicU64,
}

impl EventHookRunner {
    pub(crate) fn new(hooks: HashMap<String, Vec<Vec<String>>>, cwd: PathBuf) -> Self {
        Self {
            hooks,
            cwd,
            seq: AtomicU64::new(0),
        }
    }

    pub(crate) fn handle_event(&self, event: &Event) {
        if self.hooks.is_empty() {
            return;
        }

        let event_name = event.msg.to_string();
        let Some(commands) = self.hooks.get(event_name.as_str()) else {
            return;
        };

        for argv in commands {
            self.spawn_hook(argv, &event_name, &event.id);
        }
    }

    fn spawn_hook(&self, argv: &[String], event_name: &str, submission_id: &str) {
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        if program.is_empty() {
            return;
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed).to_string();
        let mut command = std::process::Command::new(program);
        command.args(args);
        command.current_dir(&self.cwd);
        command.env("CODEX_HOOK_EVENT", event_name);
        command.env("CODEX_HOOK_SUBMISSION_ID", submission_id);
        command.env("CODEX_HOOK_SEQ", &seq);

        if let Err(e) = command.spawn() {
            warn!("failed to spawn hook '{program}': {e}");
        }
    }
}
