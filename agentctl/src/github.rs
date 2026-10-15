//! The host's GitHub account for Agents.
//!
//! `agentctl github login` stores one GitHub token in the agent stack, independent of any
//! `gh` login on the host, so Agents can act as a separate account such as a bot. An Agent
//! opts in with `spec.accounts: [{type: github}]`; the Sandbox then sees an inert
//! `GITHUB_TOKEN` that the network mediator substitutes only in requests to GitHub's own
//! hosts, and Git commits carry the account's name and email.

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{Error, harness::MediatedSecret, persistence};
use sandbox::secret_store::SecretReference;

const PROVIDER: &str = "github";
const ACCESS_SECRET: &str = "github-access-token";
pub(crate) const ACCESS_ENVIRONMENT: &str = "GITHUB_TOKEN";
/// `gh` attaches files only with a token it classifies by prefix, so the placeholder carries the
/// fine-grained PAT prefix. Manifests that bind their own GitHub secret use the same value.
const ACCESS_PLACEHOLDER: &str = "github_pat_AGENT_MEDIATED_GITHUB_TOKEN";
/// The hosts `gh` and Git over HTTPS reach; the token is never substituted anywhere else.
const MEDIATED_HOSTS: [&str; 4] = ["github.com", "api.github.com", "gist.github.com", "uploads.github.com"];
const USER_URL: &str = "https://api.github.com/user";
/// Scopes an Agent's `gh` and Git need so common commands do not fail half-way: the GitHub CLI's
/// own defaults, workflow files, Projects that `gh pr edit` reads, and container packages.
pub const SCOPES: [&str; 6] = ["repo", "read:org", "gist", "workflow", "read:project", "write:packages"];
/// The part of [`SCOPES`] the GitHub CLI does not request by default.
const ADDITIONAL_SCOPES: &str = "workflow,read:project,write:packages";
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The GitHub account Agents act as, without its token.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubAccount {
    /// The account's login, such as `octocat`.
    pub login: String,
    /// Name used for Git commits: the profile name, or the login when the profile has none.
    pub name: String,
    /// Email used for Git commits: the public profile email, or the account's noreply address.
    pub email: String,
    /// Recommended [`SCOPES`] a classic or OAuth token was not granted, as GitHub reported at
    /// login. Fine-grained tokens report no scopes and are not checked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_scopes: Vec<String>,
}

/// Validates and stores the agent stack's GitHub token.
pub(crate) struct Authentication {
    database: persistence::Database,
    client: reqwest::Client,
    user_url: String,
}

impl Authentication {
    pub(crate) fn new(database: persistence::Database) -> Self {
        Self {
            database,
            client: reqwest::Client::new(),
            user_url: USER_URL.into(),
        }
    }

    /// Asks GitHub which account `token` belongs to, then stores both.
    ///
    /// Unlike a harness token, a GitHub token is rejected when GitHub cannot be reached: the
    /// account's identity is what the caller confirms and what Git commits carry.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is empty, GitHub rejects it or cannot be reached, or the
    /// account cannot be persisted.
    pub(crate) async fn login(&self, token: Zeroizing<String>) -> Result<GithubAccount, Error> {
        let token = Zeroizing::new(token.trim().to_owned());
        if token.is_empty() {
            return Err(Error::Invalid("no GitHub token was provided".into()));
        }
        let account = self.account(&token).await?;
        self.database
            .put_provider_account(persistence::ProviderAccountWrite {
                provider: PROVIDER.into(),
                credentials: vec![persistence::StoredSecret {
                    name: ACCESS_SECRET.into(),
                    value: Zeroizing::new(token.as_bytes().to_vec()),
                }],
                metadata_json: serde_json::to_string(&account)?,
            })
            .await?;
        Ok(account)
    }

    async fn account(&self, token: &str) -> Result<GithubAccount, Error> {
        let response = self
            .client
            .get(&self.user_url)
            .timeout(REQUEST_TIMEOUT)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(
                reqwest::header::USER_AGENT,
                concat!("agentctl/", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            .map_err(|error| Error::Invalid(format!("could not reach GitHub to check the token: {error}")))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Invalid("GitHub rejected the token".into()));
        }
        if !status.is_success() {
            return Err(Error::Invalid(format!(
                "GitHub answered {status} when asked which account the token belongs to"
            )));
        }
        let missing_scopes = response
            .headers()
            .get("x-oauth-scopes")
            .and_then(|granted| granted.to_str().ok())
            .map(missing_scopes)
            .unwrap_or_default();
        let user: User = response
            .json()
            .await
            .map_err(|error| Error::Invalid(format!("GitHub returned an unexpected user profile: {error}")))?;
        Ok(user.into_account(missing_scopes))
    }

