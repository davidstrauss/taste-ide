//! The gcloud CLI: the IDE's own copy, signed in per project.
//!
//! The IDE talks to GCP the way the tools around it do — through gcloud's
//! sign-in, and for the model's VM through gcloud's IAP tunnel — with this
//! project's rules applied (ENVIRONMENTS → "gcloud, the IDE's own, signed
//! in per project"):
//!
//! - **Not installed on the host.** Google's Linux archive is
//!   self-contained, with its own Python, so it is fetched once, pinned by
//!   version and digest ([`SDK`]), into the IDE's data directory, and run
//!   from there, inside the IDE's own sandbox. Nothing is layered onto the
//!   base system (David, 2026-10-03: "don't make me install it to the base
//!   system").
//! - **This project's credentials and no one else's.** Every invocation
//!   has `CLOUDSDK_CONFIG` set to the project's own directory and loses
//!   `GOOGLE_APPLICATION_CREDENTIALS` and every inherited `CLOUDSDK_*`, so
//!   a machine-wide sign-in, if the user has one, is never read.
//! - **Least privilege.** The IDE's own calls impersonate the keyless
//!   `taste-ide` service account ([`crate::setup::service_account`]),
//!   which holds only the custom role. The user's sign-in is used for that
//!   and for the setup, which runs as the user.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::rest::AccessToken;

/// One pinned release of the gcloud archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sdk {
    pub version: &'static str,
    pub url: &'static str,
    /// Hex SHA-256 of the archive, computed from a real download.
    pub sha256: &'static str,
    pub bytes: u64,
}

/// Measured 2026-10-03: the archive runs in place with its bundled Python
/// 3.14.7, offers `compute start-iap-tunnel` and impersonation through the
/// environment, and with `CLOUDSDK_CONFIG` set writes nothing to
/// `~/.config/gcloud`. Extracted, it is about 490 MB.
pub const SDK: Sdk = Sdk {
    version: "587.0.0",
    url: "https://dl.google.com/dl/cloudsdk/channels/rapid/downloads/google-cloud-cli-587.0.0-linux-x86_64.tar.gz",
    sha256: "57df2448d259c654796a3703af8e5b53a02d439715b2034d6bb811efc2d6dd7b",
    bytes: 88_004_933,
};

/// `$XDG_DATA_HOME/taste-ide/gcloud`, or `~/.local/share/taste-ide/gcloud`.
pub fn sdk_root() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("taste-ide").join("gcloud")
}

/// Where a release is unpacked: one directory per version, so moving the
/// pin never mixes two releases' files.
pub fn sdk_dir(root: &Path, sdk: &Sdk) -> PathBuf {
    root.join(sdk.version)
}

/// The `gcloud` launcher inside an unpacked release.
pub fn binary(root: &Path, sdk: &Sdk) -> PathBuf {
    sdk_dir(root, sdk).join("google-cloud-sdk/bin/gcloud")
}

/// The pinned release, fetched and unpacked if it is not there yet.
/// `progress` sees the download's bytes; unpacking follows it and is not
/// reported, since it is seconds.
pub async fn ensure_installed(
    root: &Path,
    progress: impl Fn(u64, u64) + Send + Sync + 'static,
) -> Result<PathBuf> {
    let launcher = binary(root, &SDK);
    if launcher.exists() {
        return Ok(launcher);
    }
    let archive = root.join(format!("google-cloud-cli-{}.tar.gz", SDK.version));
    taste_models::fetch_pinned(
        "the gcloud CLI",
        SDK.url,
        SDK.sha256,
        Some(SDK.bytes),
        &archive,
        progress,
    )
    .await?;
    let target = sdk_dir(root, &SDK);
    let archive_for_unpack = archive.clone();
    tokio::task::spawn_blocking(move || unpack(&archive_for_unpack, &target))
        .await
        .context("unpacking the gcloud CLI")??;
    // The archive has done its job; the digest was checked on arrival.
    let _ = tokio::fs::remove_file(&archive).await;
    if !launcher.exists() {
        bail!("the gcloud archive held no {}", launcher.display());
    }
    Ok(launcher)
}

