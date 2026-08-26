//! The tier that drives a REAL shepherd.
//!
//! Everything below `main` has unit tests, and every one of them can pass
//! while this dog loses log lines under load. The rename-to-reopen window is
//! the reason: between the moment a live log is renamed and the moment shep
//! is told to reopen it, shep is still writing through a descriptor that now
//! points at the renamed file. Nothing built out of a fake daemon can say
//! whether those lines survive, because nothing built out of a fake daemon
//! is holding a real descriptor. That is what this file is for.
//!
//! Gated behind the `integration` feature, and needing `$SHEP_BIN` pointed at
//! a built `shep`:
//!
//! ```text
//! cargo build --release --manifest-path ../pm2-rs/Cargo.toml -p shep
//! SHEP_BIN=../pm2-rs/target/release/shep cargo test --features integration
//! ```
//!
//! # $SHEP_HOME is a temporary directory in every test here, and that is load-bearing
//!
//! A live shepherd runs at `~/.shep` supervising real services. Every test
//! builds its own [`Shepherd`], which owns a `tempfile::tempdir` and kills
//! the daemon it booted when it drops.
//!
//! `--home` alone is NOT enough to keep a test off the real one. `shep
//! adopt` vets a candidate binary by spawning it, with this process's
//! environment and stdio inherited, and killing it about 50 milliseconds
//! later (`shep-cli/src/commands/dogs.rs`, `vet_binary`). An adopted dog
//! reads `$SHEP_HOME` from its environment, so a vetted `shep-log-rotate`
//! whose environment does not carry one connects to `~/.shep` and gets a
//! tick in against the operator's real flock before it is killed. Every
//! command run from here therefore sets `SHEP_HOME` in the child's
//! environment as well as passing `--home`.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

/// This crate's own binary, as cargo built it for this test run.
const DOG_BIN: &str = env!("CARGO_BIN_EXE_shep-log-rotate");

/// How long any "wait until the world catches up" poll gets before it gives
/// up and fails the test. Generous: these tests boot a daemon and race a
/// shell loop, and a contended machine is slow rather than broken.
const PATIENCE: Duration = Duration::from_secs(60);

/// The `shep` binary under test.
///
/// # Panics
/// If `$SHEP_BIN` is unset or does not point at a file. Loudly, rather than
/// skipping: a tier that quietly does nothing is the failure mode this whole
/// file exists to avoid.
fn shep_bin() -> PathBuf {
    let raw = std::env::var("SHEP_BIN").expect(
        "the integration tier needs $SHEP_BIN pointing at a built shep binary, for example \
         SHEP_BIN=../pm2-rs/target/release/shep",
    );
    let path = PathBuf::from(raw);
    assert!(
        path.is_file(),
        "$SHEP_BIN does not name a file: {}",
        path.display()
    );
    path
}

/// One shepherd in its own temporary `$SHEP_HOME`, killed on drop.
struct Shepherd {
    home: tempfile::TempDir,
    shep: PathBuf,
}

impl Shepherd {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("a temporary $SHEP_HOME");
        // A unix socket path is bounded by the kernel: 104 bytes on macOS,
        // 108 on Linux. `$TMPDIR` is long on macOS, so this is close enough
        // to the limit to be worth saying out loud rather than discovering
        // as "the daemon process exited before it started answering".
        let socket = home.path().join("run/shep.sock");
        assert!(
            socket.as_os_str().len() < 100,
            "$TMPDIR is too deep for a unix socket here: {} is {} bytes and the kernel allows \
             about 104. Run with a shorter TMPDIR.",
            socket.display(),
            socket.as_os_str().len()
        );
        Self {
            home,
            shep: shep_bin(),
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn logs(&self) -> PathBuf {
        self.home().join("logs")
    }

    /// Run one `shep` command against this home and hand back its output.
    ///
    /// `SHEP_HOME` goes in the environment as well as in `--home`. See this
    /// module's docs: `shep adopt` spawns the binary it is vetting with this
    /// environment, and a missing `SHEP_HOME` there points that spawn at the
    /// operator's real shepherd.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.shep)
            .args(args)
            .arg("--home")
            .arg(self.home())
            .env("SHEP_HOME", self.home())
            .output()
            .expect("shep ran")
    }

