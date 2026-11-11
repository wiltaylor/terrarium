# The release public key

`cosign.pub` in this directory is the public key of the release key pair
(docs/release.md, "How to verify a release"). The image copies the
directory to `/opt/terrarium/release/`. The updater's default image check
(`TERRARIUM_UPDATE_VERIFY=cosign`) verifies a new release image with
`/opt/terrarium/release/cosign.pub` (`TERRARIUM_UPDATE_COSIGN_KEY`).

The private key is the Terrarium secret `release-cosign` (fields
`private_key`, the encrypted PEM base64-encoded in one line, and
`password`; made with cosign 3.1.4). It never enters this repo, an image, or a
log. `just release::publish` signs with it and then verifies its own
signatures with this `cosign.pub`, so a release with a key that does not
match this file stops before the GitHub release.

Without `cosign.pub`, `just release::publish` refuses to start, and the
updater's cosign check fails closed ("the cosign public key ... is not a
file").

To change the key pair: commit the new `cosign.pub` here, release it under
the old key, and only then replace the secret. A daemon verifies each
release with the key of the image that it runs, so the release that
brings the new key must still be signed with the old one.
