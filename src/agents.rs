//! Herdr agent kinds and the CLI arguments a profile or a resume turns into.

use crate::config::Profile;

/// The kinds `herdr agent start --kind` accepts (Herdr 0.9.1).
const KINDS: &[&str] = &[
    "pi",
    "claude",
    "codex",
    "gemini",
    "cursor",
    "devin",
    "agy",
    "cline",
    "omp",
    "mastracode",
    "opencode",
    "copilot",
    "kimi",
    "kiro",
    "droid",
    "amp",
    "grok",
    "hermes",
    "kilo",
    "qodercli",
    "qwen",
    "letta",
    "maki",
    "muse",
];

pub fn is_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// Only these CLIs take the effort as a flag of its own.
pub fn has_effort_flag(kind: &str) -> bool {
    matches!(kind, "claude" | "codex")
}

/// The model flag, the effort flag, then the profile's own `args`.
pub fn profile_args(profile: &Profile) -> Vec<String> {
    let codex = profile.kind == "codex";
    let mut out = Vec::new();
    if let Some(model) = &profile.model {
        out.push(if codex { "-m" } else { "--model" }.to_string());
        out.push(model.clone());
    }
    if let Some(effort) = &profile.effort {
        match profile.kind.as_str() {
            "claude" => out.extend(["--effort".to_string(), effort.clone()]),
            "codex" => out.extend(["-c".to_string(), format!("model_reasoning_effort={effort}")]),
            _ => {}
        }
    }
    out.extend(profile.args.iter().cloned());
    out
}

/// The words that make the kind's CLI continue a native session, or `None`
/// when the kind cannot or the session id is unusable.
pub fn resume_args(kind: &str, session: &str) -> Option<Vec<String>> {
    if session.is_empty() || session.starts_with('-') {
        return None;
    }
    let s = session.to_string();
    Some(match kind {
        "claude" => vec!["--resume".into(), s],
        "codex" => vec!["resume".into(), s],
        "opencode" => vec!["--session".into(), s],
        "copilot" => vec![format!("--resume={s}")],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(kind: &str, model: Option<&str>, effort: Option<&str>, args: &[&str]) -> Profile {
        Profile {
            kind: kind.into(),
            model: model.map(Into::into),
            effort: effort.map(Into::into),
            args: args.iter().map(|a| a.to_string()).collect(),
            description: String::new(),
            instructions: None,
        }
    }

    #[test]
    fn herdr_kinds_are_known_and_others_are_not() {
        for kind in [
            "pi", "claude", "codex", "cursor", "opencode", "copilot", "muse", "qodercli",
        ] {
            assert!(is_kind(kind), "{kind}");
        }
        for kind in ["chatgpt", "", "Claude", "cursor-agent"] {
            assert!(!is_kind(kind), "{kind}");
        }
        assert_eq!(KINDS.len(), 24);
    }

    #[test]
    fn only_claude_and_codex_have_an_effort_flag() {
        assert!(has_effort_flag("claude"));
        assert!(has_effort_flag("codex"));
        for kind in ["cursor", "gemini", "opencode", "pi"] {
            assert!(!has_effort_flag(kind), "{kind}");
        }
    }

    #[test]
    fn a_profile_becomes_model_effort_and_extra_flags_per_kind() {
        let claude = profile(
            "claude",
            Some("opus"),
            Some("high"),
            &["--permission-mode", "auto"],
        );
        assert_eq!(
            profile_args(&claude),
            [
                "--model",
                "opus",
                "--effort",
                "high",
                "--permission-mode",
                "auto"
            ]
        );
        let codex = profile(
            "codex",
            Some("gpt-6-sol"),
            Some("xhigh"),
            &["-s", "workspace-write"],
        );
        assert_eq!(
            profile_args(&codex),
            [
                "-m",
                "gpt-6-sol",
                "-c",
                "model_reasoning_effort=xhigh",
                "-s",
                "workspace-write"
            ]
        );
        let gemini = profile("gemini", Some("gemini-3-pro"), Some("high"), &["--yolo"]);
        assert_eq!(profile_args(&gemini), ["--model", "gemini-3-pro", "--yolo"]);
        assert!(profile_args(&profile("claude", None, None, &[])).is_empty());
        assert_eq!(
            profile_args(&profile("codex", None, Some("low"), &[])),
            ["-c", "model_reasoning_effort=low"]
        );
    }

    #[test]
    fn resume_words_depend_on_the_kind_and_need_a_usable_session() {
        let cases: [(&str, &str, Option<&[&str]>); 7] = [
            ("claude", "abc-1", Some(&["--resume", "abc-1"])),
            ("codex", "abc-1", Some(&["resume", "abc-1"])),
            ("opencode", "ses_9", Some(&["--session", "ses_9"])),
            ("copilot", "c7", Some(&["--resume=c7"])),
            ("gemini", "abc-1", None),
            ("claude", "", None),
            ("claude", "--dangerous", None),
        ];
        for (kind, session, expected) in cases {
            let expected = expected.map(|words| words.iter().map(|w| w.to_string()).collect());
            assert_eq!(resume_args(kind, session), expected, "{kind} {session:?}");
        }
    }
}
