//! Conservative DOI identity and lossless provider evidence for scholarly merging.
use crate::model::Record;
use serde_json::{Value, json};

/// Only scholarly metadata records with a recognizable DOI participate. Titles,
/// citation counts, filenames and similar URLs are never identity evidence.
pub fn identity(record: &Record) -> Option<String> {
    if !matches!(record.kind.as_str(), "paper" | "research_work") {
        return None;
    }
    if record.source == "arxiv" {
        return crate::arxiv::paper_id(record.content["arxiv_id"].as_str()?)
            .map(|id| format!("arxiv:{id}"));
    }
    let raw = record.content["doi"].as_str()?;
    if raw != raw.trim() {
        return None;
    }
    let value = if raw.starts_with("https://") || raw.starts_with("http://") {
        let url = url::Url::parse(raw).ok()?;
        if !matches!(url.host_str(), Some("doi.org" | "dx.doi.org"))
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
        {
            return None;
        }
        decode_path(url.path().trim_start_matches('/'))?
    } else {
        raw.get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("doi:"))
            .map(|_| &raw[4..])
            .unwrap_or(raw)
            .to_string()
    };
    let (prefix, suffix) = value.split_once('/')?;
    let registrant = prefix.strip_prefix("10.")?;
    if registrant
        .split('.')
        .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
        || suffix.is_empty()
        || value.chars().any(char::is_control)
    {
        return None;
    }
    Some(format!("doi:{}", value.to_ascii_lowercase()))
}
fn decode_path(path: &str) -> Option<String> {
    let mut out = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hi = (bytes.next()? as char).to_digit(16)?;
            let lo = (bytes.next()? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
        } else {
            out.push(byte);
        }
    }
    String::from_utf8(out).ok()
}

pub fn initialize(record: &mut Record) {
    let evidence = json!({"provider":record.source,"record":record});
    record.native = json!({"research_origins":[evidence]});
    record.metrics["metadata_source_count"] = json!(1);
}

