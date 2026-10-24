# async-geotiff documentation site

This directory contains the bilingual project documentation built with
[OINK](https://github.com/pgsty/oink) and Hugo Extended.

## Local preview

Requirements:

- Go 1.27 or newer
- Hugo Extended 0.165.0 or newer

```bash
cd website
hugo server
```

Open <http://localhost:1313/>. The Chinese site is available under `/zh/`.

## Production check

```bash
cd website
GOWORK=off HUGO_MODULE_WORKSPACE=off \
  hugo --cleanDestinationDir --gc --minify --environment production \
  --printPathWarnings --panicOnWarning
```

The site pins OINK in `go.mod` and `go.sum`. Do not commit generated
`public/`, `resources/`, `.hugo_cache/`, or `.hugo_build.lock` files.

## Continuous integration

`.github/workflows/docs.yml` runs the same strict production build and uploads
`public/` as the `async-geotiff-docs` workflow artifact. The repository is public;
production documentation is hosted on Cloudflare Pages.

## Cloudflare Pages releases

`.github/workflows/cloudflare-pages.yml` deploys `website/public` to
<https://async-geotiff-rs.pages.dev/> after the `Release` workflow succeeds,
or when a GitHub Release is published manually. The completion trigger also
covers releases created with `GITHUB_TOKEN`, whose release event does not
trigger another workflow. The deployed source is the successful release's
exact commit. Manual deployment remains available through `workflow_dispatch`.
Before the first release, add the repository secret `CLOUDFLARE_API_TOKEN` with
the minimum Cloudflare permission **Account → Cloudflare Pages → Edit**. The
workflow passes the account and project IDs explicitly and never stores the
token in the repository.

See the bilingual [release guide](content/docs/reference/releases.md) for
package verification and crates.io publication.