    #[cfg(test)]
    fn with_user_url(mut self, user_url: String) -> Self {
        self.user_url = user_url;
        self
    }
}

#[derive(Deserialize)]
struct User {
    login: String,
    id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

impl User {
    fn into_account(self, missing_scopes: Vec<String>) -> GithubAccount {
        let name = self
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| self.login.clone());
        let email = self
            .email
            .filter(|email| !email.trim().is_empty())
            .unwrap_or_else(|| format!("{}+{}@users.noreply.github.com", self.id, self.login));
        GithubAccount {
            login: self.login,
            name,
            email,
            missing_scopes,
        }
    }
}

/// The recommended [`SCOPES`] that `granted`, GitHub's comma-separated `X-OAuth-Scopes`, lacks.
/// A broader scope covers its narrower forms: `project` covers `read:project`, and `admin:org`
/// or `write:org` cover `read:org`.
fn missing_scopes(granted: &str) -> Vec<String> {
    let granted: Vec<&str> = granted
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .collect();
    SCOPES
        .iter()
        .filter(|required| {
            !granted.iter().any(|scope| {
                scope == *required
                    || required.strip_prefix("read:").is_some_and(|resource| {
                        *scope == resource
                            || *scope == format!("write:{resource}")
                            || *scope == format!("admin:{resource}")
                    })
            })
        })
        .map(|scope| (*scope).to_owned())
        .collect()
}

/// Returns the stored GitHub account, if one has signed in.
///
/// # Errors
///
/// Returns an error when the database cannot be read or holds an unreadable account.
pub(crate) async fn account(database: &persistence::Database) -> Result<Option<GithubAccount>, Error> {
    let Some(metadata) = database.provider_account_metadata(PROVIDER).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&metadata)?))
}

/// The mediated `GITHUB_TOKEN` an Agent with the GitHub account receives.
///
/// # Errors
///
/// Returns an error naming `agentctl github login` when no account has signed in.
pub(crate) async fn prepare(database: &persistence::Database) -> Result<(MediatedSecret, GithubAccount), Error> {
    let Some(account) = account(database).await? else {
        return Err(Error::Invalid(
            "the Agent uses the GitHub account, but none is signed in; run `agentctl github login`".into(),
        ));
    };
    let secret = MediatedSecret {
        environment: ACCESS_ENVIRONMENT,
        placeholder: ACCESS_PLACEHOLDER.into(),
        reference: SecretReference::from_opaque(ACCESS_SECRET),
        allowed_hosts: MEDIATED_HOSTS.iter().map(|host| (*host).to_owned()).collect(),
    };
    Ok((secret, account))
}

/// Whether a manifest secret would collide with the binding the GitHub account owns. Mediation
/// needs placeholders that never contain one another, so overlapping ones are refused at apply.
pub(crate) fn conflicts_with_managed_secret(name: &str, placeholder: Option<&str>) -> bool {
    name == ACCESS_ENVIRONMENT
        || placeholder.is_some_and(|placeholder| {
            placeholder.contains(ACCESS_PLACEHOLDER) || ACCESS_PLACEHOLDER.contains(placeholder)
        })
}

