//! The general image stages a new agent's Dockerfile is made of: `agents/base-ubuntu`'s
//! Dockerfile, cut after the stage the agent needs, and the files those stages copy.

use super::sizing::Size;

/// `agents/base-ubuntu/Dockerfile`, whose stages each add to the one before.
const BASE_DOCKERFILE: &str = include_str!("ubuntu/Dockerfile");
/// The directory, beside the Dockerfile, that its stages copy files from.
pub(super) const SUPPORT_DIRECTORY: &str = "common";
/// Every file in `agents/base-ubuntu/common`, by its path there.
const SUPPORT_FILES: &[(&str, &str)] = &[
    (
        "access-vnc.conf",
        include_str!("ubuntu/common/access-vnc.conf"),
    ),
    (
        "bin/chromium",
        include_str!("ubuntu/common/bin/chromium"),
    ),
    (
        "bin/desktop",
        include_str!("ubuntu/common/bin/desktop"),
    ),
    (
        "bin/desktop-terminal",
        include_str!("ubuntu/common/bin/desktop-terminal"),
    ),
    (
        "browser-ca-init.service",
        include_str!("ubuntu/common/browser-ca-init.service"),
    ),
    (
        "browser-ca-init.sh",
        include_str!("ubuntu/common/browser-ca-init.sh"),
    ),
    (
        "browser-open.sh",
        include_str!("ubuntu/common/browser-open.sh"),
    ),
    (
        "chromium.desktop",
        include_str!("ubuntu/common/chromium.desktop"),
    ),
    (
        "desktop-dbus.service",
        include_str!("ubuntu/common/desktop-dbus.service"),
    ),
    (
        "desktop-display.service",
        include_str!("ubuntu/common/desktop-display.service"),
    ),
    (
        "desktop-openbox-rc.xml",
        include_str!("ubuntu/common/desktop-openbox-rc.xml"),
    ),
    (
        "desktop-session.service",
        include_str!("ubuntu/common/desktop-session.service"),
    ),
    (
        "desktop-session.sh",
        include_str!("ubuntu/common/desktop-session.sh"),
    ),
    (
        "desktop-terminal.desktop",
        include_str!("ubuntu/common/desktop-terminal.desktop"),
    ),
    (
        "desktop-tint2rc",
        include_str!("ubuntu/common/desktop-tint2rc"),
    ),
    (
        "desktop-tree.py",
        include_str!("ubuntu/common/desktop-tree.py"),
    ),
    (
        "desktop.conf",
        include_str!("ubuntu/common/desktop.conf"),
    ),
    (
        "full-tmpfiles.conf",
        include_str!("ubuntu/common/full-tmpfiles.conf"),
    ),
    (
        "mimeapps.list",
        include_str!("ubuntu/common/mimeapps.list"),
    ),
    (
        "novnc-service.js",
        include_str!("ubuntu/common/novnc-service.js"),
    ),
    (
        "nvim-sysinit.vim",
        include_str!("ubuntu/common/nvim-sysinit.vim"),
    ),
    (
        "playwright-chromium.sh",
        include_str!("ubuntu/common/playwright-chromium.sh"),
    ),
    (
        "tmpfiles.conf",
        include_str!("ubuntu/common/tmpfiles.conf"),
    ),
    (
        "vnc-web.service",
        include_str!("ubuntu/common/vnc-web.service"),
    ),
    (
        "vnc.service",
        include_str!("ubuntu/common/vnc.service"),
    ),
    (
        "vnc.socket",
        include_str!("ubuntu/common/vnc.socket"),
    ),
    (
        "workspace-init.service",
        include_str!("ubuntu/common/workspace-init.service"),
    ),
    (
        "workspace-init.sh",
        include_str!("ubuntu/common/workspace-init.sh"),
    ),
];

/// What an agent's image can do; each tier adds to the one before.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Tier {
    /// The harnesses, Git and the GitHub CLI.
    Basic,
    /// Also containers, through Podman with a Docker-compatible CLI and Compose.
    Developer,
    /// Also a headless browser for testing web applications.
    Web,
    /// Also a graphical desktop, opened over VNC.
    Desktop,
}

impl Tier {
    pub(super) const ALL: [Self; 4] = [Self::Basic, Self::Developer, Self::Web, Self::Desktop];

    /// The base Dockerfile's stage for this tier.
    pub(super) const fn stage(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Developer => "developer",
            Self::Web => "web",
            Self::Desktop => "desktop",
        }
    }

    /// What a person chooses, in their words.
    pub(super) const fn describe(self) -> &'static str {
        match self {
            Self::Basic => "Write code: Claude Code, Codex, Git and GitHub",
            Self::Developer => "Also build and run things in containers",
            Self::Web => "Also test web apps in a browser",
            Self::Desktop => "Also use a desktop you can watch and take over",
        }
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

    /// Whether the image offers a desktop for `access: [{type: vnc}]`.
    pub(super) const fn has_desktop(self) -> bool {
        matches!(self, Self::Desktop)
    }

    /// Whether the image has a browser, whose shared memory lands in `/tmp`.
    pub(super) const fn has_browser(self) -> bool {
        matches!(self, Self::Web | Self::Desktop)
    }
}

