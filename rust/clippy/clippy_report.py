#!/usr/bin/env python3
"""Report clippy errors from materialized diagnostics JSON files.

Reads `<label> <path>` lines on stdin (as printed by the
`rust_clippy.bxl` BXL), scans each JSON for error-level diagnostics,
and:

- exits 1 when any error is found (this is the CI gate), else 0;
- prints one `::error file=...,line=...::` workflow annotation per
  error diagnostic, so the failure shows on the exact code lines in
  both PR diffs and push commit pages -- no API access needed;
- posts inline PR review comments for the same diagnostics when a PR
  context is available (a `pull_request` event, or exactly one open PR
  associated with the pushed SHA) and `gh` + a token are present.
  Commenting is best-effort: failures warn but never fail the step.

Diagnostics without spans (e.g. "aborting due to N previous errors")
get a plain annotation and no comment.
"""

import json
import os
import shutil
import subprocess
import sys

MARKER = "buckshot-rust-clippy"


def workflow_escape(text):
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def read_pairs():
    pairs = []
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        label, _, path = line.rpartition(" ")
        if label and path:
            pairs.append((label, path))
    return pairs


def error_diagnostics(json_path):
    """Yield {code, message, path, line, end_line} for error diagnostics."""
    found = []
    try:
        with open(json_path, encoding="utf-8") as handle:
            for raw in handle:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    diag = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                if not isinstance(diag, dict) or diag.get("level") != "error":
                    continue
                code = (diag.get("code") or {}).get("code")
                message = diag.get("message", "")
                spans = diag.get("spans") or []
                primary = next(
                    (s for s in spans if s.get("is_primary")), spans[:1]
                )
                entry = {"code": code, "message": message}
                if primary:
                    entry["path"] = primary.get("file_name")
                    entry["line"] = primary.get("line_start")
                    entry["end_line"] = primary.get("line_end")
                found.append(entry)
    except OSError as exc:
        print(f"::warning::could not read clippy JSON {json_path}: {exc}")
    return found


def annotate(error):
    path = error.get("path")
    line = error.get("line")
    code = error.get("code") or "clippy"
    message = error.get("message", "")
    if path and line:
        end = error.get("end_line") or line
        print(
            f"::error file={path},line={line},endLine={end},"
            f"title={workflow_escape(code)}::{workflow_escape(message)}"
        )
    else:
        print(f"::error title={workflow_escape(code)}::{workflow_escape(message)}")


def gh_api(*args):
    """Run `gh api ...`, returning parsed JSON or None on any failure."""
    if not shutil.which("gh"):
        return None
    try:
        proc = subprocess.run(
            ["gh", "api", *args],
            capture_output=True,
            text=True,
            timeout=60,
            env={**os.environ, "GH_TOKEN": os.environ.get("GH_TOKEN", "")},
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout or "null")
    except json.JSONDecodeError:
        return proc.stdout


def resolve_pr(repo):
    """Return (pr_number, head_sha) or (None, None)."""
    event = os.environ.get("GH_EVENT_NAME", "")
    if event == "pull_request":
        number = os.environ.get("GH_PR_NUMBER", "")
        sha = os.environ.get("GH_HEAD_SHA", "")
        if number and sha:
            return number, sha
        return None, None
    sha = os.environ.get("GH_HEAD_SHA", "")
    if not sha or not shutil.which("gh"):
        return None, None
    pulls = gh_api(
        f"repos/{repo}/commits/{sha}/pulls",
        "--jq",
        "[.[] | select(.state == \"open\") | .number]",
    )
    if isinstance(pulls, list) and len(pulls) == 1:
        return str(pulls[0]), sha
    return None, None


def comment_body(error, run_url):
    code = error.get("code") or "clippy"
    message = error.get("message", "")
    body = f"**`{code}`** — {message}\n\n<sub>Reported by buckshot `rust-clippy`"
    if run_url:
        body += f" · [run]({run_url})"
    body += f"</sub>\n<!-- {MARKER}:{code} -->"
    return body


def already_commented(repo, pr, error):
    comments = gh_api(
        f"repos/{repo}/pulls/{pr}/comments",
        "--paginate",
        "--jq",
        "[.[] | {path: .path, line: .line, body: .body}]",
    )
    if not isinstance(comments, list):
        return False
    marker = f"<!-- {MARKER}:{error.get('code') or 'clippy'} -->"
    return any(
        c.get("path") == error.get("path")
        and c.get("line") == error.get("line")
        and marker in (c.get("body") or "")
        for c in comments
    )


def post_comment(repo, pr, sha, error, run_url):
    args = [
        f"repos/{repo}/pulls/{pr}/comments",
        "-f",
        f"commit_id={sha}",
        "-f",
        f"path={error['path']}",
        "-f",
        f"line={error['line']}",
        "-f",
        "side=RIGHT",
        "-f",
        "subject_type=line",
        "-f",
        f"body={comment_body(error, run_url)}",
    ]
    if error.get("end_line") and error["end_line"] != error["line"]:
        args += ["-f", f"start_line={error['line']}", "-f", "start_side=RIGHT"]
    if not shutil.which("gh"):
        return False
    try:
        proc = subprocess.run(
            ["gh", "api", *args],
            capture_output=True,
            text=True,
            timeout=60,
            env={**os.environ, "GH_TOKEN": os.environ.get("GH_TOKEN", "")},
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"::warning::could not comment on PR #{pr}: {exc}")
        return False
    if proc.returncode != 0:
        detail = (proc.stderr or "").strip().splitlines()
        tail = detail[-1] if detail else f"exit {proc.returncode}"
        print(
            f"::warning::could not comment on {error['path']}:{error['line']} "
            f"of PR #{pr} ({tail}); is the line in the PR diff and does the "
            "token have pull-requests: write?"
        )
        return False
    return True


def main():
    pairs = read_pairs()
    errors = []
    for _label, path in pairs:
        errors.extend(error_diagnostics(path))
    for error in errors:
        annotate(error)
    if not errors:
        return 0

    print(f"clippy failed with {len(errors)} error(s)")
    if os.environ.get("INLINE_COMMENTS", "true") != "true":
        return 1
    repo = os.environ.get("GH_REPOSITORY", "")
    if not repo:
        return 1
    pr, sha = resolve_pr(repo)
    if not pr:
        print(
            "no PR context for inline comments (push without an associated "
            "open PR): annotations above still mark the exact lines"
        )
        return 1
    run_url = os.environ.get("GH_RUN_URL", "")
    for error in errors:
        if not error.get("path") or not error.get("line"):
            continue
        if already_commented(repo, pr, error):
            continue
        post_comment(repo, pr, sha, error, run_url)
    return 1


if __name__ == "__main__":
    sys.exit(main())
