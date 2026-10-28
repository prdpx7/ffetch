//! Europe PMC JATS retrieval. No DTD loading, entity expansion, or script execution.
use crate::{
    model::{Attempt, Issue, Record, Run},
    net::{self, Http},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use quick_xml::{NsReader, XmlVersion, events::Event, name::ResolveResult};
use serde_json::json;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

enum Part {
    Text(String),
    Node(Node),
}
struct Node {
    name: String,
    attrs: BTreeMap<String, String>,
    parts: Vec<Part>,
}
impl Node {
    fn children(&self) -> impl Iterator<Item = &Node> {
        self.parts
            .iter()
            .filter_map(|p| if let Part::Node(n) = p { Some(n) } else { None })
    }
    fn find(&self, name: &str) -> Option<&Node> {
        if self.name == name {
            Some(self)
        } else {
            self.children().find_map(|n| n.find(name))
        }
    }
    fn collect<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        if self.name == name {
            out.push(self);
        }
        for child in self.children() {
            child.collect(name, out);
        }
    }
    fn text(&self) -> String {
        fn render(n: &Node, s: &mut String) {
            for part in &n.parts {
                match part {
                    Part::Text(t) => s.push_str(t),
                    Part::Node(n) => render(n, s),
                }
            }
            match n.name.as_str() {
                "td" | "th" => s.push('\t'),
                "p" | "title" | "tr" | "label" | "caption" | "list-item" => s.push('\n'),
                _ => (),
            }
        }
        let mut text = String::new();
        render(self, &mut text);
        text.lines()
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn pmcid(input: &str) -> Option<String> {
    let value = if input.contains("://") {
        let url = url::Url::parse(input).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || !matches!(
                url.host_str(),
                Some("pmc.ncbi.nlm.nih.gov" | "europepmc.org" | "www.europepmc.org")
            )
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return None;
        }
        url.path()
            .strip_prefix("/articles/")?
            .trim_end_matches('/')
            .to_owned()
    } else {
        input.trim().to_owned()
    };
    let upper = value.to_ascii_uppercase();
    let digits = upper.strip_prefix("PMC")?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(upper)
}

fn parse(xml: &str) -> Result<Node> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    loop {
        let (namespace, event) = reader.read_resolved_event().context("invalid JATS XML")?;
        ensure!(
            !matches!(namespace, ResolveResult::Unknown(_)),
            "unbound JATS namespace prefix"
        );
        match event {
            Event::Decl(e) => {
                if let Some(encoding) = e.encoding().transpose()? {
                    ensure!(
                        encoding.eq_ignore_ascii_case("UTF-8")
                            || encoding.eq_ignore_ascii_case("utf8"),
                        "unsupported JATS XML encoding"
                    );
                }
            }
            Event::Start(e) => {
                ensure!(stack.len() < 128, "JATS XML exceeds nesting limit");
                let mut attrs = BTreeMap::new();
                for a in e.attributes() {
                    let a = a?;
                    attrs.insert(
                        a.key.as_ref().to_owned(),
                        a.normalized_value(XmlVersion::Implicit1_0)?.into_owned(),
                    );
                }
                stack.push(Node {
                    name: e.local_name().as_ref().to_owned(),
                    attrs,
                    parts: Vec::new(),
                });
            }
            Event::Text(e) => {
                let text = e.xml_content(XmlVersion::Implicit1_0).into_owned();
                if let Some(n) = stack.last_mut() {
                    n.parts.push(Part::Text(text));
                } else {
                    ensure!(text.trim().is_empty(), "text outside JATS root");
                }
            }
            Event::CData(e) => stack
                .last_mut()
                .context("CDATA outside JATS root")?
                .parts
                .push(Part::Text(
                    e.xml_content(XmlVersion::Implicit1_0).into_owned(),
                )),
            Event::GeneralRef(e) => {
                let text = if let Some(c) = e.resolve_char_ref()? {
                    c.to_string()
                } else {
                    match e.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => anyhow::bail!("unsupported JATS entity"),
                    }
                    .into()
                };
                stack
                    .last_mut()
                    .context("entity outside JATS root")?
                    .parts
                    .push(Part::Text(text));
            }
            Event::End(_) => {
                let n = stack.pop().context("unbalanced JATS XML")?;
                if let Some(parent) = stack.last_mut() {
                    parent.parts.push(Part::Node(n));
                } else {
                    ensure!(root.is_none(), "multiple XML roots");
                    root = Some(n);
                }
            }
            Event::DocType(e) => {
                ensure!(
                    !e.as_ref().contains('[') && !e.as_ref().contains("<!ENTITY"),
                    "JATS internal DTD/entity declarations are not supported"
                );
            }
            Event::Eof => break,
            _ => (),
        }
    }
    ensure!(stack.is_empty(), "incomplete JATS XML");
    root.context("empty JATS XML")
}

