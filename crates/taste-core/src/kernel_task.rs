//! The notebooks' kernel is the project's own task.
//!
//! The IDE runs no kernel of its own. A notebook runs in the kernel the
//! project's `jupyter:kernel` task starts — one per environment, which its
//! open notebooks share — and the IDE's client in the container connects
//! to it through the connection file the kernel writes at a fixed place.
//! So the project chooses the Python, the kernel, and its environment, as
//! it does for every other thing it runs (David, 2026-10-11: "instruct the
//! agent to define a task to run the kernel at an expected name. This also
//! allows the agent to parameterize the kernel runtime as necessary"). A
//! project without the task has notebooks that say so, and offer to ask
//! the agent for it.
//!
//! One text says the convention, wherever an agent reads it: the IDE's
//! instructions, the devcontainer authoring prompt, and a notebook's
//! Prompt Agent.

/// The task's name.
pub const TASK: &str = "jupyter:kernel";

/// Where, in the environment's container, the kernel writes its
/// connection file, and the IDE's client reads it.
pub const CONNECTION_FILE: &str = "/tmp/taste-ide-kernel.json";

/// The convention, for an agent: what to define, and what the IDE does
/// with it.
pub fn convention() -> String {
    format!(
        "The IDE runs no kernel of its own: notebooks run in the kernel the project's \
         `{TASK}` task starts, one per environment, shared by the notebooks open in it. Define \
         it in the Taskfile as a long-running task that starts a Jupyter kernel writing its \
         connection file to {CONNECTION_FILE}, on the Python and with the environment \
         variables the project needs, for example:\n\n\
         ```yaml\n\
         {TASK}:\n\
         \x20 desc: The Jupyter kernel the IDE's notebooks run in\n\
         \x20 cmds:\n\
         \x20   - .venv/bin/python -m ipykernel_launcher -f {CONNECTION_FILE}\n\
         ```\n\n\
         The kernel runs from the Taskfile's directory. The IDE starts the task when a \
         notebook asks for its kernel, connects to the kernel through that file, and shows the \
         kernel's state on the task's row in Tasks; restarting the kernel restarts the task. \
         The IDE's client also needs jupyter_client (ipykernel brings it) in the first of \
         .venv/bin/python, venv/bin/python, python3, and python."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_convention_names_the_task_and_the_file() {
        let text = convention();
        assert!(text.contains("`jupyter:kernel`"));
        assert!(text.contains("-f /tmp/taste-ide-kernel.json"));
        assert!(text.contains("\n  cmds:\n    - .venv"), "{text}");
    }
}
