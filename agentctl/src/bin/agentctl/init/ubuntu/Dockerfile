# General Agent images in four stages, each adding to the one before:
#
#   basic      what the Agent platform expects of a Linux image, both harnesses, Git and the GitHub
#              CLI; published as agentctl-base-ubuntu
#   developer  Podman with a Docker-compatible CLI and Compose, for builds and services in containers
#   web        a headless Chromium through Playwright, for testing web applications
#   desktop    a graphical desktop with that browser, which `access: [{type: vnc}]` opens
#
# `agentctl init` copies the stages up to the one an agent needs into the agent's own Dockerfile,
# with the files they copy from common/, and adds the repository's toolchains in a last stage.
#
# TODO: Nothing pulls the published image yet. .github/workflows/agentctl-base-image.yml publishes
# `basic` from upstream main only, so until that first run there is nothing to pull, and Dockerfiles
# that `agentctl init` writes carry these stages themselves. Once it is published, verify pulling it
# end to end, then make init's Dockerfiles start FROM the published image.

FROM ubuntu:26.04@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b AS basic

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        iproute2 \
        jq \
        less \
        neovim \
        openssh-server \
        procps \
        python3 \
        ripgrep \
        sudo \
        systemd \
        tmux \
        unzip \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

COPY common/nvim-sysinit.vim /etc/xdg/nvim/sysinit.vim

ARG TARGETARCH
ARG GH_VERSION=2.100.0
# The harnesses run on Node, so the image carries one; a repository that needs another version
# replaces it in its own Dockerfile.
ARG NODE_VERSION=22.23.2

RUN case "${TARGETARCH}" in \
        amd64) NODE_ARCH=x64; \
            GH_SHA256=e4d4bb4498e8d007abe545b6568926793ace1b6447da598294a610018cb164be ;; \
        arm64) NODE_ARCH=arm64; \
            GH_SHA256=ea4e7a581a32ccad6cc7923cb1576ac5859ba4b9a16ab22eb8f8a96e78e2e961 ;; \
        *) echo "unsupported target architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && NODE_ARCHIVE="node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
    && NODE_SHA256="$(curl -fsSL "https://nodejs.org/download/release/v${NODE_VERSION}/SHASUMS256.txt" \
        | awk -v filename="${NODE_ARCHIVE}" '$2 == filename { print $1 }')" \
    && test -n "${NODE_SHA256}" \
    && curl -fsSL "https://nodejs.org/download/release/v${NODE_VERSION}/${NODE_ARCHIVE}" -o /tmp/node.tar.xz \
    && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
    && tar -xJf /tmp/node.tar.xz --strip-components=1 -C /usr/local \
    && GH_ARCHIVE="gh_${GH_VERSION}_linux_${TARGETARCH}.tar.gz" \
    && curl -fsSL "https://github.com/cli/cli/releases/download/v${GH_VERSION}/${GH_ARCHIVE}" -o /tmp/gh.tar.gz \
    && echo "${GH_SHA256}  /tmp/gh.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/gh.tar.gz -C /tmp \
    && install -m 0755 "/tmp/gh_${GH_VERSION}_linux_${TARGETARCH}/bin/gh" /usr/local/bin/gh \
    && rm -rf /tmp/node.tar.xz /tmp/gh.tar.gz "/tmp/gh_${GH_VERSION}_linux_${TARGETARCH}" \
    && node --version \
    && gh --version

# Route GitHub credentials through the gh CLI so plain git commands authenticate with the
# host-mediated token.
RUN git config --system credential.https://github.com.helper '!/usr/local/bin/gh auth git-credential'

ARG CODEX_VERSION=0.162.1
ARG CLAUDE_CODE_VERSION=2.1.296

# Node ignores the system trust store, so each install selects it to trust network mediation.
RUN NODE_OPTIONS=--use-openssl-ca npm install --global "@openai/codex@${CODEX_VERSION}" \
    && NODE_OPTIONS=--use-openssl-ca npm install --global --allow-scripts=@anthropic-ai/claude-code \
        "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
    && claude --version \
    && codex --version

# The platform expects the `agent` user with passwordless sudo. OpenSSH rejects even public-key
# authentication for a locked account when PAM is disabled; password and keyboard authentication
# remain disabled by the platform-owned SSH policy.
RUN usermod --login agent --home /home/agent --move-home ubuntu \
    && passwd --delete agent \
    && groupmod --new-name agent ubuntu \
    && printf 'agent ALL=(ALL) NOPASSWD:ALL\n' >/etc/sudoers.d/agent \
    && chmod 0440 /etc/sudoers.d/agent \
    && install -d -m 0755 -o agent -g agent /home/agent/code /home/agent/.local /home/agent/.local/bin \
    && install -d -m 0755 /etc/agent \
    && touch /etc/agent/repositories \
    && rm -f /etc/machine-id /var/lib/dbus/machine-id \
    && touch /etc/machine-id