fn article(xml: &str, id: &str, url: &str) -> Result<Record> {
    let root = parse(xml)?;
    ensure!(root.name == "article", "response is not a JATS article");
    let front = root
        .children()
        .find(|n| n.name == "front")
        .context("JATS front matter missing")?;
    let meta = front
        .children()
        .find(|n| n.name == "article-meta")
        .context("JATS article metadata missing")?;
    let ids: Vec<_> = meta.children().filter(|n| n.name == "article-id").collect();
    let identity = ids
        .iter()
        .find(|n| {
            matches!(
                n.attrs.get("pub-id-type").map(String::as_str),
                Some("pmc" | "pmcid")
            )
        })
        .context("JATS article has no PMC identity")?
        .text();
    let normalized = if identity.to_ascii_uppercase().starts_with("PMC") {
        pmcid(&identity)
    } else {
        pmcid(&format!("PMC{identity}"))
    };
    ensure!(
        normalized.as_deref() == Some(id),
        "JATS PMC identity does not match requested article"
    );
    let body = root
        .children()
        .find(|n| n.name == "body")
        .context("JATS article body unavailable")?;
    let text = body.text();
    ensure!(!text.is_empty(), "JATS article body is empty");
    fn blocks(n: &Node, path: &str, out: &mut Vec<serde_json::Value>) {
        if matches!(
            n.name.as_str(),
            "title" | "p" | "table-wrap" | "fig" | "disp-formula" | "list"
        ) {
            out.push(json!({"kind":n.name,"id":n.attrs.get("id"),"locator":path,"text":n.text()}));
            return;
        }
        let mut counts = BTreeMap::new();
        for child in n.children() {
            let count = counts.entry(&child.name).or_insert(0);
            *count += 1;
            blocks(child, &format!("{path}/{}[{count}]", child.name), out);
        }
    }
    let mut parts = Vec::new();
    blocks(body, "/article/body", &mut parts);
    let mut refs = Vec::new();
    // Europe PMC can relocate reference lists into body sections.
    root.collect("ref", &mut refs);
    let mut record = Record::new(
        "paper",
        "europepmc-fulltext",
        &format!("https://europepmc.org/articles/{id}"),
    );
    record.title = meta.find("article-title").map(Node::text);
    record.text = Some(text);
    let mut contributors = Vec::new();
    meta.collect("contrib", &mut contributors);
    let authors:Vec<_>=contributors.iter().filter(|n|n.attrs.get("contrib-type").is_none_or(|kind|kind=="author")).map(|n|json!({"given":n.find("given-names").map(Node::text),"family":n.find("surname").map(Node::text),"collective_name":n.find("collab").map(Node::text)})).collect();
    let names: Vec<_> = authors
        .iter()
        .map(|a| {
            [
                a["given"].as_str(),
                a["family"].as_str(),
                a["collective_name"].as_str(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
        })
        .filter(|name| !name.is_empty())
        .collect();
    if !names.is_empty() {
        record.author = Some(names.join("; "));
    }
    let mut dates = Vec::new();
    meta.collect("pub-date", &mut dates);
    let dates:Vec<_>=dates.iter().map(|date|json!({"attributes":date.attrs,"year":date.find("year").map(Node::text),"month":date.find("month").map(Node::text),"day":date.find("day").map(Node::text)})).collect();
    let mut tables = Vec::new();
    body.collect("table", &mut tables);
    let tables:Vec<_>=tables.iter().map(|table|{let mut rows=Vec::new();table.collect("tr",&mut rows);json!({"id":table.attrs.get("id"),"rows":rows.iter().map(|row|row.children().filter(|n|matches!(n.name.as_str(),"td"|"th")).map(|cell|json!({"text":cell.text(),"header":cell.name=="th","rowspan":cell.attrs.get("rowspan"),"colspan":cell.attrs.get("colspan")})).collect::<Vec<_>>()).collect::<Vec<_>>()})}).collect();
    record.content = json!({"pmcid":id,"doi":ids.iter().find(|n|n.attrs.get("pub-id-type").is_some_and(|s|s=="doi")).map(|n|n.text()),"abstract":meta.find("abstract").map(Node::text),"license":meta.find("permissions").map(Node::text),"body_blocks":parts,"references":refs.iter().map(|n|json!({"id":n.attrs.get("id"),"text":n.text()})).collect::<Vec<_>>(),"full_text_retrieved":true,"representation":"JATS body text; not publisher PDF","math_present":body.find("math").is_some(),"external_entities_resolved":false});
    record.native = json!({"article_type":root.attrs.get("article-type"),"format":"JATS XML"});
    record.content["tables"] = json!(tables);
    record.content["authors"] = json!(authors);
    record.content["publication_dates"] = json!(dates);
    record.content["images_parsed"] = json!(false);
    record.provenance.method = "europepmc-jats".into();
    record.provenance.source_url = url.into();
    Ok(record)
}

pub async fn fetch(http: &Http, input: &str, store: Option<&Store>, budget: Duration) -> Run {
    let start = Instant::now();
    let mut run = Run::new("fulltext");
    let task = async {
        let id = pmcid(input).context("fulltext requires a PMCID or supported PMC article URL")?;
        run.meta = json!({"pmcid":id,"representation":"open-access JATS XML","pdf_parsed":false});
        let mut url = net::normalize(
            &std::env::var("FFETCH_EUROPEPMC_FULLTEXT")
                .unwrap_or_else(|_| "https://www.ebi.ac.uk/europepmc/webservices/rest".into()),
        )?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid fulltext base"))?
            .pop_if_empty()
            .push(&id)
            .push("fullTextXML");
        let began = Instant::now();
        let response = http.get(url.clone()).await;
        run.attempts.push(Attempt {
            source: "europepmc-fulltext".into(),
            url: url.to_string(),
            elapsed_ms: began.elapsed().as_millis() as u64,
            outcome: response
                .as_ref()
                .map(|r| format!("HTTP {}", r.status.as_u16()))
                .unwrap_or_else(|e| e.to_string()),
        });
        let response = response?;
        ensure!(
            response.status.is_success(),
            "Europe PMC full text returned HTTP {}; XML may be unavailable or restricted",
            response.status.as_u16()
        );
        let xml = std::str::from_utf8(&response.bytes).context("JATS XML is not UTF-8")?;
        let mut record = article(xml, &id, response.url.as_str())?;
        if let Some(store) = store {
            record.provenance.artifact = Some(store.blob(&response.bytes)?);
        }
        if record.content["math_present"] == true {
            run.warnings.push("MathML layout is not fully represented in plain text; the original XML is retained when storage is enabled".into());
        }
        run.items.push(record);
        Ok::<(), anyhow::Error>(())
    };
    match net::within_budget(budget, task).await {
        Ok(Ok(())) => (),
        Ok(Err(e)) => run.errors.push(Issue::new(
            "europepmc",
            "fulltext_unavailable",
            e.to_string(),
        )),
        Err(_) => run.errors.push(Issue::new(
            "europepmc",
            "deadline_exceeded",
            "fulltext retrieval exceeded --timeout",
        )),
    }
    run.finish(start.elapsed().as_millis() as u64);
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn body_reference_lists_and_author_metadata_are_retained() {
        let xml = "<article><front><article-meta><article-id pub-id-type='pmcid'>PMC123</article-id><contrib-group><contrib contrib-type='author'><name><surname>Researcher</surname><given-names>A</given-names></name></contrib></contrib-group><pub-date pub-type='epub'><year>2025</year></pub-date></article-meta></front><body><sec><p>Full article body.</p></sec><sec sec-type='ref-list'><ref-list><ref id='r1'><mixed-citation>Body reference</mixed-citation></ref></ref-list></sec></body></article>";
        let record = article(xml, "PMC123", "https://example.org/fullTextXML").unwrap();
        assert_eq!(record.author.as_deref(), Some("A Researcher"));
        assert_eq!(record.content["publication_dates"][0]["year"], "2025");
        assert_eq!(record.content["references"][0]["id"], "r1");
        assert_eq!(record.content["references"][0]["text"], "Body reference");
    }
}
