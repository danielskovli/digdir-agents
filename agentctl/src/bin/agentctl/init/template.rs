//! The `agent.yaml` and `.agentctl/Dockerfile` that `agentctl init` writes.

use agent::harness::AuthenticationStatus;

use super::{sizing::Size, toolchains::Toolchain};

/// The published image every new agent starts from: the platform's Linux contract, both
/// harnesses, Git and the GitHub CLI, and nothing specific to one project.
pub(super) const BASE_IMAGE: &str = "ghcr.io/digdir/digdir-agents/agentctl-base-ubuntu:latest";
/// Where a built image's Dockerfile and its build context live, relative to the manifest.
pub(super) const IMAGE_DIRECTORY: &str = ".agentctl";
/// The scratch space a build gets in `/tmp`; the Sandbox default is a quarter of guest memory,
/// capped at 512 MiB, too small for a large build. A tmpfs holds only what is written to it.
const TMP_CAPACITY: &str = "2Gi";

/// Whether a new manifest gives the Agent a GitHub token, and whose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Github {
    /// No token applies; the manifest shows how to add one.
    None,
    /// The only stored token, whichever owner it is for.
    Only,
    /// The token for this owner, or the one for every organization without its own.
    Owner(String),
}

/// Which image a new manifest uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Image {
    /// `.agentctl/Dockerfile`, built with Docker on this host from the base image.
    Built,
    /// The published base image as it is.
    Base,
}

/// Where the agent's copy of the repository comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Checkout {
    /// A clone made at the agent's first boot, from `owner/name` on GitHub or a Git URL.
    Clone(String),
    /// This checkout, mounted read-write into the agent.
    Mount,
}

/// Everything the person chose for a new manifest.
#[derive(Clone, Debug)]
pub(super) struct Choices {
    pub(super) name: String,
    pub(super) github: Github,
    pub(super) image: Image,
    pub(super) checkout: Checkout,
    pub(super) size: Size,
}

/// A manifest for `choices`: every harness, required when signed in and optional otherwise, so a
/// later login adds it to the agent on its next pass.
pub(super) fn manifest(choices: &Choices, status: &AuthenticationStatus) -> String {
    let Choices {
        name,
        github,
        image,
        checkout,
        size,
    } = choices;
    let claude_default = status.claude_code || !status.codex;
    let harness = |kind: &str, signed_in: bool, default: bool| {
        let default = if default { "      default: true\n" } else { "" };
        let optional = if signed_in {
            String::new()
        } else {
            format!(
                "      # Optional until `agentctl {} login` stores a login on this host.\n      optional: true\n",
                if kind == "codex" { "codex" } else { "claude" }
            )
        };
        format!("    - type: {kind}\n      auth: mediated\n{default}{optional}")
    };
    let harnesses =
        harness("claudeCode", status.claude_code, claude_default) + &harness("codex", status.codex, !claude_default);
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
        Image::Base => format!("    image:\n      type: reference\n      reference: {BASE_IMAGE}\n"),
        Image::Built => format!(
            "    # Built with Docker on this machine from {IMAGE_DIRECTORY}/Dockerfile.\n    image:\n      type: build\n      context: {IMAGE_DIRECTORY}\n      dockerfile: Dockerfile\n"
        ),
    };
    let checkout = match checkout {
        Checkout::Clone(repository) => format!(
            "    # The image clones {repository} into {code} at first boot.
    # To work on this checkout instead, uncomment the bind mount; mounts cannot change once the
    # agent exists.
    mounts:
      # - type: bind
      #   source: .
      #   target: {code}/{name}
      #   readOnly: false
"
        ),
        Checkout::Mount => format!(
            "    # This checkout, shared with the agent: what it changes here changes on this machine.
    mounts:
      - type: bind
        source: .
        target: {code}/{name}
        readOnly: false
"
        ),
    };
    let Size {
        cpus,
        memory_gib,
        disk_gib,
    } = size;
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
      cpu: \"{cpus}\"
      memory: \"{memory_gib}Gi\"
      # The most the agent's disk may grow to. It takes space on this machine only as the agent
      # writes, so keep it below the free space here.
      rootFilesystem:
        capacity: \"{disk_gib}Gi\"
        mode: layered
    initSystem: image
{checkout}      # Build scratch space, held in memory only as it is used.
      - type: tmpfs
        target: /tmp
        capacity: \"{TMP_CAPACITY}\"
  harnesses:
{harnesses}{accounts}  access:
    - type: ssh
