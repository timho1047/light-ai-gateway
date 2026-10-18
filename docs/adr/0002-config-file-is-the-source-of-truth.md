# The configuration file stays the only source of truth

**English** | [简体中文](0002-config-file-is-the-source-of-truth.zh-CN.md)

The console could keep its edits in a database and overlay them on the file, or
regenerate the file from a serialised struct. Both were rejected: two sources of
truth would need merge rules and produce a permanent "why did my edit not stick"
class of bug, and a serialised rewrite loses the comments that carry the reasons
for each field. The console is therefore an editor of the same `config.toml`
every command reads. It rewrites the existing document with `toml_edit`, keeping
comments, order and unknown fields; validates the whole file before writing; and
replaces it atomically through a temporary file and a rename. A file that changed
under the form is never overwritten.

## Considered Options

- Regenerate from `Config` with `Serialize`: the types have no `Serialize`
  today, and the rewrite would materialise defaults over a hand-written file.
- Database overrides: quick to build, but the operator has to know which layer
  wins for every field.

## Consequences

- Secrets cannot round-trip through the form. A credential source is displayed as
  a source, and adding or rotating a key writes only to the OS credential store.
- All writers -- console, editor, a future `lgw` command -- go through the same
  validate-then-apply path, so there is one set of rules to test and document.
