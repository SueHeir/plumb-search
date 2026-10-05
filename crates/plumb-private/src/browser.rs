//! The `/private` page's glue: reads the query from the page, fetches its
//! buckets, and shows the results. Built only for WebAssembly.
//!
//! The page (rendered by the node, see `plumb-node`'s `web.rs`) has:
//!
//! - `#pq` with `data-country`, the node's home country for this browser;
//! - the form `#pq-form`, whose input `#pq-q` has no `name`, so submitting
//!   the form without this script sends nothing; the button starts
//!   disabled and is enabled here;
//! - `#pq-status`, a line saying what happened, and `#pq-results`, a list.
//!
//! The query lives in the page's fragment (`/private#q=us+bank`), which
//! browsers never send to the server, so back, reload and bookmarks work.

use plumb_core::keys::{pick_buckets, BUCKETS};
use plumb_core::{display_url, site_initial, truncate_chars, SiteRecord};
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Document, Element, HtmlButtonElement, HtmlInputElement, Request, RequestCache,
    RequestCredentials, RequestInit, RequestMode, Response, Window,
};

use crate::sealed::Targets;
use crate::{
    language_country, padding, query_from_fragment, read_bucket, safe_href, search, Options,
    Ranked, LIMIT,
};

/// What `GET /api/buckets` says about the node's buckets.
#[derive(Debug, Deserialize)]
struct TableInfo {
    /// Names the bucket table being served; part of each bucket's address,
    /// so browsers can cache buckets for good.
    table: String,
    buckets: u32,
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let form = element(&document, "pq-form")?;
    let submit = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        event.prevent_default();
        if let Err(err) = submitted() {
            web_sys::console::error_1(&err);
        }
    });
    form.add_event_listener_with_callback("submit", submit.as_ref().unchecked_ref())?;
    submit.forget();

    let changed = Closure::<dyn FnMut()>::new(|| wasm_bindgen_futures::spawn_local(show()));
    window.add_event_listener_with_callback("hashchange", changed.as_ref().unchecked_ref())?;
    changed.forget();

    if let Ok(button) = element(&document, "pq-go")?.dyn_into::<HtmlButtonElement>() {
        button.set_disabled(false);
    }
    wasm_bindgen_futures::spawn_local(show());
    Ok(())
}

/// The form was sent: put the query in the fragment, which searches.
fn submitted() -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let input: HtmlInputElement = element(&document, "pq-q")?.dyn_into()?;
    let query = input.value();
    let fragment = format!(
        "q={}",
        String::from(js_sys::encode_uri_component(query.trim()))
    );
    if window.location().hash()?.trim_start_matches('#') == fragment {
        // Same query again: no hashchange, so search here.
        wasm_bindgen_futures::spawn_local(show());
    } else {
        window.location().set_hash(&fragment)?;
    }
    Ok(())
}

/// Searches for the query in the fragment, if any, and shows the results.
async fn show() {
    if let Err(err) = try_show().await {
        let message = err
            .as_string()
            .unwrap_or_else(|| "Something went wrong.".to_string());
        let _ = set_status(&message);
        web_sys::console::error_1(&err);
    }
}

async fn try_show() -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let query = query_from_fragment(&window.location().hash()?, |s| {
        js_sys::decode_uri_component(s).ok().map(String::from)
    });
    let query = query.trim().to_string();
    let input: HtmlInputElement = element(&document, "pq-q")?.dyn_into()?;
    input.set_value(&query);
    let list = element(&document, "pq-results")?;
    list.set_text_content(None);
    if query.is_empty() {
        document.set_title("Private search - Plumb Search");
        return set_status("");
    }
    document.set_title(&format!("{query} - Private search - Plumb Search"));
    set_status("Looking it up in your browser\u{2026}")?;

    let page = element(&document, "pq")?;
    let options = Options {
        country: page
            .get_attribute("data-country")
            .filter(|c| !c.is_empty())
            .or_else(|| {
                window
                    .navigator()
                    .language()
                    .and_then(|tag| language_country(&tag))
            }),
        only_country: false,
    };

    let info: TableInfo = serde_json::from_str(&fetch_text(&window, "/api/buckets").await?)
        .map_err(|_| JsValue::from_str("This site's private search is not ready yet."))?;
    if info.buckets != BUCKETS {
        return Err("This site's buckets are of another version.".into());
    }
    let secret = padding_secret(&window)?;
    // Search operators stay in the browser: only the words pick buckets.
    let lookup = plumb_core::Operators::parse(&query).lookup_text();
    let (buckets, keys) = pick_buckets(&lookup, padding(&secret, &lookup));
    // Through this site to other nodes when it can, so no one sees both who
    // asks and what for; otherwise from this site directly.
    let (answers, sealed) = match fetch_sealed(&window, &buckets).await {
        Ok(answers) => (answers, true),
        Err(err) => {
            web_sys::console::log_1(&err);
            (fetch_buckets(&window, &info.table, &buckets).await?, false)
        }
    };
    let hits = search(&query, &keys, answers, &options, LIMIT);

    // The query may have changed while the buckets came in.
    let now = query_from_fragment(&window.location().hash()?, |s| {
        js_sys::decode_uri_component(s).ok().map(String::from)
    });
    if now.trim() != query {
        return Ok(());
    }
    list.set_text_content(None);
    for hit in &hits {
        let item = result_item(&document, hit)?;
        list.append_child(&item)?;
    }
    if hits.is_empty() {
        set_status(&format!(
            "No sites found for \u{201c}{query}\u{201d}. Query text stayed in your browser."
        ))
    } else {
        set_status(&format!(
            "Picked in your browser from {} buckets of sites{}.",
            buckets.len(),
            if sealed {
                ", fetched from other Plumb nodes through this site. Answering nodes see \
                 bucket numbers; this site sees your address. Colluding operators can combine them"
            } else {
                " from this site"
            }
        ))
    }
}

