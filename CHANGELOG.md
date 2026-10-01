# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0] - 2026-10-01


### Features

- spell out herdr-linear-agent in pane tokens and other names (#85)
- pick routings per team and postmortems when they are written (#82)


### Maintenance

- update jdx/mise-action action to v5.0.1 (#84)

## [0.5.0] - 2026-09-30


### Bug Fixes

- begin each postmortem with how the run stands (#81)
- tell the final postmortem which state the issue closed in (#80)
- color the history screens with the theme's palette (#78)


### Features

- post interim and final postmortems per team (#79)
- record agent sessions and keep their transcripts in the run folder (#76)
- browse past runs and their agent transcripts (#74)


### Maintenance

- update dependency jdx/mise to v2026.9.18 (#77)

## [0.4.0] - 2026-09-30


### Bug Fixes

- take the delegator from the latest delegation in the issue history (#73)
- end the Linear session when a run closes (#68)


### Documentation

- record the live check of allowed delegators (#72)
- record the live check of the closing response and manual reload (#70)
- tell users to disable the webhook's delivery (#67)
- record the live check of profile inheritance and config reload (#66)
- record the live check with two Linear workspaces (#63)
- record the live check of workspace-qualified runs (#62)
- record that no Linear scope or mutation exposes a worker's checks (#59)
- record that the app user cannot read a worker's checks from Linear (#58)
- record which Linear fields carry pull request checks and reviews (#57)


### Features

- take only issues delegated by allowed users (#71)
- reload the config on request and tell when it changed (#69)
- reload the config without restarting the ticker (#65)
- let a profile inherit from another profile (#64)
- support several Linear workspaces and per-team settings (#61)
- read each profile from a folder of its own (#60)
- keep coordinators off their own subagents and workers off CI polling (#56)


### Maintenance

- update dependency jdx/mise to v2026.9.17 (#49)
- update jdx/mise-action action to v5 (#50)

## [0.3.0] - 2026-09-29


### Documentation

- record the Cursor Agent and OpenCode coordinator checks (#52)
- record the live routing check in the scratch Linear team (#48)


### Features

- let the routing agent's profile set environment variables (#53)
- support Cursor Agent and OpenCode as coordinators and routing agents (#51)
- let a routing agent pick each issue's coordinator profile (#46)


### Maintenance

- update mise to 2026.9.16 (#54)

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


