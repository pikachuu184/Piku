# Summary

<!-- What does this change do, and why? Keep it concise. -->

## Related issue

<!-- e.g. Closes #123. Delete this section if not applicable. -->

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor or cleanup
- [ ] Documentation
- [ ] Build, CI, or tooling

## How this was tested

<!-- Commands run, manual steps taken, platforms verified. -->

## Checklist

- [ ] `cargo build` succeeds
- [ ] `cargo clippy` reports no new warnings
- [ ] `cargo test` passes
- [ ] `cargo fmt` has been run
- [ ] Preview parsers keep their input untrusted (no `unwrap`/`expect`/`panic` in `src/preview/loader.rs`, size caps respected)
- [ ] UI changes follow the monochrome theme (only `cx.theme().*` tokens, `cx.theme().radius`, no new hues or shadows)
- [ ] Documentation and comments updated where relevant
