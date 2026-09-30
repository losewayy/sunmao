---
name: html-artifact
description: Produce reviewable HTML artifacts (plans, reports, dashboards) with the HtmlArtifact tool, and collect human annotations via a state.json sidecar
---

# HTML artifacts

Use `HtmlArtifact` for anything meant for a human to *look at* — plans,
reports, checklists, design drafts. Do not emit `plan.md` when the
deliverable is the document itself.

## Producing

- Call `HtmlArtifact` with a `[a-z0-9_-]` slug and a complete HTML
  document. The tool writes `.sunmao/artifacts/{name}.html` and logs an
  `Artifact` event.
- Keep artifacts self-contained: inline CSS, no external network
  resources — renderers sandbox them (CSP, no node, no outbound net).
- `template.html` in this skill is a minimal reviewable-plan layout:
  title, sections, and an annotation block at the bottom. Copy and adapt.

## Annotation round-trip (state.json)

An artifact is a two-way surface. When the human should review it:

1. Emit the artifact from `template.html` (or include an equivalent
   `<section id="annotations">` block).
2. Tell the human the state path: `.sunmao/artifacts/{name}.state.json`.
3. The human (or the frontend, or the TUI) writes annotations as JSON:

   ```json
   {"annotations": [{"section": "rollout", "note": "stage 2 first", "at": "2026-09-30"}]}
   ```

4. On the next turn, `Read` the state file if it exists and fold the
   notes into the revision. Update the artifact with a new
   `HtmlArtifact` call (same name overwrites); record resolved notes by
   appending `"resolved": true` to each entry — never delete them.

The artifact is the shared document; state.json is the margin. Both
stay plain files — no server, no database.
