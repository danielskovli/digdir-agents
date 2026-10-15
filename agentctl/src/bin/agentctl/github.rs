//! `agentctl github`: stores and lists the GitHub tokens Agents use, one per owner.

use agent::{
    Error,
    control_api::Client,
    github::{GithubToken, Inspection, Repository, TokenKind},
};
use zeroize::Zeroizing;

use crate::{CommandResult, prompt};

/// Where the token comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Source {
    /// A fine-grained token created in the pre-filled GitHub form and pasted.
    Form,
    /// A token read from standard input.
    Stdin,
    /// The host's own `gh` login.
    HostGh,
    /// The GitHub CLI's browser sign-in, kept apart from the host's own login.
    Browser,
}

/// Gets a token from `source`, shows what GitHub says about it, and stores it once confirmed.
///
/// A fine-grained token is kept for `owner`, by default the owner of the current directory's
/// repository. Any other token reaches every owner and is kept as the fallback, unless `owner`
/// is named. Returns `None` when nothing was stored.
///
/// # Errors
///
/// Returns an error when no token is supplied, GitHub rejects it, or it cannot be stored.
pub(crate) async fn login(
    client: &Client,
    source: Source,
    owner: Option<String>,
) -> CommandResult<Option<GithubToken>> {
    // These errors are about the token or the sign-in, not about an Agent, so they print as is.
    sign_in(client, source, owner).await.map_err(|error| match error {
        crate::CommandError::Agent(Error::Invalid(message)) => crate::CommandError::Message(message),
        error => error,
    })
}

async fn sign_in(client: &Client, source: Source, owner: Option<String>) -> CommandResult<Option<GithubToken>> {
    let repository = current_repository();
    let interactive = source != Source::Stdin;
    if interactive && !prompt::interactive() {
        return Err(
            Error::Invalid("no terminal to sign in to GitHub on; pass a token with `--from-stdin`".into()).into(),
        );
    }
    let named_owner = owner.map(|owner| owner.to_ascii_lowercase());
    let (token, expected_login) = match source {
        Source::Stdin => (crate::read_token_from_stdin()?, None),
        Source::HostGh => (agent::github::token_from_gh()?, None),
        Source::Browser => {
            let expected = browser_friction()?;
            let token = agent::github::token_from_gh_sign_in()?
                .ok_or_else(|| Error::Invalid("`--browser` needs the GitHub CLI, `gh`, on this host".into()))?;
            (token, Some(expected))
        }
        Source::Form => {
            let owner = match named_owner
                .clone()
                .or_else(|| repository.as_ref().map(|repository| repository.owner.clone()))
            {
                Some(owner) => owner,
                None => ask_owner()?,
            };
            (create_in_form(&owner, repository.as_ref()).await?, None)
        }
    };
    let kind = TokenKind::of(&token);
    let owner = if kind.single_owner() {
        Some(
            match named_owner.or_else(|| repository.as_ref().map(|repository| repository.owner.clone())) {
                Some(owner) => owner,
                None if interactive => ask_owner()?,
                None => {
                    return Err(Error::Invalid(
                        "a fine-grained token belongs to one owner; name it with `--owner`".into(),
                    )
                    .into());
                }
            },
        )
    } else {
        named_owner
    };
    let checked = repository
        .as_ref()
        .filter(|repository| owner.as_deref().is_none_or(|owner| owner == repository.owner));
    let inspection = agent::github::inspect(&token, checked).await?;
    if let Some(expected) = expected_login.filter(|expected| !expected.eq_ignore_ascii_case(&inspection.account.login))
    {
        return Err(Error::Invalid(format!(
            "the browser signed in as {}, not {expected}; nothing was stored. Sign in as {expected} and try again",
            inspection.account.login
        ))
        .into());
    }
    let warnings = warnings(&inspection, kind, agent::github::host_login().as_deref());
    eprint!("{}", summary(&inspection, owner.as_deref(), &warnings));
    if interactive {
        let question = format!(
            "Agents will push and comment as {}. Use this token?",
            inspection.account.login
        );
        let confirmed = if warnings.is_empty() {
            prompt::confirm_default_yes(&question)?
        } else {
            prompt::confirm(&question)?
        };
        if !confirmed {
            eprintln!("Nothing was stored.");
            return Ok(None);
        }
    }
    let stored = client.github_login(token.to_string(), owner).await?;
    println!(
        "Agents of {} now act as {} and commit as {} <{}>",
        stored.scope(),
        stored.account.login,
        stored.account.name,
        stored.account.email
    );
    Ok(Some(stored))
}