"
    )
}

/// A Dockerfile on the base image that clones `checkout` at boot and installs `toolchains`.
pub(super) fn dockerfile(checkout: &Checkout, toolchains: &[Toolchain]) -> String {
    let code = agent::sandbox::platform::WORKING_DIRECTORY;
    let repositories = match checkout {
        Checkout::Clone(repository) => format!(
            "# Repositories the agent clones into {code} at first boot, one per line: owner/name on
# GitHub, or any URL Git can clone.
RUN printf '%s\\n' '{repository}' >/etc/agent/repositories
"
        ),
        Checkout::Mount => format!(
            "# The agent works on the checkout mounted at {code}. To clone repositories at first boot
# instead, list them here, one per line: owner/name on GitHub, or any URL Git can clone.
# RUN printf '%s\\n' 'owner/name' >/etc/agent/repositories
"
        ),
    };
    let blocks: String = toolchains
        .iter()
        .flat_map(|toolchain| ["\n".to_owned(), block(toolchain)])
        .collect();
    format!(
        "# Created by `agentctl init`: the image this repository's agent runs. It builds with Docker on
# the machine that runs agentctl.
#
# It starts from the published base image, which has what an agent needs: systemd, OpenSSH, the
# agent user, Claude Code, Codex, Node.js, Git and the GitHub CLI. Below are the toolchains found in
# this repository; change their versions, remove what the agent does not need and add the rest. An
# agent keeps the image it was created with, so to use changes, delete the agent and create it
# again.
FROM {BASE_IMAGE}

USER root
ARG TARGETARCH

{repositories}{blocks}
# Add packages and tools here, for example:
# RUN apt-get update \\
#     && apt-get install --yes --no-install-recommends postgresql-client \\
#     && rm -rf /var/lib/apt/lists/*

USER agent
WORKDIR {code}
"
    )
}

