use crate::{model::Record, net::Response};
use anyhow::{Result, anyhow};
use clap::ValueEnum;
use scraper::{Html, Selector};
use serde_json::{Value, json};
use url::Url;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum Extract {
    #[default]
    Article,
    Text,
    Html,
    Links,
    Tables,
    Json,
}

#[derive(Clone, Default)]
pub struct Options {
    pub mode: Extract,
    pub css: Option<String>,
}

pub fn selector(css: &str) -> Result<Selector> {
    Selector::parse(css).map_err(|e| anyhow!("invalid CSS selector: {e}"))
}

pub fn text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn clean(html: &str) -> Html {
    let mut document = Html::parse_document(html);
    let removed = document
        .select(&Selector::parse("script,style,noscript,template").unwrap())
        .map(|e| e.id())
        .collect::<Vec<_>>();
    for id in removed {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
    document
}

// Keep HTTP validation and the browser's readiness gate on the same rules.
// A Turnstile widget alone is not an interstitial: ordinary articles and forms
// may embed one. Avoid generic spinners and mere mentions of Cloudflare.
const CHALLENGE_TITLES: &[&str] = &[
    "just a moment...",
    "ddos-guard",
    "verify you are human",
    "security checkpoint",
    "captcha",
];
const DENIED_TITLES: &[&str] = &["attention required! | cloudflare", "access denied"];
const CHALLENGE_SELECTOR: &str = "form#challenge-form, #cf-challenge-running, form#captcha-form, #cf-please-wait, #trk_jschal_js";
const DENIED_SELECTOR: &str =
    "#cf-error-details .cf-error-overview h1, .cf-error-title .cf-code-label span";

pub(crate) fn challenge_rules() -> Value {
    json!({"titles":CHALLENGE_TITLES,"denied_titles":DENIED_TITLES,"selector":CHALLENGE_SELECTOR,"denied_selector":DENIED_SELECTOR})
}

/// Deliberately conservative: an article mentioning Cloudflare is not a challenge.
pub fn invalid_content(html: &str) -> Option<&'static str> {
    let doc = Html::parse_document(html);
    let title = doc
        .select(&Selector::parse("title").unwrap())
        .next()
        .map(|e| e.text().collect::<String>().trim().to_lowercase())
        .unwrap_or_default();
    if CHALLENGE_TITLES
        .iter()
        .chain(DENIED_TITLES)
        .any(|t| title == *t)
        || doc
            .select(&Selector::parse(&format!("{CHALLENGE_SELECTOR}, {DENIED_SELECTOR}")).unwrap())
            .next()
            .is_some()
    {
        return Some("challenge");
    }
    if [
        "404",
        "404 not found",
        "404 - not found",
        "page not found",
        "page not found (404)",
    ]
    .contains(&title.as_str())
    {
        return Some("soft_404");
    }
    let cleaned = clean(html);
    let visible = cleaned
        .select(&Selector::parse("body").unwrap())
        .next()
        .map(|e| e.text().collect::<String>())
        .unwrap_or_default();
    if title == "google search"
        && visible.contains("If you're having trouble accessing Google Search")
    {
        return Some("javascript_required");
    }
    if visible.trim().is_empty()
        && doc
            .select(&Selector::parse("script").unwrap())
            .next()
            .is_some()
    {
        return Some("javascript_required");
    }
    None
}

pub fn is_html(response: &Response) -> bool {
    response.content_type.contains("html") || {
        let start = String::from_utf8_lossy(&response.bytes[..response.bytes.len().min(256)])
            .trim_start()
            .to_lowercase();
        start.starts_with("<!doctype html") || start.starts_with("<html")
    }
}

pub fn extract(response: &Response, options: &Options) -> Result<Vec<Record>> {
    extract_at(response, options, &response.url)
}

