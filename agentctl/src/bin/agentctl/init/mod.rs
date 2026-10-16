//! `agentctl init`: sets up an Agent for the current directory.
//!
//! Asks for the host logins an Agent needs, writes an `agent.yaml` at the repository root when
//! the working tree has none, and creates the Agent through the TUI's create form.

use std::path::{Path, PathBuf};

use agent::{Error, control_api::Client, harness::AuthenticationStatus, local::home::ControlPlaneHome};

use crate::{CommandResult, prompt};

mod sizing;
mod template;
mod toolchains;

use template::{Checkout, Choices, Github, IMAGE_DIRECTORY, Image};

/// Sets up an Agent for the current directory, returning its name once it is Ready, or `None`
/// when the user declined a step.
///
/// # Errors
///
/// Returns an error when there is no terminal to ask on, a login or apply fails, or the Agent
/// does not become Ready.
pub(crate) async fn run(home: &ControlPlaneHome, client: &Client) -> CommandResult<Option<String>> {
    if !prompt::interactive() {
        return Err(Error::Invalid("agentctl init needs an interactive terminal".into()).into());
    }
    let directory = std::env::current_dir().map_err(Error::from)?;
    if let Ok(agent) = client.resolve_agent_variant(directory.clone(), None).await {
        eprintln!("This directory already has the agent {}.", agent.metadata.name);
        return Ok(Some(agent.metadata.name));
    }
    let mut status = client.auth_status().await?;
    if !status.any_harness() {
        if !sign_in_to_a_harness(home, client).await? {
            return Ok(None);
        }
        status = client.auth_status().await?;
    }
    let github = github_account(client, &status).await;
    if crate::tui::working_tree_manifests(&directory).is_empty() {
        let manifest = manifest_location(&directory);
        let root = manifest.parent().unwrap_or(&directory);
        if manifest.exists() {
            // Discovery honours ignore files, so an ignored manifest is not offered; never replace it.
            return Err(Error::Invalid(format!(
                "{} exists but is ignored by Git, so `agentctl init` cannot offer it; run `agentctl apply -f {}`",
                manifest.display(),
                manifest.display()
            ))
            .into());
        }
        if !prompt::confirm(&format!("No agent manifest here. Write {}?", manifest.display()))? {
            return Ok(None);
        }
        let toolchains = toolchains::detect(root);
        let image = choose_image(&toolchains)?;
        let checkout = choose_checkout(root, image)?;
        let size = choose_size(home.path())?;
        let choices = Choices {
            name: agent_name(root),
            github,
            image,
            checkout,
            size,
        };
        let dockerfile = match image {
            Image::Base => None,
            Image::Built => Some(write_dockerfile(
                root,
                &template::dockerfile(&choices.checkout, &toolchains),
            )?),
        };
        write_new(&manifest, &template::manifest(&choices, &status))?;
        eprintln!("Wrote {}.", manifest.display());
        let question = if dockerfile.is_some() {
            "Edit agent.yaml and the Dockerfile before creating the agent?"
        } else {
            "Edit agent.yaml before creating the agent?"
        };
        if prompt::confirm_lines(&[
            "The image, resources, mounts and access cannot change once the agent exists.",
            question,
        ])? && !edit_manifest(&manifest, dockerfile.as_deref())?
        {
            return Ok(None);
        }
    }
    let manifests = crate::tui::working_tree_manifests(&directory);
    let name = if let [manifest] = manifests.as_slice() {
        create_from(client, manifest).await?
    } else {
        eprintln!();
        eprintln!("This repository has several agent manifests. Choose one, and a name, in the form that opens next.");
        prompt::wait_for_enter("Press Enter to open it…")?;
        let Some(name) = crate::tui::create_agent_here(home, client).await? else {
            return Ok(None);
        };
        name
    };
    crate::wait_until_converged(client, &name, std::time::Duration::from_mins(10)).await?;
    eprintln!("The agent {name} is ready.");
    Ok(Some(name))
}

