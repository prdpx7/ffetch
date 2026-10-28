//! Native HTML/JSON adapters. Layout mismatches fail visibly; no external tools.
use crate::{
    extract,
    model::{Issue, Record, Run},
    providers::Providers,
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde_json::json;
use url::Url;

pub async fn search(
    providers: &Providers,
    engine: &str,
    query: &str,
    limit: usize,
    run: &mut Run,
) -> Result<()> {
    match engine {
        "brave" => return providers.search_brave(query, limit, run).await,
        "duckduckgo" => return providers.search_ddg(query, limit, run).await,
        "searxng" => return searxng(providers, query, limit, run).await,
        _ => (),
    }
    let (endpoint, parameter, container, anchor, snippet) = match engine {
        "google" => (
            "https://www.google.com/search",
            "q",
            "div.MjjYud,div.g",
            "a[href]",
            ".VwiC3b,.IsZvec",
        ),
        "bing" => (
            "https://www.bing.com/search",
            "q",
            "li.b_algo",
            "h2 a[href]",
            ".b_caption p",
        ),
        "yandex" => (
            "https://yandex.com/search/",
            "text",
            "li.serp-item,.serp-item",
            "a.OrganicTitle-Link,a.Link.OrganicTitle-Link",
            ".OrganicTextContentSpan,.Organic-Text",
        ),
        "baidu" => (
            "https://www.baidu.com/s",
            "wd",
            "div.result",
            "h3 a[href]",
            ".c-abstract,.content-right_8Zs40",
        ),
        _ => anyhow::bail!("unknown engine {engine}"),
    };
    let variable = format!("FFETCH_{}_SEARCH", engine.to_ascii_uppercase());
    let mut url = Url::parse(&std::env::var(variable).unwrap_or_else(|_| endpoint.into()))?;
    url.query_pairs_mut().append_pair(parameter, query);
    run.meta["search_url"] = json!(url.as_str());
    let response = providers.search_response(url).await?;
    ensure!(
        response.status.is_success(),
        "{engine} returned HTTP {}",
        response.status
    );
    let body = String::from_utf8_lossy(&response.bytes);
    ensure!(
        extract::invalid_content(&body).is_none(),
        "{engine} returned a challenge or error page"
    );
    let doc = scraper::Html::parse_document(&body);
    let containers = extract::selector(container)?;
    let anchors = extract::selector(anchor)?;
    let snippets = extract::selector(snippet)?;
    let headings = extract::selector("h3")?;
    let mut seen = std::collections::HashSet::new();
    for item in doc.select(&containers) {
        let selected = item
            .select(&anchors)
            .find(|a| engine != "google" || a.select(&headings).next().is_some());
        let Some(selected) = selected else {
            continue;
        };
        let mut link = response
            .url
            .join(selected.value().attr("href").unwrap_or_default())?;
        if engine == "google"
            && link.path() == "/url"
            && let Some(destination) = link
                .query_pairs()
                .find(|(k, _)| k == "q" || k == "url")
                .map(|(_, v)| v.into_owned())
        {
            link = Url::parse(&destination)?;
        }
        if engine == "bing" && link.path() == "/ck/a" {
            let encoded = link
                .query_pairs()
                .find(|(k, _)| k == "u")
                .map(|(_, v)| v.into_owned())
                .context("Bing redirect missing destination")?;
            let encoded = encoded
                .strip_prefix("a1")
                .context("unknown Bing redirect encoding")?;
            let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.trim_end_matches('='))?;
            link = Url::parse(std::str::from_utf8(&decoded)?)?;
        }
        if engine == "baidu"
            && let Some(tools) = item
                .value()
                .attr("data-tools")
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            && let Some(destination) = tools["url"].as_str()
        {
            link = Url::parse(destination)?;
        }
        if !matches!(link.scheme(), "http" | "https")
            || link.host_str() == response.url.host_str()
            || !seen.insert(extract::canonical(link.as_str()))
        {
            continue;
        }
        let title = if engine == "google" {
            selected
                .select(&headings)
                .next()
                .unwrap()
                .text()
                .collect::<String>()
        } else {
            selected.text().collect::<String>()
        };
        if title.trim().is_empty() {
            continue;
        }
        let mut record = Record::new("search_result", "web", link.as_str());
        record.title = Some(title.trim().into());
        record.text = item
            .select(&snippets)
            .next()
            .map(|s| s.text().collect::<String>().trim().into());
        record.provenance.method = format!("{engine}-html");
        record.provenance.source_url = response.url.to_string();
        run.items.push(record);
        if run.items.len() >= limit {
            break;
        }
    }
    ensure!(
        !run.items.is_empty(),
        "{engine} returned no recognized organic results; empty results, challenge, consent, or changed markup requires inspection"
    );
    run.meta["pagination"] = json!("first engine page");
    run.meta["adapter_validation"] = json!("layout-based; live availability varies");
    Ok(())
}

async fn searxng(providers: &Providers, query: &str, limit: usize, run: &mut Run) -> Result<()> {
    let mut url = providers
        .searxng
        .as_ref()
        .context("searxng needs FFETCH_SEARXNG_URL")?
        .join("search")?;
    url.query_pairs_mut()
        .extend_pairs([("q", query), ("format", "json")]);
    let data = providers.http.json(url.clone()).await?;
    let results = data["results"]
        .as_array()
        .context("SearXNG returned an unexpected schema")?;
    for item in results.iter().take(limit) {
        let Some(link) = item["url"]
            .as_str()
            .and_then(|s| Url::parse(s).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        else {
            continue;
        };
        let mut record = Record::new("search_result", "web", link.as_str());
        record.title = item["title"].as_str().map(str::to_owned);
        record.text = item["content"].as_str().map(str::to_owned);
        record.native = item.clone();
        record.provenance.method = "searxng".into();
        record.provenance.source_url = url.to_string();
        run.items.push(record);
    }
    if let Some(errors) = data["unresponsive_engines"]
        .as_array()
        .filter(|a| !a.is_empty())
    {
        run.errors.push(Issue::new(
            "searxng",
            "upstream_engines_failed",
            format!("Unavailable engines: {errors:?}"),
        ));
    }
    ensure!(
        results.is_empty() || !run.items.is_empty(),
        "SearXNG results had no valid HTTP(S) destinations"
    );
    Ok(())
}
