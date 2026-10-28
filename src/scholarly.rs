//! Scholarly metadata from native JSON services; no model or external runtime.
use crate::{
    extract,
    model::{Attempt, Record, Run},
    net::Http,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Instant};
use url::Url;

pub async fn search(
    http: &Http,
    source: &str,
    query: &str,
    limit: usize,
    since: Option<i64>,
    run: &mut Run,
) -> Result<()> {
    let crossref = source == "crossref";
    let endpoint = if crossref {
        ("FFETCH_CROSSREF_SEARCH", "https://api.crossref.org/works")
    } else {
        (
            "FFETCH_EUROPEPMC_SEARCH",
            "https://www.ebi.ac.uk/europepmc/webservices/rest/search",
        )
    };
    let base = Url::parse(&std::env::var(endpoint.0).unwrap_or_else(|_| endpoint.1.into()))?;
    let mut cursor = "*".to_string();
    let mut cursors = HashSet::new();
    let mut ids = HashSet::new();
    let page_size = limit.clamp(1, 100);
    let date = since
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|d| d.format("%Y-%m-%d").to_string());
    if date.is_some() {
        run.warnings.push("Publication filtering uses calendar dates; source metadata does not establish exact timestamp precision".into());
    }
    run.meta["date_filter"] =
        json!({"from":date,"precision":"calendar-day","field":"publication date"});
    run.meta["representation"] =
        json!("source metadata and available abstracts; not verified full text");
    let mut pages = 0;
    loop {
        ensure!(
            cursors.insert(cursor.clone()),
            "provider repeated its cursor; pagination stopped"
        );
        let mut url = base.clone();
        if crossref {
            url.query_pairs_mut().extend_pairs([
                ("query.bibliographic", query),
                ("sort", "score"),
                ("order", "desc"),
                ("rows", &page_size.to_string()),
                ("cursor", &cursor),
            ]);
            if let Some(date) = &date {
                url.query_pairs_mut()
                    .append_pair("filter", &format!("from-pub-date:{date}"));
            }
            if let Ok(email) = std::env::var("FFETCH_CONTACT_EMAIL") {
                url.query_pairs_mut().append_pair("mailto", &email);
            }
        } else {
            let mut expression = format!("({query})");
            if source == "pubmed" {
                expression.push_str(" AND SRC:MED");
            }
            if let Some(date) = &date {
                expression.push_str(&format!(" AND FIRST_PDATE:[{date} TO *]"));
            }
            url.query_pairs_mut().extend_pairs([
                ("query", expression.as_str()),
                ("format", "json"),
                ("resultType", "core"),
                ("pageSize", &page_size.to_string()),
                ("cursorMark", &cursor),
            ]);
        }
        let start = Instant::now();
        let result = http.json(url.clone()).await;
        run.attempts.push(Attempt {
            source: source.into(),
            url: url.to_string(),
            outcome: result
                .as_ref()
                .map(|_| "page_received".into())
                .unwrap_or_else(|e| e.to_string()),
            elapsed_ms: start.elapsed().as_millis() as u64,
        });
        let data = result?;
        if crossref {
            run.meta["query_field"] = json!("query.bibliographic");
            run.meta["provider_query"] = data["message"]["query"].clone();
        }
        let entries = if crossref {
            data["message"]["items"].as_array()
        } else {
            data["resultList"]["result"].as_array()
        }
        .context("scholarly provider returned an unexpected result schema")?;
        pages += 1;
        run.meta["pages_fetched"] = json!(pages);
        run.meta["total_hits"] = if crossref {
            data["message"]["total-results"].clone()
        } else {
            data["hitCount"].clone()
        };
        let before = run.items.len();
        for entry in entries {
            let mut record = if crossref {
                crossref_record(entry)?
            } else {
                europepmc_record(entry, source)?
            };
            let identity = crate::research::identity(&record).unwrap_or_else(|| record.url.clone());
            if !ids.insert(identity) {
                continue;
            }
            record.metrics["rank"] = json!(run.items.len() + 1);
            record.native = entry.clone();
            record.provenance.method = if crossref {
                "crossref-api"
            } else {
                "europepmc-api"
            }
            .into();
            record.provenance.source_url = url.to_string();
            record.provenance.locator =
                Some(format!("page:{pages}:record:{}", run.items.len() + 1));
            run.items.push(record);
            if run.items.len() >= limit {
                break;
            }
        }
        let next = if crossref {
            data["message"]["next-cursor"].as_str()
        } else {
            data["nextCursorMark"].as_str()
        };
        let exhausted = entries.is_empty() || entries.len() < page_size || next.is_none();
        run.meta["truncated"] = json!(!exhausted && run.items.len() >= limit);
        if run.items.len() >= limit || exhausted {
            break;
        }
        ensure!(
            run.items.len() > before,
            "pagination made no progress; retained earlier records"
        );
        cursor = next.unwrap().to_owned();
    }
    Ok(())
}

