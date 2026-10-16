//! `agentctl init`: sets up an Agent for the current directory.
//!
//! Asks for the host logins an Agent needs, writes an `agent.yaml` at the repository root when
//! the working tree has none, and creates the Agent through the TUI's create form.

use std::path::{Path, PathBuf};

use agent::{Error, control_api::Client, harness::AuthenticationStatus, local::home::ControlPlaneHome};

use crate::{CommandResult, prompt};

/// The image a new manifest starts from: the published self-development image, which has both
/// harnesses, systemd, SSH and common toolchains.
const TEMPLATE_IMAGE: &str = "ghcr.io/digdir/digdir-agents/agentctl-self-dev-minimal:latest";

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
        let image = choose_image()?;
        let dockerfile = match image {
            Image::Prebuilt => None,
            Image::Custom => Some(write_dockerfile(root)?),
        };
        write_new(&manifest, &template(&agent_name(root), &status, &github, image))?;
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
    let Some(name) = crate::tui::create_agent_here(home, client).await? else {
        return Ok(None);
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

/// Which image a new manifest uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Image {
    /// The published development image.
    Prebuilt,
    /// The repository's own `.agentctl/Dockerfile`, built with Docker on this host.
    Custom,
}

/// Where the custom image's Dockerfile and its build context live, relative to the manifest.
const IMAGE_DIRECTORY: &str = ".agentctl";

fn choose_image() -> Result<Image, Error> {
    let options = [
        "Pre-built: the published development image (recommended)".to_owned(),
        "Custom: a Dockerfile in this repository, built with Docker on this machine".to_owned(),
    ];
    Ok(
        match prompt::choose("Which image should the agent run?", &options, Some(0))? {
            Some(1) => Image::Custom,
            _ => Image::Prebuilt,
        },
    )
}

/// Writes the custom image's Dockerfile under `root`, or keeps one that is already there.
fn write_dockerfile(root: &Path) -> Result<PathBuf, Error> {
    let directory = root.join(IMAGE_DIRECTORY);
    let dockerfile = directory.join("Dockerfile");
    if dockerfile.exists() {
        eprintln!("Using the existing {}.", dockerfile.display());
        return Ok(dockerfile);
    }
    std::fs::create_dir_all(&directory)?;
    write_new(&dockerfile, &dockerfile_template())?;
    eprintln!("Wrote {}.", dockerfile.display());
    Ok(dockerfile)
}

/// A Dockerfile that builds on the published image, so the agent keeps what the platform needs.
fn dockerfile_template() -> String {
    format!(
        "# Created by `agentctl init`: the image this repository's agent runs. It builds with Docker on
# the machine that runs agentctl.
#
# It starts from the published development image, which has everything an agent needs: systemd,
# OpenSSH, the agent user, Claude Code, Codex, Git, the GitHub CLI and common toolchains. Add what
# this repository needs below. An agent keeps the image it was created with, so to use changes,
# delete the agent and create it again.
FROM {TEMPLATE_IMAGE}

USER root

# The published image clones the agent platform's own repositories at boot; this agent does not
# need them.
RUN systemctl disable agent-workspace-init.service

# Add packages and tools here, for example:
# RUN apt-get update \\
#     && apt-get install --yes --no-install-recommends postgresql-client \\
#     && rm -rf /var/lib/apt/lists/*
# RUN npm install --global pnpm

USER agent
WORKDIR {code}
",
        code = agent::sandbox::platform::WORKING_DIRECTORY
    )
}

/// Creates `path` with `contents`, refusing to replace a file that appeared meanwhile.
fn write_new(path: &Path, contents: &str) -> Result<(), Error> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// Whether a new manifest gives the Agent a GitHub token, and whose.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Github {
    /// No token applies; the manifest shows how to add one.
    None,
    /// The only stored token, whichever owner it is for.
    Only,
    /// The token for this owner, or the one for every organization without its own.
    Owner(String),
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

