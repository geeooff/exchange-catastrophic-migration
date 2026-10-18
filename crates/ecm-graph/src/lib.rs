//! Read-only Microsoft Graph source: Outlook.com (personal Microsoft accounts) and
//! Microsoft 365 mailboxes.
//!
//! Sign-in uses the OAuth 2.0 device code flow: the user opens a Microsoft page,
//! types a short code and signs in there. This tool never sees the password, asks
//! only for read permissions, and keeps the token in memory for the run.
//!
//! It needs the client id of an app registration that allows personal Microsoft
//! accounts and public client flows, with the delegated `Mail.Read` permission.

#![forbid(unsafe_code)]

use std::{thread, time::Duration};

use serde_json::Value;

pub mod auth;
pub mod mail;

/// Errors from Microsoft Graph and its sign-in.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The HTTP transport failed.
    #[error("{0}")]
    Transport(String),
    /// Sign-in failed or was refused.
    #[error("sign-in failed: {0}")]
    SignIn(String),
    /// Graph answered with an error.
    #[error("Microsoft Graph error {status} {code}: {message}")]
    Graph {
        /// HTTP status.
        status: u16,
        /// Graph error code.
        code: String,
        /// Error message.
        message: String,
    },
    /// The response is not the JSON expected.
    #[error("unexpected Microsoft Graph response: {0}")]
    MalformedResponse(String),
    /// Graph kept throttling.
    #[error("Microsoft Graph is still throttling after {0} attempts")]
    Throttled(u32),
}

/// Result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// An HTTP response.
#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    /// HTTP status.
    pub status: u16,
    /// `WWW-Authenticate` header, when the server sent one.
    pub www_authenticate: Option<String>,
    /// Body.
    pub body: Vec<u8>,
}

/// Sends HTTPS requests to any host. Implemented over WinHTTP on Windows, and by
/// fakes in tests.
pub trait Transport {
    /// Sends `method` to the full `url` with `headers` and `body`.
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<HttpResponse>;
}

#[cfg(windows)]
mod winhttp {
    use std::{cell::RefCell, collections::HashMap};

    use ecm_http::{HttpClient, HttpsUrl};

    use super::{Error, HttpResponse, Result, Transport};

    /// [`Transport`] over WinHTTP, one connection per host.
    #[derive(Default)]
    pub struct WinHttpTransport {
        clients: RefCell<HashMap<(String, u16), HttpClient>>,
    }

    impl Transport for WinHttpTransport {
        fn send(
            &self,
            method: &str,
            url: &str,
            headers: &[(&str, &str)],
            body: &[u8],
        ) -> Result<HttpResponse> {
            let url = HttpsUrl::parse(url).map_err(|e| Error::Transport(e.to_string()))?;
            let key = (url.host.clone(), url.port);
            let mut clients = self.clients.borrow_mut();
            if !clients.contains_key(&key) {
                let client = HttpClient::anonymous(url.clone())
                    .map_err(|e| Error::Transport(e.to_string()))?;
                clients.insert(key.clone(), client);
            }
            clients[&key]
                .request(method, &url.path, headers, body)
                .map(|response| HttpResponse {
                    status: response.status,
                    www_authenticate: response.header("WWW-Authenticate").map(str::to_owned),
                    body: response.body,
                })
                .map_err(|e| Error::Transport(e.to_string()))
        }
    }
}

#[cfg(windows)]
pub use winhttp::WinHttpTransport;

/// Graph's base URL.
pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";

/// How many times a request is sent while Graph throttles it.
const MAX_ATTEMPTS: u32 = 6;

/// A signed-in Graph client.
pub struct GraphClient<T> {
    transport: T,
    token: String,
    sleep: fn(Duration),
}

impl<T: Transport> GraphClient<T> {
    /// A client sending `token` as its bearer token.
    pub fn new(transport: T, token: String) -> Self {
        Self {
            transport,
            token,
            sleep: thread::sleep,
        }
    }

    /// Replaces how the client waits when throttled. Tests use it to avoid waiting.
    pub fn with_sleep(mut self, sleep: fn(Duration)) -> Self {
        self.sleep = sleep;
        self
    }

