# agentctl base image (Ubuntu)

A general starting point for Agent images, published as `ghcr.io/digdir/digdir-agents/agentctl-base-ubuntu`. It has
what the Agent platform expects of a Linux image and nothing specific to one project:

- systemd as init, OpenSSH (agentd configures and starts it), and the `agent` user with passwordless sudo;
- Claude Code, Codex and the Node.js they run on;
- Git, the GitHub CLI as Git's credential helper for `https://github.com`, and build-essential, curl, jq, ripgrep, tmux,
  neovim and Python 3.

At first boot it clones each repository listed in `/etc/agent/repositories` into `/home/agent/code`, one per line:
`owner/name` for GitHub, or any URL Git can clone. A checkout that already has `.git`, such as a mounted one, is left
alone.

`agentctl init` writes a Dockerfile that starts `FROM` this image, lists the repository and adds its toolchains. To
build one by hand:

```dockerfile
FROM ghcr.io/digdir/digdir-agents/agentctl-base-ubuntu:latest
USER root
RUN printf '%s\n' 'digdir/digdir-agents' >/etc/agent/repositories
RUN apt-get update && apt-get install --yes --no-install-recommends postgresql-client && rm -rf /var/lib/apt/lists/*
USER agent
WORKDIR /home/agent/code
```

The image is built from this directory and published whenever it changes on `main`.
