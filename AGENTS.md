# Agent Instructions

# Issue Tracking (Multica)

When you discover a problem while working in this repo — a bug, security
issue, or performance issue that isn't what the user directly asked you to
fix right now — record it as an issue via the `multica` CLI in the
**async-geotiff** project (project id `4b82c4e6-4bc0-412e-96af-b8f538f90753`,
workspace id `6265acfe-9f1b-4902-a197-16ea5437f872`). Don't just leave it in
chat — chat context doesn't survive between sessions, Multica issues do.

**Check for an existing issue before creating one:**

```
multica issue list --project 4b82c4e6-4bc0-412e-96af-b8f538f90753 --output json
multica issue search "<keyword>" --output json   # workspace-wide; filter by project_id
```

If an issue already covers the same root cause / same file:line, do **not**
create a duplicate — add a comment instead if there's new detail. Only create
a new issue when nothing existing already covers it.

**Creating one:**

```
multica issue create \
  --project 4b82c4e6-4bc0-412e-96af-b8f538f90753 \
  --title "<short, specific summary — include file:line>" \
  --description-file <path-to-a-scratch-file> \
  --priority <low|medium|high> \
  --output json
```

- Use `--description-file`, not `--description`, for anything multi-line or
  containing non-ASCII text — avoids shell-escaping corruption.
- Description should cover: what's wrong, why it matters, concrete file:line
  references, and a suggested fix.
- Check `multica label list --output json` and reuse an existing label
  (`安全`, `性能优化`, `bug`, `新需求`) via
  `multica issue label add <issue-id> <label-id>`. Only create a new label
  with `multica label create` if none fit.

This is for incidental findings surfaced during audits, review, or
debugging — it does not replace completing the task the user actually asked
for.


