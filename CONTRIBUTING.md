# Contributing to phasor

Thanks for your interest in improving phasor! Contributions of all kinds are
welcome — bug reports, fixes, features, docs.

## Getting started

```sh
git clone https://github.com/interpretica-io/phasor
cd phasor
cargo build
cargo test
```

phasor targets **macOS** and **Linux** and needs `tmux` and the `claude` CLI on
`PATH` at runtime (not for building/testing). See the README for details.

## Before you open a pull request

Run the same checks CI runs — all must pass:

```sh
cargo fmt --all --check      # formatting
cargo clippy --all-targets -- -D warnings   # lints (no warnings allowed)
cargo test                   # unit tests
```

Guidelines:

- Keep changes focused; one logical change per PR.
- Match the surrounding code style; document public and private items
  (`missing_docs_in_private_items` is enforced — every item needs a doc comment).
- Add or update tests for behaviour you change.
- Update the README / `CHANGELOG.md` when user-facing behaviour changes.

## Testing safely

phasor talks to a live `tmux` server and reads/writes real user state. **Never
let tests or manual experiments touch the real instance:**

- Use an isolated tmux server and session for any manual testing:
  `PHASOR_SOCKET=phasor_test PHASOR_SESSION=phasor_test cargo run`, and only
  ever `tmux -L phasor_test kill-server` — never the real `phasor` socket.
- The projects config lives at `~/.phasor/projects.json`. Don't overwrite it
  from tests; the unit tests use temporary files for this.

## Reporting bugs / requesting features

Open an issue using the provided templates. For security issues, **do not** open
a public issue — see [SECURITY.md](SECURITY.md).

## Licensing

By contributing, you agree that your contributions will be dual-licensed under
the [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses, as described
in the README.
