//! HTTP transport with NTLM/Negotiate authentication.
//!
//! The first implementation uses WinHTTP through Microsoft's `windows` crate, which
//! handles Negotiate and NTLM natively. Credentials come from the Windows Credential
//! Manager, never from files or the command line.
//!
//! This is the only crate where `unsafe` code is allowed, and only for calls into
//! the Windows API, each with a `// SAFETY:` comment.

#![cfg_attr(not(windows), forbid(unsafe_code))]

use std::fmt;

#[cfg(windows)]
mod credential;
#[cfg(windows)]
mod winhttp;

#[cfg(windows)]
pub use credential::read_generic_credential;
#[cfg(windows)]
pub use winhttp::HttpClient;

/// Errors from the HTTP transport.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Windows Credential Manager has no generic credential with this name.
    #[error(
        "no credential named {0:?} in the Windows Credential Manager; create it with: cmdkey /generic:{0} /user:<mailbox address> /pass"
    )]
    CredentialNotFound(String),
    /// The credential exists but cannot be used.
    #[error("credential {target:?} cannot be used: {reason}")]
    InvalidCredential {
        /// Credential name.
        target: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The URL is not an `https://` URL this client can use.
    #[error("invalid URL {url:?}: {reason}")]
    InvalidUrl {
        /// The URL.
        url: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The server rejected the credential.
    #[error("the server rejected the credential for {user:?} (HTTP 401)")]
    Unauthorized {
        /// The user name that was sent.
        user: String,
    },
    /// A Windows API call failed.
    #[error("{call} failed: {source}")]
    Windows {
        /// The API function.
        call: &'static str,
        /// The Windows error.
        source: std::io::Error,
    },
}

/// Result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A user name and password from the Credential Manager.
///
/// The password is kept as UTF-16, the form Windows stores and WinHTTP expects, and
/// is overwritten with zeros when the credential is dropped. It is never printed.
pub struct Credential {
    user: String,
    password: Vec<u16>,
}

impl Credential {
    /// Builds a credential from a user name and a UTF-16 password.
    pub fn new(user: String, password: Vec<u16>) -> Self {
        Self { user, password }
    }

    /// The user name.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The password, as UTF-16 without a terminating NUL.
    pub fn password_utf16(&self) -> &[u16] {
        &self.password
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.password.iter_mut().for_each(|c| *c = 0);
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("user", &self.user)
            .field("password", &"<hidden>")
            .finish()
    }
}

/// An HTTP response.
#[derive(Debug)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response body.
    pub body: Vec<u8>,
}

/// The parts of an `https://host[:port]/path` URL this client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpsUrl {
    /// Host name.
    pub host: String,
    /// TCP port, 443 unless given.
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl HttpsUrl {
    /// Parses an `https://` URL. Plain `http://` is refused: credentials must not
    /// travel unencrypted.
    pub fn parse(url: &str) -> Result<Self> {
        let invalid = |reason| Error::InvalidUrl {
            url: url.to_owned(),
            reason,
        };
        let rest = url
            .strip_prefix("https://")
            .ok_or_else(|| invalid("only https:// URLs are accepted"))?;
        let (authority, path) = match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash..]),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err(invalid("credentials do not belong in the URL"));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse()
                    .map_err(|_| invalid("the port is not a number"))?,
            ),
            None => (authority, 443),
        };
        if host.is_empty() {
            return Err(invalid("the host is missing"));
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Credential, HttpsUrl};

    #[test]
    fn parses_an_ews_url() {
        let url = HttpsUrl::parse("https://mail.example.com/EWS/Exchange.asmx").unwrap();
        assert_eq!(url.host, "mail.example.com");
        assert_eq!(url.port, 443);
        assert_eq!(url.path, "/EWS/Exchange.asmx");
    }

    #[test]
    fn parses_a_port_and_a_bare_host() {
        let url = HttpsUrl::parse("https://localhost:8443").unwrap();
        assert_eq!(
            (url.host.as_str(), url.port, url.path.as_str()),
            ("localhost", 8443, "/")
        );
    }

    #[test]
    fn refuses_plain_http_and_embedded_credentials() {
        assert!(HttpsUrl::parse("http://mail.example.com/EWS/Exchange.asmx").is_err());
        assert!(HttpsUrl::parse("https://user:pass@mail.example.com/EWS").is_err());
        assert!(HttpsUrl::parse("https:///EWS").is_err());
    }

    #[test]
    fn the_password_is_never_printed() {
        let credential = Credential::new(
            "user@example.test".into(),
            "secret".encode_utf16().collect(),
        );
        let printed = format!("{credential:?}");
        assert!(printed.contains("user@example.test"));
        assert!(!printed.contains("secret"));
    }
}
