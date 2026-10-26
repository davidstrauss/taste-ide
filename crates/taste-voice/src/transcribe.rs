//! Local speech-to-text: whisper.cpp on the CPU, in a helper process
//! (`taste-transcribe`), so the model's memory and any crash of its native
//! code are the helper's and not the IDE's.
//!
//! Nothing leaves the machine. The auth proxy holds the one credential the
//! IDE has and Anthropic has no speech endpoint, so a cloud transcriber
//! would be a new credential, a new destination for the user's voice, and
//! a new hole in the line CLAUDE.md draws. Not worth what it buys.
//!
//! The helper is started ahead of need ([`Transcriber::prewarm`], when the
//! microphone is pressed), so the model loads while the user speaks, and
//! exits on its own when it has been idle a while; the next use starts
//! another. A helper that has died is started again once, and a second
//! failure is the error.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};

/// The speech model behind its helper. Cheap to make: nothing starts until
/// [`Self::prewarm`] or [`Self::transcribe`] asks.
pub struct Transcriber {
    model: PathBuf,
    helper: Mutex<Option<Helper>>,
}

impl Transcriber {
    pub fn new(model: &Path) -> Self {
        Self {
            model: model.to_path_buf(),
            helper: Mutex::new(None),
        }
    }

    /// Start the helper now, if none is running, without waiting for its
    /// model: the load overlaps whatever comes next. Blocking only for the
    /// spawn itself; callers keep it off the main thread all the same.
    pub fn prewarm(&self) -> Result<()> {
        let mut helper = self.helper.lock().unwrap_or_else(|e| e.into_inner());
        if helper.as_mut().is_some_and(Helper::alive) {
            return Ok(());
        }
        *helper = Some(Helper::spawn(&self.model)?);
        Ok(())
    }

    /// 16 kHz mono samples in, one line of text out. Runs to completion on
    /// the calling thread: callers put it on the blocking pool. One at a
    /// time; a second caller waits for the first.
    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        let mut helper = self.helper.lock().unwrap_or_else(|e| e.into_inner());
        if !helper.as_mut().is_some_and(Helper::alive) {
            *helper = Some(Helper::spawn(&self.model)?);
        }
        let first = match helper.as_mut().map(|h| h.ask(samples)) {
            Some(Ok(text)) => return Ok(text),
            Some(Err(e)) => e,
            None => anyhow::anyhow!("no speech helper"),
        };
        // It went away between the check and the ask — the idle exit, a
        // crash — or never came up: once more, from the start.
        let mut fresh = Helper::spawn(&self.model)
            .with_context(|| format!("restarting the speech helper after: {first:#}"))?;
        let text = fresh.ask(samples);
        *helper = Some(fresh);
        text
    }
}

/// Where the helper is: beside this executable (`target/debug` in
/// development, the install prefix's bin otherwise), or wherever
/// `TASTE_TRANSCRIBE_BIN` names.
pub fn helper_path() -> Result<PathBuf> {
    if let Some(named) = std::env::var_os("TASTE_TRANSCRIBE_BIN") {
        return Ok(PathBuf::from(named));
    }
    let exe = std::env::current_exe().context("finding this executable")?;
    exe.parent()
        .map(|dir| dir.join("taste-transcribe"))
        .filter(|path| path.is_file())
        .context(
            "taste-transcribe is not installed beside the IDE (TASTE_TRANSCRIBE_BIN names another)",
        )
}

struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// The model said it loaded: the first line, read at the first ask.
    ready: bool,
}

#[derive(serde::Deserialize)]
struct Reply {
    #[serde(default)]
    ready: bool,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

impl Helper {
    fn spawn(model: &Path) -> Result<Self> {
        let path = helper_path()?;
        let mut child = Command::new(&path)
            .arg(model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // whisper.cpp's narration; errors come back as JSON.
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", path.display()))?;
        let stdin = child.stdin.take().context("the helper has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("the helper has no stdout")?);
        Ok(Self {
            child,
            stdin,
            stdout,
            ready: false,
        })
    }

    /// Still running: not yet exited on its idle time, nor crashed.
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn ask(&mut self, samples: &[f32]) -> Result<String> {
        if !self.ready {
            let first = self
                .read_reply()
                .context("the speech helper did not say it was ready")?;
            if !first.ready {
                bail!(
                    "the speech model did not load: {}",
                    first.error.unwrap_or_else(|| "no reason given".into())
                );
            }
            self.ready = true;
        }
        let count = u32::try_from(samples.len()).context("too many samples for one request")?;
        let mut request = Vec::with_capacity(4 + samples.len() * 4);
        request.extend_from_slice(&count.to_le_bytes());
        for sample in samples {
            request.extend_from_slice(&sample.to_le_bytes());
        }
        self.stdin
            .write_all(&request)
            .context("writing to the speech helper")?;
        self.stdin
            .flush()
            .context("flushing to the speech helper")?;
        let reply = self.read_reply()?;
        if let Some(error) = reply.error {
            bail!("transcribing: {error}");
        }
        reply
            .text
            .context("the speech helper answered without any text")
    }

    fn read_reply(&mut self) -> Result<Reply> {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .context("reading from the speech helper")?;
        if n == 0 {
            bail!("the speech helper closed its output");
        }
        serde_json::from_str(&line).with_context(|| format!("the helper said: {}", line.trim()))
    }
}

impl Drop for Helper {
    /// The IDE's end of it: a helper outlives no transcriber.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the helper, in Python, speaking its protocol: it
    /// says it is ready, answers one request with how many samples came
    /// and their sum, and exits, as the real one does when idle. So every
    /// call here starts a helper again, which is the path an idle exit
    /// leaves.
    const STAND_IN: &str = r#"import sys, struct, json
print(json.dumps({"ready": True}), flush=True)
head = sys.stdin.buffer.read(4)
if len(head) == 4:
    (n,) = struct.unpack("<I", head)
    samples = struct.unpack("<%df" % n, sys.stdin.buffer.read(4 * n))
    print(json.dumps({"text": "%d samples, sum %g" % (n, sum(samples))}), flush=True)
"#;

    #[test]
    fn a_helper_that_exits_is_started_again_and_answers() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: no python3 on this machine");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("stand-in.py");
        std::fs::write(&script, STAND_IN).unwrap();
        let helper = dir.path().join("taste-transcribe");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nexec python3 {} \"$@\"\n", script.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("TASTE_TRANSCRIBE_BIN", &helper);

        let transcriber = Transcriber::new(Path::new("/nowhere/model.bin"));
        transcriber.prewarm().unwrap();
        assert_eq!(
            transcriber.transcribe(&[0.5, 0.25, 0.25]).unwrap(),
            "3 samples, sum 1"
        );
        // The stand-in has exited: the next call starts another.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            transcriber.transcribe(&[1.0, 1.0]).unwrap(),
            "2 samples, sum 2"
        );
    }
}
