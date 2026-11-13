//! The conventional file locations (ARCHITECTURE → Conventions), as data.
//!
//! Single source of truth for two consumers: the file tree's ghost rows
//! (a missing convention shows faintly, one activation from existing) and
//! the MCP `ide_conventions` tool (agents bootstrapping a project should
//! reach for these fixed places instead of inventing configuration).

use std::path::{Path, PathBuf};

/// The names `task` (taskfile.dev) looks for a Taskfile under, in its own
/// order; the first is the canonical one a new Taskfile gets.
pub const TASKFILE_NAMES: [&str; 8] = [
    "Taskfile.yml",
    "taskfile.yml",
    "Taskfile.yaml",
    "taskfile.yaml",
    "Taskfile.dist.yml",
    "taskfile.dist.yml",
    "Taskfile.dist.yaml",
    "taskfile.dist.yaml",
];

/// What keeps a devcontainer config working in VS Code and Codespaces,
/// which run it without this IDE, for an agent writing one. Each rule is a
/// failure a config written here met there:
///
/// - **Task in the image.** The IDE brings its own Task to every container
///   (`taste_core::tasks::IDE_TASK_VERSION`), and VS Code brings none, so a
///   config that relied on the IDE's had no `task` in VS Code — and VS
///   Code's Task extension, the one editor customization an agent adds
///   (David, 2026-11-13: "an exception to the general rule to not add or
///   remove VSCode extensions to the devcontainer config"), does nothing
///   without one. The image's own Task is the one the IDE runs first.
/// - **A built Compose service names its image under `localhost/`.**
///   VS Code on podman builds its user-mapping layer FROM `localhost/<the
///   image>` for any image name without a registry, and Compose through
///   the Docker API had tagged it `docker.io/library/<the image>`, so the
///   FROM found nothing (good_job, 2026-11-13). The IDE replaces a built
///   service's `image` with its own tag, so the name costs it nothing.
/// - **A Compose project works in the checkout as root, bound with
///   `:z`.** VS Code's CLI, on podman on Linux, gives a single container
///   `--security-opt label=disable` and, for a user other than root,
///   `--userns=keep-id`, and gives a Compose service neither: good_job's
///   checkout was unreadable to every user in the container under SELinux,
///   and its files root's, so the image's user could not write them
///   (2026-11-13). Both are said in standard syntax instead of podman's
///   (David, 2026-11-13: "avoid these Podman-isms"): root in a rootless
///   container IS the user, so its files are theirs, and `z` is Docker's
///   own bind option, which gives the files a label containers may use and
///   is ignored where there is no SELinux. The bind must be the repository
///   alone: `z` on the Codespaces convention's `../..` would relabel every
///   project beside it. `keep-id` machine-wide was tried and refused: the
///   official Postgres image could not make its own data directory under
///   it. The IDE forces `z` on every bind and maps the user onto root
///   itself, so neither changes anything here.
/// - **Lifecycle commands run twice.** A new container runs them again
///   while its named volumes keep their data: good_job's setup created its
///   test database a second time and failed (2026-11-13).
pub const VS_CODE: &str = "The config must also come up in VS Code and Codespaces, which run \
     it without this IDE and have none of what the IDE adds to a container: its Task, its \
     /home/agent, its user mapping. So:\n\
     - When the project has a Taskfile, or you are writing one, install Task (taskfile.dev) \
     in the image, pinned: on Fedora `dnf install go-task`, elsewhere the release tarball \
     from github.com/go-task/task with its sha256 checked. List VS Code's Task extension in \
     .devcontainer/devcontainer.json, beside any extensions already listed: \
     `\"customizations\": { \"vscode\": { \"extensions\": [\"task.vscode-task\"] } }`. \
     That is the one editor customization to add: add no other customizations.vscode entry \
     (extension or setting), and remove none that is there.\n\
     - In a Compose file, give every service that has `build:` an `image:` under localhost/, \
     named `localhost/<project>-<service>` (for example `image: localhost/shop-app`): VS Code \
     on podman looks for the built image there and fails to start without it. Write no \
     top-level `version:` key. So that VS Code on rootless podman can read and write the \
     checkout, the service you work in binds the repository alone, with `:z`: \
     `- ..:/workspaces/${localWorkspaceFolderBasename}:z`, never `../..`; and \
     .devcontainer/devcontainer.json sets `\"remoteUser\": \"root\"`, whatever user the \
     image names. Write no podman-only setting (`userns_mode`, `x-podman`).\n\
     - Lifecycle commands (postCreateCommand and the rest) run again in every new container \
     while named volumes keep their data, so each must succeed when run a second time: \
     create a database, user, or file only when it is missing. They may use only what the \
     image installs.";

