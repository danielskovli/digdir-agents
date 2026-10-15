//! `agentctl github`: stores and lists the GitHub tokens Agents use, one per owner.

use agent::{
    Error,
    control_api::Client,
    github::{GithubToken, Inspection, Logout, Repository, TokenKind},
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
/// repository. Any other token reaches every owner and is kept for any other organization, unless `owner`
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
    let mut repository = current_repository();
    let interactive = source != Source::Stdin;
    if interactive && !prompt::interactive() {
        return Err(
            Error::Invalid("no terminal to sign in to GitHub on; pass a token with `--from-stdin`".into()).into(),
        );
    }
    let mut named_owner = owner.map(|owner| owner.to_ascii_lowercase());
    // Naming an owner asks for that owner's fine-grained token; otherwise the person chooses.
    let source = if source == Source::Form && named_owner.is_none() && agent::github::gh_installed() {
        choose_method()?
    } else {
        source
    };
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
            let owner = match named_owner.clone() {
                Some(owner) => owner,
                None => choose_owner(&mut repository)?,
            };
            named_owner = Some(owner.clone());
            (create_in_form(&owner, repository.as_ref()).await?, None)
        }
    };
    let kind = TokenKind::of(&token);
    let owner = if kind.single_owner() {
        Some(
            match named_owner.or_else(|| repository.as_ref().map(|repository| repository.owner.clone())) {
                Some(owner) => owner,
                None if interactive => choose_owner(&mut repository)?,
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
    if let Some(expected) = expected_login {
        signed_in_as(&expected, &inspection.account.login)?;
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
        "Agents in {} now act as {} and commit as {} <{}>",
        stored.scope(),
        stored.account.login,
        stored.account.name,
        stored.account.email
    );
    Ok(Some(stored))
}

/// Forgets a stored token: the one `target` names, or one chosen from a menu.
///
/// # Errors
///
/// Returns an error when there is no terminal to choose or confirm on, or the daemon fails.
pub(crate) async fn logout(client: &Client, target: Option<Logout>, yes: bool) -> CommandResult<()> {
    let tokens = client.auth_status().await?.github;
    if tokens.is_empty() {
        println!("No GitHub tokens are stored.");
        return Ok(());
    }
    if let Some(target) = target {
        return forget(client, &tokens, target, yes, "Log out").await;
    }
    if !prompt::interactive() {
        return Err(Error::Invalid("no terminal to choose on; pass `--owner` or `--everywhere`".into()).into());
    }
    eprintln!("Log out of GitHub for agents in:");
    let choices = logout_choices(&tokens);
    for (number, (label, _)) in choices.iter().enumerate() {
        eprintln!("  {}. {label}", number + 1);
    }
    let answer = prompt::ask(&format!("Choose 1-{}, or press Enter to cancel", choices.len()))?;
    let Some(index) = choice(&answer, choices.len()) else {
        eprintln!("Nothing was changed.");
        return Ok(());
    };
    forget(client, &tokens, choices[index].1.clone(), yes, "Log out").await
}

/// Forgets every stored token, after a warning.
///
/// # Errors
///
/// Returns an error when there is no terminal to confirm on without `--yes`, or the daemon fails.
pub(crate) async fn reset(client: &Client, yes: bool) -> CommandResult<()> {
    let tokens = client.auth_status().await?.github;
    if tokens.is_empty() {
        println!("No GitHub tokens are stored; there is nothing to reset.");
        return Ok(());
    }
    eprintln!("This forgets every GitHub token agentctl has stored. Agents using them lose GitHub access");
    eprintln!("until you run `agentctl github login` again.");
    forget(client, &tokens, Logout::Everywhere, yes, "Reset").await
}

/// Confirms, then forgets the tokens `target` names, and says what happened.
async fn forget(client: &Client, tokens: &[GithubToken], target: Logout, yes: bool, verb: &str) -> CommandResult<()> {
    let affected: Vec<&GithubToken> = tokens
        .iter()
        .filter(|token| match &target {
            Logout::Owner(owner) => token.owner.as_deref() == Some(owner.as_str()),
            Logout::AnyOther => token.owner.is_none(),
            Logout::Everywhere => true,
        })
        .collect();
    if affected.is_empty() {
        println!("No stored GitHub token matches; nothing was changed.");
        return Ok(());
    }
    eprintln!();
    for token in &affected {
        eprintln!("  {}", describe(token));
    }
    eprintln!();
    if !yes {
        if !prompt::interactive() {
            return Err(Error::Invalid("no terminal to confirm on; pass `--yes`".into()).into());
        }
        let question = format!(
            "{verb}: forget {}?",
            if affected.len() == 1 {
                "this token"
            } else {
                "these tokens"
            }
        );
        if !prompt::confirm(&question)? {
            eprintln!("Nothing was changed.");
            return Ok(());
        }
    }
    let forgotten = client.github_logout(target).await?;
    println!(
        "Forgot {} GitHub token{}. Agents using {} lose GitHub access on their next pass.",
        forgotten.len(),
        if forgotten.len() == 1 { "" } else { "s" },
        if forgotten.len() == 1 { "it" } else { "them" }
    );
    println!("They remain valid on GitHub until revoked: {REVOKE_URL}");
    Ok(())
}

/// Where tokens are revoked on GitHub: forgetting one here does not.
const REVOKE_URL: &str = "https://github.com/settings/personal-access-tokens";

/// The menu `logout` offers: one entry per stored token, then everywhere.
fn logout_choices(tokens: &[GithubToken]) -> Vec<(String, Logout)> {
    tokens
        .iter()
        .map(|token| {
            let target = token.owner.clone().map_or(Logout::AnyOther, Logout::Owner);
            (describe(token), target)
        })
        .chain(std::iter::once((
            "Everywhere: every token above".to_owned(),
            Logout::Everywhere,
        )))
        .collect()
}

/// A token in words: where it is used, whom it acts as, and when it expires.
fn describe(token: &GithubToken) -> String {
    let place = token
        .owner
        .as_deref()
        .map_or_else(|| "Any other organization".to_owned(), ToOwned::to_owned);
    format!(
        "{place}: acts as {} ({} token, expires {})",
        token.account.login,
        token.kind.label(),
        expiry(token, time::OffsetDateTime::now_utc())
    )
}

/// The menu entry `answer` picks among `count`, numbered from 1.
fn choice(answer: &str, count: usize) -> Option<usize> {
    answer
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|number| (1..=count).contains(number))
        .map(|number| number - 1)
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
        None => "never, or unknown".into(),
        Some(expires_at) if token.expired(now) => format!("{} (expired)", expires_at.date()),
        Some(expires_at) if token.expires_soon(now) => format!("{} (soon)", expires_at.date()),
        Some(expires_at) => expires_at.date().to_string(),
    }
}

