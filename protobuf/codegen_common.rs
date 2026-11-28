//! Shared CLI surface + `lib.rs` assembly for the proto/tonic codegen binaries.
//!
//! Both binaries take the same inputs (protoc path, extern-path mappings,
//! the sources to compile, and the transitive source closure for import
//! resolution) and emit the same single-`lib.rs` shape; only the compile
//! step differs (prost messages vs. prost messages + tonic server), which
//! stays in each binary.

use std::ffi::OsStr;
use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

/// CLI inputs shared by both codegen binaries; each binary flattens this
/// into its own `Args` (tonic adds `--client`).
#[derive(clap::Args, Debug)]
pub struct CommonArgs {
    /// Output path for the generated `lib.rs`.
    #[arg(long)]
    pub out: PathBuf,

    /// Path to the `protoc` binary to use. Required: without it,
    /// prost-build falls back to an ambient `protoc` on `PATH`, which a
    /// hermetic build must never depend on.
    #[arg(long)]
    pub protoc: PathBuf,

    /// Cross-crate type mapping, `proto.path=::rust_crate`. Pass once per
    /// external proto package referenced by the compiled sources (prost
    /// `extern_path`): references are emitted as absolute paths into that
    /// crate, which must be a dependency of the crate this `lib.rs`
    /// becomes. Well-known `google.protobuf.*` types need no entry; they
    /// map to `prost-types` automatically.
    #[arg(long = "extern-path")]
    pub extern_paths: Vec<String>,

    /// Every proto file visible for import resolution: the target's own
    /// sources plus every transitive dep's sources. `resolve_proto_paths`
    /// matches each compiled source's `import "..."` statements against
    /// these to infer protoc's `--proto-path` roots, so no import root is
    /// ever spelled as a repo-relative string (which breaks whenever this
    /// cell is nested inside a superproject or renamed).
    #[arg(long = "available-src")]
    pub available_srcs: Vec<PathBuf>,

    /// Proto sources to compile.
    pub protos: Vec<PathBuf>,
}

/// Apply `--extern-path` mappings to a prost config.
pub fn apply_extern_paths(config: &mut prost_build::Config, extern_paths: &[String]) -> Result<()> {
    for mapping in extern_paths {
        let (proto_path, rust_path) = mapping
            .split_once('=')
            .with_context(|| format!("--extern-path must be proto.path=::rust_crate, got {mapping:?}"))?;
        config.extern_path(proto_path, rust_path);
    }
    Ok(())
}

/// Resolve the protoc `--proto-path` roots for `protos` from `available`.
///
/// Each `import "a/b/c.proto"` in a direct source must suffix-match exactly
/// one available file; the root is that file's path with the import suffix
/// stripped. Paths pass through exactly as Buck materialized them (relative
/// to the action's working directory, which Buck pins to the project root),
/// so this is correct in any cell layout without any configured root.
pub fn resolve_proto_paths(protos: &[PathBuf], available: &[PathBuf]) -> Result<Vec<PathBuf>> {
    // Dedupe: diamond dep closures list shared files (e.g. well-known
    // types) more than once, which must not read as ambiguity below.
    let mut candidates: Vec<String> = Vec::new();
    for cand in available {
        let s = candidate_string(cand)?;
        if !candidates.contains(&s) {
            candidates.push(s);
        }
    }

    let mut roots: Vec<String> = Vec::new();
    for proto in protos {
        let text = fs::read_to_string(proto)
            .with_context(|| format!("reading {}", proto.display()))?;
        for import in parse_imports(&text) {
            let mut hits = Vec::new();
            for cand in &candidates {
                if let Some(root) = strip_import_suffix(cand, &import) {
                    hits.push(root);
                }
            }
            match hits.as_slice() {
                [] => bail!(
                    "{}: import \"{}\" matches no file in srcs or deps; add the target providing it to deps (or include_deps for an import-only layer)",
                    proto.display(),
                    import,
                ),
                [root] => {
                    if !roots.contains(root) {
                        roots.push(root.clone());
                    }
                }
                many => bail!(
                    "{}: import \"{}\" ambiguously matches {} files ({}); the import closure must provide each path once",
                    proto.display(),
                    import,
                    many.len(),
                    many.join(", "),
                ),
            }
        }
    }
    roots.sort();
    Ok(roots.into_iter().map(PathBuf::from).collect())
}

/// Normalize a candidate file path for suffix matching.
fn candidate_string(path: &Path) -> Result<String> {
    let s = path
        .to_str()
        .with_context(|| format!("non-UTF-8 path: {}", path.display()))?;
    Ok(s.strip_prefix("./").unwrap_or(s).to_string())
}

/// Strip `import` from the end of `candidate`, returning the directory
/// prefix (the protoc `-I` root). The match requires a `/` boundary, so
/// `notgoogle/api/x.proto` never matches `google/api/x.proto`.
fn strip_import_suffix(candidate: &str, import: &str) -> Option<String> {
    if candidate == import {
        return Some(".".to_string());
    }
    let prefix = candidate.strip_suffix(import)?;
    let dir = prefix.strip_suffix('/')?;
    if dir.is_empty() {
        return Some("/".to_string());
    }
    Some(dir.to_string())
}