    /// GETs a Graph URL (absolute, or relative to [`GRAPH`]) and parses its JSON,
    /// waiting and retrying when throttled (HTTP 429 or 503).
    pub fn get(&self, url: &str) -> Result<Value> {
        let url = if url.starts_with("https://") {
            url.to_owned()
        } else {
            format!("{GRAPH}{url}")
        };
        let authorization = format!("Bearer {}", self.token);
        let headers = [
            ("Authorization", authorization.as_str()),
            ("Accept", "application/json"),
            ("Prefer", "IdType=\"ImmutableId\""),
        ];
        for attempt in 1..=MAX_ATTEMPTS {
            let response = self.transport.send("GET", &url, &headers, &[])?;
            let (status, body) = (response.status, response.body);
            if status == 429 || status == 503 {
                if attempt < MAX_ATTEMPTS {
                    (self.sleep)(Duration::from_secs(5 * u64::from(attempt)));
                }
                continue;
            }
            if !(200..300).contains(&status) {
                let json: Value = serde_json::from_slice(&body).unwrap_or_default();
                let error = &json["error"];
                let mut message = error["message"].as_str().unwrap_or_default().to_owned();
                if let Some(challenge) = response.www_authenticate.filter(|_| status == 401) {
                    message = format!("{message} (WWW-Authenticate: {challenge})");
                }
                return Err(Error::Graph {
                    status,
                    code: error["code"].as_str().unwrap_or_default().to_owned(),
                    message,
                });
            }
            let json: Value = serde_json::from_slice(&body)
                .map_err(|e| Error::MalformedResponse(format!("HTTP {status}: {e}")))?;
            return Ok(json);
        }
        Err(Error::Throttled(MAX_ATTEMPTS))
    }

    /// Every element of a paged collection, following `@odata.nextLink`.
    pub fn get_all(&self, url: &str) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        let mut next = Some(url.to_owned());
        while let Some(url) = next {
            let page = self.get(&url)?;
            match page["value"].as_array() {
                Some(page_values) => values.extend(page_values.iter().cloned()),
                None => return Err(Error::MalformedResponse("collection without value".into())),
            }
            next = page["@odata.nextLink"].as_str().map(str::to_owned);
        }
        Ok(values)
    }
}

/// Percent-encodes a query or form value (RFC 3986 unreserved characters kept).
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod testing {
    //! A fake transport that replays canned responses and records requests.

    use std::{cell::RefCell, collections::VecDeque};

    use super::{HttpResponse, Result, Transport};

    #[derive(Default)]
    pub struct FakeTransport {
        pub responses: RefCell<VecDeque<(u16, String)>>,
        pub requests: RefCell<Vec<(String, String, String)>>,
    }

    impl FakeTransport {
        pub fn new(responses: impl IntoIterator<Item = (u16, String)>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                requests: RefCell::default(),
            }
        }
    }

    impl Transport for &FakeTransport {
        fn send(
            &self,
            method: &str,
            url: &str,
            _: &[(&str, &str)],
            body: &[u8],
        ) -> Result<HttpResponse> {
            self.requests.borrow_mut().push((
                method.to_owned(),
                url.to_owned(),
                String::from_utf8_lossy(body).into_owned(),
            ));
            let (status, body) = self
                .responses
                .borrow_mut()
                .pop_front()
                .expect("no canned response left");
            Ok(HttpResponse {
                status,
                www_authenticate: None,
                body: body.into_bytes(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, GraphClient, encode, testing::FakeTransport};

    #[test]
    fn follows_next_links_and_retries_when_throttled() {
        let fake = FakeTransport::new([
            (200, r#"{"value":[1,2],"@odata.nextLink":"https://graph.microsoft.com/v1.0/me/x?$skip=2"}"#.into()),
            (429, String::new()),
            (200, r#"{"value":[3]}"#.into()),
        ]);
        let client = GraphClient::new(&fake, "t".into()).with_sleep(|_| {});
        let values = client.get_all("/me/x").unwrap();
        assert_eq!(values, [1, 2, 3]);
        let requests = fake.requests.borrow();
        assert_eq!(requests[0].1, "https://graph.microsoft.com/v1.0/me/x");
        assert_eq!(
            requests[2].1,
            "https://graph.microsoft.com/v1.0/me/x?$skip=2"
        );
    }

    #[test]
    fn graph_errors_carry_code_and_message() {
        let fake = FakeTransport::new([(
            403,
            r#"{"error":{"code":"ErrorAccessDenied","message":"Access is denied."}}"#.into(),
        )]);
        let error = GraphClient::new(&fake, "t".into()).get("/me").unwrap_err();
        assert!(
            matches!(error, Error::Graph { status: 403, code, .. } if code == "ErrorAccessDenied")
        );
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(
            encode("offline_access Mail.Read"),
            "offline_access%20Mail.Read"
        );
        assert_eq!(encode("a=b&c/é"), "a%3Db%26c%2F%C3%A9");
    }
}