    /// Run one `shep` command and require it to succeed.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "shep {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Write this home's `shep.toml`.
    fn write_config(&self, body: &str) {
        fs::write(self.home().join("shep.toml"), body).expect("shep.toml");
    }

    /// Start this dog as a plain child process, the way an operator running
    /// it by hand would, with its output captured for the test to read.
    ///
    /// Adoption is a separate question with its own tests. A dog started
    /// this way is not in the flock, so it announces that it cannot identify
    /// itself and falls back to `log-rotate`, which is the section these
    /// tests write.
    fn spawn_dog(&self) -> DogProcess {
        let out = fs::File::create(self.home().join("dog.out")).expect("dog.out");
        let err = fs::File::create(self.home().join("dog.err")).expect("dog.err");
        let child = Command::new(DOG_BIN)
            .env("SHEP_HOME", self.home())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("the dog started");
        DogProcess(child)
    }

    /// What the dog printed on stdout, one summary line per busy tick.
    fn dog_stdout(&self) -> String {
        fs::read_to_string(self.home().join("dog.out")).unwrap_or_default()
    }

    /// What the dog printed on stderr.
    fn dog_stderr(&self) -> String {
        fs::read_to_string(self.home().join("dog.err")).unwrap_or_default()
    }
}

impl Drop for Shepherd {
    fn drop(&mut self) {
        // Before the tempdir goes, so the daemon is not still holding a home
        // that no longer exists. Failures are ignored: a test that already
        // failed must report its own reason, not this one.
        let _ = self.run(&["kill", "--style", "bare"]);
    }
}

/// A dog running as a plain child, killed on drop.
struct DogProcess(Child);

impl DogProcess {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for DogProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Write an executable `/bin/sh` script and hand back its path.
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = fs::File::create(&path).expect("script");
    file.write_all(body.as_bytes()).expect("script body");
    drop(file);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// A sheep that prints `0..lines` as fast as it can, then touches `sentinel`
/// and waits to be stopped.
///
/// The sentinel is how a test knows every line has been handed to shep
/// without guessing at a duration. Waiting on the log file's size instead
/// would race the rotation this test is measuring.
fn counter_script(dir: &Path, lines: u32, sentinel: &Path) -> PathBuf {
    write_script(
        dir,
        "counter.sh",
        &format!(
            "#!/bin/sh\n\
             i=0\n\
             while [ \"$i\" -lt {lines} ]; do echo \"$i\"; i=$((i+1)); done\n\
             : > {}\n\
             sleep 300\n",
            sentinel.display()
        ),
    )
}

/// Poll `ready` until it answers true, or fail with `what`.
fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Every rotated generation of `base` in `dir`, oldest first, followed by
/// the live file.
///
/// Deliberately a second implementation of the naming grammar rather than a
/// call into the crate's own: this crate is a binary with no library, so
/// nothing here can reach `naming::match_generation` anyway, and a test that
/// ordered files with the same code that named them could not catch an
/// ordering bug in it.
fn generations_oldest_first(dir: &Path, base: &str, numeric: bool) -> Vec<PathBuf> {
    let live = format!("{base}.log");
    let mut dated: Vec<(String, u32, PathBuf)> = Vec::new();
    let mut numbered: Vec<(u32, PathBuf)> = Vec::new();

    for entry in fs::read_dir(dir).expect("log dir") {
        let path = entry.expect("entry").path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == live {
            continue;
        }
        if numeric {
            // `web-0-out.log.3`
            let Some(rest) = name.strip_prefix(&format!("{live}.")) else {
                continue;
            };
            if let Ok(n) = rest.parse::<u32>() {
                numbered.push((n, path));
            }
        } else {
            // `web-0-out.2026-08-20T15-04-05.log`, or with a same-second
            // counter, `web-0-out.2026-08-20T15-04-05.2.log`.
            let Some(rest) = name.strip_prefix(&format!("{base}.")) else {
                continue;
            };
            let Some(rest) = rest.strip_suffix(".log") else {
                continue;
            };
            let (stamp, counter) = match rest.split_once('.') {
                Some((stamp, counter)) => (stamp, counter.parse::<u32>().unwrap_or(0)),
                None => (rest, 0),
            };
            dated.push((stamp.to_owned(), counter, path));
        }
    }

    let mut ordered: Vec<PathBuf> = if numeric {
        // `.1` is the newest, so descending is oldest first.
        numbered.sort_by_key(|(n, _)| core::cmp::Reverse(*n));
        numbered.into_iter().map(|(_, path)| path).collect()
    } else {
        // Ascending stamp, then ascending same-second counter.
        dated.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        dated.into_iter().map(|(_, _, path)| path).collect()
    };
    ordered.push(dir.join(live));
    ordered
}

/// Concatenate `files` in order and read the whole thing back as numbers.
///
/// Bytes rather than lines, and concatenated before splitting: a line can be
/// split across a rotation boundary if shep's write lands either side of the
/// rename, and reading the files separately would turn one whole line into
/// two broken ones and report a loss that did not happen.
fn concatenated_counter(files: &[PathBuf]) -> Vec<u64> {
    let mut bytes = Vec::new();
    for path in files {
        bytes.extend_from_slice(&fs::read(path).expect("a generation"));
    }
    String::from_utf8(bytes)
        .expect("the counter is ascii")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.parse::<u64>().expect("a counter line"))
        .collect()
}

