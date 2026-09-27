//! Linear: OAuth as an app actor, the Keychain-held credential, and the
//! GraphQL client the ticker reads and writes through. Only the ticker writes
//! to Linear; agents leave requests in a run's outbox.

pub mod api;
pub mod client;
pub mod credentials;
pub mod oauth;
pub mod task;
pub mod transport;

use zeroize::Zeroizing;

/// Coarse failures of a Linear request. No token is ever part of an error; a
/// GraphQL error keeps only a short message for the ticker's log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiError {
    /// The local configuration or a request argument was invalid.
    Configuration,
    /// The credential could not be leased or refreshed.
    Credential(credentials::CredentialError),
    /// The HTTPS client could not be configured.
    ClientConfiguration,
    /// The request failed before a response was received: its outcome is unknown.
    RequestFailed,
    /// The response exceeded the parser bound.
    ResponseTooLarge,
    /// The response did not contain exactly one application/json media type.
    ContentType,
    /// The HTTP status was not a successful GraphQL response.
    HttpStatus(u16),
    /// The GraphQL response reported an error.
    Graphql(String),
    /// The authenticated viewer is not this plugin's app user.
    ActorIdentityMismatch,
    /// A response field was missing or malformed.
    ReadFieldsInvalid,
    /// Linear rate-limited the request (HTTP 429, or a GraphQL error whose
    /// `extensions.code` is `RATELIMITED`). It is never a definitive refusal.
    RateLimited,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration => {
                formatter.write_str("the Linear request configuration is invalid")
            }
            Self::Credential(error) => error.fmt(formatter),
            Self::ClientConfiguration => {
                formatter.write_str("the Linear HTTPS client is unavailable")
            }
            Self::RequestFailed => {
                formatter.write_str("the Linear request failed before a response")
            }
            Self::ResponseTooLarge => formatter.write_str("the Linear response is too large"),
            Self::ContentType => formatter.write_str("the Linear response content type is invalid"),
            Self::HttpStatus(status) => write!(formatter, "Linear answered HTTP {status}"),
            Self::Graphql(message) => write!(formatter, "Linear reported an error: {message}"),
            Self::ActorIdentityMismatch => {
                formatter.write_str("the Linear viewer is not this plugin's app user; log in again")
            }
            Self::ReadFieldsInvalid => {
                formatter.write_str("a Linear response field is missing or malformed")
            }
            Self::RateLimited => formatter.write_str("Linear rate-limited the request"),
        }
    }
}

impl std::error::Error for ApiError {}

/// The result of a read whose viewer was checked to be an app actor. The
/// credential manager binds the viewer ID to the stored credential before the
/// value is released.
#[derive(Eq, PartialEq)]
pub(crate) struct VerifiedReadOutcome<T = ()> {
    viewer_id: Zeroizing<String>,
    value: T,
}

impl VerifiedReadOutcome<()> {
    #[cfg(test)]
    pub(crate) fn for_test(viewer_id: &str) -> Self {
        Self::with_value(Zeroizing::new(viewer_id.to_owned()), ())
    }
}

impl<T> VerifiedReadOutcome<T> {
    pub(crate) fn with_value(viewer_id: Zeroizing<String>, value: T) -> Self {
        Self { viewer_id, value }
    }

    pub(crate) fn into_parts(self) -> (Zeroizing<String>, T) {
        (self.viewer_id, self.value)
    }
}

impl<T> std::fmt::Debug for VerifiedReadOutcome<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedReadOutcome")
            .field("viewer_id", &"[redacted]")
            .finish()
    }
}
