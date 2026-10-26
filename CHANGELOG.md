# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Decoding of the BM787BT and BM788BT's readings, status flags and
  advertisements, following Brymen's protocol document, revision 4.
- The meter's commands: password, clock, model, firmware version and device
  name.
- A session that checks the password and model, can set the meter's clock,
  and reconnects after a lost link.
- A BlueZ backend that finds meters and connects to them over D-Bus, in pure
  Rust.

[unreleased]: https://github.com/kevstewa/bm78x/commits/main
