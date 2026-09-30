//! The terminal screens of `history`: the run list with its preview, then a
//! run's agents with the selected transcript.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListState, Paragraph, Wrap};

use super::transcript::{self, Roots, Session, Sessions};
use super::{Entry, entries, filter, minute};
use crate::herdr::{Client, Herdr};
use crate::paths::Env;
use crate::run::AgentStatus;

const PAGE: u16 = 10;

/// An agent's transcript, or why there is none.
struct Row {
    agent: String,
    kind: String,
    cwd: String,
    name: String,
    open: bool,
    found: Result<Session, String>,
}

impl Row {
    fn line(&self) -> String {
        match &self.found {
            Ok(s) => format!(
                "{}  {}  {}  {}",
                self.agent,
                self.kind,
                minute(s.modified),
                s.id.chars().take(8).collect::<String>()
            ),
            Err(_) => format!("{}  {}  no transcript", self.agent, self.kind),
        }
    }

    /// The agent's session to resume in a new workspace: not while it is
    /// open, and only in a folder that is still there.
    fn resume(&self) -> Option<Resume> {
        let session = self.found.as_ref().ok()?;
        let args = crate::agents::resume_args(&self.kind, &session.id)?;
        (!self.open && Path::new(&self.cwd).is_dir()).then(|| Resume {
            kind: self.kind.clone(),
            cwd: self.cwd.clone(),
            name: format!("{}-resumed", self.name),
            args,
        })
    }
}

/// What resuming an agent starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Resume {
    kind: String,
    cwd: String,
    name: String,
    args: Vec<String>,
}

enum Screen {
    List,
    Agents { rows: Vec<Row>, selected: usize },
}

#[derive(Debug, PartialEq)]
enum Effect {
    None,
    Quit,
    Resume(Resume),
}

struct App {
    entries: Vec<Entry>,
    roots: Roots,
    query: String,
    shown: Vec<usize>,
    selected: usize,
    scroll: u16,
    screen: Screen,
    /// The text on the right, for the entry and row it was made for.
    text: Option<((Option<usize>, usize), String)>,
    status: String,
}

fn rows(entry: &Entry, roots: &Roots) -> Vec<Row> {
    let mut rows = Vec::new();
    for agent in entry.agents() {
        let record = agent.record;
        let row = |found| Row {
            agent: agent.label.clone(),
            kind: record.kind.clone(),
            cwd: record.cwd.clone(),
            name: record.agent_name.clone(),
            open: record.status == AgentStatus::Open,
            found,
        };
        match transcript::sessions(roots, &record.kind, &record.cwd) {
            Sessions::Found { sessions, searched } if sessions.is_empty() => rows.push(row(Err(
                format!("No transcript is left in {}.", searched.display()),
            ))),
            Sessions::Found { sessions, .. } => {
                rows.extend(sessions.into_iter().map(|s| row(Ok(s))))
            }
            Sessions::Unreadable(why) => {
                rows.push(row(Err(format!("Cannot read this transcript: {why}."))))
            }
        }
    }
    rows
}

impl App {
    fn new(entries: Vec<Entry>, roots: Roots) -> App {
        let shown = filter(&entries, "");
        App {
            entries,
            roots,
            query: String::new(),
            shown,
            selected: 0,
            scroll: 0,
            screen: Screen::List,
            text: None,
            status: String::new(),
        }
    }

    fn entry(&self) -> Option<&Entry> {
        self.shown.get(self.selected).map(|&i| &self.entries[i])
    }

    fn select(&mut self, index: usize) {
        self.selected = index;
        self.scroll = 0;
    }

