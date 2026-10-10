//! Extract the same saved HTML with baseline and candidate builds.
use std::io::{self, Read};
fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).unwrap();
    let rows: Vec<serde_json::Value> = serde_json::from_str(&input).unwrap();
    for row in rows {
        let meta = plumb_crawl::extract_page_meta(
            &url::Url::parse(row["url"].as_str().unwrap()).unwrap(),
            row["html"].as_str().unwrap(),
        );
        let rich = plumb_crawl::extract_inner_page_meta(
            &url::Url::parse(row["url"].as_str().unwrap()).unwrap(),
            row["html"].as_str().unwrap(),
            plumb_crawl::InnerPageExtraction::Docs,
        )
        .search;
        println!(
            "{}",
            serde_json::json!({"id": row["id"], "url": row["url"], "meta": meta, "rich": rich, "body": meta.body_text,
            "page": meta.page_text, "title": meta.title, "description": meta.description,
            "headings": meta.headings, "sections": meta.sections,
            "structured_names": meta.structured_names, "terms": meta.terms})
        );
    }
}
