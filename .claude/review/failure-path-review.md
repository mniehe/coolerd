# Failure paths

Find failures the change lets reach a caller looking like something else.
Trace each error from its origin through propagation, wrapping, fallback,
retry, cleanup and rollback to the caller-visible outcome: a return value,
a status or exit code, a printed or logged line, a persisted record, or
nothing. Establish the intended semantics first, from the brief, a spec or
doc comment, a test, or the caller's handling: a value, absence, partial
success, cancellation and failure are different outcomes, and an empty
result the semantics call absence is not a swallowed error. Where none
settles them, absence and partial success are not findings; duplication,
skipped cleanup and unobserved work still are. A path whose semantics or
trace could not be established is not listed under CLEAN; it is a nit
naming the missing evidence. The one-hop read bounds where origins are
looked for, not the reads needed to establish a candidate's outcome or
termination owner.

## Project shape

- `src/error.rs` holds the one `Error` enum (thiserror). Variants keep their
  cause in a `source` field.
- `src/source.rs`: each source returns `Result<u8>`. A spawned program's exit
  status and its output are both checked.
- `src/main.rs`: `get_temp` retries the zone's sources, `zone_loop` returns
  the first error, `run` aborts the other loops through a `JoinSet`, and
  `main` exits 1 after `MainApp` drops. `IpmiSession::drop` is the cleanup:
  it sets every zone to 100% and restores the fan mode, logging and not
  returning its own errors.
- A failure that ends in a lower or unchanged fan duty with no log line is
  the disguised success that matters most here.

| Language | Swallows | Legitimate when the trace confirms |
|---|---|---|
| Rust | `.unwrap_or_default()` or `.unwrap_or(x)` on a `Result` whose error matters | the same on an optional setting whose default is specified |
| Rust | `let _ = f();` on a `Result` that matters | `let _ = tx.send(v)` where a closed receiver means shutdown |
| Rust | `.ok()` then a default, or `if let Ok(x) = r` with no else; `env::var(k).ok()` collapses `NotUnicode` into unset | `match env::var(k)` with `Err(VarError::NotPresent)` as absence and `NotUnicode` handled |
| Rust | `.map_err(\|_\| Error::Generic)` dropping the source | `#[source]`, `#[from]` or `.context()` keeping the cause, or a documented opaque boundary |
| Rust | a `BufWriter` or a written `File` left to Drop, which ignores the flush or close error | `flush()?` or `sync_all()?` before scope end; Drop releasing a lock guard or a read handle |
| Rust | a `tokio::spawn` `JoinHandle` dropped, detaching the task so its result and panic are lost | the handle awaited, held in a `JoinSet`, or the task reporting its own failure |

## Lenses, in priority order

1. Disguised success: an error collapsed to nil, null, an empty
   collection, a default value or a success status; an error logged and
   execution continued as if the operation had succeeded.
2. Duplication: a retry after an ambiguous effect (a write that may have
   landed before the timeout or the lost response) without idempotence or
   effective deduplication. A proven no-effect failure, such as connection
   refused before the request was sent, may be retried without them.
3. Skipped cleanup: a release, rollback or compensating action present on
   some exit paths and absent on one; a cleanup error that replaces the
   original error.
4. Lost cause: an error rewrapped without its cause, or a catch that
   raises a different, generic error, so the caller cannot tell which
   failure occurred. A documented opaque boundary that maps to an approved
   public error and keeps the cause in private diagnostics is the
   exception; `%v` formats the error's text and is not redaction.
5. Unobserved work: a promise not awaited, a task, goroutine or future
   whose result or failure no one reads, a rejection with no handler.
6. Missing termination: a wait on the network or another process whose
   required termination condition has no effective owner. Establish the
   condition and the owner (a deadline, a cancellation, EOF or lifecycle
   shutdown) before reporting; a context or signal parameter proves
   neither a bound nor that the callee observes it. A service accept loop,
   a desktop event wait or a CLI reading stdin legitimately waits for
   lifecycle cancellation or EOF.

## Check for what is absent

- Every fallible call the change adds reaches its caller as a failure, or
  the semantics name the outcome it becomes.
- Every catch, recover or fallback that returns a value has a stated
  reason that value is correct for that failure.
- Every retry after an ambiguous effect is over an idempotent or
  deduplicated operation, and every retry states how repeated failure
  terminates.
- Every resource acquired and transaction begun on the changed path is
  released or rolled back on its error exits.
- Every promise, task or goroutine the change starts is awaited, joined or
  has its failure handled.
- Every call the change adds that waits on the network or another process
  has a termination owner it observes.
- Every rewrap keeps the cause, unless it sits at a documented opaque
  boundary that keeps the cause in private diagnostics.

## Finding fields

Add to the standard format:

- OBSERVED: the caller-visible result quoted from the line that states it
  (a status string, log line or test assertion), or "traced, not observed"
  with the line the trace ends at when no line states it