/// Which task serves which forwarded port, for an agent: the task's own
/// `env`'s `PORT` in the Taskfile, which `taste-taskfile` reads. It is the
/// setting the server runs with (the twelve-factor `PORT`), and a file any
/// tool reads, VS Code's included — not a block of this IDE's own (David,
/// 2026-11-13), which is why `customizations.taste-ide.services`, where it
/// was said before, is no longer read.
pub const SERVICES: &str = "A task that runs a server sets the port it serves as PORT in its \
     own env in the Taskfile, and the server listens on that port (Rails, most Node \
     frameworks, and many Python servers read PORT themselves; otherwise pass $PORT on the \
     command line):\n\n\
     ```yaml\n\
     server:\n\
     \x20 desc: Run the app\n\
     \x20 env:\n\
     \x20   PORT: 3000\n\
     \x20 cmds:\n\
     \x20   - bin/rails server -b 0.0.0.0\n\
     ```\n\n\
     Write the number itself, not a template, and on the task, not at the Taskfile's top \
     level. That task is then the service of forwarded port 3000: the port's row in the \
     user's Ports section starts and stops it, and a connection to the port starts it when \
     nothing serves the port. The port goes in devcontainer.json's forwardPorts as well. A \
     change to the Taskfile is in force at once and rebuilds nothing. The IDE no longer reads \
     customizations.taste-ide.services in devcontainer.json: where you find that block, move \
     each port into its task's env and delete the block.";

/// Where data that must outlive a container goes, for an agent: a named
/// volume, which taste-devcontainer gives each environment its own copy
/// of, keeps across rebuilds, carries when the environment moves to
/// another VM, and hands to the container's user while it is new and
/// empty. A project's MariaDB kept its data in the image user's home and
/// lost it with every container (2026-10-24).
pub const STATE: &str = "Your environment's container is made again from its config each \
     time the environment starts — after a stop, a reload, a rebuild, or the IDE closing — \
     and everything written inside it is lost then, except the checkout and /home/agent. \
     That includes the image user's own home (a task's $HOME), so a server keeping its data \
     there starts empty every time. Data that must outlive the container — a database's data \
     directory, uploaded files, a cache worth keeping — goes on a named volume in \
     .devcontainer/devcontainer.json `mounts`, written as a string (the object form is \
     refused):\n\n\
     ```json\n\
     \"mounts\": [\"source=mysql-data,target=/home/dev/mysql-data,type=volume\"]\n\
     ```\n\n\
     and the program pointed at its target (here, `mariadbd --datadir=/home/dev/mysql-data`). \
     Each environment has its own copy of each volume, kept across stops, reloads, rebuilds, \
     and moves to another VM, and removed only when the user removes it or the environment. \
     A new, empty volume is given to the container's user, so a program running as that user \
     can write it. A change to `mounts` takes effect with devcontainer_reload, and the volume \
     starts empty: data already written inside the container is not moved into it, so dump it \
     first (mariadb-dump, pg_dump) and load the dump once the volume is in place. System \
     packages are the same: put them in the image, a RUN line in .devcontainer/'s \
     Containerfile (the build runs as root), then devcontainer_reload. The container's own \
     user is usually not root, so dnf, apt and the like fail inside it, and ide_exec refuses \
     them there; even as root, what they install goes with the container. In a Compose \
     config (\"dockerComposeFile\"), a database or cache runs as its own service from its \
     official image, and a named volume in the Compose file on that service keeps its data \
     the same way.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Convention {
    /// Absolute path where the file belongs in this workspace.
    pub path: PathBuf,
    /// What the file does, phrased for an agent deciding whether to
    /// create it.
    pub purpose: &'static str,
    pub exists: bool,
    /// Whether the file tree offers a ghost row when missing. `.taste.yaml`
    /// does not: it is reserved, and nothing needs it yet.
    pub ghost: bool,
    /// A directory rather than a file: `.devcontainer/` itself, whose
    /// ghost creates the folder and reveals the ghosts of what goes in it.
    pub is_dir: bool,
}

