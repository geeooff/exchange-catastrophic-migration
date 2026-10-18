//! Exchange Web Services (EWS) client: SOAP requests, paging, throttling and retries.
//!
//! Used both to read a source mailbox and to read and repair a target mailbox.
//! Requests are written by hand and responses parsed into a small XML tree
//! ([`xml::Element`]): the operations this tool needs (`FindFolder`, `ExportItems`,
//! `UploadItems`, extended properties) are not covered by existing EWS crates.
//!
//! Every operation is a read, except those in [`write`](mod@write), which only create.

#![forbid(unsafe_code)]

use std::{thread, time::Duration};

pub mod backup;
pub mod details;
pub mod folders;
pub mod items;
pub mod pim;
pub mod write;
pub mod xml;

use xml::Element;

/// Errors from EWS.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The HTTP transport failed.
    #[error("{0}")]
    Transport(String),
    /// The server answered with an HTTP status that carries no SOAP response.
    #[error("the server answered HTTP {status}{}", body_hint(.body))]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// Start of the response body, for diagnosis.
        body: String,
    },
    /// The response is not the SOAP document expected.
    #[error("unexpected EWS response: {0}")]
    MalformedResponse(String),
    /// The server returned a SOAP fault.
    #[error("EWS fault {code}: {message}")]
    Fault {
        /// EWS response code, e.g. `ErrorInvalidRequest`.
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// An operation failed for the whole request.
    #[error("EWS error {code}: {message}")]
    Response {
        /// EWS response code, e.g. `ErrorFolderNotFound`.
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// The server kept asking to back off.
    #[error("the server is still busy after {0} attempts")]
    Busy(u32),
    /// A local file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

fn body_hint(body: &str) -> String {
    if body.is_empty() {
        String::new()
    } else {
        format!(": {body}")
    }
}

/// Result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Sends a SOAP request and returns the HTTP status and body.
///
/// Implemented by [`ecm_http::HttpClient`] on Windows, and by fakes in tests.
pub trait Transport {
    /// Posts `body` as `text/xml` and returns the status and the response body.
    fn post_xml(&self, body: &[u8]) -> Result<(u16, Vec<u8>)>;
    /// The user the transport authenticates as, for messages.
    fn user(&self) -> &str;
}

#[cfg(windows)]
impl Transport for ecm_http::HttpClient {
    fn post_xml(&self, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        self.post("text/xml; charset=utf-8", body)
            .map(|response| (response.status, response.body))
            .map_err(|error| Error::Transport(error.to_string()))
    }

    fn user(&self) -> &str {
        ecm_http::HttpClient::user(self)
    }
}

/// The schema version requests ask for. Exchange 2013 SP1 is understood by every
/// server this tool targets, Exchange 2019 included.
const REQUEST_SERVER_VERSION: &str = "Exchange2013_SP1";

/// How many times a request is sent while the server says it is busy.
const MAX_ATTEMPTS: u32 = 8;

/// An EWS client over a [`Transport`].
pub struct EwsClient<T> {
    transport: T,
    sleep: fn(Duration),
}

impl<T: Transport> EwsClient<T> {
    /// A client over `transport`.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            sleep: thread::sleep,
        }
    }

    /// Replaces how the client waits when the server asks it to back off. Tests
    /// use it to avoid waiting.
    pub fn with_sleep(mut self, sleep: fn(Duration)) -> Self {
        self.sleep = sleep;
        self
    }

    /// The user requests authenticate as.
    pub fn user(&self) -> &str {
        self.transport.user()
    }

    /// Sends an operation and returns its response messages, one per item or
    /// folder of the request, in order. A message that failed is returned as is;
    /// the caller decides what a failed message means.
    ///
    /// `body` is the content of `soap:Body`, using the `m:` and `t:` prefixes.
    pub fn call(&self, body: &str) -> Result<Vec<ResponseMessage>> {
        let request = envelope(body);
        for attempt in 1..=MAX_ATTEMPTS {
            let (status, response) = self.transport.post_xml(request.as_bytes())?;
            let backoff = match parse_response(status, &response) {
                Ok(messages) => match messages.iter().find(|m| m.code == "ErrorServerBusy") {
                    Some(busy) => busy.backoff(),
                    None => return Ok(messages),
                },
                Err(Error::Fault { code, .. }) if code == "ErrorServerBusy" => {
                    backoff_from(&response)
                }
                Err(Error::HttpStatus { status: 503, .. }) => None,
                Err(error) => return Err(error),
            };
            if attempt < MAX_ATTEMPTS {
                (self.sleep)(
                    backoff
                        .unwrap_or(Duration::from_secs(5))
                        .max(Duration::from_secs(1)),
                );
            }
        }
        Err(Error::Busy(MAX_ATTEMPTS))
    }
}

