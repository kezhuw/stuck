# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2022-04-22
### Added
- Selectable support for `coroutine::JoinHandle`.
- `complete` in `select!` to execute code if all selectables are disabled or completed

### Changed
- Generalize `select!` to read/write item and get result
- Terminate channel after closed detected
- Separate ready/after/interval from channel receiver.

## [0.2.0] - 2022-04-20
### Added
- Support crate name customization in #[stuck::main] and #[stuck::test]
- Channel for commnication across coroutines in one task
- Select for both serial and parallel channels.

### Changed
- Refactor session/suspension implementations and semantics
- Return waked for `SessionWaker.wake` and `Resumption.resume`
- Enforce mutable `Runtime` to spawn task to avoid future breaking change

### Fixed
- Wake ealier due to partially elapsed tick

## [0.1.5] - 2022-04-09
### Added
- `Sender::try_send` to send without blocking current execution.
- Network io poller and tcp support.

### Fixed
- Out of bounds in timer ticking.

## [0.1.4] - 2022-04-07
### Added
- Rustdoc for `stuck::main` and `stuck::test`.

## [0.1.3] - 2022-04-07
### Added
- Add proc macros to bootstrap runtime for main and test

### Changed
- Stop runtime in phases to avoid unnecessary thread panic

## [0.1.2] - 2022-03-26
### Added
- Add `runtime::Builder` to customize `Runtime` construction.
- Add `task::yield_now` and `coroutine::yield_now`.
- Add mpsc channels `task::mpsc::bounded` and `task::mpsc::unbounded`.
- Timer and `time::sleep`.

### Fixed
- Enforce `Send`, `Sync` restriction for `Session` and `SessionWaker`.

## [0.1.1] - 2022-03-24
### Fixed
- Make `SessionWaker` safe to call outside runtime environment.
- Reclaim tasks in process of `Runtime::drop`.

## [0.1.0] - 2022-03-23
### Added
- Initial release.

[0.3.0]: https://github.com/kezhuw/stuck/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/kezhuw/stuck/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/kezhuw/stuck/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/kezhuw/stuck/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/kezhuw/stuck/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/kezhuw/stuck/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/kezhuw/stuck/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/kezhuw/stuck/releases/tag/v0.1.0
