//! The Tasks section's pieces that are the UI's (tasks.rs in taste-core
//! lists, runs, and keeps them): the section's glyph, and a run's light.

pub use taste_core::tasks::{list, outline, Listing, OutlineEntry, RunState, TaskInfo};

/// The Tasks section's glyph and a task tab's.
pub const TASK_ICON: &str = "system-run-symbolic";

/// A run's light on its row (`env-dot` classes, the fleet's own colours).
pub fn dot(state: RunState) -> &'static str {
    match state {
        RunState::Idle => "unknown",
        RunState::Running => "amber",
        RunState::Succeeded => "green",
        RunState::Failed => "red",
    }
}

/// The last lines of a failed run handed to the agent: the error is at the
/// end, and a whole build's output is more to read than to learn from.
const REPAIR_LINES: usize = 200;

/// The prompt a failed task's Prompt Agent sends, and its output as the
/// attachment it names. Written to the house rule on agent-facing text —
/// the evidence rides along, the steps are the tools it has — so any model
/// can act on it without first going to fetch what happened.
pub fn repair_prompt(name: &str, lines: &[String]) -> (String, (String, String)) {
    let file = format!("task-{}.log", name.replace([':', '/'], "-"));
    let from = lines.len().saturating_sub(REPAIR_LINES);
    let clipped = from > 0;
    let log = lines[from..].join("\n");
    let prompt = format!(
        "Task `{name}` failed in this environment.\n\n\
         WHAT IS TRUE\n\
         Its output is attached as {file}{}; the error is near its end.\n\n\
         HOW TO WORK\n\
         1. Read the attached output, and the Taskfile entry for `{name}`, before changing \
         anything.\n\
         2. Fix the cause in the project. If the cause is the environment (a tool missing \
         from the image), change .devcontainer/ and call devcontainer_reload.\n\
         3. Run it again with task_run and read the result with task_output. Stop after three \
         attempts.\n\
         4. If the fix needs something only the user can give (a credential, a service on \
         their machine), stop and say exactly what.\n\
         5. Finish with one short paragraph: the cause, and what you changed.",
        if clipped {
            format!(" (its last {REPAIR_LINES} lines)")
        } else {
            String::new()
        },
    );
    (prompt, (file, log))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repair_prompt_names_its_attachment_and_keeps_the_end() {
        let lines: Vec<String> = (0..250).map(|n| format!("line {n}")).collect();
        let (prompt, (file, log)) = repair_prompt("dev:init", &lines);
        assert_eq!(file, "task-dev-init.log");
        assert!(prompt.contains("attached as task-dev-init.log (its last 200 lines)"));
        assert!(prompt.contains("task_run"));
        assert!(log.starts_with("line 50\n"), "the end is kept");
        assert!(log.ends_with("line 249"));
        let (prompt, (_, log)) = repair_prompt("build", &lines[..3]);
        assert!(!prompt.contains("last 200"));
        assert_eq!(log, "line 0\nline 1\nline 2");
    }
}
