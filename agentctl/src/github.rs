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
        let user: User = response
            .json()
            .await
            .map_err(|error| Error::Invalid(format!("GitHub returned an unexpected user profile: {error}")))?;
        Ok(user.into_account())
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
    fn into_account(self) -> GithubAccount {
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
        }
    }
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

/// Reads the token of the host's active `gh` login, for an explicit `--from-gh` import.
///
/// # Errors
///
/// Returns an error when `gh` is missing, has no login, or prints no token.
pub fn token_from_gh() -> Result<Zeroizing<String>, Error> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|error| {
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
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