/// Unpack `archive` into `target`, through a sibling directory renamed into
/// place at the end, so an interrupted unpack never looks installed.
fn unpack(archive: &Path, target: &Path) -> Result<()> {
    let name = target
        .file_name()
        .context("an unpack target with no name")?
        .to_string_lossy();
    let staging = target.with_file_name(format!("{name}.unpacking"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    tar.set_preserve_permissions(true);
    // `unpack` refuses entries that would land outside `staging`.
    tar.unpack(&staging)
        .with_context(|| format!("unpacking {}", archive.display()))?;
    let _ = std::fs::remove_dir_all(target);
    std::fs::rename(&staging, target)?;
    Ok(())
}

/// Environment variables that would carry another sign-in into gcloud.
/// `CLOUDSDK_*` beyond these is removed by prefix, since any of them can
/// name a property.
const FOREIGN_CREDENTIALS: &[&str] = &["GOOGLE_APPLICATION_CREDENTIALS", "CLOUDSDK_CONFIG"];

/// gcloud as one project uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gcloud {
    pub binary: PathBuf,
    /// The project's own gcloud configuration: `gcloud/` in the
    /// workspace's state directory.
    pub config_dir: PathBuf,
    pub project: String,
    /// The service account the IDE's own calls run as, when set.
    pub impersonate: Option<String>,
}

impl Gcloud {
    /// A `gcloud` invocation with this project's configuration and nothing
    /// inherited that could name another. `as_user` leaves impersonation
    /// off, for the sign-in and the setup, which are the user's.
    fn invocation(&self, args: &[&str], as_user: bool) -> std::process::Command {
        let mut command = std::process::Command::new(&self.binary);
        command.args(args);
        isolate(&mut command, &self.config_dir);
        command.env("CLOUDSDK_CORE_PROJECT", &self.project);
        if let (false, Some(account)) = (as_user, &self.impersonate) {
            command.env("CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT", account);
        }
        command
    }

    /// `gcloud auth login`, into this project's configuration: the user's
    /// browser sign-in, which is the one credential this route keeps.
    pub fn sign_in(&self) -> Command {
        Command::from(self.invocation(&["auth", "login", "--brief"], true))
    }

    /// The account signed in to this project's configuration, if any.
    pub async fn account(&self) -> Result<Option<String>> {
        let output = Command::from(self.invocation(&["config", "get-value", "account"], true))
            .stdin(Stdio::null())
            .output()
            .await
            .context("running gcloud")?;
        let account = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((output.status.success() && !account.is_empty()).then_some(account))
    }

    /// Whether the service account this gcloud impersonates exists, asked
    /// as the user — so a project that has not been set up yet is told
    /// that, rather than handed impersonation's failure. `None` when the
    /// answer cannot be had (the user may not read IAM, or nothing is
    /// impersonated), which the caller treats as "try it and see".
    pub async fn service_account_exists(&self) -> Result<Option<bool>> {
        let Some(account) = &self.impersonate else {
            return Ok(None);
        };
        let output = Command::from(self.invocation(
            &[
                "iam",
                "service-accounts",
                "describe",
                account,
                "--format=value(email)",
            ],
            true,
        ))
        .stdin(Stdio::null())
        .output()
        .await
        .context("running gcloud")?;
        if output.status.success() {
            return Ok(Some(true));
        }
        let said = String::from_utf8_lossy(&output.stderr);
        Ok((said.contains("NOT_FOUND") || said.contains("does not exist")).then_some(false))
    }

    /// An access token as the impersonated service account, for the IDE's
    /// REST calls. gcloud does not say when it lapses; it renews any token
    /// with less than a few minutes left, and its tokens last an hour, so
    /// one is used for [`TOKEN_REUSE`] and then asked for again.
    pub async fn access_token(&self) -> Result<AccessToken> {
        let asked = SystemTime::now();
        let output =
            Command::from(self.invocation(&["auth", "print-access-token", "--quiet"], false))
                .stdin(Stdio::null())
                .output()
                .await
                .context("running gcloud")?;
        if !output.status.success() {
            bail!(
                "gcloud gave no token: {}",
                gcloud_error(&String::from_utf8_lossy(&output.stderr))
            );
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() || token.contains(char::is_whitespace) {
            bail!("gcloud's token was not a token");
        }
        Ok(AccessToken {
            token,
            expires_at: asked + TOKEN_REUSE + crate::rest::EXPIRY_MARGIN,
        })
    }

    /// `gcloud compute start-iap-tunnel` to `instance`'s `port`, listening
    /// on loopback at `local_port`, as the impersonated service account.
    /// The caller keeps the child running for as long as the tunnel is
    /// wanted.
    pub fn tunnel(&self, instance: &str, zone: &str, port: u16, local_port: u16) -> Command {
        let port = port.to_string();
        let local = format!("--local-host-port=127.0.0.1:{local_port}");
        let zone = format!("--zone={zone}");
        let mut command = Command::from(self.invocation(
            &[
                "compute",
                "start-iap-tunnel",
                instance,
                &port,
                &local,
                &zone,
            ],
            false,
        ));
        command.stdin(Stdio::null()).kill_on_drop(true);
        command
    }

    /// The setup script ([`crate::setup::SCRIPT`]) run with this gcloud,
    /// as the user: the same file a person runs by hand or in Cloud Shell.
    pub fn setup(&self, remove: bool) -> Command {
        let mut command = std::process::Command::new("bash");
        command
            .arg("-c")
            .arg(crate::setup::SCRIPT)
            .arg("gcp-setup.sh");
        if remove {
            command.arg("--remove");
        }
        command.arg(&self.project);
        isolate(&mut command, &self.config_dir);
        let bin = self.binary.parent().unwrap_or(Path::new("."));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin.to_path_buf()];
        paths.extend(std::env::split_paths(&path));
        command.env("PATH", std::env::join_paths(paths).unwrap_or(path));
        Command::from(command)
    }
}

impl Gcloud {
    /// `gcloud args…` as one argv for a console tab, which can add to the
    /// environment it inherits but not take from it: `env -u` strips
    /// every other sign-in first, and the project's own configuration is
    /// set after, so the tab is as isolated as [`Self::invocation`].
    pub fn terminal_argv(&self, args: &[&str], as_user: bool) -> (String, Vec<String>) {
        let mut argv: Vec<String> = Vec::new();
        let mut strip = |name: String| {
            argv.push("-u".into());
            argv.push(name);
        };
        for name in FOREIGN_CREDENTIALS {
            strip(name.to_string());
        }
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy().into_owned();
            if name.starts_with("CLOUDSDK_") && !FOREIGN_CREDENTIALS.contains(&name.as_str()) {
                strip(name);
            }
        }
        argv.push(format!("CLOUDSDK_CONFIG={}", self.config_dir.display()));
        argv.push("CLOUDSDK_CORE_DISABLE_USAGE_REPORTING=true".into());
        argv.push(format!("CLOUDSDK_CORE_PROJECT={}", self.project));
        if let (false, Some(account)) = (as_user, &self.impersonate) {
            argv.push(format!(
                "CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT={account}"
            ));
        }
        argv.push(self.binary.display().to_string());
        argv.extend(args.iter().map(|a| a.to_string()));
        ("env".into(), argv)
    }

    /// The setup script as one argv for a console tab, run as the user
    /// with this gcloud first on `PATH` — [`Self::setup`] for a tab.
    pub fn setup_terminal_argv(&self, remove: bool) -> (String, Vec<String>) {
        let (program, mut argv) = self.terminal_argv(&[], true);
        argv.pop(); // the gcloud binary; bash runs instead, with it on PATH
        let bin = self.binary.parent().unwrap_or(Path::new("."));
        let path = std::env::var("PATH").unwrap_or_default();
        argv.push(format!("PATH={}:{path}", bin.display()));
        argv.extend(["bash".into(), "-c".into(), crate::setup::SCRIPT.into()]);
        argv.push("gcp-setup.sh".into());
        if remove {
            argv.push("--remove".into());
        }
        argv.push(self.project.clone());
        (program, argv)
    }
}