/// Fetches every bucket at once. One that fails fails the search, rather
/// than showing results that may be missing the best one.
async fn fetch_buckets(
    window: &Window,
    table: &str,
    buckets: &[u32],
) -> Result<Vec<Vec<SiteRecord>>, JsValue> {
    let requests = js_sys::Array::new();
    for bucket in buckets {
        let url = format!(
            "/api/buckets/{}/{bucket}",
            String::from(js_sys::encode_uri_component(table))
        );
        requests.push(&window.fetch_with_request(&request(&url, RequestCache::Default)?));
    }
    let responses: js_sys::Array = JsFuture::from(js_sys::Promise::all(&requests))
        .await
        .map_err(|_| JsValue::from_str("Could not reach this site's buckets."))?
        .dyn_into()?;
    let texts = js_sys::Array::new();
    for response in responses.iter() {
        let response: Response = response.dyn_into()?;
        if !response.ok() {
            return Err("This site's buckets changed while you searched. Search again.".into());
        }
        let text = response.text()?;
        texts.push(&text);
    }
    let texts: js_sys::Array = JsFuture::from(js_sys::Promise::all(&texts))
        .await?
        .dyn_into()?;
    texts
        .iter()
        .map(|text| {
            let text = text.as_string().unwrap_or_default();
            read_bucket(&text).map_err(|err| JsValue::from_str(&err))
        })
        .collect()
}

/// Fetches the buckets from other nodes through this site, each sealed to
/// the node that answers it (see [`crate::sealed`]); each bucket is asked of
/// two nodes when there are two, so that one node alone cannot make a site
/// look more popular. Fails when this site lists no other node, or when
/// any request fails.
async fn fetch_sealed(window: &Window, buckets: &[u32]) -> Result<Vec<Vec<SiteRecord>>, JsValue> {
    let listed: Targets =
        serde_json::from_str(&fetch_text(window, "/api/oblivious/targets").await?)
            .map_err(|_| JsValue::from_str("no list of nodes"))?;
    let mut targets = listed.targets;
    if targets.is_empty() {
        return Err("no other node to ask".into());
    }
    let crypto = window.crypto()?;
    for i in (1..targets.len()).rev() {
        let j = (random_u64(&crypto) % (i as u64 + 1)) as usize;
        targets.swap(i, j);
    }
    let now = (js_sys::Date::now() / 1000.0) as u64;
    let per_bucket = targets.len().min(2);
    let mut next = 0;
    let requests = js_sys::Array::new();
    let mut openers = Vec::new();
    for &bucket in buckets {
        for _ in 0..per_bucket {
            let target = &targets[next % targets.len()];
            next += 1;
            let (sealed, opener) =
                crate::sealed::seal(target, bucket, now).map_err(|err| JsValue::from_str(&err))?;
            let url = format!(
                "/api/oblivious/forward/{}",
                String::from(js_sys::encode_uri_component(&target.peer))
            );
            let init = RequestInit::new();
            init.set_method("POST");
            init.set_mode(RequestMode::SameOrigin);
            init.set_credentials(RequestCredentials::Omit);
            init.set_cache(RequestCache::NoStore);
            let body = js_sys::Uint8Array::from(sealed.as_slice());
            init.set_body(&body);
            let request = Request::new_with_str_and_init(&url, &init)?;
            request
                .headers()
                .set("Content-Type", "application/octet-stream")?;
            requests.push(&window.fetch_with_request(&request));
            openers.push(opener);
        }
    }
    let responses: js_sys::Array = JsFuture::from(js_sys::Promise::all(&requests))
        .await?
        .dyn_into()?;
    let bodies = js_sys::Array::new();
    for response in responses.iter() {
        let response: Response = response.dyn_into()?;
        if !response.ok() {
            return Err(format!("a node did not answer ({})", response.status()).into());
        }
        let body = response.array_buffer()?;
        bodies.push(&body);
    }
    let bodies: js_sys::Array = JsFuture::from(js_sys::Promise::all(&bodies))
        .await?
        .dyn_into()?;
    bodies
        .iter()
        .zip(openers)
        .map(|(body, opener)| {
            let bytes = js_sys::Uint8Array::new(&body).to_vec();
            crate::sealed::open(opener, &bytes).map_err(|err| JsValue::from_str(&err))
        })
        .collect()
}