/// Opens `manifest` in `$VISUAL` or `$EDITOR`, or asks for it to be edited elsewhere, until it is
/// a valid manifest. Returns `false` when the person gives up on an invalid one.
fn edit_manifest(manifest: &Path, dockerfile: Option<&Path>) -> Result<bool, Error> {
    loop {
        let files: Vec<&Path> = std::iter::once(manifest).chain(dockerfile).collect();
        prompt::edit_files(&files)?;
        match agent::manifest::resolve(manifest) {
            Ok(_) => return Ok(true),
            Err(error) => {
                let reason = match error {
                    Error::Invalid(reason) => reason,
                    error => error.to_string(),
                };
                if !prompt::confirm_lines(&[
                    &format!("agent.yaml is not a valid manifest: {reason}"),
                    "Edit it again?",
                ])? {
                    eprintln!("Fix {} and run `agentctl init` again.", manifest.display());
                    return Ok(false);
                }
            }
        }
    }
}

fn docker_running() -> bool {
    std::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Asks for the image: one built here for `toolchains` when Docker runs, else the base image.
fn choose_image(toolchains: &[toolchains::Toolchain]) -> Result<Image, Error> {
    if !docker_running() {
        eprintln!(
            "Docker is not running here, so the agent runs the published base image as it is. Start Docker and run \
             `agentctl init` again to build an image with this repository's toolchains."
        );
        return Ok(Image::Base);
    }
    let found = if toolchains.is_empty() {
        "nothing beyond the base image found".to_owned()
    } else {
        toolchains
            .iter()
            .map(toolchains::Toolchain::describe)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let options = [
        format!("Built for this repository with Docker here: the base image plus {found} (recommended)"),
        "The published base image as it is: no build".to_owned(),
    ];
    Ok(
        match prompt::choose("Which image should the agent run?", &options, Some(0))? {
            Some(1) => Image::Base,
            _ => Image::Built,
        },
    )
}

/// Asks whether the agent clones the repository or works on this checkout. Only a built image
/// can clone, and only a repository with a remote can be cloned.
fn choose_checkout(root: &Path, image: Image) -> Result<Checkout, Error> {
    if image == Image::Base {
        eprintln!("The base image clones nothing, so the agent works on this checkout.");
        return Ok(Checkout::Mount);
    }
    let Some(remote) = remote(root) else {
        eprintln!("This checkout has no remote to clone, so the agent works on it directly.");
        return Ok(Checkout::Mount);
    };
    let options = [
        format!("A fresh clone of {remote}, made in the agent (recommended)"),
        "This checkout, shared with the agent: its changes land on this machine directly".to_owned(),
    ];
    Ok(
        match prompt::choose(
            "Which copy of the repository should the agent work on?",
            &options,
            Some(0),
        )? {
            Some(1) => Checkout::Mount,
            _ => Checkout::Clone(remote),
        },
    )
}

/// The repository to clone: `owner/name` on GitHub, else the `origin` remote's URL.
fn remote(root: &Path) -> Option<String> {
    if let Some(repository) = crate::github::current_repository() {
        return Some(repository.to_string());
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !url.is_empty()).then_some(url)
}

/// Asks for the agent's size, recommending one that fits this host and the free space beside
/// agentd's state in `state`.
fn choose_size(state: &Path) -> Result<sizing::Size, Error> {
    let Some(host) = sizing::Host::measure(state) else {
        let size = sizing::Host {
            cpus: 4,
            memory_gib: 16,
            free_disk_gib: 32,
        }
        .recommend();
        eprintln!("Could not measure this machine, so the agent gets {}.", size.describe());
        return Ok(size);
    };
    let recommended = host.recommend();
    let mut lines = vec![
        "How large should the agent be?".to_owned(),
        format!(
            "This machine has {} CPUs, {} GiB memory and {} GiB free disk. Edit agent.yaml next for other sizes.",
            host.cpus, host.memory_gib, host.free_disk_gib
        ),
    ];
    if host.free_disk_gib < sizing::LOW_DISK_GIB {
        lines.push(format!(
            "⚠ That is little free disk: an agent's disk grows into it as the agent writes, up to its size. \
             Free some space, or watch it with `df -h {}`.",
            state.display()
        ));
    }
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let options = [
        format!("{} (recommended)", recommended.describe()),
        format!("{}, for running several agents", recommended.smaller().describe()),
    ];
    Ok(match prompt::choose_lines(&lines, &options, Some(0))? {
        Some(1) => recommended.smaller(),
        _ => recommended,
    })
}

/// Writes the built image's Dockerfile under `root`, or keeps one that is already there.
fn write_dockerfile(root: &Path, contents: &str) -> Result<PathBuf, Error> {
    let directory = root.join(IMAGE_DIRECTORY);
    let dockerfile = directory.join("Dockerfile");
    if dockerfile.exists() {
        eprintln!("Using the existing {}.", dockerfile.display());
        return Ok(dockerfile);
    }
    std::fs::create_dir_all(&directory)?;
    write_new(&dockerfile, contents)?;
    eprintln!("Wrote {}.", dockerfile.display());
    Ok(dockerfile)
}

/// Creates `path` with `contents`, refusing to replace a file that appeared meanwhile.
fn write_new(path: &Path, contents: &str) -> Result<(), Error> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// Makes sure Agents of this repository have a working GitHub token: present, not expired, and
/// able to see the repository. Offers the login when not; GitHub is optional, so a failure
/// leaves the Agent without it rather than stopping init.
async fn github_account(client: &Client, status: &AuthenticationStatus) -> Github {
    let Some(repository) = crate::github::current_repository() else {
        return match status.github.as_slice() {
            [_] => Github::Only,
            [] => offer_github_login(client, None).await,
            _ => Github::None,
        };
    };
    let owner = repository.owner.clone();
    match client.github_check(Some(owner.clone()), Some(repository.clone())).await {
        Ok(check) => {
            let now = time::OffsetDateTime::now_utc();
            let visible = check
                .inspection
                .repository
                .as_ref()
                .is_some_and(|(_, visible)| *visible);
            let expiring = check.token.expires_soon(now)
                || check
                    .inspection
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= now + agent::github::EXPIRY_WARNING);
            if visible && !expiring {
                eprintln!(
                    "GitHub: agents act as {} in {owner}, and can see {repository}.",
                    check.inspection.account.login
                );
                return Github::Owner(owner);
            }
            let reason = format!(
                "The GitHub token for {} {}.",
                check.token.scope(),
                if visible {
                    "expires within a week"
                } else {
                    "cannot see this repository"
                }
            );
            match offer_github_login(client, Some(&reason)).await {
                Github::None if visible => Github::Owner(owner),
                chosen => chosen,
            }
        }
        // No token for this owner yet, which the offer below explains by itself.
        Err(_) => offer_github_login(client, None).await,
    }
}

/// Offers setting up a GitHub token, saying first why when there is a reason beyond having none.
async fn offer_github_login(client: &Client, reason: Option<&str>) -> Github {
    let asked = reason.map_or_else(
        || prompt::confirm("Should agents be able to work on GitHub, such as pushing code and opening pull requests?"),
        |reason| prompt::confirm_lines(&[reason, "Set up a new GitHub token for agents now?"]),
    );
    if !asked.unwrap_or(false) {
        return Github::None;
    }
    match crate::github::login(client, crate::github::Source::Form, None).await {
        Ok(Some(token)) => token.owner.map_or(Github::Only, Github::Owner),
        Ok(None) => Github::None,
        Err(error) => {
            eprintln!("agentctl: {error}; continuing without GitHub");
            Github::None
        }
    }
}

/// Creates the agent from the repository's only manifest, asking for its name with the manifest's
/// own as the suggestion, or the first free numbered one when an agent already has it.
async fn create_from(client: &Client, manifest: &Path) -> CommandResult<String> {
    let manifest_name = agent::manifest::resolve(manifest)?.agent.metadata.name;
    let agents = client.list_agents().await?;
    let taken = |name: &str| agents.iter().any(|agent| agent.metadata.name == name);
    let suggestion = if taken(&manifest_name) {
        crate::tui::numbered_name(&manifest_name, &agents)
    } else {
        manifest_name
    };
    loop {
        let answer = prompt::ask(&format!("Name of the agent [{suggestion}]"))?;
        let name = if answer.is_empty() { suggestion.clone() } else { answer };
        if let Err(invalid) = ::sandbox::SandboxName::new(name.clone()) {
            eprintln!("{name:?} cannot name an agent: {invalid}.");
            continue;
        }
        if taken(&name) {
            eprintln!("An agent named {name} already exists.");
            continue;
        }
        crate::tui::create_agent(client, manifest.to_path_buf(), name.clone(), None).await?;
        return Ok(name);
    }
}

/// Offers the Claude Code login, then the Codex one, returning whether either was stored.
async fn sign_in_to_a_harness(home: &ControlPlaneHome, client: &Client) -> CommandResult<bool> {
    let options = [
        "Claude Code".to_owned(),
        "Codex".to_owned(),
        "Neither for now".to_owned(),
    ];
    let harness = match prompt::choose(
        "Neither Claude Code nor Codex is signed in on this host. Which should agents use?",
        &options,
        Some(0),
    )? {
        Some(0) => Some(agent::Harness::ClaudeCode),
        Some(1) => Some(agent::Harness::Codex),
        _ => None,
    };
    if let Some(harness) = harness {
        crate::harness_login(home, client, harness, false).await?;
        return Ok(true);
    }
    eprintln!("Sign in with `agentctl claude login` or `agentctl codex login`, then run `agentctl init` again.");
    Ok(false)
}

/// The Agent name a new manifest suggests: the directory's name in Sandbox name characters.
fn agent_name(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if ::sandbox::SandboxName::accepts(character) {
                character
            } else {
                '-'
            }
        })
        .take(::sandbox::MAX_SANDBOX_NAME_BYTES)
        .collect();
    let name = name.trim_matches(|character: char| !character.is_ascii_alphanumeric());
    if ::sandbox::SandboxName::new(name.to_owned()).is_ok() {
        name.to_owned()
    } else {
        "agent".to_owned()
    }
}

