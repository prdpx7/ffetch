use crate::{
    browser::{Mode, Renderer},
    extract::{self, Extract, Options},
    model::{Attempt, Issue, Run},
    net::{self, Http, Response},
    pdf,
    store::Store,
};
use anyhow::{Result, anyhow, ensure};
use clap::ValueEnum;
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use url::{Host, Url};

#[derive(Clone, Copy, Debug, Default, ValueEnum, PartialEq)]
pub enum Archive {
    #[default]
    Auto,
    Off,
    LatestValid,
}

#[derive(Clone)]
pub struct Fetcher {
    pub http: Http,
    pub browser: Option<Renderer>,
    pub browser_mode: Mode,
    pub store: Option<Store>,
    pub budget: Duration,
    pub cdx: Url,
    pub replay: Url,
    pub archive_limit: usize,
    /// Path to ffetch itself, not a library host's current executable.
    pub pdf_worker: Option<PathBuf>,
}

impl Fetcher {
    pub async fn fetch(&self, input: &str, options: &Options, archive: Archive) -> Run {
        let start = Instant::now();
        let mut run = Run::new("fetch");
        run.meta =
            json!({"requested_url":input, "archive_policy":format!("{archive:?}").to_lowercase()});
        match net::normalize(input) {
            Err(e) => run
                .errors
                .push(Issue::new("web", "invalid_url", e.to_string())),
            Ok(url) => {
                if crate::net::within_budget(
                    self.budget,
                    self.resolve(&url, options, archive, &mut run),
                )
                .await
                .is_err()
                {
                    let mut issue = Issue::new(
                        "web",
                        "deadline_exceeded",
                        format!("retrieval exceeded {} seconds", self.budget.as_secs_f64()),
                    );
                    issue.url = Some(url.into());
                    issue.retryable = true;
                    issue.next_action = Some(
                        "Retry with a larger --timeout; completed items and attempts are preserved"
                            .into(),
                    );
                    run.errors.push(issue);
                }
            }
        }
        run.finish(start.elapsed().as_millis() as u64);
        run
    }

