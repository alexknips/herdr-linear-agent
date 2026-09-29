//! The async Linear client the event-driven ticker reads and writes through.
//!
//! The rules are those of `transport.rs`: HTTPS only, no redirects, no proxy,
//! no retries, the same timeouts and response bound; reads are accepted only
//! when the viewer is this app, and writes need a credential already bound to
//! that viewer. The credential manager stays synchronous and is not `Send`, so
//! one dedicated thread owns it; each request is a job on that thread, which
//! drives the HTTP call on the runtime with `Handle::block_on` while the lease
//! is held.

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::sync::oneshot;
use zeroize::Zeroizing;

pub use super::api::LinearApi;
use super::credentials::{CredentialError, CredentialManager};
use super::transport::{
    CONNECT_TIMEOUT, GRAPHQL_ENDPOINT, MAX_RESPONSE_BYTES, REQUEST_TIMEOUT, RateHeaders, decode,
    json_content_type, verified,
};
use super::{ApiError, VerifiedReadOutcome};

/// A token lease with the rules of the credential manager: a read's token
/// is released only for a verified read, a write's only once the viewer is
/// bound to the credential.
pub(crate) trait Lease {
    fn read(
        &mut self,
        callback: &mut dyn FnMut(&str) -> Result<VerifiedReadOutcome<Value>, ApiError>,
    ) -> Result<Value, ApiError>;

    fn write(
        &mut self,
        callback: &mut dyn FnMut(&str) -> Result<Value, ApiError>,
    ) -> Result<Value, ApiError>;
}

impl Lease for CredentialManager {
    fn read(
        &mut self,
        callback: &mut dyn FnMut(&str) -> Result<VerifiedReadOutcome<Value>, ApiError>,
    ) -> Result<Value, ApiError> {
        self.with_verified_read(callback)
    }

    fn write(
        &mut self,
        callback: &mut dyn FnMut(&str) -> Result<Value, ApiError>,
    ) -> Result<Value, ApiError> {
        self.with_bound_access_token(callback)
    }
}

type Job = Box<dyn FnOnce(&mut dyn Lease) + Send>;

/// The production client: HTTPS to Linear with the Keychain-held token.
pub struct Client {
    http: reqwest::Client,
    endpoint: &'static str,
    jobs: std::sync::mpsc::Sender<Job>,
    /// The rate-limit headers of the responses not taken yet.
    headers: Arc<Mutex<Vec<RateHeaders>>>,
}

impl Client {
    /// Builds the client of the workspace called `name` with the production
    /// credential manager.
    // Tests cannot reach the Keychain; they build the client with a test lease.
    #[cfg_attr(test, allow(dead_code))]
    pub async fn production(
        name: &str,
        workspace: &crate::config::Workspace,
        state_dir: &std::path::Path,
    ) -> Result<Client, ApiError> {
        let account = name.to_owned();
        let client_id = workspace.client_id.clone();
        let callback_port = workspace.callback_port;
        let lock_path = crate::linear::credentials::lock_path(state_dir, name);
        Self::with_lease(
            move || CredentialManager::production(&account, client_id, callback_port, lock_path),
            GRAPHQL_ENDPOINT,
        )
        .await
    }