/// How long a token from gcloud is used before asking again.
pub const TOKEN_REUSE: Duration = Duration::from_secs(40 * 60);

/// Strip every inherited way of naming another sign-in, and point gcloud
/// at `config_dir`.
fn isolate(command: &mut std::process::Command, config_dir: &Path) {
    for name in FOREIGN_CREDENTIALS {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("CLOUDSDK_") {
            command.env_remove(name);
        }
    }
    command
        .env("CLOUDSDK_CONFIG", config_dir)
        .env("CLOUDSDK_CORE_DISABLE_PROMPTS", "1")
        .env("CLOUDSDK_CORE_DISABLE_USAGE_REPORTING", "true");
}

/// The first sentence of what gcloud said, without its `ERROR: (gcloud.…)`
/// prefix: what fits on a status line. The whole of it belongs in a
/// tooltip.
pub fn first_sentence(said: &str) -> String {
    let said = said.trim();
    let said = said.strip_prefix("gcloud gave no token: ").unwrap_or(said);
    let said = said.strip_prefix("ERROR: ").unwrap_or(said);
    let said = match said.strip_prefix('(') {
        Some(rest) => rest.split_once(") ").map_or(said, |(_, rest)| rest),
        None => said,
    };
    match said.find(". ") {
        Some(end) => said[..=end].to_string(),
        None => said.to_string(),
    }
}

