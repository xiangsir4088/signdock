# Contributing to SignDock

Thanks for considering a contribution. Please read this before opening a PR.

## Before you start

- **SignDock is a single-user, single-account tool.** PRs that add multi-account, session-copy, account-switch, or bulk-claim features will be closed without discussion. See the [README Statement](./README.md#statement) for why.
- **No new vendor adapters without an upstream public API.** If the vendor hasn't published an official endpoint, we won't add an adapter that reverse-engineers one.
- **No changes to the credential-storage contract.** DPAPI envelope under `%APPDATA%\com.signdock.app\` is load-bearing — the "no other user can decrypt" property is a security feature, not a UX inconvenience.

## Development setup

```bash
npm install        # frontend deps (Vite + TypeScript)
npm run dev        # Vite dev server on :1420
npm run tauri dev  # Tauri desktop shell (needs Rust + Tauri 2 prerequisites)
```

Prerequisites:

- Rust stable (2021 edition)
- [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/) for your platform
- Node.js 22+

## Build and test

```bash
npm run build            # tsc && vite build (frontend type-check + bundle)
cd src-tauri && cargo test
```

CI runs both on every push and every PR. A green PR must have both:

1. `npm run build` passing (TypeScript strict mode, no `any` without `@ts-expect-error`).
2. `cargo test` passing, including the `wiremock`-based adapter tests.

## Code style

- **Rust**: `cargo fmt` and `cargo clippy -- -D warnings` clean.
- **TypeScript**: strict mode already enforced in `tsconfig.json`. Frontend lives in `src/main.ts`; keep the single-file layout unless there's a concrete reason to split.
- **Commit messages**: Conventional Commits style (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`). Bilingual messages are fine; English-only is preferred for a public repo.

## Pull request checklist

- [ ] Problem statement in the PR description — what user-visible behaviour changes, and why.
- [ ] Tests added or updated where the change touches logic under test.
- [ ] `npm run build` and `cargo test` both pass locally.
- [ ] No new vendor endpoints added unless accompanied by a `docs/` note explaining the endpoint's public nature and its upstream documentation link.
- [ ] No new scripts under `scripts/` that touch MITM, proxy manipulation, or vendor certificate trust. Those do not belong in the public repo.

## Code of Conduct

SignDock does not host a formal Code of Conduct document. Contributors are expected to interact respectfully, stay on topic, and not use the issue tracker as a channel for vendor abuse (bulk claim requests, credential sharing, etc.). PRs and issues that violate this will be closed.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](./LICENSE).
