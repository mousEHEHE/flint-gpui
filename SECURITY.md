# Security

Nobody did an independent audit of flint. I tried to do things properly, but please keep this in mind.

## Reporting a problem

Please use private reporting on GitHub: Security tab of this repository, then "Report a vulnerability". Don't open a public issue for it.

## Vault

All hosts, passwords and keys are in one file, `vault.bin`.

- The data is encrypted with XChaCha20-Poly1305 using a random 32-byte key.
- This key is encrypted with your master password through Argon2id (256 MiB, 3 passes).
- The same key is also encrypted with the 24-word recovery phrase (BIP39, 256 bits). So the phrase opens the vault too, keep it safe.

When you change the master password, the data key stays the same. An old copy of `vault.bin` still opens with the old password.

## Sync

Sync is optional. The server is not open source, so I can only describe what the client sends.

Your password is never sent. The server gets a key derived from it (Argon2id, then HKDF), and the encrypted data key. Every record is encrypted on your machine with XChaCha20-Poly1305 before upload.

The server can see:

- your GitHub account, device name, OS and app version
- your IP address and when you sync
- how many records you have and their sizes

The server can't read the records. But it can try to guess your password offline, with full Argon2id cost for each try, so please use a long one. It can also give you an old version of a record and the app will not notice.

## Teams

Each account has an Ed25519 key for signing and an X25519 key for encryption. The list of team members is signed by the owner or an admin, and every device checks the signatures itself and does not just trust the server.

Each team vault has its own key. It is sent to every member encrypted with their X25519 key. When someone is removed, the vault gets a new key and all records are encrypted again.

The second part of an invite link (after `#`) is never sent to the server.

The server can see the team name, members, their roles and vault names.

What a removed member already downloaded stays on their device. Change the passwords of the hosts after you remove someone.

## Updates

The app checks for a new version on `flint.mouse.pw`. You can turn it off in settings. It downloads the file to your Downloads folder and checks size and SHA-256, but never runs it.

There is no signature check. The hash comes from the same server as the file, so it only shows that the download is not broken. The macOS build is signed and notarized, so macOS checks it. The Windows build is not signed.

## Known problems

- The vault does not lock by itself.
- Open sessions keep passwords and keys in memory to reconnect, also when the vault is locked. Close the tab to drop them.
- Only the vault key is wiped from memory on lock. Other secrets are freed but not zeroed.
- The log file is plain text and has host names and user names in it.
- Remote files you open or drag out are saved in a cache folder without encryption. Small files from folders you open are also copied there, so drag and drop is fast.
- Host keys are trusted on first connect. Hosts from `~/.ssh/known_hosts` are accepted without asking.
- A port forward on localhost is open for every program on your machine.
- On Windows the vault file has no special permissions.
