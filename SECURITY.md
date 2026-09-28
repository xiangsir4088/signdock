# Security Policy

## Reporting a vulnerability

Please do **not** open a public issue for security-sensitive bugs.

Send a private message through GitHub's [Private Vulnerability Reporting](https://docs.github.com/en/code-security/getting-started/privately-reporting-a-vulnerability) button on this repository, or contact the maintainer directly via email listed in the maintainer's GitHub profile.

Acknowledgement: within 3 business days. Remediation target: 30 days for critical, 90 days for lower severity.

## Scope

In scope for this policy:

- SignDock runtime behaviour on the user's own machine
- Credential handling (DPAPI envelopes under `%APPDATA%\com.signdock.app\`)
- The bundled Tauri / Rust dependencies used by SignDock itself

Out of scope (we will not act on these):

- Vulnerabilities inside third-party products that SignDock integrates with (WorkBuddy, Trae, Qoder, 秒哒, etc.). Report those to the vendor.
- Requests to disable DPAPI sealing, bypass the login flow, or add multi-account / session-sharing / mass-claim features. See [README — Statement](./README.md#statement).
- "Please add feature X for vendor Y" without a corresponding upstream public API.

## How SignDock handles credentials

- SignDock does **not** accept token pasting. WorkBuddy authenticates via the vendor's official OAuth; Trae and Qoder authenticate by reading the local desktop client's already-logged-in state; 秒哒 authenticates through a SignDock-owned WebView2 window the user logs into once.
- All SignDock-owned credentials (`workbuddy-cred.json`, `miaoda-cred.json`) are stored as **DPAPI envelopes** bound to the current Windows user. They cannot be decrypted on another user or another machine.
- SignDock never writes Trae / Qoder credentials to disk — it reads the source file read-only and re-derives derived headers in memory.
- SignDock does **not** refresh Trae's or Qoder's `refreshToken` — that would evict the credential held by the real desktop client. When a token expires, the user is instructed to open the real client and let it refresh itself.
- SignDock sends traffic to the vendors' own domains using the vendor's own endpoints. No traffic goes through any SignDock-controlled relay.

## What we intentionally do NOT do

- Multi-account management, session copying, "session clone" for secondary accounts.
- Client cracking, certificate-pinning bypass, protocol downgrade, or injection into vendor executables.
- Any form of bulk / distributed claim automation.

These boundaries are design constraints, not features pending a future release.

## Bug bounty

No bug bounty. Contributions are welcome via the [Contributing](./CONTRIBUTING.md) guide; security fixes that close a real issue will be credited on merge.