COPY common/tmpfiles.conf /usr/lib/tmpfiles.d/agent-base.conf
COPY common/workspace-init.service /etc/systemd/system/agent-workspace-init.service
COPY --chmod=0755 common/workspace-init.sh /usr/local/libexec/agent-workspace-init

# agentd owns the loopback SSH server policy, unit and per-Agent state. The distribution's own
# ssh.service and socket are masked so the image cannot expose its package-generated host keys.
RUN systemctl mask ssh.service ssh.socket \
    && rm -f /etc/ssh/ssh_host_*_key /etc/ssh/ssh_host_*_key.pub

# All guest Executions inherit init.scope, whose default task limit is too low for concurrent
# harnesses; the kernel pid limit remains the Sandbox boundary. The inotify defaults suit one
# desktop, not harnesses, dev servers and test hosts watching files at once. The guest has no udev
# to activate hvc0's device unit, so its generated serial login would otherwise wait 90 seconds at
# boot.
RUN install -d /etc/systemd/system/init.scope.d \
    && printf '[Scope]\nTasksMax=infinity\n' >/etc/systemd/system/init.scope.d/50-agent-tasks.conf \
    && install -d /usr/lib/sysctl.d \
    && printf 'fs.inotify.max_user_instances = 1024\nfs.inotify.max_user_watches = 1048576\nfs.inotify.max_queued_events = 32768\n' \
        >/usr/lib/sysctl.d/50-agent-inotify.conf \
    && systemctl mask serial-getty@hvc0.service \
    && systemctl enable agent-workspace-init.service

ENV DISABLE_AUTOUPDATER=1
ENV GH_NO_UPDATE_NOTIFIER=1
ENV HOME=/home/agent
ENV PATH=/home/agent/.local/bin:${PATH}

USER agent
WORKDIR /home/agent/code

ENTRYPOINT ["/usr/lib/systemd/systemd"]

