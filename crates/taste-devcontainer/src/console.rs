//! **A guest's serial console, as lines a person reads.**
//!
//! systemd says every unit event twice on the console. Once through the
//! kernel's log — `[    5.685115] systemd[1]: Listening on
//! systemd-udevd-control.socket - udev Control Socket.`, the whole text —
//! and once as its own status line, `[  OK  ] Listening on
//! systemd-udevd-control.socket - udev Control Socket.`, which carries the
//! verdict and its colour but is cut to the console's eighty columns with
//! an ellipsis. Shown as they arrive, that is every event in two lines, one
//! of them cut short (David, 2026-09-22: "Can we format this better?").
//!
//! [`ConsoleFold`] makes each pair one line: the status line's verdict, in
//! its colour, and the kernel line's full text, the unit in bold as the
//! status line had it. A kernel line with no status line after it — a unit
//! skipped on a condition — stays as it was, timestamp and all. And a line
//! the two writers interleaved, `… - Tempo[    5.716720] systemd[1]: …`,
//! is split where the kernel's timestamp starts, since that is where the
//! second writer came in.
//!
//! It folds in the stream rather than in a view, so the VM log the panes
//! show and the one the agents' log tool returns are the same, and both
//! are half the lines.

/// The SGR systemd opens a unit name with on the console.
const BOLD: &str = "\u{1b}[0;1;39m";
const RESET: &str = "\u{1b}[0m";

/// Folds a console's lines as they arrive; see the module docs.
#[derive(Debug, Default)]
pub struct ConsoleFold {
    /// A kernel `systemd[1]:` line waiting for the status line that may
    /// follow it: the line as it came, and its text after `systemd[1]: `.
    pending: Option<(String, String)>,
    /// The full texts of the last few folds, for the remainder of a status
    /// line the kernel interrupted: `rary Directory /tmp...`, arriving on
    /// a line of its own after the fold already said all of it.
    recent: std::collections::VecDeque<String>,
}

impl ConsoleFold {
    /// One line off the console: the lines to show for it, which may be
    /// none (a kernel line held for its status line) or two (the held line
    /// and this one, when this one is not its pair).
    pub fn push(&mut self, line: &str) -> Vec<String> {
        let mut out = Vec::new();
        for part in split_at_timestamps(line) {
            self.push_one(part, &mut out);
        }
        out
    }

    /// The held line, if any: said when the console has gone quiet, so a
    /// kernel line with nothing after it is not held for ever.
    pub fn flush(&mut self) -> Option<String> {
        self.pending.take().map(|(raw, _)| raw)
    }

    fn push_one(&mut self, line: &str, out: &mut Vec<String>) {
        if let Some((raw, full)) = self.pending.take() {
            if let Some(folded) = fold(line, &full) {
                out.push(folded);
                if self.recent.len() == 4 {
                    self.recent.pop_front();
                }
                self.recent.push_back(full);
                return;
            }
            out.push(raw);
        }
        if self.is_remainder(line) {
            return;
        }
        match kernel_systemd_text(line) {
            Some(text) => self.pending = Some((line.to_string(), text.to_string())),
            None => out.push(line.to_string()),
        }
    }
}

impl ConsoleFold {
    /// Whether `line` is only the rest of a status line whose event a
    /// recent fold already said whole: no verdict, no indent, no
    /// timestamp, and every piece of it (around systemd's `…`) inside that
    /// fold's text.
    fn is_remainder(&self, line: &str) -> bool {
        if line.starts_with('[') || line.starts_with("    ") {
            return false;
        }
        let plain = strip_sgr(line);
        let pieces: Vec<&str> = plain
            .split('…')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        !pieces.is_empty()
            && self
                .recent
                .iter()
                .any(|full| pieces.iter().all(|piece| full.contains(piece)))
    }
}

