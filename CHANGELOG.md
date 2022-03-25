# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
### Added
- Add `runtime::Builder` to customize `Runtime` construction.
- Add `task::yield_now` and `coroutine::yield_now`.
- Add mpsc channels `task::mpsc::bounded` and `task::mpsc::unbounded`.

### Fixed
- Enforce `Send`, `Sync` restriction for `Session` and `SessionWaker`.

## [0.1.1] - 2022-02-24
### Fixed
- Make `SessionWaker` safe to call outside runtime environment.
- Reclaim tasks in process of `Runtime::drop`.

## [0.1.0] - 2022-02-23
### Added
- Initial release.

[0.1.1]: https://github.com/kezhuw/stuck/compare/0.1.0...0.1.1
[0.1.0]: https://github.com/kezhuw/stuck/releases/tag/v0.1.0