/// One `*ResponseMessage` of a response.
#[derive(Debug, Clone)]
pub struct ResponseMessage {
    /// `Success`, `Warning` or `Error`.
    pub class: String,
    /// `NoError` or an EWS error code.
    pub code: String,
    /// The error message, if any.
    pub message: String,
    /// The whole element, for the operation to read its payload.
    pub element: Element,
}

impl ResponseMessage {
    /// Whether the operation succeeded for this item or folder.
    pub fn is_success(&self) -> bool {
        self.class != "Error"
    }

    /// Turns a failed message into an error.
    pub fn into_result(self) -> Result<Self> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(Error::Response {
                code: self.code,
                message: self.message,
            })
        }
    }

    fn backoff(&self) -> Option<Duration> {
        let mut values = Vec::new();
        self.element.descendants_named("Value", &mut values);
        values
            .into_iter()
            .find(|value| value.attribute("Name") == Some("BackOffMilliseconds"))
            .and_then(|value| value.text.trim().parse().ok())
            .map(Duration::from_millis)
    }
}

fn backoff_from(response: &[u8]) -> Option<Duration> {
    let root = xml::parse(response).ok()?;
    let mut values = Vec::new();
    root.descendants_named("Value", &mut values);
    values
        .into_iter()
        .find(|value| value.attribute("Name") == Some("BackOffMilliseconds"))
        .and_then(|value| value.text.trim().parse().ok())
        .map(Duration::from_millis)
}

/// Wraps an operation in a SOAP envelope.
pub fn envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/" xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types" xmlns:m="http://schemas.microsoft.com/exchange/services/2006/messages">
<soap:Header><t:RequestServerVersion Version="{REQUEST_SERVER_VERSION}"/></soap:Header>
<soap:Body>{body}</soap:Body>
</soap:Envelope>"#
    )
}

/// Parses a SOAP response into its response messages, or the fault it carries.
pub fn parse_response(status: u16, response: &[u8]) -> Result<Vec<ResponseMessage>> {
    let root = match xml::parse(response) {
        Ok(root) if root.name == "Envelope" => root,
        _ => {
            let body = String::from_utf8_lossy(&response[..response.len().min(200)]);
            return Err(if status == 200 {
                Error::MalformedResponse(format!("not a SOAP envelope: {body}"))
            } else {
                Error::HttpStatus {
                    status,
                    body: body.trim().to_owned(),
                }
            });
        }
    };
    let body = root
        .child("Body")
        .ok_or_else(|| Error::MalformedResponse("no soap:Body".into()))?;
    if let Some(fault) = body.child("Fault") {
        let code = fault
            .path(&["detail", "ResponseCode"])
            .map(|code| code.text.clone())
            .or_else(|| fault.child_text("faultcode").map(str::to_owned))
            .unwrap_or_default();
        let message = fault
            .path(&["detail", "Message"])
            .map(|message| message.text.clone())
            .or_else(|| fault.child_text("faultstring").map(str::to_owned))
            .unwrap_or_default();
        return Err(Error::Fault { code, message });
    }
    let operation = body
        .children
        .first()
        .ok_or_else(|| Error::MalformedResponse("empty soap:Body".into()))?;
    // Most operations answer a list of messages; a few (GetInboxRules,
    // UpdateInboxRules) are the message themselves.
    let messages: &[Element] = match operation.child("ResponseMessages") {
        Some(messages) => &messages.children,
        None if operation.attribute("ResponseClass").is_some() => std::slice::from_ref(operation),
        None => {
            return Err(Error::MalformedResponse(format!(
                "no ResponseMessages in {}",
                operation.name
            )));
        }
    };
    Ok(messages
        .iter()
        .map(|element| ResponseMessage {
            class: element
                .attribute("ResponseClass")
                .unwrap_or("Error")
                .to_owned(),
            code: element
                .child_text("ResponseCode")
                .unwrap_or_default()
                .to_owned(),
            message: element
                .child_text("MessageText")
                .unwrap_or_default()
                .to_owned(),
            element: element.clone(),
        })
        .collect())
}

