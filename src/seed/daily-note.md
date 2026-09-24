# Daily notes

A daily note is one document for one day. Its `_id` is the local date as `YYYY-MM-DD`, for example `2026-09-23`. Its `_type` is `doc://schemas/daily`. It has the tag `daily`.

A daily note has two parts:
- `content` is the log for the day. New text goes at the end.
- `intention` is the one intention for the day. A new intention replaces the old one.

## Add to a daily note

1. Find the date. For today, use today's local date. For another day, use that date. If you do not know the date, ask the user.
2. Call `get_doc` with `href` set to the date.
3. If the document is not found, create it. Call `put_doc` with `_id` set to the date, `_type` set to `doc://schemas/daily`, and no `_parent`. Use this body: `{"title": "<date>", "content": "<text>", "tags": ["daily"]}`.
4. If the document is found, update it. Call `put_doc` with `_id` set to the date, `_type` set to `doc://schemas/daily`, and `_parent` set to its `_rev`. Send the full body:
   - Add the new text to the end of `content`. Put one empty line between entries:
     - If `content` is empty or missing, set `content` to the new text.
     - If not, remove the newlines at the end of `content`. Then add `\n\n`, then the new text.
   - Keep all other fields, including `intention`.
   - Make sure that `tags` contains `daily`. If it does not, add it. Keep the other tags.
5. If the document is deleted, the error gives the `_rev` of the tombstone. Call `put_doc` with `_parent` set to that `_rev`, and the body from step 3.
6. If `put_doc` returns a conflict, the note changed after you read it. Go back to step 2.

Write `content` as Markdown. Do not change or remove earlier text unless the user asks.

## Set the intention

Do the steps in "Add to a daily note", with these changes:
- In step 3, use this body: `{"title": "<date>", "content": "", "tags": ["daily"], "intention": "<text>"}`.
- In step 4, set `intention` to the new text. Do not change `content`.

## Find daily notes

- To list daily notes, call `list_docs` with `tag` set to `daily`. The newest changes come first.
- To find a daily note by its text, call `search_docs`.
