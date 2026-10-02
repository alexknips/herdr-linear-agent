//! A run's outbox: every Linear write the run needs, as one JSON file per
//! request under `.state/outbox/`, sent by the ticker in the order written.
//!
//! Agents never talk to Linear. `say`, `ask`, `plan set` and `finish` write a
//! request here and exit without touching the Keychain. The ticker sends each
//! request once; when the outcome is unknown it reads Linear before sending
//! again, and it never resends a write unconditionally.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::files;
use crate::linear::ApiError;
use crate::linear::api::{Activity, Content, ExternalUrl, IssueDetail};
use crate::linear::client::LinearApi;
use crate::run::{AwaitingReply, Run, RunLock, WaitReason};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateTarget {
    /// The team's first started state, unless the issue is already started,
    /// completed or canceled.
    Started,
    /// The configured review state, unless the issue is already there,
    /// completed or canceled.
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Activity {
        activity: Activity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_reason: Option<WaitReason>,
    },
    Plan {
        plan: Value,
    },
    ExternalUrls {
        urls: Vec<ExternalUrl>,
    },
    IssueState {
        target: StateTarget,
    },
    /// A comment on the issue, created with the request's id.
    Comment {
        body: String,
    },
    /// Labels added to the issue, `Label` or `Group/Label`.
    Labels {
        names: Vec<String>,
    },
}

impl Op {
    /// A question; with options, Linear shows them as a select.
    pub fn elicitation(body: impl Into<String>, options: &[(&str, &str)]) -> Op {
        Self::awaiting_reply(body, options, WaitReason::CoordinatorQuestion)
    }

    pub fn awaiting_reply(
        body: impl Into<String>,
        options: &[(&str, &str)],
        reason: WaitReason,
    ) -> Op {
        let mut activity = Activity::new(Content::Elicitation { body: body.into() });
        if !options.is_empty() {
            activity.signal = Some("select".into());
            let options: Vec<_> = options
                .iter()
                .map(|(label, value)| json!({ "label": label, "value": value }))
                .collect();
            activity.signal_metadata = Some(json!({ "options": options }));
        }
        Op::Activity {
            activity,
            wait_reason: Some(reason),
        }
    }

