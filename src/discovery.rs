//! Engine selection and deterministic fusion, shared by human and agent output.
use crate::{
    extract,
    model::{Record, Run},
};
use serde_json::json;
use std::collections::HashMap;

#[derive(serde::Serialize)]
pub struct QueryPlan {
    pub queries: Vec<String>,
    pub jobs: Vec<SearchJob>,
}

#[derive(serde::Serialize)]
pub struct SearchJob {
    pub query_index: usize,
    pub source: String,
}

pub fn query_plan(
    primary: &str,
    variants: &[String],
    sources: &[String],
    max_searches: usize,
) -> anyhow::Result<QueryPlan> {
    anyhow::ensure!(!sources.is_empty(), "search needs at least one source");
    let mut queries = Vec::new();
    for query in std::iter::once(primary).chain(variants.iter().map(String::as_str)) {
        let query = query.trim();
        anyhow::ensure!(!query.is_empty(), "search queries must not be empty");
        if !queries.iter().any(|q| q == query) {
            queries.push(query.to_owned());
        }
    }
    let count = queries
        .len()
        .checked_mul(sources.len())
        .ok_or_else(|| anyhow::anyhow!("query/source plan size overflow"))?;
    anyhow::ensure!(
        count <= max_searches,
        "query/source plan needs {count} searches, exceeding --max-searches {max_searches}; reduce queries/sources or increase the budget"
    );
    let jobs = queries
        .iter()
        .enumerate()
        .flat_map(|(query_index, _)| {
            sources.iter().map(move |source| SearchJob {
                query_index,
                source: source.clone(),
            })
        })
        .collect();
    Ok(QueryPlan { queries, jobs })
}

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
    native: HashMap<(String, String, String), NativeEntry>,
}
struct NativeEntry {
    index: usize,
    id: String,
    observations: bool,
}

fn native_key(record: &Record) -> Option<(String, String, String)> {
    let url = url::Url::parse(&record.url).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| {
        (
            record.source.clone(),
            record.kind.clone(),
            extract::canonical(&record.url),
        )
    })
}
impl Fusion {
    /// Return changed record indexes for streaming upserts. Origins include each
    /// engine's original record/provenance; fusion is not a credibility score.
    pub fn merge(&mut self, run: &mut Run, mut child: Run) -> Vec<usize> {
        let changed = self.merge_records(run, child.items.drain(..));
        run.merge(child);
        changed
    }