# Containers for builds and services: Podman, with `docker` and `docker compose` working through
# it. agentd enables the Podman socket and grants it to the agent user when the image has Podman.
FROM basic AS developer
USER root
# netavark invokes nft for bridge networking and aardvark-dns resolves container names between
# compose services.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        aardvark-dns \
        docker-compose-v2 \
        nftables \
        podman \
        podman-docker \
        shellcheck \
        sqlite3 \
        zip \
    && rm -rf /var/lib/apt/lists/* \
    && touch /etc/containers/nodocker \
    && podman --version \
    && /usr/libexec/docker/cli-plugins/docker-compose version
# `docker` and Docker Engine API clients reach Podman through the socket agentd enables.
ENV DOCKER_HOST=unix:///run/podman/podman.sock
USER agent

# A headless browser for testing web applications: Playwright's Chromium, which playwright-cli
# drives, trusting the Agent's CA so mediated HTTPS works in it.
FROM developer AS web
USER root
ARG PLAYWRIGHT_CLI_VERSION=0.1.20
# Standalone scripts resolve the globally installed `playwright` package from any directory.
ENV NODE_PATH=/usr/local/lib/node_modules
ENV PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright
# The browser's CA import marks itself done in the per-user runtime directory. Nothing else creates
# one, since systemd-logind owns that job and a Session is a tmux process rather than a login.
ENV XDG_RUNTIME_DIR=/run/user/1000
# ffmpeg converts and inspects browser captures and libnss3-tools imports the Agent CA into Chromium.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ffmpeg \
        fonts-noto-color-emoji \
        fonts-noto-core \
        libnss3-tools \
    && rm -rf /var/lib/apt/lists/* \
    && test "$(id -u agent)" = 1000 \
    && install -d -m 0700 -o agent -g agent "${XDG_RUNTIME_DIR}"
# Playwright ties every package version to one Chromium build. The browser CLI owns the version:
# the `playwright` package is installed at exactly the version the CLI depends on, so the CLI,
# standalone scripts and the single browser under PLAYWRIGHT_BROWSERS_PATH cannot drift apart.
# The bundled skill is installed user-wide where Claude Code and Codex look for skills. npm and
# Playwright run with root's own HOME, so their caches and settings stay out of the agent's.
RUN export HOME=/root \
    && NODE_OPTIONS=--use-openssl-ca npm install --global "@playwright/cli@${PLAYWRIGHT_CLI_VERSION}" \
    && PLAYWRIGHT_VERSION="$(node -p "require('${NODE_PATH}/@playwright/cli/package.json').dependencies.playwright")" \
    && test -n "${PLAYWRIGHT_VERSION}" \
    && NODE_OPTIONS=--use-openssl-ca npm install --global "playwright@${PLAYWRIGHT_VERSION}" \
    && test "$(node -p "require('playwright/package.json').version")" = "${PLAYWRIGHT_VERSION}" \
    && NODE_OPTIONS=--use-openssl-ca playwright install --with-deps chromium \
    && chmod -R a+rX "${PLAYWRIGHT_BROWSERS_PATH}" \
    && playwright --version \
    && playwright-cli --version \
    && for skills in /home/agent/.claude/skills /home/agent/.agents/skills; do \
        mkdir -p "${skills}" \
        && cp -R "${NODE_PATH}/@playwright/cli/skills/playwright-cli" "${skills}/playwright-cli"; \
    done \
    && chown -R agent:agent /home/agent/.claude /home/agent/.agents \
    && rm -rf /var/lib/apt/lists/* /root/.npm /root/.cache \
    && left="$(find /home/agent ! -user agent | head -5)" \
    && if [ -n "${left}" ]; then \
        echo "build left files under /home/agent not owned by agent: ${left}" >&2; exit 1; \
    fi
COPY common/full-tmpfiles.conf /usr/lib/tmpfiles.d/agent-web.conf
COPY common/browser-ca-init.service /etc/systemd/system/agent-browser-ca-init.service
COPY --chmod=0755 common/browser-ca-init.sh /usr/local/libexec/agent-browser-ca-init
COPY --chmod=0755 common/bin/chromium /usr/local/bin/
COPY --chmod=0755 common/playwright-chromium.sh /usr/local/libexec/agent-playwright-chromium
ENV PLAYWRIGHT_MCP_EXECUTABLE_PATH=/usr/local/libexec/agent-playwright-chromium
RUN systemctl enable agent-browser-ca-init.service \
    && AGENT_BROWSER_CA_READY_FILE=/dev/null HOME=/tmp/image-checks chromium --version \
    && /usr/local/libexec/agent-playwright-chromium --headless --version \
    && rm -rf /tmp/image-checks
USER agent

# A graphical desktop with the browser, for work that needs a real screen. The image offers it for
# `access: [{type: vnc}]`; without that access the desktop runs with no port open.
FROM web AS desktop
USER root
ARG NOVNC_VERSION=1.7.0
ARG NOVNC_SHA256=b1003a11b6e6e8d8f7f5e5586daae7f8ca651d8aee0aa155ff9ac841c48f52c6
ARG WS_VERSION=8.21.3
# xdg-utils lets `xdg-open`, and agentctl's open actions, open links on the desktop as on any other.
# The rest is a minimal X11 desktop: one process that is both X and VNC server, a window manager, a
# panel, the input and window tools the `desktop` helper uses, AT-SPI for `desktop tree`, and sakura
# as the terminal, whose text AT-SPI exposes too.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        at-spi2-core \
        dbus-x11 \
        openbox \
        python3-pyatspi \
        sakura \
        tigervnc-standalone-server \
        tint2 \
        x11-utils \
        x11-xserver-utils \
        xdg-utils \
        xdotool \
    && rm -rf /var/lib/apt/lists/*
# The browser viewer: noVNC, pinned by digest and trimmed to the files it serves, and `ws` for its
# WebSocket bridge. Neither is packaged for Ubuntu.
RUN curl -fsSL "https://github.com/novnc/noVNC/archive/refs/tags/v${NOVNC_VERSION}.tar.gz" -o /tmp/novnc.tar.gz \
    && echo "${NOVNC_SHA256}  /tmp/novnc.tar.gz" | sha256sum -c - \
    && install -d -m 0755 /usr/local/share/novnc \
    && tar -xzf /tmp/novnc.tar.gz -C /usr/local/share/novnc --strip-components=1 \
        "noVNC-${NOVNC_VERSION}/vnc.html" \
        "noVNC-${NOVNC_VERSION}/app" \
        "noVNC-${NOVNC_VERSION}/core" \
        "noVNC-${NOVNC_VERSION}/vendor" \
        "noVNC-${NOVNC_VERSION}/po" \
        "noVNC-${NOVNC_VERSION}/defaults.json" \
        "noVNC-${NOVNC_VERSION}/mandatory.json" \
        "noVNC-${NOVNC_VERSION}/package.json" \
        "noVNC-${NOVNC_VERSION}/LICENSE.txt" \
    && rm -f /tmp/novnc.tar.gz \
    && HOME=/root NODE_OPTIONS=--use-openssl-ca npm install --global "ws@${WS_VERSION}" \
    && node -e "require('ws')" \
    && test -f /usr/local/share/novnc/vnc.html \
    && rm -rf /root/.npm /root/.cache
COPY --chmod=0755 common/novnc-service.js /usr/local/libexec/agent-novnc
# What this image offers for `access: [{type: vnc}]`: units shipped disabled, which agentd enables
# when an Agent declares the capability.
COPY common/vnc.socket /etc/systemd/system/agent-vnc.socket
COPY common/vnc.service /etc/systemd/system/agent-vnc.service
COPY common/vnc-web.service /etc/systemd/system/agent-vnc-web.service
COPY common/access-vnc.conf /etc/agent-access.d/vnc.conf
COPY common/desktop.conf /etc/agent-desktop.conf
COPY common/desktop-display.service /etc/systemd/system/agent-desktop-display.service
COPY common/desktop-session.service /etc/systemd/system/agent-desktop-session.service
COPY common/desktop-dbus.service /etc/systemd/system/agent-desktop-dbus.service
COPY --chmod=0755 common/desktop-tree.py /usr/local/libexec/agent-desktop-tree
COPY common/desktop-openbox-rc.xml /etc/xdg/openbox/rc.xml
COPY common/desktop-tint2rc /etc/xdg/tint2/tint2rc
COPY --chmod=0755 common/desktop-session.sh /usr/local/libexec/agent-desktop-session
COPY --chmod=0755 common/bin/desktop common/bin/desktop-terminal /usr/local/bin/
COPY common/desktop-terminal.desktop /usr/local/share/applications/desktop-terminal.desktop
COPY common/chromium.desktop /usr/local/share/applications/chromium.desktop
COPY --chmod=0755 common/browser-open.sh /usr/local/libexec/agent-browser-open
COPY common/mimeapps.list /etc/xdg/mimeapps.list
# xdotool refuses multi-byte characters such as æ, ø and å under the POSIX locale.
ENV LANG=C.UTF-8
# Every Session uses the Agent's own display. The desktop units read the same value from
# /etc/agent-desktop.conf, and the build below fails if the two disagree.
ENV DISPLAY=:1
# AT-SPI needs a session bus shared by Sessions and desktop programs. A Session is a tmux process
# rather than a login, so agent-desktop-dbus.service provides one at this fixed address.
ENV DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
# Chromium joins the accessibility bus only with ACCESSIBILITY_ENABLED, and fills in its tree only
# with --force-renderer-accessibility, which the `chromium` launcher passes.
ENV ACCESSIBILITY_ENABLED=1
# These checks run as root with a scratch HOME: openbox writes under $HOME/.cache when invoked,
# and a root-owned file there would lock the Agent out of its own cache.
RUN systemctl enable agent-desktop-display.service agent-desktop-session.service agent-desktop-dbus.service \
    && HOME=/tmp/image-checks sh -c ' \
        Xtigervnc -version 2>&1 | head -1 \
        && openbox --version | head -1 \
        && tint2 --version \
        && xdotool --version \
        && command -v sakura >/dev/null \
        && desktop help >/dev/null \
        && test "$(xdg-mime query default x-scheme-handler/https)" = chromium.desktop' \
    && rm -rf /tmp/image-checks \
    && python3 -c 'import pyatspi' \
    && test -f /usr/share/dbus-1/services/org.a11y.Bus.service \
    && test "${DBUS_SESSION_BUS_ADDRESS}" = "unix:path=${XDG_RUNTIME_DIR}/bus" \
    && . /etc/agent-desktop.conf \
    && test "${DISPLAY}" = "${AGENT_DESKTOP_DISPLAY}" \
    && vnc_port="$(sed -n 's/^port=//p' /etc/agent-access.d/vnc.conf)" \
    && web_port="$(sed -n 's/^web-port=//p' /etc/agent-access.d/vnc.conf)" \
    && { grep -qxF "ListenStream=127.0.0.1:${vnc_port}" /etc/systemd/system/agent-vnc.socket \
        || { echo "agent-vnc.socket does not listen on the descriptor's port ${vnc_port}" >&2; exit 1; }; } \
    && { grep -qxF "Environment=AGENT_NOVNC_PORT=${web_port}" /etc/systemd/system/agent-vnc-web.service \
        || { echo "agent-vnc-web.service does not serve the descriptor's web-port ${web_port}" >&2; exit 1; }; } \
    && for unit in agent-vnc.socket agent-vnc.service agent-vnc-web.service; do \
        state="$(systemctl is-enabled "${unit}" 2>/dev/null || true)"; \
        case "${state}" in disabled | static) ;; \
            *) echo "${unit} is ${state}; it must ship disabled" >&2; exit 1 ;; \
        esac; \
    done \
    && left="$(find /home/agent ! -user agent | head -5)" \
    && if [ -n "${left}" ]; then \
        echo "build left files under /home/agent not owned by agent: ${left}" >&2; exit 1; \
    fi
USER agent
