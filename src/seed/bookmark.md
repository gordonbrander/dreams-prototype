# Bookmarks

A bookmark is one document for one web page. Its `_type` is `doc://schemas/bookmark`. It has the tag `bookmark`.

A bookmark has these fields:
- `url` is the address of the page, as the user gave it.
- `title` is the title of the page.
- `content` is a short summary of the page, then the notes from the user.
- `tags` contains `bookmark` and some topic tags.

## Make the id

The `_id` of a bookmark is `bookmarks/<origin-slug>/<path-slug>.md`. All bookmarks from one site thus share the prefix `bookmarks/<origin-slug>/`. Make the slugs from the URL with these steps:

1. Make all letters lowercase.
2. Remove the scheme, for example `https://`.
3. Remove `www.` at the start.
4. Remove the query and the fragment: all text from the first `?` or `#`.
5. Divide the rest at the first `/`. The text before it is the origin: the host and the port. The text after it is the path.
6. In the origin and in the path, replace each sequence of characters that are not `a`-`z` or `0`-`9` with one `-`. Then remove `-` at the start and at the end.
7. If the path slug is empty, use `index`.

Examples:
- `https://www.example.com/foo/bar?x=1` gives `bookmarks/example-com/foo-bar.md`.
- `http://Example.com/` gives `bookmarks/example-com/index.md`.
- `https://blog.example.org/2026/09/my_post.html#part-2` gives `bookmarks/blog-example-org/2026-09-my-post-html.md`.
- `http://localhost:8080/docs` gives `bookmarks/localhost-8080/docs.md`.

## Save a bookmark

1. Make the id from the URL.
2. Get the page with your web fetch tool. Find its title and read its text. If you cannot get the page, use the URL as the title, and tell the user.
3. Write the summary: two to five sentences or bullets in Markdown about what the page says.
4. Call `get_doc` with `href` set to the id.
5. If the document is not found, create it. Call `put_doc` with `_id` set to the id, `_type` set to `doc://schemas/bookmark`, and no `_parent`. Use this body: `{"title": "<title>", "url": "<url>", "content": "<content>", "tags": ["bookmark", ...]}`.
   - `content` is the summary. If the user gave notes, add `\n\n## Notes\n\n` and then the notes.
   - `tags` is `bookmark` and one to five topic tags. Write tags in lowercase, with `-` between words. Use tags that other bookmarks use when they fit. To see them, call `list_docs` with `tag` set to `bookmark`.
6. If the document is found, update it. Call `put_doc` with `_id` set to the id, `_type` set to `doc://schemas/bookmark`, and `_parent` set to its `_rev`. Send the full body:
   - Set `title` and `url` to the new values.
   - In `content`, replace the summary with the new summary. Keep the `## Notes` section. Add the new notes to the end of it, with one empty line between notes.
   - Keep the old tags. Add new topic tags if they fit. Make sure that `tags` contains `bookmark`.
   - Keep all other fields.
7. If the document is deleted, the error gives the `_rev` of the tombstone. Call `put_doc` with `_parent` set to that `_rev`, and the body from step 5.
8. If `put_doc` returns a conflict, the bookmark changed after you read it. Go back to step 4.
9. Tell the user the title, the tags, and the id.

## Find bookmarks

- To find the bookmark for a URL, make the id from the URL. Then call `get_doc` with `href` set to the id.
- To list bookmarks, call `list_docs` with `tag` set to `bookmark`. The newest changes come first.
- To list the bookmarks from one site, make the origin slug from its URL. Then call `list_docs` with `prefix` set to `bookmarks/<origin-slug>/`.
- To find bookmarks about a subject, call `search_docs`. It searches `title`, `content`, and `tags`. It does not search `url`.