    fn key(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Effect::Quit;
        }
        match (&key.code, ctrl) {
            (KeyCode::PageDown, _) | (KeyCode::Char('d'), true) => {
                self.scroll = self.scroll.saturating_add(PAGE);
                return Effect::None;
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('u'), true) => {
                self.scroll = self.scroll.saturating_sub(PAGE);
                return Effect::None;
            }
            _ => {}
        }
        self.status.clear();
        match &mut self.screen {
            Screen::List => match (key.code, ctrl) {
                (KeyCode::Esc, _) => return Effect::Quit,
                (KeyCode::Up, _) | (KeyCode::Char('p'), true) => {
                    self.select(self.selected.saturating_sub(1));
                }
                (KeyCode::Down, _) | (KeyCode::Char('n'), true) => {
                    let last = self.shown.len().saturating_sub(1);
                    self.select((self.selected + 1).min(last));
                }
                (KeyCode::Enter, _) => {
                    if let Some(entry) = self.entry() {
                        let rows = rows(entry, &self.roots);
                        self.screen = Screen::Agents { rows, selected: 0 };
                        self.scroll = 0;
                    }
                }
                (KeyCode::Backspace, _) => {
                    self.query.pop();
                    self.refilter();
                }
                (KeyCode::Char(c), false) => {
                    self.query.push(c);
                    self.refilter();
                }
                _ => {}
            },
            Screen::Agents { rows, selected } => match key.code {
                KeyCode::Char('q') => return Effect::Quit,
                KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                    self.screen = Screen::List;
                    self.scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    self.scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(rows.len().saturating_sub(1));
                    self.scroll = 0;
                }
                KeyCode::Char('r') => {
                    if let Some(resume) = rows.get(*selected).and_then(Row::resume) {
                        return Effect::Resume(resume);
                    }
                }
                _ => {}
            },
        }
        Effect::None
    }

    fn refilter(&mut self) {
        self.shown = filter(&self.entries, &self.query);
        self.select(0);
    }

    /// The right side: the preview of the selected run, or the selected
    /// transcript, made once per selection.
    fn text(&mut self) -> &str {
        let (at, text) = match &self.screen {
            Screen::List => {
                let at = (self.shown.get(self.selected).copied(), usize::MAX);
                let fresh = self.text.as_ref().is_none_or(|(was, _)| *was != at);
                (
                    at,
                    fresh.then(|| self.entry().map(Entry::preview).unwrap_or_default()),
                )
            }
            Screen::Agents { rows, selected } => {
                let at = (self.shown.get(self.selected).copied(), *selected);
                let fresh = self.text.as_ref().is_none_or(|(was, _)| *was != at);
                let text = fresh.then(|| match rows.get(*selected).map(|r| (&r.kind, &r.found)) {
                    Some((kind, Ok(session))) => transcript::render(kind, &session.path)
                        .unwrap_or_else(|e| {
                            format!("Cannot read {}: {e}.", session.path.display())
                        }),
                    Some((_, Err(why))) => format!(
                        "{why}\n\nThe run folder's records:\n\n{}",
                        self.entry().map(Entry::preview).unwrap_or_default()
                    ),
                    None => String::new(),
                });
                (at, text)
            }
        };
        if let Some(text) = text {
            self.text = Some((at, text));
        }
        self.text.as_ref().map_or("", |(_, text)| text.as_str())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [top, body, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                .areas(body);
        let (title, items, selected, keys) = match &self.screen {
            Screen::List => (
                format!("> {}", self.query),
                self.shown
                    .iter()
                    .map(|&i| self.entries[i].line())
                    .collect::<Vec<_>>(),
                self.selected,
                "type to filter  ↑↓ move  Enter transcripts  PgUp/PgDn scroll  Esc quit"
                    .to_string(),
            ),
            Screen::Agents { rows, selected } => {
                let resumable = rows.get(*selected).and_then(Row::resume).is_some();
                (
                    self.entry().map(|e| e.line()).unwrap_or_default(),
                    rows.iter().map(Row::line).collect(),
                    *selected,
                    format!(
                        "↑↓ move  PgUp/PgDn scroll{}  Esc back  q quit",
                        if resumable { "  r resume" } else { "" }
                    ),
                )
            }
        };
        let count = items.len();
        frame.render_widget(Line::from(title).bold(), top);
        let list = List::new(items)
            .block(Block::bordered())
            .highlight_style(Style::new().reversed());
        let mut state = ListState::default().with_selected((count > 0).then_some(selected));
        frame.render_stateful_widget(list, left, &mut state);
        let scroll = self.scroll;
        let text = self.text().to_string();
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::bordered())
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            right,
        );
        let footer = if self.status.is_empty() {
            keys
        } else {
            format!("{}  |  {keys}", self.status)
        };
        frame.render_widget(Line::from(footer).dim(), help);
    }
}

