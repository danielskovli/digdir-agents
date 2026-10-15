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
        eprintln!("This directory already has Agent {}.", agent.metadata.name);
        return Ok(Some(agent.metadata.name));
    }
    let mut status = client.auth_status().await?;
    if !status.any_harness() {
        if !sign_in_to_a_harness(home, client).await? {
            return Ok(None);
        }
        status = client.auth_status().await?;
    }
    if status.github.is_none()
        && prompt::confirm(
            "Agents can act as a GitHub account, such as a bot, to clone, push and open pull requests. Sign in to one now?",
        )?
    {
        // Optional: a token GitHub refuses leaves the Agent without the account, not without init.
        match crate::github_login(client, false, false).await {
            Ok(account) => status.github = Some(account),
            Err(error) => eprintln!("agentctl: {error}; continuing without the GitHub account"),
        }
    }
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
        if !prompt::confirm(&format!("No Agent manifest here. Write {}?", manifest.display()))? {
            return Ok(None);
        }
        write_new(&manifest, &template(&agent_name(root), &status))?;
        eprintln!(
            "Wrote {}; edit it to change the image, resources or access.",
            manifest.display()
        );
    }
    let Some(name) = crate::tui::create_agent_here(home, client).await? else {
        return Ok(None);
    };
    crate::wait_until_converged(client, &name, std::time::Duration::from_mins(10)).await?;
    eprintln!("Agent {name} is ready.");
    Ok(Some(name))
}

/// Creates `path` with `contents`, refusing to replace a file that appeared meanwhile.
fn write_new(path: &Path, contents: &str) -> Result<(), Error> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// Offers the Claude Code login, then the Codex one, returning whether either was stored.
async fn sign_in_to_a_harness(home: &ControlPlaneHome, client: &Client) -> CommandResult<bool> {
    eprintln!("Agents run Claude Code or Codex, and neither is signed in on this host.");
    for (harness, label) in [
        (agent::Harness::ClaudeCode, "Claude Code"),
        (agent::Harness::Codex, "Codex"),
    ] {
        if prompt::confirm(&format!("Sign in to {label} now?"))? {
            crate::harness_login(home, client, harness, false).await?;
            return Ok(true);
        }
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
fn template(name: &str, status: &AuthenticationStatus) -> String {
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
                "      # Optional, so the Agent is still created on a host without a Codex login.\n      optional: true\n",
            );
        }
    }
    let accounts = if status.github.is_some() {
        "  # Act as the account `agentctl github login` stored.\n  accounts:\n    - type: github\n"
    } else {
        "  # Run `agentctl github login` and uncomment to act as a GitHub account.\n  # accounts:\n  #   - type: github\n"
    };
    let code = agent::sandbox::platform::WORKING_DIRECTORY;
    format!(
        "# Created by `agentctl init`. Change it and run `agentctl apply` to update the Agent.
apiVersion: agents.platform/v1alpha1
kind: Agent
metadata:
  name: {name}
spec:
  sandbox:
    image:
      type: reference
      reference: {TEMPLATE_IMAGE}
    platform:
      os: linux
    resources:
      cpu: \"4\"
      memory: \"8Gi\"
      rootFilesystem:
        capacity: \"64Gi\"
        mode: layered
    initSystem: image
    # The image clones its own repositories into {code}. To work on this checkout
    # instead, uncomment the mount; mounts cannot change once the Agent exists.
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

    fn status(claude_code: bool, codex: bool, github: bool) -> AuthenticationStatus {
        AuthenticationStatus {
            claude_code,
            codex,
            github: github.then(|| agent::github::GithubAccount {
                login: "digdir-bot".into(),
                name: "Digdir Bot".into(),
                email: "bot@example.com".into(),
                missing_scopes: Vec::new(),
            }),
        }
    }

    fn decoded(text: &str) -> agent::Agent {
        agent::manifest::decode(text.as_bytes()).expect("the template is a valid manifest")
    }

    #[test]
    fn the_template_declares_the_signed_in_harnesses() {
        let claude = decoded(&template("worker", &status(true, false, false)));
        assert_eq!(claude.metadata.name, "worker");
        assert_eq!(claude.spec.harnesses.len(), 1);
        assert_eq!(claude.spec.harnesses[0].kind, agent::Harness::ClaudeCode);
        assert!(!claude.spec.github_account());
        assert!(claude.spec.ssh_access());
        assert_eq!(claude.spec.home, None);

        let both = decoded(&template("worker", &status(true, true, false)));
        assert_eq!(
            both.spec.default_harness().map(|harness| harness.kind),
            Some(agent::Harness::ClaudeCode)
        );
        assert!(
            both.spec
                .harness(agent::Harness::Codex)
                .is_some_and(|codex| codex.optional)
        );

        let codex = decoded(&template("worker", &status(false, true, false)));
        assert_eq!(codex.spec.harnesses.len(), 1);
        assert!(!codex.spec.harnesses[0].optional);
    }

    #[test]
    fn the_template_uses_the_github_account_only_when_one_is_signed_in() {
        assert!(
            decoded(&template("worker", &status(true, false, true)))
                .spec
                .github_account()
        );
        let without = template("worker", &status(true, false, false));
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
        let text = template("worker", &status(true, false, false));
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
