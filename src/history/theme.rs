//! The colors of the history screens. Only the terminal's 16 ANSI colors and
//! its default colors are used, never RGB: Herdr draws them with its theme's
//! palette, so the screens follow the theme, dark or light, and change with
//! it.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Block;

/// Run keys, agents, keys to press, the focused border.
const ACCENT: Color = Color::Cyan;
/// What reaches the agent: the person's messages.
const PERSON: Color = Color::Green;
/// What the agent sends: its own messages.
const AGENT: Color = Color::Blue;
/// The agent's tool calls.
const CALL: Color = Color::Yellow;
/// Headings of the run's files.
const HEADING: Color = Color::Magenta;
/// Times, tool results, borders, explanations.
const QUIET: Color = Color::DarkGray;

fn quiet() -> Style {
    Style::new().fg(QUIET)
}

/// A bordered pane titled `title`; the focused one's border is the accent.
pub fn block(title: &str, focused: bool) -> Block<'static> {
    let border = if focused { ACCENT } else { QUIET };
    Block::bordered()
        .border_style(Style::new().fg(border))
        .title(Span::styled(format!(" {title} "), Style::new().bold()))
}

/// The selected line of a list.
pub fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

pub fn prompt(query: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("> ", Style::new().fg(ACCENT).bold()),
        Span::raw(query.to_string()),
        Span::styled("▏", Style::new().fg(ACCENT)),
    ])
}

fn status(status: &str) -> Style {
    match status {
        "active" => Style::new().fg(Color::Green),
        "detached" => Style::new().fg(Color::Yellow),
        _ => quiet(),
    }
}

/// A run of the list: its key, title, state, update time and PRs.
pub fn run_line(key: &str, title: &str, state: &str, updated: &str, prs: &[&str]) -> Line<'static> {
    let mut spans = vec![
        Span::styled(key.to_string(), Style::new().fg(ACCENT).bold()),
        Span::raw(format!("  {title}  ")),
        Span::styled(state.to_string(), status(state)),
        Span::styled(format!("  {updated}"), quiet()),
    ];
    for pr in prs {
        spans.push(Span::styled(format!("  {pr}"), Style::new().fg(AGENT)));
    }
    Line::from(spans)
}

/// A transcript of the agent list: `found` holds the time, the session and
/// whether it is the run folder's copy.
pub fn agent_line(agent: &str, kind: &str, found: Option<(&str, &str, bool)>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(agent.to_string(), Style::new().fg(ACCENT).bold()),
        Span::styled(format!("  {kind}"), Style::new().fg(HEADING)),
    ];
    match found {
        Some((at, id, kept)) => spans.extend([
            Span::styled(format!("  {at}"), quiet()),
            Span::raw(format!("  {id}")),
            if kept {
                Span::styled("  copy", Style::new().fg(Color::Green))
            } else {
                Span::styled("  original", Style::new().fg(Color::Yellow))
            },
        ]),
        None => spans.push(Span::styled("  no transcript", Style::new().fg(Color::Red))),
    }
    Line::from(spans)
}

/// Key hints, then a status message when there is one.
pub fn footer(keys: &[(&str, &str)], message: &str) -> Line<'static> {
    let mut spans = Vec::new();
    if !message.is_empty() {
        spans.push(Span::styled(
            format!("{message}  "),
            Style::new().fg(Color::Yellow),
        ));
    }
    for (key, what) in keys {
        spans.push(Span::styled(
            key.to_string(),
            Style::new().fg(ACCENT).bold(),
        ));
        spans.push(Span::styled(format!(" {what}  "), quiet()));
    }
    Line::from(spans)
}