/// Every conventional location, present or not.
///
/// The devcontainer is three entries that take turns (David, 2026-09-16:
/// "If .devcontainer/ exists but no json/Containerfile in it, show a ghost
/// entry for the json + Containerfile. If .devcontainer/ doesn't exist,
/// there should be a ghost to create the directory"): the directory, a
/// ghost while it is missing; and inside it, whether it exists yet or not,
/// `devcontainer.json` while no config exists anywhere, and the
/// `Containerfile` while the config is missing or names a build file that
/// is — so the ghost folder opens onto the files it is for, and creating
/// one makes the folder with it (2026-10-05: "I should be able to open the
/// ghost folder and directly create the JSON by clicking on it").
pub fn conventions(root: &Path) -> Vec<Convention> {
    conventions_via(&crate::files::Files::Local, root)
}

/// [`conventions`], wherever the checkout is: the same fixed places,
/// asked of the files service that has the tree.
pub fn conventions_via(files: &crate::files::Files, root: &Path) -> Vec<Convention> {
    // The CONFIG file decides existence: a leftover empty .devcontainer/
    // directory must not silence the suggestion to create the file in it.
    let dir = root.join(".devcontainer");
    let has_devcontainer = files.exists(&dir.join("devcontainer.json"))
        || files.exists(&root.join(".devcontainer.json"))
        || files
            .list(&dir)
            .map(|entries| {
                entries
                    .iter()
                    .any(|e| files.exists(&dir.join(&e.name).join("devcontainer.json")))
            })
            .unwrap_or(false);
    let dir_exists = files.is_dir(&dir);
    let mut list = vec![
        Convention {
            exists: dir_exists || has_devcontainer,
            path: dir.clone(),
            purpose: "the devcontainer's folder: devcontainer.json and, when the image is \
                      built rather than pulled, its Containerfile",
            ghost: true,
            is_dir: true,
        },
        Convention {
            path: dir.join("devcontainer.json"),
            purpose: "devcontainer definition; the IDE builds and attaches to it \
                      (validated: privilege only as nested podman needs it, mounts stay in the workspace). \
                      Run as a non-root user named dev, uid 1000: \"remoteUser\": \"dev\"",
            exists: has_devcontainer,
            // Inside the folder, ghost or real: a ghost folder's rows.
            ghost: true,
            is_dir: false,
        },
    ];
    // A root `.devcontainer.json` is a config with no folder: nothing to
    // build from is wanted in a folder that is not there.
    if dir_exists || !has_devcontainer {
        if let Some(build_file) = wanted_build_file(files, &dir) {
            list.push(Convention {
                exists: false,
                path: dir.join(build_file),
                purpose: "the image the devcontainer builds from, referenced by \
                          devcontainer.json's build.dockerfile; leave it out and pull an \
                          image instead",
                ghost: true,
                is_dir: false,
            });
        }
    }
    for (name, purpose) in [
        (
            ".editorconfig",
            "editor behavior: indentation, charset, final newline",
        ),
        (".gitignore", "tree filtering and ignore rules"),
        (".gitattributes", "git text/eol and diff attributes"),
    ] {
        let path = root.join(name);
        list.push(Convention {
            exists: files.exists(&path),
            path,
            purpose,
            ghost: true,
            is_dir: false,
        });
    }
    // The project's named commands, for the Tasks section (taskfile.dev).
    // Present under any of the names `task` itself looks for; the ghost
    // offers the canonical one.
    list.push(Convention {
        exists: TASKFILE_NAMES
            .iter()
            .any(|name| files.exists(&root.join(name))),
        path: root.join(TASKFILE_NAMES[0]),
        purpose: "the project's named commands (taskfile.dev): the IDE lists them under \
                  Tasks and runs them in the environment",
        ghost: true,
        is_dir: false,
    });
    list.push(Convention {
        exists: files.exists(&root.join(".taste.yaml")),
        path: root.join(".taste.yaml"),
        purpose: "reserved for repo-level IDE configuration; currently \
                  nothing needs it — prefer the conventions above over \
                  adding configuration",
        ghost: false,
        is_dir: false,
    });
    list
}

