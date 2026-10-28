//! Plan document representations separately from search-result arrival order.
use crate::{
    extract::{self, Options},
    fetch::{Archive, Fetcher},
    fulltext,
    model::{Issue, Record, Run},
    net, research,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

pub struct Target {
    pub parent: String,
    pub url: String,
    pub pmcid: Option<String>,
    pub expected_doi: Option<String>,
}

#[derive(Default)]
pub struct Planner {
    seen: HashSet<String>,
    fulltext: HashMap<String, Vec<Option<String>>>,
}
impl Planner {
    pub fn consider(&mut self, record: &Record) -> Option<Target> {
        let url = record.content["article_url"]
            .as_str()
            .unwrap_or(&record.url)
            .to_owned();
        let identity = research::identity(record).unwrap_or_else(|| extract::canonical(&url));
        let pmcid = record.content["pmcid"]
            .as_str()
            .and_then(fulltext::pmcid)
            .or_else(|| fulltext::pmcid(&url));
        let expected_doi = record.content["doi"]
            .as_str()
            .and_then(research::doi_identity);
        if let Some(id) = &pmcid {
            let seen = self.fulltext.entry(id.clone()).or_default();
            // Repeated discovery does not fetch XML again. Conflicting explicit
            // DOI assertions get separate validation rather than being conflated.
            if seen
                .iter()
                .any(|old| old.is_none() || expected_doi.is_none() || old == &expected_doi)
            {
                return None;
            }
            seen.push(expected_doi.clone());
            self.seen.insert(identity);
        } else if !self.seen.insert(identity) {
            return None;
        }
        Some(Target {
            parent: record.id.clone(),
            url,
            pmcid,
            expected_doi,
        })
    }
}

pub async fn fulltext_read(fetcher: &Fetcher, target: &Target) -> Run {
    let started = Instant::now();
    let mut run = Run::new("fetch");
    let task = async {
        let native = fulltext::fetch(
            &fetcher.http,
            target.pmcid.as_deref().unwrap_or(&target.url),
            fetcher.store.as_ref(),
            fetcher.budget,
        )
        .await;
        run.meta = native.meta.clone();
        run.merge(native);
        finish_read(fetcher, target, &mut run).await;
    };
    if net::within_budget(fetcher.budget, task).await.is_err() {
        run.errors.push(Issue::new(
            "web",
            "deadline_exceeded",
            "document reading exceeded its shared retrieval budget; completed evidence retained",
        ));
    }
    run.finish(started.elapsed().as_millis() as u64);
    run
}

async fn finish_read(fetcher: &Fetcher, target: &Target, run: &mut Run) {
    if !run.items.is_empty() {
        if let Some(expected) = &target.expected_doi {
            let matches = run.items.iter().all(|r| {
                r.content["doi"]
                    .as_str()
                    .and_then(research::doi_identity)
                    .as_ref()
                    == Some(expected)
            });
            if !matches {
                run.items.clear();
                let mut issue = Issue::new(
                    "europepmc",
                    "fulltext_identity_mismatch",
                    "Retrieved PMCID does not establish the DOI asserted by the search result",
                );
                issue.next_action=Some("Inspect the source metadata and PMC identity; no full text was attributed to this result".into());
                run.errors.push(issue);
                return;
            }
        }
        run.meta["selected_representation"] = json!("europepmc-jats");
        return;
    }
    if run
        .errors
        .iter()
        .any(|issue| issue.code == "deadline_exceeded")
    {
        return;
    }
    let fallback = fetcher
        .fetch(&target.url, &Options::default(), Archive::Auto)
        .await;
    run.meta["fallback"] = fallback.meta.clone();
    run.merge(fallback);
    run.meta["selected_representation"] =
        json!("generic fallback; full-text completeness unverified");
    for record in &mut run.items {
        if !record.native.is_object() {
            record.native = json!({"provider_data":record.native});
        }
        record.native["fulltext_fallback"] = json!(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn later_pmc_metadata_upgrades_generic_read_and_duplicates_are_suppressed() {
        let mut planner = Planner::default();
        let mut paper = Record::new("paper", "crossref", "https://doi.org/10.1000/paper");
        paper.content = json!({"doi":"10.1000/paper"});
        assert!(planner.consider(&paper).unwrap().pmcid.is_none());
        paper.content["pmcid"] = json!("PMC123");
        assert_eq!(
            planner.consider(&paper).unwrap().pmcid.as_deref(),
            Some("PMC123")
        );
        assert!(planner.consider(&paper).is_none());
        paper.content["doi"] = json!("10.1000/conflicting");
        assert!(planner.consider(&paper).is_some());
    }

    #[tokio::test]
    async fn unavailable_xml_falls_back_without_claiming_verified_fulltext() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/paper", server.local_addr().unwrap());
        let task = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut data = [0; 4096];
            let _ = socket.read(&mut data);
            let body = "<article>Fallback article content</article>";
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        let fetcher = Fetcher {
            http: crate::net::Http::new(Duration::from_secs(2), 4096, 2).unwrap(),
            browser: None,
            browser_mode: crate::browser::Mode::Off,
            store: None,
            budget: Duration::from_secs(2),
            cdx: url::Url::parse("https://invalid.example/cdx").unwrap(),
            replay: url::Url::parse("https://invalid.example/").unwrap(),
            archive_limit: 1,
        };
        let target = Target {
            parent: "parent".into(),
            url,
            pmcid: Some("PMC123".into()),
            expected_doi: None,
        };
        let mut run = Run::new("fetch");
        run.errors.push(Issue::new(
            "europepmc",
            "fulltext_unavailable",
            "XML unavailable",
        ));
        finish_read(&fetcher, &target, &mut run).await;
        run.finish(0);
        assert_eq!(run.status, "partial");
        assert_eq!(run.items.len(), 1);
        assert_eq!(run.items[0].native["fulltext_fallback"], true);
        assert_eq!(run.errors[0].code, "fulltext_unavailable");
        // A conflicting DOI is rejected before fallback, even if the PMCID matched.
        let mut rejected = Run::new("fetch");
        let mut paper = Record::new(
            "paper",
            "europepmc-fulltext",
            "https://europepmc.org/articles/PMC123",
        );
        paper.content = json!({"doi":"10.1000/wrong"});
        rejected.items.push(paper);
        let target = Target {
            expected_doi: Some("doi:10.1000/expected".into()),
            ..target
        };
        finish_read(&fetcher, &target, &mut rejected).await;
        assert!(rejected.items.is_empty());
        assert_eq!(rejected.errors[0].code, "fulltext_identity_mismatch");
        task.join().unwrap();
    }
}