/// The GitHub repository of the current directory's `origin` remote, if it is on GitHub.
pub(crate) fn current_repository() -> Option<Repository> {
    let output = std::process::Command::new("git")
        .args(["remote", "-v"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| repository_from_remotes(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

/// The GitHub repository `git remote -v` lists: `origin` when it is on GitHub, else the first
/// remote that is.
fn repository_from_remotes(remotes: &str) -> Option<Repository> {
    let mut found = remotes.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let name = fields.next()?;
        Repository::from_remote(fields.next()?).map(|repository| (name == "origin", repository))
    });
    let first = found.next()?;
    if first.0 {
        return Some(first.1);
    }
    found
        .find(|(origin, _)| *origin)
        .map_or(Some(first.1), |(_, repository)| Some(repository))
}

/// Asks whether to create a fine-grained token or sign in through the GitHub CLI's browser flow.
fn choose_method() -> Result<Source, Error> {
    eprintln!("How should agents sign in to GitHub?");
    eprintln!("  1. Create a fine-grained token: only the repositories you pick (recommended)");
    eprintln!("  2. Sign in with the browser through the GitHub CLI: every repository the account can reach");
    loop {
        match prompt::ask("Choose 1-2 [1]")?.as_str() {
            "" | "1" => return Ok(Source::Form),
            "2" => return Ok(Source::Browser),
            _ => eprintln!("Type 1 or 2, or press Enter for 1."),
        }
    }
}

/// The organization or user to set up access for: this repository's owner when the person agrees,
/// otherwise one they name or paste a GitHub address of. A pasted repository address becomes the
/// repository the instructions and the access check use.
fn choose_owner(repository: &mut Option<Repository>) -> Result<String, Error> {
    if let Some(found) = repository.as_ref()
        && prompt::confirm_default_yes(&format!(
            "This repository is {found} on GitHub. Set up access for agents working in {}?",
            found.owner
        ))?
    {
        return Ok(found.owner.clone());
    }
    eprintln!("Which GitHub organization or user should agents work in?");
    loop {
        let answer = prompt::ask("Organization, user or repository URL")?;
        match owner_from_answer(&answer) {
            Some((owner, named)) => {
                if named.is_some() || repository.as_ref().is_some_and(|found| found.owner != owner) {
                    *repository = named;
                }
                return Ok(owner);
            }
            None => eprintln!(
                "Type a name such as digdir, or paste an address such as https://github.com/digdir/digdir-agents."
            ),
        }
    }
}

/// The owner, and the repository when there is one, that an answer names: a plain name, or a
/// GitHub address of the owner or of one of its repositories.
fn owner_from_answer(answer: &str) -> Option<(String, Option<Repository>)> {
    let answer = answer.trim().trim_end_matches('/');
    if let Some(repository) =
        Repository::from_remote(answer).or_else(|| Repository::from_remote(&format!("https://{answer}")))
    {
        return Some((repository.owner.clone(), Some(repository)));
    }
    let name = answer
        .strip_prefix("https://")
        .or_else(|| answer.strip_prefix("http://"))
        .unwrap_or(answer);
    let name = name.strip_prefix("github.com/").unwrap_or(name);
    agent::github::valid_owner(name).then(|| (name.to_ascii_lowercase(), None))
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
    eprintln!("  1. Sign in to github.com as the account agents should act as, such as a bot account.");
    eprintln!("     A private browser window keeps it apart from your own account.");
    eprintln!("  2. Open the form below. Name, owner, lifetime and permissions are filled in.");
    eprintln!(
        "  3. Under Repository access, choose \"Only select repositories\" and pick {}.",
        repository_name.map_or_else(
            || "the repositories agents work on".to_owned(),
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
    let expected = prompt::ask("Type the login of the account agents should act as")?;
    if expected.is_empty() {
        return Err(Error::Invalid("no account was named; nothing was signed in".into()));
    }
    Ok(expected)
}

/// Refuses a browser sign-in as any account but the one the person named beforehand.
fn signed_in_as(expected: &str, actual: &str) -> Result<(), Error> {
    if expected.trim_start_matches('@').eq_ignore_ascii_case(actual) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "the browser signed in as {actual}, not {expected}; nothing was stored. Sign in as {expected} and try again"
        )))
    }
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
    let _ = writeln!(text, "  For:       {}", owner.unwrap_or("any other organization"));
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

    fn token(owner: Option<&str>, login: &str) -> GithubToken {
        GithubToken {
            owner: owner.map(str::to_owned),
            account: GithubAccount {
                login: login.into(),
                name: login.into(),
                email: format!("{login}@example.com"),
            },
            kind: if owner.is_some() {
                TokenKind::FineGrained
            } else {
                TokenKind::Classic
            },
            expires_at: None,
        }
    }

    #[test]
    fn logout_offers_each_token_in_words_and_then_everywhere() {
        let choices = logout_choices(&[token(Some("digdir"), "digdir-bot"), token(None, "digdir-bot")]);
        let labels: Vec<&str> = choices.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "digdir: acts as digdir-bot (fine-grained token, expires never, or unknown)",
                "Any other organization: acts as digdir-bot (classic token, expires never, or unknown)",
                "Everywhere: every token above",
            ]
        );
        assert_eq!(choices[0].1, Logout::Owner("digdir".into()));
        assert_eq!(choices[1].1, Logout::AnyOther);
        assert_eq!(choices[2].1, Logout::Everywhere);
    }

    #[test]
    fn a_menu_choice_is_a_listed_number_and_anything_else_cancels() {
        assert_eq!(choice("1", 3), Some(0));
        assert_eq!(choice(" 3 ", 3), Some(2));
        for answer in ["", "0", "4", "digdir", "-1"] {
            assert_eq!(choice(answer, 3), None, "{answer:?}");
        }
    }

    #[test]
    fn the_repository_is_origin_on_github_else_the_first_remote_on_github() {
        let remotes = "upstream\thttps://github.com/digdir/upstream.git (fetch)\n\
                       upstream\thttps://github.com/digdir/upstream.git (push)\n\
                       origin\tgit@github.com:danielskovli/fork.git (fetch)\n";
        assert_eq!(
            repository_from_remotes(remotes),
            Some(Repository::new("danielskovli", "fork"))
        );
        let without_origin =
            "gitlab\thttps://gitlab.com/a/b.git (fetch)\nmine\thttps://github.com/Digdir/agents (fetch)\n";
        assert_eq!(
            repository_from_remotes(without_origin),
            Some(Repository::new("digdir", "agents"))
        );
        assert_eq!(
            repository_from_remotes("origin\thttps://gitlab.com/a/b.git (fetch)\n"),
            None
        );
        assert_eq!(repository_from_remotes(""), None);
    }

    #[test]
    fn an_owner_is_a_name_or_a_github_address_of_it_or_its_repositories() {
        let repository = Some(Repository::new("digdir", "digdir-agents"));
        for answer in [
            "https://github.com/digdir/digdir-agents",
            "https://github.com/Digdir/digdir-agents.git/",
            "github.com/digdir/digdir-agents",
            "git@github.com:digdir/digdir-agents.git",
        ] {
            assert_eq!(
                owner_from_answer(answer),
                Some(("digdir".to_owned(), repository.clone())),
                "{answer}"
            );
        }
        for answer in ["digdir", " Digdir ", "https://github.com/digdir", "github.com/digdir/"] {
            assert_eq!(owner_from_answer(answer), Some(("digdir".to_owned(), None)), "{answer}");
        }
        for answer in ["", "https://gitlab.com/digdir/agents", "not an owner", "-digdir"] {
            assert_eq!(owner_from_answer(answer), None, "{answer:?}");
        }
    }

    #[test]
    fn a_browser_sign_in_as_another_account_is_refused() {
        signed_in_as("digdir-bot", "Digdir-Bot").expect("the named account, in any case");
        signed_in_as("@digdir-bot", "digdir-bot").expect("with a leading @");
        let refused = signed_in_as("digdir-bot", "danielskovli").expect_err("another account");
        assert!(
            refused
                .to_string()
                .contains("signed in as danielskovli, not digdir-bot"),
            "{refused}"
        );
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
