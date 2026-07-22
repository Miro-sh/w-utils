# Publishing to external channels

The release workflow (`.github/workflows/release.yml`) can publish `w-utils`
beyond GitHub Releases. Every channel is **dormant by default**: it only runs
when its repository variable is set to `true`, and each one needs an account or
token that only the repo owner can create.

Activation happens in two places on GitHub:
[Settings → Secrets and variables → Actions](../../settings/actions)

- *Secrets* tab: tokens and keys
- *Variables* tab: the `*_ENABLED` switches

## Homebrew tap

Users run `brew install Miro-sh/tap/w-utils`.

1. Create a classic PAT at <https://github.com/settings/tokens> with the `repo` scope.
2. Add it as secret `HOMEBREW_TAP_TOKEN`.
3. Set variable `HOMEBREW_TAP_ENABLED` to `true`.

On every tag, the `homebrew` job updates `Formula/w-utils.rb` in
[Miro-sh/homebrew-tap](https://github.com/Miro-sh/homebrew-tap) via
`script/update-homebrew-tap.sh`.

## AUR (Arch Linux)

Package `w-utils-bin` on the AUR.

1. Create an account at <https://aur.archlinux.org/register>.
2. Generate a dedicated key: `ssh-keygen -t ed25519 -f aur -N ''`.
3. Paste `aur.pub` into your AUR account (SSH Public Keys).
4. Add the content of `aur` (private key) as secret `AUR_SSH_PRIVATE_KEY`.
5. Set variable `AUR_ENABLED` to `true`.

The first push from `script/publish-aur.sh` creates the package; later tags
update it.

## crates.io

`cargo install w-utils` (installs the `wcp` binary).

1. Create an account at <https://crates.io> and an API token at
   <https://crates.io/me> (scope: publish new crates + publish updates).
2. Add it as secret `CARGO_REGISTRY_TOKEN`.
3. Set variable `CRATES_ENABLED` to `true`.

The first tag after that publishes the crate.

## COPR (Fedora)

Users run `dnf copr enable <you>/w-utils && dnf install w-utils`.
No secret needed: COPR builds from this repository directly.

1. Create an account at <https://copr.fedorainfracloud.org>.
2. New project `w-utils`. In the project settings add a *package*:
   - Type: **SCM**, Clone URL: `https://github.com/Miro-sh/w-utils`
   - Subdirectory: (empty), Spec file: `packaging/w-utils.spec`
   - SRPM build method: **make srpm** (uses `.copr/Makefile`)
3. Enable the webhook: COPR project → Integrations → GitHub, so pushes to
   `main` trigger builds automatically.

Dependencies are vendored into the SRPM by `.copr/Makefile` (COPR builders
have no network access), and the spec removes `.cargo/config.toml` so the
build targets native glibc.

## What is NOT automatable

- Fedora / Debian / Ubuntu official repositories: human review processes.
- homebrew-core: PR-based, needs ~75 stars to be eligible. `brew bump-formula-pr`
  can automate updates after inclusion.
- Flathub, winget-pkgs: PR-based.
