//! Transcript gauge: tail-read a Claude Code session transcript (JSONL) and
//! extract the latest context usage. Parsing is deliberately defensive — the
//! transcript format is internal to Claude Code, so anything unrecognized is
//! skipped and the gauge just degrades to "unknown".

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct Gauge {
    pub new_offset: u64,
    /// input + cache_read + cache_creation tokens of the last assistant
    /// message seen — the size of the context the model last consumed.
    pub prompt_tokens: Option<u64>,
    pub model: Option<String>,
}

/// Read the transcript from `offset`, returning the updated gauge. If the
/// file shrank (rewritten by compaction or rotation), re-reads from 0.
pub fn read_gauge(path: &Path, offset: u64) -> std::io::Result<Gauge> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = if len < offset { 0 } else { offset };
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(start))?;

    let mut gauge = Gauge {
        new_offset: start,
        ..Gauge::default()
    };
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // Only advance past complete lines so a partially-written trailing
        // line is re-read next time.
        if !line.ends_with('\n') {
            break;
        }
        gauge.new_offset += n as u64;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
            gauge.model = Some(model.to_string());
        }
        if let Some(usage) = message.get("usage") {
            let field = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let total = field("input_tokens")
                + field("cache_read_input_tokens")
                + field("cache_creation_input_tokens");
            if total > 0 {
                gauge.prompt_tokens = Some(total);
            }
        }
    }
    Ok(gauge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extracts_last_usage_and_tracks_offset() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":"hi"}}}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"claude-x","usage":{{"input_tokens":100,"cache_read_input_tokens":900,"cache_creation_input_tokens":50,"output_tokens":10}}}}}}"#
        )
        .unwrap();
        writeln!(f, "not json at all").unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"claude-x","usage":{{"input_tokens":200,"cache_read_input_tokens":1000,"cache_creation_input_tokens":0}}}}}}"#
        )
        .unwrap();

        let g = read_gauge(tmp.path(), 0).unwrap();
        assert_eq!(g.prompt_tokens, Some(1200));
        assert_eq!(g.model.as_deref(), Some("claude-x"));
        assert!(g.new_offset > 0);

        // Incremental read from offset: nothing new → no usage.
        let g2 = read_gauge(tmp.path(), g.new_offset).unwrap();
        assert_eq!(g2.prompt_tokens, None);
        assert_eq!(g2.new_offset, g.new_offset);

        // Shrunk file → rescan from zero.
        let g3 = read_gauge(tmp.path(), g.new_offset + 10_000).unwrap();
        assert_eq!(g3.prompt_tokens, Some(1200));
    }

    #[test]
    fn partial_trailing_line_not_consumed() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        write!(f, "{{\"type\":\"assistant\"").unwrap();
        let g = read_gauge(tmp.path(), 0).unwrap();
        assert_eq!(g.new_offset, 0);
    }
}