/// The base Dockerfile's stages up to and including `tier`'s, each with the comment above it.
pub(super) fn stages(tier: Tier) -> String {
    let lines: Vec<&str> = BASE_DOCKERFILE.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("FROM "))
        .map(|(index, _)| {
            // A stage owns the comment lines directly above its FROM.
            let mut start = index;
            while start > 0 && lines[start - 1].starts_with('#') {
                start -= 1;
            }
            start
        })
        .collect();
    let last = Tier::ALL.iter().position(|candidate| *candidate == tier).unwrap_or(0);
    let end = starts.get(last + 1).copied().unwrap_or(lines.len());
    let first = starts.first().copied().unwrap_or(0);
    let mut text = lines[first..end].join("\n");
    text.truncate(text.trim_end().len());
    text.push('\n');
    text
}

/// The support files `dockerfile` copies, by their path under [`SUPPORT_DIRECTORY`].
pub(super) fn support_files(dockerfile: &str) -> Vec<(&'static str, &'static str)> {
    let prefix = format!("{SUPPORT_DIRECTORY}/");
    let copied: Vec<&str> = dockerfile
        .lines()
        .filter(|line| line.starts_with("COPY "))
        .flat_map(str::split_whitespace)
        .filter_map(|word| word.strip_prefix(prefix.as_str()))
        .collect();
    SUPPORT_FILES
        .iter()
        .filter(|(path, _)| copied.contains(path))
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn the_embedded_support_files_are_exactly_the_base_images() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/agentctl/init/ubuntu/common");
        let mut on_disk = Vec::new();
        let mut directories = vec![root.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory).expect("support directory") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    directories.push(path);
                } else {
                    let relative = path.strip_prefix(&root).expect("relative");
                    on_disk.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        on_disk.sort();
        let mut embedded: Vec<String> = SUPPORT_FILES.iter().map(|(path, _)| (*path).to_owned()).collect();
        embedded.sort();
        assert_eq!(
            embedded, on_disk,
            "SUPPORT_FILES must list every file in agents/base-ubuntu/common"
        );
    }

    #[test]
    fn each_tier_takes_the_stages_up_to_its_own() {
        let from_lines = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|line| line.starts_with("FROM "))
                .map(|line| line.rsplit(' ').next().unwrap_or_default().to_owned())
                .collect()
        };
        assert_eq!(from_lines(&stages(Tier::Basic)), ["basic"]);
        assert_eq!(from_lines(&stages(Tier::Developer)), ["basic", "developer"]);
        assert_eq!(from_lines(&stages(Tier::Web)), ["basic", "developer", "web"]);
        assert_eq!(
            from_lines(&stages(Tier::Desktop)),
            ["basic", "developer", "web", "desktop"]
        );
        assert!(
            stages(Tier::Developer).contains("# Containers for builds and services"),
            "a stage keeps the comment above it"
        );
        assert!(
            !stages(Tier::Developer).contains("A headless browser"),
            "and not the next stage's"
        );
        assert!(
            !stages(Tier::Basic).starts_with("# General Agent images"),
            "the base file's own header is left out"
        );
    }

    #[test]
    fn a_tier_brings_only_the_files_its_stages_copy() {
        let names =
            |tier: Tier| -> Vec<&str> { support_files(&stages(tier)).into_iter().map(|(path, _)| path).collect() };
        let basic = names(Tier::Basic);
        assert!(
            basic.contains(&"workspace-init.sh") && basic.contains(&"tmpfiles.conf"),
            "{basic:?}"
        );
        assert!(
            !basic
                .iter()
                .any(|path| path.contains("vnc") || path.contains("browser")),
            "{basic:?}"
        );
        let web = names(Tier::Web);
        assert!(
            web.contains(&"browser-ca-init.sh") && web.contains(&"bin/chromium"),
            "{web:?}"
        );
        assert!(!web.iter().any(|path| path.contains("vnc")), "{web:?}");
        let desktop = names(Tier::Desktop);
        assert!(
            desktop.contains(&"vnc.socket")
                && desktop.contains(&"bin/desktop")
                && desktop.contains(&"bin/desktop-terminal"),
            "{desktop:?}"
        );
        // Every file the desktop stage copies is embedded, so a generated build never misses one.
        let copied = stages(Tier::Desktop)
            .lines()
            .filter(|line| line.starts_with("COPY "))
            .flat_map(str::split_whitespace)
            .filter(|word| word.starts_with("common/"))
            .count();
        assert_eq!(copied, desktop.len(), "{desktop:?}");
    }
}
