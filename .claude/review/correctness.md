# Correctness

Find changes that are wrong: logic errors, mishandled edge cases, and config
or arithmetic mistakes that end in a wrong fan duty. Trace a value from the
config file or the sensor to the byte written over IPMI rather than reviewing
hunks alone.

## Project shape

- `src/config.rs`: serde types with `deny_unknown_fields` and all validation
  in `load_config`. Read a new key together with its validation, its default
  and `config.sample.toml`.
- `src/source.rs`: one reader per source type, the dispatch in
  `ZoneReaders::read`, and the retries in `read_zone_sources`. Readings are
  `u8` degrees Celsius.
- `src/command.rs`: `CommandRunner` runs each external program under a time
  limit and an output limit. Each source owns one runner.
- `src/curve.rs`: the pure duty logic. `duty_for_temp` is the curve,
  `aggregate` joins temperatures (rounding down) and duties (rounding up),
  and `ZoneState::next_duty` applies smoothing, hysteresis and the failure
  floor. Read a change there together with the config keys it serves.
- `src/main.rs`: `zone_loop` calls `update_duty_cycle`, which reads the
  sources, calls `next_duty` and writes the duty. Read a duty change together with `IpmiSession::drop`.
- `src/ipmi.rs`: the only caller of raw commands.

## Lenses, in priority order

1. Duty arithmetic: integer overflow or underflow on `u8` subtraction, a
   division by a zero temperature span, a result above 100, interpolation
   that falls instead of rises, a boundary temperature that picks the wrong
   step, an empty list that yields a low duty instead of 100.
2. Config compatibility: an existing config that no longer loads or loads
   to a different duty; a new key that is not optional; a default that is
   not the old behaviour; a type that lost `deny_unknown_fields`; a
   validation that names the wrong key.
3. Closed sets: a `match` over `Source`, `Aggregation` or another config
   enum that the change did not extend at every site.
4. State across polls: a value kept between polls that is not reset when a
   source fails or recovers, or that is shared between zones.
5. Simplicity: indirection that hides behaviour. A nit unless it conceals a
   defect from the lenses above.

## Check for what is absent

- Every new config value is validated before the zone loops start.
- Every documented key in `config.sample.toml` matches the name and type the
  code reads, and the `settings` example in `nix/module.nix` still loads.
- Every reading that can be absent is handled before it reaches the curve.
- A changed constant or format is changed everywhere it is duplicated,
  including `README.md`.

## Finding fields

Add to the standard format:

- Lens: Duty / Config / Closed set / State / Simplicity
- Duty effect: the duty the fans get and the duty they should get, with the
  input that shows it
