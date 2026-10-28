//! Native arXiv Atom metadata. Explicit paper versions remain separate evidence.
use crate::{
    model::{Attempt, Record, Run},
    net::Http,
};
use anyhow::{Context, Result, ensure};
use quick_xml::{NsReader, XmlVersion, events::Event, name::ResolveResult};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};
use url::Url;
const ATOM: &str = "http://www.w3.org/2005/Atom";
const ARXIV: &str = "http://arxiv.org/schemas/atom";
const OPEN: &str = "http://a9.com/-/spec/opensearch/1.1/";
static GATE: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);
#[derive(Default)]
struct Node {
    name: String,
    ns: String,
    text: String,
    attrs: BTreeMap<String, String>,
    children: Vec<Node>,
}
impl Node {
    fn child(&self, ns: &str, name: &str) -> Option<&Node> {
        self.children.iter().find(|n| n.ns == ns && n.name == name)
    }
    fn value(&self, ns: &str, name: &str) -> Option<String> {
        self.child(ns, name)
            .map(|n| n.text.trim().to_owned())
            .filter(|s| !s.is_empty())
    }
}
fn parse(xml: &str) -> Result<Node> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    loop {
        let (namespace, event) = reader.read_resolved_event().context("invalid arXiv XML")?;
        match event {
            Event::Start(event) => {
                ensure!(stack.len() < 128, "arXiv XML exceeds nesting limit");
                let ns = match namespace {
                    ResolveResult::Bound(ns) => ns.as_ref().to_owned(),
                    ResolveResult::Unbound => String::new(),
                    ResolveResult::Unknown(prefix) => anyhow::bail!("unbound XML prefix {prefix}"),
                };
                let mut node = Node {
                    name: event.local_name().as_ref().to_owned(),
                    ns,
                    ..Default::default()
                };
                for attribute in event.attributes() {
                    let a = attribute?;
                    node.attrs.insert(
                        a.key.as_ref().to_owned(),
                        a.normalized_value(XmlVersion::Implicit1_0)?.into_owned(),
                    );
                }
                stack.push(node);
            }
            Event::Text(text) => {
                let text = text.xml_content(XmlVersion::Implicit1_0);
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&text);
                } else {
                    ensure!(text.trim().is_empty(), "text outside arXiv XML root");
                }
            }
            Event::CData(text) => {
                stack
                    .last_mut()
                    .context("CDATA outside XML root")?
                    .text
                    .push_str(&text.xml_content(XmlVersion::Implicit1_0));
            }
            Event::GeneralRef(entity) => {
                let text = if let Some(c) = entity.resolve_char_ref()? {
                    c.to_string()
                } else {
                    match entity.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => anyhow::bail!("unsupported XML entity"),
                    }
                    .into()
                };
                stack
                    .last_mut()
                    .context("entity outside XML root")?
                    .text
                    .push_str(&text);
            }
            Event::End(_) => {
                let node = stack.pop().context("unbalanced arXiv XML")?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    ensure!(root.is_none(), "multiple XML roots");
                    root = Some(node);
                }
            }
            Event::DocType(_) => anyhow::bail!("arXiv feeds must not contain a DTD"),
            Event::Eof => break,
            _ => (),
        }
    }
    ensure!(stack.is_empty(), "incomplete arXiv XML");
    let root = root.context("empty arXiv response")?;
    ensure!(
        root.name == "feed" && root.ns == ATOM,
        "response is not an Atom feed"
    );
    Ok(root)
}