/// The line split wherever a kernel timestamp — `[` spaces, digits, a
/// dot, digits, `]` — starts somewhere other than its beginning.
fn split_at_timestamps(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (at, _) in line.match_indices('[') {
        if at > start && timestamp_len(&line[at..]).is_some() {
            if !line[start..at].trim().is_empty() {
                parts.push(&line[start..at]);
            }
            start = at;
        }
    }
    parts.push(&line[start..]);
    parts
}

/// The length of the kernel timestamp `text` starts with, if it does.
fn timestamp_len(text: &str) -> Option<usize> {
    let rest = text.strip_prefix('[')?;
    let body = rest.trim_start_matches(' ');
    let (secs, after) = body.split_once('.')?;
    if secs.is_empty() || !secs.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac = after.bytes().take_while(u8::is_ascii_digit).count();
    if frac == 0 || !after[frac..].starts_with(']') {
        return None;
    }
    Some(text.len() - after.len() + frac + 1)
}

/// `Listening on x.socket - X.` of `[    5.68] systemd[1]: Listening on
/// x.socket - X.`; `None` for anything else.
fn kernel_systemd_text(line: &str) -> Option<&str> {
    let len = timestamp_len(line)?;
    line[len..].trim_start().strip_prefix("systemd[1]: ")
}

/// The status line `line` as the fold of the kernel text `full` it
/// follows, when it is that event's: the verdict as the status line wrote
/// it, and the whole text.
fn fold(line: &str, full: &str) -> Option<String> {
    let (marker, text) = status_parts(line)?;
    let plain = strip_sgr(text);
    let plain = plain.trim_end();
    let same = match plain.split_once('…') {
        Some((head, tail)) => {
            full.starts_with(head) && full.ends_with(tail) && head.len() + tail.len() <= full.len()
        }
        // Whole, or cut short where the kernel's line came in.
        None => !plain.is_empty() && full.starts_with(plain),
    };
    if !same {
        return None;
    }
    let body = if text.contains(BOLD) {
        embolden_unit(full)
    } else {
        full.to_string()
    };
    Some(format!("{marker}{body}"))
}

/// A status line's verdict — `[  OK  ] `, with its colour, or the
/// indent systemd gives `Starting …` — and the text after it.
fn status_parts(line: &str) -> Option<(&str, &str)> {
    if line.starts_with('[') {
        let close = line.find("] ")?;
        let verdict = strip_sgr(&line[1..close]);
        let verdict = verdict.trim();
        let known = ["OK", "FAILED", "DEPEND", "TIME", "SKIP", "WARN", "INFO"];
        if !known.contains(&verdict) && !verdict.chars().all(|c| c == '*' || c == ' ') {
            return None;
        }
        return Some((&line[..close + 2], &line[close + 2..]));
    }
    let indent = line.len() - line.trim_start_matches(' ').len();
    (indent >= 4).then(|| (&line[..indent], &line[indent..]))
}

/// `Listening on <bold>x.socket</bold> - X.`: the word after the verb, as
/// systemd's status line sets it.
fn embolden_unit(full: &str) -> String {
    // The verb is one or two words ("Listening on", "Reached target");
    // the unit is the first word with a dot or an @ in it.
    let mut out = String::with_capacity(full.len() + 16);
    let mut done = false;
    for (i, word) in full.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if !done && (word.contains('.') || word.contains('@')) && !word.ends_with('.') {
            out.push_str(BOLD);
            out.push_str(word);
            out.push_str(RESET);
            done = true;
        } else {
            out.push_str(word);
        }
    }
    out
}

fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = "[\u{1b}[0;32m  OK  \u{1b}[0m] ";

    fn run(lines: &[&str]) -> Vec<String> {
        let mut fold = ConsoleFold::default();
        let mut out: Vec<String> = lines.iter().flat_map(|l| fold.push(l)).collect();
        out.extend(fold.flush());
        out
    }

    #[test]
    fn a_kernel_line_and_its_status_line_are_one_line() {
        let out = run(&[
            "[    5.685115] systemd[1]: Listening on systemd-udevd-control.socket - udev Control Socket.",
            &format!("{OK}Listening on {BOLD}systemd-udevd-control.socket{RESET} - udev Control Socket."),
        ]);
        assert_eq!(
            out,
            [format!(
                "{OK}Listening on {BOLD}systemd-udevd-control.socket{RESET} - udev Control Socket."
            )]
        );
    }

    /// The status line is cut to eighty columns; the fold has the whole.
    #[test]
    fn a_cut_status_line_takes_the_kernel_lines_whole_text() {
        let out = run(&[
            "[    2.2] systemd[1]: Starting coreos-touch-run-agetty.service - CoreOS: Touch /run/agetty.reload...",
            &format!("         Starting {BOLD}coreos-touch-run-agetty.s…{RESET}CoreOS: Touch /run/agetty.reload..."),
        ]);
        assert_eq!(
            out,
            [format!(
                "         Starting {BOLD}coreos-touch-run-agetty.service{RESET} - CoreOS: Touch \
                 /run/agetty.reload..."
            )]
        );
    }

    /// A unit skipped on a condition has no status line: it stays, and so
    /// does whatever came after it.
    #[test]
    fn an_unpaired_kernel_line_stays_as_it_was() {
        let skipped = "[    5.671086] systemd[1]: systemd-pcrextend.socket - TPM PCR Measurements skipped, unmet condition check ConditionSecurity=measured-uki";
        let next = "[    5.675157] systemd[1]: Listening on systemd-repart.socket - Disk Repartitioning Service Socket.";
        let status = format!(
            "{OK}Listening on {BOLD}systemd-repart.socket{RESET}…Disk Repartitioning Service Socket."
        );
        let out = run(&[skipped, next, &status]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], skipped);
        assert!(out[1]
            .ends_with("systemd-repart.socket\u{1b}[0m - Disk Repartitioning Service Socket."));
    }

    /// Two writers on one console: the kernel's line starts a line of its
    /// own, and the status line it interrupted still pairs with the kernel
    /// line before it.
    #[test]
    fn an_interleaved_line_is_split_where_the_timestamp_starts() {
        let out = run(&[
            "[    5.715018] systemd[1]: Mounting tmp.mount - Temporary Directory /tmp...",
            &format!(
                "         Mounting {BOLD}tmp.mount{RESET} - Tempo[    5.716720] systemd[1]: \
                 auth-rpcgss-module.service - Kernel Module supporting RPCSEC_GSS skipped"
            ),
        ]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].contains("Temporary Directory /tmp..."), "{out:?}");
        assert!(
            out[1].starts_with("[    5.716720] systemd[1]: auth-rpcgss"),
            "{out:?}"
        );
    }

    /// What the kernel cut off a status line arrives on a line of its
    /// own; the fold already said it, so it goes.
    #[test]
    fn the_rest_of_an_interrupted_status_line_goes() {
        let out = run(&[
            "[    5.715018] systemd[1]: Mounting tmp.mount - Temporary Directory /tmp...",
            &format!(
                "         Mounting {BOLD}tmp.mount{RESET} - Tempo[    5.716720] systemd[1]: \
                 auth-rpcgss-module.service - Kernel Module skipped"
            ),
            "rary Directory /tmp...",
            "Fedora CoreOS 44",
        ]);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[2], "Fedora CoreOS 44");
    }

    #[test]
    fn lines_that_are_neither_pass_through() {
        let out = run(&["Fedora CoreOS 44.20260829.3.1", "[taste-ide] sshd answers"]);
        assert_eq!(
            out,
            ["Fedora CoreOS 44.20260829.3.1", "[taste-ide] sshd answers"]
        );
        assert_eq!(timestamp_len("[    5.715018] x"), Some(14));
        assert_eq!(timestamp_len("[taste-ide] x"), None);
    }
}
