# Agent Instructions

See [CLAUDE.md](./CLAUDE.md) for project-specific instructions and guidelines.

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

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **async-geotiff-rs** (1226 symbols, 3027 relationships, 107 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `gitnexus_context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `gitnexus_impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `gitnexus_rename` which understands the call graph.
- NEVER commit changes without running `gitnexus_detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/async-geotiff-rs/context` | Codebase overview, check index freshness |
| `gitnexus://repo/async-geotiff-rs/clusters` | All functional areas |
| `gitnexus://repo/async-geotiff-rs/processes` | All execution flows |
| `gitnexus://repo/async-geotiff-rs/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->