/// A run's preview: headings, links and the status line stand out.
pub fn preview(text: &str) -> Text<'static> {
    text.lines()
        .enumerate()
        .map(|(n, line)| {
            let style = if n == 0 {
                Style::new().fg(ACCENT).bold()
            } else if line.starts_with("# ") {
                Style::new().fg(HEADING).bold()
            } else if line.starts_with('#') {
                Style::new().fg(HEADING)
            } else if line.starts_with("PR ") || line.starts_with("https://") {
                Style::new().fg(AGENT)
            } else if line.starts_with("Status ") {
                quiet()
            } else {
                Style::new()
            };
            Line::styled(line.to_string(), style)
        })
        .collect()
}

/// Why a transcript cannot be shown, then the run's records.
pub fn note(text: &str) -> Text<'static> {
    let (why, rest) = text.split_once("\n\n").unwrap_or((text, ""));
    let mut lines = vec![
        Line::styled(why.to_string(), Style::new().fg(Color::Yellow)),
        Line::raw(""),
    ];
    lines.extend(preview(rest).lines);
    Text::from(lines)
}

/// A rendered transcript: where it was read from, then each step colored by
/// its side. The person's messages and tool results come in; the agent's
/// messages and tool calls go out.
pub fn transcript(text: &str) -> Text<'static> {
    let mut lines = Vec::new();
    let (source, rest) = text.split_once("\n\n").unwrap_or((text, ""));
    lines.push(Line::styled(source.to_string(), quiet().italic()));
    lines.push(Line::raw(""));
    let mut body = Style::new();
    for line in rest.lines() {
        let style = if let Some(who) = line.strip_prefix("── ") {
            let color = if who == "user" { PERSON } else { AGENT };
            body = Style::new().fg(color);
            Style::new().fg(color).bold()
        } else if line.starts_with("→ ") {
            body = Style::new();
            Style::new().fg(CALL)
        } else if line.starts_with("← error") {
            body = Style::new().fg(Color::Red);
            Style::new().fg(Color::Red).bold()
        } else if line.starts_with("← ") {
            body = quiet();
            quiet().bold()
        } else if line.is_empty() {
            body = Style::new();
            body
        } else {
            body
        };
        lines.push(Line::styled(line.to_string(), style));
    }
    Text::from(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors(text: &Text) -> Vec<(String, Option<Color>)> {
        text.lines
            .iter()
            .map(|l| (l.to_string(), l.style.fg))
            .collect()
    }

    #[test]
    fn a_transcript_colors_each_side() {
        let text = "The run folder's copy, /x.jsonl\n\n── user\nStart.\n\n── assistant\nOn it.\n\n→ Bash {\"command\":\"ls\"}\n← result:\n  a.txt\n\n← error:\n  denied\n";
        let fg = |c| Some(c);
        assert_eq!(
            colors(&transcript(text)),
            [
                ("The run folder's copy, /x.jsonl".into(), fg(QUIET)),
                ("".into(), None),
                ("── user".into(), fg(PERSON)),
                ("Start.".into(), fg(PERSON)),
                ("".into(), None),
                ("── assistant".into(), fg(AGENT)),
                ("On it.".into(), fg(AGENT)),
                ("".into(), None),
                ("→ Bash {\"command\":\"ls\"}".into(), fg(CALL)),
                ("← result:".into(), fg(QUIET)),
                ("  a.txt".into(), fg(QUIET)),
                ("".into(), None),
                ("← error:".into(), fg(Color::Red)),
                ("  denied".into(), fg(Color::Red)),
            ]
        );
    }

    #[test]
    fn the_preview_marks_its_title_headings_and_links() {
        let text = "acme/DATA-1  Fix the login\nStatus closed, updated -\nPR https://github.com/acme/api/pull/7\n\n# DATA-1 Fix the login\n\nThe form fails.\n## user-1\n";
        let styles: Vec<Option<Color>> = preview(text).lines.iter().map(|l| l.style.fg).collect();
        assert_eq!(
            styles,
            [
                Some(ACCENT),
                Some(QUIET),
                Some(AGENT),
                None,
                Some(HEADING),
                None,
                None,
                Some(HEADING)
            ]
        );
    }
}