    pub fn activity(activity: Activity) -> Op {
        Op::Activity {
            activity,
            wait_reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    /// A UUIDv4; an activity is created with it as its ID.
    pub id: String,
    pub created: String,
    /// A send was started; its outcome is checked by a read before any resend.
    #[serde(default)]
    pub attempted: bool,
    /// Accepted-prompt generation when the question was queued. This uses a
    /// local monotonic token, not a comparison between server and local time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_generation: Option<u64>,
    #[serde(flatten)]
    pub op: Op,
}

fn outbox_dir(run: &Run) -> PathBuf {
    run.state_dir().join("outbox")
}

/// Queues one request. File names carry a counter allocated under the run
/// lock, so requests are sent in the order they were written.
pub fn push(run: &Run, op: Op) -> Result<Option<String>> {
    let lock = run.lock()?;
    push_held(run, &lock, op)
}

/// `push` for a caller that holds the run lock, so a request is queued in
/// the same critical section as the record field that guards it.
pub fn push_held(run: &Run, lock: &RunLock, op: Op) -> Result<Option<String>> {
    let mut record = run.record()?;
    let wait_reason = match &op {
        Op::Activity {
            activity,
            wait_reason,
        } => {
            let terminal = matches!(activity.content, Content::Response { .. });
            if !terminal
                && (record.status != crate::run::Status::Active
                    || record.stopped
                    || record.finished
                    || record.awaiting_reply.is_some())
            {
                if wait_reason.is_some() {
                    return Ok(None);
                }
                return Ok(None);
            }
            *wait_reason
        }
        _ => None,
    };
    let counter_path = run.state_dir().join("outbox-counter.json");
    let n: u64 = files::read_json::<u64>(&counter_path).unwrap_or(0) + 1;
    files::write_json(&counter_path, &n)?;
    let request = Request {
        id: uuid::Uuid::new_v4().to_string(),
        created: files::now(),
        attempted: false,
        wait_generation: wait_reason.map(|_| record.reply_generation),
        op,
    };
    files::write_json(&outbox_dir(run).join(format!("{n:010}.json")), &request)?;
    if let Some(reason) = wait_reason {
        record.awaiting_reply = Some(AwaitingReply {
            activity_id: request.id.clone(),
            asked_at: request.created.clone(),
            reason,
        });
        record.timeout_asked = false;
        run.update_held(lock, |current| *current = record)?;
    }
    Ok(Some(request.id))
}

fn queued_paths(run: &Run) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(outbox_dir(run)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    paths
}

/// Whether any request file waits, parsed or not. Read-only: the Linear
/// task alone moves, rewrites or removes outbox files.
pub fn is_empty(run: &Run) -> bool {
    queued_paths(run).is_empty()
}

/// The requests that parse, oldest first, read-only like [`is_empty`].
pub fn queued(run: &Run) -> Vec<Request> {
    queued_paths(run)
        .iter()
        .filter_map(|path| files::read_json::<Request>(path))
        .collect()
}

/// Queued requests, oldest first. A file that does not parse is moved
/// aside. Only the Linear task calls it.
pub fn pending(run: &Run) -> Vec<(PathBuf, Request)> {
    queued_paths(run)
        .into_iter()
        .filter_map(|path| match files::read_json::<Request>(&path) {
            Some(request) => Some((path, request)),
            None => {
                set_aside(&path);
                None
            }
        })
        .collect()
}

fn set_aside(path: &Path) {
    let failed = path.parent().map(|p| p.join("failed")).unwrap_or_default();
    let _ = std::fs::create_dir_all(&failed);
    if let Some(name) = path.file_name() {
        let _ = std::fs::rename(path, failed.join(name));
    }
}

/// Linear refused the request itself; sending it again would fail the same way.
fn definitive(error: &ApiError) -> bool {
    matches!(error, ApiError::Graphql(_) | ApiError::Configuration | ApiError::HttpStatus(400..=428 | 430..=499))
}

pub struct Sent {
    pub count: usize,
    /// Requests Linear refused, moved to `outbox/failed/`: (request, error).
    pub refused: Vec<(Request, ApiError)>,
    /// The error that stopped the queue; the rest waits for the next tick.
    pub blocked: Option<ApiError>,
    /// An activity went out.
    pub activity_sent: bool,
}

/// Sends a run's queued requests in order until one cannot be sent. An
/// attempted activity is looked up before it is sent again; a refusal is set
/// aside; `RateLimited` is not definitive, so such a request stays queued.
pub async fn send(
    run: &Run,
    session_id: &str,
    issue_id: &str,
    review_state: &str,
    linear: &impl LinearApi,
) -> Sent {
    let mut sent = Sent {
        count: 0,
        refused: Vec::new(),
        blocked: None,
        activity_sent: false,
    };
    for (path, mut request) in pending(run) {
        let outcome = send_one(
            &path,
            &mut request,
            run,
            session_id,
            issue_id,
            review_state,
            linear,
        )
        .await;
        match outcome {
            Ok(activity_confirmed) => {
                let _ = std::fs::remove_file(&path);
                sent.count += 1;
                sent.activity_sent |=
                    activity_confirmed && matches!(request.op, Op::Activity { .. });
            }
            Err(error) if definitive(&error) => {
                let is_wait = matches!(
                    request.op,
                    Op::Activity {
                        wait_reason: Some(_),
                        ..
                    }
                );
                let refused_wait = if is_wait {
                    match clear_refused_wait(run, &request.id) {
                        Ok(cleared) => cleared,
                        Err(failure) => {
                            sent.blocked = Some(failure);
                            break;
                        }
                    }
                } else {
                    false
                };
                set_aside(&path);
                if refused_wait {
                    let _ = crate::inbox::write(
                        run,
                        "linear",
                        "question",
                        "Linear refused a question from this run. The coordinator may continue; see the ticker log for the error.",
                    );
                }
                sent.refused.push((request, error));
            }
            Err(error) => {
                sent.blocked = Some(error);
                break;
            }
        }
    }
    sent
}

async fn send_one(
    path: &Path,
    request: &mut Request,
    run: &Run,
    session_id: &str,
    issue_id: &str,
    review_state: &str,
    linear: &impl LinearApi,
) -> Result<bool, ApiError> {
    let is_wait = matches!(
        &request.op,
        Op::Activity {
            wait_reason: Some(_),
            ..
        }
    );
    let checked = match &request.op {
        Op::Activity { .. } if request.attempted => {
            linear.activity_exists(session_id, &request.id).await?
        }
        Op::Comment { .. } if request.attempted => linear.comment_exists(&request.id).await?,
        // Replacing the plan or the URL list and adding labels are
        // idempotent, and a state move reads the issue before it writes.
        _ => false,
    };
    if checked {
        return Ok(true);
    }
    if is_wait {
        if !mark_wait_attempt_if_current(path, request, run)? {
            return Ok(false);
        }
    } else {
        request.attempted = true;
        files::write_json(path, request).map_err(|_| ApiError::Configuration)?;
    }
    let outcome = match &request.op {
        Op::Activity { activity, .. } => {
            linear
                .create_activity(session_id, &request.id, activity)
                .await
        }
        Op::Plan { plan } => linear.set_plan(session_id, plan).await,
        Op::ExternalUrls { urls } => linear.set_external_urls(session_id, urls).await,
        Op::Comment { body } => linear.create_comment(issue_id, &request.id, body).await,
        Op::Labels { names } => linear.add_labels(issue_id, names).await,
        Op::IssueState { target } => {
            let issue = linear.issue(issue_id).await?;
            let Some(state_id) = target_state(&issue, *target, review_state)? else {
                return Ok(true);
            };
            linear.set_issue_state(issue_id, &state_id).await?;
            // The write is confirmed by reading the issue again.
            if linear.issue(issue_id).await?.state.id != state_id {
                return Err(ApiError::RequestFailed);
            }
            Ok(())
        }
    };
    outcome?;
    Ok(true)
}

fn mark_wait_attempt_if_current(
    path: &Path,
    request: &mut Request,
    run: &Run,
) -> Result<bool, ApiError> {
    let _lock = run.lock().map_err(|_| ApiError::Configuration)?;
    let record = run.record().map_err(|_| ApiError::Configuration)?;
    if record.status != crate::run::Status::Active
        || record.stopped
        || record.finished
        || !record
            .awaiting_reply
            .is_some_and(|wait| wait.activity_id == request.id)
    {
        return Ok(false);
    }
    request.attempted = true;
    files::write_json(path, request).map_err(|_| ApiError::Configuration)?;
    Ok(true)
}

fn clear_refused_wait(run: &Run, activity_id: &str) -> Result<bool, ApiError> {
    let lock = run.lock().map_err(|_| ApiError::Configuration)?;
    let record = run.record().map_err(|_| ApiError::Configuration)?;
    if !record
        .awaiting_reply
        .is_some_and(|wait| wait.activity_id == activity_id)
    {
        return Ok(false);
    }
    run.update_held(&lock, |record| {
        if let Some(wait) = record.awaiting_reply.take() {
            record.cleared_wait_id = wait.activity_id;
        }
    })
    .map_err(|_| ApiError::Configuration)?;
    Ok(true)
}

/// The state to move the issue to, or `None` when it should stay where it is.
pub fn target_state(
    issue: &IssueDetail,
    target: StateTarget,
    review_state: &str,
) -> Result<Option<String>, ApiError> {
    let current = &issue.state;
    match target {
        StateTarget::Started => {
            if matches!(
                current.r#type.as_str(),
                "started" | "completed" | "canceled"
            ) {
                return Ok(None);
            }
            let first = issue
                .team
                .states
                .iter()
                .filter(|s| s.r#type == "started")
                .min_by(|a, b| a.position.total_cmp(&b.position));
            first.map(|s| Some(s.id.clone())).ok_or_else(|| {
                ApiError::Graphql(format!("team {} has no started state", issue.team.key))
            })
        }
        StateTarget::Review => {
            if current.name.eq_ignore_ascii_case(review_state)
                || matches!(current.r#type.as_str(), "completed" | "canceled")
            {
                return Ok(None);
            }
            let review = issue
                .team
                .states
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(review_state));
            review.map(|s| Some(s.id.clone())).ok_or_else(|| {
                ApiError::Graphql(format!(
                    "team {} has no state named `{review_state}`",
                    issue.team.key
                ))
            })
        }
    }
}

/// Parses the coordinator's Markdown checklist into Linear's plan: a list of
/// `{content, status}`.
pub fn parse_plan(text: &str) -> Result<Value> {
    let mut steps = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some(rest) = line
            .strip_prefix("- [")
            .or_else(|| line.strip_prefix("* ["))
        else {
            bail!("plan lines look like `- [ ] step`; got `{line}`");
        };
        let (mark, content) = rest
            .split_once(']')
            .filter(|(mark, _)| mark.chars().count() == 1)
            .ok_or_else(|| anyhow::anyhow!("plan lines look like `- [ ] step`; got `{line}`"))?;
        let status = match mark {
            " " => "pending",
            ">" => "inProgress",
            "x" | "X" => "completed",
            "-" => "canceled",
            other => bail!("`[{other}]` is not a plan mark; use [ ], [>], [x] or [-]"),
        };
        let content = content.trim();
        if content.is_empty() {
            bail!("a plan step is empty: `{line}`");
        }
        steps.push(serde_json::json!({ "content": content, "status": status }));
    }
    if steps.is_empty() {
        bail!("the plan has no steps");
    }
    Ok(Value::Array(steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::api::Content;
    use std::sync::Mutex;

    use crate::linear::api::fake::FakeLinear;
    use crate::run::RunRecord;

    fn thought(body: &str) -> Op {
        Op::activity(Activity::new(Content::Thought { body: body.into() }))
    }

    fn setup() -> (tempfile::TempDir, Run, Mutex<FakeLinear>, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let run = Run::create(
            dir.path(),
            RunRecord {
                workspace: "acme".into(),
                identifier: "DATA-1".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        let mut fake = FakeLinear::default();
        let issue = fake.add_issue("DATA-1", "DATA", "First");
        let session = fake.delegate_session("DATA-1");
        (dir, run, Mutex::new(fake), session, issue)
    }

    #[tokio::test]
    async fn requests_go_out_in_order_and_are_removed() {
        let (_dir, run, linear, session, issue) = setup();
        push(&run, thought("one")).unwrap();
        push(
            &run,
            Op::IssueState {
                target: StateTarget::Started,
            },
        )
        .unwrap();
        push(&run, thought("two")).unwrap();
        push(
            &run,
            Op::Plan {
                plan: parse_plan("- [x] a\n- [ ] b").unwrap(),
            },
        )
        .unwrap();
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.count, 4);
        assert!(sent.activity_sent && sent.blocked.is_none());
        assert!(pending(&run).is_empty());
        let fake = linear.lock().unwrap();
        let bodies: Vec<Value> = fake.sessions[0]
            .sent("thought")
            .iter()
            .map(|a| a["content"]["body"].clone())
            .collect();
        assert_eq!(bodies, ["one", "two"]);
        assert_eq!(
            fake.issue("DATA-1")["state"]["name"],
            "In Progress",
            "the started state with the lowest position"
        );
        assert_eq!(
            fake.sessions[0].plan.as_ref().unwrap()[1]["status"],
            "pending"
        );
    }

    #[tokio::test]
    async fn a_lost_response_is_checked_by_a_read_and_never_sent_twice() {
        let (_dir, run, linear, session, issue) = setup();
        push(&run, thought("once")).unwrap();
        linear.lock().unwrap().lose_next_response = true;
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        assert!(pending(&run)[0].1.attempted);
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.count, 1);
        let fake = linear.lock().unwrap();
        assert_eq!(fake.sessions[0].sent("thought").len(), 1);
        assert_eq!(fake.count("HerdrLinearAgentActivityFind"), 1);
    }

    #[tokio::test]
    async fn a_cleared_wait_with_an_unknown_write_is_read_back_but_never_resent() {
        let (_dir, run, linear, session, issue) = setup();
        let id = push(&run, Op::elicitation("Proceed?", &[]))
            .unwrap()
            .unwrap();
        linear.lock().unwrap().lose_next_response = true;
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        run.update(|r| r.awaiting_reply = None).unwrap();

        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.count, 1, "the existing Linear activity is reconciled");
        assert!(pending(&run).is_empty());
        let fake = linear.lock().unwrap();
        assert_eq!(fake.sessions[0].sent("elicitation").len(), 1);
        assert_eq!(fake.count("HerdrLinearAgentActivityFind"), 1);
        assert_ne!(id, "");
    }

    #[test]
    fn a_queued_question_repairs_a_crash_but_not_a_question_cleared_by_a_reply() {
        let (_dir, run, _linear, _session, _issue) = setup();
        let id = push(&run, Op::elicitation("Proceed?", &[]))
            .unwrap()
            .unwrap();
        run.update(|r| r.awaiting_reply = None).unwrap();
        assert_eq!(
            run.record().unwrap().awaiting_reply.unwrap().activity_id,
            id
        );

        run.update(|r| {
            r.awaiting_reply = None;
            r.cleared_wait_id = id.clone();
            r.reply_generation += 1;
        })
        .unwrap();
        let runs_dir = run.dir.parent().unwrap().parent().unwrap();
        let restarted = Run::load(runs_dir, &run.key).unwrap();
        assert!(restarted.record().unwrap().awaiting_reply.is_none());
    }

    #[tokio::test]
    async fn a_definitive_question_refusal_clears_only_its_matching_wait() {
        let (_dir, run, linear, session, issue) = setup();
        let id = push(&run, Op::elicitation("Proceed?", &[]))
            .unwrap()
            .unwrap();
        linear.lock().unwrap().fail_next = Some(ApiError::Graphql("invalid activity".into()));
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.refused.len(), 1);
        assert!(run.record().unwrap().awaiting_reply.is_none());
        assert!(
            crate::inbox::unhandled(&run)
                .iter()
                .any(|item| item.summary.contains("refused a question"))
        );

        let next = push(&run, Op::elicitation("Another question?", &[]))
            .unwrap()
            .unwrap();
        assert_ne!(id, next);
        assert_eq!(
            run.record().unwrap().awaiting_reply.unwrap().activity_id,
            next
        );
    }

    #[tokio::test]
    async fn unknown_question_outcomes_keep_the_wait_and_rate_limited_questions_stay_queued() {
        let (_dir, run, linear, session, issue) = setup();
        push(&run, Op::elicitation("Proceed?", &[])).unwrap();
        linear.lock().unwrap().fail_next = Some(ApiError::RateLimited);
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RateLimited));
        assert!(run.record().unwrap().awaiting_reply.is_some());
        assert_eq!(pending(&run).len(), 1);

        linear.lock().unwrap().fail_next = Some(ApiError::RequestFailed);
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        assert!(run.record().unwrap().awaiting_reply.is_some());
        assert_eq!(pending(&run).len(), 1);
    }

