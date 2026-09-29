# Third-party notices

## cli-stream 0.5.0

- Upstream: https://github.com/getlatentic/agent-harness
- Source commit: `cd42566aab9e21903e96b4d59ab7e674f7fb415d`
- Original path: `crates/cli-stream`
- Local copy: [`vendor/cli-stream`](vendor/cli-stream)
- License: **MIT OR Apache-2.0**. Original license texts are retained as [LICENSE-MIT](vendor/cli-stream/LICENSE-MIT) and [LICENSE-APACHE](vendor/cli-stream/LICENSE-APACHE).
- Modification: workspace-inherited package metadata and dependencies in `Cargo.toml` are made explicit so the crate builds independently. Rust source files are unchanged.

The repository's MIT license applies to original project code and does not replace third-party licenses. Other dependencies are resolved by Cargo from `Cargo.lock` and remain subject to their own licenses.

## ModelTrace

- Upstream: https://github.com/xqy2006/ModelTrace
- Source commit: `df3a0f9d3e054c0dc02d6d586686db8daf8fa7c8`
- Bank build time: `2026-09-23T06:17:08.081468+00:00`; 16 model labels.
- Local copy: [`vendor/modeltrace`](vendor/modeltrace), with the original [MIT license](vendor/modeltrace/LICENSE).
- `static/fingerprint-core.js` and `static/challenge-browser.js` are retained byte-for-byte with `.mjs` extensions. `data/unified_bank.json` is retained unchanged. AR embeds all three in its executable and uses the upstream three-challenge classifier without changing its scoring or calibration.
- `tests/modeltrace-astra-fixture.json` contains three recorded Astra answers extracted from `data/gpt_reference.jsonl` at the same commit. They are regression fixtures, not an independent accuracy benchmark.
- AR supplies its own API transport and result presentation. Node.js 20+ is required; the `llm-fingerprint-detector` npm package is no longer used.
