//! Manual live transport probe. Writes bounded raw responses for independent
//! inspection; it is not part of the installed CLI or a search-quality claim.
use anyhow::{Context, Result, ensure};
use ffetch::{extract, net::Http};
use serde::Deserialize;
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct Case {
    name: String,
    url: String,
    selector: String,
}

fn classify(case: &Case, result: &mut serde_json::Value, bytes: &[u8]) -> Result<()> {
    let body = String::from_utf8_lossy(bytes);
    let invalid = extract::invalid_content(&body);
    let doc = scraper::Html::parse_document(&body);
    let matched = doc.select(&extract::selector(&case.selector)?).count();
    result["matched_markers"] = json!(matched);
    result["recognized_error"] = json!(invalid);
    result["validated_layout"] = json!(
        result["status"]
            .as_u64()
            .is_some_and(|code| (200..300).contains(&code))
            && result["error"].is_null()
            && invalid.is_none()
            && matched > 0
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let offline = args.first().is_some_and(|arg| arg == "--classify");
    let browser_user_agent = args
        .first()
        .is_some_and(|arg| arg == "--browser-user-agent");
    if offline || browser_user_agent {
        args.remove(0);
    }
    ensure!(
        args.len() == 2,
        "usage: transport_probe [--classify|--browser-user-agent] CASES.json OUTPUT_DIRECTORY"
    );
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    ensure!(
        !cases.is_empty() && cases.len() <= 32,
        "probe needs 1-32 explicit cases"
    );
    for case in &cases {
        ffetch::net::normalize(&case.url)?;
        extract::selector(&case.selector)?;
    }
    let output = PathBuf::from(&args[1]);
    if offline {
        let mut report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output.join("results.json"))?)?;
        ensure!(
            !output.join("classified.json").exists(),
            "classified.json already exists"
        );
        for result in report["cases"]
            .as_array_mut()
            .context("report needs cases")?
        {
            let case = cases
                .iter()
                .find(|case| result["case"] == case.name)
                .context("unknown report case")?;
            if let Some(file) = result["body_file"].as_str() {
                ensure!(
                    std::path::Path::new(file)
                        .file_name()
                        .is_some_and(|name| name == file),
                    "body_file must be a single filename"
                );
                classify(case, result, &std::fs::read(output.join(file))?)?;
            }
            println!("{result}");
        }
        std::fs::write(
            output.join("classified.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        return Ok(());
    }
    std::fs::create_dir(&output).context("output directory must not already exist")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700))?;
    }
    let http = Http::new(Duration::from_secs(10), 2 * 1024 * 1024, 2)?;
    let proxy_present = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some());
    let mut results = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let began = Instant::now();
        let url = ffetch::net::normalize(&case.url)?;
        let response = if browser_user_agent {
            http.get_with_browser_user_agent(url, false).await
        } else {
            http.get_once(url).await
        };
        let mut result = json!({"case":case.name,"url":case.url,"elapsed_ms":began.elapsed().as_millis(),"transport":"ffetch-reqwest","request_profile":if browser_user_agent {"chrome152-user-agent-only"}else{"ffetch"}});
        match response {
            Ok(response) => {
                let file = format!("response-{index}.html");
                std::fs::write(output.join(&file), &response.bytes)?;
                result["status"] = json!(response.status.as_u16());
                result["final_url"] = json!(response.url.as_str());
                result["bytes"] = json!(response.bytes.len());
                result["body_file"] = json!(file);
                classify(case, &mut result, &response.bytes)?;
                result["retry_after"] = json!(response.retry_after);
            }
            Err(error) => result["error"] = json!(error.to_string()),
        }
        println!("{result}");
        results.push(result);
    }
    std::fs::write(
        output.join("results.json"),
        serde_json::to_vec_pretty(
            &json!({"cases":results,"http":http.stats(),"proxy_environment_present":proxy_present,"max_response_bytes":2*1024*1024,"timeout_seconds":10,"automatic_retry":false,"scope":"explicit public cases, one host/network/proxy configuration; layout markers require independent inspection"}),
        )?,
    )?;
    Ok(())
}