    /// Starts the thread that owns the lease `make` builds; the credential
    /// manager is not `Send`, so it is built on that thread and never leaves.
    pub(crate) async fn with_lease<L: Lease + 'static>(
        make: impl FnOnce() -> Result<L, CredentialError> + Send + 'static,
        endpoint: &'static str,
    ) -> Result<Client, ApiError> {
        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .retry(reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ApiError::ClientConfiguration)?;
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        let (ready, built) = oneshot::channel();
        std::thread::Builder::new()
            .name("linear-credentials".into())
            .spawn(move || {
                let mut lease = match make() {
                    Ok(lease) => {
                        let _ = ready.send(Ok(()));
                        lease
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                // Ends when the client, and with it the sender, is dropped.
                while let Ok(job) = queue.recv() {
                    job(&mut lease);
                }
            })
            .map_err(|_| ApiError::ClientConfiguration)?;
        built
            .await
            .map_err(|_| ApiError::ClientConfiguration)?
            .map_err(ApiError::Credential)?;
        Ok(Client {
            http,
            endpoint,
            jobs,
            headers: Arc::default(),
        })
    }

    /// Sends one operation under a credential lease and returns Linear's
    /// `data` object.
    pub async fn request(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> Result<Value, ApiError> {
        let body = serde_json::to_vec(
            &json!({ "operationName": operation, "query": query, "variables": variables }),
        )
        .map_err(|_| ApiError::Configuration)?;
        let http = self.http.clone();
        let endpoint = self.endpoint;
        let headers = self.headers.clone();
        let runtime = tokio::runtime::Handle::current();
        let (answer, answered) = oneshot::channel();
        let job: Job = Box::new(move |lease| {
            let send =
                |token: &str| runtime.block_on(post(&http, endpoint, token, &body, &headers));
            let result = if write {
                lease.write(&mut |token| send(token))
            } else {
                lease.read(&mut |token| verified(send(token)?))
            };
            let _ = answer.send(result);
        });
        self.jobs
            .send(job)
            .map_err(|_| ApiError::ClientConfiguration)?;
        answered.await.unwrap_or(Err(ApiError::ClientConfiguration))
    }
}

impl LinearApi for Client {
    async fn execute(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> Result<Value, ApiError> {
        self.request(operation, query, variables, write).await
    }

    fn take_headers(&self) -> Vec<RateHeaders> {
        std::mem::take(&mut *self.headers.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

async fn post(
    http: &reqwest::Client,
    endpoint: &str,
    token: &str,
    body: &[u8],
    headers: &Mutex<Vec<RateHeaders>>,
) -> Result<Value, ApiError> {
    use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};

    let mut authorization = Zeroizing::new(Vec::with_capacity(7 + token.len()));
    authorization.extend_from_slice(b"Bearer ");
    authorization.extend_from_slice(token.as_bytes());
    let mut authorization =
        HeaderValue::from_bytes(&authorization).map_err(|_| ApiError::Configuration)?;
    authorization.set_sensitive(true);
    let mut response = http
        .post(endpoint)
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, authorization)
        .body(body.to_vec())
        .send()
        .await
        .map_err(|_| ApiError::RequestFailed)?;
    headers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(RateHeaders::parse(response.headers()));
    let status = response.status().as_u16();
    let json_content = json_content_type(
        response
            .headers()
            .get_all(CONTENT_TYPE)
            .iter()
            .filter_map(|v| v.to_str().ok()),
    );
    let mut bytes = Vec::with_capacity(4096);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ApiError::RequestFailed)?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(ApiError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    decode(status, json_content, &bytes)
}

#[cfg(test)]
impl LinearApi for std::sync::Mutex<super::api::fake::FakeLinear> {
    fn execute(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> impl std::future::Future<Output = Result<Value, ApiError>> + Send {
        let result = self
            .lock()
            .unwrap()
            .execute(operation, query, variables, write);
        async move { result }
    }

    fn take_headers(&self) -> Vec<RateHeaders> {
        std::mem::take(&mut self.lock().unwrap().received)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::linear::api::fake::FakeLinear;
    use crate::linear::api::{self, Activity, RunQuery};

    /// A lease that hands out a fixed token, bound or not.
    struct TestLease {
        bound: bool,
    }

    impl Lease for TestLease {
        fn read(
            &mut self,
            callback: &mut dyn FnMut(&str) -> Result<VerifiedReadOutcome<Value>, ApiError>,
        ) -> Result<Value, ApiError> {
            Ok(callback("token")?.into_parts().1)
        }

        fn write(
            &mut self,
            callback: &mut dyn FnMut(&str) -> Result<Value, ApiError>,
        ) -> Result<Value, ApiError> {
            if !self.bound {
                return Err(ApiError::Credential(CredentialError::NotReady));
            }
            callback("token")
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_are_https_only_and_writes_need_a_bound_credential() {
        let unbound = Client::with_lease(
            || Ok(TestLease { bound: false }),
            "http://127.0.0.1:9/graphql",
        )
        .await
        .unwrap();
        let read = unbound
            .request(
                "HlaViewer",
                "query HlaViewer { viewer { id } }",
                json!({}),
                false,
            )
            .await;
        assert_eq!(read, Err(ApiError::RequestFailed), "plain HTTP is refused");
        assert_eq!(
            unbound
                .execute("HlaIssueState", "mutation", json!({}), true)
                .await,
            Err(ApiError::Credential(CredentialError::NotReady))
        );

        let bound = Client::with_lease(
            || Ok(TestLease { bound: true }),
            "http://127.0.0.1:9/graphql",
        )
        .await
        .unwrap();
        assert_eq!(
            bound
                .execute("HlaIssueState", "mutation", json!({}), true)
                .await,
            Err(ApiError::RequestFailed)
        );
        assert_eq!(
            Client::with_lease(
                || Err::<TestLease, _>(CredentialError::NotReady),
                GRAPHQL_ENDPOINT
            )
            .await
            .err(),
            Some(ApiError::Credential(CredentialError::NotReady))
        );
    }

    #[tokio::test]
    async fn the_fake_answers_the_async_operations() {
        let mut fake = FakeLinear::default();
        let issue = fake.add_issue("DATA-1", "DATA", "First");
        let linear = Mutex::new(fake);

        assert_eq!(linear.viewer().await.unwrap().id, api::fake::APP_USER);
        let issues = linear.delegated_issues(&["DATA".into()]).await.unwrap();
        assert_eq!(issues[0].identifier, "DATA-1");
        assert_eq!(linear.issue(&issue).await.unwrap().identifier, "DATA-1");
        assert_eq!(linear.find_session(&issue).await.unwrap(), None);
        let session = linear.create_session(&issue).await.unwrap();
        assert_eq!(session, "session-1");
        assert_eq!(
            linear.find_session(&issue).await.unwrap().as_deref(),
            Some("session-1")
        );
        let thought = Activity::new(api::Content::Thought { body: "Hi".into() });
        linear
            .create_activity(&session, "a-1", &thought)
            .await
            .unwrap();
        assert!(linear.activity_exists(&session, "a-1").await.unwrap());
        assert!(!linear.activity_exists(&session, "a-2").await.unwrap());
        linear
            .set_plan(
                &session,
                &json!([{ "content": "Plan", "status": "pending" }]),
            )
            .await
            .unwrap();
        linear
            .set_issue_state(&issue, "state-progress")
            .await
            .unwrap();
        let fake = linear.lock().unwrap();
        assert_eq!(fake.sessions[0].sent_types(), ["thought"]);
        assert_eq!(fake.issue("DATA-1")["state"]["name"], "In Progress");
    }

    #[tokio::test]
    async fn the_batched_run_read_falls_back_to_one_request_per_run() {
        let mut fake = FakeLinear::default();
        let one = fake.add_issue("DATA-1", "DATA", "First");
        let two = fake.add_issue("DATA-2", "DATA", "Second");
        let session = fake.delegate_session("DATA-1");
        let linear = Mutex::new(fake);
        let query = |issue_id: &str, session_id: &str| RunQuery {
            issue_id: issue_id.into(),
            session_id: session_id.into(),
            cursor: "2026-09-24T00:00:00Z".into(),
        };
        let updates = linear
            .read_runs(&[query(&one, &session), query(&two, "session-missing")])
            .await;
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].as_ref().unwrap().issue.state_type, "unstarted");
        assert_eq!(
            updates[1],
            Err(ApiError::Graphql("Entity not found: AgentSession".into()))
        );
        assert_eq!(linear.lock().unwrap().count("HlaRuns"), 3);

        linear.lock().unwrap().fail_next = Some(ApiError::RateLimited);
        let updates = linear.read_runs(&[query(&one, &session)]).await;
        assert_eq!(updates, [Err(ApiError::RateLimited)]);
        assert_eq!(linear.lock().unwrap().count("HlaRuns"), 4);
    }
}
