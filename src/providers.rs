use crate::{
    extract,
    model::{Issue, Record, Run},
    net::Http,
};
use anyhow::{Result, anyhow, ensure};
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};
use url::Url;

#[derive(Clone)]
pub struct Providers {
    pub http: Http,
    pub browser: Option<crate::browser::Renderer>,
    pub browser_only: bool,
    pub hn_base: Url,
    pub hn_search: Url,
    pub web_search: Url,
    pub brave_search: Url,
    pub searxng: Option<Url>,
    pub concurrency: usize,
    pub budget: Duration,
    pub reddit_community: Option<String>,
    pub youtube_region: String,
}

fn issue(source: &str, e: impl std::fmt::Display) -> Issue {
    let mut issue = Issue::new(source, "provider_failed", e.to_string());
    issue.retryable = true;
    issue.next_action = Some(
        "Retry this provider or select another source; successful records remain available".into(),
    );
    issue
}

fn date(seconds: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(seconds, 0).map(|d| d.to_rfc3339())
}

fn hn_record(item: Value) -> Result<Record> {
    let id = item["id"]
        .as_u64()
        .ok_or_else(|| anyhow!("HN item missing numeric ID"))?;
    ensure!(
        item["deleted"] != true && item["dead"] != true,
        "HN item {id} is deleted or dead"
    );
    let discussion = format!("https://news.ycombinator.com/item?id={id}");
    let kind = if item["type"] == "comment" {
        "comment"
    } else {
        "story"
    };
    let mut record = Record::new(kind, "hn", &discussion);
    record.id = format!("hn:{id}");
    record.title = item["title"].as_str().map(str::to_owned);
    record.text = item["text"].as_str().map(extract::text);
    record.author = item["by"].as_str().map(str::to_owned);
    record.published_at = item["time"].as_i64().and_then(date);
    record.metrics = json!({"score":item["score"],"comments":item["descendants"]});
    record.content =
        json!({"article_url":item["url"],"parent_id":item["parent"],"child_ids":item["kids"]});
    record.provenance.method = "hn-api".into();
    record.provenance.locator = Some(format!("item:{id}"));
    record.native = item;
    Ok(record)
}

impl Providers {
    async fn hn_item(&self, id: u64) -> Result<Record> {
        let url = self.hn_base.join(&format!("item/{id}.json"))?;
        let mut record = hn_record(self.http.json(url.clone()).await?)?;
        record.provenance.source_url = url.into();
        Ok(record)
    }

    pub async fn hn_feed(&self, feed: &str, limit: usize) -> Run {
        let start = Instant::now();
        let mut run = Run::new("trends");
        run.meta = json!({"source":"hn","feed":feed,"ranking":"platform","momentum_measured":false,"limit":limit});
        let task = async {
            let endpoint = match feed {
                "top" => "topstories",
                "new" => "newstories",
                "best" => "beststories",
                "ask" => "askstories",
                "show" => "showstories",
                _ => return Err(anyhow!("unknown HN feed: {feed}")),
            };
            let data = self
                .http
                .json(self.hn_base.join(&format!("{endpoint}.json"))?)
                .await?;
            let ids = data
                .as_array()
                .ok_or_else(|| anyhow!("HN feed is not an array"))?;
            let ids = ids
                .iter()
                .take(limit)
                .map(|id| {
                    id.as_u64()
                        .ok_or_else(|| anyhow!("HN feed contains a nonnumeric ID"))
                })
                .collect::<Result<Vec<_>>>()?;
            let tasks = ids
                .into_iter()
                .enumerate()
                .map(|(rank, id)| async move { (rank, self.hn_item(id).await) });
            let mut pending = stream::iter(tasks).buffer_unordered(self.concurrency);
            while let Some((rank, result)) = pending.next().await {
                match result {
                    Ok(mut record) => {
                        record.metrics["rank"] = json!(rank + 1);
                        run.items.push(record);
                    }
                    Err(e) => run.errors.push(issue("hn", e)),
                }
            }
            Ok(())
        };
        match tokio::time::timeout(self.budget, task).await {
            Ok(Ok(())) => (),
            Ok(Err(e)) => run.errors.push(issue("hn", e)),
            Err(_) => run.errors.push(issue(
                "hn",
                "feed deadline exceeded; some records may be missing",
            )),
        }
        run.items
            .sort_by_key(|r| r.metrics["rank"].as_u64().unwrap_or(u64::MAX));
        run.finish(start.elapsed().as_millis() as u64);
        run
    }

