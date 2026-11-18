# Prompt files

`AGENTS.md` (with `CLAUDE.md` as a symlink to it) and the criteria files in
this directory are instructions that agents follow literally. Find statements
the repository contradicts, instructions that conflict, and rules an agent
cannot act on. Check every command, path and identifier against the tree.

## Project shape

- `AGENTS.md` is loaded by every agent in this repository, including agents
  that work unattended from a brief.
- `.claude/review/review.yaml` routes reviews. Its tool commands, tags and
  critical paths must agree with the Verification section of `AGENTS.md`.
- Commit style, comment style and writing style belong to the owner's global
  instruction file and are not repeated here.

## Lenses, in priority order

1. False statements: a command that does not run as written, a path or
   symbol that does not exist, a described behaviour the code does not have.
2. Conflicts: two rules that cannot both be followed; a routine action that
   also appears under confirmation; text for a person-read session that
   reaches an unattended agent without scoping.
3. Safety gaps: a route by which an agent following the text could run the
   daemon, reach a BMC, push, or edit another repository without asking.
4. Unactionable rules: a rule with no reason, no example and no way to tell
   whether it was met.
5. Drift: a critical path, tag or command present in one of `AGENTS.md`,
   `review.yaml` and a criteria file and absent from the others.

## Check for what is absent

- Every Verification command is a tool in `review.yaml` or has a stated
  condition.
- Every critical path in `AGENTS.md` is covered by a tag that selects the
  safety dimension.
- Every file the Structure block names exists.

## Finding fields

Add to the standard format:

- Lens: False / Conflict / Safety gap / Unactionable / Drift
- Evidence: the command output or path that contradicts the text
