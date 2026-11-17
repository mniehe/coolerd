# Safety

This daemon controls cooling on real hardware and runs with raw-I/O
capabilities. Find changes that can leave fans slow while something is hot,
send a wrong command to a BMC, or widen what the unit may do. A finding here
names the physical or privilege consequence.

## Trust boundaries

- The config file is written by the machine's administrator and is trusted
  for intent, not for correctness: every value is validated in `load_config`.
- Output of spawned programs (`smartctl`, `hdparm` and later sources) and the
  contents of sensor files are untrusted text. They are parsed in
  `src/source.rs`.
- `src/freeipmi.rs` is the FFI boundary. `src/ipmi.rs` builds the raw
  Supermicro OEM request bytes.
- `nix/module.nix` sets the unit's user, groups, PATH, udev rule and ambient
  capabilities (`CAP_SYS_RAWIO`, `CAP_SYS_ADMIN`).

## Lenses, in priority order

1. Fail-safe: any exit path, including a failed zone loop, a panic and a
   signal, that does not reach `IpmiSession::drop`, which sets every zone to
   100% and restores the fan mode. A new `process::exit`, `abort`, or a
   leaked `Arc<IpmiSession>` that keeps the destructor from running.
2. Fail-slow: a failure handled by lowering or keeping the duty where the
   safe answer is a higher one; a source dropped from a zone without a
   stated fallback; a blocked read that stalls a zone loop with no bound.
3. Raw commands: a changed or new byte in `src/ipmi.rs` without a named
   constant and a stated source; a zone or duty written without a range
   check; a response parsed without a length check.
4. FFI: an `unsafe` block without a soundness comment; a pointer used after
   its owner is dropped; a `CString` freed twice or never; a changed
   allowlist in `build.rs` that drops a symbol still in use.
5. Spawned programs: a program name or argument built from config text in a
   way that changes which program runs; output parsed without bounds, so a
   malformed line panics or yields a plausible wrong temperature.
6. Privilege: a new capability, group, writable path or udev rule in
   `nix/module.nix` broader than the change needs; a secret from the config
   file reaching a log line (`Password` redacts its `Debug`).

## Check for what is absent

- Every new blocking call in a zone loop has a bound.
- Every new failure mode states what the fans do while it lasts.
- Every program the daemon spawns is on the unit PATH in `nix/module.nix`.
- A dependency change in `Cargo.toml` is reflected in `Cargo.lock`, which the
  Nix package builds from.

## Finding fields

Add to the standard format:

- Lens: Fail-safe / Fail-slow / Raw command / FFI / Spawn / Privilege
- Consequence: what the hardware or the host does when the scenario occurs
