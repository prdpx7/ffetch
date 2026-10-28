//! YouTube discovery and public metadata/captions. Embedded JSON is parsed, never executed.
use crate::{
    model::{Attempt, Record, Run},
    net::Http,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Instant};
use url::Url;

pub fn video_id(input: &str) -> Option<String> {
    let url = Url::parse(input).ok()?;
    let value = match url.host_str()? {
        "youtu.be" => url.path_segments()?.next()?.to_string(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" => {
            if url.path() == "/watch" {
                url.query_pairs().find(|(k, _)| k == "v")?.1.into_owned()
            } else {
                let mut p = url.path_segments()?;
                if !matches!(p.next()?, "shorts" | "live" | "embed") {
                    return None;
                }
                p.next()?.into()
            }
        }
        _ => return None,
    };
    valid_id(&value).then_some(value)
}
fn valid_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}
fn endpoint(env: &str, default: &str) -> Result<Url> {
    crate::net::normalize(&std::env::var(env).unwrap_or_else(|_| default.into()))
}
async fn html(http: &Http, url: Url, run: &mut Run) -> Result<String> {
    let start = Instant::now();
    let response = http.get(url.clone()).await;
    run.attempts.push(Attempt {
        source: "youtube".into(),
        url: url.to_string(),
        outcome: response
            .as_ref()
            .map(|r| format!("HTTP {}", r.status.as_u16()))
            .unwrap_or_else(|e| e.to_string()),
        elapsed_ms: start.elapsed().as_millis() as u64,
    });
    let response = response?;
    ensure!(
        response.status.is_success(),
        "YouTube returned HTTP {}",
        response.status
    );
    String::from_utf8(response.bytes).context("YouTube HTML was not UTF-8")
}
fn embedded(html: &str, name: &str) -> Result<Value> {
    for (index, _) in html.match_indices(name) {
        let tail = &html[index + name.len()..];
        let Some(equals) = tail.find('=') else {
            continue;
        };
        if equals > 16 {
            continue;
        }
        let tail = tail[equals + 1..].trim_start();
        if !tail.starts_with('{') {
            continue;
        }
        if let Some(Ok(value)) = serde_json::Deserializer::from_str(tail)
            .into_iter::<Value>()
            .next()
        {
            return Ok(value);
        }
    }
    anyhow::bail!(
        "YouTube {name} JSON unavailable; consent, challenge, unavailable content or changed markup"
    )
}
fn label(value: &Value) -> Option<String> {
    value["simpleText"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            value["runs"].as_array().map(|runs| {
                runs.iter()
                    .filter_map(|r| r["text"].as_str())
                    .collect::<String>()
            })
        })
        .filter(|s| !s.trim().is_empty())
}
fn count(value: &Value) -> Value {
    value
        .as_u64()
        .map(|n| json!(n))
        .or_else(|| {
            value
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .map(|n| json!(n))
        })
        .unwrap_or(Value::Null)
}

