//! A PR's review feedback as markdown: its reviews, review threads and
//! conversation, distilled for reading or handing to an agent.
//!
//! A port of the `gh-pr-feedback` script, so the tray needs no script. The
//! markdown is the script's, line for line; only the fetching differs: the
//! repository is the directory's `origin`, asked with the stored token.

use serde_json::Value;

/// One page of a connection is asked for at a time; GraphQL allows 100.
const PAGE: u32 = 100;
/// How many lines of a thread's diff hunk are shown, after its header.
const HUNK_TAIL_LINES: usize = 12;
/// One GraphQL call is abandoned after this many seconds.
const TIMEOUT_SECS: u64 = 60;

const PR_META_FIELDS: &str = "number title url isDraft state baseRefName headRefName reviewDecision author{login __typename}";
const COMMENT_FIELDS: &str =
    "databaseId author{login __typename} body createdAt url isMinimized minimizedReason";
const REVIEW_FIELDS: &str = "databaseId author{login __typename} state body submittedAt url";

fn thread_fields() -> String {
    format!(
        "id isResolved isOutdated path line startLine originalLine resolvedBy{{login}} comments(first:100){{nodes{{{COMMENT_FIELDS} diffHunk state}}}}"
    )
}

fn page_query(connection: &str, fields: &str) -> String {
    format!(
        "query($owner:String!,$name:String!,$number:Int!,$cursor:String){{
    repository(owner:$owner,name:$name){{pullRequest(number:$number){{{PR_META_FIELDS}
      page: {connection}(first:{PAGE},after:$cursor){{pageInfo{{hasNextPage endCursor}} nodes{{{fields}}}}}}}}}}}"
    )
}

// --------------------------------------------------------------------------
// The digest
// --------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Comment {
    author: String,
    bot: bool,
    at: Option<String>,
    url: String,
    body: String,
    /// Why it was hidden, when it was.
    hidden: Option<String>,
    pending: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Thread {
    path: String,
    line: Option<i64>,
    start_line: Option<i64>,
    original_line: Option<i64>,
    resolved: bool,
    resolved_by: Option<String>,
    outdated: bool,
    hunk: String,
    has_suggestion: bool,
    comments: Vec<Comment>,
}

#[derive(Debug, Clone, PartialEq)]
struct Review {
    author: String,
    bot: bool,
    state: String,
    at: Option<String>,
    url: String,
    body: String,
}

