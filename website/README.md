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
`public/` as the `async-geotiff-docs` workflow artifact. The repository is
private and its current GitHub plan does not support Pages, so CI validates and
packages the site but does not deploy it. Publishing requires making the
repository public, enabling private-repository Pages on the organization, or
deploying the artifact to another host.
