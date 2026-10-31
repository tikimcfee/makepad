//! The circle registry: the built-in circles of the repo the caller stands in, plus
//! `~/.makepad/index/circles.conf`, as a [`crate::CircleManifest`]; the default scope; `--in`.
//!
//! Built in: `makepad` (the tree; excludes [`crate::INDEX_OWN_DATA`]), `makepad:history`, and
//! `commercial` / `commercial:history` when `apps/commercial` is present. Machine-local:
//! `notes:local`, the repo's untracked `local/` when present, minus the directories other circles
//! claim ([`crate::Provenance::ClaimedBy`]), and every `circles.conf` circle. A circle that is not
//! configured is [`crate::Error::UnknownCircle`], never an empty result. Two sources of one circle
//! with the same label are a `circles.conf` error. Owns
//! [`crate::Circles::def_hash`]: SHA-1 over a canonical encoding of families, exclusions, ref
//! names and notes labels, never machine paths.
//!
//! `circles.conf`, one definition per line, `#` comments; a name defined twice adds sources:
//! ```text
//! notes:<topic> = notes ~/notes/<topic>              # a notes directory, label `<topic>`
//! notes:<topic> = notes local/agent_state/<dir>      # a repo-local dir it claims from notes:local,
//!                                                    # label `<repo dir>/local/agent_state/<dir>`
//! notes:<topic> = notes ~/x/y as <label>             # an explicit label
//! <repo> = tree ~/src/<repo>                         # another repo, a consumer, never a member
//! <repo>:history = history ~/src/<repo> refs/remotes/origin/main refs/heads/main
//! <name> = select <circle> package <pkg>             # derived: kind <k>,.. | path <prefix> | package <p>
//! <name> = overlap <circle> <circle> on path         # derived: on path | commit | blob | ident | package
//! <name> = union <circle> <circle> ..                # derived: blocks (default) or `fuse rank`
//! eval = ~/notes/<topic>/questions.jsonl             # machine-local eval questions
//! ```