/// Lists the stored tokens: owner, account, kind and expiry.
///
/// # Errors
///
/// Returns an error when the daemon cannot be reached.
pub(crate) async fn list(client: &Client) -> CommandResult<()> {
    let tokens = client.auth_status().await?.github;
    if tokens.is_empty() {
        println!("No GitHub tokens. Run `agentctl github login` in a repository.");
        return Ok(());
    }
    let now = time::OffsetDateTime::now_utc();
    println!("{:<20} {:<24} {:<13} EXPIRES", "OWNER", "ACCOUNT", "KIND");
    for token in tokens {
        println!(
            "{:<20} {:<24} {:<13} {}",
            token.scope(),
            token.account.login,
            token.kind.label(),
            expiry(&token, now)
        );
    }
    Ok(())
}

fn expiry(token: &GithubToken, now: time::OffsetDateTime) -> String {
    match token.expires_at {
        None => "never or unknown".into(),
        Some(expires_at) if token.expired(now) => format!("{} (expired)", expires_at.date()),
        Some(expires_at) if token.expires_soon(now) => format!("{} (soon)", expires_at.date()),
        Some(expires_at) => expires_at.date().to_string(),
    }
}

/// The GitHub repository of the current directory's `origin` remote, if it is on GitHub.
pub(crate) fn current_repository() -> Option<Repository> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| Repository::from_remote(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

fn ask_owner() -> Result<String, Error> {
    loop {
        let owner = prompt::ask("GitHub user or organization whose repositories Agents work on")?.to_ascii_lowercase();
        if agent::github::valid_owner(&owner) {
            return Ok(owner);
        }
        eprintln!("{owner:?} is not a GitHub user or organization name.");
    }
}

/// Walks through creating a fine-grained token in GitHub's pre-filled form, and reads it.
async fn create_in_form(owner: &str, repository: Option<&Repository>) -> Result<Zeroizing<String>, Error> {
    let repository_name = repository
        .filter(|repository| repository.owner == owner)
        .map(|repository| repository.name.as_str());
    let name = format!("agentctl {}", repository_name.unwrap_or(owner));
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(hostname_command)
        .unwrap_or_else(|| "this host".into());
    let description = format!(
        "Agents on {host}, created by agentctl on {}",
        time::OffsetDateTime::now_utc().date()
    );
    let url = agent::github::token_form_url(owner, &name, &description);
    eprintln!("Agents need a fine-grained GitHub token for {owner}.");
    eprintln!();
    eprintln!("  1. Sign in to github.com as the account Agents should act as, such as a bot account.");
    eprintln!("     A private browser window keeps it apart from your own account.");
    eprintln!("  2. Open the form below. Name, owner, lifetime and permissions are filled in.");
    eprintln!(
        "  3. Under Repository access, choose \"Only select repositories\" and pick {}.",
        repository_name.map_or_else(
            || "the repositories Agents work on".to_owned(),
            |name| format!("{owner}/{name}")
        )
    );
    eprintln!("  4. Generate the token and copy it.");
    eprintln!();
    eprintln!("  {url}");
    eprintln!();
    if crate::launch::launch_blocked(|name| std::env::var_os(name)).is_none()
        && prompt::confirm_default_yes("Open the form in your browser now?")?
        && let Err(error) = crate::launch::open_url(&url).await
    {
        eprintln!("Could not open the browser ({error}); open the address above instead.");
    }
    prompt::secret("Paste the token")
}

fn hostname_command() -> Option<String> {
    let output = std::process::Command::new("hostname").output().ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !name.is_empty()).then_some(name)
}

/// The friction before a browser sign-in: it yields a token that reaches every repository the
/// account can, so the person names the account first and a different one is refused.
fn browser_friction() -> Result<String, Error> {
    eprintln!("The browser sign-in creates a token that reaches every repository and organization the");
    eprintln!("account can, unlike a fine-grained token. It signs in whichever account the browser uses.");
    let expected = prompt::ask("Type the login of the account Agents should act as")?;
    if expected.is_empty() {
        return Err(Error::Invalid("no account was named; nothing was signed in".into()));
    }
    Ok(expected)
}