/// Opens a workspace in the agent's folder and resumes its session there.
async fn resume(socket: PathBuf, label: &str, resume: &Resume) -> Result<()> {
    let client = Client::new(socket);
    let placed = client.workspace_create(&resume.cwd, label).await?;
    client
        .agent_start(&resume.name, &resume.kind, &placed.pane, &resume.args)
        .await?;
    client.workspace_focus(&placed.workspace).await?;
    Ok(())
}

/// Runs the screens until the person quits. Call it off the async runtime's
/// threads: it blocks on the terminal.
pub fn run(env: &Env, runs_dir: &Path, session: Option<String>) -> Result<()> {
    let mut app = App::new(entries(runs_dir), Roots::from_env(env));
    let runtime = tokio::runtime::Handle::current();
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|frame| app.draw(frame))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.key(key) {
                Effect::None => {}
                Effect::Quit => return Ok(()),
                Effect::Resume(target) => {
                    let label = format!(
                        "{} (resumed)",
                        app.entry().map(|e| e.run.key.clone()).unwrap_or_default()
                    );
                    let done = runtime.block_on(async {
                        let socket = crate::herdr::invoking_socket(env, session.as_deref()).await?;
                        resume(socket, &label, &target).await
                    });
                    app.status = match done {
                        Ok(()) => format!("Resumed in the workspace {label}"),
                        Err(error) => format!("Could not resume: {error:#}"),
                    };
                }
            }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{Run, RunRecord};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press(app: &mut App, code: KeyCode) -> Effect {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn typing_filters_enter_lists_the_agents_and_a_missing_transcript_is_said() {
        let home = tempfile::tempdir().unwrap();
        let runs = home.path().join("runs");
        for (key, title) in [("DATA-1", "Fix the login"), ("DATA-2", "Export to CSV")] {
            let mut record = RunRecord {
                workspace: "acme".into(),
                identifier: key.into(),
                title: title.into(),
                created: "2026-09-01T00:00:00Z".into(),
                ..RunRecord::default()
            };
            record.coordinator.kind = "claude".into();
            let run = Run::create(&runs, record).unwrap();
            std::fs::write(run.issue_md(), format!("# {key} {title}\n")).unwrap();
        }
        let roots = Roots::from_env(&Env::for_test(home.path(), &[]));
        let mut app = App::new(entries(&runs), roots);
        assert!(screen(&mut app).contains("# DATA-1 Fix the login"));
        typed(&mut app, "export");
        assert_eq!(app.shown.len(), 1);
        let list = screen(&mut app);
        assert!(list.contains("> export"), "{list}");
        assert!(list.contains("# DATA-2 Export to CSV"), "{list}");

        assert_eq!(press(&mut app, KeyCode::Enter), Effect::None);
        let agents = screen(&mut app);
        assert!(
            agents.contains("coordinator  claude  no transcript"),
            "{agents}"
        );
        assert!(agents.contains("No transcript is left in"), "{agents}");
        assert!(!agents.contains("r resume"), "{agents}");
        assert_eq!(press(&mut app, KeyCode::Char('r')), Effect::None);
        press(&mut app, KeyCode::Esc);
        assert!(screen(&mut app).contains("> export"));
        assert_eq!(press(&mut app, KeyCode::Esc), Effect::Quit);
    }

    #[test]
    fn a_stopped_claude_session_in_a_folder_still_there_can_be_resumed() {
        let cwd = tempfile::tempdir().unwrap();
        let row = |open| Row {
            agent: "w1".into(),
            kind: "claude".into(),
            cwd: cwd.path().to_string_lossy().into_owned(),
            name: "acme-data-1-w1".into(),
            open,
            found: Ok(Session {
                id: "393e265d".into(),
                path: PathBuf::from("/x.jsonl"),
                modified: None,
            }),
        };
        assert_eq!(
            row(false).resume(),
            Some(Resume {
                kind: "claude".into(),
                cwd: cwd.path().to_string_lossy().into_owned(),
                name: "acme-data-1-w1-resumed".into(),
                args: vec!["--resume".into(), "393e265d".into()],
            })
        );
        assert_eq!(row(true).resume(), None, "not while it is open");
        let cursor = Row {
            kind: "cursor".into(),
            ..row(false)
        };
        assert_eq!(cursor.resume(), None, "resume_args has no cursor form");
    }
}
