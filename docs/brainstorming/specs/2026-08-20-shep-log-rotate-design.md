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
# "dated" (default) or "numeric". See §4.
naming = "dated"
# gzip rotated generations, newest one left plain so it stays greppable.
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

Both schemes are `create`-mode, which is what `shep reopen` is built for:
rename the current file, then ask shep to reopen the original path. They
differ only in what the rotated file is called and therefore in how pruning
works.

### `naming = "dated"` (default)

```
web-0-out.log  ->  web-0-out.2026-08-20T15-04-05.log     then: Reopen
```

The timestamp carries seconds because rotation is size-triggered: a chatty
sheep can rotate several times a minute, and day granularity would collide
immediately. A same-second collision gets a counter appended
(`...T15-04-05.1.log`), which in practice means a sheep filling `max_size`
twice inside one second.

**The extension stays last, deliberately.** `web-0-out.2026-08-20T15-04-05.log`
still matches `*.log`, still opens with log syntax highlighting, and still
gets found by every glob an operator already has. The numeric scheme quietly
breaks all three.

It is also self-describing, which is the argument that actually matters
during an incident: `web-0-out.log.3` cannot tell you whether it covers
Tuesday afternoon, and a dated name can.

Default because shep's users are largely arriving from pm2, where
`pm2-logrotate` is date-stamped, and because of the `*.log` point above.

### `naming = "numeric"`

```
web-0-out.log.4  ->  web-0-out.log.5     (oldest first, so nothing is overwritten)
web-0-out.log.1  ->  web-0-out.log.2
web-0-out.log    ->  web-0-out.log.1
                     then: Reopen
```

The Unix convention, and on this machine already: macOS `newsyslog` writes
`/var/log/system.log.0.gz`, Linux `logrotate` writes `syslog.1`. **The two
disagree about whether the newest is `.0` or `.1`**, which is a fair warning
against calling this settled. shep follows logrotate: newest is `.1`.

Costs one rename per retained generation on every rotation, against one for
dated. That is irrelevant at `keep = 5` and worth knowing at `keep = 200`.

### The window between rename and reopen

Identical in both schemes, real, and correct. shep holds an open descriptor,
so lines written in that gap land in the rotated file rather than vanishing.
They are in the previous generation instead of the current one, which is the
honest outcome and worth documenting rather than hiding.

**If `Reopen` fails, stop rotating for this tick.** shep is still writing into
the renamed file through its existing handle, so nothing is lost and the
situation self-corrects on the next successful reopen. Rotating further files
while the reopen path is broken multiplies a recoverable state into a
confusing one. Report it and back off.

## 5. Compression and pruning

Compress every rotated generation except the newest, which stays plain so the
most recent rotation is greppable without a decompression step. Compression
happens after the reopen, so a slow gzip never widens the rename-to-reopen
window.

Pruning is where the two schemes genuinely differ:

- **dated** sorts the matching files by their embedded timestamp and deletes
  past `keep`. No renaming, ever.
- **numeric** deletes anything above `keep` after the shift, since the shift
  itself has already ordered them.

**Prune only what this dog created**, in both schemes. Deletion is limited to
files whose names match the dog's own pattern for a log path `ListFlock`
reported: for dated, the full timestamp shape, not merely "has a date in it".
A rotator that deletes something it did not write is a far worse bug than one
that leaves files behind.

**Switching `naming` does not migrate anything.** Files written under the old
scheme stop being pruned, because they no longer match the pattern, and are
left for the operator rather than guessed at. Worth saying in the README: it
is the one configuration change that leaves litter, and silently deleting
files that a previous configuration created is not a trade this dog should
make on its own.

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
  a decoy in the log directory under each scheme, including a plausible
  near-miss: a file with a date in the name that does not match the dog's
  exact timestamp shape.
- **Both schemes** run the full no-lost-lines test, not just the default. The
  rename counts differ, so the rename-to-reopen window differs, and the
  scheme with more renames is the one more likely to expose a race.

## 8. Assumptions

Judgement calls made on Rin's behalf, per delegate mode:

1. **Rust with `shep-client` from crates.io**, not a path dependency. The
   published surface being usable is half the thing being tested.
2. **Size-based rotation is the primary trigger**, with age optional. Size is
   what fills a disk.
3. **Both naming schemes, defaulting to dated.** Rin's call, 2026-08-20,
   after pointing out that `*.log.{num}` is not what she has seen. Numeric is
   genuinely the Unix convention and is on her own machine
   (`/var/log/system.log.0.gz`), but it is not the convention the Node and
   pm2 world uses, and it breaks `*.log` globbing and editor highlighting.
4. **Dated timestamps carry seconds, with a counter for collisions.**
   Rotation is size-triggered, so day granularity would collide on a chatty
   sheep within the first minute.
5. **The newest rotated generation stays uncompressed**, in both schemes. It
   is the one someone greps.
6. **Switching `naming` does not migrate existing files.** They stop being
   pruned and are left alone. Deleting files a previous configuration created
   is not this dog's call to make.
7. **Poll rather than subscribe.** The bus has no event for this.
8. **Config re-read every tick**, mirroring the daemon's own choice not to
   cache `[dog.<name>]`.
9. **One `Reopen` per tick**, batched, rather than one per file.
10. **Defaults for every field**, so an empty section works.
11. **shep's own duration and size spellings**, strictness included.

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