#[derive(Debug, Clone, PartialEq)]
struct PullRequest {
    number: i64,
    title: String,
    url: String,
    author: String,
    is_draft: bool,
    state: String,
    base: String,
    head: String,
    review_decision: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Digest {
    pr: PullRequest,
    reviews: Vec<Review>,
    threads: Vec<Thread>,
    conversation: Vec<Comment>,
}

fn text(node: &Value, key: &str) -> String {
    node.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn opt_text(node: &Value, key: &str) -> Option<String> {
    node.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn login(actor: &Value) -> String {
    actor
        .get("login")
        .and_then(Value::as_str)
        .unwrap_or("ghost")
        .to_owned()
}

fn is_bot(actor: &Value) -> bool {
    actor.get("__typename").and_then(Value::as_str) == Some("Bot")
}

fn unix_lines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn to_comment(node: &Value, pending: bool) -> Comment {
    let author = node.get("author").unwrap_or(&Value::Null);
    let minimized = node.get("isMinimized").and_then(Value::as_bool) == Some(true);
    Comment {
        author: login(author),
        bot: is_bot(author),
        at: opt_text(node, "createdAt"),
        url: text(node, "url"),
        body: unix_lines(&text(node, "body")),
        hidden: minimized
            .then(|| opt_text(node, "minimizedReason").unwrap_or_else(|| "hidden".to_owned())),
        pending,
    }
}

fn to_thread(node: &Value) -> Thread {
    let nodes: Vec<Value> = node
        .pointer("/comments/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let comments: Vec<Comment> = nodes
        .iter()
        .map(|c| to_comment(c, c.get("state").and_then(Value::as_str) == Some("PENDING")))
        .collect();
    Thread {
        path: text(node, "path"),
        line: node.get("line").and_then(Value::as_i64),
        start_line: node.get("startLine").and_then(Value::as_i64),
        original_line: node.get("originalLine").and_then(Value::as_i64),
        resolved: node.get("isResolved").and_then(Value::as_bool) == Some(true),
        resolved_by: node
            .pointer("/resolvedBy/login")
            .and_then(Value::as_str)
            .map(str::to_owned),
        outdated: node.get("isOutdated").and_then(Value::as_bool) == Some(true),
        hunk: nodes
            .first()
            .map(|c| unix_lines(&text(c, "diffHunk")))
            .unwrap_or_default(),
        has_suggestion: comments.iter().any(|c| c.body.contains("```suggestion")),
        comments,
    }
}

fn to_review(node: &Value) -> Review {
    let author = node.get("author").unwrap_or(&Value::Null);
    Review {
        author: login(author),
        bot: is_bot(author),
        state: text(node, "state"),
        at: opt_text(node, "submittedAt"),
        url: text(node, "url"),
        body: unix_lines(&text(node, "body")),
    }
}

fn to_pull_request(meta: &Value) -> PullRequest {
    PullRequest {
        number: meta.get("number").and_then(Value::as_i64).unwrap_or(0),
        title: text(meta, "title"),
        url: text(meta, "url"),
        author: login(meta.get("author").unwrap_or(&Value::Null)),
        is_draft: meta.get("isDraft").and_then(Value::as_bool) == Some(true),
        state: text(meta, "state"),
        base: text(meta, "baseRefName"),
        head: text(meta, "headRefName"),
        review_decision: opt_text(meta, "reviewDecision"),
    }
}

/// Unresolved first, then by path, then by line -- as the script sorts.
fn compare_threads(a: &Thread, b: &Thread) -> std::cmp::Ordering {
    let line = |t: &Thread| t.line.or(t.original_line).unwrap_or(0);
    a.resolved
        .cmp(&b.resolved)
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| line(a).cmp(&line(b)))
}

// --------------------------------------------------------------------------
// Fetching
// --------------------------------------------------------------------------

/// Every node of `connection` on PR `number` of `repo` (`owner/name`), page
/// by page, and the PR itself.
fn fetch_all(
    repo: &str,
    number: i64,
    connection: &str,
    fields: &str,
) -> Result<(Value, Vec<Value>), String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("not owner/name: {repo}"))?;
    let query = page_query(connection, fields);
    let mut nodes = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let body = serde_json::json!({
            "query": query,
            "variables": { "owner": owner, "name": name, "number": number, "cursor": cursor },
        })
        .to_string();
        let response: Value = serde_json::from_str(
            &sl_core::gh::api::graphql(&body, TIMEOUT_SECS).map_err(|f| f.to_string())?,
        )
        .map_err(|error| format!("unreadable GraphQL response: {error}"))?;
        if let Some(errors) = response
            .get("errors")
            .and_then(Value::as_array)
            .filter(|e| !e.is_empty())
        {
            return Err(errors
                .iter()
                .map(|e| text(e, "message"))
                .collect::<Vec<_>>()
                .join("; "));
        }
        let pr = response
            .pointer("/data/repository/pullRequest")
            .filter(|pr| !pr.is_null())
            .ok_or_else(|| format!("PR #{number} not found"))?;
        nodes.extend(
            pr.pointer("/page/nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        );
        let has_next = pr
            .pointer("/page/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            == Some(true);
        cursor = pr
            .pointer("/page/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if !has_next || cursor.is_none() {
            return Ok((pr.clone(), nodes));
        }
    }
}

fn fetch_digest(cwd: &str, number: i64) -> Result<Digest, String> {
    let repo = sl_core::git::github_repo(cwd)
        .ok_or_else(|| format!("{cwd} has no GitHub remote named origin"))?;
    let (meta, threads) = fetch_all(&repo, number, "reviewThreads", &thread_fields())?;
    let (_, reviews) = fetch_all(&repo, number, "reviews", REVIEW_FIELDS)?;
    let (_, conversation) = fetch_all(&repo, number, "comments", COMMENT_FIELDS)?;
    let mut threads: Vec<Thread> = threads.iter().map(to_thread).collect();
    threads.sort_by(compare_threads);
    Ok(Digest {
        pr: to_pull_request(&meta),
        reviews: reviews.iter().map(to_review).collect(),
        threads,
        conversation: conversation.iter().map(|c| to_comment(c, false)).collect(),
    })
}

/// PR `number`'s feedback, in the repository `cwd` is in, as markdown;
/// resolved threads are left out.
pub fn markdown(cwd: &str, number: i64, now_ms: i64) -> Result<String, String> {
    let digest = fetch_digest(cwd, number)?;
    let generated = chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
    Ok(render_digest(&digest, true, false, generated.as_deref()))
}

// --------------------------------------------------------------------------
// Rendering
// --------------------------------------------------------------------------

fn review_state_label(state: &str) -> &str {
    match state {
        "APPROVED" => "✅ Approved",
        "CHANGES_REQUESTED" => "❌ Changes requested",
        "COMMENTED" => "💬 Commented",
        "DISMISSED" => "🚫 Dismissed",
        "PENDING" => "⏳ Pending",
        other => other,
    }
}

fn decision_label(decision: &str) -> &str {
    match decision {
        "APPROVED" => "Approved",
        "CHANGES_REQUESTED" => "Changes requested",
        "REVIEW_REQUIRED" => "Review required",
        other => other,
    }
}

/// `2026-11-09 21:50Z` from an ISO time; `-` for none.
fn stamp(iso: Option<&str>) -> String {
    match iso {
        Some(iso) => format!(
            "{}Z",
            iso.chars()
                .take(16)
                .collect::<String>()
                .replacen('T', " ", 1)
        ),
        None => "-".to_owned(),
    }
}

fn who(author: &str, bot: bool) -> String {
    format!("{}@{author}", if bot { "🤖 " } else { "" })
}

fn quote(text: &str) -> String {
    text.trim_end()
        .split('\n')
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The extension Node's `path.extname` would give, without its dot: none
/// for a name that only starts with one.
fn extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(0) | None => "",
        Some(dot) => name.get(dot + 1..).unwrap_or(""),
    }
}

/// Each ```` ```suggestion ```` fence labelled, and fenced in the file's language.
fn label_suggestions(body: &str, path: &str) -> String {
    let fence = format!("💡 **Suggested change:**\n```{}\n", extension(path));
    let mut out = String::new();
    let mut rest = body;
    while let Some(start) = rest.find("```suggestion") {
        let (before, from) = rest.split_at(start);
        let Some((_, after)) = from.split_once('\n') else {
            break;
        };
        out.push_str(before);
        out.push_str(&fence);
        rest = after;
    }
    out.push_str(rest);
    out
}

fn trim_hunk(hunk: &str) -> String {
    let lines: Vec<&str> = hunk.split('\n').collect();
    if lines.len() <= HUNK_TAIL_LINES + 1 {
        return hunk.to_owned();
    }
    let mut out = vec![lines[0], "…"];
    out.extend(&lines[lines.len() - HUNK_TAIL_LINES..]);
    out.join("\n")
}

fn location(thread: &Thread) -> String {
    match (thread.line, thread.start_line, thread.original_line) {
        (Some(line), Some(start), _) if start != line => {
            format!("`{}:{start}-{line}`", thread.path)
        }
        (Some(line), _, _) => format!("`{}:{line}`", thread.path),
        (None, _, None) => format!("`{}`", thread.path),
        (None, _, Some(original)) => format!("`{}` (was line {original})", thread.path),
    }
}

fn render_comment(comment: &Comment, path: Option<&str>, prefix: &str) -> String {
    let head = format!(
        "{prefix}**{}** · {}{}",
        who(&comment.author, comment.bot),
        stamp(comment.at.as_deref()),
        if comment.pending {
            " · ⏳ pending"
        } else {
            ""
        }
    );
    if let Some(hidden) = &comment.hidden {
        return format!("{head}\n> _(hidden: {hidden})_");
    }
    let body = match path {
        Some(path) => label_suggestions(&comment.body, path),
        None => comment.body.clone(),
    };
    format!("{head}\n{}", quote(&body))
}

fn render_thread(index: usize, thread: &Thread) -> String {
    let Some((first, replies)) = thread.comments.split_first() else {
        return String::new();
    };
    let mut tags = Vec::new();
    if thread.outdated {
        tags.push("⚠️ outdated".to_owned());
    }
    if thread.has_suggestion {
        tags.push("💡 suggestion".to_owned());
    }
    if first.pending {
        tags.push("⏳ pending review".to_owned());
    }
    if thread.resolved {
        tags.push(format!(
            "✅ resolved{}",
            thread
                .resolved_by
                .as_ref()
                .map(|by| format!(" by @{by}"))
                .unwrap_or_default()
        ));
    }
    let tags: String = tags.iter().flat_map(|tag| [" · ", tag.as_str()]).collect();
    let mut lines = vec![
        format!(
            "### {index}. {} · {}{tags}",
            location(thread),
            who(&first.author, first.bot)
        ),
        String::new(),
    ];
    lines.extend([format!("[Open on GitHub]({})", first.url), String::new()]);
    if !thread.hunk.is_empty() {
        lines.extend([
            "````diff".to_owned(),
            trim_hunk(&thread.hunk),
            "````".to_owned(),
            String::new(),
        ]);
    }
    lines.extend([render_comment(first, Some(&thread.path), ""), String::new()]);
    for reply in replies {
        lines.extend([
            render_comment(reply, Some(&thread.path), "↳ "),
            String::new(),
        ]);
    }
    lines.join("\n")
}

/// A review that says something: any verdict, or a comment with a body.
fn is_substantive(review: &Review) -> bool {
    review.state != "COMMENTED" || !review.body.trim().is_empty()
}

fn render_reviews(all: &[Review], skip_review_bodies: bool) -> Vec<String> {
    let reviews: Vec<&Review> = all.iter().filter(|r| is_substantive(r)).collect();
    let mut lines = vec![format!("## Reviews ({})", reviews.len()), String::new()];
    if reviews.is_empty() {
        lines.extend(["_None._".to_owned(), String::new()]);
        return lines;
    }
    lines.extend([
        "| Reviewer | State | Submitted |".to_owned(),
        "| --- | --- | --- |".to_owned(),
    ]);
    for review in &reviews {
        lines.push(format!(
            "| {} | {} | [{}]({}) |",
            who(&review.author, review.bot),
            review_state_label(&review.state),
            stamp(review.at.as_deref()),
            review.url
        ));
    }
    lines.push(String::new());
    for review in &reviews {
        if review.body.trim().is_empty() || skip_review_bodies {
            continue;
        }
        lines.extend([
            format!(
                "**{}** · {} · {}",
                who(&review.author, review.bot),
                review_state_label(&review.state),
                stamp(review.at.as_deref())
            ),
            quote(&review.body),
            String::new(),
        ]);
    }
    lines
}

fn render_digest(
    digest: &Digest,
    unresolved_only: bool,
    skip_review_bodies: bool,
    generated_at: Option<&str>,
) -> String {
    let pr = &digest.pr;
    let unresolved: Vec<&Thread> = digest.threads.iter().filter(|t| !t.resolved).collect();
    let resolved: Vec<&Thread> = digest.threads.iter().filter(|t| t.resolved).collect();
    let outdated = unresolved.iter().filter(|t| t.outdated).count();
    let decision = pr
        .review_decision
        .as_deref()
        .map_or("no review decision", decision_label);

    let mut lines = vec![
        format!("# PR #{}: {}", pr.number, pr.title),
        String::new(),
        format!("- **Link:** {}", pr.url),
        format!("- **Author:** @{}", pr.author),
        format!("- **Branch:** `{}` → `{}`", pr.head, pr.base),
        format!(
            "- **State:** {}{} · {decision}",
            pr.state,
            if pr.is_draft { " (draft)" } else { "" }
        ),
        format!(
            "- **Threads:** {} total · {} unresolved{} · {} resolved",
            digest.threads.len(),
            unresolved.len(),
            if outdated > 0 {
                format!(" ({outdated} outdated)")
            } else {
                String::new()
            },
            resolved.len()
        ),
        format!("- **Generated:** {}", stamp(generated_at)),
        String::new(),
    ];
    lines.extend(render_reviews(&digest.reviews, skip_review_bodies));
    lines.extend([
        format!("## Unresolved threads ({})", unresolved.len()),
        String::new(),
    ]);
    if unresolved.is_empty() {
        lines.extend(["_None._".to_owned(), String::new()]);
    }
    for (i, thread) in unresolved.iter().enumerate() {
        lines.push(render_thread(i + 1, thread));
    }
    if !unresolved_only && !resolved.is_empty() {
        lines.extend([
            format!("## Resolved threads ({})", resolved.len()),
            String::new(),
            "<details>".to_owned(),
            format!(
                "<summary>Show {} resolved threads</summary>",
                resolved.len()
            ),
            String::new(),
        ]);
        for (i, thread) in resolved.iter().enumerate() {
            lines.push(render_thread(unresolved.len() + i + 1, thread));
        }
        lines.extend(["</details>".to_owned(), String::new()]);
    }
    lines.extend([
        format!("## Conversation ({})", digest.conversation.len()),
        String::new(),
    ]);
    if digest.conversation.is_empty() {
        lines.extend(["_None._".to_owned(), String::new()]);
    }
    for comment in &digest.conversation {
        lines.extend([
            format!(
                "{}\n> \n> [↗]({})",
                render_comment(comment, None, ""),
                comment.url
            ),
            String::new(),
        ]);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(author: &str, body: &str) -> Comment {
        Comment {
            author: author.into(),
            bot: false,
            at: Some("2026-11-09T10:15:30Z".into()),
            url: format!("https://x/{author}"),
            body: body.into(),
            hidden: None,
            pending: false,
        }
    }

    fn thread(path: &str, line: Option<i64>, resolved: bool, comments: Vec<Comment>) -> Thread {
        Thread {
            path: path.into(),
            line,
            start_line: None,
            original_line: None,
            resolved,
            resolved_by: None,
            outdated: false,
            hunk: String::new(),
            has_suggestion: comments.iter().any(|c| c.body.contains("```suggestion")),
            comments,
        }
    }

    #[test]
    fn small_pieces_match_the_script() {
        assert_eq!(stamp(Some("2026-11-09T21:50:36.715Z")), "2026-11-09 21:50Z");
        assert_eq!(stamp(None), "-");
        assert_eq!(who("claude", true), "🤖 @claude");
        assert_eq!(quote("a\nb\n\n"), "> a\n> b");
        assert_eq!(extension("src/a.b.ts"), "ts");
        assert_eq!(
            extension(".gitignore"),
            "",
            "a leading dot is not an extension, as in Node"
        );
        assert_eq!(extension("Makefile"), "");
        assert_eq!(
            label_suggestions("try:\n```suggestion\nlet x = 1;\n```", "a.rs"),
            "try:\n💡 **Suggested change:**\n```rs\nlet x = 1;\n```"
        );
        let long: String = (0..20)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let trimmed = trim_hunk(&long);
        assert!(
            trimmed.starts_with("l0\n…\nl8\n") && trimmed.ends_with("l19"),
            "{trimmed}"
        );
        assert_eq!(trim_hunk("a\nb"), "a\nb");
    }

    #[test]
    fn locations_say_ranges_and_what_a_line_was() {
        let mut t = thread("a.rs", Some(9), false, vec![]);
        assert_eq!(location(&t), "`a.rs:9`");
        t.start_line = Some(5);
        assert_eq!(location(&t), "`a.rs:5-9`");
        t.line = None;
        assert_eq!(location(&t), "`a.rs`");
        t.original_line = Some(3);
        assert_eq!(location(&t), "`a.rs` (was line 3)");
    }

    #[test]
    fn threads_sort_unresolved_first_then_by_path_and_line() {
        let mut threads = [
            thread("b", Some(1), false, vec![]),
            thread("a", Some(1), true, vec![]),
            thread("a", Some(9), false, vec![]),
            thread("a", Some(2), false, vec![]),
        ];
        threads.sort_by(compare_threads);
        let order: Vec<(&str, Option<i64>, bool)> = threads
            .iter()
            .map(|t| (t.path.as_str(), t.line, t.resolved))
            .collect();
        assert_eq!(
            order,
            [
                ("a", Some(2), false),
                ("a", Some(9), false),
                ("b", Some(1), false),
                ("a", Some(1), true)
            ]
        );
    }

    #[test]
    fn a_digest_renders_as_the_script_does() {
        let digest = Digest {
            pr: PullRequest {
                number: 753,
                title: "feat: fire".into(),
                url: "https://github.com/o/r/pull/753".into(),
                author: "jakub".into(),
                is_draft: false,
                state: "OPEN".into(),
                base: "main".into(),
                head: "jakub/dn-531".into(),
                review_decision: Some("CHANGES_REQUESTED".into()),
            },
            reviews: vec![
                Review {
                    author: "ann".into(),
                    bot: false,
                    state: "CHANGES_REQUESTED".into(),
                    at: Some("2026-11-09T10:00:00Z".into()),
                    url: "https://x/r1".into(),
                    body: "Fix it".into(),
                },
                Review {
                    author: "bob".into(),
                    bot: false,
                    state: "COMMENTED".into(),
                    at: None,
                    url: "https://x/r2".into(),
                    body: "  ".into(),
                },
            ],
            threads: vec![
                Thread {
                    hunk: "@@ -1 +1 @@\n-x\n+y".into(),
                    outdated: true,
                    ..thread(
                        "a.rs",
                        Some(4),
                        false,
                        vec![comment("ann", "nit"), comment("jakub", "done")],
                    )
                },
                Thread {
                    resolved_by: Some("ann".into()),
                    ..thread("b.rs", Some(1), true, vec![comment("ann", "old")])
                },
            ],
            conversation: vec![comment("cy", "thanks")],
        };
        let expected = "# PR #753: feat: fire

- **Link:** https://github.com/o/r/pull/753
- **Author:** @jakub
- **Branch:** `jakub/dn-531` → `main`
- **State:** OPEN · Changes requested
- **Threads:** 2 total · 1 unresolved (1 outdated) · 1 resolved
- **Generated:** 2026-11-09 21:50Z

## Reviews (1)

| Reviewer | State | Submitted |
| --- | --- | --- |
| @ann | ❌ Changes requested | [2026-11-09 10:00Z](https://x/r1) |

**@ann** · ❌ Changes requested · 2026-11-09 10:00Z
> Fix it

## Unresolved threads (1)

### 1. `a.rs:4` · @ann · ⚠️ outdated

[Open on GitHub](https://x/ann)

````diff
@@ -1 +1 @@
-x
+y
````

**@ann** · 2026-11-09 10:15Z
> nit

↳ **@jakub** · 2026-11-09 10:15Z
> done

## Conversation (1)

**@cy** · 2026-11-09 10:15Z
> thanks\n> \n> [↗](https://x/cy)
";
        assert_eq!(
            render_digest(&digest, true, false, Some("2026-11-09T21:50:36.715Z")),
            expected
        );
        let all = render_digest(&digest, false, false, Some("2026-11-09T21:50:36.715Z"));
        assert!(all.contains("## Resolved threads (1)\n\n<details>\n<summary>Show 1 resolved threads</summary>\n\n### 2. `b.rs:1` · @ann · ✅ resolved by @ann\n"), "{all}");
    }

    #[test]
    fn graphql_nodes_become_the_digests_parts() {
        let node: Value = serde_json::from_str(
            r#"{"isResolved":false,"isOutdated":false,"path":"a.rs","line":3,"startLine":null,"originalLine":3,"resolvedBy":null,
                "comments":{"nodes":[{"author":{"login":"claude","__typename":"Bot"},"body":"x\r\n```suggestion\ny","createdAt":"t","url":"u","isMinimized":false,"minimizedReason":null,"diffHunk":"@@ h","state":"PENDING"},
                                     {"author":null,"body":"z","createdAt":"t","url":"u2","isMinimized":true,"minimizedReason":null,"diffHunk":"","state":"SUBMITTED"}]}}"#,
        )
        .unwrap();
        let t = to_thread(&node);
        assert_eq!(t.hunk, "@@ h");
        assert!(t.has_suggestion);
        assert_eq!(t.comments[0].body, "x\n```suggestion\ny", "CRLF as LF");
        assert!(t.comments[0].bot && t.comments[0].pending);
        assert_eq!(t.comments[1].author, "ghost", "a deleted account");
        assert_eq!(t.comments[1].hidden.as_deref(), Some("hidden"));
    }
}
