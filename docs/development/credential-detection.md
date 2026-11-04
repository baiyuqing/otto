# Credential detection

The [session manual](../manual/interactive-use.md#session-credential-redaction)
owns user-visible behavior and limitations. The detector lives in
`crates/otto-core/src/agent/redactor/` and uses the existing `regex`, `url`, `base64`
and `sha2` dependencies. It performs no network verification or subprocess
execution.

## Rules and recording boundary

Keep exact-value replacement distinct from credential-pattern detection.
`redact_record_text`, `redact_record_json` and `redact_record_message` scan
recording copies. `guard_provider_request` uses the same credential patterns
and adds its existing PII handling. Do not substitute recorded arguments into
live tool calls or broaden `redact_string` globally: its callers also handle
protocol identities, execution and streaming.

Provider patterns cover AWS access key IDs, Alibaba access key IDs and Azure
AD client secrets, alongside existing token prefixes. Contextual assignments
and JSON field names identify access/secret keys, credentials, client secrets,
passwords, tokens and wallet material. Generic assigned values have an
eight-byte minimum and placeholder exclusions; arbitrary high-entropy strings
are not treated as secrets solely because of their entropy.

Connection handling recognizes URL password userinfo, sensitive URL query
values, common password/key assignments in DSNs and Go MySQL DSNs. Wallet
handling recognizes English BIP-39 phrases with 12/15/18/21/24 words and a valid
checksum, mainnet/testnet Bitcoin WIF with a valid checksum, and contextual
private-key/seed fields. Bare hexadecimal strings and public wallet addresses
are left alone without other evidence.

The agent and native POSIX journal sanitize new message content. Native child
task, hook-audit, turn-error and plan custom entries also use this protection.
Compaction summaries are sanitized before append. Structural message/call IDs,
operation facts and opaque provider compaction state are preserved. Recorded
arguments containing replacement markers cannot be automatically replayed
after process loss; the native store repairs those calls as interrupted.

Mailbox sidecars sanitize copies of pending notification/prompt text, leaving
the running queue unchanged. A restart restores the sanitized text. Approval
tools and authorization/reviewer records retain the documented approval
scanning exception. Reading, resuming, archiving and forking old records do not
retroactively scrub their bytes.

## Provenance

- AWS, Alibaba and Azure credential-prefix patterns are adapted from
  [Gitleaks rules](https://github.com/gitleaks/gitleaks/blob/5f6ec56cfa6f8c67648d863582c7ae2f206d5c1c/config/gitleaks.toml)
  under the MIT license (notice below). These selected patterns are not the
  entire Gitleaks ruleset or a claim of equivalent coverage.
- The English wordlist in `redactor/bip39-english.txt` comes from
  [BIP-39](https://github.com/bitcoin/bips/blob/61ff3f1361838fa7eb671e650b4e614207ca9df7/bip-0039/english.txt).
  [The specification](https://github.com/bitcoin/bips/blob/61ff3f1361838fa7eb671e650b4e614207ca9df7/bip-0039.mediawiki)
  is MIT-licensed; its authors are Marek Palatinus, Pavol Rusnak, Aaron Voisine
  and Sean Bowe. Only the published wordlist and format are used.
- WIF format checks follow Bitcoin Core's
  [key encoding](https://github.com/bitcoin/bitcoin/blob/master/src/key_io.cpp)
  and [Base58Check](https://github.com/bitcoin/bitcoin/blob/master/src/base58.cpp)
  format. The bounded local decoder is not a signing or wallet implementation.
- TruffleHog's database detectors and Datadog SDS informed the evaluation;
  neither codebase nor runtime is included.

## Gitleaks MIT notice

Copyright (c) 2019 Zachary Rice

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