    async fn resolve(&self, url: &Url, options: &Options, archive: Archive, run: &mut Run) {
        let forced = self.browser_mode == Mode::Always
            || self.browser.as_ref().is_some_and(|r| r.session.is_some());
        let mut render_needed = forced;
        if archive != Archive::LatestValid && !forced {
            let start = Instant::now();
            match self.http.get(url.clone()).await.and_then(validate) {
                Ok(response) => match self.ingest(response, options, None, run).await {
                    Ok(()) => {
                        record_attempt(run, "http", url, "ok", start);
                        return;
                    }
                    Err(e) => {
                        record_attempt(run, "http", url, &format!("extraction_error: {e}"), start);
                        run.errors
                            .push(Issue::new("web", "extraction_error", e.to_string()));
                        return;
                    }
                },
                Err(e) => {
                    let reason = e.to_string();
                    render_needed = !reason.contains("Retry-After")
                        && !reason.contains("HTTP 429")
                        && !reason.contains("HTTP 503")
                        && (reason.contains("challenge")
                            || reason.contains("javascript_required")
                            || reason.contains("403")
                            || reason.contains("401")
                            || reason.contains("request failed")
                            || reason.contains("request deadline"));
                    record_attempt(run, "http", url, &reason, start);
                }
            }
        }
        if archive != Archive::LatestValid
            && render_needed
            && let Some(renderer) = &self.browser
        {
            let start = Instant::now();
            if renderer.available() {
                // Leave part of the overall budget for archive lookup and cleanup.
                let allowance = self.budget.mul_f64(0.7);
                match crate::net::within_budget(allowance, renderer.render(url)).await {
                    Ok(Ok(rendered)) => {
                        let observed = rendered.status_observed;
                        run.warnings.extend(rendered.warnings);
                        let ingested = match validate(rendered.response) {
                            Ok(response) => self.ingest(response, options, None, run).await,
                            Err(e) => Err(e),
                        };
                        match ingested {
                            Ok(()) => {
                                for record in &mut run.items {
                                    if record.kind != "pdf_page" {
                                        record.provenance.method = "browser-dom".into();
                                    }
                                }
                                run.meta["representation"] =
                                    json!("rendered DOM; not original HTTP response bytes");
                                if !observed {
                                    run.meta["http_status"] = serde_json::Value::Null;
                                }
                                record_attempt(run, "browser", url, "ok", start);
                                return;
                            }
                            Err(error) => {
                                record_attempt(run, "browser", url, &error.to_string(), start)
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        record_attempt(run, "browser", url, &format!("{error:#}"), start)
                    }
                    Err(_) => record_attempt(
                        run,
                        "browser",
                        url,
                        "browser stage deadline exceeded",
                        start,
                    ),
                }
            } else {
                record_attempt(
                    run,
                    "browser",
                    url,
                    "Chromium unavailable; install Chrome/Chromium or set --browser-path",
                    start,
                );
            }
        }
        if archive != Archive::Off && public_archive_candidate(url) {
            match self.recover(url, options, run).await {
                Ok(()) => return,
                Err(e) => {
                    let mut issue = Issue::new("wayback", "archive_unavailable", e.to_string());
                    issue.url = Some(url.to_string());
                    issue.retryable = true;
                    run.errors.push(issue);
                }
            }
        } else if archive != Archive::Off {
            run.warnings.push("Archive lookup skipped for a local/private URL or a URL containing credential-like query parameters".into());
        }
        let mut issue = Issue::new(
            "web",
            "retrieval_failed",
            "No validated document was retrieved; inspect attempts for the original failure",
        );
        issue.url = Some(url.to_string());
        issue.next_action = Some("Inspect attempts; retry with an authorized browser session, another source, or a larger timeout".into());
        run.errors.push(issue);
    }

    async fn ingest(
        &self,
        response: Response,
        options: &Options,
        capture: Option<(&str, &str)>,
        run: &mut Run,
    ) -> Result<()> {
        let artifact = self
            .store
            .as_ref()
            .map(|s| s.blob(&response.bytes))
            .transpose()?;
        let final_url = response.url.clone();
        run.meta["http_status"] = json!(response.status.as_u16());
        run.meta["final_url"] = json!(final_url.as_str());
        run.meta["artifact"] = json!(artifact);
        let mut extracted = Run::new("extract");
        if pdf::is_pdf(&response) {
            ensure!(
                options.css.is_none() && matches!(options.mode, Extract::Article | Extract::Text),
                "PDF supports --extract article or text without --css; use pdf to select physical pages"
            );
            let executable = self.pdf_worker.as_deref().ok_or_else(|| {
                anyhow!(
                    "PDF worker is not configured; set Fetcher.pdf_worker to the ffetch executable"
                )
            })?;
            let display_url = capture
                .map(|(original, _)| Url::parse(original))
                .transpose()?
                .unwrap_or_else(|| final_url.clone());
            run.meta["selected_representation"] = json!("pdf-text-layer");
            extracted = pdf::parse_response(
                response,
                &display_url,
                1,
                pdf::DEFAULT_PAGES,
                artifact.clone(),
                executable,
                self.store.as_ref(),
            )
            .await?;
            run.meta["pdf"] = extracted.meta.clone();
        } else {
            extracted.items = if let Some((original, _)) = capture {
                extract::extract_at(&response, options, &Url::parse(original)?)?
            } else {
                extract::extract(&response, options)?
            };
            if extract::is_html(&response) && std::str::from_utf8(&response.bytes).is_err() {
                extracted.warnings.push("HTML is not UTF-8; text was decoded lossily. Original bytes are retained when storage is enabled".into());
            }
        }
        for record in &mut extracted.items {
            record.provenance.artifact.clone_from(&artifact);
            if let Some((original, timestamp)) = capture {
                record.source = "wayback".into();
                if record.kind != "pdf_page" {
                    record.provenance.method = "archive".into();
                }
                record.provenance.source_url = original.into();
                record.provenance.archive_url = Some(final_url.to_string());
                record.provenance.captured_at = Some(timestamp.into());
                if record.kind != "link" && record.kind != "pdf_page" {
                    record.url = original.into();
                }
            }
        }
        run.merge(extracted);
        Ok(())
    }

    async fn recover(&self, url: &Url, options: &Options, run: &mut Run) -> Result<()> {
        let start = Instant::now();
        let mut query = self.cdx.clone();
        query.query_pairs_mut().extend_pairs([
            ("url", url.as_str()),
            ("output", "json"),
            ("fl", "timestamp,original,statuscode,mimetype"),
            ("filter", "statuscode:200"),
            ("matchType", "exact"),
            ("limit", &format!("-{}", self.archive_limit)),
        ]);
        let index = match self.http.json(query).await {
            Ok(index) => index,
            Err(e) => {
                record_attempt(run, "wayback-cdx", url, &e.to_string(), start);
                return Err(e);
            }
        };
        record_attempt(run, "wayback-cdx", url, "index_received", start);
        let rows = index
            .as_array()
            .ok_or_else(|| anyhow!("CDX returned an unexpected schema"))?;
        let mut captures = rows
            .iter()
            .filter_map(|row| {
                let stamp = row.get(0)?.as_str()?;
                let original = row.get(1)?.as_str()?;
                if stamp.len() == 14 && stamp.bytes().all(|c| c.is_ascii_digit()) {
                    Some((stamp.to_owned(), original.to_owned()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        captures.sort_by(|a, b| b.0.cmp(&a.0));
        captures.dedup();
        run.meta["archive"] = json!({"providers_checked":["wayback"],"candidate_count":captures.len(),"candidate_limit":self.archive_limit,"selection":"latest valid among checked captures","global_latest_verified":false});
        ensure!(
            !captures.is_empty(),
            "No successful captures found in the queried Wayback index"
        );
        for (stamp, original) in captures.into_iter().take(self.archive_limit) {
            let start = Instant::now();
            let replay_url = self.replay.join(&format!("web/{stamp}id_/{original}"))?;
            match self.http.get(replay_url.clone()).await.and_then(validate) {
                Ok(response) => {
                    // Replay can redirect to a different capture. Do not mislabel it.
                    let actual_stamp = response
                        .url
                        .path()
                        .split('/')
                        .nth(2)
                        .unwrap_or("")
                        .trim_end_matches("id_");
                    if response.url.host_str() != self.replay.host_str() || actual_stamp != stamp {
                        record_attempt(
                            run,
                            "wayback",
                            &replay_url,
                            "replay redirected outside the requested capture",
                            start,
                        );
                        continue;
                    }
                    let time = chrono::NaiveDateTime::parse_from_str(&stamp, "%Y%m%d%H%M%S")?
                        .and_utc()
                        .to_rfc3339();
                    let mut candidate = Run::new("fetch");
                    match self
                        .ingest(
                            response,
                            options,
                            Some((url.as_str(), &time)),
                            &mut candidate,
                        )
                        .await
                    {
                        Ok(())
                            if candidate.meta["selected_representation"] != "pdf-text-layer"
                                || !candidate.items.is_empty() =>
                        {
                            if let Some(meta) = candidate.meta.as_object() {
                                for (key, value) in meta {
                                    run.meta[key.as_str()] = value.clone();
                                }
                            }
                            run.merge(candidate);
                        }
                        result => {
                            let reason = result.err().map(|e| e.to_string()).unwrap_or_else(|| {
                                candidate
                                    .errors
                                    .iter()
                                    .map(|e| e.message.as_str())
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            });
                            record_attempt(
                                run,
                                "wayback",
                                &replay_url,
                                &format!("unusable_capture: {reason}"),
                                start,
                            );
                            continue;
                        }
                    }
                    record_attempt(run, "wayback", &replay_url, "ok", start);
                    run.warnings.push(format!(
                        "Historical content captured at {time}; this is not a live response"
                    ));
                    return Ok(());
                }
                Err(e) => record_attempt(run, "wayback", &replay_url, &e.to_string(), start),
            }
        }
        Err(anyhow!("No usable capture among the checked candidates"))
    }
}

fn record_attempt(run: &mut Run, source: &str, url: &Url, outcome: &str, start: Instant) {
    run.attempts.push(Attempt {
        source: source.into(),
        url: url.to_string(),
        outcome: outcome.into(),
        elapsed_ms: start.elapsed().as_millis() as u64,
    });
}

fn validate(response: Response) -> Result<Response> {
    ensure!(
        response.status.is_success(),
        "HTTP {}{}",
        response.status.as_u16(),
        response
            .retry_after
            .as_ref()
            .map(|s| format!("; Retry-After: {s}"))
            .unwrap_or_default()
    );
    ensure!(!response.bytes.is_empty(), "empty_response");
    // A PDF MIME type can also hide an HTML challenge. Actual PDF bytes must
    // never enter the HTML parser, regardless of the server's MIME type.
    if !response.bytes.starts_with(b"%PDF-")
        && extract::is_html(&response)
        && let Some(reason) = extract::invalid_content(&String::from_utf8_lossy(&response.bytes))
    {
        return Err(anyhow!(reason));
    }
    Ok(response)
}

pub fn public_archive_candidate(url: &Url) -> bool {
    let public_host = match url.host() {
        Some(Host::Ipv4(ip)) => {
            !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified())
        }
        Some(Host::Ipv6(ip)) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local())
        }
        Some(Host::Domain(d)) => {
            d.contains('.')
                && ![".localhost", ".local", ".internal"]
                    .iter()
                    .any(|suffix| d.ends_with(suffix))
        }
        None => false,
    };
    public_host
        && !url.query_pairs().any(|(key, _)| {
            let key = key.to_lowercase();
            [
                "token",
                "password",
                "secret",
                "signature",
                "credential",
                "api_key",
                "apikey",
                "authorization",
            ]
            .iter()
            .any(|s| key.contains(s))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archives_do_not_receive_local_or_credential_urls() {
        for url in [
            "http://localhost/a",
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://10.0.0.4/",
            "https://example.com/?access_token=secret",
        ] {
            assert!(!public_archive_candidate(&Url::parse(url).unwrap()));
        }
        assert!(public_archive_candidate(
            &Url::parse("https://example.com/paper?id=42").unwrap()
        ));
    }
}
