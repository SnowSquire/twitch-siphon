# Agent guidance

## Comments

Comments describe the code as it exists now. Never describe the change itself.

- Explain *why* something non-obvious is the way it is: invariants,
  ownership, threading, ordering constraints, failure modes.
- Do not write what changed, what it used to do, what was removed, or why
  the change was made. That lives in git history and review discussion.
- Banned framing in comments: "now", "no longer", "instead of",
  "previously", "used to", "was removed", "startup init" as a synonym for
  "this used to be seeded elsewhere", or any reference to a prior shape.
- If a comment only makes sense as a diff ("so no per-channel commands are
  sent here"), delete it or rewrite it as a timeless invariant.
- Keep comments short. Prefer making the code obvious over explaining it.
