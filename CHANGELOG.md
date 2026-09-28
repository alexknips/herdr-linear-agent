# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1] - 2026-09-28


### Bug Fixes

- update rust crate toml to v1 (#15)


### Maintenance

- keep reqwest on the version oauth2 uses (#45)

## [0.2.0] - 2026-09-28


### Bug Fixes

- leave agent session comments out of the issue hash (#42)
- keep a just-prompted worker working and open worktrees with their checkout (#40)
- recognize a coordinator Herdr is still launching (#37)


### Documentation

- record the live check of issue edits (#43)
- record the integration test of the async rewrite (#41)
- record the behavior the async rewrite keeps (#31)


### Features

- pace Linear reads by the rate-limit budget (#38)
- make the ticker event-driven on the Herdr socket (#35)
- run on tokio and read Herdr over its socket (#32)


### Maintenance

- lock file maintenance (#34)


### Refactor

- rewrite the remaining helpers and drop the herdr-projects attribution (#39)

## [0.1.0] - 2026-09-27


### Bug Fixes

- hold prompts and heartbeats after a stop until the next reply (#28)
- use the session Linear creates on delegation and finish cut-short claims (#24)
- accept Linear's space-separated scope and report the OAuth step that failed (#23)
- give the link handler the title Herdr requires (#22)
- update rust crate sha2 to 0.11 (#13)
- update rust crate base64 to 0.23 (#11)


### Documentation

- record the first integration test (#29)
- tell herdr-linear-agent apart from Linear's Linear Agent (#21)
- explain how worker brief folders stay out of commits (#20)


### Features

- optionally pre-trust Claude Code folders before launching agents (#25)
- support Linux (#16)
- implement the Linear agent plugin (#8)


### Maintenance

- update dependency jdx/mise to v2026.9.15 (#27)
- update dependency jdx/mise to v2026.9.14 (#14)
- update dependency aqua:suzuki-shunsuke/pinact to v5 (#5)
- update csm-actions/approve-pr-action action to v1 (#3)
- drop the bootstrap guards from tasks and CI (#19)
- remove a timing assumption from the routing agent scenario (#18)
- attach macOS binaries to releases (#9)
- update dependency jdx/mise to v2026.9.14 (#12)
- pin rust crate tempfile to =3.27.0 (#10)
- add release workflows (#4)
- update dependency rust to v1.98.1 (#2)
- repository foundation (#1)
- bootstrap repository


### Refactor

- rename claude.pre_trust to claude.auto_accept_trust_dialog (#26)