    #[test]
    fn stopped_and_inactive_runs_hold_progress_but_still_allow_terminal_responses() {
        let (_dir, run, _linear, _session, _issue) = setup();
        run.update(|r| r.status = crate::run::Status::Detached)
            .unwrap();
        assert!(push(&run, thought("progress")).unwrap().is_none());
        assert!(
            push(
                &run,
                Op::activity(Activity::new(Content::Response {
                    body: "Stopped.".into(),
                }))
            )
            .unwrap()
            .is_some()
        );
        assert_eq!(pending(&run).len(), 1);
    }

    #[tokio::test]
    async fn a_failure_keeps_the_rest_in_order_and_a_refusal_is_set_aside() {
        let (_dir, run, linear, session, issue) = setup();
        push(&run, thought("a")).unwrap();
        push(&run, thought("b")).unwrap();
        linear.lock().unwrap().fail_next = Some(ApiError::HttpStatus(503));
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!((sent.count, pending(&run).len()), (0, 2));
        // An attempted request whose send never reached Linear is read, then sent.
        let sent = send(&run, &session, &issue, "In Review", &linear).await;
        assert_eq!(sent.count, 2);

        push(
            &run,
            Op::IssueState {
                target: StateTarget::Review,
            },
        )
        .unwrap();
        push(&run, thought("after")).unwrap();
        let sent = send(&run, &session, &issue, "Missing State", &linear).await;
        assert_eq!(sent.refused.len(), 1);
        assert_eq!(sent.count, 1);
        assert!(
            run.state_dir()
                .join("outbox/failed")
                .read_dir()
                .unwrap()
                .count()
                == 1
        );
    }