    pub async fn hn_thread(&self, id: u64, comments: bool, limit: usize) -> Run {
        let start = Instant::now();
        let mut run = Run::new("fetch");
        run.meta =
            json!({"source":"hn","item_id":id,"comments_requested":comments,"comment_limit":limit});
        let task = async {
            let root = self.hn_item(id).await?;
            let mut queue = VecDeque::new();
            enqueue_children(&root, &mut queue);
            run.items.push(root);
            let mut visited = HashSet::from([id]);
            let mut attempted = 0;
            while comments && !queue.is_empty() && attempted < limit {
                let take = self.concurrency.min(limit - attempted);
                let batch = (0..take)
                    .filter_map(|_| queue.pop_front())
                    .filter(|id| visited.insert(*id))
                    .collect::<Vec<_>>();
                attempted += batch.len();
                let mut pending = stream::iter(batch.into_iter().map(|id| self.hn_item(id)))
                    .buffer_unordered(self.concurrency);
                while let Some(result) = pending.next().await {
                    match result {
                        Ok(record) => {
                            enqueue_children(&record, &mut queue);
                            run.items.push(record);
                        }
                        Err(e) => run.errors.push(issue("hn", e)),
                    }
                }
            }
            run.meta["comments_truncated"] = json!(comments && !queue.is_empty());
            if comments && !queue.is_empty() {
                run.warnings.push(format!(
                    "Comment traversal reached --limit {limit}; additional comments remain"
                ));
            }
            Ok::<(), anyhow::Error>(())
        };
        match tokio::time::timeout(self.budget, task).await {
            Ok(Ok(())) => (),
            Ok(Err(e)) => run.errors.push(issue("hn", e)),
            Err(_) => run.errors.push(issue(
                "hn",
                "thread deadline exceeded; partial comment tree retained",
            )),
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }

    pub async fn reddit_feed(&self, feed: &str, period: &str, limit: usize) -> Run {
        let start = Instant::now();
        let mut run = Run::new("trends");
        match tokio::time::timeout(
            self.budget,
            crate::reddit::feed(
                &self.http,
                self.reddit_community.as_deref(),
                feed,
                period,
                limit,
                &mut run,
            ),
        )
        .await
        {
            Ok(Ok(())) => (),
            Ok(Err(e)) => run.errors.push(issue("reddit", e)),
            Err(_) => run.errors.push(issue(
                "reddit",
                "feed deadline exceeded; partial records retained",
            )),
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }
    pub async fn reddit_thread(&self, id: &str, comments: bool, limit: usize) -> Run {
        let start = Instant::now();
        let mut run = Run::new("thread");
        match tokio::time::timeout(
            self.budget,
            crate::reddit::thread(&self.http, id, comments, limit, &mut run),
        )
        .await
        {
            Ok(Ok(())) => (),
            Ok(Err(e)) => run.errors.push(issue("reddit", e)),
            Err(_) => run.errors.push(issue(
                "reddit",
                "thread deadline exceeded; partial records retained",
            )),
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }

    pub async fn search(&self, source: &str, query: &str, limit: usize, since: Option<i64>) -> Run {
        let start = Instant::now();
        let mut run = Run::new("search");
        run.meta = json!({"source":source,"query":query,"limit":limit});
        let task = async {
            match source {
                "hn" => self.search_hn(query, limit, since, &mut run).await,
                "arxiv" => crate::arxiv::search(&self.http, query, limit, since, &mut run).await,
                "reddit" => self.search_reddit(query, limit, since, &mut run).await,
                "youtube" => self.search_youtube(query, limit, since, &mut run).await,
                "crossref" | "europepmc" | "pubmed" => {
                    crate::scholarly::search(&self.http, source, query, limit, since, &mut run)
                        .await
                }
                "web" => self.search_web(query, limit, since, &mut run).await,
                engine if engine.starts_with("engine:") => {
                    self.search_engine_query(&engine[7..], query, limit, &mut run)
                        .await
                }
                _ => Err(anyhow!(
                    "unsupported source {source}; supported sources: web, hn, reddit, youtube, arxiv, crossref, europepmc, pubmed"
                )),
            }
        };
        match tokio::time::timeout(self.budget, task).await {
            Ok(Ok(())) => (),
            Ok(Err(e)) => run.errors.push(issue(source, e)),
            Err(_) => run.errors.push(issue(
                source,
                "search deadline exceeded; partial results retained",
            )),
        }
        if let Some(engine) = source.strip_prefix("engine:") {
            run.meta["engine"] = json!(engine);
            if since.is_some() {
                run.warnings
                    .push(format!("{engine} does not enforce --since"));
            }
            for (index, item) in run.items.iter_mut().enumerate() {
                let native = std::mem::replace(&mut item.native, json!({}));
                item.native = json!({"provider_data":native,"origins":[{"engine":engine,"rank":index+1,"query":query,"url":item.url,"title":item.title,"snippet":item.text,"provenance":item.provenance}]});
            }
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }

    pub async fn arxiv_paper(&self, id: &str) -> Run {
        let start = Instant::now();
        let mut run = Run::new("fetch");
        match tokio::time::timeout(self.budget, crate::arxiv::lookup(&self.http, id, &mut run))
            .await
        {
            Ok(Ok(())) => (),
            Ok(Err(e)) => run.errors.push(issue("arxiv", e)),
            Err(_) => run.errors.push(issue(
                "arxiv",
                "arXiv deadline exceeded; completed records retained",
            )),
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }

    pub async fn youtube_video(&self, id: &str, language: Option<&str>) -> Run {
        let start = Instant::now();
        let mut run = Run::new("fetch");
        match tokio::time::timeout(
            self.budget,
            crate::youtube::video(&self.http, id, language, &mut run),
        )
        .await
        {
            Ok(Ok(())) => (),
            Ok(Err(e)) => run.errors.push(issue("youtube", e)),
            Err(_) => run.errors.push(issue(
                "youtube",
                "video deadline exceeded; completed metadata retained",
            )),
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }
    pub async fn youtube_popular(&self, limit: usize) -> Run {
        let start = Instant::now();
        let mut run = Run::new("trends");
        match tokio::time::timeout(
            self.budget,
            crate::youtube::popular(&self.http, &self.youtube_region, limit, &mut run),
        )
        .await
        {
            Ok(Ok(())) => (),
            Ok(Err(e)) => run.errors.push(issue("youtube", e)),
            Err(_) => run
                .errors
                .push(issue("youtube", "popular chart deadline exceeded")),
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }
    async fn search_youtube(
        &self,
        query: &str,
        limit: usize,
        since: Option<i64>,
        run: &mut Run,
    ) -> Result<()> {
        let native =
            crate::youtube::search(&self.http, query, limit, since, &self.youtube_region, run)
                .await;
        if native.is_ok() || !run.items.is_empty() {
            return native;
        }
        let error = native.unwrap_err().to_string();
        if error.contains("official HTTPS API origin") {
            return Err(anyhow!(error));
        }
        let mut unavailable = issue("youtube", &error);
        unavailable.code = "native_metadata_unavailable".into();
        run.errors.push(unavailable);
        let mut discovered = Run::new("search");
        self.search_web(
            &format!("site:youtube.com/watch {query}"),
            limit,
            since,
            &mut discovered,
        )
        .await?;
        discovered
            .items
            .retain(|r| crate::youtube::video_id(&r.url).is_some());
        for record in &mut discovered.items {
            record.content = json!({"requested_source":"youtube","video_id":crate::youtube::video_id(&record.url),"native_metadata_available":false});
        }
        run.meta["discovery_fallback"] =
            json!({"method":"web-search","native_metrics":false,"transcript_retrieved":false});
        let useful = !discovered.items.is_empty();
        run.merge(discovered);
        ensure!(
            useful,
            "YouTube metadata unavailable and web discovery returned no matching video URLs"
        );
        Ok(())
    }

    async fn search_reddit(
        &self,
        query: &str,
        limit: usize,
        since: Option<i64>,
        run: &mut Run,
    ) -> Result<()> {
        let native = crate::reddit::search(
            &self.http,
            query,
            self.reddit_community.as_deref(),
            limit,
            since,
            run,
        )
        .await;
        if native.is_ok() || !run.items.is_empty() {
            return native;
        }
        let message = native.unwrap_err().to_string();
        if message.contains("community must") || message.contains("official HTTPS OAuth origin") {
            return Err(anyhow!(message));
        }
        let mut unavailable = issue("reddit", &message);
        unavailable.code = "native_metadata_unavailable".into();
        unavailable.next_action=Some("Configure approved Reddit OAuth access for native posts/comments; web-discovery results have no native metrics".into());
        run.errors.push(unavailable);
        let community = self
            .reddit_community
            .as_deref()
            .unwrap_or("")
            .trim_start_matches("r/");
        let query = if !community.is_empty() && !community.contains('+') {
            format!("site:reddit.com/r/{community} {query}")
        } else {
            format!("site:reddit.com {query}")
        };
        let mut discovered = Run::new("search");
        self.search_web(&query, limit, since, &mut discovered)
            .await?;
        discovered.items.retain(|r| {
            let Ok(url) = Url::parse(&r.url) else {
                return false;
            };
            if !matches!(
                url.host_str(),
                Some("reddit.com" | "www.reddit.com" | "old.reddit.com" | "redd.it")
            ) {
                return false;
            }
            community.is_empty()
                || community.split('+').any(|s| {
                    url.path()
                        .to_ascii_lowercase()
                        .starts_with(&format!("/r/{}/", s.to_ascii_lowercase()))
                })
        });
        for record in &mut discovered.items {
            if !record.content.is_object() {
                record.content = json!({});
            }
            record.content["requested_source"] = json!("reddit");
            record.content["native_metadata_available"] = json!(false);
        }
        run.meta["discovery_fallback"] = json!({"method":"web-search","query":query,"native_metrics":false,"native_comments":false});
        let useful = !discovered.items.is_empty();
        run.merge(discovered);
        ensure!(
            useful,
            "Reddit API unavailable and web discovery returned no matching Reddit URLs"
        );
        Ok(())
    }

    pub(crate) async fn search_response(&self, url: Url) -> Result<crate::net::Response> {
        if self.browser_only {
            let renderer = self
                .browser
                .as_ref()
                .ok_or_else(|| anyhow!("browser rendering is disabled or unavailable"))?;
            let rendered = renderer.render(&url).await?;
            Ok(rendered.response)
        } else {
            self.http.get(url).await
        }
    }

    async fn search_engine_query(
        &self,
        engine: &str,
        query: &str,
        limit: usize,
        run: &mut Run,
    ) -> Result<()> {
        let start = Instant::now();
        let result = crate::engines::search(self, engine, query, limit, run).await;
        let route = if self.browser_only { "browser" } else { "http" };
        run.meta["retrieval_route"] = json!(route);
        run.attempts.push(crate::model::Attempt {
            source: format!("{engine}:{route}"),
            url: run.meta["search_url"].as_str().unwrap_or("").into(),
            outcome: result
                .as_ref()
                .map(|_| "ok".into())
                .unwrap_or_else(|e| format!("{e:#}")),
            elapsed_ms: start.elapsed().as_millis() as u64,
        });
        if self.browser_only && result.is_ok() {
            for record in &mut run.items {
                record.provenance.method = format!("{engine}-browser-dom");
            }
            run.meta["retrieval_route"] = json!("browser-dom");
        }
        if result.is_ok() || self.browser_only || engine == "searxng" {
            return result;
        }
        let message = result.as_ref().unwrap_err().to_string();
        if message.contains("429") || message.contains("404") || message.contains("410") {
            return result;
        }
        let Some(renderer) = self.browser.as_ref().filter(|r| r.available()) else {
            return result;
        };
        let mut rendered_provider = self.clone();
        rendered_provider.browser_only = true;
        rendered_provider.browser = Some(renderer.clone());
        let start = Instant::now();
        let mut branch = Run::new("search");
        let rendered = tokio::time::timeout(
            self.budget.mul_f64(0.7),
            crate::engines::search(&rendered_provider, engine, query, limit, &mut branch),
        )
        .await;
        let outcome = match rendered {
            Ok(result) => result,
            Err(_) => Err(anyhow!("browser search stage deadline exceeded")),
        };
        run.attempts.push(crate::model::Attempt {
            source: format!("{engine}:browser"),
            url: branch.meta["search_url"].as_str().unwrap_or("").into(),
            outcome: outcome
                .as_ref()
                .map(|_| "ok".into())
                .unwrap_or_else(|e| format!("{e:#}")),
            elapsed_ms: start.elapsed().as_millis() as u64,
        });
        match outcome {
            Ok(()) => {
                run.items.clear();
                for record in &mut branch.items {
                    record.provenance.method = format!("{engine}-browser-dom");
                }
                run.warnings.push(format!(
                    "{engine} HTTP route failed ({message}); recovered using rendered DOM"
                ));
                run.meta["retrieval_route"] = json!("browser-dom");
                run.merge(branch);
                Ok(())
            }
            Err(error) => Err(anyhow!(
                "{engine} HTTP failed ({message}); browser failed ({error:#})"
            )),
        }
    }

    async fn search_hn(
        &self,
        query: &str,
        limit: usize,
        since: Option<i64>,
        run: &mut Run,
    ) -> Result<()> {
        let mut page = 0;
        let mut seen = HashSet::new();
        loop {
            let mut url = self.hn_search.clone();
            url.query_pairs_mut().extend_pairs([
                ("query", query),
                ("tags", "story"),
                ("hitsPerPage", &limit.min(100).to_string()),
                ("page", &page.to_string()),
            ]);
            if let Some(since) = since {
                url.query_pairs_mut()
                    .append_pair("numericFilters", &format!("created_at_i>={since}"));
            }
            let data = self.http.json(url.clone()).await?;
            let hits = data["hits"]
                .as_array()
                .ok_or_else(|| anyhow!("HN search returned an unexpected schema"))?;
            for hit in hits {
                let id = hit["objectID"]
                    .as_str()
                    .ok_or_else(|| anyhow!("search hit missing objectID"))?;
                if !seen.insert(id.to_owned()) {
                    continue;
                }
                let discussion = format!("https://news.ycombinator.com/item?id={id}");
                let mut record = Record::new("story", "hn", &discussion);
                record.id = format!("hn:{id}");
                record.title = hit["title"].as_str().map(str::to_owned);
                record.text = hit["story_text"].as_str().map(extract::text);
                record.author = hit["author"].as_str().map(str::to_owned);
                record.published_at = hit["created_at"].as_str().map(str::to_owned);
                record.metrics = json!({"score":hit["points"],"comments":hit["num_comments"]});
                record.content = json!({"article_url":hit["url"]});
                record.native = hit.clone();
                record.provenance.source_url = url.to_string();
                record.provenance.method = "hn-search".into();
                run.items.push(record);
                if run.items.len() >= limit {
                    break;
                }
            }
            page += 1;
            run.meta["pages_fetched"] = json!(page);
            run.meta["provider_total"] = data["nbHits"].clone();
            if run.items.len() >= limit
                || hits.is_empty()
                || page >= data["nbPages"].as_u64().unwrap_or(1)
            {
                break;
            }
        }
        Ok(())
    }

    async fn search_web(
        &self,
        query: &str,
        limit: usize,
        since: Option<i64>,
        run: &mut Run,
    ) -> Result<()> {
        if since.is_some() {
            run.warnings.push("The web provider does not enforce --since; only sources supporting exact date filtering apply it".into());
        }
        if self.searxng.is_some() {
            let mut branch = Run::new("search");
            let result = tokio::time::timeout(
                self.budget.min(Duration::from_secs(3)),
                crate::engines::search(self, "searxng", query, limit, &mut branch),
            )
            .await;
            match result {
                Ok(Ok(())) if !branch.items.is_empty() => {
                    run.merge(branch);
                    return Ok(());
                }
                Ok(Ok(())) => run
                    .warnings
                    .push("SearXNG returned no usable results; trying direct engines".into()),
                Ok(Err(error)) => run
                    .warnings
                    .push(format!("SearXNG failed ({error}); trying direct engines")),
                Err(_) => run.warnings.push(
                    "SearXNG lookup exceeded fast-route budget; trying direct engines".into(),
                ),
            }
        }
        // Start the currently successful lightweight route first. Hedge after 300 ms
        // so a stalled provider cannot consume the whole search deadline. Dropping
        // `pending` cancels unfinished requests after a validated result arrives.
        let tasks =
            [("brave", 0), ("duckduckgo", 300)]
                .into_iter()
                .map(|(source, delay)| async move {
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    let mut branch = Run::new("search");
                    let start = Instant::now();
                    let result = match source {
                        "brave" => {
                            self.search_engine_query("brave", query, limit, &mut branch)
                                .await
                        }
                        _ => {
                            self.search_engine_query("duckduckgo", query, limit, &mut branch)
                                .await
                        }
                    };
                    (source, result, branch, start.elapsed().as_millis() as u64)
                });
        let mut pending = stream::iter(tasks).buffer_unordered(2);
        let mut failures = Vec::new();
        while let Some((source, result, mut branch, _elapsed)) = pending.next().await {
            run.attempts.append(&mut branch.attempts);
            match result {
                Ok(()) => {
                    for (index, record) in branch.items.iter_mut().enumerate() {
                        let raw = std::mem::replace(&mut record.native, json!({}));
                        record.native = json!({"provider_data":raw,"origins":[{"engine":source,"rank":index+1,"query":query,"url":record.url,"title":record.title,"snippet":record.text,"provenance":record.provenance}]});
                    }
                    run.meta["selected_provider"] = json!(source);
                    run.meta["pagination"] = json!("first provider page");
                    run.warnings.extend(failures);
                    run.merge(branch);
                    return Ok(());
                }
                Err(e) => failures.push(format!("{source}: {e}")),
            }
        }
        Err(anyhow!(
            "all web search routes failed: {}",
            failures.join("; ")
        ))
    }

    pub(crate) async fn search_brave(
        &self,
        query: &str,
        limit: usize,
        run: &mut Run,
    ) -> Result<()> {
        let mut url = self.brave_search.clone();
        url.query_pairs_mut()
            .extend_pairs([("q", query), ("source", "web")]);
        run.meta["search_url"] = json!(url.as_str());
        let response = self.search_response(url).await?;
        ensure!(
            response.status.is_success(),
            "HTTP {}",
            response.status.as_u16()
        );
        let html = String::from_utf8_lossy(&response.bytes);
        ensure!(
            extract::invalid_content(&html).is_none(),
            "challenge or error page"
        );
        let doc = scraper::Html::parse_document(&html);
        let mut seen = HashSet::new();
        for snippet in doc.select(&extract::selector(".snippet[data-type='web']")?) {
            let Some(anchor) = snippet.select(&extract::selector("a[href]")?).find(|a| {
                a.select(&extract::selector(".search-snippet-title").unwrap())
                    .next()
                    .is_some()
            }) else {
                continue;
            };
            let title = anchor
                .select(&extract::selector(".search-snippet-title")?)
                .next()
                .unwrap();
            let href = anchor.value().attr("href").unwrap_or_default();
            let link = response.url.join(href)?;
            if !matches!(link.scheme(), "http" | "https")
                || !seen.insert(extract::canonical(link.as_str()))
            {
                continue;
            }
            let mut record = Record::new("search_result", "web", link.as_str());
            record.title = Some(title.text().collect::<String>().trim().into());
            record.text = snippet
                .select(&extract::selector(".generic-snippet .content")?)
                .next()
                .map(|e| e.text().collect::<String>().trim().into());
            record.provenance.method = "brave-html".into();
            record.provenance.source_url = response.url.to_string();
            run.items.push(record);
            if run.items.len() >= limit {
                break;
            }
        }
        ensure!(
            !run.items.is_empty(),
            "no recognized organic results; possible challenge, empty results, or changed markup"
        );
        Ok(())
    }

    pub(crate) async fn search_ddg(&self, query: &str, limit: usize, run: &mut Run) -> Result<()> {
        let mut url = self.web_search.clone();
        url.query_pairs_mut().append_pair("q", query);
        run.meta["search_url"] = json!(url.as_str());
        let response = self.search_response(url.clone()).await?;
        ensure!(
            response.status.is_success(),
            "web search returned HTTP {}",
            response.status.as_u16()
        );
        let html = String::from_utf8_lossy(&response.bytes);
        ensure!(
            extract::invalid_content(&html).is_none(),
            "web search returned a challenge or error page"
        );
        let doc = scraper::Html::parse_document(&html);
        let mut seen = HashSet::new();
        for result in doc.select(&extract::selector(".result")?) {
            let Some(anchor) = result.select(&extract::selector("a.result__a")?).next() else {
                continue;
            };
            let Some(href) = anchor.value().attr("href") else {
                continue;
            };
            let absolute = response.url.join(href)?;
            let destination = absolute
                .query_pairs()
                .find(|(key, _)| key == "uddg")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_else(|| absolute.to_string());
            let destination = Url::parse(&destination)?;
            if !matches!(destination.scheme(), "http" | "https")
                || !seen.insert(extract::canonical(destination.as_str()))
            {
                continue;
            }
            let mut record = Record::new("search_result", "web", destination.as_str());
            record.title = Some(anchor.text().collect::<String>().trim().into());
            record.text = result
                .select(&extract::selector(".result__snippet")?)
                .next()
                .map(|e| e.text().collect::<String>().trim().into());
            record.provenance.method = "duckduckgo-html".into();
            record.provenance.source_url = response.url.to_string();
            run.items.push(record);
            if run.items.len() >= limit {
                break;
            }
        }
        ensure!(
            !run.items.is_empty()
                || doc
                    .select(&extract::selector(".no-results,.no-results__message")?)
                    .next()
                    .is_some(),
            "web provider returned no recognized results or empty-result marker; possible challenge or changed markup"
        );
        run.meta["pagination"] = json!("first provider page");
        Ok(())
    }
}

fn enqueue_children(record: &Record, queue: &mut VecDeque<u64>) {
    if let Some(ids) = record.content["child_ids"].as_array() {
        queue.extend(ids.iter().filter_map(Value::as_u64));
    }
}

pub fn hn_id(url: &Url) -> Option<u64> {
    if url.host_str() != Some("news.ycombinator.com") || url.path() != "/item" {
        return None;
    }
    url.query_pairs()
        .find(|(key, _)| key == "id")
        .and_then(|(_, v)| v.parse().ok())
}