pub fn paper_id(input: &str) -> Option<String> {
    let value = if input.starts_with("http://") || input.starts_with("https://") {
        let url = Url::parse(input).ok()?;
        if !matches!(
            url.host_str(),
            Some("arxiv.org" | "www.arxiv.org" | "export.arxiv.org")
        ) || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        let path = url
            .path()
            .strip_prefix("/abs/")
            .or_else(|| url.path().strip_prefix("/pdf/"))?;
        path.strip_suffix(".pdf").unwrap_or(path).to_owned()
    } else {
        input.strip_prefix("arxiv:").unwrap_or(input).to_owned()
    };
    let base =
        if let Some((base, version)) = value.rsplit_once('v').filter(|(_, v)| !v.contains('/')) {
            if version.is_empty()
                || !version.bytes().all(|b| b.is_ascii_digit())
                || version.parse::<u32>().ok()? == 0
            {
                return None;
            }
            base
        } else {
            &value
        };
    let valid = if let Some((archive, number)) = base.split_once('/') {
        !archive.is_empty()
            && archive
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
            && number.len() == 7
            && number.bytes().all(|b| b.is_ascii_digit())
    } else if let Some((date, number)) = base.split_once('.') {
        date.len() == 4
            && matches!(number.len(), 4 | 5)
            && date
                .bytes()
                .chain(number.bytes())
                .all(|b| b.is_ascii_digit())
    } else {
        false
    };
    valid.then_some(value)
}
fn entry(node: &Node) -> Result<Record> {
    let original = node
        .value(ATOM, "id")
        .context("arXiv entry has no identifier")?;
    let id = paper_id(&original).with_context(|| {
        format!(
            "arXiv returned an error or invalid identifier: {}",
            node.value(ATOM, "summary").unwrap_or_default()
        )
    })?;
    let (base, version) = id
        .rsplit_once('v')
        .filter(|(_, v)| !v.contains('/'))
        .map(|(b, v)| (b, Some(v.parse::<u32>().unwrap())))
        .unwrap_or((&id, None));
    let title = node
        .value(ATOM, "title")
        .context("arXiv entry has no title")?;
    let summary = node
        .value(ATOM, "summary")
        .context("arXiv entry has no abstract")?;
    let authors=node.children.iter().filter(|n|n.ns==ATOM&&n.name=="author").map(|n|json!({"name":n.value(ATOM,"name"),"affiliations":n.children.iter().filter(|n|n.ns==ARXIV&&n.name=="affiliation").map(|n|n.text.trim()).collect::<Vec<_>>()})).collect::<Vec<_>>();
    let mut record = Record::new("paper", "arxiv", &format!("https://arxiv.org/abs/{id}"));
    record.title = Some(title.split_whitespace().collect::<Vec<_>>().join(" "));
    record.text = Some(summary.clone());
    record.author = Some(
        authors
            .iter()
            .filter_map(|a| a["name"].as_str())
            .collect::<Vec<_>>()
            .join("; "),
    );
    record.published_at = node.value(ATOM, "published");
    let links = node
        .children
        .iter()
        .filter(|n| n.ns == ATOM && n.name == "link")
        .map(|n| json!(n.attrs))
        .collect::<Vec<_>>();
    record.content = json!({"arxiv_id":id,"arxiv_base_id":base,"version":version,"doi":node.value(ARXIV,"doi"),"doi_relation":"publication link; not proof of identical preprint/published versions","authors":authors,"categories":node.children.iter().filter(|n|n.ns==ATOM&&n.name=="category").filter_map(|n|n.attrs.get("term")).collect::<Vec<_>>(),"primary_category":node.child(ARXIV,"primary_category").and_then(|n|n.attrs.get("term")),"updated_at":node.value(ATOM,"updated"),"comment":node.value(ARXIV,"comment"),"journal_reference":node.value(ARXIV,"journal_ref"),"pdf_url":format!("https://arxiv.org/pdf/{id}"),"links":links,"article_url":record.url,"full_text_retrieved":false});
    record.native = json!({"id":original,"title":title,"summary":summary,"authors":record.content["authors"],"links":record.content["links"]});
    record.provenance.method = "arxiv-atom-api".into();
    Ok(record)
}
async fn page(http: &Http, url: Url, run: &mut Run) -> Result<String> {
    let official = matches!(url.host_str(), Some("export.arxiv.org" | "arxiv.org"));
    let mut guard = if official {
        Some(GATE.lock().await)
    } else {
        None
    };
    if let Some(last) = guard.as_deref_mut() {
        if let Some(last) = *last {
            tokio::time::sleep_until((last + Duration::from_secs(3)).into()).await;
        }
        *last = Some(Instant::now());
    }
    let start = Instant::now();
    let response = http.get_once(url.clone()).await;
    drop(guard);
    run.attempts.push(Attempt {
        source: "arxiv".into(),
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
        "arXiv returned HTTP {}{}",
        response.status.as_u16(),
        response
            .retry_after
            .map(|s| format!("; retry after {s}"))
            .unwrap_or_default()
    );
    String::from_utf8(response.bytes).context("arXiv feed was not UTF-8")
}
fn endpoint() -> Result<Url> {
    crate::net::normalize(
        &std::env::var("FFETCH_ARXIV_SEARCH")
            .unwrap_or_else(|_| "https://export.arxiv.org/api/query".into()),
    )
}
pub async fn search(
    http: &Http,
    query: &str,
    limit: usize,
    since: Option<i64>,
    run: &mut Run,
) -> Result<()> {
    let mut start = 0usize;
    let page_size = limit.clamp(1, 100);
    let mut seen = HashSet::new();
    let mut pages = 0;
    let mut query = if query.contains(':') {
        query.to_string()
    } else {
        query
            .split_whitespace()
            .map(|term| format!("all:{term}"))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    ensure!(!query.trim().is_empty(), "arXiv query is empty");
    if let Some(date) = since.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) {
        query = format!(
            "({query}) AND submittedDate:[{} TO {}]",
            date.format("%Y%m%d%H%M"),
            chrono::Utc::now().format("%Y%m%d%H%M")
        );
    }
    run.meta["query_expression"] = json!(query);
    run.meta["representation"] = json!("preprint metadata and abstract; PDF not parsed");
    loop {
        let mut url = endpoint()?;
        url.query_pairs_mut().extend_pairs([
            ("search_query", query.as_str()),
            ("start", start.to_string().as_str()),
            ("max_results", page_size.to_string().as_str()),
            ("sortBy", "relevance"),
            ("sortOrder", "descending"),
        ]);
        let xml = page(http, url.clone(), run).await?;
        let feed = parse(&xml)?;
        let entries = feed
            .children
            .iter()
            .filter(|n| n.ns == ATOM && n.name == "entry")
            .collect::<Vec<_>>();
        let total = feed
            .value(OPEN, "totalResults")
            .context("arXiv feed has no total result count")?
            .parse::<usize>()?;
        pages += 1;
        run.meta["total_hits"] = json!(total);
        run.meta["pages_fetched"] = json!(pages);
        let before = seen.len();
        for item in &entries {
            let mut record = entry(item)?;
            let id = record.content["arxiv_id"].as_str().unwrap().to_string();
            if !seen.insert(id) {
                continue;
            }
            if let Some(cutoff) = since {
                let date = record
                    .published_at
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .context("arXiv publication timestamp unavailable for exact filtering")?;
                if date.timestamp() < cutoff {
                    continue;
                }
            }
            record.metrics["rank"] = json!(run.items.len() + 1);
            record.provenance.source_url = url.to_string();
            run.items.push(record);
            if run.items.len() >= limit {
                break;
            }
        }
        start = start
            .checked_add(entries.len())
            .context("arXiv pagination overflow")?;
        let more = start < total;
        run.meta["truncated"] = json!(more || run.items.len() >= limit && total > run.items.len());
        if run.items.len() >= limit || !more || entries.is_empty() {
            break;
        }
        ensure!(
            seen.len() > before,
            "arXiv pagination repeated entries; earlier records retained"
        );
    }
    Ok(())
}
pub async fn lookup(http: &Http, id: &str, run: &mut Run) -> Result<()> {
    let id = paper_id(id).context("invalid arXiv identifier")?;
    let mut url = endpoint()?;
    url.query_pairs_mut().append_pair("id_list", &id);
    let xml = page(http, url.clone(), run).await?;
    let feed = parse(&xml)?;
    for node in feed
        .children
        .iter()
        .filter(|n| n.ns == ATOM && n.name == "entry")
    {
        let mut record = entry(node)?;
        let found = record.content["arxiv_id"].as_str().unwrap();
        let versioned = id.rsplit_once('v').is_some_and(|(_, v)| !v.contains('/'));
        ensure!(
            if versioned {
                found == id
            } else {
                record.content["arxiv_base_id"] == id
            },
            "arXiv returned a different identifier or version"
        );
        record.provenance.source_url = url.to_string();
        run.items.push(record);
    }
    ensure!(
        !run.items.is_empty(),
        "arXiv returned no record for the requested identifier"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_versions_and_does_not_accept_non_atom_error_pages() {
        assert_eq!(
            paper_id("https://arxiv.org/pdf/hep-th/9901001v2.pdf").as_deref(),
            Some("hep-th/9901001v2")
        );
        assert!(paper_id("https://other.example/abs/2501.12345v1").is_none());
        assert!(paper_id("2501.12345v0").is_none());
        assert_eq!(
            paper_id("solv-int/9901001").as_deref(),
            Some("solv-int/9901001")
        );
        assert!(parse("<html><body>Challenge</body></html>").is_err());
        assert!(parse("<feed xmlns='http://www.w3.org/2005/Atom'><entry>").is_err());
        assert!(parse("<!DOCTYPE feed [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><feed xmlns='http://www.w3.org/2005/Atom'/>").is_err());
    }
    #[test]
    fn versions_do_not_merge_with_a_publication_doi() {
        let mut a = Record::new("paper", "arxiv", "https://arxiv.org/abs/2501.12345v1");
        a.content = json!({"arxiv_id":"2501.12345v1","doi":"10.1234/paper"});
        let mut b = a.clone();
        b.content["arxiv_id"] = json!("2501.12345v2");
        let mut published = Record::new("paper", "crossref", "https://doi.org/10.1234/paper");
        published.content = json!({"doi":"10.1234/paper"});
        assert_ne!(crate::research::identity(&a), crate::research::identity(&b));
        assert_ne!(
            crate::research::identity(&a),
            crate::research::identity(&published)
        );
        assert_eq!(
            crate::discovery::plan(
                &["papers".into()],
                &[],
                crate::discovery::SearchMode::Balanced,
                false
            )
            .unwrap(),
            vec!["arxiv", "crossref", "europepmc"]
        );
    }
}
