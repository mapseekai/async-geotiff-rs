#!/bin/sh
set -eu
: "${DEMO_COG_URL:?DEMO_COG_URL must be set}"
export ASYNC_GEOTIFF_BIND="${ASYNC_GEOTIFF_BIND:-0.0.0.0:8080}"
exec /usr/local/bin/serve "${DEMO_COG_URL}"