fn doi_url(doi: &str) -> Result<String> {
    let mut url = Url::parse("https://doi.org/")?;
    url.set_path(doi);
    Ok(url.into())
}
fn plain(value: Option<&str>) -> Option<String> {
    value.map(extract::text).filter(|s| !s.is_empty())
}
pub(crate) fn crossref_record(item: &Value) -> Result<Record> {
    let doi = item["DOI"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Crossref result missing DOI")?;
    let kind = match item["type"].as_str() {
        Some(
            "journal-article"
            | "proceedings-article"
            | "posted-content"
            | "book-chapter"
            | "dissertation",
        ) => "paper",
        _ => "research_work",
    };
    let mut record = Record::new(kind, "crossref", &doi_url(doi)?);
    record.title = plain(item["title"][0].as_str());
    record.text = plain(item["abstract"].as_str());
    record.author = item["author"]
        .as_array()
        .map(|authors| {
            authors
                .iter()
                .map(|a| {
                    a["name"].as_str().map(str::to_owned).unwrap_or_else(|| {
                        format!(
                            "{} {}",
                            a["given"].as_str().unwrap_or(""),
                            a["family"].as_str().unwrap_or("")
                        )
                        .trim()
                        .to_owned()
                    })
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|s| !s.is_empty());
    let parts = item["published"]["date-parts"][0]
        .as_array()
        .or_else(|| item["issued"]["date-parts"][0].as_array());
    record.published_at = parts
        .map(|p| {
            p.iter()
                .filter_map(Value::as_u64)
                .enumerate()
                .map(|(i, n)| {
                    if i == 0 {
                        format!("{n:04}")
                    } else {
                        format!("{n:02}")
                    }
                })
                .collect::<Vec<_>>()
                .join("-")
        })
        .filter(|s| !s.is_empty());
    record.metrics =
        json!({"citations":item["is-referenced-by-count"],"provider_score":item["score"]});
    record.content = json!({"doi":doi,"publication_type":item["type"],"journal":item["container-title"],"publisher":item["publisher"],"authors":item["author"],"links":item["link"],"references":item["reference"],"relations":item["relation"],"licenses":item["license"],"abstract_available":record.text.is_some(),"full_text_retrieved":false,"article_url":record.url});
    Ok(record)
}
fn europepmc_record(item: &Value, source: &str) -> Result<Record> {
    let id = item["id"]
        .as_str()
        .context("Europe PMC result missing ID")?;
    let origin = item["source"]
        .as_str()
        .context("Europe PMC result missing source")?;
    let mut url = Url::parse("https://europepmc.org/article/")?;
    url.path_segments_mut()
        .unwrap()
        .pop_if_empty()
        .push(origin)
        .push(id);
    if source == "pubmed" {
        ensure!(origin == "MED", "PubMed query returned a non-MED record");
        url = Url::parse("https://pubmed.ncbi.nlm.nih.gov/")?;
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .push(id)
            .push("");
    }
    let mut record = Record::new("paper", source, url.as_str());
    record.title = plain(item["title"].as_str());
    record.text = plain(item["abstractText"].as_str());
    record.author = item["authorString"].as_str().map(str::to_owned);
    record.published_at = item["firstPublicationDate"]
        .as_str()
        .or_else(|| item["pubYear"].as_str())
        .map(str::to_owned);
    record.metrics = json!({"citations":item["citedByCount"]});
    record.content = json!({"doi":item["doi"],"pmid":if origin == "MED" { json!(id) } else { item["pmid"].clone() },"pmcid":item["pmcid"],"source_id":id,"source_database":origin,"journal":item["journalInfo"],"authors":item["authorList"],"full_text_links":item["fullTextUrlList"],"open_access":item["isOpenAccess"],"publication_types":item["pubTypeList"],"abstract_available":record.text.is_some(),"full_text_retrieved":false,"article_url":record.url});
    Ok(record)
}
