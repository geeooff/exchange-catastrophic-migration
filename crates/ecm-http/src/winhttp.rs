//! HTTPS client on WinHTTP, with NTLM/Negotiate.
//!
//! One session and one connection per client. WinHTTP keeps the connection alive,
//! and NTLM authenticates a connection rather than a request, so after the first
//! 401 challenge later requests usually go through directly.

use std::ffi::c_void;

use windows::{
    Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD,
        WINHTTP_AUTH_SCHEME_NEGOTIATE, WINHTTP_AUTH_SCHEME_NTLM, WINHTTP_AUTH_TARGET_SERVER,
        WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_RAW_HEADERS_CRLF,
        WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect,
        WinHttpOpen, WinHttpOpenRequest, WinHttpQueryAuthSchemes, WinHttpQueryDataAvailable,
        WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
        WinHttpSetCredentials, WinHttpSetTimeouts,
    },
    core::{HSTRING, PCWSTR},
};

use crate::{Credential, Error, HttpsUrl, Response, Result};

/// An owned WinHTTP handle, closed on drop.
struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, call: &'static str) -> Result<Self> {
        if raw.is_null() {
            Err(last_error(call))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from WinHTTP, is not null, and is closed once.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn last_error(call: &'static str) -> Error {
    Error::Windows {
        call,
        source: std::io::Error::last_os_error(),
    }
}

fn windows_error(call: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |error| Error::Windows {
        call,
        source: error.into(),
    }
}

/// An HTTPS client for one server, answering NTLM/Negotiate challenges with a
/// [`Credential`] when it has one.
pub struct HttpClient {
    // Field order matters: the connection is closed before its session.
    connection: Handle,
    _session: Handle,
    credential: Option<Credential>,
    url: HttpsUrl,
}

impl HttpClient {
    /// Opens a session and a connection to the server of `url`.
    pub fn new(url: HttpsUrl, credential: Credential) -> Result<Self> {
        Self::open(url, Some(credential))
    }

    /// Opens a session and a connection to the server of `url`, without a
    /// credential: for servers authenticated by headers, such as OAuth bearer
    /// tokens.
    pub fn anonymous(url: HttpsUrl) -> Result<Self> {
        Self::open(url, None)
    }

    fn open(url: HttpsUrl, credential: Option<Credential>) -> Result<Self> {
        // SAFETY: the agent string outlives the call; null proxy names are allowed
        // with the automatic proxy access type.
        let session = Handle::new(
            unsafe {
                WinHttpOpen(
                    &HSTRING::from(concat!("ecm/", env!("CARGO_PKG_VERSION"))),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    0,
                )
            },
            "WinHttpOpen",
        )?;
        // Resolve, connect, send, receive. EWS answers to large exports slowly.
        // SAFETY: `session` is a valid session handle.
        unsafe { WinHttpSetTimeouts(session.0, 30_000, 30_000, 120_000, 600_000) }
            .map_err(windows_error("WinHttpSetTimeouts"))?;
        // SAFETY: `session` is valid and the host name outlives the call.
        let connection = Handle::new(
            unsafe { WinHttpConnect(session.0, &HSTRING::from(url.host.as_str()), url.port, 0) },
            "WinHttpConnect",
        )?;
        Ok(Self {
            connection,
            _session: session,
            credential,
            url,
        })
    }

    /// The user name requests authenticate as, empty without a credential.
    pub fn user(&self) -> &str {
        self.credential.as_ref().map_or("", Credential::user)
    }

    /// The URL requests are sent to.
    pub fn url(&self) -> &HttpsUrl {
        &self.url
    }

    /// Sends a POST request to the client's URL and returns the response, whatever
    /// its status, except a 401 that persists once the credential was offered.
    pub fn post(&self, content_type: &str, body: &[u8]) -> Result<Response> {
        self.request(
            "POST",
            &self.url.path,
            &[("Content-Type", content_type)],
            body,
        )
    }

    /// Sends a request to `path` (path and query) on the client's server and
    /// returns the response, whatever its status, except a 401 that persists once
    /// the credential was offered. Header values must not contain line breaks.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response> {
        if headers
            .iter()
            .any(|(name, value)| name.contains(['\r', '\n']) || value.contains(['\r', '\n']))
        {
            return Err(Error::Windows {
                call: "WinHttpAddRequestHeaders",
                source: std::io::Error::other("line break in a header"),
            });
        }
        let length = u32::try_from(body.len()).map_err(|_| Error::Windows {
            call: "WinHttpSendRequest",
            source: std::io::Error::other("request body larger than 4 GB"),
        })?;
        // SAFETY: `connection` is valid; verb and path outlive the call; null
        // version, referrer and accept types select WinHTTP's defaults.
        let request = Handle::new(
            unsafe {
                WinHttpOpenRequest(
                    self.connection.0,
                    &HSTRING::from(method),
                    &HSTRING::from(path),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    std::ptr::null(),
                    WINHTTP_FLAG_SECURE,
                )
            },
            "WinHttpOpenRequest",
        )?;
        if !headers.is_empty() {
            let headers: Vec<u16> = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect::<String>()
                .encode_utf16()
                .collect();
            // SAFETY: `request` is valid and `headers` outlives the call.
            unsafe { WinHttpAddRequestHeaders(request.0, &headers, WINHTTP_ADDREQ_FLAG_ADD) }
                .map_err(windows_error("WinHttpAddRequestHeaders"))?;
        }

        let mut offered_credential = false;
        loop {
            // SAFETY: `request` is valid and `body` outlives the call, which sends
            // all of it since the optional and total lengths are equal.
            unsafe {
                WinHttpSendRequest(
                    request.0,
                    None,
                    Some(body.as_ptr().cast()),
                    length,
                    length,
                    0,
                )
            }
            .map_err(windows_error("WinHttpSendRequest"))?;
            // SAFETY: `request` is valid and the reserved pointer must be null.
            unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }
                .map_err(windows_error("WinHttpReceiveResponse"))?;

            let status = status_code(&request)?;
            if status != 401 || self.credential.is_none() {
                return Ok(Response {
                    status,
                    headers: raw_headers(&request)?,
                    body: read_body(&request)?,
                });
            }
            if offered_credential {
                return Err(Error::Unauthorized {
                    user: self.user().to_owned(),
                });
            }
            // The challenge body is discarded before the request is sent again.
            read_body(&request)?;
            self.offer_credential(&request)?;
            offered_credential = true;
        }
    }

    /// Answers a 401 challenge: NTLM when the server offers it, else Negotiate.
    fn offer_credential(&self, request: &Handle) -> Result<()> {
        let Some(credential) = &self.credential else {
            return Ok(());
        };
        let (mut supported, mut first, mut target) = (0u32, 0u32, 0u32);
        // SAFETY: `request` holds a 401 response and the three outputs are valid.
        unsafe { WinHttpQueryAuthSchemes(request.0, &mut supported, &mut first, &mut target) }
            .map_err(windows_error("WinHttpQueryAuthSchemes"))?;
        let scheme = if supported & WINHTTP_AUTH_SCHEME_NTLM.0 != 0 {
            WINHTTP_AUTH_SCHEME_NTLM.0
        } else if supported & WINHTTP_AUTH_SCHEME_NEGOTIATE.0 != 0 {
            WINHTTP_AUTH_SCHEME_NEGOTIATE.0
        } else {
            return Err(Error::Unauthorized {
                user: credential.user().to_owned(),
            });
        };
        let mut password = credential.password_utf16().to_vec();
        password.push(0);
        // SAFETY: `request` is valid; both strings are NUL-terminated and outlive
        // the call, which copies them.
        let set = unsafe {
            WinHttpSetCredentials(
                request.0,
                WINHTTP_AUTH_TARGET_SERVER,
                scheme,
                &HSTRING::from(credential.user()),
                PCWSTR(password.as_ptr()),
                std::ptr::null_mut(),
            )
        };
        password.iter_mut().for_each(|c| *c = 0);
        set.map_err(windows_error("WinHttpSetCredentials"))
    }
}