pub async fn search(
    http: &Http,
    query: &str,
    limit: usize,
    since: Option<i64>,
    region: &str,
    run: &mut Run,
) -> Result<()> {
    if std::env::var_os("FFETCH_YOUTUBE_API_KEY").is_some() {
        return api(http, Some(query), region, limit, since, run).await;
    }
    let mut url = endpoint("FFETCH_YOUTUBE_SEARCH", "https://www.youtube.com/results")?;
    url.query_pairs_mut().append_pair("search_query", query);
    let body = html(http, url.clone(), run).await?;
    let data = embedded(&body, "ytInitialData")?;
    let root = data
        .pointer("/contents/twoColumnSearchResultsRenderer/primaryContents")
        .unwrap_or(&data["contents"]);
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    let mut total = 0;
    while let Some(value) = stack.pop() {
        if let Some(video) = value.get("videoRenderer") {
            let Some(id) = video["videoId"].as_str().filter(|id| valid_id(id)) else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            total += 1;
            if run.items.len() >= limit {
                continue;
            }
            let mut record = Record::new("video", "youtube", &watch_url(id));
            record.title = label(&video["title"]);
            record.text = label(&video["detailedMetadataSnippets"][0]["snippetText"]);
            record.author = label(&video["ownerText"]).or_else(|| label(&video["longBylineText"]));
            record.metrics =
                json!({"views_label":label(&video["viewCountText"]),"rank":run.items.len()+1});
            record.content = json!({"video_id":id,"duration_label":label(&video["lengthText"]),"publication_label":label(&video["publishedTimeText"]),"transcript_retrieved":false,"article_url":record.url});
            record.native = video.clone();
            record.provenance.method = "youtube-initial-data".into();
            record.provenance.source_url = url.to_string();
            run.items.push(record);
        } else {
            match value {
                Value::Object(map)
                    if ![
                        "adSlotRenderer",
                        "adPlacementRenderer",
                        "promotedVideoRenderer",
                    ]
                    .iter()
                    .any(|key| map.contains_key(*key)) =>
                {
                    stack.extend(map.values().rev());
                }
                Value::Array(values) => stack.extend(values.iter().rev()),
                _ => (),
            }
        }
    }
    ensure!(
        !run.items.is_empty(),
        "YouTube returned no recognized video results; blocked or changed markup is not an empty success"
    );
    run.meta["pagination"] = json!("initial search page only; continuations not implemented");
    run.meta["available_initial_videos"] = json!(total);
    run.meta["truncated"] = json!(total > limit);
    if since.is_some() {
        run.warnings.push("YouTube HTML search exposes relative publication labels; --since is not enforced without Data API access".into());
    }
    Ok(())
}

async fn api(
    http: &Http,
    query: Option<&str>,
    region: &str,
    limit: usize,
    since: Option<i64>,
    run: &mut Run,
) -> Result<()> {
    let key = std::env::var("FFETCH_YOUTUBE_API_KEY")
        .context("YouTube popular rankings require FFETCH_YOUTUBE_API_KEY")?;
    ensure!(!key.trim().is_empty(), "YouTube API key must not be empty");
    ensure!(
        region.len() == 2 && region.bytes().all(|b| b.is_ascii_alphabetic()),
        "YouTube region must be a two-letter country code"
    );
    let base = endpoint(
        "FFETCH_YOUTUBE_API_BASE",
        "https://www.googleapis.com/youtube/v3/",
    )?;
    ensure!(
        base.scheme() == "https"
            && base.host_str() == Some("www.googleapis.com")
            && base.port().is_none()
            && base.path() == "/youtube/v3/",
        "YouTube API credentials require the official HTTPS API origin"
    );
    let mut token = None::<String>;
    let mut tokens = HashSet::new();
    let mut ids = HashSet::new();
    let mut pages = 0;
    loop {
        let mut url = base.join(if query.is_some() { "search" } else { "videos" })?;
        url.query_pairs_mut().extend_pairs([
            (
                "part",
                if query.is_some() {
                    "snippet"
                } else {
                    "snippet,statistics,contentDetails"
                },
            ),
            ("regionCode", region),
            ("maxResults", limit.min(50).to_string().as_str()),
        ]);
        if let Some(query) = query {
            url.query_pairs_mut()
                .extend_pairs([("q", query), ("type", "video")]);
            if let Some(since) = since.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) {
                url.query_pairs_mut()
                    .append_pair("publishedAfter", &since.to_rfc3339());
            }
        } else {
            url.query_pairs_mut().append_pair("chart", "mostPopular");
        }
        if let Some(token) = &token {
            url.query_pairs_mut().append_pair("pageToken", token);
        }
        let public_url = url.to_string();
        url.query_pairs_mut().append_pair("key", &key);
        let start = Instant::now();
        let result = http.json(url).await;
        run.attempts.push(Attempt {
            source: "youtube-api".into(),
            url: public_url.clone(),
            outcome: result
                .as_ref()
                .map(|_| "page_received".into())
                .unwrap_or_else(|e| e.to_string().replace(&key, "[redacted]")),
            elapsed_ms: start.elapsed().as_millis() as u64,
        });
        let data =
            result.map_err(|e| anyhow::anyhow!(e.to_string().replace(&key, "[redacted]")))?;
        let items = data["items"]
            .as_array()
            .context("YouTube API returned no items array")?;
        let before = ids.len();
        pages += 1;
        for item in items {
            let Some(id) = item["id"]["videoId"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .filter(|s| valid_id(s))
            else {
                continue;
            };
            if !ids.insert(id.to_string()) {
                continue;
            }
            let mut record = api_record(item)?;
            record.metrics["rank"] = json!(run.items.len() + 1);
            record.provenance.source_url = public_url.clone();
            run.items.push(record);
            if run.items.len() >= limit {
                break;
            }
        }
        ensure!(
            items.is_empty() || ids.len() > before,
            "YouTube API page contained no new valid video IDs"
        );
        run.meta["pages_fetched"] = json!(pages);
        token = data["nextPageToken"].as_str().map(str::to_owned);
        run.meta["truncated"] = json!(token.is_some());
        if run.items.len() >= limit || token.is_none() || items.is_empty() {
            break;
        }
        ensure!(
            ids.len() > before && tokens.insert(token.clone().unwrap()),
            "YouTube API pagination made no progress"
        );
    }
    Ok(())
}
pub async fn popular(http: &Http, region: &str, limit: usize, run: &mut Run) -> Result<()> {
    run.meta = json!({"source":"youtube","chart":"mostPopular","region":region,"ranking":"platform","momentum_measured":false});
    api(http, None, region, limit, None, run).await
}

