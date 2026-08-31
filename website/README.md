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
