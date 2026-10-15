//! Tokens from the GitHub CLI: the host's own login, or a browser sign-in kept apart from it.

use zeroize::Zeroizing;

use crate::Error;

/// The GitHub CLI, which owns the browser sign-in so agentctl needs no OAuth app of its own.
const GH: &str = "gh";
/// The part of [`super::SCOPES`] the GitHub CLI does not request by default; its defaults are
/// `repo`, `read:org` and `gist`.
const ADDITIONAL_SCOPES: &str = "workflow,read:project";
/// Environment that would make `gh` use another token than the one its configuration holds.
const GH_TOKEN_ENVIRONMENT: [&str; 5] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
];

/// Reads the token of the host's active `gh` login, for an explicit `--from-gh` import.
///
/// # Errors
///
/// Returns an error when `gh` is missing, has no login, or prints no token.
pub fn token_from_gh() -> Result<Zeroizing<String>, Error> {
    gh_token(std::path::Path::new(GH), None)
}

/// The login of the host's own `gh` account, when the GitHub CLI is installed and signed in.
#[must_use]
pub fn host_login() -> Option<String> {
    let output = std::process::Command::new(GH)
        .args(["api", "user", "--jq", ".login"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let login = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !login.is_empty()).then_some(login)
}

/// Signs in through the GitHub CLI's own browser flow and returns the new token, or `None` when
/// the GitHub CLI is not installed.
///
/// `gh` runs against a throwaway configuration directory and without any token in its
/// environment, so the host's own `gh` login is neither used nor changed, and the directory
/// holding the new token is deleted once it has been read.
///
/// # Errors
///
/// Returns an error when the sign-in does not complete or yields no token.
pub fn token_from_gh_sign_in() -> Result<Option<Zeroizing<String>>, Error> {
    sign_in_with(std::path::Path::new(GH))
}

fn sign_in_with(program: &std::path::Path) -> Result<Option<Zeroizing<String>>, Error> {
    let configuration = tempfile::TempDir::new()?;
    let mut login = std::process::Command::new(program);
    login.args([
        "auth",
        "login",
        "--hostname",
        "github.com",
        "--web",
        // With `https`, gh offers to configure Git's credential helper, which would point the
        // host's Git at this throwaway login. The protocol choice itself is discarded with it.
        "--git-protocol",
        "ssh",
        "--skip-ssh-key",
        // Plain text in the throwaway directory, never the host's credential store.
        "--insecure-storage",
        "--scopes",
        ADDITIONAL_SCOPES,
    ]);
    isolate(&mut login, configuration.path());
    let status = match login.status() {
        Ok(status) => status,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Invalid(format!("could not run `gh auth login`: {error}")));
        }
    };
    if !status.success() {
        return Err(Error::Invalid("the GitHub CLI sign-in did not complete".into()));
    }
    gh_token(program, Some(configuration.path())).map(Some)
}

fn isolate(command: &mut std::process::Command, configuration: &std::path::Path) {
    command.env("GH_CONFIG_DIR", configuration);
    // Whatever gh would write to the global Git configuration stays in the throwaway directory.
    command.env("GIT_CONFIG_GLOBAL", configuration.join("gitconfig"));
    for name in GH_TOKEN_ENVIRONMENT {
        command.env_remove(name);
    }
}

/// Runs `gh auth token`, against `configuration` when given and the host's own login otherwise.
fn gh_token(program: &std::path::Path, configuration: Option<&std::path::Path>) -> Result<Zeroizing<String>, Error> {
    let mut command = std::process::Command::new(program);
    command
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    if let Some(configuration) = configuration {
        isolate(&mut command, configuration);
    }
    let output = command.output().map_err(|error| {
        Error::Invalid(format!(
            "could not run `gh auth token` (is the GitHub CLI installed?): {error}"
        ))
    })?;
    if !output.status.success() {
        return Err(Error::Invalid(
            "`gh auth token` failed; run `gh auth login` first".into(),
        ));
    }
    let token = Zeroizing::new(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    if token.is_empty() {
        return Err(Error::Invalid("`gh auth token` printed no token".into()));
    }
    Ok(token)
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]

    use tempfile::TempDir;

    use super::*;

    /// A stand-in `gh` that records how it was run beside itself and keeps its token in
    /// `GH_CONFIG_DIR`, as the real one does with `--insecure-storage`.
    fn fake_gh(directory: &TempDir, login_exit: i32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let program = directory.path().join("gh");
        let log = directory.path().join("log");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\n\
                 echo \"$1 $2 config=$GH_CONFIG_DIR git=$GIT_CONFIG_GLOBAL args=$*\" >> '{log}'\n\
                 case \"$1 $2\" in\n\
                 'auth login') [ {login_exit} -eq 0 ] || exit {login_exit}; echo gho_isolated > \"$GH_CONFIG_DIR/token\" ;;\n\
                 'auth token') cat \"$GH_CONFIG_DIR/token\" ;;\n\
                 esac\n",
                log = log.display()
            ),
        )
        .expect("fake gh");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("executable");
        program
    }

    #[test]
    fn the_gh_sign_in_runs_in_a_throwaway_configuration_that_is_deleted_afterwards() {
        let directory = TempDir::new().expect("temporary directory");
        let program = fake_gh(&directory, 0);

        let token = sign_in_with(&program).expect("sign-in").expect("gh is installed");

        assert_eq!(token.as_str(), "gho_isolated");
        let log = std::fs::read_to_string(directory.path().join("log")).expect("log");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].starts_with("auth login "), "{log}");
        for flag in [
            "--web",
            "--git-protocol ssh",
            "--skip-ssh-key",
            "--insecure-storage",
            "--scopes workflow,read:project",
            "--hostname github.com",
        ] {
            assert!(lines[0].contains(flag), "{flag} in {log}");
        }
        assert!(lines[1].starts_with("auth token "), "{log}");
        let configuration = lines[0]
            .split("config=")
            .nth(1)
            .and_then(|rest| rest.split(" git=").next())
            .expect("configuration directory");
        assert!(
            lines[1].contains(&format!("config={configuration} ")),
            "both runs share it: {log}"
        );
        assert!(
            lines
                .iter()
                .all(|line| line.contains(&format!("git={configuration}/gitconfig "))),
            "gh cannot write the host's Git configuration: {log}"
        );
        assert!(
            !std::path::Path::new(configuration).exists(),
            "the token's directory is deleted"
        );
    }

    #[test]
    fn a_missing_gh_falls_back_and_an_abandoned_sign_in_is_an_error() {
        let directory = TempDir::new().expect("temporary directory");
        assert!(
            sign_in_with(&directory.path().join("missing"))
                .expect("a missing gh is not an error")
                .is_none()
        );

        let program = fake_gh(&directory, 1);
        sign_in_with(&program).expect_err("an abandoned sign-in");
    }
}
