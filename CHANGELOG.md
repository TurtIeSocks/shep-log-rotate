# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.6] - 2026-09-04

### Added

- Gzip on a blocking thread, and let ctrl-c interrupt it
- Count max_age from the last rotation, not the last write

### Changed

- One Error::io_at for the eleven map_err closures naming a path
- Name the generation listing's item instead of a three-tuple
- Take the infix by reference

### Fixed

- One file under two spellings is one rotation, and one resolved index for every guard
- Resolve a base the way the protected set was, not the way the disk reads now
- Tidy what every sheep reopened, even when a later one stops the tick
- Close what the review of the reopen, offload and max_age changes found
- A fault on disk is one log's problem, never the tick's
- Hold back the file a rename failed on, and let one bad generation not stop a prune
- A compression that fails after creating its target leaves a twin, not an orphan
- Refuse a symlink where an archive would be written
- Create the archive exclusively, so a link cannot be swapped in between check and create


## [0.1.5] - 2026-09-03

### Fixed

- Name this dog in the handshake so shep records one
- Name the section this dog was actually adopted as
- Let one config fault name one section, and the right one
- Keep the shepherd's socket path out of Debug, and say why Error keeps its own


## [0.1.4] - 2026-08-31

### Fixed

- Refresh the lockfile so the dog speaks the current protocol


## [0.1.3] - 2026-08-29


## [0.1.2] - 2026-08-28

### Fixed

- Compare trees instead of banning merge commits


## [0.1.1](https://github.com/shep-pm/shep-log-rotate/compare/v0.1.0...v0.1.1) - 2026-08-26

### Other

- let release-plz publish ([#3](https://github.com/shep-pm/shep-log-rotate/pull/3))

## [0.1.0](https://github.com/shep-pm/shep-log-rotate/releases/tag/v0.1.0) - 2026-08-26

### Added

- the binary, its closed argument surface, and the poll loop
- one rotation pass, with per-sheep reopen and abort on refusal
- compress all but the newest generation, and prune past keep
- rotate one log file, in both naming schemes
- name and match rotated generations in both schemes
- parse the [dog.log-rotate] section, with a default for every field
- crate skeleton, dependencies and the error type

### Fixed

- *(ci)* release-plz/action has no v0 tag, and a tag is the wrong thing to trust
- a repeated --print-config was refused as a flag we do not understand
- the tick summary was ungrammatical and named the wrong unit
- a log path with no directory component erased its own history
- the rename guard read a symlinked directory as a different one
- a half-compressed generation is one generation, not two
- refuse to wrap a generation counter, document the single-rotator assumption
- the matcher accepted dates that never happened
- the PRINT_CONFIG round-trip test asserted nothing at all

### Other

- release-plz owns the changelog, the tag workflow owns the upload
- publish to crates.io on a v* tag
- run the suite, and prove the dog can still talk to a shepherd
- make this publishable the moment shep is
- the design doc's own title carried an em dash
- the README said a quiet pass prints nothing, then said otherwise
- one with_gz, next to the matcher that strips the suffix
- two Report counters named a unit their own source contradicts
- three intra-doc links rustdoc could not resolve
- the README
- the real-shepherd tier, five tests against a live daemon
- shep has no day unit, so a week is 168h
- a dog cannot learn the name it was adopted under
- ignore the subagent-driven-development scratch directory
- implementation plan, with the two deviations the real code forced
- defer the dog-manifest idea, and say why it is a boundary not a gap
- support both naming schemes, defaulting to dated
- design for shep-log-rotate, a fully external dog
- Initial commit
