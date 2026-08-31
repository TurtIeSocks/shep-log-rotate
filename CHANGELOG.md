# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
