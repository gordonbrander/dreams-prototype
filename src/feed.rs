//! Feeds: documents typed `doc://schemas/feed` that name a resource to
//! fetch. A pull reads the resource with the adaptor for its `kind` and
//! writes each item as a `doc://schemas/feed-item` document under the
//! feed's id without `.md`. The item documents are the record of what was
//! seen: an item that exists, or that has a tombstone, is not new. A pull
//! returns only the new items.
//!
//! Fetching is kept apart from the store, so a caller can fetch without
//! holding a lock on the store: `gather` fetches and parses, `apply` writes.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::doc::{Doc, DocRef, PutInput};
use crate::error::StoreError;
use crate::hash::sha256_hex;
use crate::store::Store;

/// Seeded schema documents, as type paths.
pub const FEED_TYPE: &str = "doc://schemas/feed";
pub const ITEM_TYPE: &str = "doc://schemas/feed-item";

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;
/// Line width for page text. Wide, so text wraps rarely.
const TEXT_WIDTH: usize = 100;
/// Characters in a new item's `description`.
pub const DESCRIPTION_CHARS: usize = 150;

/// The body of `schemas/feed`.
pub const FEED_SCHEMA: &str = r#"{
  "title": "Feed",
  "description": "A resource to pull: `dreams feed pull`, or the pull_feeds tool. kind rss reads RSS or Atom, one item per entry. kind html reads one web page as text, and a change in the text is a new item. Each item is a doc://schemas/feed-item document under the feed's _id without .md: feeds/example-com.md has its items under feeds/example-com/. instructions are for the agent that processes the items of this feed, for example to correct for a known bias of the source; read them before you process an item.",
  "type": "object",
  "required": ["url", "kind"],
  "properties": {
    "url": {"type": "string", "minLength": 1},
    "kind": {"enum": ["rss", "html"]},
    "title": {"type": "string"},
    "instructions": {"type": "string"},
    "tags": {"type": "array", "items": {"type": "string"}}
  }
}"#;

/// The body of `schemas/feed-item`.
pub const ITEM_SCHEMA: &str = r#"{
  "title": "Feed item",
  "description": "One item that a pull of `feed` wrote, as the feed gave it. content is the item's content or summary, verbatim; for an html feed it is the text of the page. Before you process an item, read the instructions of its feed document, if it has them.",
  "type": "object",
  "required": ["feed"],
  "properties": {
    "feed": {"type": "string", "pattern": "^doc://"},
    "url": {"type": "string"},
    "title": {"type": "string"},
    "published": {"type": "string"},
    "guid": {"type": "string"},
    "content": {"type": "string"},
    "tags": {"type": "array", "items": {"type": "string"}}
  }
}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// RSS or Atom: one item per entry.
    Rss,
    /// One web page, as text.
    Html,
}

impl Kind {
    pub fn parse(text: &str) -> Result<Kind, StoreError> {
        match text {
            "rss" => Ok(Kind::Rss),
            "html" => Ok(Kind::Html),
            _ => Err(StoreError::invalid(format!("unknown feed kind {text:?}: use rss or html"))),
        }
    }
}

/// A typed view of a feed document.
#[derive(Debug, Clone)]
pub struct Feed {
    pub id: String,
    pub url: String,
    pub kind: Kind,
    pub title: Option<String>,
    /// For the agent that processes the items.
    pub instructions: Option<String>,
}

impl Feed {
    pub fn from_doc(doc: &Doc) -> Result<Feed, StoreError> {
        if doc.type_path() != Some(FEED_TYPE) {
            return Err(StoreError::invalid(format!("{} is not a {FEED_TYPE} document", doc.id)));
        }
        let text = |key: &str| doc.body.get(key).and_then(Value::as_str).map(str::to_string);
        Ok(Feed {
            id: doc.id.clone(),
            url: text("url").unwrap_or_default(),
            kind: Kind::parse(&text("kind").unwrap_or_default())?,
            title: text("title"),
            instructions: text("instructions"),
        })
    }

    /// Where the items go: the id without `.md`.
    pub fn base(&self) -> &str {
        self.id.strip_suffix(".md").unwrap_or(&self.id)
    }
}

