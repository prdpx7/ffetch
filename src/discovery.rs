//! Engine selection and deterministic fusion, shared by human and agent output.
use crate::{
    extract,
    model::{Record, Run},
};
use serde_json::json;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum, PartialEq, Eq)]
pub enum SearchMode {
    #[default]
    Balanced,
    Deep,
    Fast,
}

pub const ENGINES: &[&str] = &[
    "brave",
    "duckduckgo",
    "google",
    "bing",
    "yandex",
    "baidu",
    "searxng",
];

pub fn plan(
    sources: &[String],
    engines: &[String],
    mode: SearchMode,
    searxng: bool,
) -> anyhow::Result<Vec<String>> {
    for engine in engines {
        anyhow::ensure!(
            ENGINES.contains(&engine.as_str()),
            "unknown engine {engine}; supported: {}",
            ENGINES.join(", ")
        );
    }
    anyhow::ensure!(
        mode != SearchMode::Fast || engines.is_empty(),
        "--engines requires balanced or deep search mode"
    );
    let mut output = Vec::new();
    for source in sources {
        let expanded = if source == "papers" {
            vec![
                "arxiv".to_string(),
                "crossref".to_string(),
                "europepmc".to_string(),
            ]
        } else if source == "web" && mode != SearchMode::Fast {
            let mut names = if engines.is_empty() {
                let defaults: &[&str] = if mode == SearchMode::Deep {
                    &ENGINES[..6]
                } else {
                    &["brave", "bing", "duckduckgo"]
                };
                defaults.iter().map(|s| s.to_string()).collect()
            } else {
                engines.to_vec()
            };
            if engines.is_empty() && searxng {
                names.push("searxng".into());
            }
            names.into_iter().map(|s| format!("engine:{s}")).collect()
        } else {
            vec![source.clone()]
        };
        for source in expanded {
            if !output.contains(&source) {
                output.push(source);
            }
        }
    }
    Ok(output)
}

