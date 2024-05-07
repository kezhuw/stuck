# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2024-05-07
### Added
- feat: support uninterruptible session for asynchronous operations ([#52](https://github.com/kezhuw/stuck/pull/52))
- feat: support blocking std::thread receiver for parallel channel ([563353a](https://github.com/kezhuw/stuck/commit/563353ab3ba53677ded2e606584c88be326e681a))
- feat: support select! on std::thread ([0d0cfd5](https://github.com/kezhuw/stuck/commit/0d0cfd5a58cf7c96843af9d45be1b9db0a2fd367))
- Name stuck runtime threads ([#39](https://github.com/kezhuw/stuck/pull/39))
- feat: complete task only after all auxiliary coroutines completed ([#48](https://github.com/kezhuw/stuck/pull/48))
- ci: add build/test/lint matrix for ubuntu and macos ([#56](https://github.com/kezhuw/stuck/pull/56))

### Changed
- Defaults parallelism to STUCK_PARALLELISM_DEFAULT env var ([#36](https://github.com/kezhuw/stuck/pull/36))
- Defaults stack size to STUCK_STACK_SIZE env var ([#38](https://github.com/kezhuw/stuck/pull/38))
- Use `mmap` to allocate stack to avoid page fault in allocation ([#40](https://github.com/kezhuw/stuck/pull/40))

### Fixed
- Fix mio slab token leak after socket dropped ([#43](https://github.com/kezhuw/stuck/pull/43))
- test: fix duplicated test runs from `test_case` and `stuck::test` ([#53](https://github.com/kezhuw/stuck/pull/53))
- fix: parallel channel bound violated temporarily ([2ddef1d](https://github.com/kezhuw/stuck/commit/2ddef1da60255d495804fb39c3a413d94a130340))
- fix: parallel channel send succeed after aborted ([bfdaac4](https://github.com/kezhuw/stuck/commit/bfdaac46d7c25c0ebff92d99a670fdb0918e0d1b))
- fix: crash on macOS arm cpus, e.g. M1 ([#57](https://github.com/kezhuw/stuck/pull/57))

## [0.3.2] - 2024-04-02
### Changed
- Fix crash in macOS due to missing mcontext inside ucontext

## [0.3.1] - 2022-04-23
### Changed
- Abstract operations for both serial and parallel channels
- Construct Timer with alloc and Box::from_raw to avoid large stack requirement
- Change default stack size to 16 times page size.

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

[0.4.0]: https://github.com/kezhuw/stuck/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/kezhuw/stuck/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/kezhuw/stuck/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/kezhuw/stuck/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/kezhuw/stuck/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/kezhuw/stuck/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/kezhuw/stuck/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/kezhuw/stuck/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/kezhuw/stuck/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/kezhuw/stuck/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/kezhuw/stuck/releases/tag/v0.1.0