/// The id `feed add` uses when it is given none: `feeds/<origin-slug>.md`.
pub fn default_id(url: &str) -> Result<String, StoreError> {
    match crate::slug::origin_slug(url) {
        s if s.is_empty() => Err(StoreError::invalid(format!("cannot make an id from {url:?}; pass --id"))),
        s => Ok(format!("feeds/{s}.md")),
    }
}

/// One item to write.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemDraft {
    pub id: String,
    pub body: Map<String, Value>,
}

/// A new item as a pull reports it.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct NewItem {
    /// `doc://<id>?rev=<rev>` of the revision the pull wrote.
    pub href: String,
    pub title: String,
    /// The start of the content as plain text.
    pub description: String,
}

impl NewItem {
    fn of(kind: Kind, doc: &Doc) -> NewItem {
        let text = |key: &str| doc.body.get(key).and_then(Value::as_str).unwrap_or_default();
        let content = match kind {
            // Feed content is often HTML.
            Kind::Rss => html2text::config::plain_no_decorate()
                .string_from_read(text("content").as_bytes(), 10_000)
                .unwrap_or_else(|_| text("content").to_string()),
            Kind::Html => text("content").to_string(),
        };
        NewItem {
            href: DocRef::pinned(&doc.id, &doc.rev).to_string(),
            title: text("title").to_string(),
            description: clip_words(&content, DESCRIPTION_CHARS),
        }
    }
}

/// Collapse whitespace, then cut to `max` characters, with `…` when cut.
fn clip_words(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max).collect();
    cut.push('…');
    cut
}

/// Turn fetched bytes into the items to write. Pure.
pub fn parse(feed: &Feed, bytes: &[u8]) -> Result<Vec<ItemDraft>, StoreError> {
    let feed_ref = DocRef::from_cli(&feed.id)?.path();
    match feed.kind {
        Kind::Rss => parse_entries(feed, &feed_ref, bytes),
        Kind::Html => {
            let content = html2text::config::plain()
                .string_from_read(bytes, TEXT_WIDTH)
                .map_err(|e| StoreError::invalid(format!("reading {}: {e}", feed.url)))?;
            let Value::Object(body) = json!({
                "feed": feed_ref,
                "url": feed.url,
                "title": feed.title.clone().unwrap_or_else(|| feed.url.clone()),
                "content": content,
            }) else {
                unreachable!("a JSON object")
            };
            Ok(vec![ItemDraft { id: format!("{}/page.md", feed.base()), body }])
        }
    }
}

/// One item per entry, oldest first. Feeds list the newest first, so the
/// order is the reverse of the document order.
fn parse_entries(feed: &Feed, feed_ref: &str, bytes: &[u8]) -> Result<Vec<ItemDraft>, StoreError> {
    // No generated ids: feed-rs falls back to a random UUID, which would
    // make each pull see every entry as new.
    let parsed = feed_rs::parser::Builder::new()
        .id_generator(|_, _, _| String::new())
        .build()
        .parse(bytes)
        .map_err(|e| StoreError::invalid(format!("reading {}: {e}", feed.url)))?;
    let mut drafts = Vec::new();
    for entry in parsed.entries.into_iter().rev() {
        let link = entry.links.first().map(|l| l.href.clone());
        let title = entry.title.map(|t| t.content.trim().to_string()).filter(|t| !t.is_empty());
        let published = entry.published.or(entry.updated).map(|d| d.to_rfc3339());
        let content = entry.content.and_then(|c| c.body).or(entry.summary.map(|s| s.content));
        let seen_as = if !entry.id.is_empty() {
            entry.id.clone()
        } else if let Some(l) = &link {
            l.clone()
        } else {
            format!("{}\n{}", title.as_deref().unwrap_or_default(), published.as_deref().unwrap_or_default())
        };
        if seen_as.trim().is_empty() {
            continue;
        }
        let mut body = Map::new();
        body.insert("feed".into(), Value::String(feed_ref.to_string()));
        let fields = [
            ("guid", Some(entry.id).filter(|g| !g.is_empty())),
            ("url", link),
            ("title", title),
            ("published", published),
            ("content", content),
        ];
        for (name, value) in fields {
            if let Some(v) = value {
                body.insert(name.into(), Value::String(v));
            }
        }
        drafts.push(ItemDraft { id: format!("{}/{}.md", feed.base(), sha256_hex(seen_as.as_bytes())), body });
    }
    Ok(drafts)
}

