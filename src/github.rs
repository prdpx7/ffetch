//! Native GitHub discovery and repository/issue evidence. No local gh credentials
//! are read; an explicitly configured bearer is restricted to api.github.com.
use crate::{
    model::{Attempt, Issue, Record, Run},
    net::{self, Http},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use url::Url;

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub repository: String,
    pub number: Option<u64>,
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn repository(value: &str) -> Result<String> {
    let (owner, repo) = value
        .split_once('/')
        .context("GitHub repository needs owner/name")?;
    ensure!(
        name(owner) && name(repo),
        "invalid GitHub repository identity"
    );
    Ok(value.to_owned())
}
fn web_url(input: &str) -> Result<Url> {
    let url = net::normalize(input)?;
    ensure!(
        matches!(url.host_str(), Some("github.com" | "www.github.com")) && url.port().is_none(),
        "GitHub record changed web origin"
    );
    Ok(url)
}
pub fn target(input: &str) -> Option<Target> {
    let url = web_url(input).ok()?;
    let segments = url
        .path()
        .trim_end_matches('/')
        .trim_start_matches('/')
        .split('/')
        .collect::<Vec<_>>();
    let (owner, repo) = (*segments.first()?, *segments.get(1)?);
    let repo = if segments.len() == 2 {
        repo.strip_suffix(".git").unwrap_or(repo)
    } else {
        repo
    };
    let full_name = repository(&format!("{owner}/{repo}")).ok()?;
    let number = match segments.as_slice() {
        [_, _] => None,
        [_, _, "issues" | "pull", n] => Some(n.parse::<u64>().ok().filter(|n| *n > 0)?),
        _ => return None,
    };
    Some(Target {
        repository: full_name,
        number,
    })
}
/// Individual comment anchors need their own representation. Do not replace
/// one with an issue-body-only API read.
pub fn body_target(input: &str) -> Option<Target> {
    if web_url(input)
        .ok()?
        .fragment()
        .is_some_and(|fragment| fragment.starts_with("issuecomment-"))
    {
        return None;
    }
    target(input)
}
fn endpoint(path: &str) -> Result<Url> {
    let mut base = net::normalize(
        &std::env::var("FFETCH_GITHUB_BASE").unwrap_or_else(|_| "https://api.github.com/".into()),
    )?;
    ensure!(
        base.query().is_none() && base.fragment().is_none(),
        "GitHub API base must not contain a query or fragment"
    );
    let path = format!("{}/{}", base.path().trim_end_matches('/'), path);
    base.set_path(&path);
    Ok(base)
}
fn token() -> Option<String> {
    std::env::var("FFETCH_GITHUB_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}
pub fn archive_policy(input: &str, requested: crate::fetch::Archive) -> crate::fetch::Archive {
    archive_for(input, requested, token().is_some())
}
fn archive_for(
    input: &str,
    requested: crate::fetch::Archive,
    authenticated: bool,
) -> crate::fetch::Archive {
    // An authenticated API may mask private repositories with 404. Do not send
    // those names to an external archive during automatic fallback.
    if authenticated && requested == crate::fetch::Archive::Auto && web_url(input).is_ok() {
        crate::fetch::Archive::Off
    } else {
        requested
    }
}
pub(crate) fn issue(source: &str, error: impl std::fmt::Display) -> Issue {
    let message = error.to_string();
    let (code, action, retry) = if message.contains("credentials require") {
        (
            "credential_origin_rejected",
            "Use the official API origin with FFETCH_GITHUB_TOKEN; endpoint overrides cannot receive that token",
            false,
        )
    } else if message.contains("HTTP 401") || message.contains("authentication_required") {
        (
            "authentication_required",
            "Set an appropriate FFETCH_GITHUB_TOKEN; code search requires authentication",
            false,
        )
    } else if message.contains("HTTP 429") || message.to_ascii_lowercase().contains("rate limit") {
        (
            "rate_limited",
            "Respect GitHub rate limits and Retry-After; wait or use another source",
            true,
        )
    } else if message.contains("identity mismatch") {
        (
            "identity_mismatch",
            "Inspect the returned repository/issue identity; mismatched evidence was rejected",
            false,
        )
    } else {
        (
            "provider_failed",
            "Inspect the attempts and native API error; completed records remain available",
            true,
        )
    };
    let mut issue = Issue::new(source, code, message);
    issue.next_action = Some(action.into());
    issue.retryable = retry;
    issue
}
struct Page {
    data: Value,
    url: Url,
    artifact: Option<String>,
    status: u16,
}
async fn page(
    http: &Http,
    url: Url,
    search: bool,
    store: Option<&Store>,
    run: &mut Run,
) -> Result<Page> {
    let started = Instant::now();
    let bearer = token();
    run.meta["authentication"] = json!(if bearer.is_some() {
        "explicit-bearer"
    } else {
        "public"
    });
    let result = async {
        let response = http
            .github_get(url.clone(), bearer.as_deref(), search)
            .await?;
        ensure!(
            response.url.origin() == url.origin(),
            "GitHub API response changed origin"
        );
        let artifact = store.map(|s| s.blob(&response.bytes)).transpose()?;
        let data: Value =
            serde_json::from_slice(&response.bytes).context("GitHub API did not return JSON")?;
        let status = response.status.as_u16();
        // README absence is a valid repository state. Other errors stay explicit.
        ensure!(
            response.status.is_success() || status == 404 && url.path().ends_with("/readme"),
            "GitHub HTTP {status}: {}{}",
            data["message"].as_str().unwrap_or("API request failed"),
            response
                .retry_after
                .as_ref()
                .map(|s| format!("; Retry-After: {s}"))
                .unwrap_or_default()
        );
        Ok(Page {
            data,
            url: response.url,
            artifact,
            status,
        })
    }
    .await;
    run.attempts.push(Attempt {
        source: "github-api".into(),
        url: url.to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        outcome: result
            .as_ref()
            .map(|p: &Page| format!("HTTP {}", p.status))
            .unwrap_or_else(|e: &anyhow::Error| e.to_string()),
    });
    result
}
fn attributed(mut record: Record, page: &Page) -> Record {
    record.provenance.source_url = page.url.to_string();
    record.provenance.method = "github-rest-api".into();
    record.provenance.artifact.clone_from(&page.artifact);
    record
}
fn id(data: &Value) -> Result<u64> {
    data["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .context("GitHub record has no positive ID")
}
fn repo_record(data: &Value) -> Result<Record> {
    let identity = repository(
        data["full_name"]
            .as_str()
            .context("GitHub repository has no full_name")?,
    )?;
    let url = data["html_url"]
        .as_str()
        .context("GitHub repository has no HTML URL")?;
    ensure!(
        target(url)
            .is_some_and(|t| t.number.is_none() && t.repository.eq_ignore_ascii_case(&identity)),
        "GitHub repository identity mismatch"
    );
    let mut record = Record::new("repository", "github", url);
    record.id = format!("github:repository:{}", id(data)?);
    record.title = Some(identity.clone());
    record.text = data["description"].as_str().map(str::to_owned);
    record.author = data["owner"]["login"].as_str().map(str::to_owned);
    record.published_at = data["created_at"].as_str().map(str::to_owned);
    record.metrics = json!({"stars":data["stargazers_count"],"forks":data["forks_count"],"subscribers":data["subscribers_count"],"open_issues_and_pull_requests":data["open_issues_count"]});
    record.content = json!({"repository":identity,"default_branch":data["default_branch"],"language":data["language"],"topics":data["topics"],"license":data["license"]["spdx_id"],"archived":data["archived"],"disabled":data["disabled"],"visibility":data["visibility"],"updated_at":data["updated_at"],"pushed_at":data["pushed_at"],"homepage":data["homepage"],"clone_url":data["clone_url"],"full_text_retrieved":false});
    record.native = data.clone();
    Ok(record)
}

fn issue_record(data: &Value) -> Result<Record> {
    let url = data["html_url"]
        .as_str()
        .context("GitHub issue has no HTML URL")?;
    let identity = target(url)
        .filter(|t| t.number.is_some())
        .context("invalid GitHub issue URL")?;
    ensure!(
        identity.number == data["number"].as_u64(),
        "GitHub issue identity mismatch"
    );
    let mut record = Record::new(
        if data["pull_request"].is_object() {
            "pull_request"
        } else {
            "issue"
        },
        "github",
        url,
    );
    record.id = format!("github:issue:{}", id(data)?);
    record.title = data["title"].as_str().map(str::to_owned);
    record.text = data["body"].as_str().map(str::to_owned);
    record.author = data["user"]["login"].as_str().map(str::to_owned);
    record.published_at = data["created_at"].as_str().map(str::to_owned);
    record.metrics = json!({"comments":data["comments"],"reactions":data["reactions"]});
    record.content = json!({"repository":identity.repository,"number":identity.number,"state":data["state"],"state_reason":data["state_reason"],"labels":data["labels"],"updated_at":data["updated_at"],"closed_at":data["closed_at"],"body_format":"markdown","review_comments_retrieved":false});
    record.native = data.clone();
    Ok(record)
}
fn code_record(data: &Value) -> Result<Record> {
    let repo = repository(
        data["repository"]["full_name"]
            .as_str()
            .context("code match has no repository")?,
    )?;
    let url = web_url(
        data["html_url"]
            .as_str()
            .context("code match has no HTML URL")?,
    )?;
    let path = data["path"].as_str().context("code match has no path")?;
    ensure!(
        !path.is_empty()
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid GitHub code path"
    );
    let parts = url
        .path_segments()
        .context("code URL has no path")?
        .collect::<Vec<_>>();
    ensure!(
        parts.len() >= 5
            && parts[2] == "blob"
            && format!("{}/{}", parts[0], parts[1]).eq_ignore_ascii_case(&repo),
        "GitHub code identity mismatch"
    );
    let mut expected = Url::parse("https://github.com/")?;
    expected
        .path_segments_mut()
        .unwrap()
        .clear()
        .extend(&parts[..4])
        .extend(path.split('/'));
    ensure!(
        expected.path() == url.path(),
        "GitHub code file identity mismatch"
    );
    let sha = data["sha"]
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .context("code match has no valid blob SHA")?;
    let mut record = Record::new("code_match", "github", url.as_str());
    record.id = format!("github:code:{repo}:{}:{sha}", path);
    record.title = data["path"].as_str().map(str::to_owned);
    record.text = data["text_matches"].as_array().map(|matches| {
        matches
            .iter()
            .filter_map(|m| m["fragment"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    });
    record.content = json!({"repository":repo,"path":data["path"],"blob_sha":sha,"text_matches":data["text_matches"],"full_file_retrieved":false,"coverage":"API search snippets; default branch and index restrictions apply"});
    record.native = data.clone();
    Ok(record)
}

pub async fn search(
    http: &Http,
    source: &str,
    query: &str,
    limit: usize,
    since: Option<i64>,
    run: &mut Run,
    progress: Option<&tokio::sync::mpsc::Sender<Vec<Record>>>,
) -> Result<()> {
    ensure!(
        limit > 0 && !query.trim().is_empty(),
        "GitHub search requires a query and a positive result limit"
    );
    let (kind, date_field) = match source {
        "github" => ("repositories", "pushed_at"),
        "github-issues" => ("issues", "created_at"),
        "github-code" => ("code", ""),
        _ => anyhow::bail!("unsupported GitHub source"),
    };
    if kind == "code" {
        ensure!(
            since.is_none(),
            "GitHub code search does not enforce --since"
        );
        ensure!(
            token().is_some(),
            "authentication_required: GitHub code search requires FFETCH_GITHUB_TOKEN"
        );
    }
    let mut effective = query.to_owned();
    if let Some(date) = since.and_then(|time| chrono::DateTime::from_timestamp(time, 0)) {
        effective.push_str(&format!(
            " {}:>={}",
            if kind == "repositories" {
                "pushed"
            } else {
                "created"
            },
            date.format("%Y-%m-%d")
        ));
    }
    run.meta["github_search"] = json!({"kind":kind,"effective_query":effective,"since_field":date_field,"api_result_cap":1000,"pages_requested":0,"pages_completed":0,"stop_reason":"in_progress","coverage":"bounded API index; not the entire GitHub corpus"});
    let per_page = limit.clamp(1, 100);
    run.meta["github_search"]["per_page"] = json!(per_page);
    run.meta["github_search"]["incomplete_results"] = json!(false);
    let mut seen = HashSet::new();
    for number in 1..=1000usize.div_ceil(per_page) {
        let mut url = endpoint(&format!("search/{kind}"))?;
        url.query_pairs_mut().extend_pairs([
            ("q", effective.as_str()),
            ("per_page", &per_page.to_string()),
            ("page", &number.to_string()),
        ]);
        run.meta["github_search"]["pages_requested"] = json!(number);
        let page = match page(http, url, true, None, run).await {
            Ok(page) => page,
            Err(error) => {
                run.meta["github_search"]["stop_reason"] = json!("page_failed");
                return Err(error);
            }
        };
        let total = page.data["total_count"]
            .as_u64()
            .context("GitHub search total_count is missing")?;
        let incomplete = page.data["incomplete_results"]
            .as_bool()
            .context("GitHub search incomplete_results is missing")?;
        let items = page.data["items"]
            .as_array()
            .context("GitHub search items are missing")?;
        run.meta["github_search"]["reported_total"] = json!(total);
        run.meta["github_search"]["pages_completed"] = json!(number);
        if incomplete {
            run.meta["github_search"]["incomplete_results"] = json!(true);
        }
        if items.is_empty() && total > ((number - 1) * per_page) as u64 && !incomplete {
            run.errors.push(Issue::new(source,"github_search_gap","GitHub returned an empty page before its reported result count; coverage is incomplete"));
        }
        if incomplete
            && !run
                .errors
                .iter()
                .any(|e| e.code == "github_search_incomplete")
        {
            run.errors.push(Issue::new(
                source,
                "github_search_incomplete",
                "GitHub reports incomplete search results; returned matches are retained",
            ));
        }
        let before = run.items.len();
        let mut newly_observed = 0;
        for data in items {
            let parsed = match kind {
                "repositories" => repo_record(data),
                "issues" => issue_record(data),
                _ => code_record(data),
            };
            match parsed {
                Ok(mut record) => {
                    if !seen.insert(record.id.clone()) {
                        continue;
                    }
                    newly_observed += 1;
                    if let Some(since) = since {
                        let time = data[date_field]
                            .as_str()
                            .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
                            .context("GitHub --since result has no valid timestamp")?;
                        if time.timestamp() < since {
                            continue;
                        }
                    }
                    record = attributed(record, &page);
                    record.native["discovery_queries"] = json!([query]);
                    record.native["search_page"] = json!(number);
                    run.items.push(record);
                    if run.items.len() >= limit {
                        break;
                    }
                }
                Err(error) => run.errors.push(issue(source, error)),
            }
        }
        if let Some(progress) = progress.filter(|_| run.items.len() > before) {
            progress
                .send(run.items[before..].to_vec())
                .await
                .context("search progress receiver closed")?;
        }
        let reason = if run.items.len() >= limit {
            Some("result_limit")
        } else if items.is_empty() {
            Some("empty_page")
        } else if newly_observed == 0 {
            Some("no_new_records")
        } else if number * per_page >= 1000 {
            Some("api_result_cap")
        } else if items.len() < per_page || (number * per_page) as u64 >= total {
            Some("reported_results_consumed")
        } else {
            None
        };
        if let Some(reason) = reason {
            run.meta["github_search"]["stop_reason"] = json!(reason);
            return Ok(());
        }
    }
    run.meta["github_search"]["stop_reason"] = json!("api_result_cap");
    Ok(())
}

pub async fn fetch(
    http: &Http,
    target: &Target,
    comments: bool,
    limit: usize,
    store: Option<&Store>,
    budget: Duration,
) -> Run {
    let start = Instant::now();
    let mut run = read_run(target, comments, limit);
    match net::within_budget(budget, read(http, target, comments, limit, store, &mut run)).await {
        Ok(Ok(())) => (),
        Ok(Err(error)) => run.errors.push(issue("github", error)),
        Err(_) => {
            run.meta["stop_reason"] = json!("deadline");
            run.errors.push(Issue::new(
                "github",
                "deadline_exceeded",
                "GitHub reading exceeded the shared timeout; completed evidence retained",
            ));
        }
    }
    run.finish(start.elapsed().as_millis() as u64);
    run
}
pub(crate) fn read_run(target: &Target, comments: bool, limit: usize) -> Run {
    let mut run = Run::new(if comments && target.number.is_some() {
        "thread"
    } else {
        "fetch"
    });
    run.meta = json!({"repository":target.repository,"issue_number":target.number,"selected_representation":"github-rest-api","comments_limit":limit});
    run
}
pub(crate) async fn read(
    http: &Http,
    target: &Target,
    comments: bool,
    limit: usize,
    store: Option<&Store>,
    run: &mut Run,
) -> Result<()> {
    let full_name = repository(&target.repository)?;
    let path = if let Some(number) = target.number {
        format!("repos/{full_name}/issues/{number}")
    } else {
        format!("repos/{full_name}")
    };
    // Metadata and the current default-branch README need separate API calls,
    // but neither request depends on the other's body. Keep metadata on a later
    // README failure, and cancel the README immediately on metadata failure.
    let mut readme_run = Run::new("github-readme");
    let readme_url = target
        .number
        .is_none()
        .then(|| endpoint(&format!("repos/{full_name}/readme")))
        .transpose()?;
    let mut readme_task =
        readme_url.map(|url| Box::pin(page(http, url, false, store, &mut readme_run)));
    let mut metadata_task = Box::pin(page(http, endpoint(&path)?, false, store, run));
    let mut readme_ready = None;
    let metadata = if let Some(task) = readme_task.as_mut() {
        tokio::select! {
            result = &mut metadata_task => result,
            result = task => {readme_ready=Some(result); metadata_task.as_mut().await}
        }
    } else {
        metadata_task.as_mut().await
    };
    drop(metadata_task);
    let page = metadata?;
    let mut record = if target.number.is_some() {
        issue_record(&page.data)?
    } else {
        repo_record(&page.data)?
    };
    ensure!(
        record.content["repository"]
            .as_str()
            .is_some_and(|repo| repo.eq_ignore_ascii_case(&full_name))
            && (target.number.is_none() || record.content["number"].as_u64() == target.number),
        "GitHub requested resource identity mismatch"
    );
    // Search identities stay stable for upserts. A fresh native read is a
    // separate evidence observation, linked back by search --read.
    record.id = uuid::Uuid::new_v4().to_string();
    let parent = record.id.clone();
    let reported_comments = page.data["comments"].as_u64();
    run.items.push(attributed(record, &page));
    if let Some(number) = target.number {
        run.meta["reported_comments"] = json!(reported_comments);
        run.meta["comments_returned"] = json!(0);
        run.meta["comment_scope"] = json!(
            "issue conversation; pull-request review comments are separate and are not fetched"
        );
        if !comments {
            run.meta["stop_reason"] = json!("body_only");
            return Ok(());
        }
        ensure!(limit > 0, "GitHub thread needs a positive comment limit");
        let mut seen = HashSet::new();
        let per_page = limit.min(100);
        for number_page in 1.. {
            let mut url = endpoint(&format!("repos/{full_name}/issues/{number}/comments"))?;
            url.query_pairs_mut().extend_pairs([
                ("per_page", per_page.to_string()),
                ("page", number_page.to_string()),
            ]);
            let page = self::page(http, url, false, store, run).await?;
            let items = page
                .data
                .as_array()
                .context("GitHub comments are not a list")?;
            let before = seen.len();
            for data in items {
                let id = id(data)?;
                if !seen.insert(id) {
                    continue;
                }
                let url = data["html_url"]
                    .as_str()
                    .context("GitHub comment has no URL")?;
                ensure!(
                    self::target(url).is_some_and(|t| t.number == Some(number)
                        && t.repository.eq_ignore_ascii_case(&full_name))
                        && web_url(url)?.fragment() == Some(format!("issuecomment-{id}").as_str()),
                    "GitHub comment identity mismatch"
                );
                let mut record = Record::new("comment", "github", url);
                record.text = data["body"].as_str().map(str::to_owned);
                record.author = data["user"]["login"].as_str().map(str::to_owned);
                record.published_at = data["created_at"].as_str().map(str::to_owned);
                record.content = json!({"repository":full_name,"issue_number":number,"parent_id":parent,"body_format":"markdown","updated_at":data["updated_at"]});
                record.metrics = json!({"reactions":data["reactions"]});
                record.native = data.clone();
                run.items.push(attributed(record, &page));
                run.meta["comments_returned"] = json!(seen.len());
                if seen.len() >= limit {
                    run.meta["stop_reason"] = json!("comment_limit");
                    return Ok(());
                }
            }
            if items.len() < per_page || seen.len() == before {
                run.meta["stop_reason"] = json!(if items.len() < per_page {
                    "short_comment_page"
                } else {
                    "no_new_comments"
                });
                return Ok(());
            }
        }
    } else {
        if comments {
            run.errors.push(Issue::new("github","unsupported_enrichment","Repository discussions are not issue comments; supply a GitHub issue/pull URL to read a thread"));
        }
        run.meta["coverage"] = json!(
            "repository metadata and README; source files, releases and discussions are not enumerated"
        );
        let result = if let Some(result) = readme_ready {
            result
        } else {
            readme_task.take().context("missing README request")?.await
        };
        drop(readme_task);
        run.attempts.extend(readme_run.attempts);
        let readme = result?;
        if readme.status == 404 {
            run.meta["readme"] = json!("not_found");
            return Ok(());
        }
        run.meta["readme_response_artifact"] = json!(readme.artifact);
        ensure!(
            readme.data["encoding"] == "base64",
            "GitHub README content is unavailable in this API representation"
        );
        let encoded = readme.data["content"]
            .as_str()
            .context("GitHub README has no content")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(
                encoded
                    .bytes()
                    .filter(|b| !b.is_ascii_whitespace())
                    .collect::<Vec<_>>(),
            )
            .context("invalid README base64")?;
        if let Some(size) = readme.data["size"].as_u64() {
            ensure!(
                size == bytes.len() as u64,
                "GitHub README size does not match content"
            );
        }
        let text = std::str::from_utf8(&bytes)
            .context("GitHub README is not UTF-8; inspect its API artifact")?;
        let url = web_url(
            readme.data["html_url"]
                .as_str()
                .context("README has no HTML URL")?,
        )?;
        ensure!(
            url.path()
                .to_ascii_lowercase()
                .starts_with(&format!("/{full_name}/blob/").to_ascii_lowercase()),
            "GitHub README identity mismatch"
        );
        let mut record = Record::new("repository_readme", "github", url.as_str());
        record.title = Some(format!("README: {full_name}"));
        record.text = Some(text.into());
        record.content = json!({"repository":full_name,"parent_id":parent,"path":readme.data["path"],"blob_sha":readme.data["sha"],"bytes":bytes.len(),"format":"markdown","text_artifact":store.map(|s|s.blob(&bytes)).transpose()?,"revision_pinned":false});
        record.native = json!({"path":readme.data["path"],"sha":readme.data["sha"],"size":readme.data["size"],"type":readme.data["type"]});
        record = attributed(record, &readme);
        record.provenance.locator = readme.data["path"]
            .as_str()
            .map(|path| format!("file:{path}"));
        run.items.push(record);
        run.meta["readme"] = json!("retrieved");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supported_identities_and_code_fragments_do_not_cross_repositories_or_files() {
        assert!(body_target("https://github.com/lab/gripper/issues/7#issuecomment-42").is_none());
        assert!(target("https://github.com/lab/gripper/issues/7#issuecomment-42").is_some());
        assert_eq!(
            target("https://github.com/Lab/Gripper.git")
                .unwrap()
                .repository,
            "Lab/Gripper"
        );
        assert_eq!(
            target("https://github.com/lab/gripper/pull/7")
                .unwrap()
                .number,
            Some(7)
        );
        for input in [
            "https://github.com.evil.test/lab/gripper",
            "https://github.com/user:password@evil.test/lab/gripper",
            "https://github.com/lab/gripper/issues/0",
            "https://github.com/lab/gripper/blob/main/file.rs",
        ] {
            assert!(target(input).is_none());
        }
        let mut code = json!({"sha":"b".repeat(40),"path":"src/force control.rs","html_url":"https://github.com/lab/gripper/blob/main/src/force%20control.rs","repository":{"full_name":"lab/gripper"},"text_matches":[{"fragment":"force_limit = 4"}]});
        let record = code_record(&code).unwrap();
        assert_eq!(record.text.as_deref(), Some("force_limit = 4"));
        assert_eq!(record.content["full_file_retrieved"], false);
        code["path"] = json!("src/another_file.rs");
        assert!(
            code_record(&code)
                .unwrap_err()
                .to_string()
                .contains("identity mismatch")
        );
        code["repository"]["full_name"] = json!("other/gripper");
        assert!(code_record(&code).is_err());
    }
    #[test]
    fn repository_and_issue_routes_share_a_case_insensitive_read_identity() {
        let mut planner = crate::reading::Planner::default();
        let first = Record::new("repository", "github", "https://github.com/Lab/Gripper");
        let second = Record::new("search_result", "web", "https://github.com/lab/gripper.git");
        assert!(planner.consider(&first).is_some());
        assert!(planner.consider(&second).is_none());
        let issue = Record::new("issue", "github", "https://github.com/Lab/Gripper/issues/7");
        let pull = Record::new(
            "pull_request",
            "github",
            "https://github.com/lab/gripper/pull/7",
        );
        assert!(planner.consider(&issue).is_some());
        assert!(planner.consider(&pull).is_none());
    }
    #[test]
    fn automatic_authenticated_reads_do_not_disclose_repository_paths_to_archives() {
        use crate::fetch::Archive;
        for url in [
            "https://github.com/lab/private",
            "https://github.com/lab/private/blob/main/key.rs",
        ] {
            assert_eq!(archive_for(url, Archive::Auto, true), Archive::Off);
            assert_eq!(archive_for(url, Archive::Auto, false), Archive::Auto);
            assert_eq!(
                archive_for(url, Archive::LatestValid, true),
                Archive::LatestValid
            );
        }
        assert_eq!(
            archive_for("https://example.org/", Archive::Auto, true),
            Archive::Auto
        );
    }
}
