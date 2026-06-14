I've reviewed the diff and the full current state of the project against the overview. The changes add a `humbugs` CLI subcommand (which is just an alias for `check` — see `src/main.rs:102 Cmd::Check | Cmd::Humbugs =>`) and a corresponding plugin command (`plugin/commands/humbugs.md`). This is purely a convenience entry point; it doesn't alter the architecture, the check suite pipeline (formatters→tests→lint), any invariant, or any tool definition. The language-support note in README is informational only.

The overview's description of purpose and architecture is still accurate.

UNCHANGED