/// A manifest with the signed-in harnesses and, when one is signed in, the GitHub account.
fn template(name: &str, status: &AuthenticationStatus, github: &Github, image: Image) -> String {
    let mut harnesses = String::new();
    if status.claude_code {
        harnesses.push_str("    - type: claudeCode\n      auth: mediated\n");
        if status.codex {
            harnesses.push_str("      default: true\n");
        }
    }
    if status.codex {
        harnesses.push_str("    - type: codex\n      auth: mediated\n");
        if status.claude_code {
            harnesses.push_str(
                "      # Optional, so the agent is still created on a host without a Codex login.\n      optional: true\n",
            );
        }
    }
    let accounts = match github {
        Github::Owner(owner) => format!(
            "  # Act as the GitHub token `agentctl github login` stored for {owner}.\n  accounts:\n    - type: github\n      owner: {owner}\n"
        ),
        Github::Only => {
            "  # Act as the GitHub token `agentctl github login` stored.\n  accounts:\n    - type: github\n".to_owned()
        }
        Github::None => {
            "  # Run `agentctl github login` and uncomment to act as a GitHub account.\n  # accounts:\n  #   - type: github\n"
                .to_owned()
        }
    };
    let code = agent::sandbox::platform::WORKING_DIRECTORY;
    let image = match image {
        Image::Prebuilt => format!("    image:\n      type: reference\n      reference: {TEMPLATE_IMAGE}\n"),
        Image::Custom => format!(
            "    # Built with Docker on this machine from {IMAGE_DIRECTORY}/Dockerfile.\n    image:\n      type: build\n      context: {IMAGE_DIRECTORY}\n      dockerfile: Dockerfile\n"
        ),
    };
    format!(
        "# Created by `agentctl init`. Change it and run `agentctl apply` to update the agent.
apiVersion: agents.platform/v1alpha1
kind: Agent
metadata:
  name: {name}
spec:
  sandbox:
{image}    platform:
      os: linux
    resources:
      cpu: \"4\"
      memory: \"8Gi\"
      rootFilesystem:
        capacity: \"64Gi\"
        mode: layered
    initSystem: image
    # The image clones its own repositories into {code}. To work on this checkout
    # instead, uncomment the mount; mounts cannot change once the agent exists.
    # mounts:
    #   - type: bind
    #     source: .
    #     target: {code}/{name}
    #     readOnly: false
  harnesses:
{harnesses}{accounts}  access:
    - type: ssh
"
    )
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

    fn status(claude_code: bool, codex: bool) -> AuthenticationStatus {
        AuthenticationStatus {
            claude_code,
            codex,
            github: Vec::new(),
        }
    }

    fn decoded(text: &str) -> agent::Agent {
        agent::manifest::decode(text.as_bytes()).expect("the template is a valid manifest")
    }

    #[test]
    fn the_template_declares_the_signed_in_harnesses() {
        let claude = decoded(&template(
            "worker",
            &status(true, false),
            &Github::None,
            Image::Prebuilt,
        ));
        assert_eq!(claude.metadata.name, "worker");
        assert_eq!(claude.spec.harnesses.len(), 1);
        assert_eq!(claude.spec.harnesses[0].kind, agent::Harness::ClaudeCode);
        assert!(!claude.spec.github_account());
        assert!(claude.spec.ssh_access());

        let both = decoded(&template("worker", &status(true, true), &Github::None, Image::Prebuilt));
        assert_eq!(
            both.spec.default_harness().map(|harness| harness.kind),
            Some(agent::Harness::ClaudeCode)
        );
        assert!(
            both.spec
                .harness(agent::Harness::Codex)
                .is_some_and(|codex| codex.optional)
        );

        let codex = decoded(&template(
            "worker",
            &status(false, true),
            &Github::None,
            Image::Prebuilt,
        ));
        assert_eq!(codex.spec.harnesses.len(), 1);
        assert!(!codex.spec.harnesses[0].optional);
    }

    #[test]
    fn the_template_names_the_github_owner_only_when_a_token_applies() {
        let owned = decoded(&template(
            "worker",
            &status(true, false),
            &Github::Owner("digdir".into()),
            Image::Prebuilt,
        ));
        assert!(owned.spec.github_account());
        assert_eq!(owned.spec.github_owner(), Some("digdir"));
        let only = decoded(&template(
            "worker",
            &status(true, false),
            &Github::Only,
            Image::Prebuilt,
        ));
        assert!(only.spec.github_account());
        assert_eq!(only.spec.github_owner(), None);
        let without = template("worker", &status(true, false), &Github::None, Image::Prebuilt);
        assert!(!decoded(&without).spec.github_account());
        assert!(without.contains("# accounts:"), "{without}");
    }

    #[test]
    fn the_suggested_name_is_the_directory_in_sandbox_name_characters() {
        assert_eq!(agent_name(Path::new("/src/digdir-agents")), "digdir-agents");
        assert_eq!(agent_name(Path::new("/src/My Repo.rs")), "my-repo-rs");
        assert_eq!(agent_name(Path::new("/src/_hidden_")), "hidden");
        assert_eq!(agent_name(Path::new("/")), "agent");
        assert_eq!(agent_name(Path::new("/src/---")), "agent");
    }

    #[test]
    fn the_template_offers_this_checkout_as_a_commented_mount() {
        let text = template("worker", &status(true, false), &Github::None, Image::Prebuilt);
        let uncommented = text
            .replace("    # mounts:", "    mounts:")
            .replace("    #   ", "      ");
        let agent = decoded(&uncommented);
        assert!(matches!(
            agent.spec.sandbox.mounts.as_slice(),
            [agent::MountSpec::Bind { source, target, read_only: false }]
                if source == Path::new(".") && target.as_str() == format!("{}/worker", agent::sandbox::platform::WORKING_DIRECTORY)
        ));
    }

    #[test]
    fn a_custom_image_is_built_from_the_repositorys_own_dockerfile() {
        let agent = decoded(&template("worker", &status(true, false), &Github::None, Image::Custom));
        let image = serde_yaml_ng::to_value(&agent.spec.sandbox.image).expect("image");
        assert_eq!(image["type"], "build", "{image:?}");
        assert_eq!(image["context"], IMAGE_DIRECTORY, "{image:?}");
        assert_eq!(image["dockerfile"], "Dockerfile", "{image:?}");

        let dockerfile = dockerfile_template();
        assert!(
            dockerfile.contains(&format!("\nFROM {TEMPLATE_IMAGE}\n")),
            "{dockerfile}"
        );
        assert!(
            dockerfile.contains("RUN systemctl disable agent-workspace-init.service"),
            "{dockerfile}"
        );
        assert!(
            dockerfile
                .trim_end()
                .ends_with(&format!("WORKDIR {}", agent::sandbox::platform::WORKING_DIRECTORY)),
            "{dockerfile}"
        );
    }

    #[test]
    fn the_dockerfile_is_written_once_and_an_existing_one_is_kept() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let written = write_dockerfile(directory.path()).expect("written");
        assert_eq!(written, directory.path().join(".agentctl").join("Dockerfile"));
        std::fs::write(&written, "FROM mine\n").expect("customized");
        assert_eq!(write_dockerfile(directory.path()).expect("kept"), written);
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
