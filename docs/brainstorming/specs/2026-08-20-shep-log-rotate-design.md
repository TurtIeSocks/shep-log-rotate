# shep-log-rotate — design

**Date:** 2026-08-20
**Status:** delegate-mode design, awaiting Rin's review
**Repo:** `github.com/TurtIeSocks/shep-log-rotate` (bare, README only)

## The ask

Rin, 2026-08-20: a log-rotation dog, written as a **fully external dog**, to
test shep's dogs API from outside the monorepo. Rotation is the deliverable;
being the first genuine third-party dog is the point.

## What the contract actually says

Established by reading `docs/dogs.md`, the protocol, and the daemon's
`Reopen` handler, rather than assumed.

- **A dog is an ordinary supervised process with a marker.** The kill ladder,
  backoff, restart budget and `Errored` semantics do not branch on it being a
  dog. A wildcard selector never touches one; naming it exactly always does.
- **It talks to the daemon as a client**, over `$SHEP_HOME/run/shep.sock`,
  handshaking exactly as `shep flock` does. Not the fd-3 shepherd channel:
  that is for sheep.
- **`$SHEP_HOME` is the only thing it inherits.** No `[dog.<name>]` value
  rides along in the environment. Config comes from
  `Request::DogConfig { name }`, returned as opaque text to parse however the
  dog wants.
- **`shep adopt` vets once**, at adoption: refuses a non-file, a path with no
  execute bit, or anything world-writable including its directory; warns on
  group-writable; then actually spawns and kills it, because the only honest
  way to know a kernel can exec a file is to ask that kernel. It records the
  canonicalised absolute path.
- **No sandbox.** An adopted dog runs at the shepherd's trust level. The docs
  are direct about this and the comparison they draw is fair: it is the same
  trust a Flockfile's `script` already has.

### The rotation-specific findings

**`shep reopen` exists for exactly this.** Its own help reads "Reopen log
files after an external rotator has renamed them (`create`-mode rotation)".
This dog is the rotator that sentence anticipates.

**`ListFlock` hands over the paths.** Every `ProcessInfo` carries `out_file`
and `err_file`, resolved, so the dog never has to guess a naming scheme or
read shep's config.

**`Flush` is destructive and must not appear in a rotation sequence.** Its
wire doc: "flush what is still pending, then TRUNCATE the recorded paths."
That is `shep flush`, the operator emptying logs on purpose. Reaching for it
before a rename, on the intuition that "flush" means "settle the buffers",
would delete the lines being rotated. **This design nearly made that mistake
and the doc is why it did not.**

**The daemon's own logs are out of reach.** `Request::Reopen` resolves
through `supervisor.reopen(selector)`, which walks sheep. `shepd.out.log` and
`shepd.err.log` have no reopen path, so renaming them would leave the daemon
writing into a file nobody can find. See "Deliberately not doing" below.

## 1. Shape

**Rust, depending on the published `shep-client` crate.** Not on the
workspace by path.

That choice is the test. `shep-client` is a crate Rin publishes, and the
question worth answering is whether somebody outside the monorepo can build a
dog with it. Depending on it by version from crates.io is the only way to
find out; a path dependency would quietly paper over anything missing from
the published surface.

Single binary, no subcommands. It runs, it rotates, it exits only when
stopped.

## 2. Configuration

`[dog.log-rotate]` in `shep.toml`, fetched at startup with
`Request::DogConfig`:

```toml
[dog.log-rotate]
# Rotate a log once it reaches this size.
max_size = "10M"
# Optionally also rotate on age, whatever the size.
max_age = "7d"
# Generations to keep. Older ones are deleted.
keep = 5
# gzip generations from .2 down. .1 stays plain so `tail` still works on it.
compress = true
# How often to look.
interval = "60s"
```

Every field has a default, so an empty `[dog.log-rotate]` is a working
configuration. Durations and sizes use shep's own strict spellings (`10M`,
`60s`), because a dog that accepts `10MB` while shep refuses it teaches the
wrong lesson about the ecosystem it lives in.

**Re-read on every tick, not cached at startup.** The daemon serves
`[dog.<name>]` per request rather than caching it at boot, and a dog that
caches undoes that on its own side. Changing `max_size` should not need a
`shep disable` / `shep enable`.

## 3. The loop

Poll, not subscribe. Rotation is inherently periodic, and the bus does not
publish "a log got big". `Subscribe` would add a socket to babysit for
nothing.

Each tick:

1. `DogConfig` for current settings.
2. `ListFlock` for every sheep and its `out_file` / `err_file`.
3. `stat` each path. A path that cannot be stat'd is skipped and counted, not
   an error: a sheep registered but never started has no log file yet, and
   that is normal rather than broken.
