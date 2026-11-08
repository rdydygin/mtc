//! Single sign-on through the system browser.
//!
//! 1. [`login_options`] reads which providers the server offers, and from its
//!    version, how the browser hands the session back ([`Handoff`]).
//! 2. [`SsoAttempt::new`] starts an attempt; the user signs in at
//!    [`SsoAttempt::login_url`] in their browser.
//! 3. The server's last page opens a callback URL, which the app passes to
//!    [`SsoAttempt::complete`]:
//!    - [`Handoff::Desktop`] (server 9.1 and later): the browser lands on
//!      `<site>/login/desktop` with our one-time `client_token` and a
//!      one-time `server_token`, and that page rewrites its URL to the
//!      [`CALLBACK_SCHEME`] scheme (the `dev-` token prefix selects
//!      `mattermost-dev://`, which keeps us clear of the official app's
//!      `mattermost://`). We check the `client_token` is ours and trade the
//!      `server_token` for a session at `users/login/desktop_token`.
//!    - [`Handoff::Mobile`] (older servers): the mobile apps' flow. The page
//!      opens `mmauth://callback?MMAUTHTOKEN=…`, which carries the session
//!      token itself. Nothing ties it to one attempt, so the app accepts it
//!      only while an attempt is waiting, and we check it by loading the user.
//!
//! The callback URL can also be pasted by hand: for the desktop handoff the
//! browser's address bar still holds the `/login/desktop` URL, and for the
//! mobile one the page's "click here" link holds the `mmauth://` URL.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use url::Url;

use crate::{Client, User, client::normalize_server_url, runtime};

/// The URL scheme the server's `/login/desktop` page opens for our tokens.
pub const CALLBACK_SCHEME: &str = "mattermost-dev";

/// The URL scheme older servers open for the mobile apps' flow. It is in the
/// servers' default `NativeAppSettings.AppCustomURLSchemes`.
pub const MOBILE_CALLBACK_SCHEME: &str = "mmauth";

const MOBILE_CALLBACK: &str = "mmauth://callback";

/// Whether `url` is an SSO callback the app should route to [`SsoAttempt`].
pub fn is_callback_url(url: &str) -> bool {
    Url::parse(url)
        .is_ok_and(|url| matches!(url.scheme(), CALLBACK_SCHEME | MOBILE_CALLBACK_SCHEME))
}

/// A sign-in method the server offers besides email and password.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SsoProvider {
    kind: SsoKind,
    label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SsoKind {
    GitLab,
    Google,
    Office365,
    OpenId,
    Saml,
}

impl SsoProvider {
    pub fn kind(&self) -> SsoKind {
        self.kind
    }

    /// The button text, honoring the server's custom labels.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The OAuth service name in `oauth/<service>/…` paths; `None` for SAML.
    fn oauth_service(&self) -> Option<&'static str> {
        match self.kind {
            SsoKind::GitLab => Some("gitlab"),
            SsoKind::Google => Some("google"),
            SsoKind::Office365 => Some("office365"),
            SsoKind::OpenId => Some("openid"),
            SsoKind::Saml => None,
        }
    }
}

/// How the browser hands the session back to the app.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Handoff {
    /// Through `<site>/login/desktop` and `mattermost-dev://` (server 9.1+).
    #[default]
    Desktop,
    /// Through `mmauth://`, the mobile apps' flow, for older servers.
    Mobile,
}

impl Handoff {
    /// Pick the handoff from the server's version string, such as `7.10.5`.
    /// Unknown versions get the desktop handoff, which current servers have.
    fn for_version(version: Option<&str>) -> Self {
        let mut parts = version
            .unwrap_or_default()
            .split('.')
            .map(str::parse::<u32>);
        match (parts.next(), parts.next()) {
            (Some(Ok(major)), Some(Ok(minor))) if (major, minor) < (9, 1) => Self::Mobile,
            _ => Self::Desktop,
        }
    }
}

/// What the server allows on its sign-in screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoginOptions {
    pub providers: Vec<SsoProvider>,
    /// Whether email/username and password sign-in is enabled.
    pub password: bool,
    pub handoff: Handoff,
}