/// GET `url`. A status that is not 2xx is an error.
pub fn fetch(url: &str) -> Result<Vec<u8>, StoreError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("dreams/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let fail = |e: ureq::Error| StoreError::invalid(format!("fetching {url}: {e}"));
    agent.get(url).call().map_err(fail)?.body_mut().with_config().limit(MAX_BODY_BYTES).read_to_vec().map_err(fail)
}

/// A feed that could not be pulled.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct PullError {
    pub feed: String,
    pub error: String,
}

/// The new items of one feed, with what an agent needs to process them.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct FeedItems {
    /// `doc://<id>` of the feed.
    pub feed: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The feed's instructions for the agent that processes its items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Items that were not seen before, in the order they were written.
    pub items: Vec<NewItem>,
}

/// What a pull wrote.
#[derive(Debug, Clone, Default, PartialEq, Serialize, JsonSchema)]
pub struct PullReport {
    /// Each feed that has new items, in id order.
    pub feeds: Vec<FeedItems>,
    /// Feeds that failed. The others were still pulled.
    pub errors: Vec<PullError>,
}

/// The feeds to pull: one by id, or every feed in id order. A feed
/// document that cannot be read becomes an error.
pub fn feeds(store: &Store, only: Option<&str>) -> Result<Vec<Result<Feed, PullError>>, StoreError> {
    let docs = match only {
        Some(id) => {
            let doc = store.get(id)?;
            Feed::from_doc(&doc)?;
            vec![doc]
        }
        None => {
            let mut docs = store.list_all(Some(FEED_TYPE))?;
            docs.sort_by(|a, b| a.id.cmp(&b.id));
            docs
        }
    };
    Ok(docs
        .iter()
        .map(|d| Feed::from_doc(d).map_err(|e| PullError { feed: d.id.clone(), error: e.to_string() }))
        .collect())
}

/// Fetch and parse each feed. Touches no store.
pub fn gather(feeds: Vec<Result<Feed, PullError>>) -> Vec<Result<(Feed, Vec<ItemDraft>), PullError>> {
    feeds
        .into_iter()
        .map(|f| {
            let feed = f?;
            let failed = |e: StoreError| PullError { feed: feed.id.clone(), error: e.to_string() };
            let drafts = fetch(&feed.url).and_then(|bytes| parse(&feed, &bytes)).map_err(failed)?;
            Ok((feed, drafts))
        })
        .collect()
}