/// The heart of this file: rotate a busy log many times and prove the
/// sequence written into it comes back whole.
///
/// Run for both naming schemes because the rename counts differ. `dated`
/// renames once per rotation; `numeric` shifts every surviving generation
/// down by one and then renames the live file, so a directory holding forty
/// generations does forty-one renames per rotation. More renames is a wider
/// stretch of wall clock between the first rename and the reopen that ends
/// it, so `numeric` is the scheme likelier to expose a race, and testing
/// only the default would test the easier half.
fn no_line_is_lost(naming: &str) {
    const LINES: u32 = 120_000;

    let shepherd = Shepherd::new();
    let sentinel = shepherd.home().join("finished");
    let script = counter_script(shepherd.home(), LINES, &sentinel);
    shepherd.write_config(&format!(
        "[dog.log-rotate]\n\
         max_size = \"16K\"\n\
         keep = 5000\n\
         compress = false\n\
         interval = \"50\"\n\
         naming = \"{naming}\"\n"
    ));

    shepherd.ok(&[
        "start",
        script.to_str().expect("script path"),
        "--name",
        "counter",
        "--style",
        "bare",
    ]);
    let mut dog = shepherd.spawn_dog();

    wait_until("the sheep to finish writing", || sentinel.exists());
    // The dog first, so nothing renames anything while the flock is being
    // stopped, then the sheep, whose stop drains shep's side of the pipe.
    dog.stop();
    shepherd.ok(&["stop", "counter", "--style", "bare"]);

    // A dog nobody adopted is not in the flock, so it cannot find its own
    // name, and the one thing it must never do about that is fall back
    // quietly: an operator who mistyped the name in `shep adopt` would get
    // this same silence with every setting discarded.
    assert!(
        shepherd
            .dog_stderr()
            .contains("cannot tell what it was adopted as"),
        "{naming}: an unadopted dog must say so: {:?}",
        shepherd.dog_stderr()
    );

    let files = generations_oldest_first(&shepherd.logs(), "counter-0-out", naming == "numeric");
    assert!(
        files.len() >= 5,
        "{naming}: only {} file(s), so this run never rotated and proves nothing. \
         The dog said: {:?}",
        files.len(),
        shepherd.dog_stdout()
    );

    let counter = concatenated_counter(&files);
    assert_eq!(counter.first(), Some(&0), "{naming}: the log starts at 0");
    // A short tail is not a loss across a rotation, it is shep never having
    // been handed those lines: the sheep is killed while its last writes are
    // still in flight. A HOLE is the thing this test exists to catch.
    assert!(
        counter.len() >= u64::from(LINES) as usize * 9 / 10,
        "{naming}: only {} of {LINES} lines came back, which is too few to call this a \
         rotation test",
        counter.len()
    );
    let gaps: Vec<(u64, u64)> = counter
        .windows(2)
        .filter(|pair| pair[1] != pair[0] + 1)
        .map(|pair| (pair[0], pair[1]))
        .collect();
    assert!(
        gaps.is_empty(),
        "{naming}: {} line(s) lost across {} rotation(s). First gaps: {:?}",
        gaps.iter().map(|(a, b)| b - a - 1).sum::<u64>(),
        files.len() - 1,
        &gaps[..gaps.len().min(5)]
    );
}