/// Ask an unauthenticated server how people can sign in.
pub async fn login_options(server: &str) -> Result<LoginOptions> {
    let server = normalize_server_url(server)?;
    runtime::run(async move {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()?;
        let url = server.join("api/v4/config/client?format=old")?;
        let config: Value = http
            .get(url)
            .send()
            .await
            .map_err(|_| {
                anyhow!("Couldn’t reach the server. Check the address and your connection.")
            })?
            .error_for_status()
            .map_err(|_| anyhow!("This doesn’t look like a Mattermost server."))?
            .json()
            .await
            .map_err(|_| anyhow!("This doesn’t look like a Mattermost server."))?;
        Ok(parse_login_options(&config))
    })
    .await
}

fn parse_login_options(config: &Value) -> LoginOptions {
    let flag = |key: &str| config.get(key).and_then(Value::as_str) == Some("true");
    let text = |key: &str, fallback: &str| {
        config
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(fallback)
            .to_string()
    };
    let mut providers = Vec::new();
    let mut add = |enabled: bool, kind: SsoKind, label: String| {
        if enabled {
            providers.push(SsoProvider { kind, label });
        }
    };
    add(
        flag("EnableSaml"),
        SsoKind::Saml,
        text("SamlLoginButtonText", "SAML"),
    );
    add(
        flag("EnableSignUpWithOpenId"),
        SsoKind::OpenId,
        text("OpenIdButtonText", "OpenID"),
    );
    add(
        flag("EnableSignUpWithOffice365"),
        SsoKind::Office365,
        "Microsoft".into(),
    );
    add(
        flag("EnableSignUpWithGoogle"),
        SsoKind::Google,
        "Google".into(),
    );
    add(
        flag("EnableSignUpWithGitLab"),
        SsoKind::GitLab,
        text("GitLabButtonText", "GitLab"),
    );
    // Unlicensed servers leave some providers (notably GitLab) out of the
    // public config even when enabled. `MTC_SSO_PROVIDERS=gitlab,saml`
    // offers them anyway.
    if let Ok(extra) = std::env::var("MTC_SSO_PROVIDERS") {
        for name in extra.split(',').map(str::trim) {
            let kind = match name {
                "gitlab" => SsoKind::GitLab,
                "google" => SsoKind::Google,
                "office365" | "microsoft" => SsoKind::Office365,
                "openid" => SsoKind::OpenId,
                "saml" => SsoKind::Saml,
                _ => continue,
            };
            if !providers.iter().any(|p| p.kind == kind) {
                let label = format!("{kind:?}").replace("Office365", "Microsoft");
                providers.push(SsoProvider { kind, label });
            }
        }
    }
    LoginOptions {
        providers,
        password: flag("EnableSignInWithEmail")
            || flag("EnableSignInWithUsername")
            || flag("EnableLdap"),
        handoff: Handoff::for_version(config.get("Version").and_then(Value::as_str)),
    }
}

/// One browser sign-in attempt.
#[derive(Clone, Debug)]
pub struct SsoAttempt {
    server: Url,
    handoff: Handoff,
    client_token: String,
}