fn raw_headers(request: &Handle) -> Result<String> {
    let mut size = 0u32;
    // SAFETY: `request` holds a response; with no buffer, WinHTTP only reports the
    // size needed, in bytes, and fails with ERROR_INSUFFICIENT_BUFFER.
    let _ = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_RAW_HEADERS_CRLF,
            PCWSTR::null(),
            None,
            &mut size,
            std::ptr::null_mut(),
        )
    };
    if size == 0 {
        return Ok(String::new());
    }
    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    // SAFETY: the buffer holds `size` bytes, as WinHTTP asked for.
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_RAW_HEADERS_CRLF,
            PCWSTR::null(),
            Some(buffer.as_mut_ptr().cast()),
            &mut size,
            std::ptr::null_mut(),
        )
    }
    .map_err(windows_error("WinHttpQueryHeaders"))?;
    buffer.truncate(size as usize / 2);
    Ok(String::from_utf16_lossy(&buffer))
}

fn status_code(request: &Handle) -> Result<u16> {
    let mut status = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `request` holds a response; the buffer is a u32 of the size given.
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut size,
            std::ptr::null_mut(),
        )
    }
    .map_err(windows_error("WinHttpQueryHeaders"))?;
    u16::try_from(status).map_err(|_| Error::Windows {
        call: "WinHttpQueryHeaders",
        source: std::io::Error::other(format!("invalid HTTP status {status}")),
    })
}

fn read_body(request: &Handle) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut available = 0u32;
        // SAFETY: `request` holds a response and `available` is a valid output.
        unsafe { WinHttpQueryDataAvailable(request.0, &mut available) }
            .map_err(windows_error("WinHttpQueryDataAvailable"))?;
        if available == 0 {
            return Ok(body);
        }
        let start = body.len();
        body.resize(start + available as usize, 0);
        let mut read = 0u32;
        // SAFETY: the buffer has `available` writable bytes from `start`.
        unsafe {
            WinHttpReadData(
                request.0,
                body[start..].as_mut_ptr().cast(),
                available,
                &mut read,
            )
        }
        .map_err(windows_error("WinHttpReadData"))?;
        body.truncate(start + read as usize);
    }
}
