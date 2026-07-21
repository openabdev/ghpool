use std::env;
use std::process::{Command, exit};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let ghpool_url = env::var("GHPOOL_URL")
        .unwrap_or_else(|_| "http://ghpool.openab.local:8080".to_string());

    // Handle version
    if args.first().map(|s| s.as_str()) == Some("version") || args.first().map(|s| s.as_str()) == Some("--version") {
        println!("ghp version {}", env!("CARGO_PKG_VERSION"));
        let gh = find_real_gh();
        let _ = Command::new(&gh).arg("--version").status();
        exit(0);
    }

    // git credential helper protocol: `ghp git-credential <get|store|erase>`
    // Configure with:
    //   git config --global credential."https://github.com".helper "!ghp git-credential"
    //   git config --global credential."https://github.com".useHttpPath true
    if args.first().map(|s| s.as_str()) == Some("git-credential") {
        exit(git_credential(args.get(1).map(|s| s.as_str()), &ghpool_url));
    }

    // Try to handle as a pooled read via ghpool REST
    if let Some(code) = try_pooled(&args, &ghpool_url) {
        exit(code);
    }

    // Writes / unsupported commands: fall through to real gh
    let gh = find_real_gh();
    let status = Command::new(&gh).args(&args).status().unwrap_or_else(|e| {
        eprintln!("ghp: failed to exec {}: {}", gh, e);
        exit(1);
    });
    exit(status.code().unwrap_or(1));
}

fn try_pooled(args: &[String], base: &str) -> Option<i32> {
    if args.is_empty() { return None; }
    match args[0].as_str() {
        "api" => try_api(args, base),
        "issue" if args.get(1).map(|s| s.as_str()) == Some("list") => try_issue_list(args, base),
        "issue" if args.get(1).map(|s| s.as_str()) == Some("view") => try_issue_view(args, base),
        "pr" if args.get(1).map(|s| s.as_str()) == Some("list") => try_pr_list(args, base),
        "pr" if args.get(1).map(|s| s.as_str()) == Some("view") => try_pr_view(args, base),
        "pr" if args.get(1).map(|s| s.as_str()) == Some("diff") => try_pr_diff(args, base),
        "pr" if args.get(1).map(|s| s.as_str()) == Some("checks") => try_pr_checks(args, base),
        "run" if args.get(1).map(|s| s.as_str()) == Some("list") => try_run_list(args, base),
        _ => None,
    }
}

// gh api <path> [--jq .field]  (GET only)
fn try_api(args: &[String], base: &str) -> Option<i32> {
    if args.len() < 2 { return None; }
    // Bail on write indicators
    if args.iter().any(|a| a == "-X" || a == "--method" || a == "-f" || a == "--field" || a == "--input") {
        return None;
    }
    let path = &args[1];
    if path == "graphql" { return None; } // GraphQL: fall through

    let url = format!("{}/{}", base, path.trim_start_matches('/'));
    let body = http_get(&url)?;

    if let Some(expr) = flag_val(args, "--jq").or_else(|| flag_val(args, "-q")) {
        let val: serde_json::Value = serde_json::from_str(&body).ok()?;
        println!("{}", jq_extract(&val, &expr));
    } else {
        print!("{}", body);
    }
    Some(0)
}

// gh issue list -R owner/repo
fn try_issue_list(args: &[String], base: &str) -> Option<i32> {
    let repo = repo_flag(args)?;
    let limit = flag_val(args, "-L").or_else(|| flag_val(args, "--limit")).unwrap_or("30".into());
    let state = flag_val(args, "-s").or_else(|| flag_val(args, "--state")).unwrap_or("open".into());

    let url = format!("{}/repos/{}/issues?state={}&per_page={}", base, repo, state, limit);
    let body = http_get(&url)?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body).ok()?;

    for item in &items {
        if item.get("pull_request").is_some() { continue; }
        println!("#{}\t{}\t{}",
            item["number"].as_u64().unwrap_or(0),
            item["state"].as_str().unwrap_or("").to_uppercase(),
            item["title"].as_str().unwrap_or(""));
    }
    Some(0)
}