/// gcloud's own explanation: from its `ERROR:` line to the end, which is
/// often several lines of one sentence, joined back into one; the last
/// line otherwise.
fn gcloud_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match lines.iter().position(|l| l.starts_with("ERROR:")) {
        Some(at) => lines[at..].join(" "),
        None => lines.last().copied().unwrap_or("").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn gcloud(dir: &Path) -> Gcloud {
        Gcloud {
            binary: dir.join("gcloud"),
            config_dir: dir.join("config"),
            project: "my-project-1".into(),
            impersonate: Some("taste-ide@my-project-1.iam.gserviceaccount.com".into()),
        }
    }

    fn env_of<'a>(command: &'a std::process::Command, name: &str) -> Option<Option<&'a OsStr>> {
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .map(|(_, value)| value)
    }

    #[test]
    fn every_call_has_the_projects_config_and_no_other_sign_in() {
        let dir = Path::new("/state");
        let command = gcloud(dir).invocation(&["version"], false);
        assert_eq!(
            env_of(&command, "CLOUDSDK_CONFIG"),
            Some(Some(OsStr::new("/state/config")))
        );
        assert_eq!(
            env_of(&command, "GOOGLE_APPLICATION_CREDENTIALS"),
            Some(None)
        );
        assert_eq!(
            env_of(&command, "CLOUDSDK_CORE_PROJECT"),
            Some(Some(OsStr::new("my-project-1")))
        );
        assert_eq!(
            env_of(&command, "CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT"),
            Some(Some(OsStr::new(
                "taste-ide@my-project-1.iam.gserviceaccount.com"
            )))
        );
    }

    #[test]
    fn the_users_own_steps_do_not_impersonate() {
        let command = gcloud(Path::new("/state")).invocation(&["auth", "login"], true);
        assert_eq!(
            env_of(&command, "CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT"),
            None
        );
    }

    #[test]
    fn the_tunnel_listens_on_loopback_only() {
        let command = gcloud(Path::new("/state")).tunnel(
            "taste-0a1b2c3d-serve",
            "us-central1-a",
            8080,
            41234,
        );
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "compute",
                "start-iap-tunnel",
                "taste-0a1b2c3d-serve",
                "8080",
                "--local-host-port=127.0.0.1:41234",
                "--zone=us-central1-a",
            ]
        );
    }

    #[test]
    fn a_console_tab_is_isolated_the_same_way() {
        let (program, argv) =
            gcloud(Path::new("/state")).terminal_argv(&["auth", "login", "--brief"], true);
        assert_eq!(program, "env");
        let at = |s: &str| argv.iter().position(|a| a == s);
        assert!(at("GOOGLE_APPLICATION_CREDENTIALS").is_some_and(|i| argv[i - 1] == "-u"));
        assert!(argv.contains(&"CLOUDSDK_CONFIG=/state/config".to_string()));
        assert!(!argv
            .iter()
            .any(|a| a.starts_with("CLOUDSDK_AUTH_IMPERSONATE")));
        assert_eq!(
            &argv[argv.len() - 4..],
            ["/state/gcloud", "auth", "login", "--brief"]
        );
        // Every assignment comes after every removal, so none is undone.
        let last_strip = argv.iter().rposition(|a| a == "-u").unwrap();
        let first_set = argv.iter().position(|a| a.contains('=')).unwrap();
        assert!(last_strip < first_set);
    }

    #[test]
    fn the_setup_tab_runs_the_script_with_this_gcloud_first() {
        let (program, argv) = gcloud(Path::new("/state")).setup_terminal_argv(false);
        assert_eq!(program, "env");
        let path = argv.iter().find(|a| a.starts_with("PATH=")).unwrap();
        assert!(path.starts_with("PATH=/state:"), "{path}");
        let bash = argv.iter().position(|a| a == "bash").unwrap();
        assert_eq!(argv[bash + 1], "-c");
        assert_eq!(argv[bash + 2], crate::setup::SCRIPT);
        assert_eq!(&argv[bash + 3..], ["gcp-setup.sh", "my-project-1"]);
        assert!(!argv.contains(&"/state/gcloud".to_string()));
    }

    #[test]
    fn the_setup_runs_the_checked_in_script_with_this_gcloud_first() {
        let command = gcloud(Path::new("/state")).setup(true);
        let command = command.as_std();
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], crate::setup::SCRIPT);
        assert_eq!(&args[2..], ["gcp-setup.sh", "--remove", "my-project-1"]);
        let path = env_of(command, "PATH").flatten().unwrap().to_string_lossy();
        assert!(path.starts_with("/state:"), "{path}");
        assert_eq!(
            env_of(command, "CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT"),
            None
        );
    }

    /// A stand-in gcloud that records its arguments and the variables that
    /// matter, and prints a token.
    fn stub(dir: &Path, token: &str) -> PathBuf {
        let path = dir.join("gcloud");
        crate::testing::install_stub(
            &path,
            &format!(
                "#!/usr/bin/env bash\n\
                 echo \"$* | config=$CLOUDSDK_CONFIG as=${{CLOUDSDK_AUTH_IMPERSONATE_SERVICE_ACCOUNT:-me}} gac=${{GOOGLE_APPLICATION_CREDENTIALS:-none}}\" >> \"{log}\"\n\
                 case \"$*\" in\n\
                   \"auth print-access-token\"*) echo '{token}' ;;\n\
                   \"config get-value account\") echo 'david@example.com' ;;\n\
                 esac\n",
                log = dir.join("calls").display(),
            ),
        );
        path
    }

    #[tokio::test]
    async fn a_token_comes_from_gcloud_as_the_service_account() {
        let dir = tempfile::tempdir().unwrap();
        stub(dir.path(), "ya29.stub");
        let gcloud = gcloud(dir.path());
        let before = SystemTime::now();
        let token = gcloud.access_token().await.unwrap();
        assert_eq!(token.token, "ya29.stub");
        assert!(token.expires_at >= before + TOKEN_REUSE);
        assert_eq!(
            gcloud.account().await.unwrap().as_deref(),
            Some("david@example.com")
        );

        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        let lines: Vec<_> = calls.lines().collect();
        assert!(lines[0].starts_with("auth print-access-token"));
        assert!(lines[0].contains("as=taste-ide@my-project-1.iam.gserviceaccount.com"));
        assert!(lines[0].contains("gac=none"));
        assert!(lines[0].contains(&format!("config={}", dir.path().join("config").display())));
        assert!(lines[1].contains("as=me"), "{}", lines[1]);
    }

    #[tokio::test]
    async fn no_token_is_a_failure_in_gclouds_words() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gcloud");
        crate::testing::install_stub(
            &path,
            "#!/usr/bin/env bash\necho 'ERROR: (gcloud.auth.print-access-token) You do not currently have an active account selected.' >&2\nexit 1\n",
        );
        let error = gcloud(dir.path())
            .access_token()
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("do not currently have an active account"),
            "{error}"
        );
    }

    #[test]
    fn a_status_line_gets_the_first_sentence_without_the_prefix() {
        assert_eq!(
            first_sentence(
                "gcloud gave no token: ERROR: (gcloud.auth.print-access-token) NOT_FOUND: Failed to impersonate [a@b]. Make sure the account that's trying to impersonate it has access."
            ),
            "NOT_FOUND: Failed to impersonate [a@b]."
        );
        assert_eq!(first_sentence("plain words"), "plain words");
    }

    #[tokio::test]
    async fn a_missing_account_is_told_apart_from_an_unknowable_one() {
        async fn answer(body: &str) -> Option<bool> {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("gcloud");
            crate::testing::install_stub(&path, &format!("#!/usr/bin/env bash\n{body}\n"));
            gcloud(dir.path()).service_account_exists().await.unwrap()
        }
        assert_eq!(
            answer("echo taste-ide@my-project-1.iam.gserviceaccount.com").await,
            Some(true)
        );
        assert_eq!(
            answer("echo 'ERROR: (gcloud.iam.service-accounts.describe) NOT_FOUND: Unknown service account' >&2; exit 1").await,
            Some(false)
        );
        assert_eq!(
            answer("echo 'ERROR: PERMISSION_DENIED: Permission iam.serviceAccounts.get denied' >&2; exit 1").await,
            None
        );
    }

    #[test]
    fn a_multi_line_error_is_kept_whole() {
        let said = gcloud_error(
            "WARNING: something first\nERROR: (gcloud.auth.print-access-token) You do not currently have an active account selected.\nPlease run:\n\n  $ gcloud auth login\n\nto obtain new credentials.\n",
        );
        assert_eq!(
            said,
            "ERROR: (gcloud.auth.print-access-token) You do not currently have an active account selected. Please run: $ gcloud auth login to obtain new credentials."
        );
    }

    #[test]
    fn an_unpack_lands_whole_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::fast(),
            ));
            let mut header = tar::Header::new_gnu();
            let body = b"#!/bin/sh\n";
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "google-cloud-sdk/bin/gcloud", &body[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let target = dir.path().join("587.0.0");
        unpack(&archive, &target).unwrap();
        let launcher = target.join("google-cloud-sdk/bin/gcloud");
        assert!(launcher.exists());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            launcher.metadata().unwrap().permissions().mode() & 0o111,
            0o111
        );
        assert!(!dir.path().join("587.0.0.unpacking").exists());
    }

    #[test]
    fn the_pin_is_well_formed() {
        assert_eq!(SDK.sha256.len(), 64);
        assert!(SDK.url.contains(SDK.version));
        assert!(binary(Path::new("/r"), &SDK).ends_with("587.0.0/google-cloud-sdk/bin/gcloud"));
    }
}
