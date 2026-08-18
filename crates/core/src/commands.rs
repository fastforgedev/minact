//! GitHub Actions runtime commands.
//!
//! Steps talk back to the runner two ways, and this module parses both:
//!
//! * **Environment files** — a step appends to the file named by
//!   `$GITHUB_OUTPUT`, `$GITHUB_ENV`, `$GITHUB_PATH` or `$GITHUB_STEP_SUMMARY`.
//!   This is the current mechanism.
//! * **Workflow commands** — a step prints `::set-output name=k::v` or
//!   `::error::msg` on stdout. Output-setting this way is deprecated on
//!   GitHub but still very common in the wild, so it stays supported.

use std::collections::HashMap;

/// A workflow command parsed from a line of step output.
///
/// The wire format is `::name key=value,key2=value2::message`, where both the
/// property section and the message are optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub name: String,
    pub properties: HashMap<String, String>,
    pub message: String,
}

impl ParsedCommand {
    /// Look up a command property (e.g. `name` in `::set-output name=foo::bar`).
    pub fn property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(String::as_str)
    }
}

/// Parse a line of step output as a workflow command.
///
/// Returns `None` when the line is ordinary output, which includes lines that
/// merely look command-ish such as `echo "::::"` or `:: not a command ::`.
pub fn parse_workflow_command(line: &str) -> Option<ParsedCommand> {
    let rest = line.trim().strip_prefix("::")?;

    // Split the command spec from the message on the next `::`.
    let (spec, message) = {
        let idx = rest.find("::")?;
        (&rest[..idx], &rest[idx + 2..])
    };

    // The name runs up to the first space; anything after it is properties.
    let (name, props) = match spec.find(' ') {
        Some(idx) => (&spec[..idx], spec[idx + 1..].trim()),
        None => (spec, ""),
    };

    if !is_command_name(name) {
        return None;
    }

    let mut properties = HashMap::new();
    if !props.is_empty() {
        for pair in props.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let (key, value) = match pair.split_once('=') {
                Some((key, value)) => (key.trim(), value),
                // A property with no `=` is malformed; ignore it rather than
                // discarding the whole command.
                None => continue,
            };
            if key.is_empty() {
                continue;
            }
            properties.insert(key.to_string(), unescape_property(value));
        }
    }

    Some(ParsedCommand {
        name: name.to_string(),
        properties,
        message: unescape_message(message),
    })
}

/// Command names are `[A-Za-z][A-Za-z0-9-]*`, which rules out most prose that
/// happens to be wrapped in colons.
fn is_command_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn unescape_message(value: &str) -> String {
    value
        .replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

fn unescape_property(value: &str) -> String {
    value
        .replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%3A", ":")
        .replace("%2C", ",")
        .replace("%25", "%")
}

/// Parse the `key=value` / heredoc format used by `$GITHUB_OUTPUT` and
/// `$GITHUB_ENV`.
///
/// Two forms are accepted:
///
/// ```text
/// key=value
///
/// key<<EOF
/// line one
/// line two
/// EOF
/// ```
///
/// Later entries win, matching the runner. Order is preserved so callers can
/// apply the entries sequentially.
pub fn parse_key_value_file(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::new();
    let mut lines = content.lines();

    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }

        // Heredoc form: `key<<DELIMITER`.
        if let Some((key, delimiter)) = split_heredoc_header(line) {
            if key.is_empty() {
                return Err(format!("missing key in line: {}", line));
            }
            if delimiter.is_empty() {
                return Err(format!("missing heredoc delimiter in line: {}", line));
            }

            let mut value_lines: Vec<&str> = Vec::new();
            let mut terminated = false;
            for value_line in lines.by_ref() {
                if value_line == delimiter {
                    terminated = true;
                    break;
                }
                value_lines.push(value_line);
            }
            if !terminated {
                return Err(format!(
                    "unterminated heredoc for key '{}': delimiter '{}' never appeared",
                    key, delimiter
                ));
            }
            entries.push((key.to_string(), value_lines.join("\n")));
            continue;
        }

        // Simple form: `key=value`.
        match line.split_once('=') {
            Some((key, value)) => {
                let key = key.trim();
                if key.is_empty() {
                    return Err(format!("missing key in line: {}", line));
                }
                entries.push((key.to_string(), value.to_string()));
            }
            None => {
                return Err(format!(
                    "invalid format, expected 'key=value' or 'key<<DELIMITER': {}",
                    line
                ));
            }
        }
    }

    Ok(entries)
}

