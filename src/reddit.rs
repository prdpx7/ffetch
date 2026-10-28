//! Read-only Reddit listing/thread adapter. Public access may require approved OAuth.
use crate::{
    model::{Attempt, Record, Run},
    net::Http,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    time::Instant,
};
use url::Url;

fn base() -> Result<Url> {
    let default = if std::env::var_os("FFETCH_REDDIT_TOKEN").is_some() {
        "https://oauth.reddit.com/"
    } else {
        "https://www.reddit.com/"
    };
    Ok(Url::parse(
        &std::env::var("FFETCH_REDDIT_BASE").unwrap_or_else(|_| default.into()),
    )?)
}
fn community_path(community: Option<&str>) -> Result<String> {
    match community {
        None => Ok(String::new()),
        Some(value) => {
            let value = value.strip_prefix("r/").unwrap_or(value);
            ensure!(
                !value.is_empty()
                    && value.len() <= 256
                    && value.split('+').all(|s| !s.is_empty()
                        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')),
                "community must be a subreddit name or names joined by +"
            );
            Ok(format!("r/{value}/"))
        }
    }
}
async fn page(http: &Http, url: Url, run: &mut Run) -> Result<Value> {
    let start = Instant::now();
    let token = std::env::var("FFETCH_REDDIT_TOKEN").ok();
    run.meta["authentication"] = json!(if token.is_some() {
        "oauth-bearer"
    } else {
        "public"
    });
    let response = http.reddit_get(url.clone(), token.as_deref()).await;
    let result = response.and_then(|response| {
        ensure!(
            response.status.is_success(),
            "Reddit returned HTTP {}{}; public access may require an approved OAuth token",
            response.status.as_u16(),
            response
                .retry_after
                .map(|s| format!("; retry after {s}"))
                .unwrap_or_default()
        );
        serde_json::from_slice(&response.bytes).context("Reddit did not return a JSON listing")
    });
    run.attempts.push(Attempt {
        source: "reddit".into(),
        url: url.to_string(),
        outcome: result
            .as_ref()
            .map(|_| "page_received".into())
            .unwrap_or_else(|e| e.to_string()),
        elapsed_ms: start.elapsed().as_millis() as u64,
    });
    result
}
fn listing(data: &Value) -> Result<&Vec<Value>> {
    ensure!(
        data["kind"] == "Listing",
        "Reddit returned an unexpected listing type"
    );
    data["data"]["children"]
        .as_array()
        .context("Reddit listing children are missing")
}
fn record(child: &Value) -> Result<Record> {
    let data = &child["data"];
    let kind = child["kind"].as_str().context("Reddit entry has no kind")?;
    ensure!(
        matches!(kind, "t1" | "t3"),
        "unsupported Reddit entry type {kind}"
    );
    let id = data["id"].as_str().context("Reddit entry has no ID")?;
    ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()),
        "invalid Reddit ID"
    );
    let permalink = data["permalink"]
        .as_str()
        .filter(|s| s.starts_with('/') && !s.starts_with("//"));
    let canonical = Url::parse("https://www.reddit.com/")?;
    let url = if let Some(path) = permalink {
        canonical.join(path)?
    } else if kind == "t1" {
        let post = data["link_id"]
            .as_str()
            .and_then(|s| s.strip_prefix("t3_"))
            .context("Reddit comment has no post identity or permalink")?;
        ensure!(
            !post.is_empty() && post.bytes().all(|b| b.is_ascii_alphanumeric()),
            "invalid comment post identity"
        );
        canonical.join(&format!("comments/{post}/_/{id}/"))?
    } else {
        canonical.join(&format!("comments/{id}/"))?
    };
    ensure!(
        url.host_str() == Some("www.reddit.com") && url.scheme() == "https",
        "Reddit permalink changed origin"
    );
    ensure!(
        data["name"]
            .as_str()
            .is_none_or(|name| name == format!("{kind}_{id}")),
        "Reddit entry name disagrees with its ID"
    );
    let mut r = Record::new(
        if kind == "t3" { "post" } else { "comment" },
        "reddit",
        url.as_str(),
    );
    r.title = data["title"].as_str().map(str::to_owned);
    r.text = data[if kind == "t3" { "selftext" } else { "body" }]
        .as_str()
        .map(str::to_owned);
    r.author = data["author"].as_str().map(str::to_owned);
    r.published_at = data["created_utc"]
        .as_f64()
        .and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
        .map(|d| d.to_rfc3339());
    r.metrics = json!({"score":data["score"],"comments":data["num_comments"],"upvote_ratio":data["upvote_ratio"]});
    let external = data["url_overridden_by_dest"]
        .as_str()
        .or_else(|| data["url"].as_str())
        .and_then(|s| Url::parse(s).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    r.content = json!({"native_id":data["name"].as_str().map(str::to_owned).unwrap_or_else(||format!("{kind}_{id}")),"subreddit":data["subreddit"],"parent_id":data["parent_id"],"post_id":data["link_id"],"article_url":external.map(|u|u.to_string()),"is_self":data["is_self"],"over_18":data["over_18"],"edited":data["edited"],"removed_by_category":data["removed_by_category"],"locked":data["locked"]});
    // Avoid cloning the entire descendant tree for every comment.
    r.native = Value::Object(
        data.as_object()
            .context("Reddit entry data is not an object")?
            .iter()
            .filter(|(key, _)| key.as_str() != "replies")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    r.provenance.method = "reddit-api".into();
    Ok(r)
}

pub async fn search(
    http: &Http,
    query: &str,
    community: Option<&str>,
    limit: usize,
    since: Option<i64>,
    run: &mut Run,
) -> Result<()> {
    run.meta["community"] = json!(community);
    let path = format!("{}search.json", community_path(community)?);
    browse(
        http,
        &path,
        Some(query),
        limit,
        since,
        ("relevance", "all"),
        run,
    )
    .await
}
pub async fn feed(
    http: &Http,
    community: Option<&str>,
    feed: &str,
    period: &str,
    limit: usize,
    run: &mut Run,
) -> Result<()> {
    ensure!(
        ["hot", "new", "top", "rising"].contains(&feed),
        "Reddit feed must be hot, new, top or rising"
    );
    ensure!(
        ["hour", "day", "week", "month", "year", "all"].contains(&period),
        "invalid Reddit ranking period"
    );
    let path = format!("{}{feed}.json", community_path(community)?);
    run.meta = json!({"source":"reddit","community":community,"feed":feed,"period":period,"ranking":"platform","momentum_measured":false});
    browse(http, &path, None, limit, None, (feed, period), run).await
}
async fn browse(
    http: &Http,
    path: &str,
    query: Option<&str>,
    limit: usize,
    since: Option<i64>,
    ranking: (&str, &str),
    run: &mut Run,
) -> Result<()> {
    let (sort, period) = ranking;
    let mut after = None::<String>;
    let mut cursors = HashSet::new();
    let mut seen = HashSet::new();
    let mut pages = 0;
    loop {
        let mut url = base()?.join(path)?;
        url.query_pairs_mut().extend_pairs([
            ("limit", limit.min(100).to_string().as_str()),
            ("raw_json", "1"),
            ("sort", sort),
            ("t", period),
        ]);
        if let Some(query) = query {
            url.query_pairs_mut()
                .extend_pairs([("q", query), ("restrict_sr", "on")]);
        }
        if let Some(after) = &after {
            url.query_pairs_mut().append_pair("after", after);
        }
        let data = page(http, url.clone(), run).await?;
        let entries = listing(&data)?;
        pages += 1;
        run.meta["pages_fetched"] = json!(pages);
        let before = seen.len();
        for child in entries {
            if child["kind"] != "t3" {
                continue;
            }
            let mut r = record(child)?;
            if !seen.insert(r.content["native_id"].as_str().unwrap().to_string()) {
                continue;
            }
            if since.is_some_and(|cutoff| {
                child["data"]["created_utc"]
                    .as_f64()
                    .is_none_or(|s| s < cutoff as f64)
            }) {
                continue;
            }
            r.metrics["rank"] = json!(run.items.len() + 1);
            r.provenance.source_url = url.to_string();
            run.items.push(r);
            if run.items.len() >= limit {
                break;
            }
        }
        after = data["data"]["after"].as_str().map(str::to_owned);
        run.meta["truncated"] = json!(after.is_some());
        if run.items.len() >= limit || after.is_none() || entries.is_empty() {
            break;
        }
        ensure!(seen.len() > before, "Reddit pagination made no progress");
        ensure!(
            cursors.insert(after.clone().unwrap()),
            "Reddit repeated its pagination cursor"
        );
    }
    Ok(())
}

pub fn thread_id(input: &str) -> Option<String> {
    let value = if let Some(id) = input.strip_prefix("reddit:") {
        id.to_string()
    } else {
        let url = Url::parse(input).ok()?;
        let segments = url
            .path_segments()?
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        match url.host_str()? {
            "redd.it" => segments.first()?.to_string(),
            "reddit.com" | "www.reddit.com" | "old.reddit.com" => {
                segments.windows(2).find(|w| w[0] == "comments")?[1].to_string()
            }
            _ => return None,
        }
    };
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(value)
}
pub async fn thread(
    http: &Http,
    id: &str,
    comments: bool,
    limit: usize,
    run: &mut Run,
) -> Result<()> {
    ensure!(
        id.bytes().all(|b| b.is_ascii_alphanumeric()) && !id.is_empty(),
        "invalid Reddit thread ID"
    );
    let mut url = base()?.join(&format!("comments/{id}.json"))?;
    url.query_pairs_mut().extend_pairs([
        ("raw_json", "1"),
        ("limit", &limit.min(500).to_string()),
        ("sort", "confidence"),
    ]);
    let data = page(http, url.clone(), run).await?;
    let parts = data
        .as_array()
        .context("Reddit thread is not a listing pair")?;
    let posts = listing(parts.first().context("Reddit thread has no post listing")?)?;
    let mut post = record(posts.first().context("Reddit thread has no post")?)?;
    ensure!(
        post.content["native_id"] == format!("t3_{id}"),
        "Reddit returned a different thread identity"
    );
    post.provenance.source_url = url.to_string();
    run.items.push(post);
    let mut queue = VecDeque::new();
    if comments {
        queue.extend(listing(
            parts
                .get(1)
                .context("Reddit thread has no comments listing")?,
        )?);
    }
    let mut count = 0;
    let mut more = Vec::new();
    let mut more_nodes = 0;
    let mut seen = HashSet::new();
    while let Some(child) = queue.pop_front() {
        if child["kind"] == "more" {
            more_nodes += 1;
            if let Some(ids) = child["data"]["children"].as_array() {
                more.extend(ids.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            continue;
        }
        if count >= limit {
            queue.push_front(child);
            break;
        }
        if child["kind"] != "t1" {
            continue;
        }
        let mut r = record(child)?;
        if !seen.insert(r.content["native_id"].as_str().unwrap().to_string()) {
            continue;
        }
        if let Some(children) = child["data"]["replies"]["data"]["children"].as_array() {
            queue.extend(children);
        }
        r.provenance.source_url = url.to_string();
        run.items.push(r);
        count += 1;
    }
    run.meta["comments_requested"] = json!(comments);
    run.meta["comments_returned"] = json!(count);
    let reported = run
        .items
        .first()
        .and_then(|r| r.metrics["comments"].as_u64());
    run.meta["comments_reported"] = json!(reported);
    run.meta["more_placeholders"] = json!(more_nodes);
    run.meta["comments_truncated"] = json!(
        comments
            && (!queue.is_empty() || more_nodes > 0 || reported.is_some_and(|n| n > count as u64))
    );
    run.meta["unexpanded_comment_ids"] = json!(more);
    run.meta["coverage"] =
        json!("returned listing only; morechildren expansion not yet implemented");
    Ok(())
}
