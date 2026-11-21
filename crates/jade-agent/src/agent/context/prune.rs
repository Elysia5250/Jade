//! Bound a fresh tool result so one command cannot fill the window.
//! Older observations are masked in `mask`, not truncated here.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub messages_pruned: usize,
    pub bytes_saved: usize,
}

/// Bound a single fresh tool result using a 38% head / 62% tail split so both
/// starting headers and trailing errors/exit codes survive.
pub fn bound_tool_output(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes || max_bytes < 256 {
        return s.to_string();
    }
    let reserve = 96usize;
    let budget = max_bytes.saturating_sub(reserve);
    let head_len = floor_char_boundary(s, budget * 38 / 100);
    let tail_target = s.len().saturating_sub(budget - head_len);
    let tail_start = ceil_char_boundary(s, tail_target);
    if tail_start <= head_len {
        return s.to_string();
    }
    let omitted = tail_start - head_len;
    format!(
        "{}\n... [truncated {omitted} bytes from middle] ...\n{}",
        &s[..head_len],
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_tool_output_preserves_head_and_tail() {
        let text = format!("HEADER\n{}\nFOOTER [job #3 exit=2]", "x".repeat(4_000));
        let bounded = bound_tool_output(&text, 600);
        assert!(bounded.starts_with("HEADER\n"));
        assert!(bounded.ends_with("FOOTER [job #3 exit=2]"));
        assert!(bounded.len() <= 600);
    }
}