4. Decide: over `max_size`, or older than `max_age`.
5. For each file that qualifies, rotate it (§4).
6. One `Reopen` for the whole batch, naming the affected sheep.
7. Compress and prune (§5), after the reopen rather than before.

## 4. Rotating one file

Classic `create` mode, which is what `shep reopen` is built for:

```
web-0-out.log.4  ->  web-0-out.log.5     (oldest first, so nothing is overwritten)
web-0-out.log.1  ->  web-0-out.log.2
web-0-out.log    ->  web-0-out.log.1
                     then: Reopen
```

Numeric generations rather than timestamps: they sort obviously, need no
parsing to prune, and match what every operator already expects from
logrotate.

**The window between rename and reopen is real, and correct.** shep holds an
open descriptor, so lines written in that gap land in `.1` rather than
vanishing. They are in the rotated generation instead of the current one,
which is the honest outcome and worth documenting rather than hiding.

**If `Reopen` fails, stop rotating for this tick.** shep is still writing
into `.1` through its existing handle, so nothing is lost and the situation
self-corrects on the next successful reopen. Rotating further files while the
reopen path is broken multiplies a recoverable state into a confusing one.
Report it and back off.

## 5. Compression and pruning

Compress from `.2` down, leaving `.1` plain so the most recent rotation is
still greppable without a decompression step. Compression happens after the
reopen, so a slow gzip never widens the rename-to-reopen window.

**Prune only what this dog created.** Deletion is limited to paths matching
its own generation pattern for a log path `ListFlock` reported. It will not
delete a stray file in the log directory that merely looks old. A rotator
that deletes something it did not write is a much worse bug than one that
leaves files behind.

## 6. Deliberately not doing

- **The daemon's own logs.** `shepd.out.log` and `shepd.err.log` have no
  reopen path in the protocol, so rotating them would leave the daemon
  writing to a renamed file. Recorded as a shep-side gap rather than worked
  around here; a rotator that quietly breaks the daemon's own logging is
  worse than one that leaves them alone.
- **copytruncate mode.** Copy-then-truncate loses whatever is written between
  the two operations. shep supports the rename-and-reopen shape properly,
  which is the one without that hole.
- **Watching for its own death.** `docs/dogs.md` is explicit that nothing
  watches across dogs, by design. `shep barks` and the metrics dog's
  `shep_dog_up` are where a dead dog shows up.
- **Rotating on a signal.** `SIGHUP`-triggered rotation is a shape people
  expect, but the shepherd owns this process's signals and its kill ladder.
  Adding a signal handler here would be arguing with the supervisor.

## 7. Testing

The property that matters is **no log line is lost across a rotation**, and
it needs a real shep to prove.

- **The integration test**: boot a real shepherd with a sheep that prints a
  monotonically increasing counter as fast as it can, force several rotations
  by setting `max_size` very low, stop everything, then concatenate every
  generation in order and assert the counter has no gaps. A rotator that
  loses lines under load is worse than no rotator, and only a counter proves
  it did not.
- **The adoption path end to end**: `shep adopt`, `shep enable`, confirm it
  shows in `shep dogs`, `shep rehome`, confirm it is gone. That is the
  external-dog contract itself under test, which is the reason this project
  exists.
- **A refused `Reopen`** leaves shep writing into `.1` and the dog reporting
  rather than looping.
- **Pruning never deletes a file the dog did not create**, tested by putting
  a decoy in the log directory.

## 8. Assumptions

Judgement calls made on Rin's behalf, per delegate mode:

1. **Rust with `shep-client` from crates.io**, not a path dependency. The
   published surface being usable is half the thing being tested.
2. **Size-based rotation is the primary trigger**, with age optional. Size is
   what fills a disk.
3. **Numeric generations, not timestamps.** Simpler to prune, matches
   logrotate, no parsing.
4. **`.1` stays uncompressed.** The most recent rotation is the one someone
   greps.
5. **Poll rather than subscribe.** The bus has no event for this.
6. **Config re-read every tick**, mirroring the daemon's own choice not to
   cache `[dog.<name>]`.
7. **One `Reopen` per tick**, batched, rather than one per file.
8. **Defaults for every field**, so an empty section works.
9. **shep's own duration and size spellings**, strictness included.

## 9. What this exercise already found in shep

Worth carrying back regardless of whether this dog gets built:

1. **`Flush`'s name invites a destructive mistake.** "Flush the logs before
   rotating" is the natural instinct and it truncates them. The wire doc is
   accurate; the name is the trap. Worth a sentence in `docs/dogs.md` warning
   a dog author off it, since a rotator is the most likely thing to reach for
   it.
2. **The daemon's own logs cannot be rotated by anything.** No reopen path
   exists for them. On a long-running shepherd they grow without bound.
3. **`docs/dogs.md` documents the wire but not a worked external dog.** This
   project can become the example it points at.
