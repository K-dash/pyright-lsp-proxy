use crate::error::ProxyError;

/// Apply incremental change (range-based partial replacement)
pub(crate) fn apply_incremental_change(
    text: &mut String,
    range: &serde_json::Value,
    new_text: &str,
) -> Result<(), ProxyError> {
    let start = range
        .get("start")
        .ok_or_else(|| ProxyError::InvalidMessage("didChange range missing start".to_string()))?;
    let end = range
        .get("end")
        .ok_or_else(|| ProxyError::InvalidMessage("didChange range missing end".to_string()))?;

    let start_line = start
        .get("line")
        .and_then(|l| l.as_u64())
        .ok_or_else(|| ProxyError::InvalidMessage("didChange start missing line".to_string()))?
        as usize;
    let start_char = start
        .get("character")
        .and_then(|c| c.as_u64())
        .ok_or_else(|| {
            ProxyError::InvalidMessage("didChange start missing character".to_string())
        })? as usize;

    let end_line = end
        .get("line")
        .and_then(|l| l.as_u64())
        .ok_or_else(|| ProxyError::InvalidMessage("didChange end missing line".to_string()))?
        as usize;
    let end_char = end
        .get("character")
        .and_then(|c| c.as_u64())
        .ok_or_else(|| ProxyError::InvalidMessage("didChange end missing character".to_string()))?
        as usize;

    let start_offset = position_to_offset(text, start_line, start_char)?;
    let end_offset = position_to_offset(text, end_line, end_char)?;

    if start_offset > end_offset {
        return Err(ProxyError::InvalidMessage(format!(
            "Invalid range: start offset ({}) > end offset ({})",
            start_offset, end_offset
        )));
    }

    text.replace_range(start_offset..end_offset, new_text);

    Ok(())
}

/// Convert LSP position (line, character) to byte offset
/// LSP character is UTF-16 code unit count
pub(crate) fn position_to_offset(
    text: &str,
    line: usize,
    character: usize,
) -> Result<usize, ProxyError> {
    let mut current_line = 0;
    let mut line_start_offset = 0;

    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            if current_line == line {
                return find_offset_in_line(text, line_start_offset, idx, character);
            }
            current_line += 1;
            line_start_offset = idx + 1;
        }
    }

    if current_line == line {
        return find_offset_in_line(text, line_start_offset, text.len(), character);
    }

    Err(ProxyError::InvalidMessage(format!(
        "Position out of range: line={} (max={}), character={}",
        line, current_line, character
    )))
}

/// Count UTF-16 code units within line and return byte offset
fn find_offset_in_line(
    text: &str,
    line_start: usize,
    line_end: usize,
    character: usize,
) -> Result<usize, ProxyError> {
    let line_text = &text[line_start..line_end];
    let mut utf16_offset = 0;

    for (idx, ch) in line_text.char_indices() {
        if utf16_offset >= character {
            return Ok(line_start + idx);
        }
        utf16_offset += ch.len_utf16();
    }

    Ok(line_end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_position_to_offset_simple() {
        let text = "hello\nworld\n";

        assert_eq!(position_to_offset(text, 0, 0).unwrap(), 0);
        assert_eq!(position_to_offset(text, 0, 5).unwrap(), 5);
        assert_eq!(position_to_offset(text, 1, 0).unwrap(), 6);
        assert_eq!(position_to_offset(text, 1, 5).unwrap(), 11);
    }

    #[test]
    fn test_position_to_offset_multibyte() {
        let text = "こんにちは\nworld\n";

        assert_eq!(position_to_offset(text, 0, 0).unwrap(), 0);
        assert_eq!(position_to_offset(text, 0, 1).unwrap(), 3);
        assert_eq!(position_to_offset(text, 1, 0).unwrap(), 16);
    }

    #[test]
    fn test_apply_incremental_change_simple_replace() {
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 5 }
        });

        apply_incremental_change(&mut text, &range, "hi").unwrap();
        assert_eq!(text, "hi world");
    }

    #[test]
    fn test_apply_incremental_change_insert() {
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 0, "character": 5 }
        });

        apply_incremental_change(&mut text, &range, " beautiful").unwrap();
        assert_eq!(text, "hello beautiful world");
    }

    #[test]
    fn test_apply_incremental_change_delete() {
        let mut text = "hello beautiful world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 0, "character": 15 }
        });

        apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_apply_incremental_change_multiline() {
        let mut text = "def hello():\n    print('hello')\n".to_string();
        let range = json!({
            "start": { "line": 1, "character": 11 },
            "end": { "line": 1, "character": 16 }
        });

        apply_incremental_change(&mut text, &range, "world").unwrap();
        assert_eq!(text, "def hello():\n    print('world')\n");
    }

    #[test]
    fn test_apply_incremental_change_cross_line() {
        let mut text = "line1\nline2\nline3\n".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 2, "character": 0 }
        });

        apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "line1line3\n");
    }

    #[test]
    fn test_position_to_offset_surrogate_pair() {
        let text = "a😀b\n";

        assert_eq!(position_to_offset(text, 0, 0).unwrap(), 0);
        assert_eq!(position_to_offset(text, 0, 1).unwrap(), 1);
        assert_eq!(position_to_offset(text, 0, 3).unwrap(), 5);
        assert_eq!(position_to_offset(text, 0, 4).unwrap(), 6);
    }

    #[test]
    fn test_position_to_offset_line_end_clamp() {
        let text = "abc\ndef\n";

        assert_eq!(position_to_offset(text, 0, 100).unwrap(), 3);
        assert_eq!(position_to_offset(text, 1, 100).unwrap(), 7);
    }

    #[test]
    fn test_position_to_offset_line_out_of_range() {
        let text = "abc\ndef\n";

        let result = position_to_offset(text, 10, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_incremental_change_invalid_range() {
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 10 },
            "end": { "line": 0, "character": 5 }
        });

        let result = apply_incremental_change(&mut text, &range, "test");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_incremental_change_with_emoji() {
        let mut text = "hello 😀 world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 6 },
            "end": { "line": 0, "character": 9 }
        });

        apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_position_to_offset_empty_text() {
        let text = "";

        assert_eq!(position_to_offset(text, 0, 0).unwrap(), 0);
    }

    #[test]
    fn test_position_to_offset_no_trailing_newline() {
        let text = "abc";

        assert_eq!(position_to_offset(text, 0, 0).unwrap(), 0);
        assert_eq!(position_to_offset(text, 0, 3).unwrap(), 3);
    }
}