// gh issue view <number> -R owner/repo
fn try_issue_view(args: &[String], base: &str) -> Option<i32> {
    let repo = repo_flag(args)?;
    let number = args.get(2).and_then(|s| s.parse::<u64>().ok())?;

    let url = format!("{}/repos/{}/issues/{}", base, repo, number);
    let body = http_get(&url)?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;

    println!("#{} {}", v["number"], v["title"].as_str().unwrap_or(""));
    println!("State: {} | Author: {}", v["state"].as_str().unwrap_or(""), v["user"]["login"].as_str().unwrap_or(""));
    if let Some(b) = v["body"].as_str() {
        if !b.is_empty() { println!("\n{}", b); }
    }
    Some(0)
}

// gh pr list -R owner/repo
fn try_pr_list(args: &[String], base: &str) -> Option<i32> {
    let repo = repo_flag(args)?;
    let limit = flag_val(args, "-L").or_else(|| flag_val(args, "--limit")).unwrap_or("30".into());
    let state = flag_val(args, "-s").or_else(|| flag_val(args, "--state")).unwrap_or("open".into());

    let url = format!("{}/repos/{}/pulls?state={}&per_page={}", base, repo, state, limit);
    let body = http_get(&url)?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body).ok()?;

    for item in &items {
        println!("#{}\t{}\t{}",
            item["number"].as_u64().unwrap_or(0),
            item["state"].as_str().unwrap_or("").to_uppercase(),
            item["title"].as_str().unwrap_or(""));
    }
    Some(0)
}

// gh pr view <number> -R owner/repo
fn try_pr_view(args: &[String], base: &str) -> Option<i32> {
    let repo = repo_flag(args)?;
    let number = args.get(2).and_then(|s| s.parse::<u64>().ok())?;

    let url = format!("{}/repos/{}/pulls/{}", base, repo, number);
    let body = http_get(&url)?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;

    println!("#{} {}", v["number"], v["title"].as_str().unwrap_or(""));
    println!("State: {} | Author: {} | Branch: {}",
        v["state"].as_str().unwrap_or(""),
        v["user"]["login"].as_str().unwrap_or(""),
        v["head"]["ref"].as_str().unwrap_or(""));
    if let Some(b) = v["body"].as_str() {
        if !b.is_empty() { println!("\n{}", b); }
    }
    Some(0)
}

// gh pr diff <number> -R owner/repo
fn try_pr_diff(args: &[String], base: &str) -> Option<i32> {
    let repo = repo_flag(args)?;
    let number = args.get(2).and_then(|s| s.parse::<u64>().ok())?;

    let url = format!("{}/raw/repos/{}/pulls/{}", base, repo, number);
    let client = reqwest::blocking::Client::new();
    let resp = client.get(&url)
        .header("Accept", "application/vnd.github.v3.diff")
        .send().ok()?;
    if !resp.status().is_success() { return None; }
    print!("{}", resp.text().ok()?);
    Some(0)
}

// gh pr checks <number> -R owner/repo
fn try_pr_checks(args: &[String], base: &str) -> Option<i32> {
    let repo = repo_flag(args)?;
    let number = args.get(2).and_then(|s| s.parse::<u64>().ok())?;

    // Get the PR head SHA
    let pr_url = format!("{}/repos/{}/pulls/{}", base, repo, number);
    let pr_body = http_get(&pr_url)?;
    let pr: serde_json::Value = serde_json::from_str(&pr_body).ok()?;
    let sha = pr["head"]["sha"].as_str()?;

    // Get check runs for that SHA
    let url = format!("{}/repos/{}/commits/{}/check-runs", base, repo, sha);
    let body = http_get(&url)?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let runs = v["check_runs"].as_array()?;

    for run in runs {
        let name = run["name"].as_str().unwrap_or("");
        let status = run["status"].as_str().unwrap_or("");
        let conclusion = run["conclusion"].as_str().unwrap_or("");
        let display = if status == "completed" { conclusion } else { status };
        println!("{}\t{}", display, name);
    }
    Some(0)
}

// gh run list -R owner/repo
fn try_run_list(args: &[String], base: &str) -> Option<i32> {
    let repo = repo_flag(args)?;
    let limit = flag_val(args, "-L").or_else(|| flag_val(args, "--limit")).unwrap_or("10".into());

    let url = format!("{}/repos/{}/actions/runs?per_page={}", base, repo, limit);
    let body = http_get(&url)?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let runs = v["workflow_runs"].as_array()?;

    for run in runs {
        println!("{}\t{}\t{}\t{}",
            run["status"].as_str().unwrap_or(""),
            run["conclusion"].as_str().unwrap_or(""),
            run["name"].as_str().unwrap_or(""),
            run["head_branch"].as_str().unwrap_or(""));
    }
    Some(0)
}