pub async fn video(http: &Http, id: &str, language: Option<&str>, run: &mut Run) -> Result<()> {
    ensure!(valid_id(id), "invalid YouTube video ID");
    let mut url = endpoint("FFETCH_YOUTUBE_WATCH", "https://www.youtube.com/watch")?;
    url.query_pairs_mut().append_pair("v", id);
    let body = html(http, url.clone(), run).await?;
    let data = embedded(&body, "ytInitialPlayerResponse")?;
    let details = &data["videoDetails"];
    ensure!(
        details["videoId"] == id,
        "YouTube player returned a different or unavailable video"
    );
    let mut record = Record::new("video", "youtube", &watch_url(id));
    record.title = details["title"].as_str().map(str::to_owned);
    record.text = details["shortDescription"].as_str().map(str::to_owned);
    record.author = details["author"].as_str().map(str::to_owned);
    record.metrics = json!({"views":count(&details["viewCount"])});
    record.published_at = data["microformat"]["playerMicroformatRenderer"]["publishDate"]
        .as_str()
        .map(str::to_owned);
    let tracks = data["captions"]["playerCaptionsTracklistRenderer"]["captionTracks"].as_array();
    let descriptions=tracks.map(|tracks|tracks.iter().map(|t|json!({"language":t["languageCode"],"name":label(&t["name"]),"automatic":t["kind"]=="asr"})).collect::<Vec<_>>()).unwrap_or_default();
    record.content = json!({"video_id":id,"duration_seconds":count(&details["lengthSeconds"]),"channel_id":details["channelId"],"caption_tracks":descriptions,"playability_status":data["playabilityStatus"]["status"],"media_downloaded":false});
    record.native = details.clone();
    record.provenance.method = "youtube-player-metadata".into();
    record.provenance.source_url = url.to_string();
    run.items.push(record);
    if let Some(language) = language {
        let mut candidates = tracks
            .map(|tracks| {
                tracks
                    .iter()
                    .filter(|t| t["languageCode"].as_str() == Some(language))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        candidates.sort_by_key(|t| t["kind"] == "asr");
        ensure!(
            !candidates.is_empty(),
            "Requested caption language is unavailable"
        );
        let mut errors = Vec::new();
        let mut chosen = None;
        for track in candidates.into_iter().take(2) {
            let start = Instant::now();
            let result = caption_track(http, track).await;
            run.attempts.push(Attempt {
                source: "youtube-captions".into(),
                url: watch_url(id),
                outcome: result
                    .as_ref()
                    .map(|_| "caption_segments_received".into())
                    .unwrap_or_else(|e| e.to_string()),
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
            run.meta["caption_tracks_attempted"] = json!(errors.len() + 1);
            match result {
                Ok(segments) => {
                    chosen = Some((track, segments));
                    break;
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        let (track, segments) = chosen
            .ok_or_else(|| anyhow::anyhow!("Caption retrieval failed: {}", errors.join("; ")))?;
        run.warnings.extend(
            errors
                .into_iter()
                .map(|e| format!("Preferred caption track failed: {e}")),
        );
        let mut record = Record::new("transcript", "youtube", &watch_url(id));
        record.text = Some(
            segments
                .iter()
                .filter_map(|s| s["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        record.content = json!({"video_id":id,"language":language,"automatic":track["kind"]=="asr","segments":segments});
        record.provenance.method = "youtube-timedtext".into();
        record.provenance.locator = Some(format!("captions:{language}"));
        run.items.push(record);
    }
    Ok(())
}

async fn caption_track(http: &Http, track: &Value) -> Result<Vec<Value>> {
    let mut url = Url::parse(
        track["baseUrl"]
            .as_str()
            .context("Caption track has no URL")?,
    )?;
    ensure!(
        url.scheme() == "https"
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(
                url.host_str(),
                Some("www.youtube.com" | "youtube.com" | "m.youtube.com" | "video.google.com")
            )
            && matches!(url.path(), "/api/timedtext" | "/timedtext"),
        "Caption URL is not a recognized HTTPS timed-text origin"
    );
    let pairs = url
        .query_pairs()
        .filter(|(k, _)| k != "fmt")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(pairs)
        .append_pair("fmt", "json3");
    let response = http.get(url).await?;
    ensure!(
        response.status.is_success(),
        "Caption endpoint returned HTTP {}",
        response.status
    );
    caption_payload(&response.bytes)
}
fn caption_payload(bytes: &[u8]) -> Result<Vec<Value>> {
    ensure!(
        !bytes.is_empty(),
        "Caption endpoint returned an empty response"
    );
    if let Ok(data) = serde_json::from_slice::<Value>(bytes) {
        return caption_segments(&data);
    }
    let raw = std::str::from_utf8(bytes)
        .context("Caption response was not UTF-8")?
        .trim();
    ensure!(
        raw.starts_with("<transcript") || raw.starts_with("<?xml"),
        "Caption response was not JSON3 or recognized transcript XML"
    );
    let document = scraper::Html::parse_fragment(raw);
    let selector = crate::extract::selector("transcript > text[start]")?;
    let mut segments = Vec::new();
    let ms = |s: &str| -> Option<u64> {
        let seconds = s.parse::<f64>().ok()?;
        (seconds.is_finite() && seconds >= 0.0 && seconds < (u64::MAX / 1000) as f64)
            .then(|| (seconds * 1000.0).round() as u64)
    };
    for node in document.select(&selector) {
        let Some(start) = node.value().attr("start").and_then(ms) else {
            continue;
        };
        let text = node.text().collect::<String>();
        if text.trim().is_empty() {
            continue;
        }
        segments.push(json!({"start_ms":start,"duration_ms":node.value().attr("dur").and_then(ms),"text":text}));
    }
    ensure!(!segments.is_empty(), "Caption XML had no usable timed text");
    Ok(segments)
}

fn caption_segments(data: &Value) -> Result<Vec<Value>> {
    let mut segments = Vec::new();
    for event in data["events"]
        .as_array()
        .context("Caption response has no JSON3 events")?
    {
        let Some(start) = event["tStartMs"].as_u64() else {
            continue;
        };
        let text = event["segs"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(|s| s["utf8"].as_str())
                    .collect::<String>()
            })
            .unwrap_or_default();
        if !text.trim().is_empty() {
            segments.push(json!({"start_ms":start,"duration_ms":event["dDurationMs"],"text":text}));
        }
    }
    ensure!(
        !segments.is_empty(),
        "Caption track returned no usable segments"
    );
    Ok(segments)
}
fn api_record(item: &Value) -> Result<Record> {
    let id = item["id"]["videoId"]
        .as_str()
        .or_else(|| item["id"].as_str())
        .filter(|s| valid_id(s))
        .context("YouTube API record has no valid video ID")?;
    let snippet = &item["snippet"];
    let mut record = Record::new("video", "youtube", &watch_url(id));
    record.title = snippet["title"].as_str().map(str::to_owned);
    record.text = snippet["description"].as_str().map(str::to_owned);
    record.author = snippet["channelTitle"].as_str().map(str::to_owned);
    record.published_at = snippet["publishedAt"].as_str().map(str::to_owned);
    record.metrics = json!({"views":count(&item["statistics"]["viewCount"]),"likes":count(&item["statistics"]["likeCount"]),"comments":count(&item["statistics"]["commentCount"])});
    record.content = json!({"video_id":id,"channel_id":snippet["channelId"],"duration":item["contentDetails"]["duration"],"article_url":record.url});
    record.native = item.clone();
    record.provenance.method = "youtube-data-api".into();
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_json_is_parsed_without_executing_trailing_script() {
        let data = embedded(
            r#"var ytInitialData = {"text":"brace } and \"quote\"", "number":2}; throw new Error('do not execute');"#,
            "ytInitialData",
        );
        // Separate escaped-string example prevents a brace scanner from truncating values.
        assert!(data.is_ok());
        assert_eq!(
            embedded(
                "window[\"ytInitialData\"] = {\"ok\":true}; badCode()",
                "ytInitialData"
            )
            .unwrap()["ok"],
            true
        );
        assert!(embedded("var unrelated={};", "ytInitialData").is_err());
    }
    #[test]
    fn supported_video_urls_and_caption_timestamps() {
        for url in [
            "https://youtu.be/abcdefghijk",
            "https://www.youtube.com/watch?v=abcdefghijk",
            "https://youtube.com/shorts/abcdefghijk",
        ] {
            assert_eq!(video_id(url).as_deref(), Some("abcdefghijk"));
        }
        assert!(video_id("https://evil.example/watch?v=abcdefghijk").is_none());
        let segments=caption_segments(&json!({"events":[{"tStartMs":1200,"dDurationMs":900,"segs":[{"utf8":"Robot "},{"utf8":"gripper"}]},{"tStartMs":2100}]})).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0]["text"], "Robot gripper");
        assert_eq!(segments[0]["start_ms"], 1200);
        assert!(caption_segments(&json!({"events":[]})).is_err());
        let xml = caption_payload(
            b"<transcript><text start='1.25' dur='0.5'>Robot &amp; gripper</text></transcript>",
        )
        .unwrap();
        assert_eq!(xml[0]["start_ms"], 1250);
        assert_eq!(xml[0]["text"], "Robot & gripper");
        assert!(caption_payload(b"").is_err());
    }
    #[test]
    fn official_video_record_preserves_metrics_without_guessing() {
        let r=api_record(&json!({"id":"abcdefghijk","snippet":{"title":"Robot","publishedAt":"2025-01-02T03:04:05Z"},"statistics":{"viewCount":"1234","likeCount":"unknown"},"contentDetails":{"duration":"PT2M"}})).unwrap();
        assert_eq!(r.metrics["views"], 1234);
        assert!(r.metrics["likes"].is_null());
        assert_eq!(r.content["duration"], "PT2M");
    }
}
