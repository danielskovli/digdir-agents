# agentctl base images (Ubuntu)

General Agent images, in four stages of one Dockerfile, each adding to the one before:

| Stage | Adds |
| --- | --- |
| `basic` | What the Agent platform expects of a Linux image: systemd as init, OpenSSH (agentd configures and starts it) and the `agent` user with passwordless sudo. Claude Code, Codex and the Node.js they run on; Git with the GitHub CLI as its credential helper for `https://github.com`; build-essential, curl, jq, ripgrep, tmux, neovim and Python 3 |
| `developer` | Podman, with `docker` and `docker compose` working through it; agentd enables its socket |
| `web` | A headless Chromium through Playwright and playwright-cli, trusting the Agent's CA, with ffmpeg |
| `desktop` | A graphical desktop with that browser, opened by `access: [{type: vnc}]` |

At first boot every stage clones each repository listed in `/etc/agent/repositories` into `/home/agent/code`, one per
line: `owner/name` for GitHub, or any URL Git can clone. A checkout that already has `.git`, such as a mounted one, is
left alone.

`agentctl init` copies the stages up to the one an agent needs into the agent's own Dockerfile, with the files they
copy from `common/`, and adds a last stage with the repository's toolchains. The `basic` stage is also published as
`ghcr.io/digdir/digdir-agents/agentctl-base-ubuntu`, to build on by hand:

```dockerfile
FROM ghcr.io/digdir/digdir-agents/agentctl-base-ubuntu:latest
USER root
RUN printf '%s\n' 'digdir/digdir-agents' >/etc/agent/repositories
RUN apt-get update && apt-get install --yes --no-install-recommends postgresql-client && rm -rf /var/lib/apt/lists/*
USER agent
WORKDIR /home/agent/code
```

Pull requests build every stage; `main` publishes `basic`. The browser and desktop files in `common/` started as copies
of `agents/self-dev`'s.