// --- helpers ---

fn http_get(url: &str) -> Option<String> {
    reqwest::blocking::get(url).ok()?.text().ok()
}

/// Git credential helper backed by ghpool's /git-credential endpoint:
/// exchanges GHPOOL_KEY for a short-lived, single-repo GitHub App
/// installation token. Only the `get` operation does anything; `store`
/// and `erase` are no-ops (tokens are ephemeral, nothing to persist).
///
/// Exit codes: 0 with output = credential provided; 1 = decline quietly so
/// git falls through to the next configured helper (if any).
fn git_credential(op: Option<&str>, base: &str) -> i32 {
    match op {
        Some("get") => {}
        Some("store") | Some("erase") => return 0,
        _ => {
            eprintln!("usage: ghp git-credential <get|store|erase>");
            return 1;
        }
    }
    let Ok(key) = env::var("GHPOOL_KEY") else {
        return 1; // no key in env — decline, let git try other helpers
    };

    let mut input = String::new();
    use std::io::Read as _;
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 1;
    }
    let attrs = parse_credential_input(&input);
    if attrs.get("protocol").map(String::as_str) != Some("https") {
        return 1;
    }
    let host_ok = matches!(
        attrs.get("host").map(String::as_str),
        Some("github.com") | Some("gist.github.com")
    );
    if !host_ok {
        return 1;
    }
    let Some(repo) = attrs.get("path").and_then(|p| repo_from_path(p)) else {
        return 1; // no owner/repo (useHttpPath not set?) — decline
    };

    let url = format!("{}/git-credential?repo={}", base, repo);
    let client = reqwest::blocking::Client::new();
    let Ok(resp) = client
        .get(&url)
        .header("X-Ghpool-Key", key)
        .timeout(std::time::Duration::from_secs(15))
        .send()
    else {
        return 1;
    };
    if !resp.status().is_success() {
        return 1;
    }
    let Ok(v) = resp.json::<serde_json::Value>() else {
        return 1;
    };
    let (Some(user), Some(pass)) = (v["username"].as_str(), v["password"].as_str()) else {
        return 1;
    };
    println!("username={}", user);
    println!("password={}", pass);
    0
}

