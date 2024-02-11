# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.4] - 2024-02-11

### Added

- update workflow for manual initiation

### Changed

- Minor logging tweaks.

### Fixed

- There was a bug in the threading code where `worker.try_tick()` was only being called once, instead of in a loop. This resulted in spawned tasks only executing a single tick concurrently, before the `await` code ran it to completion.

## [0.0.3] - 2023-11-28

### Added

- code coverage
- additional tests
- new category to Cargo.toml

### Changed

- made Executor::root_worker public

### Fixed

- clippy lints
- readme link to documentation

### Removed

- Worker::key method

## [0.0.2] - 2023-11-22

### Added

- Better documentation.
- Better Worker API.
- spawn_task API
- Cleaned up private bits to keep them that way.
- Added a [Reqwest](https://github.com/seanmonstar/reqwest) test
- Added a [Warp](https://github.com/seanmonstar/warp) example
- Added a silly benchmark I noticed on Reddit to examples
- Updated README

## [0.0.1] - 2023-11-19

### Added

- Initial release of crate. Basic functionality is there, it just needs some
  documentation ond fine tuning of the API.

[unreleased]: https://github.com/uberfoo/puteketeke/compare/v0.0.4...develop
[0.0.4]: https://github.com/uberfoo/puteketeke/compare/v0.0.3...v0.0.4
[0.0.3]: https://github.com/uberfoo/puteketeke/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/uberfoo/puteketeke/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/uberFoo/puteketeke/releases/tag/v0.0.1