/// The GitHub CLI, which owns the browser sign-in so agentctl needs no OAuth app of its own.
const GH: &str = "gh";
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use sandbox::secret_store::SecretStore as _;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// Answers one request with `status` and `body`, returning the endpoint and the request text.
    async fn serve_once(status: &'static str, body: &'static str) -> (String, tokio::sync::oneshot::Receiver<String>) {
        serve_once_with(status, "", body).await
    }

    /// Like [`serve_once`], adding `headers`, each ending in CRLF.
    async fn serve_once_with(
        status: &'static str,
        headers: &'static str,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind user endpoint");
        let endpoint = format!("http://{}/user", listener.local_addr().expect("local address"));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut chunk = [0_u8; 4_096];
            let read = stream.read(&mut chunk).await.expect("read request");
            let _ = sender.send(String::from_utf8_lossy(&chunk[..read]).into_owned());
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("write response");
        });
        (endpoint, receiver)
    }

    fn database(directory: &TempDir) -> persistence::Database {
        persistence::Database::open(&directory.path().join("agent.db")).expect("database")
    }

    /// A stand-in `gh` that records how it was run beside itself and keeps its token in
    /// `GH_CONFIG_DIR`, as the real one does with `--insecure-storage`.
    #[cfg(unix)]
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

    #[cfg(unix)]
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
            "--scopes workflow,read:project,write:packages",
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

    #[cfg(unix)]
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

    #[tokio::test(flavor = "local")]
    async fn stores_the_token_and_the_account_it_belongs_to() {
        let directory = TempDir::new().expect("temporary directory");
        let database = database(&directory);
        let (endpoint, request) = serve_once(
            "200 OK",
            r#"{"login":"digdir-bot","id":42,"name":"Digdir Bot","email":"bot@example.com"}"#,
        )
        .await;
        let manager = Authentication::new(database.clone()).with_user_url(endpoint);

        let account = manager
            .login(Zeroizing::new("ghp_token-canary\n".into()))
            .await
            .expect("login");

        assert_eq!(
            account,
            GithubAccount {
                login: "digdir-bot".into(),
                name: "Digdir Bot".into(),
                email: "bot@example.com".into(),
                missing_scopes: Vec::new(),
            }
        );
        let request = request.await.expect("request");
        assert!(request.contains("authorization: Bearer ghp_token-canary"), "{request}");
        let stored = database
            .resolve(&SecretReference::from_opaque(ACCESS_SECRET))
            .await
            .expect("stored token");
        assert_eq!(stored.expose(), b"ghp_token-canary");
        assert_eq!(super::account(&database).await.expect("account"), Some(account));
    }

    #[tokio::test(flavor = "local")]
    async fn a_profile_without_name_or_public_email_falls_back_to_the_login_and_noreply_address() {
        let directory = TempDir::new().expect("temporary directory");
        let database = database(&directory);
        let (endpoint, _request) =
            serve_once("200 OK", r#"{"login":"digdir-bot","id":42,"name":null,"email":null}"#).await;

        let account = Authentication::new(database)
            .with_user_url(endpoint)
            .login(Zeroizing::new("ghp_token".into()))
            .await
            .expect("login");

        assert_eq!(account.name, "digdir-bot");
        assert_eq!(account.email, "42+digdir-bot@users.noreply.github.com");
    }

    #[tokio::test(flavor = "local")]
    async fn a_token_without_the_recommended_scopes_is_stored_with_what_it_lacks() {
        let directory = TempDir::new().expect("temporary directory");
        let (endpoint, _request) = serve_once_with(
            "200 OK",
            "X-OAuth-Scopes: repo, gist, admin:org, project\r\n",
            r#"{"login":"digdir-bot","id":42}"#,
        )
        .await;

        let account = Authentication::new(database(&directory))
            .with_user_url(endpoint)
            .login(Zeroizing::new("ghp_narrow".into()))
            .await
            .expect("a narrow token is still stored");

        assert_eq!(account.missing_scopes, ["workflow", "write:packages"]);
    }

    #[test]
    fn broader_scopes_cover_their_read_forms() {
        assert!(missing_scopes("repo,read:org,gist,workflow,read:project,write:packages").is_empty());
        assert!(missing_scopes("repo, write:org, gist, workflow, project, write:packages").is_empty());
        assert_eq!(missing_scopes(""), SCOPES);
        assert_eq!(missing_scopes("public_repo"), SCOPES);
    }

    #[tokio::test(flavor = "local")]
    async fn a_rejected_or_unreachable_token_is_not_stored() {
        let directory = TempDir::new().expect("temporary directory");
        let database = database(&directory);
        let (endpoint, _request) = serve_once("401 Unauthorized", r#"{"message":"Bad credentials"}"#).await;
        let error = Authentication::new(database.clone())
            .with_user_url(endpoint)
            .login(Zeroizing::new("ghp_rejected".into()))
            .await
            .expect_err("rejected");
        assert!(!error.to_string().contains("ghp_rejected"), "{error}");

        Authentication::new(database.clone())
            .with_user_url("http://127.0.0.1:1/user".into())
            .login(Zeroizing::new("ghp_unreachable".into()))
            .await
            .expect_err("unreachable");

        assert_eq!(super::account(&database).await.expect("account"), None);
        assert!(
            prepare(&database)
                .await
                .is_err_and(|error| error.to_string().contains("agentctl github login"))
        );
    }

    #[tokio::test(flavor = "local")]
    async fn the_mediated_token_reaches_only_github_hosts() {
        let directory = TempDir::new().expect("temporary directory");
        let database = database(&directory);
        let (endpoint, _request) =
            serve_once("200 OK", r#"{"login":"digdir-bot","id":42,"name":null,"email":null}"#).await;
        Authentication::new(database.clone())
            .with_user_url(endpoint)
            .login(Zeroizing::new("ghp_token".into()))
            .await
            .expect("login");

        let (secret, account) = prepare(&database).await.expect("prepare");

        assert_eq!(account.login, "digdir-bot");
        assert_eq!(secret.environment, "GITHUB_TOKEN");
        assert_eq!(secret.placeholder, ACCESS_PLACEHOLDER);
        assert_eq!(
            secret.allowed_hosts,
            ["github.com", "api.github.com", "gist.github.com", "uploads.github.com"]
        );
    }
}
