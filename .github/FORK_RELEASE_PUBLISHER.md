# Fork release publisher setup

Branch-merge releases and human-created `local-features-v*` recovery tags use a
repository-scoped GitHub App token to create or update the release and publish
assets. The App-created tag event is skipped after identifying its exact
publisher; the original branch-merge run finishes the release.

## Configure the publisher App

1. In the `epsalmond` GitHub account, open **Settings → Developer settings →
   GitHub Apps → New GitHub App**. Give it a release-publisher name; a
   homepage URL is informational only. Disable webhooks and leave callback
   URLs empty.
2. Grant only **Contents: read and write** and **Workflows: read and write**
   repository permissions. Release API operations need Workflows permission
   when the target commit changes `.github/workflows/`; the Actions `GITHUB_TOKEN`
   cannot receive that permission ([GitHub release API](https://docs.github.com/en/rest/releases/releases)).
   Create the App, then use **Generate a private key** on its settings page.
3. Open **Install App** and install it on **only** the `codex` repository
   owned by `epsalmond`.
4. In `epsalmond/codex`, open **Settings → Secrets and variables → Actions**
   and add these repository-level values:

   - Repository variable `RELEASE_APP_CLIENT_ID`: the App's **Client ID**
     from its settings page.
   - Repository variable `RELEASE_APP_BOT_LOGIN`: its exact bot login,
     `<app-slug>[bot]` (the App slug followed by `[bot]`).
   - Repository secret `RELEASE_APP_PRIVATE_KEY`: the generated private key,
     including its PEM header and footer.

The workflow checks that these values are present before validation and builds
on both branch-merge and tag-recovery runs. It compares the configured bot
login with the slug reported by the App token action before publishing. It
never prints the private key or token.

## Verify a release

After configuration, merge the reviewed fix into `eric/local-features` through
the normal two-parent PR merge. Confirm its branch-merge release run creates a
tag resolving to that exact merge commit and publishes all six release assets.
The five binaries/archives must verify against the sixth asset, `SHA256SUMS`.
The App's tag push starts a second workflow event; that event should skip at
`prepare`, while the branch run continues. A human tag push still runs the
recovery path, using the same App token to upload or repair its release assets.
