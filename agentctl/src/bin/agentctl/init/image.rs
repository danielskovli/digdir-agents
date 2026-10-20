//! The base images a new agent's Dockerfile starts from, chosen by what the agent should be able
//! to do: the published `agentctl-base` images (`images/agentctl-base`), plus Podman for
//! containers.

use super::sizing::Size;

/// The headless base image: the platform's Linux contract, the harnesses, Git and the GitHub CLI.
const HEADLESS: &str = "ghcr.io/digdir/agentctl-base:ubuntu-26.04";
/// The headless image plus Chromium with playwright-cli and a desktop for VNC access.
const DESKTOP: &str = "ghcr.io/digdir/agentctl-base:ubuntu-26.04-desktop";

/// What an agent's image can do; each tier adds to the one before.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Tier {
    /// The harnesses, Git and the GitHub CLI.
    Basic,
    /// Also containers, through Podman with a Docker-compatible CLI and Compose.
    Developer,
    /// Also a browser for testing web applications.
    Web,
    /// Also a graphical desktop, opened over VNC.
    Desktop,
}

impl Tier {
    pub(super) const ALL: [Self; 4] = [Self::Basic, Self::Developer, Self::Web, Self::Desktop];

    /// What a person chooses, in their words.
    pub(super) const fn describe(self) -> &'static str {
        match self {
            Self::Basic => "Write code: Claude Code, Codex, Git and GitHub",
            Self::Developer => "Also build and run things in containers",
            Self::Web => "Also test web apps in a browser",
            Self::Desktop => "Also use a desktop you can watch and take over",
        }
    }

    /// Whether the agent's own stage installs Podman.
    pub(super) const fn has_containers(self) -> bool {
        !matches!(self, Self::Basic)
    }

    /// The most a size recommendation for this tier asks for.
    pub(super) const fn most(self) -> Size {
        match self {
            Self::Basic => Size {
                cpus: 2,
                memory_gib: 4,
                disk_gib: 32,
            },
            Self::Developer | Self::Web | Self::Desktop => Size {
                cpus: 4,
                memory_gib: 8,
                disk_gib: 64,
            },
        }
    }

    /// Whether the agent opens the image's desktop with `access: [{type: vnc}]`.
    pub(super) const fn has_desktop(self) -> bool {
        matches!(self, Self::Desktop)
    }

    /// Whether the image has a browser, whose shared memory lands in `/tmp`.
    pub(super) const fn has_browser(self) -> bool {
        matches!(self, Self::Web | Self::Desktop)
    }
}

/// The base image for an agent with or without a browser. The browser comes only with the desktop
/// image, so an agent that tests web apps runs it without opening the desktop.
pub(crate) const fn base_image(browser: bool) -> &'static str {
    if browser { DESKTOP } else { HEADLESS }
}

/// Podman for containers, with `docker` and `docker compose` working through it. agentd enables
/// the Podman socket and grants it to the agent user when the image has Podman.
pub(crate) const CONTAINERS: &str = r"# Containers for builds and services: Podman, with `docker` and `docker compose` working through it.
# netavark invokes nft for bridge networking and aardvark-dns resolves container names between
# compose services.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        aardvark-dns \
        build-essential \
        docker-compose-v2 \
        nftables \
        podman \
        podman-docker \
    && rm -rf /var/lib/apt/lists/* \
    && touch /etc/containers/nodocker \
    && podman --version \
    && /usr/libexec/docker/cli-plugins/docker-compose version
ENV DOCKER_HOST=unix:///run/podman/podman.sock
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_map_onto_the_two_base_images() {
        assert_eq!(base_image(Tier::Basic.has_browser()), HEADLESS);
        assert_eq!(base_image(Tier::Developer.has_browser()), HEADLESS);
        assert_eq!(base_image(Tier::Web.has_browser()), DESKTOP);
        assert_eq!(base_image(Tier::Desktop.has_browser()), DESKTOP);
        assert!(!Tier::Basic.has_containers() && Tier::Developer.has_containers());
        assert!(Tier::Desktop.has_desktop() && !Tier::Web.has_desktop());
    }
}
