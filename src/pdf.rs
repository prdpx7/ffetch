//! Embedded text-layer extraction in a disposable instance of the same binary.
use crate::{
    model::{Attempt, Issue, Record, Run},
    net::{self, Http},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
const STREAM_LIMIT: usize = 16 * 1024 * 1024;
const OUTPUT_LIMIT: u64 = 32 * 1024 * 1024;

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Internal subprocess entry point, before initializing an async runtime.
pub fn worker(input: &Path, output: &Path, url: &str, start: u32, count: u32) -> Result<()> {
    ensure!(
        start > 0 && (1..=500).contains(&count),
        "invalid PDF page selection"
    );
    #[cfg(unix)]
    {
        // Keep hostile object streams from exhausting the parent process. This
        // direct kernel call needs no external PDF program or runtime.
        let cap = libc::rlimit {
            rlim_cur: 512 * 1024 * 1024,
            rlim_max: 512 * 1024 * 1024,
        };
        ensure!(
            unsafe { libc::setrlimit(libc::RLIMIT_AS, &cap) } == 0,
            "cannot apply PDF worker memory limit"
        );
    }
    let began = Instant::now();
    let mut run = Run::new("pdf");
    let task = || -> Result<Run> {
        let bytes = std::fs::read(input)?;
        ensure!(
            bytes.starts_with(b"%PDF-"),
            "response does not begin with a PDF signature"
        );
        let doc = lopdf::Document::load_mem_with_options(
            &bytes,
            lopdf::LoadOptions::with_max_decompressed_size(STREAM_LIMIT),
        )?;
        drop(bytes);
        ensure!(
            !doc.is_encrypted(),
            "PDF requires decryption; password input is not supported"
        );
        let pages = doc.get_pages();
        ensure!(!pages.is_empty(), "PDF has no pages");
        ensure!(
            pages.contains_key(&start),
            "requested starting page is outside the PDF"
        );
        let mut run = Run::new("pdf");
        run.meta = json!({"total_pages":pages.len(),"start_page":start,"page_limit":count,"pdf_version":doc.version,"ocr":false,"layout_verified":false,"per_stream_limit":STREAM_LIMIT,"coverage":"requested text-layer page range"});
        run.meta["xref_recovery"] = json!(
            doc.xref_recovery
                .as_ref()
                .map(|recovery| format!("{recovery:?}"))
        );
        if doc.xref_recovery.is_some() {
            run.warnings.push("PDF cross-reference data was repaired; recovered objects may not represent every revision".into());
        }
        let mut output_bytes = 0;
        for &page in pages
            .keys()
            .filter(|&&page| page >= start)
            .take(count as usize)
        {
            let mut record = Record::new("pdf_page", "web", url);
            record.provenance.method = "lopdf-text-layer".into();
            record.provenance.locator = Some(format!("page:{page}"));
            let mut page_url = url::Url::parse(url)?;
            page_url.set_fragment(Some(&format!("page={page}")));
            record.url = page_url.into();
            record.content = json!({"format":"pdf","page":page,"total_pages":pages.len(),"ocr":false,"layout_verified":false});
            match doc.extract_text_with_limit(&[page], STREAM_LIMIT) {
                Ok(text) if !text.trim().is_empty() => {
                    output_bytes += text.len();
                    if output_bytes > STREAM_LIMIT {
                        run.errors.push(Issue::new("pdf","pdf_text_limit","PDF text exceeds output byte budget; previously extracted pages retained"));
                        run.meta["output_budget_exhausted"] = json!(true);
                        break;
                    }
                    record.text = Some(text);
                    record.content["text_available"] = json!(true);
                }
                result => {
                    record.content["text_available"] = json!(false);
                    let detail = result.err().map(|e| e.to_string()).unwrap_or_else(|| {
                        "no extractable text; page may be blank or require OCR".into()
                    });
                    let mut issue = Issue::new(
                        "pdf",
                        "pdf_page_text_unavailable",
                        format!("Page {page}: {detail}"),
                    );
                    issue.next_action = Some(
                        "Inspect the saved PDF; OCR or unsupported font decoding may be needed"
                            .into(),
                    );
                    run.errors.push(issue);
                }
            }
            run.items.push(record);
        }
        run.meta["pages_returned"] = json!(run.items.len());
        run.meta["pages_outside_selection"] = json!(pages.len().saturating_sub(run.items.len()));
        run.warnings.push("PDF text order follows content streams; columns, formulas, images and table structure are not reconstructed".into());
        Ok(run)
    };
    match task() {
        Ok(result) => run = result,
        Err(e) => run
            .errors
            .push(Issue::new("pdf", "pdf_parse_failed", e.to_string())),
    }
    run.finish(began.elapsed().as_millis() as u64);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    serde_json::to_writer(file, &run)?;
    Ok(())
}

pub async fn fetch(
    http: &Http,
    input: &str,
    start: u32,
    count: u32,
    store: Option<&Store>,
    budget: Duration,
    executable: &Path,
) -> Run {
    let began = Instant::now();
    let mut run = Run::new("pdf");
    run.meta = json!({"requested_url":input,"start_page":start,"page_limit":count});
    let task = async {
        ensure!(
            start > 0 && (1..=500).contains(&count),
            "PDF page selection must start at 1 or later and contain 1-500 pages"
        );
        let url = net::normalize(input)?;
        let sent = Instant::now();
        let response = http.get(url.clone()).await;
        run.attempts.push(Attempt {
            source: "http".into(),
            url: url.to_string(),
            elapsed_ms: sent.elapsed().as_millis() as u64,
            outcome: response
                .as_ref()
                .map(|r| format!("HTTP {}", r.status.as_u16()))
                .unwrap_or_else(|e| e.to_string()),
        });
        let response = response?;
        ensure!(
            response.status.is_success(),
            "PDF request returned HTTP {}",
            response.status.as_u16()
        );
        let artifact = store.map(|s| s.blob(&response.bytes)).transpose()?;
        run.meta["artifact"] = json!(artifact);
        run.meta["final_url"] = json!(response.url.as_str());
        ensure!(
            response.bytes.starts_with(b"%PDF-"),
            "response does not begin with a PDF signature"
        );
        let directory = tempfile::Builder::new().prefix("ffetch-pdf-").tempdir()?;
        let input_file = directory.path().join("source.pdf");
        let output_file = directory.path().join("result.json");
        std::fs::write(&input_file, &response.bytes)?;
        let final_url = response.url.to_string();
        drop(response);
        let mut child = Worker(
            Command::new(executable)
                .arg("__pdf-worker")
                .arg(&input_file)
                .arg(&output_file)
                .arg(final_url)
                .arg(start.to_string())
                .arg(count.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("could not start PDF worker")?,
        );
        loop {
            if let Some(status) = child.0.try_wait()? {
                ensure!(
                    status.success(),
                    "PDF worker failed or exceeded resource limits ({status}); original artifact retained when storage is enabled"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let size = std::fs::metadata(&output_file)?.len();
        ensure!(
            size <= OUTPUT_LIMIT,
            "PDF worker result exceeds byte budget"
        );
        let mut parsed: Run = serde_json::from_reader(std::fs::File::open(output_file)?)?;
        for record in &mut parsed.items {
            record.provenance.artifact = artifact.clone();
        }
        if let Some(metadata) = parsed.meta.as_object() {
            for (key, value) in metadata {
                run.meta[key.as_str()] = value.clone();
            }
        }
        run.meta["artifact"] = json!(artifact);
        run.merge(parsed);
        Ok::<(), anyhow::Error>(())
    };
    match net::within_budget(budget, task).await {
        Ok(Ok(())) => (),
        Ok(Err(e)) => run
            .errors
            .push(Issue::new("pdf", "pdf_retrieval_failed", e.to_string())),
        Err(_) => run.errors.push(Issue::new(
            "pdf",
            "deadline_exceeded",
            "PDF retrieval/extraction exceeded --timeout; worker cancelled",
        )),
    }
    run.finish(began.elapsed().as_millis() as u64);
    run
}