/// Split a `key<<DELIMITER` header, if the line is one.
fn split_heredoc_header(line: &str) -> Option<(&str, &str)> {
    let (key, delimiter) = line.split_once("<<")?;
    // `key=a<<b` is a plain assignment, not a heredoc.
    if key.contains('=') {
        return None;
    }
    Some((key.trim(), delimiter.trim()))
}

/// Parse the `$GITHUB_PATH` file: one directory per line, blank lines ignored.
pub fn parse_path_file(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Replace every registered mask value in a line with `***`.
///
/// Masks come from `::add-mask::` and from secret values, so that a step
/// echoing a secret does not leak it into the log.
pub fn apply_masks(line: &str, masks: &[String]) -> String {
    let mut result = line.to_string();
    for mask in masks {
        if mask.is_empty() {
            continue;
        }
        if result.contains(mask.as_str()) {
            result = result.replace(mask.as_str(), "***");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_with_properties() {
        let cmd = parse_workflow_command("::set-output name=version::1.2.3").unwrap();
        assert_eq!(cmd.name, "set-output");
        assert_eq!(cmd.property("name"), Some("version"));
        assert_eq!(cmd.message, "1.2.3");
    }

    #[test]
    fn parses_command_without_properties_or_message() {
        let cmd = parse_workflow_command("::endgroup::").unwrap();
        assert_eq!(cmd.name, "endgroup");
        assert!(cmd.properties.is_empty());
        assert_eq!(cmd.message, "");
    }

    #[test]
    fn parses_multiple_properties_and_unescapes() {
        let cmd = parse_workflow_command("::error file=src/a%3Ab.rs,line=4::boom%0Aagain").unwrap();
        assert_eq!(cmd.name, "error");
        assert_eq!(cmd.property("file"), Some("src/a:b.rs"));
        assert_eq!(cmd.property("line"), Some("4"));
        assert_eq!(cmd.message, "boom\nagain");
    }

    #[test]
    fn ignores_lines_that_are_not_commands() {
        assert!(parse_workflow_command("hello world").is_none());
        assert!(parse_workflow_command("::not a command").is_none());
        assert!(parse_workflow_command(":: spaced out ::x").is_none());
        assert!(parse_workflow_command("::::").is_none());
        // A separator line of colons must not be swallowed as a command.
        assert!(parse_workflow_command("::::::::").is_none());
    }

    #[test]
    fn parses_simple_key_value_file() {
        let entries = parse_key_value_file("ver=1.2.3\nname=minact\n").unwrap();
        assert_eq!(
            entries,
            vec![
                ("ver".to_string(), "1.2.3".to_string()),
                ("name".to_string(), "minact".to_string()),
            ]
        );
    }

    #[test]
    fn keeps_equals_signs_inside_values() {
        let entries = parse_key_value_file("query=a=b=c\n").unwrap();
        assert_eq!(entries, vec![("query".to_string(), "a=b=c".to_string())]);
    }

    #[test]
    fn parses_heredoc_values() {
        let content = "notes<<EOF\nline one\nline two\nEOF\nver=9\n";
        let entries = parse_key_value_file(content).unwrap();
        assert_eq!(
            entries,
            vec![
                ("notes".to_string(), "line one\nline two".to_string()),
                ("ver".to_string(), "9".to_string()),
            ]
        );
    }

    #[test]
    fn heredoc_preserves_blank_and_equals_lines() {
        let content = "body<<EOF\nfirst\n\nkey=value\nEOF\n";
        let entries = parse_key_value_file(content).unwrap();
        assert_eq!(
            entries,
            vec![("body".to_string(), "first\n\nkey=value".to_string())]
        );
    }

    #[test]
    fn rejects_unterminated_heredoc() {
        let err = parse_key_value_file("notes<<EOF\nline one\n").unwrap_err();
        assert!(err.contains("unterminated heredoc"), "{}", err);
    }

    #[test]
    fn rejects_garbage_lines() {
        let err = parse_key_value_file("just some text\n").unwrap_err();
        assert!(err.contains("invalid format"), "{}", err);
    }

    #[test]
    fn parses_path_file() {
        let paths = parse_path_file("/usr/local/bin\n\n  /opt/tools  \n");
        assert_eq!(paths, vec!["/usr/local/bin", "/opt/tools"]);
    }

    #[test]
    fn masks_registered_values() {
        let masks = vec!["s3cret".to_string()];
        assert_eq!(apply_masks("token=s3cret done", &masks), "token=*** done");
    }
}
