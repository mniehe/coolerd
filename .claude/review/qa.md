# Tests

Judge the tests by what they would catch. Nothing here can run against IPMI
hardware, so a pure-function test is the only proof a change gets. The
question for every changed behaviour is: which test fails if this is wrong?
The failure scenario of a test finding is the mutation to the production code
that the suite would not catch, and the test command that stays green. Never
claim the mutation exists in production.

## Conventions

- Tests live in a `#[cfg(test)] mod tests` block in the file they test and
  run with `nix develop -c cargo test`.
- Logic is tested as a pure function: the curve and aggregation on plain
  numbers, parsers on captured output, config validation on TOML strings.
- A source that spawns a program is tested with a fake executable script,
  never the real binary.
- `config.sample.toml` and every config in `docs/examples/` must load in a
  test through `load_config`, because they are the documentation. So must
  every block fenced as `toml` in `docs/configuration.md` and
  `docs/failure.md`.
- A new module option or recipe is covered by the `module-eval` check in
  `flake.nix`, which requires a heading in `docs/nixos.md` for each option
  and evaluates each recipe in `docs/examples/`.

## Lenses, in priority order

1. Missing coverage: duty arithmetic, config validation or output parsing in
   the change with no test. A wrong duty first, a rejected valid config
   second, error wording last.
2. Untestable shape: new logic written inside `zone_loop`,
   `update_duty_cycle` or a function that also spawns or locks IPMI, where it
   cannot be tested, instead of in a pure function.
3. Assertion strength: a test that checks only `is_ok()` or `is_err()` where
   the value or the error variant is the point; a duty test with one input
   that a constant function would pass.
4. Discrimination: a test that would pass against the tree before the
   change. If you cannot tell by reading, say so in the Verification hint.
5. Flakiness: real sleeps and clocks, a shared temp path, a test that needs
   a program outside the dev shell, a timeout test with a tight margin.

## Check for what is absent

- Every validation rule the change adds has one config that breaks it.
- Every boundary of a curve or window has a test: below the first step, on a
  step, above the last, and the first poll.
- Every failure mode of a source has a test that names the resulting duty.
- An existing config form the change must keep valid has a test that loads
  it.

## Finding fields

Add to the standard format:

- Production code: path:start-end of the code that should be tested
- Test code: path:start-end of the weak test, or ABSENT
- Would catch: the defect that slips through without this test
