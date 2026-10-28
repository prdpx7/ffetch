//! Bounded traversal of DOI references reported by Crossref, not inferred links.
use crate::{
    model::{Attempt, Issue, Record, Run},
    net::{self, Http},
    research,
};
use anyhow::{Context, Result, ensure};
use futures::{StreamExt, stream::FuturesUnordered};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};
use url::Url;

pub fn doi(input: &str) -> Option<String> {
    research::doi_identity(input.trim()).map(|key| key[4..].to_owned())
}

fn endpoint() -> Result<Url> {
    net::normalize(
        &std::env::var("FFETCH_CROSSREF_WORKS")
            .unwrap_or_else(|_| "https://api.crossref.org/works".into()),
    )
}

fn work_url(base: &Url, doi: &str) -> Result<Url> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid Crossref works base"))?
        .pop_if_empty()
        .push(doi);
    if let Ok(email) = std::env::var("FFETCH_CONTACT_EMAIL") {
        url.query_pairs_mut().append_pair("mailto", &email);
    }
    Ok(url)
}

async fn lookup(http: &Http, url: Url, expected: &str) -> Result<Record> {
    let data = http.json(url.clone()).await?;
    ensure!(
        data["status"] == "ok" && data["message-type"] == "work",
        "Crossref returned an unexpected work envelope"
    );
    let mut record = crate::scholarly::crossref_record(&data["message"])?;
    ensure!(
        record.content["doi"].as_str().and_then(doi).as_deref() == Some(expected),
        "Crossref DOI does not match the requested work"
    );
    record.native = data["message"].clone();
    record.provenance.source_url = url.into();
    record.provenance.method = "crossref-work-api".into();
    Ok(record)
}

pub async fn references(
    http: &Http,
    seed: &str,
    depth: u32,
    limit: usize,
    concurrency: usize,
    budget: Duration,
) -> Run {
    let started = Instant::now();
    let mut run = Run::new("references");
    let mut states = HashMap::new();
    let mut queue = VecDeque::new();
    let mut edges = Vec::new();
    let mut inspected = 0;
    let mut budget_limited = false;
    let mut depth_limited = 0;
    let mut unknown_lists = 0;
    let task = async {
        ensure!((1..=3).contains(&depth), "citation depth must be 1-3");
        let seed = doi(seed).context("references requires a DOI or DOI resolver URL")?;
        let base = endpoint()?;
        run.meta = json!({"seed_doi":seed,"direction":"references","depth":depth,"reference_budget":limit,"representation":"Crossref metadata, not full text or independently verified citations"});
        states.insert(seed.clone(), "pending");
        queue.push_back((seed, 0));
        let mut pending = FuturesUnordered::new();
        let mut wave = 0;
        loop {
            while pending.len() < concurrency.max(1)
                && queue.front().is_some_and(|(_, level)| *level == wave)
            {
                let Some((id, level)) = queue.pop_front() else {
                    break;
                };
                let url = work_url(&base, &id)?;
                pending.push(async move {
                    let start = Instant::now();
                    let result = lookup(http, url.clone(), &id).await;
                    (id, level, url, start.elapsed(), result)
                });
            }
            let Some((id, level, url, elapsed, result)) = pending.next().await else {
                if let Some((_, level)) = queue.front() {
                    wave = *level;
                    continue;
                }
                break;
            };
            run.attempts.push(Attempt {
                source: "crossref-work".into(),
                url: url.to_string(),
                elapsed_ms: elapsed.as_millis() as u64,
                outcome: result
                    .as_ref()
                    .map(|_| "ok".into())
                    .unwrap_or_else(|e| e.to_string()),
            });
            match result {
                Err(error) => {
                    states.insert(id, "failed");
                    let mut issue = Issue::new("crossref", "work_lookup_failed", error.to_string());
                    issue.url = Some(url.into());
                    issue.retryable = true;
                    issue.next_action = Some("Use retained works and reference entries; the target may belong to another DOI registration agency or be temporarily unavailable".into());
                    run.errors.push(issue);
                }
                Ok(mut record) => {
                    states.insert(id.clone(), "fetched");
                    record.native["citation_depth"] = json!(level);
                    if let Some(references) = record.content["references"].as_array() {
                        if record.native["reference-count"]
                            .as_u64()
                            .is_some_and(|count| count > references.len() as u64)
                        {
                            unknown_lists += 1;
                        }
                        if level == depth {
                            if !references.is_empty() {
                                depth_limited += 1;
                            }
                        } else {
                            for reference in references {
                                if inspected >= limit {
                                    budget_limited = true;
                                    break;
                                }
                                inspected += 1;
                                let target = reference["DOI"].as_str().and_then(doi);
                                if let Some(target) = &target
                                    && !states.contains_key(target)
                                {
                                    states.insert(target.clone(), "pending");
                                    queue.push_back((target.clone(), level + 1));
                                }
                                edges.push(json!({"from_doi":id,"to_doi":target,"depth":level+1,"reference":reference,"provenance":record.provenance,"lookup_status":if target.is_some(){"pending"}else{"unresolved_doi"}}));
                            }
                        }
                    } else if record.native["reference-count"].as_u64() != Some(0) {
                        unknown_lists += 1;
                    }
                    run.items.push(record);
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    match net::within_budget(budget, task).await {
        Ok(Ok(())) => (),
        Ok(Err(error)) => run.errors.push(Issue::new(
            "crossref",
            "reference_traversal_failed",
            error.to_string(),
        )),
        Err(_) => {
            let mut issue = Issue::new(
                "crossref",
                "deadline_exceeded",
                "citation traversal exceeded --timeout; completed works and reported references retained",
            );
            issue.retryable = true;
            issue.next_action = Some("Increase --timeout or retry pending DOIs from meta.edges; completed works remain available".into());
            run.errors.push(issue);
        }
    }
    for edge in &mut edges {
        if let Some(target) = edge["to_doi"].as_str() {
            edge["lookup_status"] = json!(states.get(target).copied().unwrap_or("pending"));
        }
    }
    run.meta["references_inspected"] = json!(inspected);
    run.meta["reference_budget_exhausted"] = json!(budget_limited);
    run.meta["depth_limited_works"] = json!(depth_limited);
    run.meta["unavailable_reference_lists"] = json!(unknown_lists);
    run.meta["fetched_works"] = json!(run.items.len());
    run.meta["pending_works"] = json!(states.values().filter(|&&s| s == "pending").count());
    run.meta["failed_works"] = json!(states.values().filter(|&&s| s == "failed").count());
    run.meta["coverage"] = json!(
        "bounded provider reference metadata; missing DOI/reference lists are not proof of absent citations"
    );
    run.meta["edges"] = json!(edges);
    let unresolved = edges.iter().filter(|edge| edge["to_doi"].is_null()).count();
    run.meta["unresolved_references"] = json!(unresolved);
    if unresolved > 0 {
        run.warnings.push(format!("{unresolved} inspected references have no usable DOI; original entries remain in meta.edges"));
    }
    if budget_limited {
        run.warnings.push("Reference-entry budget reached; additional entries remain in the retrieved works' raw metadata".into());
    }
    if unknown_lists > 0 {
        run.warnings.push("Some works have no available reference list; citation coverage is unknown for those works".into());
    }
    run.finish(started.elapsed().as_millis() as u64);
    run
}