#[derive(Default)]
pub struct Fusion {
    destinations: HashMap<String, usize>,
    research: HashMap<String, usize>,
}
impl Fusion {
    /// Return changed record indexes for streaming upserts. Origins include each
    /// engine's original record/provenance; fusion is not a credibility score.
    pub fn merge(&mut self, run: &mut Run, mut child: Run) -> Vec<usize> {
        let mut changed = Vec::new();
        for mut record in child.items.drain(..) {
            if let Some(key) = crate::research::identity(&record) {
                if let Some(&index) = self.research.get(&key) {
                    crate::research::merge(&mut run.items[index], record);
                    changed.push(index);
                } else {
                    crate::research::initialize(&mut record);
                    self.research.insert(key, run.items.len());
                    changed.push(run.items.len());
                    run.items.push(record);
                }
                continue;
            }
            if record.kind != "search_result" || record.source != "web" {
                changed.push(run.items.len());
                run.items.push(record);
                continue;
            }
            let key = extract::canonical(&record.url);
            let origins = record.native.get("origins").cloned().unwrap_or(json!([]));
            if let Some(&index) = self.destinations.get(&key) {
                let existing = &mut run.items[index];
                if let Some(new) = origins.as_array() {
                    let all = existing.native["origins"].as_array_mut().unwrap();
                    for origin in new {
                        if !all.iter().any(|o| o["engine"] == origin["engine"]) {
                            all.push(origin.clone());
                        }
                    }
                }
                if existing.text.as_deref().unwrap_or("").is_empty() {
                    existing.text = record.text.take();
                }
                score(existing);
                changed.push(index);
            } else {
                record.native["origins"] = origins;
                score(&mut record);
                self.destinations.insert(key, run.items.len());
                changed.push(run.items.len());
                run.items.push(record);
            }
        }
        run.merge(child);
        changed
    }
    pub fn finish(&mut self, run: &mut Run) {
        // Stable tie-break avoids completion-order effects on equal scores.
        run.items.sort_by(|a, b| {
            let a_web = a.kind == "search_result" && a.source == "web";
            let b_web = b.kind == "search_result" && b.source == "web";
            b_web.cmp(&a_web).then_with(|| {
                if a_web && b_web {
                    b.metrics["fusion_score"]
                        .as_f64()
                        .unwrap_or(0.0)
                        .total_cmp(&a.metrics["fusion_score"].as_f64().unwrap_or(0.0))
                        .then_with(|| a.url.cmp(&b.url))
                } else {
                    std::cmp::Ordering::Equal
                }
            })
        });
        let mut rank = 0;
        for item in &mut run.items {
            if item.kind == "search_result" && item.source == "web" {
                rank += 1;
                item.metrics["rank"] = json!(rank);
            }
        }
        self.destinations.clear();
        self.research.clear();
        for (index, record) in run.items.iter().enumerate() {
            if let Some(key) = crate::research::identity(record) {
                self.research.insert(key, index);
            } else if record.kind == "search_result" && record.source == "web" {
                self.destinations
                    .insert(extract::canonical(&record.url), index);
            }
        }
        run.meta["ranking"] = json!({"method":"reciprocal-rank-fusion", "constant":60,"independent_indexes_verified":false,"ordered_record_ids":run.items.iter().map(|r| &r.id).collect::<Vec<_>>()});
    }
}
fn score(record: &mut Record) {
    let origins = record.native["origins"].as_array().unwrap();
    let sum: f64 = origins
        .iter()
        .map(|o| 1.0 / (60.0 + o["rank"].as_u64().unwrap_or(1) as f64))
        .sum();
    record.metrics["fusion_score"] = json!(sum);
    record.metrics["engine_count"] = json!(origins.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    fn branch(engine: &str, urls: &[&str]) -> Run {
        let mut run = Run::new("search");
        for (rank, url) in urls.iter().enumerate() {
            let mut item = Record::new("search_result", "web", url);
            item.native = json!({"origins":[{"engine":engine,"rank":rank+1}]});
            run.items.push(item);
        }
        run
    }
    #[test]
    fn consensus_ranking_is_independent_of_engine_completion_order() {
        for reverse in [false, true] {
            let mut run = Run::new("search");
            let mut fusion = Fusion::default();
            let mut branches = vec![
                branch(
                    "google",
                    &["https://example.org/a", "https://example.org/shared"],
                ),
                branch(
                    "bing",
                    &["https://example.org/b", "https://example.org/shared"],
                ),
            ];
            if reverse {
                branches.reverse();
            }
            for branch in branches {
                fusion.merge(&mut run, branch);
            }
            fusion.finish(&mut run);
            assert_eq!(run.items.len(), 3);
            assert_eq!(run.items[0].url, "https://example.org/shared");
            assert_eq!(run.items[0].metrics["engine_count"], 2);
            assert_eq!(run.items[0].metrics["rank"], 1);
            assert_eq!(run.items[1].url, "https://example.org/a");
        }
    }
    #[test]
    fn merge_after_final_ordering_keeps_identity_indexes_valid() {
        let mut fusion = Fusion::default();
        let mut run = Run::new("search");
        let mut first = Run::new("search");
        let mut paper = Record::new("paper", "crossref", "https://doi.org/10.1234/paper");
        paper.content = json!({"doi":"10.1234/paper"});
        let id = paper.id.clone();
        first.items.push(paper);
        fusion.merge(&mut run, first);
        fusion.merge(&mut run, branch("google", &["https://example.org/web"]));
        fusion.finish(&mut run);
        let mut second = Run::new("search");
        let mut paper = Record::new(
            "paper",
            "europepmc",
            "https://europepmc.org/article/MED/12345",
        );
        paper.content = json!({"doi":"10.1234/paper"});
        second.items.push(paper);
        fusion.merge(&mut run, second);
        assert_eq!(run.items.len(), 2);
        assert_eq!(run.items[0].kind, "search_result");
        assert_eq!(run.items[1].id, id);
        assert_eq!(run.items[1].metrics["metadata_source_count"], 2);
    }
    #[test]
    fn engine_plan_is_explicit_and_rejects_unknown_names() {
        let sources = vec!["web".into(), "hn".into()];
        let engines = vec!["bing".into(), "google".into(), "bing".into()];
        assert_eq!(
            plan(&sources, &engines, SearchMode::Deep, false).unwrap(),
            vec!["engine:bing", "engine:google", "hn"]
        );
        assert!(plan(&sources, &["madeup".into()], SearchMode::Deep, false).is_err());
    }
}