/// Where a new manifest for `directory` goes: the repository root, so the Agent is found from
/// anywhere in the checkout, or `directory` itself outside a repository.
fn manifest_location(directory: &Path) -> PathBuf {
    crate::tui::repository_root(directory)
        .unwrap_or(directory)
        .join(agent::manifest::MANIFEST_FILE)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn the_suggested_name_is_the_directory_in_sandbox_name_characters() {
        assert_eq!(agent_name(Path::new("/src/digdir-agents")), "digdir-agents");
        assert_eq!(agent_name(Path::new("/src/My Repo.rs")), "my-repo-rs");
        assert_eq!(agent_name(Path::new("/src/_hidden_")), "hidden");
        assert_eq!(agent_name(Path::new("/")), "agent");
        assert_eq!(agent_name(Path::new("/src/---")), "agent");
    }

    #[test]
    fn the_dockerfile_is_written_once_and_an_existing_one_is_kept() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let written = write_dockerfile(directory.path(), "FROM base\n").expect("written");
        assert_eq!(written, directory.path().join(".agentctl").join("Dockerfile"));
        std::fs::write(&written, "FROM mine\n").expect("customized");
        assert_eq!(
            write_dockerfile(directory.path(), "FROM base\n").expect("kept"),
            written
        );
        assert_eq!(std::fs::read_to_string(&written).expect("read"), "FROM mine\n");
    }

    #[test]
    fn a_new_manifest_never_replaces_an_existing_file() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().join("agent.yaml");
        std::fs::write(&path, "mine").expect("existing manifest");
        write_new(&path, "template").expect_err("an existing file is kept");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "mine");
    }

    #[test]
    fn a_new_manifest_goes_to_the_repository_root() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let nested = directory.path().join("crates").join("tool");
        std::fs::create_dir_all(&nested).expect("nested directory");
        assert_eq!(manifest_location(&nested), nested.join("agent.yaml"));
        std::fs::create_dir(directory.path().join(".git")).expect("repository");
        assert_eq!(manifest_location(&nested), directory.path().join("agent.yaml"));
    }
}