/// The build file `.devcontainer/` is missing, if it is missing one: the
/// file `devcontainer.json`'s `build.dockerfile` names when that file is
/// absent, or `Containerfile` when there is no config yet. `None` when a
/// build file is present, or when the config pulls an image and names
/// none — an image-based config wants no Containerfile ghost beside it.
fn wanted_build_file(files: &crate::files::Files, dir: &Path) -> Option<String> {
    let config = files.read_to_string(&dir.join("devcontainer.json")).ok();
    let named = config.as_deref().and_then(build_dockerfile);
    match (config.is_some(), named) {
        (true, Some(name)) => (!files.exists(&dir.join(&name))).then_some(name),
        (true, None) => None,
        (false, _) => {
            let present = ["Containerfile", "Dockerfile"]
                .iter()
                .any(|name| files.exists(&dir.join(name)));
            (!present).then(|| "Containerfile".to_string())
        }
    }
}

/// `build.dockerfile` (or the older top-level `dockerFile`) out of a
/// devcontainer.json, read leniently: the file may carry comments and
/// trailing commas, so the value is found rather than parsed.
fn build_dockerfile(text: &str) -> Option<String> {
    for key in ["\"dockerfile\"", "\"dockerFile\""] {
        let Some(at) = text.find(key) else { continue };
        let rest = &text[at + key.len()..];
        let rest = rest.trim_start().strip_prefix(':')?.trim_start();
        let rest = rest.strip_prefix('"')?;
        let end = rest.find('"')?;
        let name = rest[..end].trim();
        if !name.is_empty() && !name.contains('/') && !name.contains("..") {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state text names the home that is kept by the path it is kept
    /// at, and shows a mount in the one form the IDE takes.
    #[test]
    fn the_state_text_names_the_kept_home_and_the_string_form() {
        assert!(STATE.contains(crate::policy::AGENT_HOME_IN_DEVCONTAINER));
        assert!(
            STATE.contains(
                "\"mounts\": [\"source=mysql-data,target=/home/dev/mysql-data,type=volume\"]"
            ),
            "{STATE}"
        );
    }

    /// The services text shows a task's own env giving PORT, which is
    /// what the Taskfile reader reads, and says the old block is gone.
    #[test]
    fn the_services_text_shows_the_port_on_the_task() {
        assert!(
            SERVICES.contains("\n  env:\n    PORT: 3000\n"),
            "{SERVICES}"
        );
        assert!(SERVICES.contains("no longer reads"));
    }

    #[test]
    fn existing_files_are_marked_and_ghosts_selectable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".editorconfig"), "root = true\n").unwrap();
        let list = conventions(dir.path());
        let editorconfig = list
            .iter()
            .find(|c| c.path.ends_with(".editorconfig"))
            .unwrap();
        assert!(editorconfig.exists);
        let ghosts: Vec<_> = list.iter().filter(|c| !c.exists && c.ghost).collect();
        // No folder yet: the folder is a ghost, and so are the files in it.
        assert!(ghosts
            .iter()
            .any(|c| c.path.ends_with(".devcontainer") && c.is_dir));
        assert!(ghosts
            .iter()
            .any(|c| c.path.ends_with(".devcontainer/devcontainer.json")));
        assert!(ghosts
            .iter()
            .any(|c| c.path.ends_with(".devcontainer/Containerfile")));
        // A root `.devcontainer.json` wants no folder and nothing in one.
        std::fs::write(dir.path().join(".devcontainer.json"), "{}").unwrap();
        let list = conventions(dir.path());
        assert!(!list
            .iter()
            .any(|c| !c.exists && c.ghost && c.path.starts_with(dir.path().join(".devcontainer"))));
        assert!(!ghosts.iter().any(|c| c.path.ends_with(".taste.yaml")));
    }

    /// Everything safe mode lets be written is offered as a ghost while it
    /// is missing (`.devcontainer.json` aside, the other spelling of the
    /// config the `.devcontainer/` folder offers). The ghosts may offer
    /// more — the Taskfile, which safe mode does not write.
    #[test]
    fn whatever_safe_mode_writes_is_a_ghost() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ghosts: Vec<PathBuf> = conventions(root)
            .into_iter()
            .filter(|c| c.ghost && !c.exists)
            .map(|c| c.path)
            .collect();
        for scope in crate::policy::safe_mode_scope(root) {
            let offered = ghosts.iter().any(|ghost| ghost.starts_with(&scope));
            let spelling = scope == root.join(".devcontainer.json");
            assert!(
                offered || spelling,
                "{} is writable in safe mode and offered by no ghost",
                scope.display()
            );
        }
    }

    #[test]
    fn a_taskfile_under_any_of_tasks_names_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let taskfile = |list: Vec<Convention>| {
            list.into_iter()
                .find(|c| c.path.ends_with("Taskfile.yml"))
                .unwrap()
        };
        assert!(!taskfile(conventions(dir.path())).exists);
        std::fs::write(dir.path().join("taskfile.yaml"), "version: '3'\n").unwrap();
        assert!(taskfile(conventions(dir.path())).exists);
    }

    #[test]
    fn an_empty_devcontainer_folder_offers_the_config_and_a_containerfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".devcontainer")).unwrap();
        let ghosts = |list: Vec<Convention>| -> Vec<String> {
            list.into_iter()
                .filter(|c| !c.exists && c.ghost)
                .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            ghosts(conventions(dir.path())),
            vec![
                "devcontainer.json",
                "Containerfile",
                ".editorconfig",
                ".gitignore",
                ".gitattributes",
                "Taskfile.yml"
            ]
        );

        // An image-based config wants no Containerfile; a build-based one
        // wants the file it names, until it exists.
        let config = dir.path().join(".devcontainer/devcontainer.json");
        std::fs::write(&config, r#"{"image": "fedora:44"}"#).unwrap();
        assert!(!ghosts(conventions(dir.path()))
            .iter()
            .any(|g| g.ends_with("file")));
        std::fs::write(
            &config,
            "{\n  // built\n  \"build\": { \"dockerfile\": \"Containerfile.dev\" },\n}",
        )
        .unwrap();
        assert!(ghosts(conventions(dir.path())).contains(&"Containerfile.dev".to_string()));
        std::fs::write(
            dir.path().join(".devcontainer/Containerfile.dev"),
            "FROM x\n",
        )
        .unwrap();
        assert!(!ghosts(conventions(dir.path())).contains(&"Containerfile.dev".to_string()));
    }
}
