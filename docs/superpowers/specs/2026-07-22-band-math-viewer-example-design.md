# Band-math viewer example design

## Goal

Make the existing MapLibre, OpenLayers, and Leaflet demo pages demonstrate the public arithmetic `expression` styling API with both a reusable NDVI preset and arbitrary user input.

## Scope

Each page retains its current dataset, source-mode, stretch, colour map, output, and cache-invalidation controls. It adds:

- an `Expression` text input;
- an `NDVI` action that applies `(b5-b4)/(b5+b4)`;
- expression-aware request construction.

No server, library API, parser, rendering, dependency, or URL-contract changes are required.

## User interaction

1. A visitor may enter an arithmetic expression such as `(b5-b4)/(b5+b4)` and choose the existing styling controls.
2. Applying that form sends `expression=<value>` instead of `bidx`.
3. The NDVI action sets the expression, selects custom stretching with `rescale=-1,1` and `colormap_name=viridis`, clears the bands input, and applies the current page's regular refresh path.
4. Clearing Expression restores normal band selection; the existing default `bidx=1,2,3` is sent again.
5. A permalink containing `expression` initializes the input and reproduces the tile request.

`URLSearchParams` remains the sole query serializer, so the `+` in the NDVI expression is encoded as `%2B` automatically.

## Request rules

Expression mode is active exactly when the trimmed Expression input is non-empty.

| Mode | Send | Omit |
|---|---|---|
| Expression | `expression`, the selected stretch mode or one explicit `rescale` range, optional `colormap_name`, remaining compatible controls | all `bidx` values |
| Normal | existing `bidx`, stretching and colour-map behavior | `expression` |

The pages do not validate expression grammar or source-band availability. The existing server remains the single validation authority and keeps returning HTTP 400 for invalid requests.

## Page-specific refresh behavior

- MapLibre increments its style version, rebuilds its raster source/layer, syncs status and permalink.
- OpenLayers rebuilds its XYZ source/layer through its existing Apply handler.
- Leaflet rebuilds its tile layer through its existing Apply handler.

The NDVI action invokes the same apply path as a normal form submission; it does not add a second refresh mechanism.

## Verification

Browser smoke tests cover each viewer:

- expression from the URL populates the input and produces a tile URL with `expression` and no `bidx`;
- NDVI produces `(b5-b4)/(b5%2Bb4)`, one `rescale=-1,1`, and `colormap_name=viridis` while omitting `bidx`;
- clearing Expression resumes the normal `bidx` request;
- existing source and format controls still produce tile requests after applying a style.

Manual visual verification confirms each viewer still refreshes tiles after applying NDVI and the URL status/permalink reflects the expression request.
