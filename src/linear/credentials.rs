//! Local Linear OAuth credential lifecycle.
//!
//! This module is deliberately the only owner of persisted OAuth material.  It
//! keeps the access and refresh token in one bounded envelope, serializes all
//! lifecycle transitions under one in-process mutex and one user-owned
//! advisory lock, and never includes secret material in public diagnostics.
//!
//! The production store is the macOS Keychain on macOS and the Secret Service
//! (GNOME Keyring, KWallet) on Linux. Other hosts return
//! [`CredentialError::UnsupportedPlatform`] before reading a path,
//! environment variable, or secret.
//!
//! Ported from Nagi (`crates/nagi/src/linear/credentials.rs` at `ee7d657`).
//! The secret's service name is `dev.herdr-linear-agent.linear.oauth.v1`, with
//! one account per Linear workspace named after the workspace in the config,
//! and each workspace's advisory lock file lives in
//! `$XDG_STATE_HOME/herdr-linear-agent/` (see [`lock_path`]).

#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::linear::oauth::{self, OAuthConfig};
use crate::linear::oauth::{OAuthError, TokenBundle};
use crate::linear::{ApiError, VerifiedReadOutcome};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use oauth2::reqwest::blocking::{Body, Client, ClientBuilder, Response};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use oauth2::reqwest::header::CONTENT_TYPE;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use oauth2::reqwest::redirect::Policy;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use oauth2::url::form_urlencoded::Serializer;
use serde::{Deserialize, Deserializer, Serialize, Serializer as SerdeSerializer};
use std::fmt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::{self, Read};
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::sync::MutexGuard;
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};

// P0-05 binds each persisted credential to the exact OAuth client ID that
// obtained it. The version bump intentionally makes pre-binding records
// unreadable; there is no silent migration of credential material.
const ENVELOPE_VERSION: u8 = 2;
const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_CLIENT_ID_BYTES: usize = 4 * 1024;
const MAX_VIEWER_ID_BYTES: usize = 4 * 1024;
const MILLIS_PER_SECOND: i64 = 1_000;
const REPLAY_GRACE_MILLIS: i64 = 30 * 60 * MILLIS_PER_SECOND;
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
const MAX_PROVIDER_RESPONSE_BYTES: usize = 64 * 1024;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REFRESH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REFRESH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(any(target_os = "macos", target_os = "linux"))]
const TOKEN_ENDPOINT: &str = "https://api.linear.app/oauth/token";
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REVOKE_ENDPOINT: &str = "https://api.linear.app/oauth/revoke";

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
static PROCESS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
/// A workspace's advisory lock file: `credentials-<workspace>.lock` in the
/// state dir.
pub fn lock_path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir.join(format!("credentials-{workspace}.lock"))
}

fn process_lock() -> &'static Mutex<()> {
    PROCESS_LOCK.get_or_init(|| Mutex::new(()))
}

/// Coarse failures returned by the credential lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    /// This host has neither the macOS Keychain nor the Secret Service.
    #[cfg_attr(
        any(target_os = "macos", target_os = "linux"),
        expect(dead_code, reason = "constructed only on unsupported hosts")
    )]
    UnsupportedPlatform,
    /// The local OAuth configuration is invalid.
    Configuration,
    /// The authorization operation failed at the given step. OAuth errors
    /// are coarse and carry no provider payload or secret.
    Authorization(OAuthError),
    /// The credential store could not complete a definitive operation.
    Storage,
    /// The store outcome could not be verified; callers must fail closed.
    StorageUncertain,
    /// The cooperative process/advisory lock could not be acquired safely.
    LockUnavailable,
    /// The stored envelope is malformed, unsupported, or exceeds its bound.
    InvalidEnvelope,
    /// A refresh or revoke intent is pending and cannot be silently replaced.
    PendingLifecycle,
    /// The retained credential requires a new authorization.
    ReauthorizationRequired,
    /// A first refresh attempt failed or was not fully classified; one replay remains.
    RefreshAmbiguous,
    /// A refresh replay is outside its strict grace deadline.
    ReplayExpired,
    /// A replay was already consumed; another send is forbidden.
    ReplayConsumed,
    /// The wall clock moved before an immutable lifecycle timestamp.
    ClockRollback,
    /// A token is not currently usable.
    NotReady,
    /// `logout` was requested without its explicit confirmation gate.
    ConfirmationRequired,
    /// Revoke could not be confirmed with the exact HTTP contract.
    RevokeUnconfirmed,
    /// An operation was given an unsupported command or configuration value.
    InvalidInput,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedPlatform => "Linear OAuth credentials are unsupported on this host",
            Self::Configuration => "Linear OAuth configuration is invalid",
            Self::Authorization(error) => {
                return write!(formatter, "Linear OAuth authorization failed: {error}");
            }
            Self::Storage => "Linear OAuth credential storage failed",
            Self::StorageUncertain => "Linear OAuth credential storage could not be verified",
            Self::LockUnavailable => "Linear OAuth credential lock is unavailable",
            Self::InvalidEnvelope => "Linear OAuth credential record is invalid",
            Self::PendingLifecycle => "Linear OAuth credential lifecycle is pending",
            Self::ReauthorizationRequired => "Linear OAuth reauthorization is required",
            Self::RefreshAmbiguous => "Linear OAuth refresh outcome is uncertain",
            Self::ReplayExpired => "Linear OAuth refresh replay grace has expired",
            Self::ReplayConsumed => "Linear OAuth refresh replay is unavailable",
            Self::ClockRollback => "system clock moved backwards during Linear OAuth lifecycle",
            Self::NotReady => "Linear OAuth access is not ready",
            Self::ConfirmationRequired => "logout requires --confirm-revoke",
            Self::RevokeUnconfirmed => "Linear OAuth revocation was not confirmed",
            Self::InvalidInput => "Linear OAuth command or configuration is invalid",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CredentialError {}

/// Local-only classification used by `auth linear status`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialStatus {
    /// No record exists in the selected local store.
    SignedOut,
    /// A non-expired access token is available to the lifecycle manager.
    Ready,
    /// The access token is expired and a refresh is needed.
    ExpiredOrRefreshNeeded,
    /// One refresh request is ambiguous and is eligible for its one replay.
    ReplayPending,
    /// The retained bundle must be replaced by a new authorization.
    ReauthorizationRequired,
    /// Revoke was not confirmed; the retained bundle cannot be used.
    RevokePending,
    /// Provider revoke was confirmed, but local deletion is unfinished.
    RevokedDeletePending,
    /// The local store, lock, clock, or envelope could not be classified.
    Unavailable,
}

impl fmt::Display for CredentialStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::SignedOut => "signed_out",
            Self::Ready => "ready",
            Self::ExpiredOrRefreshNeeded => "expired_or_refresh_needed",
            Self::ReplayPending => "replay_pending",
            Self::ReauthorizationRequired => "reauthorization_required",
            Self::RevokePending => "revoke_pending",
            Self::RevokedDeletePending => "revoked_delete_pending",
            Self::Unavailable => "unavailable",
        };
        formatter.write_str(value)
    }
}

/// A secret text value that never reveals itself through formatting.
#[derive(Clone)]
struct SecretText(Zeroizing<String>);

impl SecretText {
    fn new(value: String) -> Result<Self, CredentialError> {
        if value.is_empty()
            || value.len() > MAX_TOKEN_BYTES
            || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            let mut value = value;
            value.zeroize();
            return Err(CredentialError::InvalidEnvelope);
        }
        Ok(Self(Zeroizing::new(value)))
    }

    #[cfg(test)]
    fn from_static(value: &str) -> Self {
        Self(Zeroizing::new(value.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn clone_inner(&self) -> Zeroizing<String> {
        // `Zeroizing` deliberately does not expose an ownership escape hatch:
        // cloning here keeps the source buffer's drop-time wipe guarantee.
        self.0.clone()
    }
}

impl Serialize for SecretText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: SerdeSerializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SecretText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        SecretText::new(value).map_err(|_| serde::de::Error::custom("invalid secret value"))
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LifecycleState {
    Ready,
    ReplayPending,
    RevokePending,
    RevokedDeletePending,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RefreshIntent {
    client_id: String,
    refresh_token: SecretText,
    first_send_at_ms: i64,
    replay_deadline_ms: i64,
    attempt_count: u8,
    replay_consumed: bool,
}

impl fmt::Debug for RefreshIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RefreshIntent")
            .field("client_id", &"[redacted]")
            .field("refresh_token", &self.refresh_token)
            .field("first_send_at_ms", &self.first_send_at_ms)
            .field("replay_deadline_ms", &self.replay_deadline_ms)
            .field("attempt_count", &self.attempt_count)
            .field("replay_consumed", &self.replay_consumed)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RevokeIntent {
    token: SecretText,
    started_at_ms: i64,
    confirmed_at_ms: Option<i64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialEnvelope {
    version: u8,
    revision: u64,
    /// Non-secret local metadata tying the token to the OAuth client used at
    /// login. This is compared before an access-token lease is issued.
    client_id: String,
    /// The first verified app viewer bound to this OAuth client. It is
    /// bootstrapped by the read contract under the credential lock and then
    /// compared on every later read; it is never emitted in public evidence.
    viewer_id: Option<String>,
    state: LifecycleState,
    access_token: SecretText,
    refresh_token: SecretText,
    access_expires_at_ms: i64,
    refresh: Option<RefreshIntent>,
    revoke: Option<RevokeIntent>,
}

impl fmt::Debug for CredentialEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialEnvelope")
            .field("version", &self.version)
            .field("revision", &self.revision)
            .field("client_id", &"[redacted]")
            .field("viewer_id", &"[redacted]")
            .field("state", &self.state)
            .field("access_token", &self.access_token)
            .field("refresh_token", &self.refresh_token)
            .field("access_expires_at_ms", &self.access_expires_at_ms)
            .field("refresh", &self.refresh)
            .field("revoke", &self.revoke)
            .finish()
    }
}

impl CredentialEnvelope {
    fn ready(
        client_id: &str,
        revision: u64,
        access_token: Zeroizing<String>,
        refresh_token: Zeroizing<String>,
        access_expires_at_ms: i64,
    ) -> Result<Self, CredentialError> {
        Ok(Self {
            version: ENVELOPE_VERSION,
            revision,
            client_id: bounded_client_id(client_id)?,
            viewer_id: None,
            state: LifecycleState::Ready,
            access_token: SecretText::new(access_token.to_string())?,
            refresh_token: SecretText::new(refresh_token.to_string())?,
            access_expires_at_ms,
            refresh: None,
            revoke: None,
        })
    }

    fn validate(&self) -> Result<(), CredentialError> {
        if self.version != ENVELOPE_VERSION
            || self.revision == 0
            || self.access_expires_at_ms < 0
            || bounded_client_id(&self.client_id).is_err()
            || self
                .viewer_id
                .as_deref()
                .is_some_and(|viewer_id| bounded_viewer_id(viewer_id).is_err())
        {
            return Err(CredentialError::InvalidEnvelope);
        }
        if self.access_token.as_str().len() > MAX_TOKEN_BYTES
            || self.refresh_token.as_str().len() > MAX_TOKEN_BYTES
        {
            return Err(CredentialError::InvalidEnvelope);
        }
        if self.client_id_if_present().is_some_and(|value| {
            value.is_empty()
                || value.len() > MAX_CLIENT_ID_BYTES
                || value.bytes().any(|byte| byte.is_ascii_control())
        }) {
            return Err(CredentialError::InvalidEnvelope);
        }
        match self.state {
            LifecycleState::Ready => {
                if self.refresh.is_some() || self.revoke.is_some() {
                    return Err(CredentialError::InvalidEnvelope);
                }
            }
            LifecycleState::ReplayPending => {
                let Some(intent) = self.refresh.as_ref() else {
                    return Err(CredentialError::InvalidEnvelope);
                };
                let Some(expected_deadline) =
                    intent.first_send_at_ms.checked_add(REPLAY_GRACE_MILLIS)
                else {
                    return Err(CredentialError::InvalidEnvelope);
                };
                if self.revoke.is_some()
                    || intent.refresh_token.as_str() != self.refresh_token.as_str()
                    || intent.client_id != self.client_id
                    || intent.first_send_at_ms < 0
                    || intent.replay_deadline_ms != expected_deadline
                    || intent.replay_deadline_ms <= intent.first_send_at_ms
                    || !(intent.attempt_count == 1 || intent.attempt_count == 2)
                    || (intent.replay_consumed && intent.attempt_count != 2)
                    || (!intent.replay_consumed && intent.attempt_count != 1)
                {
                    return Err(CredentialError::InvalidEnvelope);
                }
            }
            LifecycleState::RevokePending | LifecycleState::RevokedDeletePending => {
                let Some(intent) = self.revoke.as_ref() else {
                    return Err(CredentialError::InvalidEnvelope);
                };
                if self.refresh.is_some()
                    || intent.token.as_str() != self.refresh_token.as_str()
                    || intent.started_at_ms < 0
                    || (self.state == LifecycleState::RevokePending
                        && intent.confirmed_at_ms.is_some())
                    || (self.state == LifecycleState::RevokedDeletePending
                        && intent.confirmed_at_ms != Some(intent.started_at_ms))
                {
                    return Err(CredentialError::InvalidEnvelope);
                }
            }
        }
        Ok(())
    }

    fn client_id_if_present(&self) -> Option<&str> {
        self.refresh
            .as_ref()
            .map(|intent| intent.client_id.as_str())
    }
}

fn serialize_envelope(
    envelope: &CredentialEnvelope,
) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
    envelope.validate()?;
    let bytes = serde_json::to_vec(envelope).map_err(|_| CredentialError::InvalidEnvelope)?;
    if bytes.is_empty() || bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(CredentialError::InvalidEnvelope);
    }
    Ok(Zeroizing::new(bytes))
}

