use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const SCHEMA_VERSION: &str = "ffetch/1";

pub(crate) fn combined_queries(first: &Record, second: &Record) -> Vec<String> {
    let mut queries = Vec::new();
    for record in [first, second] {
        if let Some(values) = record.native["discovery_queries"].as_array() {
            for query in values.iter().filter_map(serde_json::Value::as_str) {
                if !queries.iter().any(|q| q == query) {
                    queries.push(query.to_owned());
                }
            }
        }
    }
    queries.sort();
    queries
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub retrieved_at: String,
    pub source_url: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
}

impl Provenance {
    pub fn new(url: &str, method: &str) -> Self {
        Self {
            retrieved_at: Utc::now().to_rfc3339(),
            source_url: url.into(),
            method: method.into(),
            captured_at: None,
            archive_url: None,
            artifact: None,
            locator: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub url: String,
    pub title: Option<String>,
    pub text: Option<String>,
    pub published_at: Option<String>,
    pub author: Option<String>,
    pub metrics: Value,
    pub content: Value,
    pub native: Value,
    pub provenance: Provenance,
}

impl Record {
    pub fn new(kind: &str, source: &str, url: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.into(),
            source: source.into(),
            url: url.into(),
            title: None,
            text: None,
            published_at: None,
            author: None,
            metrics: json!({}),
            content: Value::Null,
            native: Value::Null,
            provenance: Provenance::new(url, "http"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub code: String,
    pub source: String,
    pub url: Option<String>,
    pub message: String,
    pub retryable: bool,
    pub next_action: Option<String>,
}

impl Issue {
    pub fn new(source: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            source: source.into(),
            url: None,
            message: message.into(),
            retryable: false,
            next_action: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub source: String,
    pub url: String,
    pub outcome: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Run {
    pub schema: String,
    pub run_id: String,
    pub operation: String,
    pub status: String,
    pub started_at: String,
    pub elapsed_ms: u64,
    pub items: Vec<Record>,
    pub errors: Vec<Issue>,
    pub warnings: Vec<String>,
    pub attempts: Vec<Attempt>,
    pub meta: Value,
}

impl Run {
    pub fn new(operation: &str) -> Self {
        Self {
            schema: SCHEMA_VERSION.into(),
            run_id: uuid::Uuid::new_v4().to_string(),
            operation: operation.into(),
            status: "ok".into(),
            started_at: Utc::now().to_rfc3339(),
            elapsed_ms: 0,
            items: vec![],
            errors: vec![],
            warnings: vec![],
            attempts: vec![],
            meta: json!({}),
        }
    }
    pub fn finish(&mut self, elapsed_ms: u64) {
        self.elapsed_ms = elapsed_ms;
        self.status = if self.errors.is_empty() {
            "ok"
        } else if self.items.is_empty() {
            "error"
        } else {
            "partial"
        }
        .into();
    }
    pub fn merge(&mut self, other: Self) {
        self.items.extend(other.items);
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
        self.attempts.extend(other.attempts);
    }
    pub fn exit_code(&self) -> u8 {
        match self.status.as_str() {
            "ok" => 0,
            "partial" => 3,
            _ => 1,
        }
    }
}
