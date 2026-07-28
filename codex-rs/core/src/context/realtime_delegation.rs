use super::ContextualUserFragment;
use codex_utils_string::approx_bytes_for_tokens;

pub(crate) const REALTIME_DELEGATION_MAX_ESTIMATED_TOKENS: usize = 8_192;
const REALTIME_DELEGATION_INPUT_MAX_ESTIMATED_TOKENS: usize = 2_048;
const INPUT_TRUNCATION_MARKER: &str = "… input truncated …";
const TRANSCRIPT_TRUNCATION_MARKER: &str = "… earlier transcript truncated …\n";

#[derive(Clone, Copy)]
enum TruncationPlacement {
    Middle,
    PreserveTail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RealtimeDelegationSource {
    Handoff,
    TranscriptTailFlush,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RealtimeDelegation<'a> {
    input: &'a str,
    transcript_delta: Option<&'a str>,
    source: RealtimeDelegationSource,
}

impl<'a> RealtimeDelegation<'a> {
    pub(crate) fn new(
        input: &'a str,
        transcript_delta: Option<&'a str>,
        source: RealtimeDelegationSource,
    ) -> Self {
        Self {
            input,
            transcript_delta,
            source,
        }
    }
}

impl ContextualUserFragment for RealtimeDelegation<'_> {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<realtime_delegation>", "</realtime_delegation>")
    }

    fn body(&self) -> String {
        let source = match self.source {
            RealtimeDelegationSource::Handoff => "",
            RealtimeDelegationSource::TranscriptTailFlush => {
                "  <source>transcript_tail_flush</source>\n"
            }
        };
        let transcript_delta = self.transcript_delta.filter(|text| !text.is_empty());
        let (open_marker, close_marker) = Self::type_markers();
        let max_body_bytes = approx_bytes_for_tokens(REALTIME_DELEGATION_MAX_ESTIMATED_TOKENS)
            .saturating_sub(open_marker.len())
            .saturating_sub(close_marker.len());
        let fixed_body_bytes = "\n".len()
            + source.len()
            + "  <input></input>\n".len()
            + transcript_delta
                .map(|_| "  <transcript_delta></transcript_delta>\n".len())
                .unwrap_or_default();
        let content_budget = max_body_bytes.saturating_sub(fixed_body_bytes);
        let input_budget = if transcript_delta.is_some() {
            content_budget.min(approx_bytes_for_tokens(
                REALTIME_DELEGATION_INPUT_MAX_ESTIMATED_TOKENS,
            ))
        } else {
            content_budget
        };
        let input = escape_xml_text_bounded(
            self.input,
            input_budget,
            INPUT_TRUNCATION_MARKER,
            TruncationPlacement::Middle,
        );
        let transcript_delta = transcript_delta.map(|transcript_delta| {
            escape_xml_text_bounded(
                transcript_delta,
                content_budget.saturating_sub(input.len()),
                TRANSCRIPT_TRUNCATION_MARKER,
                TruncationPlacement::PreserveTail,
            )
        });

        let mut body = String::with_capacity(max_body_bytes);
        body.push('\n');
        body.push_str(source);
        body.push_str("  <input>");
        body.push_str(&input);
        body.push_str("</input>\n");
        if let Some(transcript_delta) = transcript_delta {
            body.push_str("  <transcript_delta>");
            body.push_str(&transcript_delta);
            body.push_str("</transcript_delta>\n");
        }
        body
    }
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_xml_text_bounded(
    input: &str,
    max_escaped_bytes: usize,
    truncation_marker: &str,
    placement: TruncationPlacement,
) -> String {
    if escaped_xml_len(input) <= max_escaped_bytes {
        return escape_xml_text(input);
    }
    if truncation_marker.len() >= max_escaped_bytes {
        let marker_end = escaped_prefix_end(truncation_marker, max_escaped_bytes);
        return truncation_marker[..marker_end].to_string();
    }

    let content_budget = max_escaped_bytes.saturating_sub(truncation_marker.len());
    let (prefix_budget, suffix_budget) = match placement {
        TruncationPlacement::Middle => {
            let prefix_budget = content_budget / 2;
            (prefix_budget, content_budget - prefix_budget)
        }
        TruncationPlacement::PreserveTail => (0, content_budget),
    };
    let prefix_end = escaped_prefix_end(input, prefix_budget);
    let suffix_start = escaped_suffix_start(input, suffix_budget).max(prefix_end);

    let mut output = String::with_capacity(max_escaped_bytes);
    output.push_str(&escape_xml_text(&input[..prefix_end]));
    output.push_str(truncation_marker);
    output.push_str(&escape_xml_text(&input[suffix_start..]));
    output
}

fn escaped_xml_len(input: &str) -> usize {
    input
        .chars()
        .map(escaped_xml_char_len)
        .fold(0, usize::saturating_add)
}

fn escaped_xml_char_len(character: char) -> usize {
    match character {
        '&' => "&amp;".len(),
        '<' | '>' => "&lt;".len(),
        _ => character.len_utf8(),
    }
}

fn escaped_prefix_end(input: &str, max_escaped_bytes: usize) -> usize {
    let mut escaped_bytes = 0usize;
    let mut end = 0usize;
    for (index, character) in input.char_indices() {
        let character_bytes = escaped_xml_char_len(character);
        if escaped_bytes.saturating_add(character_bytes) > max_escaped_bytes {
            break;
        }
        escaped_bytes = escaped_bytes.saturating_add(character_bytes);
        end = index + character.len_utf8();
    }
    end
}

fn escaped_suffix_start(input: &str, max_escaped_bytes: usize) -> usize {
    let mut escaped_bytes = 0usize;
    let mut start = input.len();
    for (index, character) in input.char_indices().rev() {
        let character_bytes = escaped_xml_char_len(character);
        if escaped_bytes.saturating_add(character_bytes) > max_escaped_bytes {
            break;
        }
        escaped_bytes = escaped_bytes.saturating_add(character_bytes);
        start = index;
    }
    start
}
