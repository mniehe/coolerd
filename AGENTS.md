# Project Context

ipmi-fan-control is a Rust daemon that sets the case-fan duty cycle on
Supermicro motherboards over IPMI. It reads temperatures from IPMI sensors,
files, `smartctl` and `hdparm`, maps them through a per-zone curve, and writes
the duty with Supermicro OEM raw commands through libfreeipmi. This repository
is a fork of the archived `chenxiaolong/ipmi-fan-control`, so nothing goes
upstream. It runs on a real server through the NixOS module in `nix/`.

A wrong duty value can overheat hardware, and nothing on a development machine
will show it. When a choice is unclear, choose the one that leaves the fans
faster.

Your closing summary names every file you changed and every behaviour change,
including ones nobody asked for, because reviewers read the summary before the
diff.

## Working

The Finishing work block applies when a person reads this session directly. An
agent working from a brief follows its role prompt instead: it stops the way
that prompt says and returns its packet with nothing around it. The same
holds for the closing summary above and for the completion rule under
Verification: an agent working from a brief reports in the format its role
prompt defines, still naming every behaviour change, and runs the Verification
commands only when the brief has it deliver a change. The confirmation list
below applies to every agent.

### Finishing work
Please avoid stopping while you still owe work the user asked for. Be persistent and do not stop unless needed.

While status notes are welcome, as is clarifying with the user on open decisions, carry on with work that does not depend on the user's answer without unnecessary stops.

If you notice yourself inviting the user to redirect you or offering to wait, instead proceed on the next part of the task. You may meet ordinary obstacles like errors, timeouts, locked files, empty results or failing tools. Diagnose them first, and when they are not real blockers, work through them with the access you have (wait and retry, fix the request, use another tool or source) rather than stopping unnecessarily.

If someone clearly decided something is a hard blocker, such as a file marked not to be touched, access that was intentionally withheld, or a safety guardrail, leave it alone, say plainly what you found, and look for another safe way to finish the task. Stopping before the task is complete is very rarely merited, as in when the task can't move forward without user input, or where the blocker is deliberate and should not be worked past. This does not override the need for confirmation on risky or destructive actions.

Before you start, say in a line what you're about to do; brief updates while you work help the user follow along. Close with a short recap that stands on its own (what you found, what you did, and what's next), so a reader who only sees the last message has the full picture.

### What needs confirmation

Ask first, naming the exact action, before:

- running the built binary, `ipmitool`, or any other command that can reach a
  BMC. On start the daemon switches the board to full fan mode and takes over
  the duty cycle, and `testing.md` holds raw commands aimed at a live machine;
- any command on a deployed host, and any `nixos-rebuild` or other activation;
- pushing, merging, or anything that lands on `master`. The server's
  configuration pins this repository as a flake input, so `master` is what
  gets deployed;
- editing another repository, including the one that holds the server's
  configuration;
- changing `flake.lock`, because it sets the toolchain and the freeipmi
  version of the deployed build;
- adding, removing or upgrading a dependency in `Cargo.toml`;
- adding a clippy `#[allow]`, deleting a test, or skipping a Verification
  command.

Routine and needs no confirmation: every command under Verification, local
commits on a branch other than `master`, and editing files inside the
worktree other than `flake.lock` and the dependency tables of `Cargo.toml`.

## Structure

```
/src
  main.rs       # CLI, zone loops, curve interpolation, aggregation, shutdown path
  config.rs     # TOML config types (serde) and validation in load_config
  source.rs     # Temperature sources: ipmi, file, smartctl, hdparm
  ipmi.rs       # Supermicro OEM commands: fan mode, duty cycle, sensor readings
  freeipmi.rs   # Safe wrappers over the libfreeipmi and libipmimonitoring FFI
  bindings.rs   # Includes the bindgen output; never edited by hand
  error.rs      # The one Error enum (thiserror) and the Result alias
build.rs        # Runs bindgen over wrapper.h at build time
wrapper.h       # The freeipmi headers that bindgen reads
config.sample.toml  # The documented config; the reference for every key
/nix
  module.nix    # NixOS module: options, service user, udev rule, systemd unit
  overlay.nix   # The Nix package, built from Cargo.lock
flake.nix       # Dev shell, package, module and overlay outputs
/dist           # Upstream packaging for RPM, Debian and Arch
/.github/workflows  # CI (clippy, build) and tagged releases
testing.md      # The owner's notes of raw ipmitool commands; not documentation
```

## Development

Every command runs inside the flake dev shell, which supplies the Rust
toolchain, a C linker, clang for bindgen, pkg-config and freeipmi. A bare
`cargo` outside it fails for lack of them.

