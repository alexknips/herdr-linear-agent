//! The Linear operations herdr-linear-agent uses. Every query and mutation is
//! a fixed string in this file; nothing from the config or an agent becomes
//! GraphQL text. Every read also selects the viewer, so the transport can
//! check that the credential still belongs to the app user.

use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ApiError;
use super::transport::RateHeaders;

/// Pages read per poll at most: 50 issues each.
const MAX_PAGES: usize = 4;

const VIEWER_QUERY: &str = r#"query HlaViewer {
  viewer { id app isMe name }
}"#;

const DELEGATED_QUERY: &str = r#"query HlaDelegatedIssues($teamKeys: [String!]!, $after: String) {
  viewer { id app isMe }
  issues(
    first: 50
    after: $after
    filter: {
      delegate: { isMe: { eq: true } }
      team: { key: { in: $teamKeys } }
      state: { type: { nin: ["completed", "canceled"] } }
    }
  ) {
    nodes {
      id identifier title url updatedAt state { type } team { key }
      agentSessions(first: 10) {
        nodes {
          id status createdAt appUser { id } creator { id name }
          activities(first: 1) { nodes { createdAt content { __typename } } }
        }
      }
      history(first: 20) { nodes { createdAt actor { id name } toDelegate { id } } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

const ISSUE_QUERY: &str = r#"query HlaIssue($id: String!) {
  viewer { id app isMe }
  issue(id: $id) {
    id identifier title url description updatedAt estimate
    state { id name type }
    delegate { id }
    team {
      id key name issueEstimationType
      states { nodes { id name type position } }
    }
    labels { nodes { name parent { name } } }
    comments(first: 50) { nodes { id parentId isArtificialAgentSessionRoot body createdAt user { name isMe } } }
  }
}"#;

/// The app's recent sessions. Linear creates one when an issue is delegated
/// to the app (with the agent session webhook category enabled).
const SESSIONS_QUERY: &str = r#"query HlaSessions {
  viewer { id app isMe }
  agentSessions(first: 50) { nodes { id status createdAt issue { id } appUser { id } } }
}"#;

const SESSION_CREATE: &str = r#"mutation HlaSessionCreate($issueId: String!) {
  agentSessionCreateOnIssue(input: { issueId: $issueId }) { success agentSession { id } }
}"#;

const ACTIVITY_CREATE: &str = r#"mutation HlaActivityCreate($input: AgentActivityCreateInput!) {
  agentActivityCreate(input: $input) { success agentActivity { id } }
}"#;

const ACTIVITY_FIND: &str = r#"query HlaActivityFind($sessionId: String!, $id: ID!) {
  viewer { id app isMe }
  agentSession(id: $sessionId) { activities(filter: { id: { eq: $id } }) { nodes { id } } }
}"#;

const SESSION_UPDATE: &str = r#"mutation HlaSessionUpdate($id: String!, $input: AgentSessionUpdateInput!) {
  agentSessionUpdate(id: $id, input: $input) { success }
}"#;

const ISSUE_STATE_UPDATE: &str = r#"mutation HlaIssueState($id: String!, $stateId: String!) {
  issueUpdate(id: $id, input: { stateId: $stateId }) { success }
}"#;

/// One run's part of `HlaRuns`; `@` is replaced by the run's index. The text
/// is fixed: only the alias number varies.
const RUN_PART: &str = r#"  i@: issue(id: $i@) { updatedAt state { type name } delegate { id } }
  s@: agentSession(id: $s@) {
    activities(first: 50, filter: { type: { eq: "prompt" }, createdAt: { gt: $c@ } }) {
      nodes { id createdAt signal user { id name } content { ... on AgentActivityPromptContent { body } } }
    }
  }
"#;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Viewer {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// One delegated issue as the poll sees it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueRef {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub updated_at: String,
    #[serde(deserialize_with = "state_type")]
    pub state: String,
    #[serde(deserialize_with = "team_key")]
    pub team: String,
    /// Who delegated the issue to the app, and when.
    #[serde(skip)]
    pub delegator: Option<Delegator>,
    /// The app's newest session on the issue.
    #[serde(skip)]
    pub session: Option<SessionRef>,
}

