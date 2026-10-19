//! OAuth 2.0 device code sign-in with the Microsoft identity platform.

use std::{
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{Error, Result, Transport, encode};

/// Read-only permissions asked for (`User.Read` for `/me`).
pub const SCOPES: &str = "Mail.Read Contacts.Read Calendars.Read Tasks.Read User.Read";

/// [`SCOPES`] plus `offline_access`, for runs longer than an access token lives
/// (about an hour). The refresh token stays in memory and dies with the run.
pub const LONG_RUN_SCOPES: &str =
    "Mail.Read Contacts.Read Calendars.Read Tasks.Read User.Read offline_access";

/// Permissions asked for to change a mailbox ([`crate::write`](mod@crate::write)), with a refresh
/// token for long runs.
pub const WRITE_SCOPES: &str = "Mail.ReadWrite User.Read offline_access";

/// Tokens from a sign-in. Kept in memory only, never written or logged.
pub struct Tokens {
    /// Bearer token for Graph requests.
    pub access: String,
    /// Token to get a new access token without the user, when `offline_access`
    /// was asked for.
    pub refresh: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokens { .. }")
    }
}

/// What to show the user so they can sign in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodePrompt {
    /// Page to open, e.g. `https://www.microsoft.com/link`.
    pub verification_uri: String,
    /// Code to type there.
    pub user_code: String,
    /// Microsoft's own instructions, in the user's language when it can.
    pub message: String,
}

/// Signs in with the device code flow and returns an access token.
///
/// `tenant` is `consumers` for Outlook.com accounts, `organizations` or a tenant
/// id for Microsoft 365. `prompt` is called once, with what the user must do.
pub fn sign_in<T: Transport>(
    transport: &T,
    client_id: &str,
    tenant: &str,
    prompt: &mut dyn FnMut(&DeviceCodePrompt),
) -> Result<String> {
    sign_in_with_sleep(transport, client_id, tenant, SCOPES, prompt, thread::sleep)
        .map(|tokens| tokens.access)
}

/// Like [`sign_in`], asking for [`SCOPES`] plus `extra` (space-separated), such
/// as `MailboxSettings.Read` to read inbox rules.
pub fn sign_in_with<T: Transport>(
    transport: &T,
    client_id: &str,
    tenant: &str,
    extra: &str,
    prompt: &mut dyn FnMut(&DeviceCodePrompt),
) -> Result<String> {
    let scopes = format!("{SCOPES} {extra}");
    sign_in_with_sleep(transport, client_id, tenant, &scopes, prompt, thread::sleep)
        .map(|tokens| tokens.access)
}

/// Like [`sign_in`], but also asks for a refresh token ([`LONG_RUN_SCOPES`]), so
/// that [`refresh`] can renew the access token during a long run.
pub fn sign_in_for_long_run<T: Transport>(
    transport: &T,
    client_id: &str,
    tenant: &str,
    prompt: &mut dyn FnMut(&DeviceCodePrompt),
) -> Result<Tokens> {
    sign_in_with_sleep(
        transport,
        client_id,
        tenant,
        LONG_RUN_SCOPES,
        prompt,
        thread::sleep,
    )
}

/// Signs in to change the mailbox ([`WRITE_SCOPES`]), with a refresh token for
/// [`refresh_with`]. The user is asked to consent to `Mail.ReadWrite` once.
pub fn sign_in_to_write<T: Transport>(
    transport: &T,
    client_id: &str,
    tenant: &str,
    prompt: &mut dyn FnMut(&DeviceCodePrompt),
) -> Result<Tokens> {
    sign_in_with_sleep(
        transport,
        client_id,
        tenant,
        WRITE_SCOPES,
        prompt,
        thread::sleep,
    )
}

/// Gets new tokens from a refresh token, without the user.
pub fn refresh<T: Transport>(
    transport: &T,
    client_id: &str,
    tenant: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    refresh_with(transport, client_id, tenant, refresh_token, LONG_RUN_SCOPES)
}

/// Like [`refresh`], for tokens signed in with other `scopes` (such as
/// [`WRITE_SCOPES`]).
pub fn refresh_with<T: Transport>(
    transport: &T,
    client_id: &str,
    tenant: &str,
    refresh_token: &str,
    scopes: &str,
) -> Result<Tokens> {
    let (status, token) = post_form(
        transport,
        &format!("{}/token", authority(tenant)),
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("scope", scopes),
        ],
    )?;
    if status != 200 {
        return Err(Error::SignIn(describe(&token)));
    }
    tokens(&token)
}

fn authority(tenant: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0",
        encode(tenant)
    )
}

fn tokens(token: &Value) -> Result<Tokens> {
    Ok(Tokens {
        access: token["access_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::MalformedResponse("no access_token".into()))?,
        refresh: token["refresh_token"].as_str().map(str::to_owned),
    })
}

fn post_form<T: Transport>(
    transport: &T,
    url: &str,
    form: &[(&str, &str)],
) -> Result<(u16, Value)> {
    let body = form
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = transport.send(
        "POST",
        url,
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Accept", "application/json"),
        ],
        body.as_bytes(),
    )?;
    let status = response.status;
    let json = serde_json::from_slice(&response.body)
        .map_err(|e| Error::MalformedResponse(format!("HTTP {status} from sign-in: {e}")))?;
    Ok((status, json))
}