/// A random number from the browser's cryptographic generator.
fn random_u64(crypto: &web_sys::Crypto) -> u64 {
    let mut bytes = [0u8; 8];
    let _ = crypto.get_random_values_with_u8_array(&mut bytes);
    u64::from_le_bytes(bytes)
}

async fn fetch_text(window: &Window, url: &str) -> Result<String, JsValue> {
    let response: Response =
        JsFuture::from(window.fetch_with_request(&request(url, RequestCache::NoStore)?))
            .await
            .map_err(|_| JsValue::from_str("Could not reach this site."))?
            .dyn_into()?;
    if !response.ok() {
        return Err("This site's private search is not ready yet.".into());
    }
    JsFuture::from(response.text()?)
        .await?
        .as_string()
        .ok_or_else(|| "Could not read this site's answer.".into())
}

/// A request to this site only, without cookies.
fn request(url: &str, cache: RequestCache) -> Result<Request, JsValue> {
    let init = RequestInit::new();
    init.set_method("GET");
    init.set_mode(RequestMode::SameOrigin);
    init.set_credentials(RequestCredentials::Omit);
    init.set_cache(cache);
    Request::new_with_str_and_init(url, &init)
}

/// One result, built from text nodes only, laid out like the server's
/// results. Sites show their first letter rather than their icon: fetching
/// icons would tell this site which results the browser found.
fn result_item(document: &Document, hit: &Ranked) -> Result<Element, JsValue> {
    let item = document.create_element("li")?;
    let href = safe_href(&hit.url);
    let title = truncate_chars(hit.title.as_deref().unwrap_or(&hit.domain), 150);
    let link = match &href {
        Some(href) => {
            let link = document.create_element("a")?;
            link.set_attribute("href", href)?;
            link.set_attribute("rel", "noreferrer")?;
            link
        }
        None => document.create_element("div")?,
    };
    link.set_class_name("r");
    let site = span(document, "site", None)?;
    let (letter, color) = site_initial(&hit.domain);
    let badge = span(document, &format!("ic l{color}"), Some(&letter.to_string()))?;
    badge.set_attribute("aria-hidden", "true")?;
    site.append_child(&badge)?;
    let names = span(document, "sn", None)?;
    let domain = span(document, "dn", Some(&hit.domain))?;
    names.append_child(&domain)?;
    // The address, unless it says no more than the domain.
    let shown = href.as_deref().map(display_url);
    if let Some(shown) = shown.filter(|shown| *shown != hit.domain) {
        let url = span(document, "u", Some(&shown))?;
        names.append_child(&url)?;
    }
    site.append_child(&names)?;
    link.append_child(&site)?;
    let heading = span(document, "t", Some(&title))?;
    link.append_child(&heading)?;
    item.append_child(&link)?;
    if let Some(description) = &hit.description {
        let text = document.create_element("p")?;
        text.set_class_name("d");
        text.set_text_content(Some(description));
        item.append_child(&text)?;
    }
    Ok(item)
}

/// A `<span>` of class `class`, holding `text` when there is some.
fn span(document: &Document, class: &str, text: Option<&str>) -> Result<Element, JsValue> {
    let span = document.create_element("span")?;
    span.set_class_name(class);
    if text.is_some() {
        span.set_text_content(text);
    }
    Ok(span)
}

fn set_status(text: &str) -> Result<(), JsValue> {
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or("no document")?;
    element(&document, "pq-status")?.set_text_content(Some(text));
    Ok(())
}

fn element(document: &Document, id: &str) -> Result<Element, JsValue> {
    document
        .get_element_by_id(id)
        .ok_or_else(|| JsValue::from_str(&format!("the page has no #{id}")))
}

/// Where the browser keeps the secret its padding buckets come from.
const SECRET_KEY: &str = "plumb-private-padding";

thread_local! {
    /// The secret, for browsers that keep no local storage (some private
    /// windows): new on each page load.
    static PAGE_SECRET: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// This browser's padding secret (see [`crate::padding`]), made from the
/// browser's cryptographic generator the first time and kept in local
/// storage.
fn padding_secret(window: &Window) -> Result<Vec<u8>, JsValue> {
    let storage = window.local_storage().ok().flatten();
    if let Some(secret) = storage
        .as_ref()
        .and_then(|s| s.get_item(SECRET_KEY).ok().flatten())
        .filter(|s| s.len() == 64)
    {
        return Ok(secret.into_bytes());
    }
    if let Some(secret) = PAGE_SECRET.with(|s| s.borrow().clone()) {
        return Ok(secret.into_bytes());
    }
    let mut bytes = [0u8; 32];
    window
        .crypto()?
        .get_random_values_with_u8_array(&mut bytes)?;
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(storage) = storage {
        let _ = storage.set_item(SECRET_KEY, &secret);
    }
    PAGE_SECRET.with(|s| *s.borrow_mut() = Some(secret.clone()));
    Ok(secret.into_bytes())
}
