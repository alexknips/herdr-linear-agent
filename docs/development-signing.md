# Signing local macOS development builds

This setup is for developers who repeatedly rebuild and run the plugin on
macOS with real Linear credentials. It is optional for development and avoids
repeated Keychain authorization when the executable changes.

Users installing published releases do not need to create a certificate,
install Xcode, or run `build-dev`. They still authorize the plugin's normal
Keychain access when macOS asks. Lint and tests use no production credentials
and need no signing identity. Linux uses Secret Service instead of this setup.

## Why a fixed Apple identity helps

The plugin stores its OAuth credentials in the user's file-based Keychain.
Its access controls can check both the executable's designated requirement
(DR) and a partition list. In the previous local test, a self-signed identity
kept the DR stable, but the partition list still identified each build by its
changing code hash. Choosing **Always Allow** added that build's hash, and the
next rebuild prompted again.

An Apple Development signature supplies an Apple-issued Team ID. In the
[2026-10-03 verification](verification.md#local-macos-signing-checked-on-2026-10-03),
that identity retained access across changed build hashes after the initial
permissions were granted. Keep the signing identity and executable identifier
consistent, and verify this behavior on your own Mac before relying on it.

## 1. Create an Apple Development certificate

Install [Xcode from the Mac App Store](https://developer.apple.com/xcode/resources/).
The standalone Command Line Tools provide build tools but do not provide the
Xcode account interface used below. You do not need to open this Rust project
in Xcode.

1. Open Xcode and complete its initial setup.
2. Open **Xcode > Settings > Accounts**.
3. Add your Apple Account and complete sign-in.
4. Select the account, then the team you want to use for local development.
   An account without a paid membership has a **Personal Team**.
5. Click **Manage Certificates**.
6. Click **+**, choose **Apple Development**, and wait for creation to finish.

A free Personal Team manages certificates through Xcode; a paid Apple Developer
Program membership is not required for this local setup. Apple documents the
[Personal Team workflow](https://developer.apple.com/help/account/basics/about-your-developer-account)
and [certificate creation in Xcode](https://developer.apple.com/documentation/xcode/sharing-your-teams-signing-certificates).
Paid members with the appropriate permissions can also create certificates
through their developer account using a certificate signing request.

Xcode creates the certificate and corresponding private key in the local
Keychain. After that, Cargo and `codesign` can build and sign the plugin from
the terminal; Xcode does not need to remain open. Keep the private key on the
Mac. Do not commit or upload it, or put it in the plugin configuration.

## 2. Confirm that the identity is valid

Run:

```sh
security find-identity -v -p codesigning
```

Expect an entry similar to:

```text
1) YOUR_CERTIFICATE_SHA1 "Apple Development: YOUR_ACCOUNT (IDENTITY_SUFFIX)"
   1 valid identities found
```

Copy either the full quoted name or its SHA-1 hash. The suffix in the display
name is not necessarily the Team ID reported by the signed executable.

If the command reports `0 valid identities found`, remove `-v` to include
matching identities that failed validation:

```sh
security find-identity -p codesigning
```

- If no matching identity appears, open **Keychain Access > login > My
  Certificates**. Expand the Apple Development certificate and confirm that
  it has a private key. Also check the status in Xcode's **Manage Certificates**.
  A certificate without its corresponding private key cannot sign code.
- If a matching identity appears but is not valid, inspect the certificate's
  validity, issuer, and trust status in Keychain Access. Check for expiration,
  a missing intermediate certificate, or a trust override.

In our test, the certificate and key existed, but the WWDR G3 intermediate
certificate was missing. Apple lists G3 as the issuer for Apple Development
certificates in its [WWDR documentation](https://developer.apple.com/help/account/certificates/wwdr-intermediate-certificates).
If your certificate's issuer is WWDR G3, obtain **Worldwide Developer Relations
- G3** from [Apple PKI](https://www.apple.com/certificateauthority/) and import
it into the login Keychain. The equivalent terminal commands are:

```sh
curl -fsS https://www.apple.com/certificateauthority/AppleWWDRCAG3.cer \
  -o /tmp/AppleWWDRCAG3.cer
security add-certificates -k "$HOME/Library/Keychains/login.keychain-db" \
  /tmp/AppleWWDRCAG3.cer
security find-identity -v -p codesigning
```

This adds Apple's intermediate certificate; it does not change certificate
trust overrides or credential access permissions. Use the default trust
settings rather than marking an invalid certificate **Always Trust**.

## 3. Build and sign the executable

In the development checkout, set the identity for your shell session:

```sh
export HLA_SIGN_IDENTITY="Apple Development: YOUR_ACCOUNT (IDENTITY_SUFFIX)"
mise run build-dev
```

Using the SHA-1 hash instead of the name selects one identity when names are
ambiguous. `build-dev` requires this variable and fails if it is unset.

The task builds the release binary, signs it with the fixed identifier
`dev.herdr-linear-agent`, and verifies the signature. Its output is
`target/release/herdr-linear-agent`, matching the plugin manifest. Run it in
the checkout you use for development; it does not install the result into
another Herdr plugin checkout or restart a ticker.

Use `build-dev` after source changes when running against real credentials.
The ordinary `mise run build` creates a debug binary without this local
signature. `scripts/install.sh` downloads a published release or builds from
source and does not apply your development signature.

## 4. Grant the initial permissions

There are two separate permissions, so the initial setup can show multiple
dialogs:

| Requester | Protected item | Initial action |
| --- | --- | --- |
| `codesign` | The Apple Development private key | Enter the Keychain password and choose **Always Allow** for `codesign`. |
| `herdr-linear-agent` | The stored Linear credential | Choose **Always Allow** for the signed plugin. There is one credential item per configured workspace. |

After signing, exercise the credential read using:

```sh
target/release/herdr-linear-agent action doctor
```

Doctor checks credentials and reads Linear identity and rate-budget information
without printing tokens or starting a ticker. Confirm that every configured
workspace reports a stored credential and a known budget. If a ticker from a
different build is running, doctor can report a version mismatch separately;
that is not a credential-access failure.

A locked Keychain may ask to be unlocked separately. If signing keeps asking
after **Always Allow**, investigate the signing key's access controls and
partition list before retrying. If only the plugin keeps asking, check the
credential item's access controls and the actual executable's signature.
Do not grant all applications access to work around either case.

## 5. Verify a changed build without approving another dialog

Successful access immediately after clicking **Always Allow** only verifies
that build. To check persistence, first record its signature details:

```sh
codesign -dv --verbose=4 target/release/herdr-linear-agent 2>&1
codesign -d -r- target/release/herdr-linear-agent
```

Then rebuild. Touching `build.rs` forces a new build ID without editing its
contents:

```sh
touch build.rs
mise run build-dev
codesign -dv --verbose=4 target/release/herdr-linear-agent 2>&1
codesign -d -r- target/release/herdr-linear-agent
target/release/herdr-linear-agent action doctor
```

Confirm that `CDHash` changed, `TeamIdentifier` and the designated requirement
stayed the same, and signing and all credential checks completed. During this
verification, leave any authorization dialog unanswered: clicking **Allow**
or **Always Allow** would hide the failure being tested. A successful result
requires no additional approval. Repeat the rebuild once more to confirm it.

The local verification covered signing and doctor reads across rebuilds. It
did not cover reboot or certificate renewal. When the certificate expires or
the identity changes, update `HLA_SIGN_IDENTITY` and repeat this check.
