//! Slugs for document ids.

/// `https://www.Example.com:8080/a?b` to `example-com-8080`: the origin
/// rules of the bookmark skill.
pub fn origin_slug(url: &str) -> String {
    let lower = url.to_lowercase();
    let rest = lower.split_once("://").map_or(lower.as_str(), |(_, r)| r);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    ::slug::slugify(rest.split('/').next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_slugs_follow_the_bookmark_rules() {
        assert_eq!(origin_slug("https://www.example.com/foo/bar?x=1"), "example-com");
        assert_eq!(origin_slug("http://Example.com/"), "example-com");
        assert_eq!(origin_slug("https://blog.example.org/2026/09/my_post.html#part-2"), "blog-example-org");
        assert_eq!(origin_slug("http://localhost:8080/docs"), "localhost-8080");
    }
}