/// Soft warnings about a token, each one line.
fn warnings(inspection: &Inspection, kind: TokenKind, host_login: Option<&str>) -> Vec<String> {
    let login = &inspection.account.login;
    let mut warnings = Vec::new();
    if host_login.is_some_and(|host| host.eq_ignore_ascii_case(login)) {
        warnings.push(format!("{login} is also your own `gh` login on this host"));
    }
    if !agent::github::looks_like_automation(login) {
        warnings.push(format!(
            "{login} is not named like an automation account (…-bot, …-agent)"
        ));
    }
    if !kind.single_owner() {
        warnings.push(format!(
            "a {} token reaches every repository {login} can, not selected ones",
            kind.label()
        ));
    }
    if let Some((repository, false)) = &inspection.repository {
        warnings.push(format!(
            "the token cannot see {repository}: it was not selected, or the organization has not approved the token yet"
        ));
    }
    if !inspection.missing_scopes.is_empty() {
        warnings.push(format!(
            "the token lacks {}, so some `gh` and Git commands will fail",
            inspection.missing_scopes.join(", ")
        ));
    }
    let now = time::OffsetDateTime::now_utc();
    if let Some(expires_at) = inspection
        .expires_at
        .filter(|expires_at| *expires_at <= now + agent::github::EXPIRY_WARNING)
    {
        warnings.push(format!("the token expires on {}", expires_at.date()));
    }
    warnings
}

/// What the token is, as a short block, followed by its warnings.
fn summary(inspection: &Inspection, owner: Option<&str>, warnings: &[String]) -> String {
    use std::fmt::Write as _;

    let mut text = String::new();
    let _ = writeln!(text);
    let _ = writeln!(
        text,
        "  Acts as:   {} ({} <{}>)",
        inspection.account.login, inspection.account.name, inspection.account.email
    );
    let _ = writeln!(text, "  Kind:      {}", inspection.kind.label());
    let _ = writeln!(
        text,
        "  For:       {}",
        owner.unwrap_or("every owner without a token of its own")
    );
    if let Some((repository, visible)) = &inspection.repository {
        let _ = writeln!(
            text,
            "  Can see:   {repository} {}",
            if *visible { "yes" } else { "no" }
        );
    }
    let _ = writeln!(
        text,
        "  Expires:   {}",
        inspection.expires_at.map_or_else(
            || "never, or GitHub did not say".to_owned(),
            |expires_at| expires_at.date().to_string()
        )
    );
    for warning in warnings {
        let _ = writeln!(text, "  ⚠ {warning}");
    }
    let _ = writeln!(text);
    text
}

#[cfg(test)]
mod tests {
    use agent::github::GithubAccount;

    use super::*;

    fn inspection(login: &str, kind: TokenKind, repository: Option<bool>) -> Inspection {
        Inspection {
            account: GithubAccount {
                login: login.into(),
                name: "Name".into(),
                email: "name@example.com".into(),
            },
            kind,
            expires_at: None,
            missing_scopes: Vec::new(),
            repository: repository.map(|visible| (Repository::new("digdir", "agents"), visible)),
        }
    }

    #[test]
    fn a_fine_grained_bot_token_that_sees_its_repository_raises_no_warning() {
        let checked = inspection("digdir-bot", TokenKind::FineGrained, Some(true));
        assert!(warnings(&checked, TokenKind::FineGrained, Some("danielskovli")).is_empty());
    }

    #[test]
    fn a_personal_broad_token_that_misses_its_repository_warns_of_each() {
        let checked = inspection("danielskovli", TokenKind::Classic, Some(false));
        let warnings = warnings(&checked, TokenKind::Classic, Some("DanielSkovli"));
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(warnings[0].contains("your own `gh` login"), "{warnings:?}");
        assert!(warnings[1].contains("automation account"), "{warnings:?}");
        assert!(warnings[2].contains("every repository"), "{warnings:?}");
        assert!(warnings[3].contains("cannot see digdir/agents"), "{warnings:?}");
    }

    #[test]
    fn the_summary_names_the_account_owner_access_and_expiry() {
        let mut checked = inspection("digdir-bot", TokenKind::FineGrained, Some(true));
        checked.expires_at = time::OffsetDateTime::from_unix_timestamp(1_799_834_400).ok();
        let text = summary(&checked, Some("digdir"), &["a warning".into()]);
        for expected in [
            "Acts as:   digdir-bot (Name <name@example.com>)",
            "Kind:      fine-grained",
            "For:       digdir",
            "Can see:   digdir/agents yes",
            "Expires:   2027-01-13",
            "⚠ a warning",
        ] {
            assert!(text.contains(expected), "{expected} in {text}");
        }
    }
}
