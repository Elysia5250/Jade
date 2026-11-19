//! Fold source observations down to an outline.
//!
//! Rust uses tree-sitter. Other source files use signature lines and indent.
//! Shell logs and prose stay with the caller.

use std::collections::HashSet;
use tree_sitter::Node;

const SOURCE_EXT: &[&str] = &[
    "rs", "py", "pyi", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java", "c", "h", "cc", "cpp",
    "cxx", "hpp", "hh", "rb", "swift", "kt", "kts", "cs", "php", "scala", "lua", "zig", "sh",
    "bash", "zsh", "sql", "vue", "svelte",
];

pub fn is_source_path(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    !ext.is_empty() && ext.len() < 8 && SOURCE_EXT.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// Outline of a source file. `None` when the path is not source.
pub fn fold_source(path: &str, source: &str, focus: &HashSet<String>) -> Option<String> {
    if !is_source_path(path) {
        return None;
    }
    let folded = if path
        .rsplit('.')
        .next()
        .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
    {
        fold_rust(source, focus).unwrap_or_else(|| fold_generic(source, focus))
    } else {
        fold_generic(source, focus)
    };
    Some(folded)
}

fn fold_rust(source: &str, focus: &HashSet<String>) -> Option<String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut out = String::new();
    render_children(tree.root_node(), source, focus, &mut out);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn render_children(node: Node, src: &str, focus: &HashSet<String>, out: &mut String) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        render_item(child, src, focus, out);
    }
}

fn render_item(node: Node, src: &str, focus: &HashSet<String>, out: &mut String) {
    match node.kind() {
        "use_declaration" => push_line(out, node_text(node, src)),
        "function_item" | "function_signature_item" => render_function(node, src, focus, out),
        "struct_item" | "enum_item" | "union_item" | "trait_item" | "type_item" | "const_item"
        | "static_item" | "mod_item" => render_container(node, src, focus, out),
        "impl_item" => {
            push_line(out, &header_without_body(node, src));
            if let Some(body) = node.child_by_field_name("body") {
                render_children(body, src, focus, out);
            }
        }
        "inner_attribute_item" | "attribute_item" => {}
        other if node.child_count() > 0 && !other.ends_with("_item") => {
            render_children(node, src, focus, out);
        }
        _ => {}
    }
}

fn render_function(node: Node, src: &str, focus: &HashSet<String>, out: &mut String) {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, src).to_string())
        .unwrap_or_default();
    if !name.is_empty() && focus.contains(&name) {
        push_block(out, node_text(node, src));
    } else {
        push_line(out, &signature_line(node, src));
    }
}

fn render_container(node: Node, src: &str, focus: &HashSet<String>, out: &mut String) {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(n, src).to_string())
        .unwrap_or_default();
    let keep_body = !name.is_empty() && focus.contains(&name);
    if keep_body {
        push_block(out, node_text(node, src));
        return;
    }
    if node.kind() == "mod_item" {
        if let Some(body) = node.child_by_field_name("body") {
            push_line(out, &header_without_body(node, src));
            render_children(body, src, focus, out);
            return;
        }
    }
    push_line(out, &signature_line(node, src));
}

fn signature_line(node: Node, src: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte());
    one_line(&src[node.start_byte()..end.min(src.len())])
}

fn header_without_body(node: Node, src: &str) -> String {
    signature_line(node, src)
}

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.start_byte().min(src.len())..node.end_byte().min(src.len())]
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn push_line(out: &mut String, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn push_block(out: &mut String, block: &str) {
    let block = block.trim();
    if block.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(block);
}

fn fold_generic(source: &str, focus: &HashSet<String>) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_error_line(line) {
            out.push(line.trim_end().to_string());
            i += 1;
            continue;
        }
        if let Some(name) = signature_name(line) {
            let indent = indent_of(line);
            if focus.contains(&name) {
                out.push(line.trim_end().to_string());
                i += 1;
                while i < lines.len() && keeps_body_line(lines[i], indent) {
                    out.push(lines[i].trim_end().to_string());
                    i += 1;
                }
            } else {
                out.push(one_line(line));
                i += 1;
                while i < lines.len() && keeps_body_line(lines[i], indent) {
                    i += 1;
                }
            }
            continue;
        }
        i += 1;
    }
    if out.is_empty() {
        return head_tail(source);
    }
    out.join("\n")
}

fn keeps_body_line(line: &str, header_indent: usize) -> bool {
    line.trim().is_empty() || indent_of(line) > header_indent
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error") || lower.contains("panic")
}

fn signature_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
        return None;
    }
    const MARKS: &[&str] = &[
        "fn ",
        "def ",
        "class ",
        "struct ",
        "enum ",
        "interface ",
        "trait ",
        "type ",
        "func ",
        "function ",
        "impl ",
    ];
    let rest = MARKS
        .iter()
        .find_map(|mark| trimmed.find(mark).map(|idx| &trimmed[idx + mark.len()..]))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.len() < 2 {
        None
    } else {
        Some(name)
    }
}

fn head_tail(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    if lines.len() <= 18 {
        return source.to_string();
    }
    let mut out = lines[..8].join("\n");
    out.push_str("\n...");
    out.push('\n');
    out.push_str(&lines[lines.len() - 10..].join("\n"));
    out
}

pub fn collect_idents(text: &str, into: &mut HashSet<String>) {
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            push_ident(&cur, into);
            cur.clear();
        }
    }
    push_ident(&cur, into);
}

fn push_ident(cur: &str, into: &mut HashSet<String>) {
    if cur.len() < 3 || cur.chars().all(|c| c.is_ascii_digit()) {
        return;
    }
    const SKIP: &[&str] = &[
        "the", "and", "for", "fn", "let", "pub", "use", "mod", "error", "Error", "failed", "panic",
        "true", "false", "self", "Self", "mut", "impl",
    ];
    if SKIP.contains(&cur) {
        return;
    }
    into.insert(cur.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_outline_keeps_named_function_and_folds_the_rest() {
        let src = r#"
use std::io::Read;

fn keep_me() {
    let marker = 1;
    println!("{marker}");
}

fn drop_me() {
    let secret = "SHOULD_FOLD";
    println!("{secret}");
}
"#;
        let focus = HashSet::from(["keep_me".to_string()]);
        let out = fold_source("src/lib.rs", src, &focus).unwrap();
        assert!(out.contains("use std::io::Read"), "{out}");
        assert!(out.contains("fn keep_me"), "{out}");
        assert!(out.contains("marker"), "{out}");
        assert!(out.contains("fn drop_me"), "{out}");
        assert!(!out.contains("SHOULD_FOLD"), "{out}");
    }

    #[test]
    fn generic_outline_keeps_the_named_block() {
        let src = "def keep_me():\n    return 1\n\ndef drop_me():\n    secret = 'SHOULD_FOLD'\n    return secret\n";
        let focus = HashSet::from(["keep_me".to_string()]);
        let out = fold_source("app.py", src, &focus).unwrap();
        assert!(out.contains("return 1"), "{out}");
        assert!(out.contains("def drop_me"), "{out}");
        assert!(!out.contains("SHOULD_FOLD"), "{out}");
    }
}