pub(crate) fn sign_in_with_sleep<T: Transport>(
    transport: &T,
    client_id: &str,
    tenant: &str,
    scopes: &str,
    prompt: &mut dyn FnMut(&DeviceCodePrompt),
    sleep: fn(Duration),
) -> Result<Tokens> {
    let base = authority(tenant);
    let (status, code) = post_form(
        transport,
        &format!("{base}/devicecode"),
        &[("client_id", client_id), ("scope", scopes)],
    )?;
    if status != 200 {
        return Err(Error::SignIn(describe(&code)));
    }
    let text = |key: &str| code[key].as_str().map(str::to_owned);
    let device_code =
        text("device_code").ok_or_else(|| Error::MalformedResponse("no device_code".into()))?;
    prompt(&DeviceCodePrompt {
        verification_uri: text("verification_uri").unwrap_or_default(),
        user_code: text("user_code").unwrap_or_default(),
        message: text("message").unwrap_or_default(),
    });
    let mut interval = Duration::from_secs(code["interval"].as_u64().unwrap_or(5));
    let deadline = Instant::now() + Duration::from_secs(code["expires_in"].as_u64().unwrap_or(900));

    loop {
        sleep(interval);
        let (status, token) = post_form(
            transport,
            &format!("{base}/token"),
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", &device_code),
            ],
        )?;
        if status == 200 {
            return tokens(&token);
        }
        match token["error"].as_str() {
            Some("authorization_pending") if Instant::now() < deadline => {}
            Some("slow_down") if Instant::now() < deadline => interval += Duration::from_secs(5),
            _ => return Err(Error::SignIn(describe(&token))),
        }
    }
}

fn describe(response: &Value) -> String {
    match (
        response["error"].as_str(),
        response["error_description"].as_str(),
    ) {
        (Some(error), Some(description)) => {
            format!(
                "{error}: {}",
                description.lines().next().unwrap_or(description)
            )
        }
        (Some(error), None) => error.to_owned(),
        _ => "unexpected answer".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceCodePrompt, LONG_RUN_SCOPES, SCOPES, refresh, sign_in_with_sleep};
    use crate::testing::FakeTransport;

    const CODE: &str = r#"{"device_code":"DC","user_code":"ABCD-EFGH","verification_uri":"https://www.microsoft.com/link","expires_in":900,"interval":5,"message":"To sign in, open https://www.microsoft.com/link and enter ABCD-EFGH."}"#;

    #[test]
    fn polls_until_the_user_signs_in() {
        let fake = FakeTransport::new([
            (200, CODE.into()),
            (400, r#"{"error":"authorization_pending"}"#.into()),
            (400, r#"{"error":"slow_down"}"#.into()),
            (
                200,
                r#"{"token_type":"Bearer","access_token":"TOKEN","expires_in":3600}"#.into(),
            ),
        ]);
        let mut shown = None;
        let token = sign_in_with_sleep(
            &&fake,
            "CLIENT",
            "consumers",
            SCOPES,
            &mut |p| shown = Some(p.clone()),
            |_| {},
        )
        .unwrap();
        assert_eq!(token.access, "TOKEN");
        assert!(token.refresh.is_none());
        assert_eq!(
            shown.unwrap(),
            DeviceCodePrompt {
                verification_uri: "https://www.microsoft.com/link".into(),
                user_code: "ABCD-EFGH".into(),
                message: "To sign in, open https://www.microsoft.com/link and enter ABCD-EFGH."
                    .into(),
            }
        );
        let requests = fake.requests.borrow();
        assert_eq!(
            requests[0].1,
            "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode"
        );
        assert!(
            requests[0].2.contains("client_id=CLIENT")
                && requests[0].2.contains("scope=Mail.Read%20")
        );
        assert!(requests[3].2.contains("device_code=DC"));
        assert!(!requests[0].2.contains("offline_access"));
    }

    #[test]
    fn a_refused_sign_in_is_an_error() {
        let fake = FakeTransport::new([
            (200, CODE.into()),
            (400, r#"{"error":"access_denied","error_description":"AADSTS70000: The user denied.\r\nTrace ID: x"}"#.into()),
        ]);
        let error =
            sign_in_with_sleep(&&fake, "C", "consumers", SCOPES, &mut |_| {}, |_| {}).unwrap_err();
        assert_eq!(
            error.to_string(),
            "sign-in failed: access_denied: AADSTS70000: The user denied."
        );
    }

    #[test]
    fn an_unknown_client_id_fails_before_prompting() {
        let fake = FakeTransport::new([(
            400,
            r#"{"error":"unauthorized_client","error_description":"AADSTS700016: Application not found."}"#.into(),
        )]);
        let mut prompted = false;
        assert!(
            sign_in_with_sleep(
                &&fake,
                "C",
                "consumers",
                SCOPES,
                &mut |_| prompted = true,
                |_| {}
            )
            .is_err()
        );
        assert!(!prompted);
    }

    #[test]
    fn a_long_run_sign_in_keeps_the_refresh_token_and_can_renew() {
        let fake = FakeTransport::new([
            (200, CODE.into()),
            (
                200,
                r#"{"access_token":"A1","refresh_token":"R1","expires_in":3600}"#.into(),
            ),
            (
                200,
                r#"{"access_token":"A2","refresh_token":"R2","expires_in":3600}"#.into(),
            ),
        ]);
        let first = sign_in_with_sleep(
            &&fake,
            "C",
            "consumers",
            LONG_RUN_SCOPES,
            &mut |_| {},
            |_| {},
        )
        .unwrap();
        assert_eq!(first.refresh.as_deref(), Some("R1"));
        let renewed = refresh(&&fake, "C", "consumers", "R1").unwrap();
        assert_eq!(
            (renewed.access.as_str(), renewed.refresh.as_deref()),
            ("A2", Some("R2"))
        );
        let requests = fake.requests.borrow();
        assert!(requests[0].2.contains("offline_access"));
        assert!(
            requests[2].2.contains("grant_type=refresh_token")
                && requests[2].2.contains("refresh_token=R1")
        );
    }
}
