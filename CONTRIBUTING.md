# Contributing to Softromeda Screen Recorder

Thank you for helping improve Softromeda Screen Recorder! Bug reports, translations, fixes and new
features are all welcome. Everyone taking part is expected to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Reporting problems

Open an issue at <https://github.com/softromeda/screenrecorder/issues>. Please include your
Windows version, what you did, what you expected and what happened instead. The log file
(*Options → General → Open log file*) helps a lot with crashes and failed recordings.

Security vulnerabilities must not be reported in public issues: follow the
[Security Policy](SECURITY.md) instead.

## Making changes

1. Fork the repository and create a branch for your change.
2. Build and test it:

   ```bash
   cargo build --release
   cargo test
   ```

3. Keep the style of the surrounding code and run `cargo fmt` before committing.
4. New text in the user interface goes through `tr(...)` / `tf(...)` and needs an entry in every
   file in `locales/`; `cargo test` reports any that are missing.
5. Every source file starts with the Softromeda license header (copy it from any existing file,
   e.g. `src/main.rs`). Keep it unchanged in files you edit and add it to files you create.
6. Open a pull request describing what changed and why.

## License of contributions

Softromeda Screen Recorder is licensed under the GNU General Public License v3.0 or later, with the
additional terms in [NOTICE](NOTICE). By contributing, you agree that your contribution is
licensed under the same terms, and that the existing copyright notices and the attribution
*"Originally created by Softromeda"* stay in place.

Each commit must be signed off to certify the
[Developer Certificate of Origin](https://developercertificate.org/): that you wrote the change
or otherwise have the right to submit it under this license. Add the sign-off with `-s`:

```bash
git commit -s -m "Fix crash when the microphone is unplugged"
```

This adds a line like `Signed-off-by: Your Name <you@example.com>` to the commit message.