/// Parse `key=value` lines of the git credential helper protocol.
fn parse_credential_input(input: &str) -> std::collections::HashMap<String, String> {
    input
        .lines()
        .take_while(|l| !l.is_empty())
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// `owner/repo` from a git request path: strips a trailing `.git` and any
/// extra segments (`owner/repo/info/refs` → `owner/repo`).
fn repo_from_path(path: &str) -> Option<String> {
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if repo.is_empty() {
        return None;
    }
    Some(format!("{}/{}", owner, repo))
}

fn flag_val(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn repo_flag(args: &[String]) -> Option<String> {
    flag_val(args, "-R").or_else(|| flag_val(args, "--repo"))
}

fn jq_extract(val: &serde_json::Value, expr: &str) -> String {
    let path = expr.trim_start_matches('.');
    if path.is_empty() {
        return serde_json::to_string_pretty(val).unwrap_or_default();
    }
    let mut current = val;
    for part in path.split('.') {
        if part.is_empty() { continue; }
        current = &current[part];
    }
    match current {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn find_real_gh() -> String {
    // Look for gh-real first (when ghp replaces /usr/bin/gh)
    for dir in env::var("PATH").unwrap_or_default().split(':') {
        let candidate = format!("{}/gh-real", dir);
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    // Fallback: find gh that isn't ourselves
    let self_path = env::current_exe().ok();
    for dir in env::var("PATH").unwrap_or_default().split(':') {
        let candidate = format!("{}/gh", dir);
        if std::path::Path::new(&candidate).exists() {
            if let Some(ref sp) = self_path {
                if std::fs::canonicalize(&candidate).ok() == std::fs::canonicalize(sp).ok() {
                    continue;
                }
            }
            return candidate;
        }
    }
    "/usr/bin/gh".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn test_flag_val() {
        let a = args(&["pr", "view", "123", "-R", "owner/repo"]);
        assert_eq!(flag_val(&a, "-R"), Some("owner/repo".to_string()));
        assert_eq!(flag_val(&a, "--repo"), None);
    }

    #[test]
    fn test_repo_flag() {
        let a = args(&["pr", "diff", "42", "--repo", "foo/bar"]);
        assert_eq!(repo_flag(&a), Some("foo/bar".to_string()));

        let a = args(&["pr", "diff", "42", "-R", "baz/qux"]);
        assert_eq!(repo_flag(&a), Some("baz/qux".to_string()));
    }

    #[test]
    fn test_repo_flag_missing() {
        let a = args(&["pr", "diff", "42"]);
        assert_eq!(repo_flag(&a), None);
    }

    #[test]
    fn test_jq_extract_simple() {
        let val: serde_json::Value = serde_json::json!({"name": "hello", "count": 42});
        assert_eq!(jq_extract(&val, ".name"), "hello");
        assert_eq!(jq_extract(&val, ".count"), "42");
        assert_eq!(jq_extract(&val, ".missing"), "null");
    }

    #[test]
    fn test_jq_extract_nested() {
        let val: serde_json::Value = serde_json::json!({"head": {"sha": "abc123"}});
        assert_eq!(jq_extract(&val, ".head.sha"), "abc123");
    }

    #[test]
    fn test_try_pooled_returns_none_for_writes() {
        // pr create, pr merge, etc. should not be handled
        let a = args(&["pr", "create", "--title", "test"]);
        assert_eq!(try_pooled(&a, "http://fake:8080"), None);

        let a = args(&["pr", "merge", "123", "-R", "o/r"]);
        assert_eq!(try_pooled(&a, "http://fake:8080"), None);
    }

    #[test]
    fn test_try_pooled_returns_none_for_empty() {
        let a: Vec<String> = vec![];
        assert_eq!(try_pooled(&a, "http://fake:8080"), None);
    }

    #[test]
    fn test_try_pr_diff_missing_repo() {
        // No -R flag → returns None (falls through)
        let a = args(&["pr", "diff", "123"]);
        assert_eq!(try_pr_diff(&a, "http://unreachable:9999"), None);
    }

    #[test]
    fn test_try_pr_diff_bad_number() {
        let a = args(&["pr", "diff", "notanumber", "-R", "o/r"]);
        assert_eq!(try_pr_diff(&a, "http://unreachable:9999"), None);
    }

    #[test]
    fn test_try_pr_checks_missing_repo() {
        let a = args(&["pr", "checks", "123"]);
        assert_eq!(try_pr_checks(&a, "http://unreachable:9999"), None);
    }

    #[test]
    fn test_try_pr_checks_bad_number() {
        let a = args(&["pr", "checks", "abc", "-R", "o/r"]);
        assert_eq!(try_pr_checks(&a, "http://unreachable:9999"), None);
    }

    #[test]
    fn test_try_api_write_indicators_return_none() {
        let a = args(&["api", "/repos/o/r/issues", "-X", "POST"]);
        assert_eq!(try_api(&a, "http://fake:8080"), None);

        let a = args(&["api", "/repos/o/r/issues", "-f", "title=x"]);
        assert_eq!(try_api(&a, "http://fake:8080"), None);

        let a = args(&["api", "graphql"]);
        assert_eq!(try_api(&a, "http://fake:8080"), None);
    }
}

#[cfg(test)]
mod git_credential_tests {
    use super::*;

    #[test]
    fn test_parse_credential_input() {
        let attrs = parse_credential_input(
            "protocol=https\nhost=github.com\npath=openabdev/openab.git\n\nignored=after-blank\n",
        );
        assert_eq!(attrs.get("protocol").unwrap(), "https");
        assert_eq!(attrs.get("host").unwrap(), "github.com");
        assert_eq!(attrs.get("path").unwrap(), "openabdev/openab.git");
        assert!(!attrs.contains_key("ignored"), "parsing stops at blank line");
    }

    #[test]
    fn test_repo_from_path() {
        assert_eq!(repo_from_path("openabdev/openab.git").as_deref(), Some("openabdev/openab"));
        assert_eq!(repo_from_path("openabdev/openab").as_deref(), Some("openabdev/openab"));
        assert_eq!(repo_from_path("/oablab/chi.git").as_deref(), Some("oablab/chi"));
        assert_eq!(
            repo_from_path("openabdev/openab/info/refs").as_deref(),
            Some("openabdev/openab")
        );
        assert_eq!(repo_from_path("justowner"), None);
        assert_eq!(repo_from_path("owner/.git"), None);
        assert_eq!(repo_from_path(""), None);
    }
}