```bash
nix develop                   # enter the shell
nix develop -c cargo build    # or run one command through it
```

The dev shell's toolchain comes from `flake.lock` and is the authority for
lint results. CI and a system toolchain run other clippy versions and can
report different lints.

No IPMI hardware exists on a development machine, so the daemon cannot be run
here. Behaviour is proved by tests of pure functions.

## Rules

- Keep the hand-written style of the surrounding code and do not run
  `cargo fmt`: the tree is not rustfmt output, and formatting it would bury
  the real change in the diff.
- Every fallible path returns a variant of `Error` in `src/error.rs`. Do not
  panic or call `process::exit` outside `main`: a return from `main_wrapper`,
  with `Ok` or with an `Error`, is what runs `IpmiSession::drop`, which sets
  every zone to 100% and restores the fan mode.
- That shutdown path is the safety net. A change must keep it running on every
  exit, including a failed zone loop and a signal.
- A duty cycle is a `u8` from 0 to 100. Validate it where it enters, in
  `load_config`, and keep the arithmetic in integers that cannot overflow.
- An existing config file must keep loading with the same result. A new key is
  optional and its default is today's behaviour. Config types keep
  `deny_unknown_fields`, so a misspelt key is an error and not a silent
  default.
- Validation lives in `load_config` and names the key it rejects, in the form
  `zones[0].steps[1].dcycle: reason`.
- A source that runs an external program keeps the spawn apart from the
  parsing of its output, as `parse_hdparm_output` does, so a test can cover
  the parsing without the binary.
- A change to the config format updates `config.sample.toml` and `README.md`
  in the same change, because the sample is the only reference users have. It
  also updates the `settings` example in `nix/module.nix` when it alters a key
  or a value that the example shows.
- A program the daemon spawns must be on the unit's PATH. Add it to
  `runtimeDeps` in `nix/module.nix` and to the dependency list in `README.md`.
- Raw IPMI bytes are named constants in `src/ipmi.rs`. A new one carries a
  comment that says where the value comes from, because no test can check it.
- Each `unsafe` block carries a comment that says why it is sound, as the
  existing ones in `src/freeipmi.rs` do.
- Tests live in a `#[cfg(test)] mod tests` block in the file they test. Write
  the failing test first and watch it fail. A change to untested logic first
  moves that logic into a pure function.
- Leave `/dist` alone unless a build dependency changes. It is upstream
  packaging that nothing here exercises.

## Verification

A task is complete only when the commands below have run in this session and
their output shows success. Report the commands and results in the closing
summary; "should pass" is not a result.

```bash
nix develop -c cargo build
nix develop -c cargo clippy -- -D warnings
nix develop -c cargo test
nix flake check          # when the change touches flake.nix or nix/
nix build .#default      # when the change touches Cargo.toml, Cargo.lock, build.rs, wrapper.h or nix/overlay.nix
```

Review every change to code or prompts before it is done and resolve every
Must Fix. What changed sets the depth, unless the brief an agent works from
names another depth:

| Change | Review |
|---|---|
| Only `README.md`, `testing.md` or comments in `config.sample.toml` | None |
| Only prompt files, none of them a critical path | `/pantheon:review --dimension prompt`: one reviewer |
| Under 200 source lines, no critical path | `/pantheon:review --dimension correctness`: one reviewer, enough for a diff one agent can hold |
| 200 to 1000 source lines, or a critical path at any size | `/pantheon:review`: the manifest selects the dimensions |
| Over 1000 source lines | Split into ranges of at most 1000 and review each; one review cannot cover more |

Take the line count from the source-lines figure, S, on the
`Files: N files, M changed lines, S source lines` line that
`pantheon-cli review prep` prints. If the installed CLI prints no S, use M.

A change that touches a prompt file together with code gets the review its
size calls for and then, unless that run's `Dimensions:` line already lists
`prompt`, a second run of `/pantheon:review --dimension prompt` on the same
range. Do not add the flag to the first run: `--dimension` replaces the
manifest's selection.

Prompt files are what an agent reads as instructions: `AGENTS.md`, its symlink
`CLAUDE.md`, `.claude/review/*.md` and `.claude/review/review.yaml`.

Critical paths, where a small mistake sends a wrong command to a BMC, leaves
the fans slow, or breaks the deployed unit:

- `src/ipmi.rs` and `src/freeipmi.rs`
- `build.rs` and `wrapper.h`
- the duty calculation and the shutdown path in `src/main.rs`
- `nix/**`
- `.claude/review/review.yaml`