impl SsoAttempt {
    pub fn new(server: &str, handoff: Handoff) -> Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|err| anyhow!("no secure randomness: {err}"))?;
        let random: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            server: normalize_server_url(server)?,
            handoff,
            // The prefix makes the server's page open `mattermost-dev://`.
            client_token: format!("dev-{random}"),
        })
    }

    pub fn server(&self) -> &Url {
        &self.server
    }

    pub fn handoff(&self) -> Handoff {
        self.handoff
    }

    /// Where to send the browser to sign in with `provider`.
    pub fn login_url(&self, provider: &SsoProvider) -> Result<Url> {
        let mobile = self.handoff == Handoff::Mobile;
        let mut url = match provider.oauth_service() {
            Some(service) if mobile => {
                self.server.join(&format!("oauth/{service}/mobile_login"))?
            }
            Some(service) => self.server.join(&format!("oauth/{service}/login"))?,
            None => self.server.join("login/sso/saml")?,
        };
        {
            let mut query = url.query_pairs_mut();
            if provider.kind == SsoKind::Saml {
                query.append_pair("action", if mobile { "mobile" } else { "login" });
            }
            match self.handoff {
                Handoff::Desktop => query.append_pair("desktop_token", &self.client_token),
                Handoff::Mobile => query.append_pair("redirect_to", MOBILE_CALLBACK),
            };
        }
        Ok(url)
    }

    /// Whether `url` looks like this attempt's callback: for the desktop
    /// handoff the `mattermost-dev://` or `https://…/login/desktop` form with
    /// our `client_token`, for the mobile one any `mmauth://` callback.
    pub fn matches(&self, url: &str) -> bool {
        match parse_callback(url) {
            Ok(Callback::Desktop { client_token, .. }) => {
                self.handoff == Handoff::Desktop && client_token == self.client_token
            }
            Ok(Callback::Mobile { .. }) => self.handoff == Handoff::Mobile,
            Err(_) => false,
        }
    }

    /// Finish signing in with the callback URL from the browser.
    pub async fn complete(&self, callback: &str) -> Result<(Client, User)> {
        let server_token = match parse_callback(callback)? {
            Callback::Desktop {
                client_token,
                server_token,
            } if self.handoff == Handoff::Desktop && client_token == self.client_token => {
                server_token
            }
            Callback::Mobile { token } if self.handoff == Handoff::Mobile => {
                return Client::with_token(self.server.as_str(), &token).await;
            }
            _ => bail!("This sign-in link belongs to a different attempt. Start again."),
        };
        let server = self.server.clone();
        let (token, user) = runtime::run(async move {
            let http = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?;
            let response = http
                .post(server.join("api/v4/users/login/desktop_token")?)
                .json(&json!({ "token": server_token, "deviceId": "" }))
                .send()
                .await?;
            if !response.status().is_success() {
                bail!("The sign-in link has expired or was already used. Start again.");
            }
            let token = response
                .headers()
                .get("Token")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| anyhow!("The server didn’t return a session token."))?;
            let user: User = response.json().await?;
            Ok((token, user))
        })
        .await?;
        Ok((Client::new_unverified(self.server.as_str(), &token)?, user))
    }
}

enum Callback {
    Desktop {
        client_token: String,
        server_token: String,
    },
    Mobile {
        token: String,
    },
}