pub fn extract_at(response: &Response, options: &Options, base: &Url) -> Result<Vec<Record>> {
    let url = base.as_str();
    if options.css.is_none()
        && (response.content_type.contains("json") || matches!(options.mode, Extract::Json))
    {
        let mut record = Record::new("json", "web", url);
        record.content = serde_json::from_slice(&response.bytes)?;
        return Ok(vec![record]);
    }
    if !is_html(response) {
        let mut record = Record::new("document", "web", url);
        if response.content_type.starts_with("text/") {
            record.text = Some(String::from_utf8_lossy(&response.bytes).into_owned());
        }
        record.content = json!({"mime_type":response.content_type,"bytes":response.bytes.len(),"parsed":record.text.is_some()});
        return Ok(vec![record]);
    }
    let raw = String::from_utf8_lossy(&response.bytes);
    let doc = clean(&raw);
    let title = doc
        .select(&selector("title")?)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_owned());
    let scoped = if let Some(css) = &options.css {
        let selected = doc
            .select(&selector(css)?)
            .map(|e| e.html())
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Ok(vec![]);
        }
        selected.join("\n")
    } else {
        doc.html()
    };
    let selected = Html::parse_fragment(&scoped);
    let mut records = Vec::new();
    match options.mode {
        Extract::Links => {
            let mut seen = std::collections::HashSet::new();
            for (index, e) in selected.select(&selector("a[href]")?).enumerate() {
                if let Ok(link) = base.join(e.value().attr("href").unwrap_or_default()) {
                    if !matches!(link.scheme(), "http" | "https") || !seen.insert(link.to_string())
                    {
                        continue;
                    }
                    let mut record = Record::new("link", "web", link.as_str());
                    record.title = Some(e.text().collect::<String>().trim().to_owned());
                    record.provenance.source_url = url.into();
                    record.provenance.locator = Some(format!("selected:a[{index}]"));
                    records.push(record);
                }
            }
        }
        Extract::Tables => {
            for (index, table) in selected.select(&selector("table")?).enumerate() {
                let rows: Vec<Value> = table.select(&selector("tr")?).filter(|row| {
                    row.ancestors().filter_map(scraper::ElementRef::wrap).find(|e| e.value().name() == "table").is_some_and(|e| e.id() == table.id())
                }).map(|row| {
                    let cells: Vec<Value> = row.children().filter_map(scraper::ElementRef::wrap)
                        .filter(|c| matches!(c.value().name(), "td" | "th"))
                        .map(|c| json!({"text":text(&c.inner_html()),"header":c.value().name()=="th","rowspan":c.value().attr("rowspan").unwrap_or("1"),"colspan":c.value().attr("colspan").unwrap_or("1")})).collect();
                    Value::Array(cells)
                }).collect();
                let mut record = Record::new("table", "web", url);
                record.title = table
                    .select(&selector("caption")?)
                    .next()
                    .map(|e| e.text().collect::<String>())
                    .or_else(|| title.clone());
                record.content = json!({"rows":rows});
                record.provenance.locator = Some(format!("selected:table[{index}]"));
                records.push(record);
            }
        }
        mode => {
            let mut record = Record::new("document", "web", url);
            record.title = title;
            let html = if matches!(mode, Extract::Article) && options.css.is_none() {
                doc.select(&selector("article,main,[role=main]")?)
                    .max_by_key(|e| e.text().map(str::len).sum::<usize>())
                    .map(|e| e.html())
                    .unwrap_or(scoped)
            } else {
                scoped
            };
            if matches!(mode, Extract::Html) {
                record.text = Some(if options.css.is_none() {
                    raw.into_owned()
                } else {
                    html
                });
            } else {
                record.text = Some(text(&html));
            }
            record.content = json!({"mime_type":response.content_type});
            record.provenance.locator = options.css.as_ref().map(|s| format!("css:{s}"));
            records.push(record);
        }
    }
    Ok(records)
}

pub fn canonical(url: &str) -> String {
    let Ok(mut url) = Url::parse(url) else {
        return url.into();
    };
    url.set_fragment(None);
    let pairs = url
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && k != "fbclid" && k != "gclid")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    url.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(html: &str) -> Response {
        Response {
            url: Url::parse("https://example.org/docs/").unwrap(),
            status: reqwest::StatusCode::OK,
            content_type: "text/html".into(),
            retry_after: None,
            bytes: html.as_bytes().to_vec(),
        }
    }
    #[test]
    fn extracts_relative_links_and_preserves_table_units() {
        let response = response(
            "<title>Motor</title><article><h1>Specs</h1><script>secret()</script><a href='../motor.pdf'>PDF</a><table><tr><th>Torque</th><th>Mass</th></tr><tr><td>4 N m</td><td>200 g</td></tr></table></article>",
        );
        let links = extract(
            &response,
            &Options {
                mode: Extract::Links,
                css: None,
            },
        )
        .unwrap();
        assert_eq!(links[0].url, "https://example.org/motor.pdf");
        let tables = extract(
            &response,
            &Options {
                mode: Extract::Tables,
                css: None,
            },
        )
        .unwrap();
        assert_eq!(tables[0].content["rows"][1][0]["text"], "4 N m");
        let article = extract(&response, &Options::default()).unwrap();
        assert!(!article[0].text.as_ref().unwrap().contains("secret"));
    }
    #[test]
    fn detects_false_success_without_rejecting_discussion_of_challenges() {
        assert_eq!(
            invalid_content("<title>Just a moment...</title><p>Checking</p>"),
            Some("challenge")
        );
        assert_eq!(
            invalid_content("<title>404 Not Found</title>"),
            Some("soft_404")
        );
        assert_eq!(
            invalid_content("<div id='root'></div><script src='app.js'></script>"),
            Some("javascript_required")
        );
        assert_eq!(
            invalid_content("<article>How Cloudflare works: verify you are human</article>"),
            None
        );
        assert_eq!(invalid_content("<p>OK</p>"), None);
        assert_eq!(
            invalid_content("<title>DDoS-Guard</title><p>Checking</p>"),
            Some("challenge")
        );
        assert_eq!(
            invalid_content("<title>Research</title><div id='cf-please-wait'>Checking</div>"),
            Some("challenge")
        );
        assert_eq!(
            invalid_content(
                "<title>Research</title><article>Cloudflare Turnstile integration</article><div class='cf-turnstile'></div><input name='cf-turnstile-response'>"
            ),
            None
        );
        assert_eq!(
            invalid_content(
                "<article>Research <div class='lds-ring'></div><p class='ray_id'>Diagnostic example</p></article>"
            ),
            None
        );
    }
}
