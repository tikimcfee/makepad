# repo_index fixture

A tiny tree for the unit tests of `libs/repo_index`. The tests commit it into a scratch git
repository and index it as a `tree` circle with these exclusions:

- `excluded/`: provenance `IndexOwnData`. The term `fixture_sentinel_excluded` must never
  appear in any chunk, term or statistic, whether from the base or the overlay.
- `notes/`: claimed by the notes circle `notes:topic-a` (provenance `ClaimedBy`).

What each part exercises:

- `alpha`: a described crate with a `//!` header, documented and undocumented items, an impl
  whose methods become items, a trait, a macro, a const, and a path dependency on `beta`.
- `beta`: a crate with no description and no header.
- `docs/guide.md`: Markdown sections, a fenced block holding a fake heading.
- `ui/demo.splash`: splash top-level definitions.
- `notes/topic-a`: notes sections citing `alpha/src/lib.rs:15` and the item `Gizmo::spin`;
  `COPY.md` is byte-identical to `PLAN.md` and must collapse into one hit.