fn parse_callback(input: &str) -> Result<Callback> {
    let url = Url::parse(input.trim()).map_err(|_| anyhow!("That isn’t a sign-in link."))?;
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
            .filter(|v| !v.is_empty())
    };
    if url.scheme() == MOBILE_CALLBACK_SCHEME {
        return match param("MMAUTHTOKEN") {
            Some(token) => Ok(Callback::Mobile { token }),
            None => bail!("The sign-in link is incomplete. Copy the whole link."),
        };
    }
    let scheme_ok = matches!(
        url.scheme(),
        "https" | "http" | "mattermost" | CALLBACK_SCHEME
    );
    if !scheme_ok || !url.path().ends_with("/login/desktop") {
        bail!("That isn’t a Mattermost sign-in link.");
    }
    match (param("client_token"), param("server_token")) {
        (Some(client_token), Some(server_token)) => Ok(Callback::Desktop {
            client_token,
            server_token,
        }),
        _ => bail!("The sign-in link is incomplete. Copy the whole address."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_enabled_providers_with_custom_labels() {
        let config = json!({
            "EnableSaml": "true",
            "SamlLoginButtonText": "Okta",
            "EnableSignUpWithOpenId": "false",
            "EnableSignUpWithGitLab": "true",
            "GitLabButtonText": "",
            "EnableSignInWithEmail": "false",
            "EnableSignInWithUsername": "false",
            "EnableLdap": "false",
        });
        let options = parse_login_options(&config);
        let labels: Vec<_> = options.providers.iter().map(|p| p.label()).collect();
        assert_eq!(labels, ["Okta", "GitLab"]);
        assert!(!options.password);
    }

    #[test]
    fn picks_the_handoff_from_the_server_version() {
        let handoff = |version| Handoff::for_version(version);
        assert_eq!(handoff(Some("7.10.5")), Handoff::Mobile);
        assert_eq!(handoff(Some("9.0.2")), Handoff::Mobile);
        assert_eq!(handoff(Some("9.1.0")), Handoff::Desktop);
        assert_eq!(handoff(Some("10.11.3")), Handoff::Desktop);
        assert_eq!(handoff(Some("dev")), Handoff::Desktop);
        assert_eq!(handoff(None), Handoff::Desktop);
    }

    #[test]
    fn builds_login_urls_and_accepts_only_our_callback() {
        let sso = SsoAttempt::new("chat.example.com", Handoff::Desktop).unwrap();
        let saml = SsoProvider {
            kind: SsoKind::Saml,
            label: "SAML".into(),
        };
        let url = sso.login_url(&saml).unwrap();
        assert_eq!(url.path(), "/login/sso/saml");
        assert!(url.query().unwrap().contains("action=login"));
        assert!(
            url.query()
                .unwrap()
                .contains(&format!("desktop_token={}", sso.client_token))
        );

        let ours = format!(
            "mattermost-dev://chat.example.com/login/desktop?client_token={}&server_token=abc",
            sso.client_token
        );
        assert!(sso.matches(&ours));
        let pasted = ours.replacen("mattermost-dev", "https", 1);
        assert!(sso.matches(&pasted));
        assert!(
            !sso.matches(
                "mattermost-dev://x/login/desktop?client_token=dev-other&server_token=abc"
            )
        );
        assert!(!sso.matches("https://x/login?client_token=a&server_token=b"));
        assert!(!sso.matches("mmauth://callback?MMAUTHTOKEN=t&MMCSRF=c"));
    }

    #[test]
    fn mobile_handoff_uses_mobile_login_and_mmauth() {
        let sso = SsoAttempt::new("chat.example.com/mm", Handoff::Mobile).unwrap();
        let gitlab = SsoProvider {
            kind: SsoKind::GitLab,
            label: "GitLab".into(),
        };
        let url = sso.login_url(&gitlab).unwrap();
        assert_eq!(url.path(), "/mm/oauth/gitlab/mobile_login");
        assert_eq!(url.query(), Some("redirect_to=mmauth%3A%2F%2Fcallback"));
        let saml = SsoProvider {
            kind: SsoKind::Saml,
            label: "SAML".into(),
        };
        let url = sso.login_url(&saml).unwrap();
        assert_eq!(url.path(), "/mm/login/sso/saml");
        assert!(url.query().unwrap().starts_with("action=mobile&"));

        assert!(sso.matches("mmauth://callback?MMAUTHTOKEN=t&MMCSRF=c"));
        assert!(!sso.matches("mmauth://callback?MMCSRF=c"));
        let desktop = format!(
            "mattermost-dev://x/login/desktop?client_token={}&server_token=abc",
            sso.client_token
        );
        assert!(!sso.matches(&desktop));
        assert!(is_callback_url("mmauth://callback?MMAUTHTOKEN=t"));
        assert!(is_callback_url(&desktop));
        assert!(!is_callback_url("https://x/login/desktop"));
    }

    /// End-to-end against a live server: run the browser leg (for example
    /// with curl), then pass the callback URL it ends on: the `/login/desktop`
    /// URL, or the `mmauth://` link on older servers.
    /// `MTC_SSO_SERVER=… MTC_SSO_CALLBACK=… cargo test -p mattermost -- --ignored`
    #[test]
    #[ignore]
    fn completes_against_live_server() {
        let server = std::env::var("MTC_SSO_SERVER").expect("MTC_SSO_SERVER");
        let callback = std::env::var("MTC_SSO_CALLBACK").expect("MTC_SSO_CALLBACK");
        let (handoff, client_token) = match parse_callback(&callback).unwrap() {
            Callback::Desktop { client_token, .. } => (Handoff::Desktop, client_token),
            Callback::Mobile { .. } => (Handoff::Mobile, String::new()),
        };
        let sso = SsoAttempt {
            server: normalize_server_url(&server).unwrap(),
            handoff,
            client_token,
        };
        let (client, user) = futures::executor::block_on(sso.complete(&callback)).unwrap();
        let me = futures::executor::block_on(client.me()).unwrap();
        assert_eq!(me.id, user.id);
        println!(
            "signed in as {} via SSO ({handoff:?} handoff)",
            user.username
        );
        if handoff == Handoff::Desktop {
            // The server token is single-use.
            assert!(futures::executor::block_on(sso.complete(&callback)).is_err());
        }
    }
}
