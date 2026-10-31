# Vendored libraries

agentdialer carries the call with these libraries (decision D-9, `PLAN.md`
§0.0), and reads phone numbers with `phonenumber`. Each directory is an exact copy of the upstream tree at the pinned
commit, plus the patches listed here. Keep each patch small and in its own
commit, so that an upstream update is a rebase.

| Directory | Upstream | Commit | Version | License | Patches |
|---|---|---|---|---|---|
| `rsipstack/` | https://github.com/restsend/rsipstack | `5ba8ae89d4a43dfb1faaa10f7f42560198f9928c` (2026-10-20) | 0.7.7 | MIT (`LICENSE`) | none |
| `rustrtc/` | https://github.com/restsend/rustrtc | `dbf930ab2971a103ca607feb0a82fd83d314e137` (2026-10-19) | 0.4.7 + later commits | MIT (`LICENSE`) | none yet; planned: safe RTP latching, an `rtp-only` feature |
| `audio-codec/` | https://github.com/restsend/audio-codec | `7c531be71ae9b336d407bae6127428bfd751386b` (2026-10-20) | 0.4.13 | MIT (`Cargo.toml`) | `LICENSE` added: upstream has no license file |
| `phonenumber/` | https://github.com/whisperfish/rust-phonenumber | `0a1516ee708d751aee80ee0e258f4bfad3ff6c65` (2026-07-02) | 0.3.10+9.0.33 (libphonenumber metadata 9.0.33) | Apache-2.0 (`LICENSE`) | `assets/geocoding/` and `assets/carrier/` removed (12 MB): the build reads only `assets/PhoneNumberMetadata.xml` |

`phonenumber` is the crates.io package of 0.3.10+9.0.33, which names the
pinned commit in `.cargo_vcs_info.json` (vendored 2026-10-30). `ad_contact`
is its only user. To take new numbering plans, update this copy and run the
`ad_contact` tests.

Checks done when vendored (2026-10-21):
- `rsipstack` and `audio-codec`: `src/` equals the crates.io package of the
  same version, which names the same commit in `.cargo_vcs_info.json`.
- `rustrtc`: crates.io 0.4.7 was published from `b89dc2c4`. The pinned commit
  is newer. It adds an H264 packetizer (not used), the `telephone_event_pt`
  SDP helper, and a test fix. Nothing else in `src/` changes.

Rules:
- Read every change in an update before you take it.
- Never run `cargo fmt` or `clippy --fix` on this directory. The workspace
  excludes it, so the test tier does not lint it.
- Copied single files from `rustpbx` (`f33300246a765333ac217a4035cda8385a6e42f0`,
  MIT) live in our crates, not here. Each starts with a comment that names
  the upstream file and commit.