    pub fn merge_records(
        &mut self,
        run: &mut Run,
        records: impl IntoIterator<Item = Record>,
    ) -> Vec<usize> {
        let mut changed = Vec::new();
        for mut record in records {
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
                if let Some(key) = native_key(&record) {
                    if let Some(entry) = self.native.get_mut(&key) {
                        let existing = &mut run.items[entry.index];
                        let queries = crate::model::combined_queries(existing, &record);
                        if !existing.native.is_object() {
                            existing.native = json!({"provider_data":existing.native});
                        }
                        if !entry.observations {
                            let first = json!(existing);
                            existing.native["search_observations"] = json!([first]);
                            entry.observations = true;
                        }
                        let observations = existing.native["search_observations"]
                            .as_array_mut()
                            .unwrap();
                        if !observations.iter().any(|o| {
                            o["provenance"]["source_url"] == record.provenance.source_url
                                && o["native"]["discovery_queries"]
                                    == record.native["discovery_queries"]
                        }) {
                            observations.push(json!(record));
                        }
                        if !queries.is_empty() {
                            existing.native["discovery_queries"] = json!(queries);
                        }
                        changed.push(entry.index);
                        continue;
                    }
                    self.native.insert(
                        key,
                        NativeEntry {
                            index: run.items.len(),
                            id: record.id.clone(),
                            observations: false,
                        },
                    );
                }
                changed.push(run.items.len());
                run.items.push(record);
                continue;
            }
            let key = extract::canonical(&record.url);
            let origins = record.native.get("origins").cloned().unwrap_or(json!([]));
            if let Some(&index) = self.destinations.get(&key) {
                let existing = &mut run.items[index];
                let queries = crate::model::combined_queries(existing, &record);
                if let Some(new) = origins.as_array() {
                    let all = existing.native["origins"].as_array_mut().unwrap();
                    for origin in new {
                        if !all.iter().any(|o| {
                            o["engine"] == origin["engine"] && o["query"] == origin["query"]
                        }) {
                            all.push(origin.clone());
                        }
                    }
                }
                if existing.text.as_deref().unwrap_or("").is_empty() {
                    existing.text = record.text.take();
                }
                score(existing);
                if !queries.is_empty() {
                    existing.native["discovery_queries"] = json!(queries);
                }
                changed.push(index);
            } else {
                record.native["origins"] = origins;
                score(&mut record);
                self.destinations.insert(key, run.items.len());
                changed.push(run.items.len());
                run.items.push(record);
            }
        }
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
        for (index, record) in run.items.iter().enumerate() {
            if let Some(key) = native_key(record)
                && let Some(entry) = self.native.get_mut(&key)
                && entry.id == record.id
            {
                entry.index = index;
            }
        }
        run.meta["ranking"] = json!({"method":"reciprocal-rank-fusion", "constant":60,"query_aggregation":"best observed rank per engine","independent_indexes_verified":false,"ordered_record_ids":run.items.iter().map(|r| &r.id).collect::<Vec<_>>()});
    }
}
fn score(record: &mut Record) {
    let origins = record.native["origins"].as_array().unwrap();
    let mut engines = std::collections::BTreeMap::new();
    let mut queries = std::collections::BTreeSet::new();
    for origin in origins {
        let rank = origin["rank"].as_u64().unwrap_or(1);
        let engine = origin["engine"].as_str().unwrap_or("unknown");
        engines
            .entry(engine)
            .and_modify(|best: &mut u64| *best = (*best).min(rank))
            .or_insert(rank);
        if let Some(query) = origin["query"].as_str() {
            queries.insert(query);
        }
    }
    let sum: f64 = engines
        .values()
        .map(|rank| 1.0 / (60.0 + *rank as f64))
        .sum();
    record.metrics["fusion_score"] = json!(sum);
    record.metrics["engine_count"] = json!(engines.len());
    record.metrics["query_count"] = json!(queries.len());
    record.metrics["observation_count"] = json!(origins.len());
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
    fn query_variants_preserve_observations_without_inflating_engine_agreement() {
        let mut run = Run::new("search");
        let mut fusion = Fusion::default();
        for query in ["gripper", "tendon"] {
            let mut result = branch("bing", &["https://example.org/one-engine"]);
            result.items[0].native["origins"][0]["query"] = json!(query);
            fusion.merge(&mut run, result);
        }
        for engine in ["brave", "google"] {
            fusion.merge(
                &mut run,
                branch(
                    engine,
                    &[
                        "https://example.org/other",
                        "https://example.org/two-engines",
                    ],
                ),
            );
        }
        let one = run
            .items
            .iter()
            .find(|r| r.url.ends_with("one-engine"))
            .unwrap();
        let two = run
            .items
            .iter()
            .find(|r| r.url.ends_with("two-engines"))
            .unwrap();
        assert_eq!(one.metrics["engine_count"], 1);
        assert_eq!(one.metrics["query_count"], 2);
        assert_eq!(one.native["origins"].as_array().unwrap().len(), 2);
        assert!(
            two.metrics["fusion_score"].as_f64().unwrap()
                > one.metrics["fusion_score"].as_f64().unwrap()
        );
    }

    #[test]
    fn native_query_observations_keep_identity_after_final_ordering() {
        let mut run = Run::new("search");
        let mut fusion = Fusion::default();
        let native = |query: &str, score: u64| {
            let mut item = Record::new("story", "hn", "https://news.ycombinator.com/item?id=1");
            item.id = "hn:1".into();
            item.metrics = json!({"score":score});
            item.native = json!({"discovery_queries":[query]});
            item.provenance.source_url = format!("https://hn.algolia.com/search?query={query}");
            item
        };
        fusion.merge_records(&mut run, [native("gripper", 10)]);
        fusion.merge(&mut run, branch("bing", &["https://example.org/paper"]));
        fusion.finish(&mut run);
        fusion.merge_records(&mut run, [native("tendon", 20)]);
        assert_eq!(run.items.len(), 2);
        let item = &run.items[1];
        assert_eq!(item.id, "hn:1");
        assert_eq!(
            item.native["discovery_queries"],
            json!(["gripper", "tendon"])
        );
        assert_eq!(
            item.native["search_observations"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            item.native["search_observations"][0]["metrics"]["score"],
            10
        );
        assert_eq!(
            item.native["search_observations"][1]["metrics"]["score"],
            20
        );
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
