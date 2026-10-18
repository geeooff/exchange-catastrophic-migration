//! OAuth 2.0 device code sign-in with the Microsoft identity platform.

use std::{
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{Error, Result, Transport, encode};

/// Read-only permissions asked for. `offline_access` is not requested: the token
/// lives for one run only.
pub const SCOPES: &str = "Mail.Read Contacts.Read Calendars.Read Tasks.Read";

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
    sign_in_with_sleep(transport, client_id, tenant, prompt, thread::sleep)
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
    let (status, response) = transport.send(
        "POST",
        url,
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Accept", "application/json"),
        ],
        body.as_bytes(),
    )?;
    let json = serde_json::from_slice(&response)
        .map_err(|e| Error::MalformedResponse(format!("HTTP {status} from sign-in: {e}")))?;
    Ok((status, json))
}

pub(crate) fn sign_in_with_sleep<T: Transport>(
    transport: &T,
    client_id: &str,
    tenant: &str,
    prompt: &mut dyn FnMut(&DeviceCodePrompt),
    sleep: fn(Duration),
) -> Result<String> {
    let base = format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0",
        encode(tenant)
    );
    let (status, code) = post_form(
        transport,
        &format!("{base}/devicecode"),
        &[("client_id", client_id), ("scope", SCOPES)],
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
            return token["access_token"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::MalformedResponse("no access_token".into()));
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
    use super::{DeviceCodePrompt, sign_in_with_sleep};
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
            &mut |p| shown = Some(p.clone()),
            |_| {},
        )
        .unwrap();
        assert_eq!(token, "TOKEN");
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
        let error = sign_in_with_sleep(&&fake, "C", "consumers", &mut |_| {}, |_| {}).unwrap_err();
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
            sign_in_with_sleep(&&fake, "C", "consumers", &mut |_| prompted = true, |_| {}).is_err()
        );
        assert!(!prompted);
    }
}
