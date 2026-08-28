# Domain Docs

How engineering skills should consume this repository's domain documentation when exploring the codebase.

## Before exploring, read these

- **`CONTEXT.md`** at the repository root: the glossary for the whole repository. rec_bag is a single domain context.
- **`docs/adr/`**: read ADRs that affect the whole system.

If any of these files do not exist, **proceed silently**. Do not flag their absence or suggest creating them upfront. The `/domain-modeling` skill creates them lazily when terms or decisions are actually resolved.

## File structure

This repository uses a single-context layout:

```text
/
├── CONTEXT.md                          ← the glossary for the whole repository
├── docs/
│   └── adr/                            ← architectural decisions
└── packages/
```

If the vocabulary of one area ever diverges enough to justify its own language, `/domain-modeling` will introduce a `CONTEXT-MAP.md` at the root and split `CONTEXT.md` per context. Until that file exists, assume one context.

## Use the glossary's vocabulary

When output names a domain concept—in an issue title, refactor proposal, hypothesis, or test name—use the term defined in the relevant `CONTEXT.md`. Do not drift to synonyms the glossary explicitly avoids.

If a needed concept is absent from the glossary, either the proposed language does not match the project or there is a genuine documentation gap. Reconsider the term or note the gap for `/domain-modeling`.

## Flag ADR conflicts

If output contradicts an existing ADR, surface the conflict explicitly rather than silently overriding it:

> _Contradicts ADR-0007, but worth reopening because…_