#[test]
fn no_log_line_is_lost_across_a_dated_rotation() {
    no_line_is_lost("dated");
}

#[test]
fn no_log_line_is_lost_across_a_numeric_rotation() {
    no_line_is_lost("numeric");
}

#[test]
fn the_dog_reads_the_section_of_the_name_it_was_adopted_under() {
    // Adopted as something that is NOT the default name, because the default
    // name is exactly what a broken lookup falls back to. The section gives
    // it a `max_size` far below the 10M default, so a single rotated
    // generation is proof the section was found: the sheep below writes a
    // few hundred kilobytes, which the default would never rotate.
    let shepherd = Shepherd::new();
    let sentinel = shepherd.home().join("finished");
    let script = counter_script(shepherd.home(), 40_000, &sentinel);
    shepherd.write_config(
        "[dog.weathervane]\n\
         max_size = \"8K\"\n\
         keep = 5000\n\
         compress = false\n\
         interval = \"50\"\n\
         naming = \"numeric\"\n",
    );

    shepherd.ok(&["adopt", DOG_BIN, "--name", "weathervane", "--style", "bare"]);
    shepherd.ok(&[
        "start",
        script.to_str().expect("script path"),
        "--name",
        "counter",
        "--style",
        "bare",
    ]);

    wait_until("the sheep to finish writing", || sentinel.exists());
    wait_until("the adopted dog to rotate something", || {
        generations_oldest_first(&shepherd.logs(), "counter-0-out", true).len() > 1
    });

    // The dog is in the flock under its adopted name, and identified itself.
    let listing = shepherd.ok(&["dogs", "--format", "json"]);
    assert!(listing.contains("weathervane"), "{listing}");
    let complaint = fs::read_to_string(shepherd.logs().join("weathervane-0-err.log"))
        .expect("the dog's own stderr log");
    assert!(
        !complaint.contains("cannot tell what it was adopted as"),
        "the dog failed to identify itself and fell back to the default name, so every \
         setting in [dog.weathervane] was silently discarded: {complaint}"
    );

    // And it rotated at 8K rather than at the 10M default.
    let files = generations_oldest_first(&shepherd.logs(), "counter-0-out", true);
    assert!(
        files.len() > 1,
        "nothing rotated, so [dog.weathervane] was never read: {files:?}"
    );
    let counter = concatenated_counter(&files);
    assert_eq!(counter.first(), Some(&0));
}

#[test]
fn adopt_puts_a_dog_in_the_listing_and_rehome_takes_it_out() {
    // The external-dog contract itself, against a running shepherd. This is
    // the reason the project exists: a dog nobody has to build into shep.
    let shepherd = Shepherd::new();
    let idle = write_script(shepherd.home(), "idle.sh", "#!/bin/sh\nsleep 300\n");
    shepherd.write_config(
        "[dog.weathervane]\n\
         max_size = \"64M\"\n\
         interval = \"1h\"\n",
    );

    // A shepherd first, so `adopt` takes the path that starts the dog now
    // rather than the one that only edits shep.toml.
    shepherd.ok(&[
        "start",
        idle.to_str().expect("script path"),
        "--name",
        "idle",
        "--style",
        "bare",
    ]);
    assert!(
        !shepherd
            .ok(&["dogs", "--format", "json"])
            .contains("weathervane"),
        "nothing is adopted yet"
    );

    shepherd.ok(&["adopt", DOG_BIN, "--name", "weathervane", "--style", "bare"]);
    wait_until("the adopted dog to appear in the listing", || {
        shepherd
            .ok(&["dogs", "--format", "json"])
            .contains("weathervane")
    });
    let adopted = shepherd.ok(&["dogs", "--format", "json"]);
    assert!(adopted.contains("\"kind\":\"adopted\""), "{adopted}");
    assert!(adopted.contains(DOG_BIN), "{adopted}");

    shepherd.ok(&["rehome", "weathervane", "--style", "bare"]);
    wait_until("the rehomed dog to leave the listing", || {
        !shepherd
            .ok(&["dogs", "--format", "json"])
            .contains("weathervane")
    });
    let config = fs::read_to_string(shepherd.home().join("shep.toml")).expect("shep.toml");
    assert!(
        !config.contains("weathervane"),
        "rehome forgets the [dog.<name>] table too: {config}"
    );
}

