# Third-party components

## Font

`assets/fonts/CascadiaCode.ttf` is Cascadia Code, © Microsoft Corporation,
SIL Open Font License 1.1. Full text: `assets/fonts/CascadiaCode-LICENSE.txt`.

## Icons

The icons in `assets/icons/` are from [Lucide](https://lucide.dev), except
`servers.svg`, which was drawn for flint. Lucide is under the ISC License,
© Lucide Icons and Contributors. Some of its icons come from Feather,
© Cole Bemis, MIT License. Full text: `assets/icons/LUCIDE-LICENSE.txt`.

The marks in `assets/icons/distro/` are from [Simple Icons](https://simpleicons.org),
CC0 1.0. They name the operating system a host runs; the brands belong to
their owners.

`gpui-kit-assets` embeds its own icon set, which is mostly Lucide icons under
the same license.

## Dependencies

- `gpui-pre`, `gpui-pre-platform` (Zed Industries, crates.io snapshots of
  `gpui`): Apache-2.0
- `gpui-kit`, `gpui-component`, `gpui-kit-assets` (Longbridge): Apache-2.0
- `alacritty_terminal`: Apache-2.0
- `russh`, `russh-sftp`: Apache-2.0
- `tokio`: MIT
- `argon2`, `chacha20poly1305`: MIT OR Apache-2.0 (vault encryption)
- `bip39`: CC0-1.0 (recovery phrase)
- `ed25519-dalek`, `x25519-dalek`: BSD-3-Clause (team keys)
- `ureq`: MIT OR Apache-2.0 (sync and the update check). Its TLS is `rustls`
  (Apache-2.0 OR ISC OR MIT) with `ring` (Apache-2.0 AND ISC),
  `rustls-webpki` (ISC) and `webpki-roots` (CDLA-Permissive-2.0, Mozilla's
  root certificates).

To list every crate with its license:

    cargo tree -e normal,build --prefix none -f '{p} {l}' | sort -u
