# shep-log-rotate

A log-rotation dog for [shep](https://github.com/TurtIeSocks/shep).

shep writes each sheep's stdout and stderr to a file and appends forever.
This dog watches those files, renames the ones that have grown too big or
got too old, asks the shepherd to reopen them, and then gzips and deletes
older generations so the log directory stays bounded.

It is an external dog. Nothing here is built into shep: it is an ordinary
binary you adopt, and it talks to the daemon over the same socket the CLI
uses.

## Install

```sh
cargo install --git https://github.com/TurtIeSocks/shep-log-rotate
shep adopt log-rotate ~/.cargo/bin/shep-log-rotate
```

`shep adopt` records the binary in `shep.toml` and starts it. From then on
the shepherd supervises it like anything else in the flock, and `shep dogs`
lists it.

The name you adopt it under is the name it looks itself up by, and it is
also the config key. Adopt it as `log-rotate` and it reads
`[dog.log-rotate]`; adopt it as `rotator` and it reads `[dog.rotator]`. Any
name works, as long as the section matches.

Be careful with that second command, though. `shep adopt` checks that a
binary is runnable by actually running it, for about fifty milliseconds,
with your shell's environment. If `$SHEP_HOME` is not set in that shell,
the copy it starts connects to your default shepherd at `~/.shep` and may
get a rotation pass in there before it is killed. That is usually what you
want. If it is not, set `$SHEP_HOME` for the `adopt` command as well as
passing `--home`.

## Configuration

Everything lives in one table in `shep.toml`. Ask the binary for a starting
point:

```sh
shep-log-rotate --print-config >> ~/.shep/shep.toml
```

Every line it prints is commented, so appending it changes nothing until you
uncomment something.

```toml
[dog.log-rotate]
max_size = "10M"    # rotate once a log reaches this size
max_age  = "168h"   # optionally also rotate on age, whatever the size
keep     = 5        # rotated generations to keep, at least 1
naming   = "dated"  # "dated" or "numeric", see below
compress = true     # gzip all but the newest generation
interval = "60s"    # how often to look
```

| Option | Default | Notes |
| --- | --- | --- |
| `max_size` | `"10M"` | shep's size grammar: `K`, `M`, `G`, uppercase, binary. `10M` is 10 MiB. `10MB` is refused. |
| `max_age` | unset | Age alone is enough to rotate a log that never reached `max_size`. Unset means size only. |
| `keep` | `5` | Counts rotated generations, never the live file. `keep = 0` is refused, since it would delete each rotation as it was made. |
| `naming` | `"dated"` | See below. |
| `compress` | `true` | The newest generation is left plain so it still greps without a decompression step. |
| `interval` | `"60s"` | How long between passes. |

`max_age` and `interval` use shep's duration grammar, which has hours,
minutes and seconds in lowercase and no day unit at all. A week is `"168h"`,
not `"7d"`. A bare number is milliseconds, so `max_age = "7"` means seven
milliseconds and not seven of anything else.

The dog re-reads this table on every pass, so editing `shep.toml` takes
effect on the next interval without a restart.

## The two naming schemes

`dated` names each generation after the moment it was rotated:

```
web-0-out.log                              the live log
web-0-out.2026-08-20T15-04-05.log          rotated at that time
web-0-out.2026-08-20T15-04-05.1.log        a second rotation in the same second
```

The timestamp is UTC, always, whatever the machine's timezone is set to.
Sorting the directory by name puts the generations in order, and a
generation is never renamed once it has been written. The names still end
in `.log`, so a glob or a log shipper watching `*.log` keeps working.

`numeric` is the logrotate convention:

```
web-0-out.log       the live log
web-0-out.log.1     the newest generation
web-0-out.log.2     older
web-0-out.log.3     older still
```

Every rotation shifts the whole series down by one before renaming the live
file into `.1`. The names are short and predictable, which is what makes
them easy to write a script against. Note that `.1` is the newest here.
macOS `newsyslog` numbers the same idea from `.0`, so a script carried over
from a Mac is off by one.

The cost of `numeric` is renames. `dated` does one per rotation; `numeric`
does one per generation it is keeping, every time. With `keep = 5` that is
six renames instead of one, and each of them is a moment where a file
briefly does not have its usual name.

`numeric` also has a name collision `dated` cannot have. A log with no
extension and a log whose extension is a number produce the same names: a
sheep writing to `/var/log/web` rotates into `/var/log/web.1`, and that may
already be a different sheep's live log. Both readings are correct, and no
amount of care in the matcher can separate them, so the dog asks the
shepherd instead. If a name it would rotate into is a live log belonging to
anything in the flock, it leaves that log alone entirely and says so:

```
rotated 0, compressed 0, deleted 0, left 1 alone whose rotated name is a live log
```

That is not something the dog can fix. Give the two logs different names, or
use `dated`.

### Switching between them

Switching `naming` does not migrate anything. The old scheme's files stop
matching, which means they also stop being counted against `keep` and stop
being deleted. They sit there until you move them or remove them yourself.

That is the safe direction to fail in. The alternative is a rotator that
deletes files it does not recognise, and a rotator that deletes files it
does not recognise will eventually delete something that was not its.

## What it does not rotate

`shepd.out.log` and `shepd.err.log` are left alone, and that is not an
oversight. The daemon is not a member of its own flock, so the dog never
sees those paths, and there would be nothing safe to do with them if it
did: shep has no way to tell the daemon to reopen its own log files.
Renaming one would leave the daemon writing into a file with no name, and
its output would stop appearing. On a shepherd that runs for months those
two files grow without limit. Rotating them is a job for whatever manages
the shepherd, or for a `shep kill` and a restart.

## What it does rotate, including itself

Dogs are ordinary supervised processes with a marker on them, so `shep dogs`
and the flock listing both report them, and this dog rotates their logs
along with everybody else's. That includes its own. Seeing
`log-rotate-0-out.log.1` next to your services' generations is expected: a
log directory that is bounded except for four files is the surprising
behaviour, not this.

It does mean the dog has to be quiet, and it is. A pass that rotated nothing
prints nothing at all. Only a pass that renamed something, refused to, or
was refused a reopen prints its one line. If this dog logged a line per file
per interval, its own log would be the busiest file in the directory it
exists to keep small.

## The window between rename and reopen

Rotating a file that something has open is not atomic and cannot be made
atomic. The dog renames `web-0-out.log` to its new name, then asks the
shepherd to reopen it. In between, shep is still holding a descriptor that
now points at the renamed file, and anything the sheep writes goes there.

Those lines are not lost. They land at the end of the previous generation
instead of at the start of the new one, which is a slightly wrong boundary
rather than a hole. shep reopens with create-and-append, so the reopen never
truncates anything either.

This is the honest outcome rather than the ideal one, and it matters if you
are counting on a generation boundary meaning something exact. The
integration tests drive a real shepherd, with a sheep printing a counter as
fast as it can through about twenty-five rotations per run. They then
concatenate every generation and look for holes in the counter. There are
none, in either scheme.

## Building from source

```sh
git clone https://github.com/TurtIeSocks/shep-log-rotate
cd shep-log-rotate
cargo test
```

The real-shepherd tests are behind a feature, because they need a `shep`
binary a fresh clone does not have:

```sh
SHEP_BIN=/path/to/shep cargo test --features integration
```

Cargo skips a target whose required features are off without saying so, so a
plain `cargo test` runs a test called
`heads_up_the_real_shepherd_tier_is_not_running` whose whole job is to put
that name in the output.

## A note on the dependency

`Cargo.toml` depends on `shep-client` by git URL rather than by version. No
shep crate is published on crates.io yet. When one is, this becomes a
version and the git line goes away.

## License

MIT OR Apache-2.0, at your option.