#[test]
fn a_generation_name_reached_through_a_symlinked_directory_is_left_alone() {
    // The seam between `tick` and `prune::tidy`. They were written against
    // different normalisation stories: `tidy` resolves the log directory
    // before matching, `tick` compares the spellings the shepherd reported.
    // Every unit test agreed with both, because a fake daemon hands one
    // spelling to both halves.
    //
    // A real shepherd does not have to. Two sheep given the same directory,
    // one through a symlink and one not, is enough, and the rename guard
    // missed it: measured against this exact setup, `alpha` renamed
    // `beta`'s live log out from under `beta`'s open descriptor on the first
    // tick, and `beta`'s next lines went to an inode with no name.
    let shepherd = Shepherd::new();
    let real = shepherd.home().join("real");
    fs::create_dir(&real).expect("real");
    let link = shepherd.home().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let sentinel = shepherd.home().join("finished");
    let alpha = counter_script(shepherd.home(), 40_000, &sentinel);
    let beta = write_script(
        shepherd.home(),
        "beta.sh",
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 200 ]; do echo \"B$i\"; sleep 0.05; i=$((i+1)); done\nsleep 300\n",
    );

    // `alpha`'s log lives under the symlink; `beta`'s lives under the real
    // path AND is a name `alpha` rotates into under numeric naming. Both
    // readings of `app.log.1` are correct, which is why only the shepherd
    // can settle it.
    let flockfile = shepherd.home().join("flock.toml");
    fs::write(
        &flockfile,
        format!(
            "[[app]]\n\
             name = \"alpha\"\n\
             script = \"{alpha}\"\n\
             out_file = \"{link}/app.log\"\n\
             err_file = \"{link}/alpha-err.log\"\n\
             \n\
             [[app]]\n\
             name = \"beta\"\n\
             script = \"{beta}\"\n\
             out_file = \"{real}/app.log.1\"\n\
             err_file = \"{real}/beta-err.log\"\n",
            alpha = alpha.display(),
            beta = beta.display(),
            link = link.display(),
            real = real.display(),
        ),
    )
    .expect("flockfile");

    shepherd.write_config(
        "[dog.log-rotate]\n\
         max_size = \"8K\"\n\
         keep = 5000\n\
         compress = false\n\
         interval = \"50\"\n\
         naming = \"numeric\"\n",
    );
    shepherd.ok(&[
        "start",
        "--flockfile",
        flockfile.to_str().expect("flockfile path"),
        "--style",
        "bare",
    ]);
    let mut dog = shepherd.spawn_dog();

    wait_until("alpha to finish writing", || sentinel.exists());
    wait_until("the dog to report the collision", || {
        shepherd
            .dog_stdout()
            .contains("whose rotated name is a live log")
    });
    dog.stop();
    shepherd.ok(&["stop", "alpha", "--style", "bare"]);
    shepherd.ok(&["stop", "beta", "--style", "bare"]);

    // `beta`'s live log still holds `beta`'s lines and nobody else's.
    let betas = fs::read_to_string(real.join("app.log.1")).expect("beta's live log");
    assert!(
        betas.starts_with("B0\n"),
        "beta's live log was renamed out from under it: {:?}",
        &betas[..betas.len().min(40)]
    );
    assert!(
        !betas.contains("\n0\n"),
        "alpha's output landed in beta's live log"
    );

    // And `alpha` was left alone whole: skipped, not half rotated.
    let alphas = fs::read_to_string(real.join("app.log")).expect("alpha's live log");
    assert!(alphas.starts_with("0\n"), "alpha's log lost its start");
    assert!(
        !real.join("app.log.1.1").exists(),
        "beta rotated a generation that is really alpha's output"
    );
    assert!(
        !real.join("app.log.2").exists(),
        "alpha rotated despite the collision"
    );
}
