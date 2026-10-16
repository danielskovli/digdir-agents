FROM ubuntu:26.04@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b

# A general starting point for Agent images: what the Agent platform expects of a Linux image,
# both harnesses, Git and the GitHub CLI, and nothing specific to one project. `agentctl init`
# writes Dockerfiles that start FROM this image and add a repository's own toolchains.

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
    && install -d -m 0755 -o agent -g agent /home/agent/code /home/agent/.local/bin \
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
