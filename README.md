# Veilwire

**English | [简体中文](README.zh-CN.md)**

**Anonymous, end-to-end encrypted chat and calls. No account. No phone number. No central directory.**

Veilwire is an Android messenger built on one rule: the server that carries your messages should never
learn who you are or what you said. Your identity is a BIP39 phrase generated on the device — no sign-up,
no email, no phone number, no contact upload. Every message is sealed with Olm (3DH + double ratchet)
before it leaves the phone.

A *relay* holds opaque ciphertext in per-recipient queues and does nothing else. Anyone can run one — a
single Rust binary that serves both the WebSocket queues and TURN for calls — and clients can point at
whichever relay they trust, or run their own. If a relay disappears, you switch to another; there is no
directory to take down.

> The Chinese README carries more detailed engineering notes: [README.zh-CN.md](README.zh-CN.md).

---

## Download

**APKs are not stored in this repository.** Each build is 26 MB; keeping history here would bloat every
clone and could never be cleaned up. Releases live on the
[Releases](https://github.com/qiujian444-dev/Veilwire/releases) page.

| File | Purpose |
|---|---|
| `veilwire-<version>-arm64.apk` | Real devices (Android 8.0+, arm64) |
| `veilwire-<version>-x86_64.apk` | Emulators (x86_64) |
| `SHA256SUMS` | Checksums |
| `BUILD-INFO.txt` | Build metadata (commit, toolchain versions, signing certificate) |

Release builds are signed with `CN=Veilwire` (RSA 4096). Certificate SHA-256:

```
4a991f0447b4ebef81b38c26ef937de86b632f3f3e9dd4e1b66e2d929e6eed1a
```

Check it before installing — if it does not match, do not install:

```bash
sha256sum -c SHA256SUMS
apksigner verify --print-certs veilwire-0.13.1-arm64.apk | grep "SHA-256"
```

> ⚠️ Test builds before 0.13.1 were signed with a debug key. They **cannot be upgraded in place** to a
> release-signed build — uninstall first. Chat history lives in app-private storage and is lost on
> uninstall; the identity itself can be restored from the 12-word phrase.

Maintainers cut a release with:

```bash
bash scripts/release.sh 0.15.2            # requires a clean tree and matching versionName
bash scripts/release.sh 0.13.1 --dry-run  # build + checksums only, no git/GitHub
```

---

## Why not just clone Session

Session hardcodes its node list as public IPs. Block those IPs and the whole network dies — that already
happened in Russia. Copying that design means shipping a messenger that cannot connect.

Our trade-off: **"decentralized" does not mean "serverless."** No accounts, no central directory, and any
relay can be replaced by anyone. We accept the fact that offline delivery and audio/video fallback need a
relay.

## What works today

- Encrypted 1:1 text, images, video, files (up to 20 MB) and voice notes
- Voice and video calls over WebRTC, with TURN built into the same relay binary
- Add contacts by QR invite code; a stranger can reply-invite without a new code
- Disappearing messages, message recall, multi-select forward and delete
- Panic wipe behind an arithmetic challenge code
- Safety-number comparison (60 digits) to detect a man in the middle
- Deterministic avatars, dark mode, offline queueing, background delivery via foreground service
- Relay discovery over the Mainline DHT (BEP5) — no relay address to type; opt-in per relay
- Taking a photo from the composer and sending it directly (never written to the system gallery)

## Repository layout

```
docs/          design docs, protocol spec, call signalling, relay deployment guide (Chinese)
rust/
  crates/proto/        protocol frames (identifier-less queues)
  crates/identity/     Ed25519 identity, invite codes, BIP39 phrase
  crates/secure/       end-to-end encryption (Olm, double ratchet)
  crates/dht/          Mainline DHT (BEP5) client: relay announcement + client discovery
  crates/relay/        relay server (persistence + rate limiting) + embedded TURN + load tool
  crates/punch/        UDP hole punching and NAT behaviour probing
  crates/client/       client business layer shared by CLI and Android
  crates/cli/          command-line client
  crates/android-jni/  JNI bridge exposing the client to Kotlin
android/app/     the Android app (Compose + remoteMessaging foreground service)
android/stage0/  early keep-alive demo, superseded by android/app
scripts/         build-android.sh, test-android.sh, release.sh, turn_probe.py, relay_check.py
```

## Contributing / working with an AI assistant

Read **[CLAUDE.md](./CLAUDE.md)** first. It covers the build and test commands, the
directory map, the constraints that are easy to break (conversation keys, relay
routing, call generations, DHT injection defences, error/localisation rules) and
what to run before committing. `AGENTS.md` points at the same file.

Documentation is kept in sync with the code — if a change alters behaviour,
commands or guarantees, update `CLAUDE.md` or the relevant `docs/` page in the
same commit.

## Quick start (Rust core, Windows/Git Bash)

```bash
# A non-ASCII path breaks the linker, so build from an ASCII directory
mkdir -p /c/anonchat-build/src && cp -r rust/. /c/anonchat-build/src/
cd /c/anonchat-build/src && CARGO_TARGET_DIR=/c/anonchat-build/target cargo build --offline
```

```bash
# Hole punching / NAT probing
punch serve --port 7000
punch nat --server <IP>:7000
punch client --server <IP>:7000 --id a --peer b --secs 20

# Relay + load test
relay serve --addr 127.0.0.1:8080
relay load --url ws://127.0.0.1:8080 --clients 20 --secs 300

# End to end: two "devices" exchanging encrypted messages through a real relay
relay serve --addr 127.0.0.1:8080 &
cli demo --relay ws://127.0.0.1:8080
```

### Use it as a chat client (two terminals, persistent identities)

```bash
relay serve --addr 127.0.0.1:8080 &

cli init --dir ./alice --name Alice
cli invite --dir ./alice --relay ws://127.0.0.1:8080
cli init --dir ./bob --name Bob
cli invite --dir ./bob --relay ws://127.0.0.1:8080

cli add --dir ./alice --invite <Bob's invite>
cli add --dir ./bob   --invite <Alice's invite>

cli send --dir ./alice --to Bob --text "hello"
cli recv --dir ./bob
```

Identity, ratchet state and the dedup table are persisted under `--dir`, so every command is a fresh
process and a conversation survives restarts.

> `cargo test` only builds the test harness — it does not refresh `target/debug/relay.exe`. Rebuild
> explicitly after changing relay sources. Add `--offline` when the network is unreliable.

## Constraints that are not negotiable

| Constraint | Why |
|---|---|
| Foreground service type `remoteMessaging`, **never `dataSync`** | Android 15 caps `dataSync` at 6 hours per 24h — it crashes past that |
| For calls, declare `phoneCall\|microphone\|camera` and **acquire permissions before `startForeground`** | Wrong order throws `SecurityException` |
| No FCM or vendor push | A push provider necessarily learns that a device has a message — that contradicts anonymity |
| Reconnect with exponential backoff, **never periodic polling** | Under Doze, polling gets a 9-minute window; maintenance windows stretch to 6 hours |
| Battery-optimisation whitelist is **required**, not an optimisation | Doze suspends all app networking; foreground services are not exempt |
| Do not copy code from `libsession` / `session-android` | GPLv3 is viral; read the architecture only |

## Feature notes

- **Voice notes** — hold to talk, release to send, swipe up to cancel; max 60 s, recordings under 0.7 s are
  discarded. The level meter above the input bar shows the mic is actually capturing.
- **Images / video** — system photo picker (no storage permission). Images are downscaled to 1600 px /
  JPEG 80 unless already small enough, in which case they are sent byte-identical. Bubbles keep the
  aspect ratio; tap for an in-app full-screen viewer. Video is never transcoded — the 20 MB limit is
  enforced with a clear message instead.
- **Media never lands in Downloads** — voice, photos and video are message content, not files the user
  asked for. Long-press a bubble to save explicitly.
- **Panic wipe** — the challenge code is derived per contact from
  `SHA-256("panic" + sorted identity keys)`, so both sides have it the moment they add each other.
  Zero configuration, unique per pair, symmetric.
- **Sending failures are visible** — failed messages stay in the list with a red mark and one tap to retry.

## Build the Android APK

```bash
bash scripts/build-android.sh          # debug
bash scripts/build-android.sh release  # release; uses the release key if configured, else the debug key
```

Artifacts are split per ABI (WebRTC's libjingle is 11–12 MB per architecture):

| File | Purpose | Size |
|---|---|---|
| `dist/anonchat-arm64-release.apk` | **Real devices** | 26 MB |
| `build/outputs/apk/release/app-x86_64-release.apk` | Emulators | 25 MB |
| `build/outputs/apk/debug/app-x86_64-debug.apk` | Debuggable emulator build (`run-as` works) | 33 MB |

Toolchain versions matter — a different rustc or NDK produces different binaries:

| Component | Version |
|---|---|
| JDK | 17 |
| Android SDK | platform 35 / build-tools 35.0.0 / NDK 27.0.12077973 |
| Gradle | 8.9 |
| Rust | 1.98.1 with target `aarch64-linux-android` (**msvc toolchain**) |

`JAVA_HOME`, `ANDROID_HOME`, `GRADLE_BIN` and `BUILD_DIR` can all be overridden by environment variables.

Traps the scripts already handle:

1. **Non-ASCII paths** break the Rust linker — sources are copied to an ASCII build dir; Gradle needs
   `android.overridePathCheck=true`.
2. **Wrong rustup toolchain** — the default is gnu, the build uses msvc; pass `--toolchain` explicitly.
3. **Slow registries** — cargo uses the USTC mirror, Gradle uses Tencent/Aliyun mirrors.
4. **Unit tests cannot run from a non-ASCII path** — Gradle's test worker fails to resolve the classpath
   (`ClassNotFoundException` while the classes are clearly under `build/tmp/kotlin-classes`). `assemble`
   is unaffected; only `test` is. Use `bash scripts/test-android.sh`, which copies to an ASCII directory
   first (77 test cases).

## Build and verify the APK yourself

**Why this section exists**: the whole security story ends at *"is the binary on my phone built from this
source?"* We operate no server, but the APK is still a binary blob. If all you can do is trust our APK,
encryption protects you from the relay — not from the client.

So the safest option is not verification, it is **building it yourself**.

```bash
git clone https://github.com/qiujian444-dev/Veilwire.git
cd veilwire

export JAVA_HOME=/path/to/jdk-17
export ANDROID_HOME=/path/to/android-sdk
export GRADLE_BIN=/path/to/gradle-8.9/bin/gradle

bash scripts/build-android.sh release
# → dist/anonchat-arm64-release.apk
```

Metadata first:

```bash
aapt2 dump badging dist/anonchat-arm64-release.apk | head -3     # versionCode / versionName
apksigner verify --print-certs dist/anonchat-arm64-release.apk   # certificate fingerprint
```

> **How to read the signature**: release builds are signed with the `CN=Veilwire` private key, and the
> certificate SHA-256 is published in the Download section above. So this check *is* meaningful now: a
> mismatching fingerprint means a forged build. Your own build will always have a different signature
> (you do not have that key) — that is expected, not a problem.

What actually matters is comparing the code. An APK is a zip; the code lives in `classes*.dex`,
`lib/<abi>/*.so`, `resources.arsc` and `assets/`. Comparing whole-file SHA-256 usually fails (signing
block, zip alignment, timestamps), so compare entry by entry and skip `META-INF/`:

```bash
python3 - <<'EOF'
import hashlib, zipfile

def entries(path):
    with zipfile.ZipFile(path) as z:
        return {
            i.filename: hashlib.sha256(z.read(i)).hexdigest()
            for i in z.infolist()
            if not i.filename.startswith("META-INF/") and not i.filename.endswith("/")
        }

a, b = entries("official.apk"), entries("mine.apk")
diff = [k for k in sorted(set(a) | set(b)) if a.get(k) != b.get(k)]
print(f"entries: official={len(a)} mine={len(b)}")
print("mismatches:", diff if diff else "none")
EOF
```

If every entry matches, the two APKs contain the same code; only signing and packaging timestamps differ.

### What can be verified today

| Object | Comparable across machines? | Notes |
|---|---|---|
| `classes*.dex` | Mostly yes | D8 output is stable for the same JDK/AGP/Gradle; version drift can change it |
| `resources.arsc`, `assets/` | Mostly yes | Depends on aapt2 version |
| `lib/<abi>/libanonchat.so` | **No** | Rust cross-compile output; a different rustc, NDK or build path changes the hash — not a backdoor |
| Whole APK | No | Signing block and zip timestamps always differ |

**If your `classes.dex` differs from ours, do not install it** — that means source and binary disagree.
A differing `.so` is normal and proves nothing either way.

### Still missing (PRs welcome)

- [x] Ship with a real signing key (`CN=Veilwire`, RSA 4096); certificate SHA-256 published above
- [ ] Reproducible builds: pin rustc in `rust-toolchain.toml`, `--remap-path-prefix` to strip build paths,
      fix `SOURCE_DATE_EPOCH` and zip timestamps so `.so` files reproduce byte for byte
- [ ] Attach build metadata to every release: source commit, toolchain versions, per-entry hash manifest

## Known limitations

| Limitation | Impact |
|---|---|
| One-time keys work once | Reinstalling on one side invalidates the old invite and old messages; re-add each other |
| Calls fail across some carriers without TURN | Signalling works, media does not — run the in-app connectivity test first |
| Network switch during a call | ICE restart is supported; there is also a manual "reconnect" button |
| Files capped at 20 MB, no resume | Sent in chunks over the message channel (~854 messages); an interrupted transfer restarts |
| Voice notes cannot be exported | They live in app-private storage and are gone on uninstall (deliberate) |
| No group chat, no multi-device sync | Out of scope for now |

## License

**AGPL-3.0-only** — GNU Affero General Public License, version 3 only, *not* "or any later version".
Full text in [LICENSE](LICENSE).

Copyright (C) 2026 Veilwire contributors

> Why AGPL rather than MIT/GPL: other people will run this code on servers. GPL's obligations attach to
> *distribution*, so running a modified relay as a service triggers nothing. AGPL §13 closes that gap by
> requiring operators of modified versions to offer the source to their users. The relay is the only
> component that can see metadata — it has to be auditable.