fn parse_envelope(bytes: &[u8]) -> Result<CredentialEnvelope, CredentialError> {
    if bytes.is_empty() || bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(CredentialError::InvalidEnvelope);
    }
    let envelope: CredentialEnvelope =
        serde_json::from_slice(bytes).map_err(|_| CredentialError::InvalidEnvelope)?;
    envelope.validate()?;
    Ok(envelope)
}

/// A bounded response returned by a provider transport.
struct ProviderResponse {
    status: u16,
    body: Zeroizing<Vec<u8>>,
}

impl ProviderResponse {
    #[cfg(test)]
    fn synthetic(status: u16, body: &[u8]) -> Result<Self, CredentialError> {
        if body.len() > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(CredentialError::InvalidInput);
        }
        Ok(Self {
            status,
            body: Zeroizing::new(body.to_vec()),
        })
    }
}

impl fmt::Debug for ProviderResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderResponse")
            .field("status", &self.status)
            .field("body", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderTransportError {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    ClientConfiguration,
    #[cfg(any(test, target_os = "macos", target_os = "linux"))]
    NoResponse,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    ResponseTooLarge,
}

trait ProviderTransport {
    fn refresh(
        &mut self,
        client_id: &str,
        refresh_token: &str,
    ) -> Result<ProviderResponse, ProviderTransportError>;
    fn revoke(&mut self, refresh_token: &str) -> Result<ProviderResponse, ProviderTransportError>;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct SecretBody {
    bytes: Zeroizing<Vec<u8>>,
    offset: usize,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl SecretBody {
    fn new(bytes: Zeroizing<Vec<u8>>) -> Self {
        Self { bytes, offset: 0 }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Read for SecretBody {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        let remaining = self.bytes.len().saturating_sub(self.offset);
        let length = remaining.min(destination.len());
        if length == 0 {
            return Ok(0);
        }
        destination[..length].copy_from_slice(&self.bytes[self.offset..self.offset + length]);
        self.offset += length;
        Ok(length)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct HttpsProviderTransport {
    client: Client,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl HttpsProviderTransport {
    fn new() -> Result<Self, ProviderTransportError> {
        let client = ClientBuilder::new()
            .https_only(true)
            .redirect(Policy::none())
            .no_proxy()
            .retry(oauth2::reqwest::retry::never())
            .connect_timeout(REFRESH_CONNECT_TIMEOUT)
            .timeout(REFRESH_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ProviderTransportError::ClientConfiguration)?;
        Ok(Self { client })
    }

    fn post_form(
        &self,
        endpoint: &str,
        body: Zeroizing<Vec<u8>>,
    ) -> Result<ProviderResponse, ProviderTransportError> {
        let body_length = body.len() as u64;
        let response = self
            .client
            .post(endpoint)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::sized(SecretBody::new(body), body_length))
            .send()
            .map_err(|_| ProviderTransportError::NoResponse)?;
        read_provider_response(response)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl fmt::Debug for HttpsProviderTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HttpsProviderTransport([configured])")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ProviderTransport for HttpsProviderTransport {
    fn refresh(
        &mut self,
        client_id: &str,
        refresh_token: &str,
    ) -> Result<ProviderResponse, ProviderTransportError> {
        let mut form = Serializer::new(String::new());
        form.append_pair("grant_type", "refresh_token");
        form.append_pair("refresh_token", refresh_token);
        form.append_pair("client_id", client_id);
        self.post_form(TOKEN_ENDPOINT, Zeroizing::new(form.finish().into_bytes()))
    }

    fn revoke(&mut self, refresh_token: &str) -> Result<ProviderResponse, ProviderTransportError> {
        let mut form = Serializer::new(String::new());
        form.append_pair("token", refresh_token);
        form.append_pair("token_type_hint", "refresh_token");
        self.post_form(REVOKE_ENDPOINT, Zeroizing::new(form.finish().into_bytes()))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn read_provider_response(response: Response) -> Result<ProviderResponse, ProviderTransportError> {
    let status = response.status().as_u16();
    let mut body = Zeroizing::new(Vec::with_capacity(MAX_PROVIDER_RESPONSE_BYTES.min(4096)));
    let mut limited = response.take((MAX_PROVIDER_RESPONSE_BYTES + 1) as u64);
    limited
        .read_to_end(&mut body)
        .map_err(|_| ProviderTransportError::NoResponse)?;
    if body.len() > MAX_PROVIDER_RESPONSE_BYTES {
        return Err(ProviderTransportError::ResponseTooLarge);
    }
    Ok(ProviderResponse { status, body })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoreError {
    #[cfg(any(test, target_os = "macos", target_os = "linux"))]
    Unavailable,
    #[cfg(test)]
    Uncertain,
}

trait CredentialStore {
    fn read(&mut self) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError>;
    fn write(&mut self, bytes: &[u8]) -> Result<(), StoreError>;
    fn delete(&mut self) -> Result<(), StoreError>;
    fn verify_absent(&mut self) -> Result<bool, StoreError>;
}

#[cfg(target_os = "macos")]
mod keychain {
    use super::*;
    use core_foundation::data::CFData;
    use security_framework::base::Error as SecurityError;
    use security_framework::item::{
        ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, ItemUpdateOptions,
        ItemUpdateValue, Location, SearchResult, update_item,
    };
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework_sys::base::{errSecDuplicateItem, errSecItemNotFound, errSecParam};

    const KEYCHAIN_SERVICE: &str = "dev.herdr-linear-agent.linear.oauth.v1";

    /// Generic-password store backed by the user's default file-based
    /// Keychain (normally the login Keychain).
    ///
    /// This deliberately uses the `SecItem` API without a data-protection
    /// selector, access group, or synchronizable attribute.  The standalone
    /// executable uses the user's default file-based Keychain (normally the
    /// login Keychain) according to Security.framework's default semantics;
    /// this selection is not derived from executable signing status.
    pub struct KeychainStore {
        service: String,
        account: String,
        default_keychain: SecKeychain,
    }

    impl KeychainStore {
        /// The item of one workspace: the account is the workspace's name.
        pub fn production(account: &str) -> Result<Self, SecurityError> {
            Self::new(KEYCHAIN_SERVICE, account)
        }

        fn new(service: &str, account: &str) -> Result<Self, SecurityError> {
            let default_keychain = SecKeychain::default()?;
            Ok(Self {
                service: service.to_owned(),
                account: account.to_owned(),
                default_keychain,
            })
        }

        fn search_options(&self, load_data: bool) -> ItemSearchOptions {
            let mut options = ItemSearchOptions::new();
            options
                .class(ItemClass::generic_password())
                .service(&self.service)
                .account(&self.account)
                .limit(1)
                .load_data(load_data);
            options.keychains(std::slice::from_ref(&self.default_keychain));
            options
        }

        fn add_options(&self, bytes: &[u8]) -> ItemAddOptions {
            let value = ItemAddValue::Data {
                class: ItemClass::generic_password(),
                data: CFData::from_buffer(bytes),
            };
            let mut options = ItemAddOptions::new(value);
            options
                .set_service(&self.service)
                .set_account_name(&self.account)
                // Pin creation to the user's default file-based Keychain;
                // unlike the data-protection selector, this needs no bundle
                // identity or restricted entitlement.
                .set_location(Location::FileKeychain(self.default_keychain.clone()));
            options
        }

        fn update_options(bytes: &[u8]) -> ItemUpdateOptions {
            let mut options = ItemUpdateOptions::new();
            options.set_value(ItemUpdateValue::Data(CFData::from_buffer(bytes)));
            options
        }

        fn read_item(&self) -> Result<Option<Vec<u8>>, SecurityError> {
            let mut results = self.search_options(true).search()?;
            match results.pop() {
                None => Ok(None),
                Some(SearchResult::Data(data)) => Ok(Some(data)),
                // A generic-password query requesting data must return CFData;
                // any other representation is an unavailable store, not an
                // opportunity to inspect or log the value.
                Some(_) => Err(SecurityError::from_code(errSecParam)),
            }
        }

        fn write_item(&self, bytes: &[u8]) -> Result<(), SecurityError> {
            match self.add_options(bytes).add() {
                Ok(()) => Ok(()),
                Err(error) if error.code() == errSecDuplicateItem => {
                    update_item(&self.search_options(false), &Self::update_options(bytes))
                }
                Err(error) => Err(error),
            }
        }

        fn delete_item(&self) -> Result<(), SecurityError> {
            self.search_options(false).delete()
        }

        fn map_error(error: SecurityError) -> StoreError {
            // Keep OSStatus values out of logs and public errors.  In
            // particular, errSecAuthFailed/interaction-required outcomes are
            // intentionally indistinguishable from other availability errors.
            let _ = error.code();
            StoreError::Unavailable
        }
    }

    impl CredentialStore for KeychainStore {
        fn read(&mut self) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
            match self.read_item() {
                Ok(Some(data)) => Ok(Some(Zeroizing::new(data))),
                Ok(None) => Ok(None),
                Err(error) if error.code() == errSecItemNotFound => Ok(None),
                Err(error) => Err(Self::map_error(error)),
            }
        }

        fn write(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
            self.write_item(bytes).map_err(Self::map_error)
        }

        fn delete(&mut self) -> Result<(), StoreError> {
            match self.delete_item() {
                Ok(()) => Ok(()),
                Err(error) if error.code() == errSecItemNotFound => Ok(()),
                Err(_error) => {
                    // Make deletion idempotent without treating an uncertain
                    // read as absence.
                    match self.read()? {
                        None => Ok(()),
                        Some(mut bytes) => {
                            bytes.zeroize();
                            Err(StoreError::Unavailable)
                        }
                    }
                }
            }
        }

        fn verify_absent(&mut self) -> Result<bool, StoreError> {
            Ok(self.read()?.is_none())
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_store(account: &str) -> Result<Box<dyn CredentialStore>, CredentialError> {
    Ok(Box::new(
        keychain::KeychainStore::production(account).map_err(|_| CredentialError::Storage)?,
    ))
}

/// Generic secret in the user's default Secret Service collection (GNOME
/// Keyring, KWallet), with the same service name the macOS Keychain item uses.
#[cfg(target_os = "linux")]
mod secret_store {
    use super::*;
    use secret_service::EncryptionType;
    use secret_service::blocking::{Item, SecretService};
    use std::collections::HashMap;

    const SERVICE: &str = "dev.herdr-linear-agent.linear.oauth.v1";
    const LABEL: &str = "herdr-linear-agent Linear OAuth credential";

    /// The secret of one workspace: the account is the workspace's name.
    pub struct SecretServiceStore {
        pub account: String,
    }

    impl SecretServiceStore {
        fn attributes(&self) -> HashMap<&str, &str> {
            HashMap::from([("service", SERVICE), ("account", self.account.as_str())])
        }

        fn items<'a>(&self, service: &'a SecretService<'a>) -> Result<Vec<Item<'a>>, StoreError> {
            let found = service
                .search_items(self.attributes())
                .map_err(|_| StoreError::Unavailable)?;
            Ok(found.unlocked.into_iter().chain(found.locked).collect())
        }
    }

    // Error details stay out of logs and public errors, as for the Keychain.
    fn connect() -> Result<SecretService<'static>, StoreError> {
        SecretService::connect(EncryptionType::Dh).map_err(|_| StoreError::Unavailable)
    }

    impl CredentialStore for SecretServiceStore {
        fn read(&mut self) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
            let service = connect()?;
            let Some(item) = self.items(&service)?.into_iter().next() else {
                return Ok(None);
            };
            item.unlock().map_err(|_| StoreError::Unavailable)?;
            item.get_secret()
                .map(|secret| Some(Zeroizing::new(secret)))
                .map_err(|_| StoreError::Unavailable)
        }

        fn write(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
            let service = connect()?;
            let collection = service
                .get_default_collection()
                .map_err(|_| StoreError::Unavailable)?;
            collection.unlock().map_err(|_| StoreError::Unavailable)?;
            collection
                .create_item(
                    LABEL,
                    self.attributes(),
                    bytes,
                    true,
                    "application/octet-stream",
                )
                .map(|_| ())
                .map_err(|_| StoreError::Unavailable)
        }

        fn delete(&mut self) -> Result<(), StoreError> {
            let service = connect()?;
            for item in self.items(&service)? {
                item.delete().map_err(|_| StoreError::Unavailable)?;
            }
            Ok(())
        }

        fn verify_absent(&mut self) -> Result<bool, StoreError> {
            Ok(self.read()?.is_none())
        }
    }
}

#[cfg(target_os = "linux")]
fn platform_store(account: &str) -> Result<Box<dyn CredentialStore>, CredentialError> {
    Ok(Box::new(secret_store::SecretServiceStore {
        account: account.to_owned(),
    }))
}

trait WallClock {
    /// Returns the current Unix epoch time in milliseconds.
    fn now(&self) -> Result<i64, CredentialError>;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Default)]
struct SystemWallClock;

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl WallClock for SystemWallClock {
    fn now(&self) -> Result<i64, CredentialError> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| CredentialError::ClockRollback)?
            .as_millis()
            .try_into()
            .map_err(|_| CredentialError::ClockRollback)
    }
}

trait LockGuard {}

trait CriticalSection {
    fn lock(&self) -> Result<Box<dyn LockGuard>, CredentialError>;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct ProcessAndFileGuard {
    _process: MutexGuard<'static, ()>,
    _file: std::fs::File,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl LockGuard for ProcessAndFileGuard {}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct SystemCriticalSection {
    lock_path: PathBuf,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl CriticalSection for SystemCriticalSection {
    fn lock(&self) -> Result<Box<dyn LockGuard>, CredentialError> {
        let process = process_lock()
            .lock()
            .map_err(|_| CredentialError::LockUnavailable)?;
        let file =
            open_advisory_lock(&self.lock_path).map_err(|_| CredentialError::LockUnavailable)?;
        Ok(Box::new(ProcessAndFileGuard {
            _process: process,
            _file: file,
        }))
    }
}

/// The lock file's directory must exist, be a real directory owned by this
/// user, and not be writable by group or others.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn check_lock_directory(path: &std::path::Path) -> Result<(), io::Error> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lock path has no directory"))?;
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "insecure lock directory",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_advisory_lock(path: &std::path::Path) -> Result<std::fs::File, io::Error> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    check_lock_directory(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "insecure lock file",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "insecure lock file",
        ));
    }
    file.lock()?;
    Ok(file)
}

trait AuthorizationProvider {
    fn authorize(&mut self) -> Result<TokenBundle, OAuthError>;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct ProductionAuthorization {
    config: OAuthConfig,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl AuthorizationProvider for ProductionAuthorization {
    fn authorize(&mut self) -> Result<TokenBundle, OAuthError> {
        oauth::authorize_production(&self.config)
    }
}

struct LoadedRecord {
    envelope: CredentialEnvelope,
    bytes: Zeroizing<Vec<u8>>,
}

/// The local credential lifecycle manager.
pub struct CredentialManager {
    client_id: String,
    store: Box<dyn CredentialStore>,
    transport: Option<Box<dyn ProviderTransport>>,
    clock: Box<dyn WallClock>,
    critical_section: Box<dyn CriticalSection>,
    authorizer: Option<Box<dyn AuthorizationProvider>>,
}

impl fmt::Debug for CredentialManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialManager")
            .field("client_id", &"[redacted]")
            .field("store", &"[configured]")
            .field("transport", &"[configured]")
            .finish()
    }
}

impl CredentialManager {
    /// Constructs the production manager for one workspace: `account` is its
    /// name, which keys the stored secret, and `lock_path` its advisory lock
    /// file (see [`lock_path`]).
    pub fn production(
        account: &str,
        client_id: impl Into<String>,
        callback_port: u16,
        lock_path: PathBuf,
    ) -> Result<Self, CredentialError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let client_id = client_id.into();
            let client_id = bounded_client_id(&client_id)?;
            let config = OAuthConfig::with_callback_port(client_id.clone(), callback_port)
                .map_err(|_| CredentialError::Configuration)?;
            let transport =
                HttpsProviderTransport::new().map_err(|_| CredentialError::Configuration)?;
            Ok(Self {
                client_id,
                store: platform_store(account)?,
                transport: Some(Box::new(transport)),
                clock: Box::new(SystemWallClock),
                critical_section: Box::new(SystemCriticalSection { lock_path }),
                authorizer: Some(Box::new(ProductionAuthorization { config })),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (account, client_id, callback_port, lock_path);
            Err(CredentialError::UnsupportedPlatform)
        }
    }

    /// Constructs the production manager for local status inspection.  It
    /// installs no network-capable transport, because status is intentionally
    /// side-effect free.
    pub fn production_status(account: &str, lock_path: PathBuf) -> Result<Self, CredentialError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            Ok(Self {
                client_id: String::new(),
                store: platform_store(account)?,
                transport: None,
                clock: Box::new(SystemWallClock),
                critical_section: Box::new(SystemCriticalSection { lock_path }),
                authorizer: None,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (account, lock_path);
            Err(CredentialError::UnsupportedPlatform)
        }
    }

    #[cfg(test)]
    fn with_dependencies(
        client_id: &str,
        store: Box<dyn CredentialStore>,
        transport: Box<dyn ProviderTransport>,
        clock: Box<dyn WallClock>,
        critical_section: Box<dyn CriticalSection>,
        authorizer: Box<dyn AuthorizationProvider>,
    ) -> Self {
        Self {
            client_id: client_id.to_owned(),
            store,
            transport: Some(transport),
            clock,
            critical_section,
            authorizer: Some(authorizer),
        }
    }

    /// Runs browser authorization and atomically records the resulting bundle.
    pub fn login(&mut self) -> Result<(), CredentialError> {
        let _guard = self.critical_section.lock()?;
        let previous = self.load_record()?;
        if previous.is_some() {
            return Err(CredentialError::PendingLifecycle);
        }
        let bundle = self
            .authorizer
            .as_mut()
            .ok_or(CredentialError::Configuration)?
            .authorize()
            .map_err(CredentialError::Authorization)?;
        let (access, refresh, lifetime) = bundle.into_credential_parts();
        let now = self.clock.now()?;
        let expires_at_ms = now
            .checked_add(duration_millis(lifetime)?)
            .ok_or(CredentialError::Configuration)?;
        let envelope =
            CredentialEnvelope::ready(&self.client_id, 1, access, refresh, expires_at_ms)?;
        self.write_record(&envelope, None).map(|_| ())
    }

    /// Reads and classifies only local state.  It never refreshes, revokes,
    /// launches a browser, deletes an item, or contacts Linear.
    pub fn status(&mut self) -> CredentialStatus {
        let Ok(_guard) = self.critical_section.lock() else {
            return CredentialStatus::Unavailable;
        };
        let Ok(record) = self.load_record() else {
            return CredentialStatus::Unavailable;
        };
        let Some(record) = record else {
            return CredentialStatus::SignedOut;
        };
        match record.envelope.state {
            LifecycleState::ReplayPending => {
                let Some(intent) = record.envelope.refresh.as_ref() else {
                    return CredentialStatus::Unavailable;
                };
                match self.clock.now() {
                    Ok(now) if now < intent.first_send_at_ms => CredentialStatus::Unavailable,
                    Ok(now) if replay_is_eligible(intent, now) => CredentialStatus::ReplayPending,
                    Ok(_) => CredentialStatus::ReauthorizationRequired,
                    Err(_) => CredentialStatus::Unavailable,
                }
            }
            LifecycleState::RevokePending => CredentialStatus::RevokePending,
            LifecycleState::RevokedDeletePending => CredentialStatus::RevokedDeletePending,
            LifecycleState::Ready => match self.clock.now() {
                Ok(now) if now < record.envelope.access_expires_at_ms => CredentialStatus::Ready,
                Ok(_) => CredentialStatus::ExpiredOrRefreshNeeded,
                Err(_) => CredentialStatus::Unavailable,
            },
        }
    }

    /// Test-only access to the generic token lease. Production provider
    /// operations must use [`Self::with_verified_read`] so an actor binding is
    /// enforced before the lease leaves this manager.
    #[cfg(test)]
    pub(crate) fn with_access_token<T>(
        &mut self,
        callback: impl FnOnce(&str) -> T,
    ) -> Result<T, CredentialError> {
        let _guard = self.critical_section.lock()?;
        let record = self.load_access_record()?;
        Ok(callback(record.envelope.access_token.as_str()))
    }

    /// Acquires a serialized token lease, accepts only a typed fully verified
    /// read outcome, persists or checks its app viewer binding while the lock
    /// remains held. A failed provider verification never reaches the binding
    /// write.
    pub(crate) fn with_verified_read<T>(
        &mut self,
        callback: impl FnOnce(&str) -> Result<VerifiedReadOutcome<T>, ApiError>,
    ) -> Result<T, ApiError> {
        let _guard = self.critical_section.lock().map_err(ApiError::Credential)?;
        let record = self.load_access_record().map_err(ApiError::Credential)?;
        let outcome = callback(record.envelope.access_token.as_str())?;
        let (viewer_id, value) = outcome.into_parts();
        self.bind_viewer_id(record, viewer_id)
            .map_err(ApiError::Credential)?;
        Ok(value)
    }

    /// A serialized token lease for a provider write. It is issued only after a
    /// verified read has bound the app viewer to this credential, so a write
    /// never goes out under an identity that was not checked first.
    pub(crate) fn with_bound_access_token<T>(
        &mut self,
        callback: impl FnOnce(&str) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let _guard = self.critical_section.lock().map_err(ApiError::Credential)?;
        let record = self.load_access_record().map_err(ApiError::Credential)?;
        if record.envelope.viewer_id.is_none() {
            return Err(ApiError::Credential(CredentialError::NotReady));
        }
        callback(record.envelope.access_token.as_str())
    }

    /// Explicitly revokes the latest refresh token and then removes local state.
    pub fn logout(&mut self, confirm_revoke: bool) -> Result<(), CredentialError> {
        if !confirm_revoke {
            return Err(CredentialError::ConfirmationRequired);
        }
        let _guard = self.critical_section.lock()?;
        let Some(record) = self.load_record()? else {
            return Ok(());
        };
        if record.envelope.state == LifecycleState::RevokedDeletePending {
            return self.finish_local_delete(record);
        }
        if matches!(
            record.envelope.state,
            LifecycleState::ReplayPending | LifecycleState::RevokePending
        ) {
            return Err(CredentialError::PendingLifecycle);
        }
        let now = self.clock.now()?;
        let mut pending = record.envelope.clone();
        pending.revision = next_revision(&pending)?;
        pending.state = LifecycleState::RevokePending;
        pending.refresh = None;
        pending.revoke = Some(RevokeIntent {
            token: SecretText::new(pending.refresh_token.as_str().to_owned())?,
            started_at_ms: now,
            confirmed_at_ms: None,
        });
        let pending_record = self.write_record(&pending, Some(&record.bytes))?;

        let response = match self
            .transport
            .as_mut()
            .ok_or(CredentialError::RevokeUnconfirmed)?
            .revoke(pending_record.envelope.refresh_token.as_str())
        {
            Ok(response) => response,
            Err(_) => return Err(CredentialError::RevokeUnconfirmed),
        };
        if response.status != 200 {
            return Err(CredentialError::RevokeUnconfirmed);
        }
        let confirmed_at_ms = pending_record
            .envelope
            .revoke
            .as_ref()
            .ok_or(CredentialError::InvalidEnvelope)?
            .started_at_ms;
        let mut tombstone = pending;
        tombstone.revision = next_revision(&tombstone)?;
        tombstone.state = LifecycleState::RevokedDeletePending;
        tombstone
            .revoke
            .as_mut()
            .ok_or(CredentialError::InvalidEnvelope)?
            .confirmed_at_ms = Some(confirmed_at_ms);
        self.read_exact_record(&pending_record.bytes)?;
        match self.write_record(&tombstone, Some(&pending_record.bytes)) {
            Ok(record) => self.finish_local_delete(record),
            Err(CredentialError::Storage) => {
                // A definitive tombstone-write failure means the exact prior
                // bytes were observed. Re-read that same record before the
                // terminal delete; uncertainty never takes this path.
                let record = self.read_exact_record(&pending_record.bytes)?;
                self.finish_local_delete(record)
            }
            Err(error) => Err(error),
        }
    }

    fn load_record(&mut self) -> Result<Option<LoadedRecord>, CredentialError> {
        let Some(bytes) = self.store.read().map_err(map_store_error)? else {
            return Ok(None);
        };
        let envelope = parse_envelope(&bytes)?;
        Ok(Some(LoadedRecord { envelope, bytes }))
    }

    fn load_access_record(&mut self) -> Result<LoadedRecord, CredentialError> {
        let record = self.load_record()?.ok_or(CredentialError::NotReady)?;
        self.validate_client_binding(&record.envelope)?;
        let now = self.clock.now()?;
        match record.envelope.state {
            LifecycleState::Ready if now < record.envelope.access_expires_at_ms => Ok(record),
            LifecycleState::Ready => self.refresh_record(record, now),
            LifecycleState::ReplayPending => self.replay_record(record, now),
            LifecycleState::RevokePending | LifecycleState::RevokedDeletePending => {
                Err(CredentialError::NotReady)
            }
        }
    }

    fn validate_client_binding(
        &self,
        envelope: &CredentialEnvelope,
    ) -> Result<(), CredentialError> {
        let client_id = bounded_client_id(&self.client_id)?;
        if envelope.client_id != client_id {
            return Err(CredentialError::Configuration);
        }
        Ok(())
    }

    fn bind_viewer_id(
        &mut self,
        record: LoadedRecord,
        viewer_id: Zeroizing<String>,
    ) -> Result<(), CredentialError> {
        let record = self.read_exact_record(&record.bytes)?;
        let viewer_id = bounded_viewer_id(viewer_id.as_str())?;
        if let Some(expected) = record.envelope.viewer_id.as_deref() {
            if expected == viewer_id {
                return Ok(());
            }
            return Err(CredentialError::Configuration);
        }

        let mut bound = record.envelope.clone();
        bound.revision = next_revision(&bound)?;
        bound.viewer_id = Some(viewer_id);
        self.write_record(&bound, Some(&record.bytes)).map(|_| ())
    }

    fn read_exact_record(&mut self, expected: &[u8]) -> Result<LoadedRecord, CredentialError> {
        let observed = self
            .store
            .read()
            .map_err(|_| CredentialError::StorageUncertain)?
            .ok_or(CredentialError::StorageUncertain)?;
        if observed.as_slice() != expected {
            return Err(CredentialError::StorageUncertain);
        }
        let envelope = parse_envelope(&observed).map_err(|_| CredentialError::StorageUncertain)?;
        Ok(LoadedRecord {
            envelope,
            bytes: observed,
        })
    }

    fn write_record(
        &mut self,
        envelope: &CredentialEnvelope,
        previous_bytes: Option<&Zeroizing<Vec<u8>>>,
    ) -> Result<LoadedRecord, CredentialError> {
        let bytes = serialize_envelope(envelope)?;
        let _write_result = self.store.write(&bytes);
        let observed = match self.store.read() {
            Ok(observed) => observed,
            Err(_) => return Err(CredentialError::StorageUncertain),
        };
        let Some(observed) = observed else {
            return if previous_bytes.is_none() {
                Err(CredentialError::Storage)
            } else {
                Err(CredentialError::StorageUncertain)
            };
        };
        if observed.as_slice() == bytes.as_slice() {
            // This exact post-write read is both the verification and the
            // record returned to the caller; do not reread to choose a
            // token-bearing or deletion target.
            let envelope =
                parse_envelope(&observed).map_err(|_| CredentialError::StorageUncertain)?;
            return Ok(LoadedRecord {
                envelope,
                bytes: observed,
            });
        }
        if previous_bytes.is_some_and(|expected| observed.as_slice() == expected.as_slice()) {
            return Err(CredentialError::Storage);
        }
        Err(CredentialError::StorageUncertain)
    }

    fn refresh_record(
        &mut self,
        record: LoadedRecord,
        now: i64,
    ) -> Result<LoadedRecord, CredentialError> {
        self.validate_client_binding(&record.envelope)?;
        let deadline = now
            .checked_add(REPLAY_GRACE_MILLIS)
            .ok_or(CredentialError::Configuration)?;
        let mut pending = record.envelope.clone();
        pending.revision = next_revision(&pending)?;
        pending.state = LifecycleState::ReplayPending;
        pending.revoke = None;
        pending.refresh = Some(RefreshIntent {
            client_id: bounded_client_id(&self.client_id)?,
            refresh_token: SecretText::new(pending.refresh_token.as_str().to_owned())?,
            first_send_at_ms: now,
            replay_deadline_ms: deadline,
            attempt_count: 1,
            replay_consumed: false,
        });
        let pending_record = self.write_record(&pending, Some(&record.bytes))?;
        let response = self
            .transport
            .as_mut()
            .ok_or_else(|| refresh_failure_error(&pending))?
            .refresh(
                pending
                    .refresh
                    .as_ref()
                    .ok_or(CredentialError::InvalidEnvelope)?
                    .client_id
                    .as_str(),
                pending.refresh_token.as_str(),
            )
            .map_err(|_| refresh_failure_error(&pending))?;
        self.accept_refresh_response(pending_record, response, now)
    }

    fn replay_record(
        &mut self,
        record: LoadedRecord,
        now: i64,
    ) -> Result<LoadedRecord, CredentialError> {
        self.validate_client_binding(&record.envelope)?;
        let intent = record
            .envelope
            .refresh
            .as_ref()
            .ok_or(CredentialError::InvalidEnvelope)?;
        if !replay_is_eligible(intent, now) {
            if now < intent.first_send_at_ms {
                return Err(CredentialError::ClockRollback);
            }
            if now >= intent.replay_deadline_ms {
                return Err(CredentialError::ReplayExpired);
            }
            return Err(CredentialError::ReplayConsumed);
        }
        let mut replay = record.envelope.clone();
        replay.revision = next_revision(&replay)?;
        replay
            .refresh
            .as_mut()
            .ok_or(CredentialError::InvalidEnvelope)?
            .attempt_count = 2;
        replay
            .refresh
            .as_mut()
            .ok_or(CredentialError::InvalidEnvelope)?
            .replay_consumed = true;
        let replay_record = self.write_record(&replay, Some(&record.bytes))?;
        let intent = replay
            .refresh
            .as_ref()
            .ok_or(CredentialError::InvalidEnvelope)?;
        let response = self
            .transport
            .as_mut()
            .ok_or_else(|| refresh_failure_error(&replay))?
            .refresh(intent.client_id.as_str(), intent.refresh_token.as_str())
            .map_err(|_| refresh_failure_error(&replay))?;
        self.accept_refresh_response(replay_record, response, now)
    }

    fn accept_refresh_response(
        &mut self,
        pending_record: LoadedRecord,
        response: ProviderResponse,
        original_now: i64,
    ) -> Result<LoadedRecord, CredentialError> {
        let pending = pending_record.envelope;
        if response.status != 200 {
            return Err(refresh_failure_error(&pending));
        }
        let bundle = match parse_refresh_response(response) {
            Ok(bundle) => bundle,
            Err(_) => return Err(refresh_failure_error(&pending)),
        };
        let now = self.clock.now()?;
        let first_send_at_ms = pending
            .refresh
            .as_ref()
            .ok_or(CredentialError::InvalidEnvelope)?
            .first_send_at_ms;
        if now < first_send_at_ms || now < original_now {
            return Err(CredentialError::ClockRollback);
        }
        let (access, refresh, lifetime) = bundle.into_credential_parts();
        let expires_at_ms = match duration_millis(lifetime)
            .ok()
            .and_then(|lifetime| now.checked_add(lifetime))
        {
            Some(expires_at) => expires_at,
            None => return Err(refresh_failure_error(&pending)),
        };
        let mut ready = CredentialEnvelope::ready(
            pending.client_id.as_str(),
            next_revision(&pending)?,
            access,
            refresh,
            expires_at_ms,
        )?;
        ready.viewer_id = pending.viewer_id.clone();
        self.read_exact_record(&pending_record.bytes)?;
        self.write_record(&ready, Some(&pending_record.bytes))
    }

    fn finish_local_delete(&mut self, record: LoadedRecord) -> Result<(), CredentialError> {
        let observed = self
            .store
            .read()
            .map_err(map_store_error)?
            .ok_or(CredentialError::StorageUncertain)?;
        if observed.as_slice() != record.bytes.as_slice() {
            return Err(CredentialError::StorageUncertain);
        }
        self.store.delete().map_err(map_store_error)?;
        if self.store.verify_absent().map_err(map_store_error)? {
            Ok(())
        } else {
            Err(CredentialError::StorageUncertain)
        }
    }
}

fn duration_millis(duration: Duration) -> Result<i64, CredentialError> {
    let seconds = i64::try_from(duration.as_secs()).map_err(|_| CredentialError::Configuration)?;
    let whole_millis = seconds
        .checked_mul(MILLIS_PER_SECOND)
        .ok_or(CredentialError::Configuration)?;
    whole_millis
        .checked_add(i64::from(duration.subsec_millis()))
        .ok_or(CredentialError::Configuration)
}

pub(crate) fn bounded_client_id(client_id: &str) -> Result<String, CredentialError> {
    if client_id.is_empty()
        || client_id.len() > MAX_CLIENT_ID_BYTES
        || client_id.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(CredentialError::Configuration);
    }
    Ok(client_id.to_owned())
}

fn bounded_viewer_id(viewer_id: &str) -> Result<String, CredentialError> {
    if viewer_id.trim().is_empty()
        || viewer_id.len() > MAX_VIEWER_ID_BYTES
        || viewer_id.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(CredentialError::Configuration);
    }
    Ok(viewer_id.to_owned())
}

fn next_revision(envelope: &CredentialEnvelope) -> Result<u64, CredentialError> {
    envelope
        .revision
        .checked_add(1)
        .filter(|revision| *revision != 0)
        .ok_or(CredentialError::StorageUncertain)
}

fn replay_is_eligible(intent: &RefreshIntent, now: i64) -> bool {
    now >= intent.first_send_at_ms
        && now < intent.replay_deadline_ms
        && !intent.replay_consumed
        && intent.attempt_count == 1
}

fn refresh_failure_error(envelope: &CredentialEnvelope) -> CredentialError {
    if envelope
        .refresh
        .as_ref()
        .is_some_and(|intent| intent.attempt_count == 2 && intent.replay_consumed)
    {
        CredentialError::ReauthorizationRequired
    } else {
        CredentialError::RefreshAmbiguous
    }
}

fn map_store_error(error: StoreError) -> CredentialError {
    match error {
        #[cfg(any(test, target_os = "macos", target_os = "linux"))]
        StoreError::Unavailable => CredentialError::Storage,
        #[cfg(test)]
        StoreError::Uncertain => CredentialError::StorageUncertain,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRefreshResponse {
    access_token: Option<SecretText>,
    refresh_token: Option<SecretText>,
    token_type: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    #[serde(default)]
    actor: Option<String>,
}

fn parse_refresh_response(response: ProviderResponse) -> Result<TokenBundle, CredentialError> {
    let mut raw: RawRefreshResponse =
        serde_json::from_slice(&response.body).map_err(|_| CredentialError::InvalidInput)?;
    if raw
        .token_type
        .as_deref()
        .is_none_or(|value| !value.eq_ignore_ascii_case("Bearer"))
        || raw.expires_in.is_none_or(|value| value == 0)
        || !raw
            .scope
            .as_deref()
            .is_some_and(crate::linear::oauth::scope_matches)
        || raw.actor.as_deref().is_some_and(|value| value != "app")
    {
        return Err(CredentialError::InvalidInput);
    }
    let access = raw
        .access_token
        .take()
        .ok_or(CredentialError::InvalidInput)?
        .clone_inner();
    let refresh = raw
        .refresh_token
        .take()
        .ok_or(CredentialError::InvalidInput)?
        .clone_inner();
    let expires = Duration::from_secs(raw.expires_in.ok_or(CredentialError::InvalidInput)?);
    Ok(TokenBundle::from_credential_parts(access, refresh, expires))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    const CLIENT_ID: &str = "synthetic-client";
    const ACCESS: &str = "synthetic-access";
    const REFRESH: &str = "synthetic-refresh";
    const NEW_ACCESS: &str = "synthetic-new-access";
    const NEW_REFRESH: &str = "synthetic-new-refresh";
    const VIEWER: &str = "synthetic-app-viewer";
    const OTHER_VIEWER: &str = "synthetic-other-app-viewer";
    const NOW_MS: i64 = 100_000;
    const EXPIRED_AT_MS: i64 = 100_000;
    const READY_AT_MS: i64 = 200_000;
    const REPLAY_DEADLINE_MS: i64 = NOW_MS + REPLAY_GRACE_MILLIS;

    #[derive(Clone)]
    struct FakeClock {
        now: Rc<Cell<i64>>,
    }

    impl WallClock for FakeClock {
        fn now(&self) -> Result<i64, CredentialError> {
            Ok(self.now.get())
        }
    }

    struct FailAfterFirstClock {
        now: i64,
        calls: Rc<Cell<usize>>,
    }

    impl WallClock for FailAfterFirstClock {
        fn now(&self) -> Result<i64, CredentialError> {
            let calls = self.calls.get();
            self.calls.set(calls + 1);
            if calls == 0 {
                Ok(self.now)
            } else {
                Err(CredentialError::ClockRollback)
            }
        }
    }

    struct FailingClock;

    impl WallClock for FailingClock {
        fn now(&self) -> Result<i64, CredentialError> {
            Err(CredentialError::ClockRollback)
        }
    }

    struct FakeLock;
    struct FakeGuard;
    impl LockGuard for FakeGuard {}
    impl CriticalSection for FakeLock {
        fn lock(&self) -> Result<Box<dyn LockGuard>, CredentialError> {
            Ok(Box::new(FakeGuard))
        }
    }

    #[derive(Default)]
    struct MemoryStore {
        value: Option<Zeroizing<Vec<u8>>>,
        fail_read: bool,
        fail_read_after_write: Option<usize>,
        fail_write: bool,
        fail_write_on: Option<usize>,
        write_error_after_commit: bool,
        read_after_write: ReadAfterWrite,
        fail_verify: bool,
        fail_delete: Rc<Cell<bool>>,
        writes: Rc<Cell<usize>>,
        deletes: Rc<Cell<usize>>,
        replace_on_next_read: Option<Rc<Cell<bool>>>,
        replacement: Option<Zeroizing<Vec<u8>>>,
        replace_after_write_verification: Option<(usize, Zeroizing<Vec<u8>>)>,
    }

    #[derive(Clone, Copy, Default)]
    enum ReadAfterWrite {
        #[default]
        Current,
        Error,
        Unexpected,
    }

    impl CredentialStore for MemoryStore {
        fn read(&mut self) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
            if self.fail_read {
                return Err(StoreError::Unavailable);
            }
            if self
                .replace_on_next_read
                .as_ref()
                .is_some_and(|replace| replace.get())
            {
                self.replace_on_next_read
                    .as_ref()
                    .expect("replacement flag")
                    .set(false);
                self.value = self.replacement.take();
            }
            if self
                .fail_read_after_write
                .is_some_and(|write| self.writes.get() >= write)
            {
                return Err(StoreError::Uncertain);
            }
            let observed = if self.writes.get() > 0 {
                match &self.read_after_write {
                    ReadAfterWrite::Current => self.value.clone(),
                    ReadAfterWrite::Error => return Err(StoreError::Uncertain),
                    ReadAfterWrite::Unexpected => {
                        return Ok(Some(Zeroizing::new(b"unexpected-read-back".to_vec())));
                    }
                }
            } else {
                self.value.clone()
            };
            if self
                .replace_after_write_verification
                .as_ref()
                .is_some_and(|(write, _)| self.writes.get() >= *write)
            {
                let (_, replacement) = self
                    .replace_after_write_verification
                    .take()
                    .expect("post-verification replacement");
                self.value = Some(replacement);
            }
            Ok(observed)
        }

        fn write(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
            self.writes.set(self.writes.get() + 1);
            if self.fail_write || self.fail_write_on == Some(self.writes.get()) {
                return Err(StoreError::Unavailable);
            }
            self.value = Some(Zeroizing::new(bytes.to_vec()));
            if self.write_error_after_commit {
                return Err(StoreError::Unavailable);
            }
            Ok(())
        }

        fn delete(&mut self) -> Result<(), StoreError> {
            self.deletes.set(self.deletes.get() + 1);
            if self.fail_delete.get() {
                return Err(StoreError::Unavailable);
            }
            self.value = None;
            Ok(())
        }

        fn verify_absent(&mut self) -> Result<bool, StoreError> {
            if self.fail_verify {
                return Err(StoreError::Uncertain);
            }
            Ok(self.value.is_none())
        }
    }

    struct ReplacingReadStore {
        value: Option<Zeroizing<Vec<u8>>>,
        replacement: Option<Zeroizing<Vec<u8>>>,
        reads: usize,
        deletes: Rc<Cell<usize>>,
    }

    impl CredentialStore for ReplacingReadStore {
        fn read(&mut self) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
            self.reads += 1;
            if self.reads == 2 {
                self.value = self.replacement.take();
            }
            Ok(self.value.clone())
        }

        fn write(&mut self, _bytes: &[u8]) -> Result<(), StoreError> {
            Err(StoreError::Unavailable)
        }

        fn delete(&mut self) -> Result<(), StoreError> {
            self.deletes.set(self.deletes.get() + 1);
            self.value = None;
            Ok(())
        }

        fn verify_absent(&mut self) -> Result<bool, StoreError> {
            Ok(self.value.is_none())
        }
    }

    enum FakeOutcome {
        Response(ProviderResponse),
        NoResponse,
    }

    struct FakeTransport {
        refreshes: Rc<Cell<usize>>,
        revokes: Rc<Cell<usize>>,
        refresh_outcomes: VecDeque<FakeOutcome>,
        revoke_outcomes: VecDeque<FakeOutcome>,
        requests: RefCell<Vec<(String, String)>>,
        replace_after_refresh: Option<Rc<Cell<bool>>>,
        replace_after_revoke: Option<Rc<Cell<bool>>>,
    }

    impl FakeTransport {
        fn new(refresh_outcomes: impl IntoIterator<Item = FakeOutcome>) -> Self {
            Self {
                refreshes: Rc::new(Cell::new(0)),
                revokes: Rc::new(Cell::new(0)),
                refresh_outcomes: refresh_outcomes.into_iter().collect(),
                revoke_outcomes: VecDeque::new(),
                requests: RefCell::new(Vec::new()),
                replace_after_refresh: None,
                replace_after_revoke: None,
            }
        }

        fn replace_store_after_refresh(&mut self, flag: Rc<Cell<bool>>) {
            self.replace_after_refresh = Some(flag);
        }

        fn replace_store_after_revoke(&mut self, flag: Rc<Cell<bool>>) {
            self.replace_after_revoke = Some(flag);
        }
    }

    impl ProviderTransport for FakeTransport {
        fn refresh(
            &mut self,
            client_id: &str,
            refresh_token: &str,
        ) -> Result<ProviderResponse, ProviderTransportError> {
            self.refreshes.set(self.refreshes.get() + 1);
            self.requests
                .borrow_mut()
                .push((client_id.to_owned(), refresh_token.to_owned()));
            match self
                .refresh_outcomes
                .pop_front()
                .unwrap_or(FakeOutcome::NoResponse)
            {
                FakeOutcome::Response(response) => {
                    if let Some(replace) = &self.replace_after_refresh {
                        replace.set(true);
                    }
                    Ok(response)
                }
                FakeOutcome::NoResponse => Err(ProviderTransportError::NoResponse),
            }
        }

        fn revoke(
            &mut self,
            _refresh_token: &str,
        ) -> Result<ProviderResponse, ProviderTransportError> {
            self.revokes.set(self.revokes.get() + 1);
            match self
                .revoke_outcomes
                .pop_front()
                .unwrap_or(FakeOutcome::NoResponse)
            {
                FakeOutcome::Response(response) => {
                    if let Some(replace) = &self.replace_after_revoke {
                        replace.set(true);
                    }
                    Ok(response)
                }
                FakeOutcome::NoResponse => Err(ProviderTransportError::NoResponse),
            }
        }
    }

    struct FakeAuthorizer {
        calls: usize,
        result: Option<Result<TokenBundle, OAuthError>>,
    }

    impl AuthorizationProvider for FakeAuthorizer {
        fn authorize(&mut self) -> Result<TokenBundle, OAuthError> {
            self.calls += 1;
            self.result
                .take()
                .unwrap_or(Err(OAuthError::InvalidTokenResponse))
        }
    }

    fn token_response(access: &str, refresh: &str, expires: u64) -> ProviderResponse {
        ProviderResponse::synthetic(
            200,
            format!(
                "{{\"access_token\":\"{access}\",\"refresh_token\":\"{refresh}\",\"token_type\":\"Bearer\",\"expires_in\":{expires},\"scope\":\"read,write,app:assignable\"}}"
            )
            .as_bytes(),
        )
        .expect("response")
    }

    fn fake_manager(
        store: MemoryStore,
        transport: FakeTransport,
        clock_ms: i64,
        authorizer: FakeAuthorizer,
    ) -> CredentialManager {
        fake_manager_with_clock(store, transport, Rc::new(Cell::new(clock_ms)), authorizer)
    }

    fn fake_manager_with_clock(
        store: MemoryStore,
        transport: FakeTransport,
        clock: Rc<Cell<i64>>,
        authorizer: FakeAuthorizer,
    ) -> CredentialManager {
        CredentialManager::with_dependencies(
            CLIENT_ID,
            Box::new(store),
            Box::new(transport),
            Box::new(FakeClock { now: clock }),
            Box::new(FakeLock),
            Box::new(authorizer),
        )
    }

    fn ready_bytes(revision: u64, expires_at_ms: i64) -> Zeroizing<Vec<u8>> {
        serialize_envelope(&ready_envelope(revision, expires_at_ms)).expect("bytes")
    }

    fn ready_envelope(revision: u64, expires_at_ms: i64) -> CredentialEnvelope {
        CredentialEnvelope::ready(
            CLIENT_ID,
            revision,
            Zeroizing::new(ACCESS.to_owned()),
            Zeroizing::new(REFRESH.to_owned()),
            expires_at_ms,
        )
        .expect("envelope")
    }

    fn ready_bytes_with_viewer(
        revision: u64,
        expires_at_ms: i64,
        viewer_id: &str,
    ) -> Zeroizing<Vec<u8>> {
        let mut envelope = ready_envelope(revision, expires_at_ms);
        envelope.viewer_id = Some(viewer_id.to_owned());
        serialize_envelope(&envelope).expect("bytes")
    }

    fn replay_pending_bytes(
        first_send_at_ms: i64,
        attempt_count: u8,
        replay_consumed: bool,
    ) -> Zeroizing<Vec<u8>> {
        replay_pending_bytes_with_revision(1, first_send_at_ms, attempt_count, replay_consumed)
    }

    fn replay_pending_bytes_with_revision(
        revision: u64,
        first_send_at_ms: i64,
        attempt_count: u8,
        replay_consumed: bool,
    ) -> Zeroizing<Vec<u8>> {
        replay_pending_bytes_with_revision_and_expiry(
            revision,
            first_send_at_ms,
            attempt_count,
            replay_consumed,
            READY_AT_MS,
        )
    }

    fn replay_pending_bytes_with_revision_and_expiry(
        revision: u64,
        first_send_at_ms: i64,
        attempt_count: u8,
        replay_consumed: bool,
        access_expires_at_ms: i64,
    ) -> Zeroizing<Vec<u8>> {
        let mut envelope = CredentialEnvelope::ready(
            CLIENT_ID,
            revision,
            Zeroizing::new(ACCESS.to_owned()),
            Zeroizing::new(REFRESH.to_owned()),
            access_expires_at_ms,
        )
        .expect("ready");
        envelope.state = LifecycleState::ReplayPending;
        envelope.refresh = Some(RefreshIntent {
            client_id: CLIENT_ID.to_owned(),
            refresh_token: SecretText::from_static(REFRESH),
            first_send_at_ms,
            replay_deadline_ms: first_send_at_ms
                .checked_add(REPLAY_GRACE_MILLIS)
                .expect("replay deadline"),
            attempt_count,
            replay_consumed,
        });
        serialize_envelope(&envelope).expect("replay envelope")
    }

    fn assert_status_without_mutation(
        manager: &mut CredentialManager,
        expected: CredentialStatus,
        initial: &[u8],
        writes: &Cell<usize>,
        deletes: &Cell<usize>,
    ) {
        assert_eq!(manager.status(), expected);
        assert_eq!(writes.get(), 0);
        assert_eq!(deletes.get(), 0);
        let loaded = manager.load_record().expect("read").expect("record");
        assert_eq!(loaded.bytes.as_slice(), initial);
    }

    fn authorizer() -> FakeAuthorizer {
        FakeAuthorizer {
            calls: 0,
            result: Some(Ok(TokenBundle::synthetic(
                Zeroizing::new(ACCESS.to_owned()),
                Zeroizing::new(REFRESH.to_owned()),
                Duration::from_secs(60),
            ))),
        }
    }

    #[test]
    fn envelope_is_bounded_versioned_and_strict() {
        let valid = ready_bytes(1, READY_AT_MS);
        assert!(parse_envelope(&valid).is_ok());
        assert!(matches!(
            parse_envelope(br#"{"version":1,"revision":1}"#),
            Err(CredentialError::InvalidEnvelope)
        ));
        let unknown = format!("{}\n", String::from_utf8_lossy(&valid));
        assert!(
            parse_envelope(unknown.as_bytes()).is_ok(),
            "whitespace is harmless"
        );
        let extra = String::from_utf8_lossy(&valid).replace('}', ",\"extra\":true}");
        assert!(matches!(
            parse_envelope(extra.as_bytes()),
            Err(CredentialError::InvalidEnvelope)
        ));
        assert!(matches!(
            parse_envelope(&vec![b'x'; MAX_ENVELOPE_BYTES + 1]),
            Err(CredentialError::InvalidEnvelope)
        ));

        let legacy = String::from_utf8_lossy(&valid).replacen("\"version\":2", "\"version\":1", 1);
        assert!(matches!(
            parse_envelope(legacy.as_bytes()),
            Err(CredentialError::InvalidEnvelope)
        ));

        let mut invalid_viewer = ready_envelope(1, READY_AT_MS);
        invalid_viewer.viewer_id = Some("viewer\nwith-control".to_owned());
        assert!(matches!(
            invalid_viewer.validate(),
            Err(CredentialError::InvalidEnvelope)
        ));
    }

    #[test]
    fn revoked_delete_pending_requires_exact_nonnegative_confirmation_marker() {
        let mut valid = CredentialEnvelope::ready(
            CLIENT_ID,
            1,
            Zeroizing::new(ACCESS.to_owned()),
            Zeroizing::new(REFRESH.to_owned()),
            READY_AT_MS,
        )
        .expect("ready");
        valid.state = LifecycleState::RevokedDeletePending;
        valid.revoke = Some(RevokeIntent {
            token: SecretText::from_static(REFRESH),
            started_at_ms: 10,
            confirmed_at_ms: Some(10),
        });
        assert!(valid.validate().is_ok());

        for (started_at_ms, confirmed_at_ms) in [
            (-1, Some(-1)),
            (0, Some(-1)),
            (10, None),
            (10, Some(9)),
            (10, Some(11)),
        ] {
            let mut malformed = valid.clone();
            malformed.revoke = Some(RevokeIntent {
                token: SecretText::from_static(REFRESH),
                started_at_ms,
                confirmed_at_ms,
            });
            assert!(
                matches!(malformed.validate(), Err(CredentialError::InvalidEnvelope)),
                "invalid tombstone marker"
            );
        }
    }

    #[test]
    fn debug_display_status_and_errors_are_redacted() {
        let manager = fake_manager(
            MemoryStore::default(),
            FakeTransport::new([]),
            NOW_MS,
            authorizer(),
        );
        assert!(!format!("{manager:?}").contains(ACCESS));
        assert!(!CredentialError::Storage.to_string().contains(REFRESH));
        assert_eq!(CredentialStatus::Ready.to_string(), "ready");

        let mut envelope = ready_envelope(1, READY_AT_MS);
        envelope.viewer_id = Some(VIEWER.to_owned());
        let debug = format!("{envelope:?}");
        assert!(!debug.contains(CLIENT_ID));
        assert!(!debug.contains(VIEWER));
    }

    #[test]
    fn verified_read_bootstraps_viewer_under_lock_and_increments_revision() {
        let initial = ready_bytes(1, READY_AT_MS);
        let store = MemoryStore {
            value: Some(initial.clone()),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());

        let typed_value = manager
            .with_verified_read(|token| {
                assert_eq!(token, ACCESS);
                Ok(VerifiedReadOutcome::with_value(
                    Zeroizing::new(VIEWER.to_owned()),
                    7_u8,
                ))
            })
            .expect("verified read");
        assert_eq!(typed_value, 7);
        assert_eq!(writes.get(), 1);
        let record = manager.load_record().expect("record").expect("record");
        assert_eq!(record.envelope.revision, 2);
        assert_eq!(record.envelope.viewer_id.as_deref(), Some(VIEWER));
    }

    #[test]
    fn verified_read_matching_viewer_is_idempotent_and_mismatch_preserves_bytes() {
        let initial = ready_bytes_with_viewer(7, READY_AT_MS, VIEWER);
        let store = MemoryStore {
            value: Some(initial.clone()),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());
        manager
            .with_verified_read(|_| Ok(VerifiedReadOutcome::for_test(VIEWER)))
            .expect("matching viewer");
        assert_eq!(writes.get(), 0);
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            initial
        );

        let before_mismatch = manager
            .load_record()
            .expect("record")
            .expect("record")
            .bytes;
        assert_eq!(
            manager.with_verified_read(|_| Ok(VerifiedReadOutcome::for_test(OTHER_VIEWER))),
            Err(ApiError::Credential(CredentialError::Configuration))
        );
        assert_eq!(writes.get(), 0);
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            before_mismatch
        );
    }

    #[test]
    fn verified_read_rejects_replacement_before_existing_viewer_match() {
        let initial = ready_bytes_with_viewer(7, READY_AT_MS, VIEWER);
        let replacement = ready_bytes_with_viewer(8, READY_AT_MS, OTHER_VIEWER);
        let replacement_pending = Rc::new(Cell::new(false));
        let store = MemoryStore {
            value: Some(initial),
            replace_on_next_read: Some(Rc::clone(&replacement_pending)),
            replacement: Some(replacement.clone()),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());
        assert_eq!(
            manager.with_verified_read(|_| {
                replacement_pending.set(true);
                Ok(VerifiedReadOutcome::for_test(VIEWER))
            }),
            Err(ApiError::Credential(CredentialError::StorageUncertain))
        );
        assert_eq!(writes.get(), 0);
        let record = manager.load_record().expect("record").expect("record");
        assert_eq!(record.bytes, replacement);
        assert_eq!(record.envelope.viewer_id.as_deref(), Some(OTHER_VIEWER));
    }

    #[test]
    fn verified_read_rejects_replacement_before_first_viewer_binding() {
        let initial = ready_bytes(1, READY_AT_MS);
        let replacement = ready_bytes_with_viewer(2, READY_AT_MS, OTHER_VIEWER);
        let replacement_pending = Rc::new(Cell::new(false));
        let store = MemoryStore {
            value: Some(initial),
            replace_on_next_read: Some(Rc::clone(&replacement_pending)),
            replacement: Some(replacement.clone()),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());
        assert_eq!(
            manager.with_verified_read(|_| {
                replacement_pending.set(true);
                Ok(VerifiedReadOutcome::for_test(VIEWER))
            }),
            Err(ApiError::Credential(CredentialError::StorageUncertain))
        );
        assert_eq!(writes.get(), 0);
        let record = manager.load_record().expect("record").expect("record");
        assert_eq!(record.bytes, replacement);
        assert_eq!(record.envelope.viewer_id.as_deref(), Some(OTHER_VIEWER));
    }

    #[test]
    fn failed_or_invalid_verified_read_never_bootstraps_or_writes() {
        let initial = ready_bytes(1, READY_AT_MS);
        let store = MemoryStore {
            value: Some(initial.clone()),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());
        assert_eq!(
            manager.with_verified_read(|_| {
                Err::<VerifiedReadOutcome<()>, _>(ApiError::ActorIdentityMismatch)
            }),
            Err(ApiError::ActorIdentityMismatch)
        );
        assert_eq!(writes.get(), 0);
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            initial
        );

        assert_eq!(
            manager.with_verified_read(|_| {
                Err::<VerifiedReadOutcome<()>, _>(ApiError::ReadFieldsInvalid)
            }),
            Err(ApiError::ReadFieldsInvalid)
        );
        assert_eq!(writes.get(), 0);

        for viewer_id in ["", "   ", "bad\nviewer"] {
            assert_eq!(
                manager.with_verified_read(|_| Ok(VerifiedReadOutcome::for_test(viewer_id))),
                Err(ApiError::Credential(CredentialError::Configuration))
            );
            assert_eq!(writes.get(), 0);
        }
    }

    #[test]
    fn client_binding_mismatch_does_not_invoke_read_or_write() {
        let mut envelope = ready_envelope(1, READY_AT_MS);
        envelope.client_id = "different-client".to_owned();
        let initial = serialize_envelope(&envelope).expect("bytes");
        let store = MemoryStore {
            value: Some(initial.clone()),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let callback_called = Rc::new(Cell::new(false));
        let callback_called_in_closure = Rc::clone(&callback_called);
        let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());
        assert_eq!(
            manager.with_verified_read(|_| {
                callback_called_in_closure.set(true);
                Ok(VerifiedReadOutcome::for_test(VIEWER))
            }),
            Err(ApiError::Credential(CredentialError::Configuration))
        );
        assert!(!callback_called.get());
        assert_eq!(writes.get(), 0);
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            initial
        );
    }

    #[test]
    fn bind_write_failure_preserves_record() {
        let initial = ready_bytes(1, READY_AT_MS);
        let store = MemoryStore {
            value: Some(initial.clone()),
            fail_write: true,
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());
        assert_eq!(
            manager.with_verified_read(|_| Ok(VerifiedReadOutcome::for_test(VIEWER))),
            Err(ApiError::Credential(CredentialError::Storage))
        );
        assert_eq!(writes.get(), 1);
        let record = manager.load_record().expect("record").expect("record");
        assert_eq!(record.bytes, initial);
        assert_eq!(record.envelope.viewer_id, None);
    }

    #[test]
    fn login_requires_confirmed_logout_before_replacing_any_record() {
        let initial = ready_bytes(1, READY_AT_MS);
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(initial.clone()),
                ..MemoryStore::default()
            },
            FakeTransport::new([]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(manager.login(), Err(CredentialError::PendingLifecycle));
        let loaded = manager.load_record().expect("read").expect("record");
        assert_eq!(loaded.bytes.as_slice(), initial.as_slice());

        let replay_pending = replay_pending_bytes(NOW_MS, 1, false);
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(replay_pending.clone()),
                ..MemoryStore::default()
            },
            FakeTransport::new([]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(manager.login(), Err(CredentialError::PendingLifecycle));
        assert_eq!(
            manager
                .load_record()
                .expect("read")
                .expect("record")
                .bytes
                .as_slice(),
            replay_pending.as_slice()
        );
    }

    #[test]
    fn login_preserves_absence_when_authorization_or_write_fails() {
        let mut manager = fake_manager(
            MemoryStore::default(),
            FakeTransport::new([]),
            NOW_MS,
            FakeAuthorizer {
                calls: 0,
                result: Some(Err(OAuthError::Browser(
                    crate::linear::oauth::BrowserError::Unsupported,
                ))),
            },
        );
        assert_eq!(
            manager.login(),
            Err(CredentialError::Authorization(OAuthError::Browser(
                crate::linear::oauth::BrowserError::Unsupported
            )))
        );
        assert!(manager.load_record().expect("read").is_none());

        let mut manager = fake_manager(
            MemoryStore {
                fail_write: true,
                ..MemoryStore::default()
            },
            FakeTransport::new([]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(manager.login(), Err(CredentialError::Storage));
        assert!(manager.load_record().expect("read").is_none());
    }

    #[test]
    fn write_error_after_commit_is_confirmed_without_compensation() {
        let old_bytes = ready_bytes(1, READY_AT_MS);
        let store = MemoryStore {
            value: Some(old_bytes.clone()),
            write_error_after_commit: true,
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let deletes = Rc::clone(&store.deletes);
        let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());
        let replacement = ready_envelope(2, READY_AT_MS);

        let written = manager
            .write_record(&replacement, Some(&old_bytes))
            .expect("write");
        assert_eq!(
            written.bytes,
            serialize_envelope(&replacement).expect("bytes")
        );
        assert_eq!(writes.get(), 1);
        assert_eq!(deletes.get(), 0);
        assert_eq!(manager.status(), CredentialStatus::Ready);
    }

    #[test]
    fn definite_write_failure_preserves_old_or_absent_state_without_mutation() {
        let old_store = MemoryStore {
            value: Some(ready_bytes(1, READY_AT_MS)),
            fail_write: true,
            ..MemoryStore::default()
        };
        let old_writes = Rc::clone(&old_store.writes);
        let old_deletes = Rc::clone(&old_store.deletes);
        let old_bytes = old_store.value.clone().expect("old record");
        let mut manager = fake_manager(old_store, FakeTransport::new([]), NOW_MS, authorizer());
        let replacement = ready_envelope(2, READY_AT_MS);
        assert!(matches!(
            manager.write_record(&replacement, Some(&old_bytes)),
            Err(CredentialError::Storage)
        ));
        assert_eq!(old_writes.get(), 1);
        assert_eq!(old_deletes.get(), 0);
        assert_eq!(
            manager.load_record().expect("read").expect("record").bytes,
            old_bytes
        );

        let absent_store = MemoryStore {
            fail_write: true,
            ..MemoryStore::default()
        };
        let absent_writes = Rc::clone(&absent_store.writes);
        let absent_deletes = Rc::clone(&absent_store.deletes);
        let mut manager = fake_manager(absent_store, FakeTransport::new([]), NOW_MS, authorizer());
        assert!(matches!(
            manager.write_record(&replacement, None),
            Err(CredentialError::Storage)
        ));
        assert_eq!(absent_writes.get(), 1);
        assert_eq!(absent_deletes.get(), 0);
        assert!(manager.load_record().expect("read").is_none());
    }

    #[test]
    fn read_back_uncertainty_does_not_attempt_compensation() {
        for read_after_write in [ReadAfterWrite::Error, ReadAfterWrite::Unexpected] {
            let store = MemoryStore {
                value: Some(ready_bytes(1, READY_AT_MS)),
                read_after_write,
                ..MemoryStore::default()
            };
            let writes = Rc::clone(&store.writes);
            let deletes = Rc::clone(&store.deletes);
            let mut manager = fake_manager(store, FakeTransport::new([]), NOW_MS, authorizer());
            let old_bytes = ready_bytes(1, READY_AT_MS);
            let replacement = ready_envelope(2, READY_AT_MS);

            assert!(matches!(
                manager.write_record(&replacement, Some(&old_bytes)),
                Err(CredentialError::StorageUncertain)
            ));
            assert_eq!(writes.get(), 1);
            assert_eq!(deletes.get(), 0);
        }
    }

    #[test]
    fn status_is_side_effect_free_and_distinguishes_missing_from_read_error() {
        let mut manager = fake_manager(
            MemoryStore::default(),
            FakeTransport::new([]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(manager.status(), CredentialStatus::SignedOut);
        let mut manager = fake_manager(
            MemoryStore {
                fail_read: true,
                ..MemoryStore::default()
            },
            FakeTransport::new([]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(manager.status(), CredentialStatus::Unavailable);
        let initial = ready_bytes(1, READY_AT_MS);
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(initial),
                ..MemoryStore::default()
            },
            FakeTransport::new([]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(manager.status(), CredentialStatus::Ready);
    }

    #[test]
    fn status_classifies_replay_states_without_mutation() {
        for (attempt_count, replay_consumed, clock_ms, expected) in [
            (
                1,
                false,
                Some(REPLAY_DEADLINE_MS - 1),
                CredentialStatus::ReplayPending,
            ),
            (
                1,
                false,
                Some(REPLAY_DEADLINE_MS),
                CredentialStatus::ReauthorizationRequired,
            ),
            (
                2,
                true,
                Some(NOW_MS + 1),
                CredentialStatus::ReauthorizationRequired,
            ),
            (1, false, Some(NOW_MS - 1), CredentialStatus::Unavailable),
            (1, false, None, CredentialStatus::Unavailable),
        ] {
            let initial = replay_pending_bytes(NOW_MS, attempt_count, replay_consumed);
            let store = MemoryStore {
                value: Some(initial.clone()),
                ..MemoryStore::default()
            };
            let writes = Rc::clone(&store.writes);
            let deletes = Rc::clone(&store.deletes);
            let mut manager = match clock_ms {
                Some(clock_ms) => fake_manager_with_clock(
                    store,
                    FakeTransport::new([]),
                    Rc::new(Cell::new(clock_ms)),
                    authorizer(),
                ),
                None => CredentialManager::with_dependencies(
                    CLIENT_ID,
                    Box::new(store),
                    Box::new(FakeTransport::new([])),
                    Box::new(FailingClock),
                    Box::new(FakeLock),
                    Box::new(authorizer()),
                ),
            };

            assert_status_without_mutation(&mut manager, expected, &initial, &writes, &deletes);
        }
    }

    #[test]
    fn refresh_persists_intent_before_one_request_and_replaces_bundle_on_success() {
        let initial = ready_bytes(1, EXPIRED_AT_MS);
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(initial),
                ..MemoryStore::default()
            },
            FakeTransport::new([FakeOutcome::Response(token_response(
                NEW_ACCESS,
                NEW_REFRESH,
                60,
            ))]),
            NOW_MS,
            authorizer(),
        );
        let access = manager
            .with_access_token(|token| token.to_owned())
            .expect("refresh");
        assert_eq!(access, NEW_ACCESS);
        assert_eq!(manager.status(), CredentialStatus::Ready);
    }

    #[test]
    fn refresh_and_ambiguous_refresh_preserve_verified_viewer_binding() {
        let initial = ready_bytes_with_viewer(1, EXPIRED_AT_MS, VIEWER);
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(initial),
                ..MemoryStore::default()
            },
            FakeTransport::new([FakeOutcome::Response(token_response(
                NEW_ACCESS,
                NEW_REFRESH,
                60,
            ))]),
            NOW_MS,
            authorizer(),
        );
        manager
            .with_access_token(|token| token.to_owned())
            .expect("refresh");
        let record = manager.load_record().expect("record").expect("record");
        assert_eq!(record.envelope.viewer_id.as_deref(), Some(VIEWER));

        let initial = ready_bytes_with_viewer(1, EXPIRED_AT_MS, VIEWER);
        let store = MemoryStore {
            value: Some(initial.clone()),
            ..MemoryStore::default()
        };
        let mut manager = fake_manager(
            store,
            FakeTransport::new([FakeOutcome::NoResponse]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::RefreshAmbiguous)
        );
        let pending = manager.load_record().expect("record").expect("record");
        assert_eq!(pending.envelope.viewer_id.as_deref(), Some(VIEWER));
        assert!(
            pending
                .bytes
                .windows(VIEWER.len())
                .any(|window| window == VIEWER.as_bytes())
        );
    }

    #[test]
    fn refresh_replacement_during_provider_call_is_not_adopted() {
        let replace = Rc::new(Cell::new(false));
        let replacement = ready_bytes(9, READY_AT_MS);
        let store = MemoryStore {
            value: Some(ready_bytes(1, EXPIRED_AT_MS)),
            replace_on_next_read: Some(Rc::clone(&replace)),
            replacement: Some(replacement.clone()),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let deletes = Rc::clone(&store.deletes);
        let mut transport = FakeTransport::new([FakeOutcome::Response(token_response(
            NEW_ACCESS,
            NEW_REFRESH,
            60,
        ))]);
        transport.replace_store_after_refresh(replace);
        let mut manager = fake_manager(store, transport, NOW_MS, authorizer());

        assert_eq!(
            manager.with_access_token(|_| "unexpected callback"),
            Err(CredentialError::StorageUncertain)
        );
        assert_eq!(writes.get(), 1);
        assert_eq!(deletes.get(), 0);
        assert_eq!(
            manager.load_record().expect("read").expect("record").bytes,
            replacement
        );
    }

    #[test]
    fn refresh_returns_exact_ready_record_after_post_verification_replacement() {
        let replacement = ready_bytes(9, READY_AT_MS);
        let store = MemoryStore {
            value: Some(ready_bytes(1, EXPIRED_AT_MS)),
            replace_after_write_verification: Some((2, replacement.clone())),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let deletes = Rc::clone(&store.deletes);
        let mut manager = fake_manager(
            store,
            FakeTransport::new([FakeOutcome::Response(token_response(
                NEW_ACCESS,
                NEW_REFRESH,
                60,
            ))]),
            NOW_MS,
            authorizer(),
        );

        assert_eq!(
            manager.with_access_token(|token| token.to_owned()),
            Ok(NEW_ACCESS.to_owned())
        );
        assert_eq!(writes.get(), 2);
        assert_eq!(deletes.get(), 0);
        assert_eq!(
            manager.load_record().expect("read").expect("record").bytes,
            replacement
        );
    }

    #[test]
    fn first_refresh_clock_failure_retains_unconsumed_replay() {
        let pending =
            replay_pending_bytes_with_revision_and_expiry(2, NOW_MS, 1, false, EXPIRED_AT_MS);
        let store = MemoryStore {
            value: Some(ready_bytes(1, EXPIRED_AT_MS)),
            ..MemoryStore::default()
        };
        let deletes = Rc::clone(&store.deletes);
        let transport = FakeTransport::new([FakeOutcome::Response(token_response(
            NEW_ACCESS,
            NEW_REFRESH,
            60,
        ))]);
        let revokes = Rc::clone(&transport.revokes);
        let mut manager = CredentialManager::with_dependencies(
            CLIENT_ID,
            Box::new(store),
            Box::new(transport),
            Box::new(FailAfterFirstClock {
                now: NOW_MS,
                calls: Rc::new(Cell::new(0)),
            }),
            Box::new(FakeLock),
            Box::new(authorizer()),
        );

        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::ClockRollback)
        );
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            pending
        );
        assert_eq!(manager.logout(true), Err(CredentialError::PendingLifecycle));
        assert_eq!(revokes.get(), 0);
        assert_eq!(deletes.get(), 0);
    }

    #[test]
    fn no_response_retains_replay_pending_and_never_serves_old_access() {
        let initial = ready_bytes(1, EXPIRED_AT_MS);
        let store = MemoryStore {
            value: Some(initial),
            ..MemoryStore::default()
        };
        let deletes = Rc::clone(&store.deletes);
        let transport = FakeTransport::new([FakeOutcome::NoResponse]);
        let revokes = Rc::clone(&transport.revokes);
        let mut manager = fake_manager(store, transport, NOW_MS, authorizer());
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::RefreshAmbiguous)
        );
        assert_eq!(manager.status(), CredentialStatus::ReplayPending);
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            replay_pending_bytes_with_revision_and_expiry(2, NOW_MS, 1, false, EXPIRED_AT_MS)
        );
        assert_eq!(manager.logout(true), Err(CredentialError::PendingLifecycle));
        assert_eq!(revokes.get(), 0);
        assert_eq!(deletes.get(), 0);
    }

    #[test]
    fn one_replay_can_recover_a_lost_response_and_rotates_both_tokens() {
        let clock = Rc::new(Cell::new(NOW_MS));
        let transport = FakeTransport::new([
            FakeOutcome::NoResponse,
            FakeOutcome::Response(token_response(NEW_ACCESS, NEW_REFRESH, 60)),
        ]);
        let calls = Rc::clone(&transport.refreshes);
        let mut manager = fake_manager_with_clock(
            MemoryStore {
                value: Some(ready_bytes(1, EXPIRED_AT_MS)),
                ..MemoryStore::default()
            },
            transport,
            Rc::clone(&clock),
            authorizer(),
        );
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::RefreshAmbiguous)
        );
        clock.set(NOW_MS + 1);
        let access = manager
            .with_access_token(|token| token.to_owned())
            .expect("one replay");
        assert_eq!(access, NEW_ACCESS);
        assert_eq!(calls.get(), 2);
        assert_eq!(manager.status(), CredentialStatus::Ready);
        let record = manager.load_record().expect("record").expect("record");
        assert!(
            record
                .bytes
                .windows(NEW_REFRESH.len())
                .any(|window| window == NEW_REFRESH.as_bytes())
        );
    }

    #[test]
    fn replay_failure_is_persistently_consumed_and_cannot_send_a_third_request() {
        let clock = Rc::new(Cell::new(NOW_MS));
        let transport = FakeTransport::new([FakeOutcome::NoResponse, FakeOutcome::NoResponse]);
        let calls = Rc::clone(&transport.refreshes);
        let revokes = Rc::clone(&transport.revokes);
        let store = MemoryStore {
            value: Some(ready_bytes(1, EXPIRED_AT_MS)),
            ..MemoryStore::default()
        };
        let deletes = Rc::clone(&store.deletes);
        let mut manager =
            fake_manager_with_clock(store, transport, Rc::clone(&clock), authorizer());
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::RefreshAmbiguous)
        );
        clock.set(NOW_MS + 1);
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::ReauthorizationRequired)
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            replay_pending_bytes_with_revision_and_expiry(3, NOW_MS, 2, true, EXPIRED_AT_MS)
        );
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::ReplayConsumed)
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(manager.logout(true), Err(CredentialError::PendingLifecycle));
        assert_eq!(revokes.get(), 0);
        assert_eq!(deletes.get(), 0);
        assert_eq!(manager.status(), CredentialStatus::ReauthorizationRequired);
    }

    #[test]
    fn clock_rollback_retains_replay_pending_without_a_replay_send() {
        let clock = Rc::new(Cell::new(NOW_MS));
        let transport = FakeTransport::new([FakeOutcome::NoResponse]);
        let calls = Rc::clone(&transport.refreshes);
        let revokes = Rc::clone(&transport.revokes);
        let store = MemoryStore {
            value: Some(ready_bytes(1, EXPIRED_AT_MS)),
            ..MemoryStore::default()
        };
        let deletes = Rc::clone(&store.deletes);
        let mut manager =
            fake_manager_with_clock(store, transport, Rc::clone(&clock), authorizer());
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::RefreshAmbiguous)
        );
        let pending =
            replay_pending_bytes_with_revision_and_expiry(2, NOW_MS, 1, false, EXPIRED_AT_MS);
        clock.set(NOW_MS - 1);
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::ClockRollback)
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            pending
        );
        assert_eq!(manager.status(), CredentialStatus::Unavailable);
        assert_eq!(manager.logout(true), Err(CredentialError::PendingLifecycle));
        assert_eq!(revokes.get(), 0);
        assert_eq!(deletes.get(), 0);
    }

    #[test]
    fn expired_replay_retains_exact_pending_bytes_without_side_effects() {
        let pending = replay_pending_bytes_with_revision(2, NOW_MS, 1, false);
        let store = MemoryStore {
            value: Some(pending.clone()),
            ..MemoryStore::default()
        };
        let deletes = Rc::clone(&store.deletes);
        let transport = FakeTransport::new([]);
        let revokes = Rc::clone(&transport.revokes);
        let mut manager = fake_manager_with_clock(
            store,
            transport,
            Rc::new(Cell::new(REPLAY_DEADLINE_MS)),
            authorizer(),
        );

        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::ReplayExpired)
        );
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .bytes,
            pending
        );
        assert_eq!(manager.logout(true), Err(CredentialError::PendingLifecycle));
        assert_eq!(revokes.get(), 0);
        assert_eq!(deletes.get(), 0);
        assert_eq!(manager.status(), CredentialStatus::ReauthorizationRequired);
    }

    #[test]
    fn invalid_first_refresh_response_retains_one_replay_and_can_recover() {
        for outcome in [
            FakeOutcome::Response(ProviderResponse::synthetic(400, b"{}").expect("response")),
            FakeOutcome::Response(
                ProviderResponse::synthetic(200, br#"{"unexpected":true}"#).expect("response"),
            ),
            FakeOutcome::Response(token_response(NEW_ACCESS, NEW_REFRESH, u64::MAX)),
        ] {
            let store = MemoryStore {
                value: Some(ready_bytes(1, EXPIRED_AT_MS)),
                ..MemoryStore::default()
            };
            let deletes = Rc::clone(&store.deletes);
            let transport = FakeTransport::new([
                outcome,
                FakeOutcome::Response(token_response(NEW_ACCESS, NEW_REFRESH, 60)),
            ]);
            let refreshes = Rc::clone(&transport.refreshes);
            let revokes = Rc::clone(&transport.revokes);
            let clock = Rc::new(Cell::new(NOW_MS));
            let mut manager =
                fake_manager_with_clock(store, transport, Rc::clone(&clock), authorizer());
            assert_eq!(
                manager.with_access_token(|_| ()),
                Err(CredentialError::RefreshAmbiguous)
            );
            assert_eq!(manager.status(), CredentialStatus::ReplayPending);
            assert_eq!(
                manager
                    .load_record()
                    .expect("record")
                    .expect("record")
                    .bytes,
                replay_pending_bytes_with_revision_and_expiry(2, NOW_MS, 1, false, EXPIRED_AT_MS)
            );
            assert_eq!(manager.logout(true), Err(CredentialError::PendingLifecycle));
            assert_eq!(revokes.get(), 0);
            assert_eq!(deletes.get(), 0);

            clock.set(NOW_MS + 1);
            assert_eq!(
                manager.with_access_token(|token| token.to_owned()),
                Ok(NEW_ACCESS.to_owned())
            );
            assert_eq!(refreshes.get(), 2);
            assert_eq!(manager.status(), CredentialStatus::Ready);
            assert_eq!(revokes.get(), 0);
            assert_eq!(deletes.get(), 0);
        }
    }

    #[test]
    fn login_is_blocked_while_refresh_intent_is_pending() {
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(ready_bytes(1, EXPIRED_AT_MS)),
                ..MemoryStore::default()
            },
            FakeTransport::new([FakeOutcome::NoResponse]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(
            manager.with_access_token(|_| ()),
            Err(CredentialError::RefreshAmbiguous)
        );
        assert_eq!(manager.login(), Err(CredentialError::PendingLifecycle));
    }

    #[test]
    fn replay_deadline_is_strict_at_1_799_999_and_1_800_000_ms() {
        assert_eq!(REPLAY_GRACE_MILLIS, 1_800_000);
        let intent = RefreshIntent {
            client_id: CLIENT_ID.to_owned(),
            refresh_token: SecretText::from_static(REFRESH),
            first_send_at_ms: 0,
            replay_deadline_ms: REPLAY_GRACE_MILLIS,
            attempt_count: 1,
            replay_consumed: false,
        };
        let mut envelope = CredentialEnvelope::ready(
            CLIENT_ID,
            1,
            Zeroizing::new(ACCESS.to_owned()),
            Zeroizing::new(REFRESH.to_owned()),
            NOW_MS,
        )
        .expect("ready");
        envelope.state = LifecycleState::ReplayPending;
        envelope.refresh = Some(intent);
        assert!(envelope.validate().is_ok());
        let intent = envelope.refresh.as_ref().expect("intent");
        assert!(replay_is_eligible(intent, 1_799_999));
        assert!(!replay_is_eligible(intent, 1_800_000));
    }

    #[test]
    fn replay_consumed_before_send_allows_no_third_request() {
        let mut envelope = CredentialEnvelope::ready(
            CLIENT_ID,
            1,
            Zeroizing::new(ACCESS.to_owned()),
            Zeroizing::new(REFRESH.to_owned()),
            NOW_MS,
        )
        .expect("ready");
        envelope.state = LifecycleState::ReplayPending;
        envelope.refresh = Some(RefreshIntent {
            client_id: CLIENT_ID.to_owned(),
            refresh_token: SecretText::from_static(REFRESH),
            first_send_at_ms: NOW_MS,
            replay_deadline_ms: REPLAY_DEADLINE_MS,
            attempt_count: 2,
            replay_consumed: true,
        });
        assert!(envelope.validate().is_ok());
        assert_eq!(
            CredentialStatus::ReplayPending.to_string(),
            "replay_pending"
        );
    }

    #[test]
    fn revoke_requires_confirmation_and_preserves_on_ambiguity() {
        let initial = ready_bytes(1, READY_AT_MS);
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(initial.clone()),
                ..MemoryStore::default()
            },
            FakeTransport::new([]),
            NOW_MS,
            authorizer(),
        );
        assert_eq!(
            manager.logout(false),
            Err(CredentialError::ConfirmationRequired)
        );
        assert_eq!(
            manager.logout(true),
            Err(CredentialError::RevokeUnconfirmed)
        );
        assert_eq!(manager.status(), CredentialStatus::RevokePending);
    }

    #[test]
    fn non_200_revoke_is_retained_and_never_retried_automatically() {
        let mut transport = FakeTransport::new([]);
        transport.revoke_outcomes.push_back(FakeOutcome::Response(
            ProviderResponse::synthetic(204, b"").expect("response"),
        ));
        let calls = Rc::clone(&transport.revokes);
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(ready_bytes_with_viewer(1, READY_AT_MS, VIEWER)),
                ..MemoryStore::default()
            },
            transport,
            NOW_MS,
            authorizer(),
        );
        assert_eq!(
            manager.logout(true),
            Err(CredentialError::RevokeUnconfirmed)
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(manager.status(), CredentialStatus::RevokePending);
        assert_eq!(
            manager
                .load_record()
                .expect("record")
                .expect("record")
                .envelope
                .viewer_id
                .as_deref(),
            Some(VIEWER)
        );
        assert_eq!(manager.logout(true), Err(CredentialError::PendingLifecycle));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn exact_200_revoke_writes_tombstone_then_deletes() {
        let initial = ready_bytes(1, READY_AT_MS);
        let mut transport = FakeTransport::new([]);
        transport.revoke_outcomes.push_back(FakeOutcome::Response(
            ProviderResponse::synthetic(200, b"").expect("response"),
        ));
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(initial),
                ..MemoryStore::default()
            },
            transport,
            NOW_MS,
            authorizer(),
        );
        assert_eq!(manager.logout(true), Ok(()));
        assert_eq!(manager.status(), CredentialStatus::SignedOut);
    }

    #[test]
    fn revoke_replacement_during_provider_call_is_not_adopted_or_deleted() {
        let replace = Rc::new(Cell::new(false));
        let replacement = ready_bytes(9, READY_AT_MS);
        let store = MemoryStore {
            value: Some(ready_bytes(1, READY_AT_MS)),
            replace_on_next_read: Some(Rc::clone(&replace)),
            replacement: Some(replacement.clone()),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let deletes = Rc::clone(&store.deletes);
        let mut transport = FakeTransport::new([]);
        transport.revoke_outcomes.push_back(FakeOutcome::Response(
            ProviderResponse::synthetic(200, b"").expect("response"),
        ));
        transport.replace_store_after_revoke(replace);
        let mut manager = fake_manager(store, transport, NOW_MS, authorizer());

        assert_eq!(manager.logout(true), Err(CredentialError::StorageUncertain));
        assert_eq!(writes.get(), 1);
        assert_eq!(deletes.get(), 0);
        assert_eq!(
            manager.load_record().expect("read").expect("record").bytes,
            replacement
        );
    }

    #[test]
    fn revoke_final_delete_requires_exact_tombstone_after_post_verification_replacement() {
        let replacement = ready_bytes(9, READY_AT_MS);
        let store = MemoryStore {
            value: Some(ready_bytes(1, READY_AT_MS)),
            replace_after_write_verification: Some((2, replacement.clone())),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&store.writes);
        let deletes = Rc::clone(&store.deletes);
        let mut transport = FakeTransport::new([]);
        transport.revoke_outcomes.push_back(FakeOutcome::Response(
            ProviderResponse::synthetic(200, b"").expect("response"),
        ));
        let mut manager = fake_manager(store, transport, NOW_MS, authorizer());

        assert_eq!(manager.logout(true), Err(CredentialError::StorageUncertain));
        assert_eq!(writes.get(), 2);
        assert_eq!(deletes.get(), 0);
        assert_eq!(
            manager.load_record().expect("read").expect("record").bytes,
            replacement
        );
    }

    #[test]
    fn definitive_tombstone_write_failure_deletes_only_exact_retained_record() {
        let mut transport = FakeTransport::new([]);
        transport.revoke_outcomes.push_back(FakeOutcome::Response(
            ProviderResponse::synthetic(200, b"").expect("response"),
        ));
        let calls = Rc::clone(&transport.revokes);
        let initial_store = MemoryStore {
            value: Some(ready_bytes(1, READY_AT_MS)),
            fail_write_on: Some(2),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&initial_store.writes);
        let deletes = Rc::clone(&initial_store.deletes);
        let mut manager = fake_manager(initial_store, transport, NOW_MS, authorizer());

        assert_eq!(manager.logout(true), Ok(()));
        assert_eq!(calls.get(), 1);
        assert_eq!(writes.get(), 2);
        assert_eq!(deletes.get(), 1);
        assert_eq!(manager.status(), CredentialStatus::SignedOut);
    }

    #[test]
    fn uncertain_tombstone_write_never_deletes_or_retries_revoke() {
        let mut transport = FakeTransport::new([]);
        transport.revoke_outcomes.push_back(FakeOutcome::Response(
            ProviderResponse::synthetic(200, b"").expect("response"),
        ));
        let calls = Rc::clone(&transport.revokes);
        let initial_store = MemoryStore {
            value: Some(ready_bytes(1, READY_AT_MS)),
            fail_read_after_write: Some(2),
            ..MemoryStore::default()
        };
        let writes = Rc::clone(&initial_store.writes);
        let deletes = Rc::clone(&initial_store.deletes);
        let mut manager = fake_manager(initial_store, transport, NOW_MS, authorizer());

        assert_eq!(manager.logout(true), Err(CredentialError::StorageUncertain));
        assert_eq!(calls.get(), 1);
        assert_eq!(writes.get(), 2);
        assert_eq!(deletes.get(), 0);
    }

    #[test]
    fn delete_failure_keeps_tombstone_and_later_confirmed_logout_finishes_without_revoke() {
        let mut transport = FakeTransport::new([]);
        transport.revoke_outcomes.push_back(FakeOutcome::Response(
            ProviderResponse::synthetic(200, b"").expect("response"),
        ));
        let calls = Rc::clone(&transport.revokes);
        let fail_delete = Rc::new(Cell::new(true));
        let mut manager = fake_manager(
            MemoryStore {
                value: Some(ready_bytes(1, READY_AT_MS)),
                fail_delete: Rc::clone(&fail_delete),
                ..MemoryStore::default()
            },
            transport,
            NOW_MS,
            authorizer(),
        );
        assert_eq!(manager.logout(true), Err(CredentialError::Storage));
        assert_eq!(manager.status(), CredentialStatus::RevokedDeletePending);
        assert_eq!(calls.get(), 1);
        fail_delete.set(false);
        assert_eq!(manager.logout(true), Ok(()));
        assert_eq!(calls.get(), 1);
        assert_eq!(manager.status(), CredentialStatus::SignedOut);
    }

    #[test]
    fn parse_refresh_rejects_unknown_or_malformed_contract() {
        let valid = token_response(NEW_ACCESS, NEW_REFRESH, 60);
        assert!(parse_refresh_response(valid).is_ok());
        let malformed = ProviderResponse::synthetic(
            200,
            br#"{"access_token":"a","refresh_token":"r","token_type":"Bearer","expires_in":60,"scope":"read,write,app:assignable","unexpected":true}"#,
        )
        .expect("response");
        assert!(matches!(
            parse_refresh_response(malformed),
            Err(CredentialError::InvalidInput)
        ));
    }

    #[test]
    fn synthetic_tokens_are_not_debugged() {
        let response = token_response(NEW_ACCESS, NEW_REFRESH, 60);
        assert!(!format!("{response:?}").contains(NEW_ACCESS));
        assert!(!format!("{response:?}").contains(NEW_REFRESH));
        let secret = SecretText::from_static(REFRESH);
        assert!(!format!("{secret:?}").contains(REFRESH));
    }

    #[test]
    fn terminal_delete_requires_an_exact_last_read() {
        let deletes = Rc::new(Cell::new(0));
        let mut manager = CredentialManager::with_dependencies(
            CLIENT_ID,
            Box::new(ReplacingReadStore {
                value: Some(ready_bytes(1, READY_AT_MS)),
                replacement: Some(ready_bytes(2, READY_AT_MS)),
                reads: 0,
                deletes: Rc::clone(&deletes),
            }),
            Box::new(FakeTransport::new([])),
            Box::new(FakeClock {
                now: Rc::new(Cell::new(NOW_MS)),
            }),
            Box::new(FakeLock),
            Box::new(authorizer()),
        );
        let record = manager.load_record().expect("record read").expect("record");
        assert_eq!(
            manager.finish_local_delete(record),
            Err(CredentialError::StorageUncertain)
        );
        assert_eq!(deletes.get(), 0);
    }

    #[test]
    fn process_mutex_serializes_independent_threads() {
        use std::sync::mpsc;
        use std::thread;

        let guard = process_lock().lock().expect("process mutex");
        let (blocked_sender, blocked_receiver) = mpsc::channel();
        let (acquired_sender, acquired_receiver) = mpsc::channel();
        let child = thread::spawn(move || {
            assert!(matches!(
                process_lock().try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            blocked_sender.send(()).expect("blocked signal");
            let child_guard = process_lock().lock().expect("process mutex after release");
            acquired_sender.send(()).expect("acquired signal");
            drop(child_guard);
        });

        blocked_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("child observes the held mutex");
        drop(guard);
        acquired_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("child acquires after release");
        child.join().expect("child lock probe");
    }
}