/// Write the items that were not seen before, and return them.
///
/// - No document: create it.
/// - A tombstone: skip it. It was seen, then deleted.
/// - An rss item that exists: skip it, even when the feed edited it.
/// - An html page that exists: write the next revision when its text changed.
pub fn apply(store: &mut Store, feed: &Feed, drafts: Vec<ItemDraft>) -> Result<Vec<NewItem>, StoreError> {
    let mut new = Vec::new();
    for draft in drafts {
        let parent = match store.get(&draft.id) {
            Err(StoreError::NotFound { .. }) => None,
            Err(StoreError::Deleted { .. }) => continue,
            Ok(head) if feed.kind == Kind::Rss || head.body == draft.body => continue,
            Ok(head) => Some(head.rev),
            Err(e) => return Err(e),
        };
        let input = PutInput { id: Some(draft.id), parent, type_id: Some(ITEM_TYPE.into()), body: draft.body };
        match store.put(input) {
            Ok(doc) => new.push(NewItem::of(feed.kind, &doc)),
            // Another writer got there first: seen.
            Err(StoreError::Conflict { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(new)
}

/// Apply every gathered feed. A feed with no new items is left out of the
/// report. One failure never stops the others.
pub fn apply_all(store: &mut Store, gathered: Vec<Result<(Feed, Vec<ItemDraft>), PullError>>) -> PullReport {
    let mut report = PullReport::default();
    for g in gathered {
        let applied = g.and_then(|(feed, drafts)| match apply(store, &feed, drafts) {
            Ok(items) => Ok((feed, items)),
            Err(e) => Err(PullError { feed: feed.id.clone(), error: e.to_string() }),
        });
        match applied {
            Ok((_, items)) if items.is_empty() => {}
            Ok((feed, items)) => report.feeds.push(FeedItems {
                feed: format!("{}{}", DocRef::SCHEME, feed.id),
                title: feed.title,
                instructions: feed.instructions,
                items,
            }),
            Err(e) => report.errors.push(e),
        }
    }
    report
}

/// Pull one feed, or every feed.
pub fn pull(store: &mut Store, only: Option<&str>) -> Result<PullReport, StoreError> {
    let gathered = gather(feeds(store, only)?);
    Ok(apply_all(store, gathered))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        crate::seed::seed(&mut store).unwrap();
        store
    }

    fn feed(kind: Kind) -> Feed {
        Feed {
            id: "feeds/example-com.md".into(),
            url: "https://example.com/feed".into(),
            kind,
            title: None,
            instructions: None,
        }
    }

    const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Example</title><link>https://example.com/</link>
  <item><title>Newest</title><guid>g-2</guid><link>https://example.com/2</link>
    <description>&lt;p&gt;Second &lt;b&gt;post&lt;/b&gt;&lt;/p&gt;</description>
    <pubDate>Tue, 22 Sep 2026 10:00:00 GMT</pubDate></item>
  <item><title>No guid</title><link>https://example.com/1</link><description>First</description></item>
  <item><title>Only a title</title></item>
</channel></rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"><title>Example</title><id>urn:feed</id>
  <updated>2026-09-22T10:00:00Z</updated>
  <entry><title>An entry</title><id>urn:entry:1</id><updated>2026-09-22T10:00:00Z</updated>
    <link href="https://example.com/e1"/><content type="html">&lt;p&gt;Body&lt;/p&gt;</content></entry>
</feed>"#;

    #[test]
    fn rss_items_are_oldest_first_with_stable_keys() {
        let f = feed(Kind::Rss);
        let drafts = parse(&f, RSS.as_bytes()).unwrap();
        let titles: Vec<&str> = drafts.iter().map(|d| d.body["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["Only a title", "No guid", "Newest"]);
        let newest = &drafts[2];
        assert_eq!(newest.id, format!("feeds/example-com/{}.md", sha256_hex(b"g-2")));
        assert_eq!(newest.body["guid"], "g-2");
        assert_eq!(newest.body["url"], "https://example.com/2");
        assert_eq!(newest.body["feed"], "doc://feeds/example-com.md");
        assert_eq!(newest.body["content"], "<p>Second <b>post</b></p>");
        assert!(newest.body["published"].as_str().unwrap().starts_with("2026-09-22T10:00:00"));
        // No guid: the link is the key.
        assert_eq!(drafts[1].id, format!("feeds/example-com/{}.md", sha256_hex(b"https://example.com/1")));
        assert!(!drafts[1].body.contains_key("guid"));
        // No guid and no link: the title and date are the key.
        assert_eq!(drafts[0].id, format!("feeds/example-com/{}.md", sha256_hex(b"Only a title\n")));
        assert_eq!(parse(&f, RSS.as_bytes()).unwrap(), drafts, "a second parse gives the same drafts");
    }

    #[test]
    fn atom_entries_parse() {
        let drafts = parse(&feed(Kind::Rss), ATOM.as_bytes()).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].id, format!("feeds/example-com/{}.md", sha256_hex(b"urn:entry:1")));
        assert_eq!(drafts[0].body["url"], "https://example.com/e1");
        assert_eq!(drafts[0].body["content"], "<p>Body</p>");
    }

    #[test]
    fn html_is_one_page_of_text() {
        let f = feed(Kind::Html);
        let a = parse(&f, b"<html><body><h1>Hello</h1><p>World</p></body></html>").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].id, "feeds/example-com/page.md");
        let content = a[0].body["content"].as_str().unwrap();
        assert!(content.contains("Hello") && content.contains("World") && !content.contains("<p>"), "{content}");
        assert_eq!(a[0].body["title"], "https://example.com/feed");
        let b =
            parse(&f, b"<html><body><h1 class=\"x\">Hello</h1><p>World</p><script>n=1</script></body></html>").unwrap();
        assert_eq!(a, b, "markup that keeps the text keeps the body");
    }

    #[test]
    fn default_ids_use_the_origin_slug() {
        assert_eq!(default_id("https://news.ycombinator.com/rss").unwrap(), "feeds/news-ycombinator-com.md");
        assert!(default_id("https://").is_err());
    }

    #[test]
    fn descriptions_are_plain_short_text() {
        assert_eq!(clip_words("  a \n\n b\tc ", 150), "a b c");
        let long = "é".repeat(200);
        let cut = clip_words(&long, 150);
        assert_eq!(cut.chars().count(), 151);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn rss_apply_reports_each_item_once() {
        let mut store = store();
        let f = feed(Kind::Rss);
        let drafts = parse(&f, RSS.as_bytes()).unwrap();
        let first = apply(&mut store, &f, drafts.clone()).unwrap();
        assert_eq!(first.len(), drafts.len());
        let newest = first.last().unwrap();
        assert_eq!(newest.title, "Newest");
        assert_eq!(newest.description, "Second post");
        let doc = store.get_href(&newest.href, false).unwrap();
        assert_eq!(doc.type_path(), Some(ITEM_TYPE));

        assert_eq!(apply(&mut store, &f, drafts.clone()).unwrap(), [], "a second pull sees nothing new");

        // An edit by the feed is not new.
        let mut edited = drafts.clone();
        edited[2].body.insert("title".into(), json!("Newest, edited"));
        assert_eq!(apply(&mut store, &f, edited).unwrap(), []);

        // A deleted item stays deleted.
        let head = store.get(&drafts[0].id).unwrap();
        store.delete(&head.id, &head.rev).unwrap();
        assert_eq!(apply(&mut store, &f, drafts.clone()).unwrap(), []);
        assert!(matches!(store.get(&drafts[0].id), Err(StoreError::Deleted { .. })));
    }

    #[test]
    fn html_apply_reports_changed_text() {
        let mut store = store();
        let f = feed(Kind::Html);
        let v1 = parse(&f, b"<p>One</p>").unwrap();
        let first = apply(&mut store, &f, v1.clone()).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(apply(&mut store, &f, v1).unwrap(), []);
        let second = apply(&mut store, &f, parse(&f, b"<p>Two</p>").unwrap()).unwrap();
        assert_eq!(second.len(), 1);
        assert_ne!(second[0].href, first[0].href);
        assert_eq!(second[0].description, "Two");
        assert_eq!(store.history("feeds/example-com/page.md", None).unwrap().revisions.len(), 2);
    }

    #[test]
    fn apply_all_groups_new_items_by_feed() {
        let mut store = store();
        let rss = Feed {
            title: Some("Example".into()),
            instructions: Some("This source leans one way; look for the other side.".into()),
            ..feed(Kind::Rss)
        };
        let page = Feed { id: "feeds/page.md".into(), ..feed(Kind::Html) };
        let gathered = || {
            vec![
                Ok((rss.clone(), parse(&rss, RSS.as_bytes()).unwrap())),
                Ok((page.clone(), parse(&page, b"<p>Page</p>").unwrap())),
                Err(PullError { feed: "feeds/bad.md".into(), error: "no".into() }),
            ]
        };
        let report = apply_all(&mut store, gathered());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.feeds.len(), 2);
        let first = &report.feeds[0];
        assert_eq!(first.feed, "doc://feeds/example-com.md");
        assert_eq!(first.title.as_deref(), Some("Example"));
        assert_eq!(first.instructions, rss.instructions);
        assert_eq!(first.items.len(), parse(&rss, RSS.as_bytes()).unwrap().len());
        assert_eq!(report.feeds[1].feed, "doc://feeds/page.md");
        assert_eq!(report.feeds[1].instructions, None);

        let again = apply_all(&mut store, gathered());
        assert_eq!(again.feeds, [], "a feed with no new items is left out");
    }
}
