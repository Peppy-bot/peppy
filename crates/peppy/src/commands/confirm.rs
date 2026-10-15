use std::io::{self, BufRead, Write};

use core_node_api::InstanceState;

use crate::error::{Error, Result};

/// Prompts the user with a message and waits for y/yes confirmation.
/// If `reader` is provided, reads from it; otherwise reads from stdin.
pub(crate) fn confirm_prompt(message: &str, reader: Option<&mut dyn BufRead>) -> Result<bool> {
    print!("{message}");
    io::stdout()
        .flush()
        .map_err(|e| Error::ExecutionFailed(format!("Failed to write confirmation prompt: {e}")))?;

    let mut input = String::new();
    if let Some(reader) = reader {
        reader.read_line(&mut input)
    } else {
        io::stdin().read_line(&mut input)
    }
    .map_err(|e| Error::ExecutionFailed(format!("Failed to read confirmation response: {e}")))?;

    let response = input.trim().to_ascii_lowercase();
    Ok(matches!(response.as_str(), "y" | "yes"))
}

/// Whether an instance has a process or awaits a restart, so that stopping
/// its node changes what runs; an exited instance is cleared with nothing to
/// stop.
pub(crate) fn has_or_awaits_process(state: InstanceState) -> bool {
    state.has_process() || state == InstanceState::Restarting
}

/// Instances a confirmation question names, each with its state.
pub(crate) struct TrackedInstances<'a>(pub(crate) &'a [(String, InstanceState)]);

impl TrackedInstances<'_> {
    /// `it` for one instance, `them` for more.
    pub(crate) fn pronoun(&self) -> &'static str {
        if self.0.len() == 1 { "it" } else { "them" }
    }

    /// `is` for one instance, `are` for more.
    pub(crate) fn verb(&self) -> &'static str {
        if self.0.len() == 1 { "is" } else { "are" }
    }

    /// Whether one of them may be starting when the command reaches the
    /// daemon: one starting or restarting now, or running, which may exit and
    /// restart under its policy meanwhile.
    pub(crate) fn may_start(&self) -> bool {
        self.0
            .iter()
            .any(|(_, state)| has_or_awaits_process(*state))
    }
}

impl std::fmt::Display for TrackedInstances<'_> {
    /// `2 instances ("cam_a" running, "cam_b" restarting)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.0.len();
        let suffix = if count == 1 { "instance" } else { "instances" };
        let listed = self
            .0
            .iter()
            .map(|(id, state)| format!("\"{id}\" {state}"))
            .collect::<Vec<_>>()
            .join(", ");
        write!(f, "{count} {suffix} ({listed})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn answer(input: &str) -> bool {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        confirm_prompt("Proceed? ", Some(&mut reader)).expect("reading from a cursor never fails")
    }

    #[test]
    fn affirmative_answers_confirm() {
        assert!(answer("y\n"));
        assert!(answer("yes\n"));
        assert!(answer("Y\n"), "matching is case-insensitive");
        assert!(answer("YES\n"));
        assert!(answer("  yes  \n"), "surrounding whitespace is trimmed");
    }

    #[test]
    fn anything_else_declines() {
        assert!(!answer("n\n"));
        assert!(!answer("no\n"));
        assert!(!answer("\n"), "a bare newline declines");
        assert!(!answer(""), "EOF declines");
        assert!(!answer("yep\n"), "only y/yes count as yes");
    }

    #[test]
    fn tracked_instances_are_counted_and_named_with_their_states() {
        let one = [("a".to_owned(), InstanceState::Failed)];
        assert_eq!(
            TrackedInstances(&one).to_string(),
            "1 instance (\"a\" failed)"
        );
        assert_eq!(TrackedInstances(&one).pronoun(), "it");
        assert_eq!(TrackedInstances(&one).verb(), "is");
        let two = [
            ("a".to_owned(), InstanceState::Running),
            ("b".to_owned(), InstanceState::Restarting),
        ];
        assert_eq!(
            TrackedInstances(&two).to_string(),
            "2 instances (\"a\" running, \"b\" restarting)"
        );
        assert_eq!(TrackedInstances(&two).pronoun(), "them");
        assert_eq!(TrackedInstances(&two).verb(), "are");
    }

    /// A starting, running or restarting instance has or awaits a process; a
    /// finished or failed one has neither.
    #[test]
    fn an_exited_instance_neither_has_nor_awaits_a_process() {
        for state in [
            InstanceState::Starting,
            InstanceState::Running,
            InstanceState::Restarting,
        ] {
            assert!(has_or_awaits_process(state), "{state}");
        }
        for state in [InstanceState::Finished, InstanceState::Failed] {
            assert!(!has_or_awaits_process(state), "{state}");
        }
    }
}
