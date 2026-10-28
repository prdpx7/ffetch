// These tests exercise the public extraction/query library without a network or
// subprocess, so downstream hosts can reuse the same implementation as the CLI.
use ffetch::{
    extract::{self, Extract, Options},
    net::Response,
    query,
};

#[test]
fn javascript_interstitials_are_not_evidence() {
    assert_eq!(
        extract::invalid_content(
            "<title>Captcha</title><p>Enable JavaScript to complete this challenge</p>"
        ),
        Some("challenge")
    );
    assert_eq!(
        extract::invalid_content(
            "<title>Google Search</title><p>If you're having trouble accessing Google Search, click here.</p>"
        ),
        Some("javascript_required")
    );
}

#[test]
fn json_evidence_can_be_queried_without_shelling_out() {
    let response = Response {
        url: url::Url::parse("https://example.org/data").unwrap(),
        status: reqwest::StatusCode::OK,
        content_type: "application/json".into(),
        retry_after: None,
        bytes: br#"{"measurements":[{"unit":"N m","value":4},{"unit":"N m","value":8}]}"#.to_vec(),
    };
    let records = extract::extract(
        &response,
        &Options {
            mode: Extract::Json,
            css: None,
        },
    )
    .unwrap();
    let output = query::jq(
        serde_json::to_value(records).unwrap(),
        ".[0].content.measurements | map(.value) | add / length",
    )
    .unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].as_f64(), Some(6.0));
}