/// Who delegated an issue to the app, and when: the actor of the newest
/// history entry that delegated it, or, for an issue delegated as it was
/// created (which leaves no entry), the creator of the app's first session.
/// Delegating again adds an entry but no session: Linear keeps the one it has.
#[derive(Debug, Clone, PartialEq)]
pub struct Delegator {
    /// `None` when no person did: automation or an agent.
    pub user: Option<User>,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRef {
    pub id: String,
    pub status: String,
    /// When the session's latest activity is a response, when it was sent.
    /// A session's `endedAt` and `updatedAt` stay at its first completion.
    pub responded_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

fn state_type<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    struct State {
        r#type: String,
    }
    Ok(State::deserialize(deserializer)?.r#type)
}

fn team_key<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    struct Team {
        key: String,
    }
    Ok(Team::deserialize(deserializer)?.key)
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WorkflowState {
    pub id: String,
    pub name: String,
    pub r#type: String,
    #[serde(default)]
    pub position: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Team {
    pub id: String,
    pub key: String,
    pub name: String,
    /// `notUsed`, `exponential`, `fibonacci`, `linear` or `tShirt`.
    pub estimation_type: String,
    pub states: Vec<WorkflowState>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Label {
    pub name: String,
    /// The label group's name, for a label inside a group.
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Comment {
    pub author: String,
    pub created_at: String,
    pub body: String,
    /// Part of an agent session's thread (its root, an activity, or a reply)
    /// or written by this app, rather than a person's comment on the issue.
    #[serde(default)]
    pub in_session: bool,
}

/// Everything the run folder's `issue.md` and the routing rules need.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct IssueDetail {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub description: String,
    pub updated_at: String,
    pub estimate: Option<f64>,
    pub state: WorkflowState,
    pub delegate_id: Option<String>,
    pub team: Team,
    pub labels: Vec<Label>,
    pub comments: Vec<Comment>,
}

/// The content of an activity the plugin sends. The JSON shape follows
/// Linear's activity content payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Thought {
        body: String,
    },
    Action {
        action: String,
        parameter: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    Elicitation {
        body: String,
    },
    Response {
        body: String,
    },
    Error {
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub content: Content,
    #[serde(default)]
    pub ephemeral: bool,
    /// `select` for an elicitation with options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_metadata: Option<Value>,
}

impl Activity {
    pub fn new(content: Content) -> Self {
        Activity {
            content,
            ephemeral: false,
            signal: None,
            signal_metadata: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalUrl {
    pub label: String,
    pub url: String,
}

/// What one tick reads for one active run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunQuery {
    pub issue_id: String,
    pub session_id: String,
    /// Prompts created after this RFC 3339 timestamp are returned.
    pub cursor: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IssueStatus {
    pub updated_at: String,
    pub state_type: String,
    pub state_name: String,
    pub delegate_id: Option<String>,
}

/// A person's message in an Agent Session.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub id: String,
    pub created_at: String,
    pub signal: Option<String>,
    pub user_id: String,
    pub user_name: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunUpdate {
    pub issue: IssueStatus,
    /// Oldest first.
    pub prompts: Vec<Prompt>,
}

/// The Linear operations the ticker needs. An implementation supplies
/// `execute`; every operation's text and parsing is here. Tests fake Linear
/// by implementing `execute`.
pub trait LinearApi: Sync {
    /// Sends one GraphQL operation and returns its `data` object.
    fn execute(
        &self,
        operation: &str,
        query: &str,
        variables: Value,
        write: bool,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    /// The rate-limit headers of every response since the last call, oldest
    /// first.
    fn take_headers(&self) -> Vec<RateHeaders>;

    /// Sends a mutation and returns its `payload` field once its `success`
    /// is true.
    fn mutate(
        &self,
        operation: &'static str,
        query: &'static str,
        variables: Value,
        payload: &'static str,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        async move {
            let data = self.execute(operation, query, variables, true).await?;
            let result = field(&data, payload)?;
            if result["success"].as_bool() != Some(true) {
                return Err(ApiError::Graphql(format!("{payload} did not succeed")));
            }
            Ok(result.clone())
        }
    }

    fn viewer(&self) -> impl Future<Output = Result<Viewer, ApiError>> + Send {
        async move {
            let data = self
                .execute("HlaViewer", VIEWER_QUERY, json!({}), false)
                .await?;
            serde_json::from_value(field(&data, "viewer")?.clone())
                .map_err(|_| ApiError::ReadFieldsInvalid)
        }
    }

    /// Issues delegated to the app user in the teams whose state is neither
    /// completed nor canceled.
    fn delegated_issues(
        &self,
        team_keys: &[String],
    ) -> impl Future<Output = Result<Vec<IssueRef>, ApiError>> + Send {
        async move {
            let mut issues = Vec::new();
            let mut after: Option<String> = None;
            for _ in 0..MAX_PAGES {
                let variables = json!({ "teamKeys": team_keys, "after": after });
                let data = self
                    .execute("HlaDelegatedIssues", DELEGATED_QUERY, variables, false)
                    .await?;
                let viewer = text(field(&data, "viewer")?, "id")?;
                let page = field(&data, "issues")?;
                for node in nodes(page) {
                    let mut issue: IssueRef = serde_json::from_value(node.clone())
                        .map_err(|_| ApiError::ReadFieldsInvalid)?;
                    (issue.delegator, issue.session) =
                        delegation(node, &viewer).map_err(|_| ApiError::ReadFieldsInvalid)?;
                    issues.push(issue);
                }
                let info = &page["pageInfo"];
                if info["hasNextPage"].as_bool() != Some(true) {
                    break;
                }
                after = Some(text(info, "endCursor")?);
            }
            Ok(issues)
        }
    }

    fn issue(&self, id: &str) -> impl Future<Output = Result<IssueDetail, ApiError>> + Send {
        async move {
            let data = self
                .execute("HlaIssue", ISSUE_QUERY, json!({ "id": id }), false)
                .await?;
            parse_issue(field(&data, "issue")?)
        }
    }

    /// The issue's newest open session of this app: the one Linear created
    /// on delegation, or one the plugin created earlier.
    fn find_session(
        &self,
        issue_id: &str,
    ) -> impl Future<Output = Result<Option<String>, ApiError>> + Send {
        async move {
            let data = self
                .execute("HlaSessions", SESSIONS_QUERY, json!({}), false)
                .await?;
            let viewer = text(field(&data, "viewer")?, "id")?;
            nodes(field(&data, "agentSessions")?)
                .filter(|s| s["issue"]["id"] == issue_id && s["appUser"]["id"] == viewer.as_str())
                .filter(|s| s["status"] != "complete")
                .max_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()))
                .map(|session| text(session, "id"))
                .transpose()
        }
    }

    /// Opens a new Agent Session on the issue and returns its ID.
    fn create_session(
        &self,
        issue_id: &str,
    ) -> impl Future<Output = Result<String, ApiError>> + Send {
        async move {
            let payload = self
                .mutate(
                    "HlaSessionCreate",
                    SESSION_CREATE,
                    json!({ "issueId": issue_id }),
                    "agentSessionCreateOnIssue",
                )
                .await?;
            text(field(&payload, "agentSession")?, "id")
        }
    }

    /// Sends an activity with the caller's UUID as its ID.
    fn create_activity(
        &self,
        session_id: &str,
        id: &str,
        activity: &Activity,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move {
            let mut input = json!({
                "agentSessionId": session_id,
                "id": id,
                "content": serde_json::to_value(&activity.content).map_err(|_| ApiError::Configuration)?,
                "ephemeral": activity.ephemeral,
            });
            if let Some(signal) = &activity.signal {
                input["signal"] = json!(signal);
            }
            if let Some(metadata) = &activity.signal_metadata {
                input["signalMetadata"] = metadata.clone();
            }
            let variables = json!({ "input": input });
            self.mutate(
                "HlaActivityCreate",
                ACTIVITY_CREATE,
                variables,
                "agentActivityCreate",
            )
            .await
            .map(|_| ())
        }
    }

    fn activity_exists(
        &self,
        session_id: &str,
        id: &str,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send {
        async move {
            let variables = json!({ "sessionId": session_id, "id": id });
            let data = self
                .execute("HlaActivityFind", ACTIVITY_FIND, variables, false)
                .await?;
            Ok(nodes(&field(&data, "agentSession")?["activities"]).any(|n| n["id"] == id))
        }
    }

    fn set_plan(
        &self,
        session_id: &str,
        plan: &Value,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move {
            let variables = json!({ "id": session_id, "input": { "plan": plan } });
            self.mutate(
                "HlaSessionUpdate",
                SESSION_UPDATE,
                variables,
                "agentSessionUpdate",
            )
            .await
            .map(|_| ())
        }
    }

    fn set_external_urls(
        &self,
        session_id: &str,
        urls: &[ExternalUrl],
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move {
            let variables = json!({ "id": session_id, "input": { "externalUrls": urls } });
            self.mutate(
                "HlaSessionUpdate",
                SESSION_UPDATE,
                variables,
                "agentSessionUpdate",
            )
            .await
            .map(|_| ())
        }
    }

    fn set_issue_state(
        &self,
        issue_id: &str,
        state_id: &str,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async move {
            let variables = json!({ "id": issue_id, "stateId": state_id });
            self.mutate(
                "HlaIssueState",
                ISSUE_STATE_UPDATE,
                variables,
                "issueUpdate",
            )
            .await
            .map(|_| ())
        }
    }

    /// Every given run's issue state and new prompts, in one `HlaRuns`
    /// request, in query order.
    fn run_updates(
        &self,
        runs: &[RunQuery],
    ) -> impl Future<Output = Result<Vec<RunUpdate>, ApiError>> + Send {
        async move {
            if runs.is_empty() {
                return Ok(Vec::new());
            }
            let mut declarations = Vec::new();
            let mut body = String::from("  viewer { id app isMe }\n");
            let mut variables = serde_json::Map::new();
            for (n, run) in runs.iter().enumerate() {
                declarations.push(format!(
                    "$i{n}: String!, $s{n}: String!, $c{n}: DateTimeOrDuration!"
                ));
                body.push_str(&RUN_PART.replace('@', &n.to_string()));
                variables.insert(format!("i{n}"), json!(run.issue_id));
                variables.insert(format!("s{n}"), json!(run.session_id));
                variables.insert(format!("c{n}"), json!(run.cursor));
            }
            let query = format!("query HlaRuns({}) {{\n{body}}}", declarations.join(", "));
            let data = self
                .execute("HlaRuns", &query, Value::Object(variables), false)
                .await?;
            (0..runs.len())
                .map(|n| {
                    let issue = field(&data, &format!("i{n}"))?;
                    let session = field(&data, &format!("s{n}"))?;
                    let mut prompts = nodes(&session["activities"])
                        .map(|a| {
                            Ok(Prompt {
                                id: text(a, "id")?,
                                created_at: text(a, "createdAt")?,
                                signal: a["signal"].as_str().map(str::to_string),
                                user_id: a["user"]["id"].as_str().unwrap_or("").to_string(),
                                user_name: a["user"]["name"].as_str().unwrap_or("").to_string(),
                                body: a["content"]["body"].as_str().unwrap_or("").to_string(),
                            })
                        })
                        .collect::<Result<Vec<_>, ApiError>>()?;
                    prompts.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                    Ok(RunUpdate {
                        issue: IssueStatus {
                            updated_at: text(issue, "updatedAt")?,
                            state_type: text(field(issue, "state")?, "type")?,
                            state_name: text(field(issue, "state")?, "name")?,
                            delegate_id: issue["delegate"]["id"].as_str().map(str::to_string),
                        },
                        prompts,
                    })
                })
                .collect()
        }
    }

    /// The batched run read, one result per run in order. When the batch
    /// fails, each run is read with its own request so one broken run does
    /// not hide the others; a rate-limited batch is not split.
    fn read_runs(
        &self,
        runs: &[RunQuery],
    ) -> impl Future<Output = Vec<Result<RunUpdate, ApiError>>> + Send {
        async move {
            match self.run_updates(runs).await {
                Ok(updates) => updates.into_iter().map(Ok).collect(),
                Err(ApiError::RateLimited) => {
                    runs.iter().map(|_| Err(ApiError::RateLimited)).collect()
                }
                Err(_) => {
                    let mut updates = Vec::with_capacity(runs.len());
                    for run in runs {
                        updates.push(
                            self.run_updates(std::slice::from_ref(run))
                                .await
                                .map(|mut one| one.remove(0)),
                        );
                    }
                    updates
                }
            }
        }
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, ApiError> {
    value
        .get(name)
        .filter(|v| !v.is_null())
        .ok_or(ApiError::ReadFieldsInvalid)
}

fn text(value: &Value, name: &str) -> Result<String, ApiError> {
    field(value, name)?
        .as_str()
        .map(str::to_string)
        .ok_or(ApiError::ReadFieldsInvalid)
}

/// An issue's delegator and the app's newest session on it, from a node of
/// `HlaDelegatedIssues`.
fn delegation(
    issue: &Value,
    viewer: &str,
) -> serde_json::Result<(Option<Delegator>, Option<SessionRef>)> {
    let created = |a: &&Value, b: &&Value| a["createdAt"].as_str().cmp(&b["createdAt"].as_str());
    let sessions: Vec<&Value> = nodes(&issue["agentSessions"])
        .filter(|s| s["appUser"]["id"] == viewer)
        .collect();
    let delegated = nodes(&issue["history"])
        .filter(|h| h["toDelegate"]["id"] == viewer)
        .max_by(created)
        .map(|h| (&h["actor"], &h["createdAt"]))
        .or_else(|| {
            let first = sessions.iter().copied().min_by(created)?;
            Some((&first["creator"], &first["createdAt"]))
        });
    let delegator = delegated
        .map(|(user, at)| {
            Ok::<_, serde_json::Error>(Delegator {
                user: serde_json::from_value(user.clone())?,
                at: serde_json::from_value(at.clone())?,
            })
        })
        .transpose()?;
    let session = sessions
        .into_iter()
        .max_by(created)
        .map(|s| {
            let latest = nodes(&s["activities"]).next();
            Ok::<_, serde_json::Error>(SessionRef {
                id: serde_json::from_value(s["id"].clone())?,
                status: serde_json::from_value(s["status"].clone())?,
                responded_at: latest
                    .filter(|a| a["content"]["__typename"] == "AgentActivityResponseContent")
                    .and_then(|a| a["createdAt"].as_str())
                    .map(str::to_string),
            })
        })
        .transpose()?;
    Ok((delegator, session))
}

fn nodes(value: &Value) -> impl Iterator<Item = &Value> {
    value["nodes"].as_array().into_iter().flatten()
}

fn parse_state(value: &Value) -> Result<WorkflowState, ApiError> {
    Ok(WorkflowState {
        id: text(value, "id")?,
        name: text(value, "name")?,
        r#type: text(value, "type")?,
        position: value["position"].as_f64().unwrap_or(0.0),
    })
}

fn parse_issue(issue: &Value) -> Result<IssueDetail, ApiError> {
    let team = field(issue, "team")?;
    let states = nodes(field(team, "states")?)
        .map(parse_state)
        .collect::<Result<Vec<_>, _>>()?;
    // Linear's bot opens each agent session's thread with a root comment of
    // its own; the app's activities and the session's replies are its children.
    let session_roots: Vec<&str> = nodes(&issue["comments"])
        .filter(|c| {
            c["isArtificialAgentSessionRoot"] == true
                || (c["user"].is_null() && c["parentId"].is_null())
        })
        .filter_map(|c| c["id"].as_str())
        .collect();
    Ok(IssueDetail {
        id: text(issue, "id")?,
        identifier: text(issue, "identifier")?,
        title: text(issue, "title")?,
        url: text(issue, "url")?,
        description: issue["description"].as_str().unwrap_or("").to_string(),
        updated_at: text(issue, "updatedAt")?,
        estimate: issue["estimate"].as_f64(),
        state: parse_state(field(issue, "state")?)?,
        delegate_id: issue["delegate"]["id"].as_str().map(str::to_string),
        team: Team {
            id: text(team, "id")?,
            key: text(team, "key")?,
            name: text(team, "name")?,
            estimation_type: team["issueEstimationType"]
                .as_str()
                .unwrap_or("notUsed")
                .to_string(),
            states,
        },
        labels: nodes(&issue["labels"])
            .map(|l| {
                Ok(Label {
                    name: text(l, "name")?,
                    group: l["parent"]["name"].as_str().map(str::to_string),
                })
            })
            .collect::<Result<_, ApiError>>()?,
        comments: nodes(&issue["comments"])
            .map(|c| {
                let in_session = session_roots.contains(&c["id"].as_str().unwrap_or(""))
                    || c["parentId"]
                        .as_str()
                        .is_some_and(|p| session_roots.contains(&p))
                    || c["user"]["isMe"] == true;
                Ok(Comment {
                    in_session,
                    author: c["user"]["name"]
                        .as_str()
                        .unwrap_or("(unknown)")
                        .to_string(),
                    created_at: text(c, "createdAt")?,
                    body: c["body"].as_str().unwrap_or("").to_string(),
                })
            })
            .collect::<Result<_, ApiError>>()?,
    })
}

#[cfg(test)]
pub mod fake {
    //! An in-memory Linear behind the client's `execute`. It answers the
    //! operations this module sends by name, keeps the issues, sessions and
    //! activities a test sets up, and records every call.

    use super::*;

    pub const APP_USER: &str = "app-user-1";

    /// The rate-limit headers of the response to (operation, variables).
    pub type Headers = Box<dyn Fn(&str, &Value) -> RateHeaders + Send>;

    /// The number of runs in the variables of one `HlaRuns` request.
    pub fn runs(variables: &Value) -> usize {
        variables
            .as_object()
            .map_or(0, |v| v.keys().filter(|k| k.starts_with('i')).count())
    }

    #[derive(Debug, Clone, Default)]
    pub struct FakeSession {
        pub id: String,
        pub issue_id: String,
        /// `pending` when created; tests set other states.
        pub status: String,
        pub created_at: String,
        /// `{ id, name }` of the person who delegated, or null.
        pub creator: Value,
        /// Activity records in the shape the API returns, oldest first.
        pub activities: Vec<Value>,
        pub plan: Option<Value>,
        pub external_urls: Vec<ExternalUrl>,
    }

    impl FakeSession {
        /// The type of every activity the plugin sent, oldest first.
        pub fn sent_types(&self) -> Vec<String> {
            self.activities
                .iter()
                .filter(|a| a["type"] != "prompt")
                .map(|a| a["type"].as_str().unwrap_or("").to_string())
                .collect()
        }

        /// Activities of one type the plugin sent.
        pub fn sent(&self, kind: &str) -> Vec<Value> {
            self.activities
                .iter()
                .filter(|a| a["type"] == kind)
                .cloned()
                .collect()
        }
    }

    #[derive(Default)]
    pub struct FakeLinear {
        /// Issue records in the shape `HlaIssue` returns.
        pub issues: Vec<Value>,
        pub sessions: Vec<FakeSession>,
        /// (operation name, variables, write) of every call.
        pub calls: Vec<(String, Value, bool)>,
        /// The next call fails with this error without taking effect, once.
        pub fail_next: Option<ApiError>,
        /// The next write takes effect but its response is lost, once.
        pub lose_next_response: bool,
        /// Sessions fail as for an app without the agent session webhook category.
        pub sessions_disabled: bool,
        /// The present the fake stamps activities after; the wall clock when
        /// unset, a test clock otherwise.
        pub present: Option<jiff::Timestamp>,
        /// The rate-limit headers of each response; none when unset.
        pub headers: Option<Headers>,
        /// Headers of the responses not taken yet.
        pub received: Vec<RateHeaders>,
        /// Session threads (root, activities, replies) do not show as issue
        /// comments and so do not move `updatedAt`, for tests that count reads.
        pub no_session_comments: bool,
        /// The organization's URL key in issue URLs, `acme` when empty; set,
        /// it also sets the issue ids apart from another fake's.
        pub organization: String,
        clock: i64,
    }

    /// The name the fake gives a user ID.
    pub fn name_of(user: &str) -> String {
        format!("Person {user}")
    }

    fn viewer() -> Value {
        json!({ "id": APP_USER, "app": true, "isMe": true, "name": "Agent" })
    }

    impl FakeLinear {
        /// A timestamp after the present and after every earlier one, so
        /// activities sort after a cursor the plugin took from the clock.
        fn tick_clock(&mut self) -> String {
            self.clock += 1;
            let present = self.present.unwrap_or_else(jiff::Timestamp::now);
            (present + jiff::SignedDuration::from_secs(self.clock)).to_string()
        }

        /// Adds an issue delegated to the app user in `team`.
        pub fn add_issue(&mut self, identifier: &str, team: &str, title: &str) -> String {
            let n = self.issues.len() + 1;
            let (id, organization) = if self.organization.is_empty() {
                (format!("00000000-0000-4000-8000-{n:012}"), "acme")
            } else {
                (
                    format!("{}-{n:012}", self.organization),
                    self.organization.as_str(),
                )
            };
            let url = format!("https://linear.app/{organization}/issue/{identifier}/x");
            self.issues.push(json!({
                "id": id,
                "identifier": identifier,
                "title": title,
                "url": url,
                "description": format!("Description of {identifier}"),
                "updatedAt": "2026-09-25T00:00:00.000Z",
                "estimate": null,
                "state": { "id": "state-todo", "name": "Todo", "type": "unstarted", "position": 1.0 },
                "delegate": { "id": APP_USER },
                "team": {
                    "id": format!("team-{team}"), "key": team, "name": team, "issueEstimationType": "fibonacci",
                    "states": { "nodes": [
                        { "id": "state-todo", "name": "Todo", "type": "unstarted", "position": 1.0 },
                        { "id": "state-review", "name": "In Review", "type": "started", "position": 3.0 },
                        { "id": "state-progress", "name": "In Progress", "type": "started", "position": 2.0 },
                        { "id": "state-done", "name": "Done", "type": "completed", "position": 4.0 },
                        { "id": "state-canceled", "name": "Canceled", "type": "canceled", "position": 5.0 }
                    ] }
                },
                "labels": { "nodes": [] },
                "comments": { "nodes": [] },
                "history": { "nodes": [] }
            }));
            id
        }

        pub fn issue_mut(&mut self, identifier: &str) -> &mut Value {
            self.issues
                .iter_mut()
                .find(|i| i["identifier"] == identifier || i["id"] == identifier)
                .expect("fake issue")
        }

        pub fn issue(&self, identifier: &str) -> &Value {
            self.issues
                .iter()
                .find(|i| i["identifier"] == identifier || i["id"] == identifier)
                .expect("fake issue")
        }

        /// Moves an issue to the team state named `name`.
        pub fn set_state(&mut self, identifier: &str, name: &str) {
            let issue = self.issue_mut(identifier);
            let state = issue["team"]["states"]["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == name)
                .cloned()
                .expect("state");
            issue["state"] = state;
            issue["updatedAt"] = json!("2026-09-26T00:00:00.000Z");
        }

        /// The newest session on an issue.
        pub fn session(&self, identifier: &str) -> &FakeSession {
            let id = self.issue(identifier)["id"].clone();
            self.sessions
                .iter()
                .rev()
                .find(|s| s.issue_id == id)
                .expect("fake session")
        }

        fn new_session(&mut self, issue_id: String) -> String {
            let id = format!("session-{}", self.sessions.len() + 1);
            let created_at = self.tick_clock();
            self.sessions.push(FakeSession {
                id: id.clone(),
                issue_id: issue_id.clone(),
                status: "pending".into(),
                created_at,
                ..FakeSession::default()
            });
            // Linear's bot opens the session's thread on the issue.
            self.comment(
                &issue_id,
                json!({
                    "id": format!("root-{id}"), "parentId": null, "isArtificialAgentSessionRoot": true,
                    "user": null, "body": "This thread is for an agent session with herdr-agent."
                }),
            );
            id
        }

        /// Adds a comment to the issue and moves its `updatedAt`, as Linear
        /// does for every comment, session activities and replies included.
        fn comment(&mut self, issue_id: &str, mut node: Value) {
            let in_thread = node["parentId"].is_string() || node["user"].is_null();
            if self.no_session_comments && in_thread {
                return;
            }
            let at = self.tick_clock();
            node["createdAt"] = json!(at);
            let issue = self
                .issues
                .iter_mut()
                .find(|i| i["id"] == issue_id)
                .expect("fake issue");
            issue["comments"]["nodes"]
                .as_array_mut()
                .expect("comment nodes")
                .push(node);
            issue["updatedAt"] = json!(at);
        }

        /// A person's comment on the issue, outside any session.
        pub fn add_comment(&mut self, identifier: &str, author: &str, body: &str) {
            let issue_id = self.issue(identifier)["id"].as_str().unwrap().to_string();
            let n = self.clock;
            self.comment(
                &issue_id,
                json!({
                    "id": format!("comment-{n}"), "parentId": null, "isArtificialAgentSessionRoot": false,
                    "user": { "name": author, "isMe": false }, "body": body
                }),
            );
        }

        fn session_comment(&mut self, session_id: &str, author: &str, is_me: bool, body: &str) {
            let n = self.clock;
            let issue_id = self
                .sessions
                .iter()
                .find(|s| s.id == session_id)
                .map(|s| s.issue_id.clone())
                .expect("fake session");
            self.comment(
                &issue_id,
                json!({
                    "id": format!("comment-{n}"), "parentId": format!("root-{session_id}"),
                    "isArtificialAgentSessionRoot": false,
                    "user": { "name": author, "isMe": is_me }, "body": body
                }),
            );
        }

        /// The session Linear creates by itself when the issue is delegated,
        /// with no person as its creator.
        pub fn delegate_session(&mut self, identifier: &str) -> String {
            let issue_id = self.issue(identifier)["id"].as_str().unwrap().to_string();
            self.new_session(issue_id)
        }

        /// `user` delegates the issue again: Linear records it in the history
        /// and keeps the session it has.
        pub fn redelegate_by(&mut self, identifier: &str, user: &str) {
            let at = self.tick_clock();
            let issue = self.issue_mut(identifier);
            issue["delegate"] = json!({ "id": APP_USER });
            issue["history"]["nodes"].as_array_mut().unwrap().insert(
                0,
                json!({ "createdAt": at, "actor": { "id": user, "name": name_of(user) }, "toDelegate": { "id": APP_USER } }),
            );
        }

        /// The session Linear opens when `user` delegates the issue.
        pub fn delegate_by(&mut self, identifier: &str, user: &str) -> String {
            let id = self.delegate_session(identifier);
            self.session_mut(&id).unwrap().creator = json!({ "id": user, "name": name_of(user) });
            id
        }

        /// A person's message in the issue's session.
        pub fn add_prompt(
            &mut self,
            identifier: &str,
            user_id: &str,
            body: &str,
            signal: Option<&str>,
        ) {
            let created = self.tick_clock();
            let id = self.issue(identifier)["id"].clone();
            let n = self
                .sessions
                .iter()
                .map(|s| s.activities.len())
                .sum::<usize>();
            let session = self
                .sessions
                .iter_mut()
                .rev()
                .find(|s| s.issue_id == id)
                .expect("fake session");
            session.activities.push(json!({
                "id": format!("prompt-{n}"), "type": "prompt", "createdAt": created, "signal": signal,
                "user": { "id": user_id, "name": name_of(user_id) }, "content": { "type": "prompt", "body": body }
            }));
            let session_id = session.id.clone();
            self.session_comment(&session_id, user_id, false, body);
        }

        pub fn count(&self, operation: &str) -> usize {
            self.calls
                .iter()
                .filter(|(name, _, _)| name == operation)
                .count()
        }

        fn session_mut(&mut self, id: &str) -> Result<&mut FakeSession, ApiError> {
            self.sessions
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or(ApiError::Graphql("Entity not found: AgentSession".into()))
        }

        fn answer(&mut self, operation: &str, variables: &Value) -> Result<Value, ApiError> {
            match operation {
                "HlaViewer" => Ok(json!({ "viewer": viewer() })),
                "HlaDelegatedIssues" => {
                    let teams: Vec<&str> = variables["teamKeys"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    let nodes: Vec<Value> = self
                        .issues
                        .iter()
                        .filter(|i| i["delegate"]["id"] == APP_USER)
                        .filter(|i| teams.contains(&i["team"]["key"].as_str().unwrap_or("")))
                        .filter(|i| !matches!(i["state"]["type"].as_str(), Some("completed" | "canceled")))
                        .map(|i| {
                            json!({
                                "id": i["id"], "identifier": i["identifier"], "title": i["title"], "url": i["url"],
                                "updatedAt": i["updatedAt"], "state": { "type": i["state"]["type"] }, "team": { "key": i["team"]["key"] },
                                "agentSessions": { "nodes": self.sessions.iter().filter(|s| s.issue_id == i["id"]).map(|s| json!({
                                    "id": s.id, "status": s.status, "createdAt": s.created_at, "appUser": { "id": APP_USER }, "creator": s.creator,
                                    "activities": { "nodes": s.activities.last().map(|a| json!({
                                        "createdAt": a["createdAt"],
                                        "content": { "__typename": if a["type"] == "response" { "AgentActivityResponseContent" } else { "AgentActivityThoughtContent" } }
                                    })).into_iter().collect::<Vec<_>>() }
                                })).collect::<Vec<_>>() },
                                "history": i["history"]
                            })
                        })
                        .collect();
                    Ok(
                        json!({ "viewer": viewer(), "issues": { "nodes": nodes, "pageInfo": { "hasNextPage": false, "endCursor": null } } }),
                    )
                }
                "HlaIssue" => {
                    let id = variables["id"].as_str().unwrap_or("");
                    let issue = self
                        .issues
                        .iter()
                        .find(|i| i["id"] == id || i["identifier"] == id)
                        .cloned()
                        .ok_or(ApiError::Graphql("Entity not found".into()))?;
                    Ok(json!({ "viewer": viewer(), "issue": issue }))
                }
                "HlaSessions" | "HlaSessionCreate" if self.sessions_disabled => {
                    Err(ApiError::Graphql(
                        "Agent sessions are not enabled for this application.".into(),
                    ))
                }
                "HlaSessions" => {
                    let nodes: Vec<Value> = self
                        .sessions
                        .iter()
                        .map(|s| json!({ "id": s.id, "status": s.status, "createdAt": s.created_at, "issue": { "id": s.issue_id }, "appUser": { "id": APP_USER } }))
                        .collect();
                    Ok(json!({ "viewer": viewer(), "agentSessions": { "nodes": nodes } }))
                }
                "HlaSessionCreate" => {
                    let issue_id = self.issue(variables["issueId"].as_str().unwrap_or(""))["id"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    let id = self.new_session(issue_id);
                    Ok(
                        json!({ "agentSessionCreateOnIssue": { "success": true, "agentSession": { "id": id } } }),
                    )
                }
                "HlaActivityCreate" => {
                    let input = &variables["input"];
                    let created = self.tick_clock();
                    let session =
                        self.session_mut(input["agentSessionId"].as_str().unwrap_or(""))?;
                    if session.activities.iter().any(|a| a["id"] == input["id"]) {
                        return Err(ApiError::Graphql("duplicate activity id".into()));
                    }
                    let mut activity = json!({
                        "id": input["id"], "type": input["content"]["type"], "content": input["content"], "createdAt": created,
                        "ephemeral": input["ephemeral"], "signal": input["signal"], "signalMetadata": input["signalMetadata"], "user": { "id": APP_USER }
                    });
                    if activity["signal"].is_null() {
                        activity["signal"] = Value::Null;
                    }
                    // A response ends the session, as in Linear.
                    if activity["type"] == "response" {
                        session.status = "complete".into();
                    }
                    session.activities.push(activity);
                    if input["ephemeral"] != true {
                        let body = input["content"]["body"].as_str().unwrap_or("").to_string();
                        let session_id = input["agentSessionId"].as_str().unwrap_or("").to_string();
                        self.session_comment(&session_id, "herdr-agent", true, &body);
                    }
                    Ok(
                        json!({ "agentActivityCreate": { "success": true, "agentActivity": { "id": input["id"] } } }),
                    )
                }
                "HlaActivityFind" => {
                    let id = variables["id"].clone();
                    let session =
                        self.session_mut(variables["sessionId"].as_str().unwrap_or(""))?;
                    let found: Vec<Value> = session
                        .activities
                        .iter()
                        .filter(|a| a["id"] == id)
                        .map(|a| json!({ "id": a["id"] }))
                        .collect();
                    Ok(
                        json!({ "viewer": viewer(), "agentSession": { "activities": { "nodes": found } } }),
                    )
                }
                "HlaSessionUpdate" => {
                    let input = variables["input"].clone();
                    let session = self.session_mut(variables["id"].as_str().unwrap_or(""))?;
                    if let Some(plan) = input.get("plan") {
                        session.plan = Some(plan.clone());
                    }
                    if let Some(urls) = input.get("externalUrls") {
                        session.external_urls = serde_json::from_value(urls.clone()).unwrap();
                    }
                    Ok(json!({ "agentSessionUpdate": { "success": true } }))
                }
                "HlaIssueState" => {
                    let state_id = variables["stateId"].clone();
                    let issue = self.issue_mut(variables["id"].as_str().unwrap_or(""));
                    let state = issue["team"]["states"]["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|s| s["id"] == state_id)
                        .cloned()
                        .ok_or(ApiError::Graphql("bad state".into()))?;
                    issue["state"] = state;
                    Ok(json!({ "issueUpdate": { "success": true } }))
                }
                "HlaRuns" => {
                    let mut data = json!({ "viewer": viewer() });
                    let mut n = 0;
                    while let Some(issue_id) =
                        variables.get(format!("i{n}")).and_then(Value::as_str)
                    {
                        let issue = self.issue(issue_id).clone();
                        data[format!("i{n}")] = json!({ "updatedAt": issue["updatedAt"], "state": { "type": issue["state"]["type"], "name": issue["state"]["name"] }, "delegate": issue["delegate"] });
                        let cursor: jiff::Timestamp = variables[format!("c{n}")]
                            .as_str()
                            .unwrap()
                            .parse()
                            .unwrap();
                        let session =
                            self.session_mut(variables[format!("s{n}")].as_str().unwrap())?;
                        let prompts: Vec<Value> = session
                            .activities
                            .iter()
                            .filter(|a| a["type"] == "prompt")
                            .filter(|a| {
                                a["createdAt"]
                                    .as_str()
                                    .unwrap()
                                    .parse::<jiff::Timestamp>()
                                    .unwrap()
                                    > cursor
                            })
                            .cloned()
                            .collect();
                        data[format!("s{n}")] = json!({ "activities": { "nodes": prompts } });
                        n += 1;
                    }
                    Ok(data)
                }
                other => panic!("FakeLinear: unexpected operation {other}"),
            }
        }
    }

    impl FakeLinear {
        pub fn execute(
            &mut self,
            operation: &str,
            _query: &str,
            variables: Value,
            write: bool,
        ) -> Result<Value, ApiError> {
            self.calls
                .push((operation.to_string(), variables.clone(), write));
            if let Some(headers) = &self.headers {
                self.received.push(headers(operation, &variables));
            }
            if let Some(error) = self.fail_next.take() {
                return Err(error);
            }
            let answer = self.answer(operation, &variables);
            if write && std::mem::take(&mut self.lose_next_response) {
                return Err(ApiError::RequestFailed);
            }
            answer
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::fake::FakeLinear;
    use super::*;

    #[tokio::test]
    async fn reads_parse_into_typed_records() {
        let mut fake = FakeLinear::default();
        let id = fake.add_issue("DATA-1", "DATA", "First");
        fake.add_issue("OTHER-1", "OTHER", "Elsewhere");
        fake.add_issue("DATA-2", "DATA", "Done already");
        fake.issue_mut("DATA-2")["state"]["type"] = json!("completed");
        fake.issue_mut("DATA-1")["labels"] = json!({ "nodes": [{ "name": "S", "parent": { "name": "size" } }, { "name": "bug", "parent": null }] });
        let linear = Mutex::new(fake);

        assert_eq!(linear.viewer().await.unwrap().id, fake::APP_USER);
        let issues = linear.delegated_issues(&["DATA".into()]).await.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(
            (
                issues[0].identifier.as_str(),
                issues[0].team.as_str(),
                issues[0].state.as_str()
            ),
            ("DATA-1", "DATA", "unstarted")
        );

        let detail = linear.issue(&id).await.unwrap();
        assert_eq!(detail.team.states.len(), 5);
        assert_eq!(
            detail.labels[0],
            Label {
                name: "S".into(),
                group: Some("size".into())
            }
        );
        assert_eq!(detail.labels[1].group, None);
        assert_eq!(detail.delegate_id.as_deref(), Some(fake::APP_USER));
        assert!(
            linear
                .lock()
                .unwrap()
                .calls
                .iter()
                .all(|(_, _, write)| !write)
        );
    }

    #[tokio::test]
    async fn writes_and_run_updates_round_trip() {
        let mut fake = FakeLinear::default();
        let issue = fake.add_issue("DATA-1", "DATA", "First");
        let linear = Mutex::new(fake);
        assert_eq!(linear.find_session(&issue).await.unwrap(), None);
        let session = linear.create_session(&issue).await.unwrap();
        assert_eq!(
            linear.find_session(&issue).await.unwrap().as_deref(),
            Some(session.as_str())
        );
        let thought = Activity::new(Content::Thought {
            body: "Picked up".into(),
        });
        linear
            .create_activity(&session, "a-1", &thought)
            .await
            .unwrap();
        assert!(linear.activity_exists(&session, "a-1").await.unwrap());
        assert!(!linear.activity_exists(&session, "a-2").await.unwrap());
        let ask = Activity {
            signal: Some("select".into()),
            signal_metadata: Some(json!({ "options": [{ "label": "Yes", "value": "yes" }] })),
            ..Activity::new(Content::Elicitation {
                body: "Proceed?".into(),
            })
        };
        linear.create_activity(&session, "a-2", &ask).await.unwrap();
        linear
            .set_plan(
                &session,
                &json!([{ "content": "Plan", "status": "pending" }]),
            )
            .await
            .unwrap();
        linear
            .set_external_urls(
                &session,
                &[ExternalUrl {
                    label: "PR".into(),
                    url: "https://github.com/o/r/pull/1".into(),
                }],
            )
            .await
            .unwrap();
        linear
            .set_issue_state(&issue, "state-progress")
            .await
            .unwrap();

        {
            let mut fake = linear.lock().unwrap();
            let stored = &fake.sessions[0];
            assert_eq!(stored.sent_types(), ["thought", "elicitation"]);
            assert_eq!(
                stored.sent("elicitation")[0]["signalMetadata"]["options"][0]["value"],
                "yes"
            );
            assert_eq!(stored.plan.as_ref().unwrap()[0]["status"], "pending");
            assert_eq!(fake.issue("DATA-1")["state"]["name"], "In Progress");
            fake.add_prompt("DATA-1", "user-1", "first", None);
            fake.add_prompt("DATA-1", "user-2", "stop now", Some("stop"));
        }

        let query = RunQuery {
            issue_id: issue.clone(),
            session_id: session.clone(),
            cursor: "2026-09-24T00:00:00Z".into(),
        };
        let updates = linear.run_updates(&[query.clone(), query]).await.unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].issue.state_type, "started");
        assert_eq!(updates[0].prompts.len(), 2);
        assert_eq!(updates[0].prompts[1].signal.as_deref(), Some("stop"));
        assert_eq!(updates[0].prompts[0].user_id, "user-1");
        let later = RunQuery {
            issue_id: issue,
            session_id: session,
            cursor: updates[0].prompts[1].created_at.clone(),
        };
        assert!(
            linear.run_updates(&[later]).await.unwrap()[0]
                .prompts
                .is_empty()
        );
        assert!(linear.run_updates(&[]).await.unwrap().is_empty());
    }

    #[test]
    fn a_missing_field_is_an_error() {
        assert_eq!(
            parse_issue(&json!({ "id": "x" })).unwrap_err(),
            ApiError::ReadFieldsInvalid
        );
    }
}