fn block(toolchain: &Toolchain) -> String {
    match toolchain {
        Toolchain::Node { version, corepack } => node(version.as_deref(), *corepack),
        Toolchain::Dotnet { sdk } => dotnet(sdk.as_deref()),
        Toolchain::Go { version } => go(version.as_deref()),
        Toolchain::Rust { channel } => rust(channel.as_deref()),
        Toolchain::Python => "# Python virtual environments and pip for the image's Python 3.
RUN apt-get update \\
    && apt-get install --yes --no-install-recommends python3-dev python3-pip python3-venv \\
    && rm -rf /var/lib/apt/lists/*
"
        .to_owned(),
        Toolchain::Java { maven } => format!(
            "# A JDK{}; Gradle projects bring their own wrapper.
RUN apt-get update \\
    && apt-get install --yes --no-install-recommends openjdk-21-jdk-headless{} \\
    && rm -rf /var/lib/apt/lists/*
",
            if *maven { " and Maven" } else { "" },
            if *maven { " maven" } else { "" }
        ),
    }
}

fn node(version: Option<&str>, corepack: bool) -> String {
    let install = version.map(|version| {
        // An exact version installs that release; a shorter one the latest of its major line.
        let release = if version.split('.').count() == 3 {
            format!("release/v{version}")
        } else {
            format!("release/latest-v{}.x", version.split('.').next().unwrap_or(version))
        };
        format!(
            "# Node.js {version}, in place of the base image's.
ARG NODE_RELEASE={release}
RUN case \"${{TARGETARCH}}\" in amd64) NODE_ARCH=x64 ;; arm64) NODE_ARCH=arm64 ;; esac \\
    && NODE_SUMS=\"$(curl -fsSL \"https://nodejs.org/download/${{NODE_RELEASE}}/SHASUMS256.txt\")\" \\
    && NODE_ARCHIVE=\"$(printf '%s\\n' \"${{NODE_SUMS}}\" | awk -v arch=\"linux-${{NODE_ARCH}}.tar.xz\" '$2 ~ (arch \"$\") {{ print $2; exit }}')\" \\
    && test -n \"${{NODE_ARCHIVE}}\" \\
    && curl -fsSL \"https://nodejs.org/download/${{NODE_RELEASE}}/${{NODE_ARCHIVE}}\" -o /tmp/node.tar.xz \\
    && printf '%s\\n' \"${{NODE_SUMS}}\" | awk -v name=\"${{NODE_ARCHIVE}}\" '$2 == name {{ print $1 \"  /tmp/node.tar.xz\" }}' | sha256sum -c - \\
    && tar -xJf /tmp/node.tar.xz --strip-components=1 -C /usr/local \\
    && rm /tmp/node.tar.xz \\
    && node --version
"
        )
    });
    let corepack = if corepack {
        "# The package manager package.json names, through Corepack.
RUN npm install --global corepack \\
    && corepack enable
"
    } else {
        ""
    };
    format!("{}{corepack}", install.unwrap_or_default())
}

fn dotnet(sdk: Option<&str>) -> String {
    let (what, selection) = sdk.map_or_else(
        || ("the latest LTS release".to_owned(), "--channel LTS".to_owned()),
        |sdk| (format!("{sdk}, as global.json asks"), format!("--version {sdk}")),
    );
    format!(
        "# The .NET SDK: {what}.
ENV DOTNET_ROOT=/usr/local/share/dotnet
ENV DOTNET_CLI_TELEMETRY_OPTOUT=1
ENV DOTNET_NOLOGO=1
ARG DOTNET_INSTALL_SCRIPT_COMMIT=6f559c420847ded38591392dafe785ad511f39f5
ARG DOTNET_INSTALL_SCRIPT_SHA256=082f7685e156738a1b2e2ed8381a621870d4ce8e8c59278034556f05c186eb2e
RUN apt-get update \\
    && apt-get install --yes --no-install-recommends libicu-dev \\
    && rm -rf /var/lib/apt/lists/* \\
    && case \"${{TARGETARCH}}\" in amd64) DOTNET_ARCH=x64 ;; arm64) DOTNET_ARCH=arm64 ;; esac \\
    && curl -fsSL \\
        \"https://raw.githubusercontent.com/dotnet/install-scripts/${{DOTNET_INSTALL_SCRIPT_COMMIT}}/src/dotnet-install.sh\" \\
        -o /tmp/dotnet-install.sh \\
    && echo \"${{DOTNET_INSTALL_SCRIPT_SHA256}}  /tmp/dotnet-install.sh\" | sha256sum -c - \\
    && bash /tmp/dotnet-install.sh --architecture \"${{DOTNET_ARCH}}\" --install-dir \"${{DOTNET_ROOT}}\" --no-path {selection} \\
    && ln -s \"${{DOTNET_ROOT}}/dotnet\" /usr/local/bin/dotnet \\
    && rm /tmp/dotnet-install.sh \\
    && dotnet --version
"
    )
}

fn go(version: Option<&str>) -> String {
    let what = version.map_or_else(
        || "the latest release".to_owned(),
        |version| format!("{version}, as go.mod asks"),
    );
    format!(
        "# Go: {what}. An empty GO_VERSION takes the latest release; a version without its patch
# number, the latest patch.
ARG GO_VERSION={version}
ENV PATH=/usr/local/go/bin:${{HOME}}/go/bin:${{PATH}}
RUN GO_FILE=\"$(curl -fsSL 'https://go.dev/dl/?mode=json&include=all' | jq -r --arg v \"${{GO_VERSION}}\" --arg arch \"${{TARGETARCH}}\" \\
        '[.[] | select(.stable and ($v == \"\" or .version == \"go\" + $v or (.version | startswith(\"go\" + $v + \".\"))))][0].files[] | select(.os == \"linux\" and .arch == $arch and .kind == \"archive\") | .filename + \" \" + .sha256')\" \\
    && test -n \"${{GO_FILE}}\" \\
    && curl -fsSL \"https://go.dev/dl/${{GO_FILE% *}}\" -o /tmp/go.tar.gz \\
    && echo \"${{GO_FILE#* }}  /tmp/go.tar.gz\" | sha256sum -c - \\
    && tar -xzf /tmp/go.tar.gz -C /usr/local \\
    && rm /tmp/go.tar.gz \\
    && go version
",
        version = version.unwrap_or_default()
    )
}

fn rust(channel: Option<&str>) -> String {
    let channel = channel.unwrap_or("stable");
    format!(
        "# Rust through rustup, at {channel}. The agent user owns RUSTUP_HOME, so a checkout that pins
# another toolchain installs it on first use. Cargo's home is the agent's own.
ARG RUST_TOOLCHAIN={channel}
ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=${{HOME}}/.cargo
ENV PATH=${{HOME}}/.cargo/bin:${{PATH}}
RUN apt-get update \\
    && apt-get install --yes --no-install-recommends pkg-config libssl-dev rustup \\
    && rm -rf /var/lib/apt/lists/* \\
    && install -d -o agent -g agent /usr/local/rustup \"${{CARGO_HOME}}\" \\
    && rustup set profile minimal \\
    && rustup default \"${{RUST_TOOLCHAIN}}\" \\
    && rustup component add clippy rustfmt \\
    && chown -R agent:agent /usr/local/rustup \"${{CARGO_HOME}}\" \\
    && cargo --version
"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::path::Path;

    use super::*;

    fn status(claude_code: bool, codex: bool) -> AuthenticationStatus {
        AuthenticationStatus {
            claude_code,
            codex,
            github: Vec::new(),
        }
    }

    fn choices(github: Github, image: Image, checkout: Checkout) -> Choices {
        Choices {
            name: "worker".into(),
            github,
            image,
            checkout,
            size: Size {
                cpus: 2,
                memory_gib: 4,
                disk_gib: 16,
            },
        }
    }

    fn built() -> Choices {
        choices(Github::None, Image::Built, Checkout::Clone("digdir/worker".into()))
    }

    fn decoded(text: &str) -> agent::Agent {
        agent::manifest::decode(text.as_bytes()).expect("the template is a valid manifest")
    }

    #[test]
    fn signed_in_harnesses_are_required_and_the_others_optional() {
        let claude = decoded(&manifest(&built(), &status(true, false)));
        assert_eq!(claude.metadata.name, "worker");
        let kinds: Vec<_> = claude
            .spec
            .harnesses
            .iter()
            .map(|harness| (harness.kind, harness.optional))
            .collect();
        assert_eq!(
            kinds,
            [(agent::Harness::ClaudeCode, false), (agent::Harness::Codex, true)]
        );
        assert_eq!(
            claude.spec.default_harness().map(|harness| harness.kind),
            Some(agent::Harness::ClaudeCode)
        );
        assert!(claude.spec.ssh_access());

        let codex = decoded(&manifest(&built(), &status(false, true)));
        assert_eq!(
            codex.spec.default_harness().map(|harness| harness.kind),
            Some(agent::Harness::Codex)
        );
        assert!(
            codex
                .spec
                .harness(agent::Harness::ClaudeCode)
                .is_some_and(|claude| claude.optional)
        );

        let both = decoded(&manifest(&built(), &status(true, true)));
        assert!(both.spec.harnesses.iter().all(|harness| !harness.optional));
    }

    #[test]
    fn the_github_owner_is_named_only_when_a_token_applies() {
        let owned = decoded(&manifest(
            &choices(Github::Owner("digdir".into()), Image::Built, Checkout::Mount),
            &status(true, false),
        ));
        assert_eq!(owned.spec.github_owner(), Some("digdir"));
        let only = decoded(&manifest(
            &choices(Github::Only, Image::Built, Checkout::Mount),
            &status(true, false),
        ));
        assert!(only.spec.github_account());
        assert_eq!(only.spec.github_owner(), None);
        let without = manifest(&built(), &status(true, false));
        assert!(!decoded(&without).spec.github_account());
        assert!(without.contains("# accounts:"), "{without}");
    }

    #[test]
    fn the_size_and_a_tmp_scratch_space_are_in_the_manifest() {
        let agent = decoded(&manifest(&built(), &status(true, false)));
        let resources = serde_yaml_ng::to_value(agent.spec.sandbox.resources).expect("resources");
        assert_eq!(resources["cpu"], "2", "{resources:?}");
        assert_eq!(resources["memory"], "4Gi", "{resources:?}");
        assert_eq!(resources["rootFilesystem"]["capacity"], "16Gi", "{resources:?}");
        assert!(matches!(
            agent.spec.sandbox.mounts.as_slice(),
            [agent::MountSpec::Tmpfs { target, .. }] if target.as_str() == "/tmp"
        ));
    }

    #[test]
    fn a_mounted_checkout_is_bound_and_a_cloned_one_offers_the_mount_commented() {
        let target = format!("{}/worker", agent::sandbox::platform::WORKING_DIRECTORY);
        let is_checkout = |mount: &agent::MountSpec| {
            matches!(mount, agent::MountSpec::Bind { source, target: bound, read_only: false }
                if source == Path::new(".") && bound.as_str() == target)
        };
        let mounted = decoded(&manifest(
            &choices(Github::None, Image::Base, Checkout::Mount),
            &status(true, false),
        ));
        assert!(matches!(mounted.spec.sandbox.mounts.as_slice(), [checkout, _tmp] if is_checkout(checkout)));

        let cloned = manifest(&built(), &status(true, false));
        assert!(cloned.contains("clones digdir/worker"), "{cloned}");
        let uncommented = cloned
            .replace("      # - type: bind", "      - type: bind")
            .replace("      #   ", "        ");
        assert!(
            matches!(decoded(&uncommented).spec.sandbox.mounts.as_slice(), [checkout, _tmp] if is_checkout(checkout))
        );
    }

    #[test]
    fn the_image_is_the_base_or_built_from_the_repositorys_dockerfile() {
        let base = decoded(&manifest(
            &choices(Github::None, Image::Base, Checkout::Mount),
            &status(true, false),
        ));
        let image = serde_yaml_ng::to_value(&base.spec.sandbox.image).expect("image");
        assert_eq!(image["reference"], BASE_IMAGE, "{image:?}");

        let built = decoded(&manifest(&built(), &status(true, false)));
        let image = serde_yaml_ng::to_value(&built.spec.sandbox.image).expect("image");
        assert_eq!(image["type"], "build", "{image:?}");
        assert_eq!(image["context"], IMAGE_DIRECTORY, "{image:?}");
        assert_eq!(image["dockerfile"], "Dockerfile", "{image:?}");
    }

    #[test]
    fn the_dockerfile_clones_the_repository_and_installs_each_toolchain() {
        let written = dockerfile(
            &Checkout::Clone("digdir/worker".into()),
            &[
                Toolchain::Node {
                    version: Some("20".into()),
                    corepack: true,
                },
                Toolchain::Dotnet {
                    sdk: Some("10.0.302".into()),
                },
                Toolchain::Go { version: None },
                Toolchain::Rust { channel: None },
                Toolchain::Python,
                Toolchain::Java { maven: true },
            ],
        );
        for expected in [
            &format!("\nFROM {BASE_IMAGE}\n") as &str,
            "RUN printf '%s\\n' 'digdir/worker' >/etc/agent/repositories\n",
            "ARG NODE_RELEASE=release/latest-v20.x\n",
            "corepack enable",
            "--version 10.0.302",
            "ARG GO_VERSION=\n",
            "ARG RUST_TOOLCHAIN=stable\n",
            "python3-venv",
            "openjdk-21-jdk-headless maven",
        ] {
            assert!(written.contains(expected), "{expected} in {written}");
        }
        assert!(
            written
                .trim_end()
                .ends_with(&format!("WORKDIR {}", agent::sandbox::platform::WORKING_DIRECTORY)),
            "{written}"
        );

        let mounted = dockerfile(&Checkout::Mount, &[]);
        assert!(mounted.contains("# RUN printf"), "nothing is cloned: {mounted}");
        assert!(!mounted.contains("NODE_RELEASE"), "{mounted}");
    }

    #[test]
    fn every_dockerfile_line_starts_an_instruction_or_continues_one() {
        let all = [
            Toolchain::Node {
                version: Some("20.11.1".into()),
                corepack: true,
            },
            Toolchain::Dotnet { sdk: None },
            Toolchain::Go {
                version: Some("1.26".into()),
            },
            Toolchain::Rust {
                channel: Some("1.97.1".into()),
            },
            Toolchain::Python,
            Toolchain::Java { maven: false },
        ];
        let written = dockerfile(&Checkout::Clone("digdir/worker".into()), &all);
        let mut continued = false;
        for line in written.lines() {
            let trimmed = line.trim_start();
            if !continued && !trimmed.is_empty() && !trimmed.starts_with('#') {
                let instruction = trimmed.split_whitespace().next().unwrap_or_default();
                assert!(
                    ["FROM", "USER", "ARG", "ENV", "RUN", "WORKDIR"].contains(&instruction),
                    "{line:?} neither starts an instruction nor continues one in\n{written}"
                );
            }
            if !trimmed.starts_with('#') {
                continued = line.ends_with('\\');
            }
        }
        assert!(!continued, "the last instruction is complete");
    }

    #[test]
    fn an_exact_node_version_installs_that_release() {
        let block = node(Some("20.11.1"), false);
        assert!(block.contains("ARG NODE_RELEASE=release/v20.11.1\n"), "{block}");
        assert!(!block.contains("corepack"), "{block}");
    }
}
