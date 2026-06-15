use super::reply::{MAX_REPORT_LINES, close_truncated_json};
use super::*;
use crate::openrouter::{FunctionCall, Usage};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// A scripted `Chat` backend: each `chat` call pops the next canned reply.
/// Lets the agent loop be exercised end-to-end with zero network access —
/// no request ever leaves the process.
struct FakeChat {
    replies: VecDeque<Message>,
    usage: Usage,
}

impl FakeChat {
    fn new(replies: Vec<Message>) -> Self {
        Self {
            replies: replies.into(),
            usage: Usage::default(),
        }
    }
}

impl Chat for FakeChat {
    async fn chat(
        &mut self,
        _agent: &str,
        _model: &str,
        _messages: &[Message],
        _tools: &[Value],
        _max_tokens: Option<u32>,
    ) -> Result<Message> {
        Ok(self
            .replies
            .pop_front()
            .expect("FakeChat ran out of scripted replies"))
    }
    fn usage(&self) -> &Usage {
        &self.usage
    }
    fn last_finish_reason(&self) -> Option<&str> {
        None
    }
}

fn tool_call_msg(id: &str, name: &str, arguments: &str) -> Message {
    Message {
        role: "assistant".into(),
        content: None,
        tool_calls: Some(vec![ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }]),
        tool_call_id: None,
    }
}

fn orchestrator(client: FakeChat) -> Orchestrator<FakeChat> {
    let root = std::env::temp_dir().join(format!("scrooge-fake-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    build_orchestrator(root, client)
}

fn build_orchestrator(root: PathBuf, client: FakeChat) -> Orchestrator<FakeChat> {
    Orchestrator {
        client,
        toolbox: Toolbox::new(root),
        cheap_model: "fake-cheap".into(),
        sota_model: "fake-sota".into(),
        max_turns: 20,
    }
}

/// A clean, per-test temp directory (unique by test tag) with a `.scrooge`
/// dir ready. Not a git repo — call `git_init` when the flow needs one.
fn fresh_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("scrooge-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".scrooge")).unwrap();
    root
}

fn git_init(root: &Path) {
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "git init failed in test setup");
}

/// Stage everything and commit, with an inline identity so the test doesn't
/// depend on the machine's git config.
fn git_commit_all(root: &Path) {
    let status = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .is_ok_and(|s| s.success())
    };
    assert!(status(&["add", "-A"]), "git add failed");
    assert!(
        status(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ]),
        "git commit failed"
    );
}

/// Write a `.scrooge/checks.toml` with a single synthetic language whose
/// commands are plain shell, so checks pass/fail deterministically without a
/// real toolchain.
fn seed_checks(root: &Path, quick: &str, test: &str) {
    std::fs::write(
        root.join(".scrooge").join("checks.toml"),
        format!("[synthetic]\nquick = {quick:?}\ntest = {test:?}\n"),
    )
    .unwrap();
}

/// Pre-seed the overview so `ensure_overview` loads it instead of spending a
/// (scripted) model call to write one.
fn seed_overview(root: &Path) {
    std::fs::write(root.join(".scrooge").join("overview.md"), "A test project.").unwrap();
}

fn delegate_call(id: &str, instructions: &str) -> Message {
    tool_call_msg(
        id,
        "delegate_to_cratchit",
        &format!("{{\"instructions\": {}}}", serde_json::json!(instructions)),
    )
}

