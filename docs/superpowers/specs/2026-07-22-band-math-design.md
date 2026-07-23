# Band-math expression API design

## Goal

Make the arithmetic `expression` language the only public way to calculate derived raster bands. Remove the built-in index API, including NDVI and SAVI, without changing normal RGB rendering.

## Scope

- Keep `TileStyle::expression: Option<String>` and the `expression` query parameter.
- Keep the existing expression grammar: one-indexed `bN` references, finite numeric literals, parentheses, unary negation, and `+`, `-`, `*`, `/`.
- Remove the `Index` enum, its built-in index variants, and the conversion layer that emits expressions.
- Document the expression grammar, value semantics, and failure modes in the public library documentation.
- Preserve the non-expression render path and its fast paths.

Out of scope:

- Named aliases such as `ndvi`.
- New functions such as `abs`, `min`, `max`, or `clamp`.
- Comparisons, Boolean operators, or conditional expressions.
- Global statistics for calculated expression outputs.
- Changes to non-expression stretch defaults.

## Public API

The public calculation interface is the existing string field:

```rust
let style = TileStyle {
    expression: Some("(b5-b4)/(b5+b4)".into()),
    ..Default::default()
};
```

`bN` is a one-indexed source-band reference. An expression produces exactly one derived band. It remains mutually exclusive with `bidx` and `color_formula`.

No typed `Index`, named-index function, alias table, or expression builder is exposed.

## Processing flow

1. A Rust caller sets `TileStyle.expression`, or an HTTP caller supplies `expression`.
2. Style validation rejects incompatible fields before tile work starts.
3. The expression parser validates syntax and compiles it to the existing internal RPN representation.
4. The renderer evaluates that RPN once per output pixel over the referenced source bands.
5. The result follows the existing single-band rendering path and output encoding.
6. Requests without an expression retain their existing RGB and typed-LUT fast paths.

## Value and error semantics

- Empty, malformed, overlong, over-tokenized, and over-nested expressions return `AgError::Query`.
- Referencing a missing source band returns `AgError::Query`.
- An expression remains incompatible with `bidx` and `color_formula`.
- If any referenced input is nodata or non-finite, the output pixel is transparent.
- Division by zero or any non-finite calculated result produces a transparent output pixel; it is never silently replaced with zero.
- A caller-provided `rescale` controls the calculated output range.
- Without `rescale`, the existing per-tile expression range computation remains unchanged. This design does not add global expression statistics.

## Removal

Remove the `Index` enum, all built-in index variants, and `to_expression()`. Replace their tests with generic expression tests that use equivalent expression strings, for example `(b5-b4)/(b5+b4)`.

## Verification

Unit tests must cover:

- arithmetic precedence, parentheses, unary negation, numeric literals, and one-indexed band references;
- a generic expression equivalent to the removed NDVI helper;
- out-of-range band references;
- expression incompatibility with `bidx` and `color_formula`;
- nodata, non-finite operands, and division by zero becoming transparent;
- parser byte, token, and nesting limits.

An end-to-end tile test must confirm that an expression renders through the derived single-band path. A regression test must also confirm that a non-expression U8 RGB request remains on its existing output path.

## Compatibility

This is a clean API cutover: callers must use expression strings instead of built-in index types. No aliases, deprecated re-exports, or compatibility shims remain.
