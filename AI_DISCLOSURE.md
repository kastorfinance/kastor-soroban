# AI disclosure

We use an AI coding assistant (Claude Code, by Anthropic) the way we use any other tool: it drafts, we decide. It writes first versions of contracts, tests, scripts and documents. I review them as technical lead, change what does not convince me, and nothing reaches `main` without my approval, passing tests and CI.

The table says what the assistant did in each area and who answers for the result.

| Area | What the assistant did | Reviewed and approved by | Date |
|---|---|---|---|
| Porting the Solidity contracts to Rust/Soroban (`kyc-registry`, `property-token`, `primary-sale`, `dividends`) | Drafted the contract code and the tests | Carlos Serna, technical lead | 2026-10-15 |
| Bugs in the Solidity contracts | Flagged the three issues in `docs/MIGRATION.md` while reading the original code | Carlos Serna, technical lead | 2026-10-15 |
| `secondary-market` | Designed and wrote the contract and its 18 tests from the whitepaper's description of the secondary market; there was no Solidity version to port | Carlos Serna, technical lead: business rules (non-custodial design, capped fee, operator cancel) and code | 2026-10-15 |
| Deploy script, CI, release workflow, documentation | Drafted | Carlos Serna, technical lead | 2026-10-15 |

All code is covered by tests that run in CI. Nothing in this repository has been audited.

— Carlos Serna, technical lead, Kastor Finance