pub fn merge(existing: &mut Record, incoming: Record) {
    let id = existing.id.clone();
    let mut origins = existing.native["research_origins"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !origins
        .iter()
        .any(|o| o["provider"] == incoming.source && o["record"]["url"] == incoming.url)
    {
        origins.push(json!({"provider":incoming.source,"record":incoming}));
    }
    origins.sort_by(|a, b| {
        priority(a["provider"].as_str().unwrap_or(""))
            .cmp(&priority(b["provider"].as_str().unwrap_or("")))
    });
    // Every origin is serialized from our typed Record, never parsed from page instructions.
    let mut selected: Record = serde_json::from_value(origins[0]["record"].clone())
        .expect("typed research evidence roundtrip");
    let mut selected_json = serde_json::to_value(&selected).expect("typed record serializes");
    let mut fields = serde_json::Map::new();
    let mut conflicts = serde_json::Map::new();
    for field in [
        "title",
        "text",
        "author",
        "published_at",
        "url",
        "kind",
        "source",
    ] {
        let values = values(&origins, field);
        if let Some(first) = values.first() {
            selected_json[field] = first["value"].clone();
            fields.insert(field.into(), first["provider"].clone());
        }
        if distinct(&values) > 1 {
            conflicts.insert(field.into(), json!(values));
        }
    }
    for section in ["content", "metrics"] {
        if !selected_json[section].is_object() {
            selected_json[section] = json!({});
        }
        let mut keys = std::collections::BTreeSet::new();
        for origin in &origins {
            if let Some(object) = origin["record"][section].as_object() {
                keys.extend(object.keys().cloned());
            }
        }
        for key in keys {
            let path = format!("{section}.{key}");
            let values = origins
                .iter()
                .filter_map(|o| {
                    let value = &o["record"][section][&key];
                    useful(value).then(|| json!({"provider":o["provider"],"value":value}))
                })
                .collect::<Vec<_>>();
            if let Some(first) = values.first() {
                selected_json[section][&key] = first["value"].clone();
                fields.insert(path.clone(), first["provider"].clone());
            }
            if distinct(&values) > 1 {
                conflicts.insert(path, json!(values));
            }
        }
    }
    selected = serde_json::from_value(selected_json).expect("merged typed research fields");
    selected.id = id;
    selected.metrics["metadata_source_count"] = json!(
        origins
            .iter()
            .filter_map(|o| o["provider"].as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    );
    selected.metrics["metadata_record_count"] = json!(origins.len());
    selected.native = json!({"research_origins":origins,"field_sources":fields,"conflicts":conflicts,"independent_corroboration":false});
    *existing = selected;
}
fn priority(source: &str) -> (u8, &str) {
    (
        match source {
            "pubmed" => 0,
            "europepmc" => 1,
            "crossref" => 2,
            _ => 3,
        },
        source,
    )
}
fn useful(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}
fn values(origins: &[Value], field: &str) -> Vec<Value> {
    origins
        .iter()
        .filter(|o| useful(&o["record"][field]))
        .map(|o| json!({"provider":o["provider"],"value":o["record"][field]}))
        .collect()
}
fn distinct(values: &[Value]) -> usize {
    let mut unique = Vec::new();
    for value in values {
        if !unique.contains(&&value["value"]) {
            unique.push(&value["value"]);
        }
    }
    unique.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn work(provider: &str, doi: &str) -> Record {
        let mut r = Record::new(
            "paper",
            provider,
            &format!("https://{provider}.example/item"),
        );
        r.content = json!({"doi":doi});
        r.native = json!({"raw_provider":"original"});
        r
    }
    #[test]
    fn doi_equivalence_is_conservative_and_ascii_only() {
        assert_eq!(
            identity(&work("a", "10.1234/ABC")),
            identity(&work("b", "https://doi.org/10.1234%2Fabc"))
        );
        assert_ne!(
            identity(&work("a", "10.1234/Ä")),
            identity(&work("b", "10.1234/ä"))
        );
        assert_ne!(
            identity(&work("a", "10.1234/paper")),
            identity(&work("b", "10.1234/paper/supplement"))
        );
        assert!(identity(&work("a", "10.500.100/Name With Space")).is_some());
        for invalid in [
            "not a DOI",
            "10./paper",
            "https://other.example/10.1234/paper",
            "10.1234/",
            " 10.1234/paper",
            "https://doi.org/10.1234/paper?ambiguous=1",
        ] {
            assert!(identity(&work("a", invalid)).is_none(), "{invalid}");
        }
    }
    #[test]
    fn merged_fields_have_sources_and_conflicts_without_losing_originals() {
        let mut crossref = work("crossref", "10.1234/ABC");
        crossref.title = Some("Crossref title".into());
        crossref.text = Some("Only available abstract".into());
        crossref.published_at = Some("2024".into());
        crossref.content["references"] = json!([{"DOI":"10.1234/ref"}]);
        crossref.metrics = json!({"citations":5});
        let mut epmc = work("europepmc", "10.1234/abc");
        epmc.title = Some("Europe PMC title".into());
        epmc.published_at = Some("2025-01-01".into());
        epmc.content["pmid"] = json!("12345");
        epmc.metrics = json!({"citations":7});
        for reversed in [false, true] {
            let (mut first, second) = if reversed {
                (epmc.clone(), crossref.clone())
            } else {
                (crossref.clone(), epmc.clone())
            };
            let id = first.id.clone();
            initialize(&mut first);
            merge(&mut first, second);
            assert_eq!(first.id, id);
            assert_eq!(first.source, "europepmc");
            assert_eq!(first.text.as_deref(), Some("Only available abstract"));
            assert_eq!(first.native["field_sources"]["text"], "crossref");
            assert_eq!(first.content["references"][0]["DOI"], "10.1234/ref");
            assert_eq!(first.content["pmid"], "12345");
            assert_eq!(
                first.native["conflicts"]["published_at"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(
                first.native["conflicts"]["metrics.citations"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(
                first.native["research_origins"][0]["record"]["native"]["raw_provider"],
                "original"
            );
            assert_eq!(first.metrics["metadata_source_count"], 2);
            assert_eq!(first.native["independent_corroboration"], false);
        }
    }
}