/// All `import "..."` paths in proto `text`, ignoring comments. Handles the
/// `import public` / `import weak` modifiers the same as plain imports.
fn parse_imports(text: &str) -> Vec<String> {
    let clean = strip_comments(text);
    let bytes = clean.as_bytes();
    let mut imports = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if &clean[start..i] == "import" {
                i = skip_ws_and_words(&clean, bytes, i, &["public", "weak"]);
                if i < bytes.len() && bytes[i] == b'"' {
                    i += 1;
                    let mut lit = String::new();
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            lit.push(bytes[i + 1] as char);
                            i += 2;
                        } else {
                            lit.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    // Skip rather than error on malformed lines; protoc
                    // still reports the real syntax error downstream.
                    if i < bytes.len() {
                        imports.push(lit);
                        i += 1;
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    imports
}

/// Advance past ASCII whitespace and any of `words` (whole words only).
fn skip_ws_and_words(text: &str, bytes: &[u8], mut i: usize, words: &[&str]) -> usize {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut advanced = false;
        for word in words {
            if text[i..].starts_with(word)
                && text[i + word.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
            {
                i += word.len();
                advanced = true;
                break;
            }
        }
        if !advanced {
            return i;
        }
    }
}

/// Strip `//` and `/* */` comments, preserving string contents (so a `//`
/// inside a string literal isn't treated as a comment).
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            out.push(c);
            while let Some(c) = chars.next() {
                out.push(c);
                if c == '\\' {
                    if let Some(e) = chars.next() {
                        out.push(e);
                    }
                } else if c == '"' {
                    break;
                }
            }
        } else if c == '/' {
            match chars.peek() {
                Some('/') => {
                    while let Some(c) = chars.next() {
                        if c == '\n' {
                            out.push(c);
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev_star = false;
                    while let Some(c) = chars.next() {
                        if prev_star && c == '/' {
                            break;
                        }
                        prev_star = c == '*';
                    }
                    out.push(' ');
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Assemble the single self-contained `lib.rs` from the `<package>.rs`
/// files a codegen run left in `dir`, inlined at the top level so
/// consumers see types as `my_proto::Pose2d` rather than
/// `my_proto::my::pkg::Pose2d`.
pub fn assemble_lib(dir: &Path, generator: &str) -> Result<String> {
    let mut lib = String::new();
    writeln!(lib, "//! Generated by buckshot's {generator} — do not edit.")?;
    writeln!(lib, "#![allow(dead_code, clippy::all)]")?;
    writeln!(lib)?;

    let mut packages: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() == Some(OsStr::new("rs"))
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            packages.push(stem.to_string());
        }
    }
    packages.sort();

    for pkg in &packages {
        let pkg_path = dir.join(format!("{pkg}.rs"));
        let pkg_src = fs::read_to_string(&pkg_path)
            .with_context(|| format!("reading {generator} output {}", pkg_path.display()))?;
        writeln!(lib, "// --- {pkg} ---")?;
        lib.push_str(&pkg_src);
        writeln!(lib)?;
    }
    Ok(lib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_ignore_comments_and_handle_modifiers() {
        let text = "// import \"commented.proto\";\n\
            import \"real.proto\"; // trailing comment\n\
            /* import \"blocked.proto\"; */\n\
            /* multi\n\
               import \"spanning.proto\";\n\
               line */\n\
            import public \"pub.proto\";\n\
            import weak \"weak.proto\";\n\
            // For example `import google.cloud.library_v2` prose.\n";
        assert_eq!(
            parse_imports(text),
            vec!["real.proto", "pub.proto", "weak.proto"],
        );
    }

    #[test]
    fn suffix_match_requires_component_boundary() {
        assert_eq!(
            strip_import_suffix("protos/google/api/x.proto", "google/api/x.proto"),
            Some("protos".to_string()),
        );
        assert_eq!(
            strip_import_suffix("protos/notgoogle/api/x.proto", "google/api/x.proto"),
            None,
        );
        assert_eq!(
            strip_import_suffix("x.proto", "x.proto"),
            Some(".".to_string()),
        );
        assert_eq!(strip_import_suffix("other/y.proto", "x.proto"), None);
    }

    #[test]
    fn resolves_roots_from_closure() {
        let dir = tempfile::tempdir().unwrap();
        let proto = dir.path().join("operations.proto");
        std::fs::write(
            &proto,
            "syntax = \"proto3\";\nimport \"google/api/a.proto\";\nimport \"google/rpc/s.proto\";\n",
        )
        .unwrap();
        let roots = resolve_proto_paths(
            &[proto],
            &[
                PathBuf::from("some/cell/protos/google/api/a.proto"),
                PathBuf::from("some/cell/protos/google/rpc/s.proto"),
                PathBuf::from("some/cell/protos/google/rpc/unused.proto"),
            ],
        )
        .unwrap();
        assert_eq!(roots, vec![PathBuf::from("some/cell/protos")]);
    }

    #[test]
    fn no_imports_needs_no_roots() {
        let dir = tempfile::tempdir().unwrap();
        let proto = dir.path().join("plain.proto");
        std::fs::write(&proto, "syntax = \"proto3\";\nmessage M {}\n").unwrap();
        let roots = resolve_proto_paths(&[proto], &[PathBuf::from("anywhere/plain.proto")]).unwrap();
        assert!(roots.is_empty());
    }

    #[test]
    fn missing_import_names_the_path_and_fix() {
        let dir = tempfile::tempdir().unwrap();
        let proto = dir.path().join("a.proto");
        std::fs::write(&proto, "syntax = \"proto3\";\nimport \"google/api/a.proto\";\n").unwrap();
        let err = resolve_proto_paths(&[proto], &[PathBuf::from("elsewhere/b.proto")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("google/api/a.proto"), "{err}");
        assert!(err.contains("deps"), "{err}");
    }

    #[test]
    fn ambiguous_import_fails() {
        let dir = tempfile::tempdir().unwrap();
        let proto = dir.path().join("a.proto");
        std::fs::write(&proto, "syntax = \"proto3\";\nimport \"google/api/a.proto\";\n").unwrap();
        let err = resolve_proto_paths(
            &[proto],
            &[
                PathBuf::from("one/protos/google/api/a.proto"),
                PathBuf::from("two/protos/google/api/a.proto"),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ambiguously"), "{err}");
    }
}