    #[tokio::test]
    async fn state_targets_leave_later_states_alone() {
        let (_dir, _run, linear, _session, issue) = setup();
        let mut detail = linear.issue(&issue).await.unwrap();
        assert_eq!(
            target_state(&detail, StateTarget::Started, "In Review")
                .unwrap()
                .as_deref(),
            Some("state-progress")
        );
        assert_eq!(
            target_state(&detail, StateTarget::Review, "in review")
                .unwrap()
                .as_deref(),
            Some("state-review")
        );
        detail.state = detail
            .team
            .states
            .iter()
            .find(|s| s.name == "In Review")
            .cloned()
            .unwrap();
        assert_eq!(
            target_state(&detail, StateTarget::Started, "In Review").unwrap(),
            None
        );
        assert_eq!(
            target_state(&detail, StateTarget::Review, "In Review").unwrap(),
            None
        );
        detail.state = detail
            .team
            .states
            .iter()
            .find(|s| s.name == "Done")
            .cloned()
            .unwrap();
        assert_eq!(
            target_state(&detail, StateTarget::Review, "In Review").unwrap(),
            None
        );
    }

    #[test]
    fn plans_parse_from_a_checklist() {
        let plan = parse_plan(
            "- [ ] Read the code\n- [>] Start workers\n* [x] Done thing\n- [-] Dropped\n",
        )
        .unwrap();
        let statuses: Vec<&str> = plan
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["pending", "inProgress", "completed", "canceled"]);
        assert_eq!(plan[0]["content"], "Read the code");
        for bad in ["", "Read the code", "- [?] x", "- [ ]   ", "- [xx] y"] {
            assert!(parse_plan(bad).is_err(), "{bad}");
        }
    }
}