#[cfg(test)]
pub(crate) mod testing {
    //! A fake transport that replays canned responses and records requests.

    use std::{cell::RefCell, collections::VecDeque};

    use super::{Result, Transport};

    pub struct FakeTransport {
        pub responses: RefCell<VecDeque<(u16, String)>>,
        pub requests: RefCell<Vec<String>>,
    }

    impl FakeTransport {
        pub fn new(responses: impl IntoIterator<Item = (u16, String)>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                requests: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for &FakeTransport {
        fn post_xml(&self, body: &[u8]) -> Result<(u16, Vec<u8>)> {
            self.requests
                .borrow_mut()
                .push(String::from_utf8(body.to_vec()).unwrap());
            let (status, body) = self
                .responses
                .borrow_mut()
                .pop_front()
                .expect("no canned response left");
            Ok((status, body.into_bytes()))
        }

        fn user(&self) -> &str {
            "user@example.test"
        }
    }

    /// A SOAP envelope around a response body.
    pub fn soap(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" xmlns:m="http://schemas.microsoft.com/exchange/services/2006/messages" xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types"><s:Body>{body}</s:Body></s:Envelope>"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Error, EwsClient,
        testing::{FakeTransport, soap},
    };

    const BUSY: &str = r#"<s:Fault><faultcode>a:ErrorServerBusy</faultcode><faultstring>The server cannot service this request right now.</faultstring><detail><e:ResponseCode xmlns:e="urn:e">ErrorServerBusy</e:ResponseCode><e:Message xmlns:e="urn:e">busy</e:Message><t:MessageXml><t:Value Name="BackOffMilliseconds">250</t:Value></t:MessageXml></detail></s:Fault>"#;
    const OK: &str = r#"<m:GetFolderResponse><m:ResponseMessages><m:GetFolderResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode></m:GetFolderResponseMessage></m:ResponseMessages></m:GetFolderResponse>"#;

    #[test]
    fn backs_off_while_the_server_is_busy() {
        let fake = FakeTransport::new([(500, soap(BUSY)), (503, String::new()), (200, soap(OK))]);
        let client = EwsClient::new(&fake).with_sleep(|_| {});
        let messages = client.call("<m:GetFolder/>").unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_success());
        assert_eq!(fake.requests.borrow().len(), 3);
    }

    #[test]
    fn gives_up_after_too_many_busy_answers() {
        let fake = FakeTransport::new(std::iter::repeat_n((500, soap(BUSY)), 8));
        let client = EwsClient::new(&fake).with_sleep(|_| {});
        assert!(matches!(client.call("<m:GetFolder/>"), Err(Error::Busy(8))));
    }

    #[test]
    fn reports_faults_and_non_soap_answers() {
        let fault = r#"<s:Fault><faultcode>a:ErrorSchemaValidation</faultcode><faultstring>The request failed schema validation.</faultstring></s:Fault>"#;
        let fake = FakeTransport::new([(500, soap(fault)), (404, "<html>Not found</html>".into())]);
        let client = EwsClient::new(&fake).with_sleep(|_| {});
        assert!(
            matches!(client.call("x"), Err(Error::Fault { code, .. }) if code == "a:ErrorSchemaValidation")
        );
        assert!(matches!(
            client.call("x"),
            Err(Error::HttpStatus { status: 404, .. })
        ));
    }

    #[test]
    fn requests_carry_the_server_version() {
        let fake = FakeTransport::new([(200, soap(OK))]);
        EwsClient::new(&fake).call("<m:GetFolder/>").unwrap();
        let request = &fake.requests.borrow()[0];
        assert!(request.contains(r#"<t:RequestServerVersion Version="Exchange2013_SP1"/>"#));
        assert!(request.contains("<soap:Body><m:GetFolder/></soap:Body>"));
    }
}
