# Daily brief

A brief is a short page of food for thought for one day. It brings back ideas from the user's own notes. It uses the day's intention as its theme. The user must be able to read it in about two minutes.

## Make a brief

1. Find the theme.
   - Find today's local date. Call `get_doc` with `href` set to the date plus `.md`, for example `2026-09-23.md`. See the daily-note skill.
   - If the note has an `intention`, use it as the theme.
   - If not, call `changes` or `list_docs` to find the notes the user changed recently. Find one theme in them.
2. Find notes. Call `search_docs` many times:
   - `search_docs` finds keywords, not meanings. A note must contain every word in the query. Use one or two words in each query.
   - Search for the words in the theme. Then search for synonyms and related words.
   - Use only the user's own notes. Do not use daily notes, runs, tasks, runners, skills, prompts, or schemas.
   - If yesterday's daily note has a `## Brief` section, do not use the notes it links to.
   - Call `get_doc` to read the notes that you select.
3. Write the brief in Markdown, with the sections in "Brief sections".

## Brief sections

### Theme

One line. Say the theme and why you selected it: the intention, or the notes it came from.

### Review

Excerpts from 3 notes that relate to the theme. For each note:
- A short quote from the note.
- A link to the note as `doc://<id>`.
- One line that tells why the note is important for the theme today.

### Prompt

One provocation that relates to the theme. It must help the user find new ideas. It must not test what the user remembers. Use one of these methods:
- Oblique Strategies: a short, strange instruction, for example "Honor thy error as a hidden intention".
- SCAMPER: substitute, combine, adapt, modify, put to another use, eliminate, reverse.
- Six Thinking Hats: facts, feelings, risks, benefits, new ideas, process.
- Brainstorm questions: "What if...?", "How might we...?", "What would make this ten times bigger?".
- The questions at the end of a chapter in a textbook: apply, compare, predict, criticize.

### Collider

Join two ideas to make a new idea.
1. Select two notes that are far apart: different topics, or different times. They can be notes from the review.
2. Use the Zettelkasten Compass to find a relation between the two notes. For each note X, ask:
   - North, "Where does X come from?" What is the origin of X? What group or category does X belong to? What is one level higher? Zoom out. What caused X?
   - West, "What is similar to X?" What other disciplines could X already exist in? What other disciplines could X help? What are other ways to say or do X?
   - South, "Where can X lead to?" What does X contribute to? What group or category could X be the headline of? What is one level lower? Zoom in. What does X help to grow?
   - East, "What competes with X?" What is the opposite of X? What does X not have? What is its disadvantage? What could make X much stronger?
3. Write a draft for a new note that joins the two notes: a title and one or two short paragraphs. Link to the two notes as `doc://<id>`. Say which compass directions you used.
4. Write one prompt that asks the user to continue the draft.

Keep the draft in the brief. Do not create a new document.