/// The shared Cratchit loop dispatches a scripted tool call against the real
/// (deterministic, offline) toolbox, then returns the model's final text —
/// all from spoofed replies, proving the loop never touches the network.
#[tokio::test]
async fn tool_loop_dispatches_then_returns_final_text() {
    // Reply 1: ask for a free, offline call-graph lookup. Reply 2: the
    // final report once the tool result is in hand.
    let mut orch = orchestrator(FakeChat::new(vec![
        tool_call_msg("c1", "callers", r#"{"name": "nonexistent_fn"}"#),
        Message::text("assistant", "done: examined the call graph"),
    ]));
    let defs = tools::definitions();
    let mut log = vec![Message::text("user", "investigate")];
    let out = orch.tool_loop(&mut log, &defs, 40).await.unwrap();
    assert_eq!(out.as_deref(), Some("done: examined the call graph"));
    // The tool result for the dispatched call landed back in the log.
    assert!(
        log.iter().any(|m| m.tool_call_id.as_deref() == Some("c1")),
        "expected a tool result for the dispatched call in the log"
    );
}

/// When the model never asks for a tool, the loop returns its text on the
/// first turn without spending any of the iteration budget on tools.
#[tokio::test]
async fn tool_loop_returns_immediately_without_tool_calls() {
    let mut orch = orchestrator(FakeChat::new(vec![Message::text(
        "assistant",
        "no tools needed",
    )]));
    let mut log = vec![Message::text("user", "answer directly")];
    let out = orch.tool_loop(&mut log, &[], 40).await.unwrap();
    assert_eq!(out.as_deref(), Some("no tools needed"));
}

#[test]
fn parse_tool_args_recovers_truncated_instruction() {
    // Well-formed args parse regardless of the truncation flag.
    let v = parse_tool_args(r#"{"instructions": "edit foo.rs"}"#, false);
    assert_eq!(v["instructions"], "edit foo.rs");

    // Cut mid-string (the output cap's typical landing spot): with the
    // truncation flag set, the surviving prefix is kept.
    let v = parse_tool_args(
        r#"{"instructions": "edit foo.rs and rename bar to ba"#,
        true,
    );
    assert_eq!(v["instructions"], "edit foo.rs and rename bar to ba");

    // Cut right after an escaped quote inside the string.
    let v = parse_tool_args(r#"{"instructions": "set the flag to \"on"#, true);
    assert_eq!(v["instructions"], "set the flag to \"on");

    // Cut between fields, leaving a dangling comma.
    let v = parse_tool_args(r#"{"instructions": "do it","#, true);
    assert_eq!(v["instructions"], "do it");

    // Unrecoverable garbage falls back to Null rather than panicking.
    assert!(parse_tool_args("{not json at all", true).is_null());

    // Without the truncation signal, broken JSON is NOT repaired — a genuine
    // malformed call returns Null instead of a guessed (partial) value.
    assert!(parse_tool_args(r#"{"instructions": "edit foo.rs and rename"#, false).is_null());
}

#[test]
fn clamp_report_caps_lines_and_marks_truncation() {
    let many = (0..30)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = clamp_report(&many);
    assert!(out.contains("[report clamped]"));
    // The capped body keeps MAX_REPORT_LINES, plus the one marker line.
    assert_eq!(out.lines().count(), MAX_REPORT_LINES + 1);
    // A short report passes through untouched.
    assert_eq!(clamp_report("a\nb"), "a\nb");
}

/// The full planning loop, driven entirely by scripted replies: Scrooge ends
/// his turn with no tool call and, since nothing was changed on disk, the run
/// finishes immediately without a single check ever running.
#[tokio::test]
async fn run_task_finishes_when_nothing_changed() {
    let root = fresh_root("noop");
    git_init(&root);
    seed_overview(&root);
    let mut orch = build_orchestrator(
        root,
        FakeChat::new(vec![Message::text("assistant", "DONE")]),
    );
    let out = orch.run_task("do nothing").await.unwrap();
    // One Scrooge turn (the immediate DONE), no work done.
    assert!(out.contains("1 turn"), "unexpected: {out}");
}

/// One delegation, then DONE: the delegation is counted, Cratchit's text
/// report flows back (he changed nothing, so no CHECKS verdict is owed), and
/// the loop accepts completion.
#[tokio::test]
async fn run_task_runs_one_delegation_then_finishes() {
    let root = fresh_root("delegate");
    git_init(&root);
    seed_overview(&root);
    let mut orch = build_orchestrator(
        root,
        FakeChat::new(vec![
            delegate_call("d1", "investigate foo, change nothing"),
            Message::text("assistant", "looked at foo; nothing to change"),
            Message::text("assistant", "DONE"),
        ]),
    );
    let out = orch.run_task("inspect foo").await.unwrap();
    // Two Scrooge turns: the delegation, then the DONE.
    assert!(out.contains("2 turn"), "unexpected: {out}");
}

/// The code-changing entry points refuse a non-git root rather than silently
/// losing their change-detection (and the read-only/CHECKS contract with it).
#[tokio::test]
async fn run_task_requires_a_git_repo() {
    let root = fresh_root("nogit");
    let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
    let err = orch.run_task("anything").await.unwrap_err();
    assert!(format!("{err:#}").contains("git"), "unexpected: {err:#}");
}

/// Scrooge gets at most one delegation per turn: a second call in the same
/// turn is rejected and does not advance the step counter.
#[tokio::test]
async fn second_delegation_in_one_turn_is_rejected() {
    let root = fresh_root("oneper");
    git_init(&root);
    let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
    let mut st = RunState::default();
    let mut delegated = true; // pretend a delegation already happened this turn
    let call = ToolCall {
        id: "x".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "delegate_to_cratchit".into(),
            arguments: r#"{"instructions": "do x"}"#.into(),
        },
    };
    let out = orch
        .run_scrooge_call("task", &call, false, &mut delegated, &mut st)
        .await
        .unwrap();
    assert!(out.contains("only one delegate"), "unexpected: {out}");
}

/// An empty/unrepairable `instructions` is bounced without burning a
/// delegation or briefing Cratchit with nothing.
#[tokio::test]
async fn empty_instructions_are_bounced() {
    let root = fresh_root("empty");
    git_init(&root);
    let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
    let mut st = RunState::default();
    let mut delegated = false;
    let call = ToolCall {
        id: "x".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "delegate_to_cratchit".into(),
            arguments: r#"{"instructions": "   "}"#.into(),
        },
    };
    let out = orch
        .run_scrooge_call("task", &call, false, &mut delegated, &mut st)
        .await
        .unwrap();
    assert!(out.contains("non-empty"), "unexpected: {out}");
    assert!(
        !delegated,
        "a bounced call must not consume the turn's delegation"
    );
}

/// `web_answer` is refused once the per-task budget is spent — and the refusal
/// is decided locally, without dispatching to the toolbox (no network).
#[tokio::test]
async fn web_answer_budget_is_enforced() {
    let root = fresh_root("web");
    git_init(&root);
    let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
    let mut st = RunState {
        web_calls: SCROOGE_WEB_LOOKUPS,
        ..RunState::default()
    };
    let mut delegated = false;
    let call = ToolCall {
        id: "w".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "web_answer".into(),
            arguments: r#"{"query": "anything"}"#.into(),
        },
    };
    let out = orch
        .run_scrooge_call("task", &call, false, &mut delegated, &mut st)
        .await
        .unwrap();
    assert!(out.contains("budget exhausted"), "unexpected: {out}");
}

fn write_file_call(id: &str, path: &str, content: &str) -> Message {
    tool_call_msg(
        id,
        "write_file",
        &format!(
            "{{\"path\": {}, \"content\": {}}}",
            serde_json::json!(path),
            serde_json::json!(content)
        ),
    )
}

/// A code-changing dispatch whose quick check fails: the failure is routed
/// back to Cratchit `CHECK_RETRIES` times, then reported to Scrooge as a
/// FAILING verdict rather than spinning forever.
#[tokio::test]
async fn execute_and_verify_escalates_after_check_retries() {
    let root = fresh_root("checkretry");
    git_init(&root);
    seed_overview(&root);
    seed_checks(&root, "exit 1", "exit 1");
    // Reply 1+2: Cratchit writes a file then reports. Replies 3+4: the two
    // retries (the quick check fails every time, so it never goes green).
    let mut orch = build_orchestrator(
        root,
        FakeChat::new(vec![
            write_file_call("w", "f.txt", "x"),
            Message::text("assistant", "wrote f.txt"),
            Message::text("assistant", "still failing 1"),
            Message::text("assistant", "still failing 2"),
        ]),
    );
    let (report, verdict) = orch
        .execute_and_verify("t", "create f.txt", None, CheckTier::Quick)
        .await
        .unwrap();
    assert_eq!(verdict, Some(false));
    assert!(report.contains("CHECKS: FAILING"), "unexpected: {report}");
}

/// A code-changing dispatch whose quick check passes ends with a clean
/// verdict on the first attempt — no retry loop.
#[tokio::test]
async fn execute_and_verify_reports_clean_when_check_passes() {
    let root = fresh_root("checkclean");
    git_init(&root);
    seed_overview(&root);
    seed_checks(&root, "true", "true");
    let mut orch = build_orchestrator(
        root,
        FakeChat::new(vec![
            write_file_call("w", "f.txt", "x"),
            Message::text("assistant", "wrote f.txt"),
        ]),
    );
    let (report, verdict) = orch
        .execute_and_verify("t", "create f.txt", None, CheckTier::Quick)
        .await
        .unwrap();
    assert_eq!(verdict, Some(true));
    assert!(report.contains("CHECKS: clean"), "unexpected: {report}");
}

/// A read-only dispatch (Cratchit changes nothing on disk) returns his
/// findings with no CHECKS line and a None verdict — the checks are skipped.
#[tokio::test]
async fn execute_and_verify_skips_checks_when_nothing_changed() {
    let root = fresh_root("nochange");
    git_init(&root);
    seed_overview(&root);
    seed_checks(&root, "exit 1", "exit 1"); // would fail if it ever ran
    let mut orch = build_orchestrator(
        root,
        FakeChat::new(vec![Message::text(
            "assistant",
            "investigated; nothing to change",
        )]),
    );
    let (report, verdict) = orch
        .execute_and_verify("t", "just look", None, CheckTier::Quick)
        .await
        .unwrap();
    assert_eq!(verdict, None);
    assert!(!report.contains("CHECKS"), "unexpected: {report}");
}

/// `try_finish` keeps nudging Scrooge to fix red checks until the attempt
/// budget is spent, then gives up rather than looping.
#[tokio::test]
async fn try_finish_nudges_then_gives_up_on_red_checks() {
    let root = fresh_root("finishred");
    git_init(&root);
    let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
    let structure_before: Vec<String> = vec![];

    // First call (attempts remaining): nudge, don't finish.
    let mut log = Vec::new();
    let mut st = RunState {
        dirty: true,
        checks_clean: Some(false),
        finish_attempts: 0,
        ..RunState::default()
    };
    let out = orch
        .try_finish("t", &mut log, &mut st, &structure_before)
        .await
        .unwrap();
    assert!(out.is_none(), "should not finish while checks are red");
    assert_eq!(st.finish_attempts, 1);
    assert_eq!(
        log.last().and_then(|m| m.content.as_deref()),
        Some(SCROOGE_DONE_WITH_FAILURES)
    );

    // Last call (budget spent): give up with a terminal message.
    st.finish_attempts = MAX_FINISH_ATTEMPTS;
    let out = orch
        .try_finish("t", &mut log, &mut st, &structure_before)
        .await
        .unwrap();
    assert_eq!(
        out.as_deref(),
        Some("checks still failing; review the output above")
    );
}

/// With nothing changed, `try_finish` accepts completion immediately without
/// running any checks.
#[tokio::test]
async fn try_finish_accepts_when_not_dirty() {
    let root = fresh_root("finishclean");
    git_init(&root);
    let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
    let mut log = Vec::new();
    let mut st = RunState::default(); // dirty: false
    let out = orch.try_finish("t", &mut log, &mut st, &[]).await.unwrap();
    assert!(
        out.unwrap().contains("turn"),
        "expected a completion banner"
    );
}

/// The full suite gates DONE: green quick checks plus a passing full suite
/// accept completion (structure unchanged here, so no overview pass).
#[tokio::test]
async fn try_finish_accepts_when_full_suite_clean() {
    let root = fresh_root("finishfull");
    git_init(&root);
    seed_overview(&root);
    seed_checks(&root, "true", "true");
    let structure_before = codemap::build_cached(&root).unwrap().structure_signature();
    let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
    let mut log = Vec::new();
    let mut st = RunState {
        dirty: true,
        checks_clean: Some(true),
        ..RunState::default()
    };
    let out = orch
        .try_finish("t", &mut log, &mut st, &structure_before)
        .await
        .unwrap();
    assert!(
        out.unwrap().contains("turn"),
        "expected a completion banner"
    );
}

/// Two edits with an identical `git diff --stat` (one changed line each)
/// still yield distinct fingerprints — the bug the hash fixes — and a revert
/// reproduces the clean fingerprint exactly.
#[test]
fn worktree_fingerprint_distinguishes_same_stat_edits() {
    let root = fresh_root("fingerprint");
    git_init(&root);
    std::fs::write(root.join("a.txt"), "a\nb\nc\n").unwrap();
    git_commit_all(&root);
    let clean = worktree_fingerprint(&root);
    assert!(clean.is_some(), "fingerprint needs a git repo");

    std::fs::write(root.join("a.txt"), "X\nb\nc\n").unwrap();
    let fp1 = worktree_fingerprint(&root);
    std::fs::write(root.join("a.txt"), "Y\nb\nc\n").unwrap();
    let fp2 = worktree_fingerprint(&root);

    assert_ne!(clean, fp1, "an edit must change the fingerprint");
    assert_ne!(fp1, fp2, "same-stat edits must still differ");

    std::fs::write(root.join("a.txt"), "a\nb\nc\n").unwrap();
    assert_eq!(worktree_fingerprint(&root), clean, "revert restores it");
}

#[test]
fn close_truncated_json_balances_open_structures() {
    // Open string is closed.
    assert_eq!(close_truncated_json(r#"{"a": "hello"#), r#"{"a": "hello"}"#);
    // Nested array+object are closed in reverse.
    assert_eq!(close_truncated_json(r#"{"a": [1, 2"#), r#"{"a": [1, 2]}"#);
    // A dangling separator between elements is dropped.
    assert_eq!(close_truncated_json(r#"{"a": 1,"#), r#"{"a": 1}"#);
    // A trailing escape can't be allowed to escape our closing quote.
    assert_eq!(close_truncated_json(r#"{"a": "x\"#), r#"{"a": "x"}"#);
    // An already-closed string inside an open object.
    assert_eq!(close_truncated_json(r#"{"a": "x""#), r#"{"a": "x"}"#);
    // The repaired output always parses.
    assert!(serde_json::from_str::<Value>(&close_truncated_json(r#"{"a": [{"b": "c"#)).is_ok());
}
