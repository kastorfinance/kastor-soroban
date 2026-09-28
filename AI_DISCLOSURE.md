# AI disclosure

This repository was written with the help of an AI coding assistant (Claude Code, Anthropic) under the supervision of the Kastor Finance technical team.

| Area | AI involvement | Human responsibility |
|---|---|---|
| Porting the Solidity contracts to Rust/Soroban | Drafted the contract code and the tests | Review of every contract, business rules, approval of the design changes |
| Finding bugs in the Solidity contracts | Flagged the three issues in `docs/MIGRATION.md` while reading the original code | Confirming them against the deployed contracts and the platform's history |
| Deploy script, CI, documentation | Drafted | Review, testnet runs, keys and deployments |

All code is covered by tests that run in CI. Nothing in this repository has been audited.

<!-- TODO(reviewer): add name and date of the human review of each contract. -->
