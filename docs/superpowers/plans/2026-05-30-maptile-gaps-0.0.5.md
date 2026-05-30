# maptile Integration Gaps — 0.0.5 (band-math `expression` + `Index`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Close maptile gap #1 — band-math indices. Add a generic arithmetic `expression` (NDVI-style `(b5-b4)/(b5+b4)`) evaluated per-pixel into a synthetic single band, plus a typed `Index` enum (Ndvi/Ndwi/Ndbi/Evi/Savi) that lowers to expression strings. Release 0.0.5.

**Architecture:** Hand-rolled recursive-descent parser → RPN `Vec<Token>`, compiled once and stored in `CompiledTileStyle` (cached via `compile_cached`). At render time, when an expression is present, evaluate it per-pixel over the (already unscaled) source bands → one synthetic `Array2<f64>` + a validity mask ("all referenced bands valid AND result finite"), then route through the existing single-band gradient/colormap path. Auto-stretch for expression output comes from the synthetic band's own min/max (NEVER `dataset_stats`, which are per-source-band raw DN and meaningless for an index).

**Tech Stack:** Rust, `ndarray`. No new deps (hand-rolled parser, not nom).

**Source spec:** `docs/superpowers/specs/2026-05-30-maptile-integration-gaps-design.md`.

## Design decisions (advisor-vetted — bake these in, they prevent silent-wrong bugs)

1. **Auto-stretch trap (CRITICAL, same class as the 0.0.4 nodata/footprint bugs):** `dataset_stats` are per-source-band raw DN. For NDVI∈[-1,1] they are meaningless. When `expression` is set and NO explicit `rescale`, the stretch MUST come from the synthetic band's own finite min/max — skip the stats path entirely. Document that callers should pass `rescale=-1,1` for normalized indices.
2. **One validity mask:** the expression mask = (every REFERENCED band — only those named by `b<N>`, not all bands — is valid at that pixel) AND (the result `is_finite()`). The div-by-zero/NaN → transparent rule is folded into the finiteness check. Compute once.
3. **Compiled representation:** parse → RPN `Vec<Token>` stored in `CompiledTileStyle`. `Token` and `CompiledTileStyle` must derive `Clone + Debug`. Validate band-reference range ONCE before the pixel loop (`max band ref < bands` → else `AgError::query`); never panic per-pixel.
4. **Parser:** hand-rolled recursive descent matching the grammar. Reject: empty expr, trailing garbage, unknown chars, `b0` (bands are 1-indexed), bare `b` with no digit, unbalanced parens — all → `AgError::query("expression", …)`.
5. **`Index` sugar:** `to_expression() -> String` that flows through the SAME parse→compile→eval path. No second eval path.
6. **Mutual exclusivity (in `compile`):** `expression` XOR `bidx` (error if both). `expression` + `color_formula` → error (color_formula is RGB-oriented; expression yields one band). `expression` is independent of colormap/colormap_name (an expression band is colormapped like any single band).
7. **Scope (v1):** single-band output only; arithmetic only (`+ - * /`, unary minus, parens, float literals, `b<N>`); NO functions. Deferred items stay deferred.

**Testing rule (from the recurring bug pattern):** for every pixel transform, TEST THE MASKED/NODATA/STRETCH PIXEL, not just the happy-path value.

## Verified code facts
- `CompiledTileStyle` (`src/render/pipeline.rs:26-35`): `#[derive(Debug, Clone)]`, fields `composite, rescale, formula: Vec<Op>, bidx: Vec<usize>, nodata, tile_size, resampling, auto_rgb`.
- `render` (`pipeline.rs:37-110`): fast path → `to_f64()` → `bidx` band selection → `compute_valid_mask` → `render_single_band`/`render_three_band` → `encode_png`. `render_single_band(band: &Array2<f64>, valid: Option<&Array2<bool>>, style)` (`:260`) does gradient: `t = ((v-lo)/span)*255` via `comp.vmin[0]/vmax[0]` → LUT.
- `compute_valid_mask(image, bands: &[Array2<f64>], nodata, h, w) -> Option<Array2<bool>>` (`:210`): masks pixels where any band hits nodata / mask=false.
- `compile` (`src/style/params.rs:162`): builds `CompiledTileStyle`; has the mutual-exclusion guard for colormap/colormap_name already.
- `TileStyle` (`params.rs:13`, `#[non_exhaustive]`): has `bidx, rescale, …, colormap_name, colormap, color_formula, resampling, reproject, tilesize, unscale`. `apply_param` match; `Default`.
- `effective_compiled` (`src/tiler.rs:88-137`): early-returns if `!style.rescale.is_empty()`; else derives `ranges` from `dataset_stats` and sets `next.rescale` + `next.composite.vmin/vmax`. Has access to `image: &ImageData`. (0.0.4 added an `unscale_ranges` shadow here.)
- `tile` (`tiler.rs:227`): `warp_tile` → unscale (0.0.4) → `compile_cached` → `effective_compiled` → `render`. Expression sees the post-unscale image automatically (correct ordering).
- The crate's existing parser (`src/render/color_formula.rs`) uses nom — we deliberately do NOT for this grammar.

---

## File Structure

| File | Change | Responsibility |
|------|--------|----------------|
| `src/render/expression.rs` | Create | `Token`, `parse_expression`, `max_band_ref`, `eval_rpn`; `Index` enum + `to_expression` |
| `src/render/mod.rs` | Modify | `pub mod expression;` |
| `src/render/pipeline.rs` | Modify | `CompiledTileStyle.expression: Vec<Token>`; expression branch in `render` |
| `src/style/params.rs` | Modify | `TileStyle.expression` field + parse arm + compile (mutual excl. + store RPN) |
| `src/tiler.rs` | Modify | `effective_compiled`: expression → rescale from synthetic band min/max |
| `Cargo.toml`, `CHANGELOG.md` | Modify | Release 0.0.5 |

---

## Task A1: Expression evaluator (`src/render/expression.rs`)

**Files:** Create `src/render/expression.rs`; modify `src/render/mod.rs` (`pub mod expression;`).

Grammar (recursive descent, 1-indexed `b<N>` in source, stored 0-indexed):
```
expr   := term (('+'|'-') term)*
term   := factor (('*'|'/') factor)*
factor := number | band | '(' expr ')' | '-' factor
band   := 'b' DIGIT+        // b1 → Band(0)
number := FLOAT
```

- [ ] **Step 1: tests first.** Create `src/render/expression.rs` with the `Token` enum, stub `pub fn parse_expression`, `pub fn max_band_ref`, `pub fn eval_rpn` as `todo!()`, and this test module:

```rust
//! Arithmetic band-math expression: parse `(b5-b4)/(b5+b4)` → RPN, eval per-pixel.
//! Grammar is arithmetic-only: + - * /, unary minus, parens, float literals, b<N>.

use crate::error::{AgError, AgResult};

/// RPN token. `Band(i)` is 0-indexed (source `b1` → `Band(0)`).
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Num(f64),
    Band(usize),
    Add,
    Sub,
    Mul,
    Div,
    Neg,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpn(s: &str) -> Vec<Token> {
        parse_expression(s).unwrap()
    }

    #[test]
    fn ndvi_value() {
        let t = rpn("(b5-b4)/(b5+b4)");
        // bands vec is 0-indexed: b5 → bands[4], b4 → bands[3].
        let mut bands = vec![0.0; 5];
        bands[4] = 0.5; // b5 = NIR
        bands[3] = 0.1; // b4 = RED
        let v = eval_rpn(&t, &bands);
        assert!((v - 0.6666667).abs() < 1e-5, "ndvi={v}");
    }

    #[test]
    fn precedence_and_parens() {
        // 1 + 2*3 = 7 ; (1+2)*3 = 9
        assert_eq!(eval_rpn(&rpn("1+2*3"), &[]), 7.0);
        assert_eq!(eval_rpn(&rpn("(1+2)*3"), &[]), 9.0);
    }

    #[test]
    fn unary_minus() {
        assert_eq!(eval_rpn(&rpn("-b1+3"), &[2.0]), 1.0);
    }

    #[test]
    fn div_by_zero_is_nonfinite() {
        let v = eval_rpn(&rpn("b1/b2"), &[1.0, 0.0]);
        assert!(!v.is_finite(), "expected inf/nan, got {v}");
    }

    #[test]
    fn max_band_ref_reports_highest() {
        assert_eq!(max_band_ref(&rpn("(b5-b4)/(b5+b4)")), Some(4)); // 0-indexed
        assert_eq!(max_band_ref(&rpn("1+2")), None);
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(parse_expression("   "), Err(AgError::Query { .. })));
    }

    #[test]
    fn rejects_band_zero() {
        assert!(matches!(parse_expression("b0+1"), Err(AgError::Query { .. })));
    }

    #[test]
    fn rejects_bare_b() {
        assert!(matches!(parse_expression("b+1"), Err(AgError::Query { .. })));
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(matches!(parse_expression("b1+1 xyz"), Err(AgError::Query { .. })));
        assert!(matches!(parse_expression("b1)"), Err(AgError::Query { .. })));
    }

    #[test]
    fn rejects_unbalanced_parens() {
        assert!(matches!(parse_expression("(b1+1"), Err(AgError::Query { .. })));
    }
}
```

- [ ] **Step 2: run → fail** (`cargo test --lib expression` → todo!/compile error).

- [ ] **Step 3: implement.** Add above the test module. A recursive-descent parser that EMITS RPN, plus eval and max_band_ref:

```rust
pub fn parse_expression(input: &str) -> AgResult<Vec<Token>> {
    let toks = lex(input)?;
    let mut p = Parser { toks: &toks, pos: 0, out: Vec::new() };
    p.expr()?;
    if p.pos != p.toks.len() {
        return Err(AgError::query("expression", "unexpected trailing input"));
    }
    if p.out.is_empty() {
        return Err(AgError::query("expression", "empty expression"));
    }
    Ok(p.out)
}

/// Highest 0-indexed band referenced, or None if no band refs.
pub fn max_band_ref(rpn: &[Token]) -> Option<usize> {
    rpn.iter().filter_map(|t| match t {
        Token::Band(i) => Some(*i),
        _ => None,
    }).max()
}

/// Evaluate RPN given per-pixel band values (`bands[i]` = value of source band i+1).
/// Caller must validate `max_band_ref < bands.len()` once before the pixel loop;
/// here an out-of-range band defensively yields NaN (never panics).
pub fn eval_rpn(rpn: &[Token], bands: &[f64]) -> f64 {
    let mut stack: Vec<f64> = Vec::with_capacity(8);
    for t in rpn {
        match t {
            Token::Num(n) => stack.push(*n),
            Token::Band(i) => stack.push(bands.get(*i).copied().unwrap_or(f64::NAN)),
            Token::Neg => {
                let a = stack.pop().unwrap_or(f64::NAN);
                stack.push(-a);
            }
            op => {
                let b = stack.pop().unwrap_or(f64::NAN);
                let a = stack.pop().unwrap_or(f64::NAN);
                stack.push(match op {
                    Token::Add => a + b,
                    Token::Sub => a - b,
                    Token::Mul => a * b,
                    Token::Div => a / b,
                    _ => f64::NAN,
                });
            }
        }
    }
    stack.pop().unwrap_or(f64::NAN)
}

// ---- lexer ----
#[derive(Debug, Clone, PartialEq)]
enum Lexeme {
    Num(f64),
    Band(usize),
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
}

fn lex(s: &str) -> AgResult<Vec<Lexeme>> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '+' => { out.push(Lexeme::Plus); i += 1; }
            '-' => { out.push(Lexeme::Minus); i += 1; }
            '*' => { out.push(Lexeme::Star); i += 1; }
            '/' => { out.push(Lexeme::Slash); i += 1; }
            '(' => { out.push(Lexeme::LParen); i += 1; }
            ')' => { out.push(Lexeme::RParen); i += 1; }
            'b' | 'B' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j].is_ascii_digit() { j += 1; }
                if j == start {
                    return Err(AgError::query("expression", "band ref 'b' needs a number"));
                }
                let n: usize = s[start..j].parse()
                    .map_err(|_| AgError::query("expression", "invalid band number"))?;
                if n == 0 {
                    return Err(AgError::query("expression", "bands are 1-indexed (b1 is first)"));
                }
                out.push(Lexeme::Band(n - 1));
                i = j;
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') { j += 1; }
                let n: f64 = s[start..j].parse()
                    .map_err(|_| AgError::query("expression", "invalid number"))?;
                out.push(Lexeme::Num(n));
                i = j;
            }
            other => {
                return Err(AgError::query("expression", format!("unexpected character {other:?}")));
            }
        }
    }
    Ok(out)
}

// ---- recursive-descent parser emitting RPN ----
struct Parser<'a> {
    toks: &'a [Lexeme],
    pos: usize,
    out: Vec<Token>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Lexeme> {
        self.toks.get(self.pos)
    }
    fn expr(&mut self) -> AgResult<()> {
        self.term()?;
        while let Some(op) = self.peek() {
            let t = match op {
                Lexeme::Plus => Token::Add,
                Lexeme::Minus => Token::Sub,
                _ => break,
            };
            self.pos += 1;
            self.term()?;
            self.out.push(t);
        }
        Ok(())
    }
    fn term(&mut self) -> AgResult<()> {
        self.factor()?;
        while let Some(op) = self.peek() {
            let t = match op {
                Lexeme::Star => Token::Mul,
                Lexeme::Slash => Token::Div,
                _ => break,
            };
            self.pos += 1;
            self.factor()?;
            self.out.push(t);
        }
        Ok(())
    }
    fn factor(&mut self) -> AgResult<()> {
        match self.peek() {
            Some(Lexeme::Num(n)) => { let n = *n; self.out.push(Token::Num(n)); self.pos += 1; Ok(()) }
            Some(Lexeme::Band(i)) => { let i = *i; self.out.push(Token::Band(i)); self.pos += 1; Ok(()) }
            Some(Lexeme::Minus) => { self.pos += 1; self.factor()?; self.out.push(Token::Neg); Ok(()) }
            Some(Lexeme::LParen) => {
                self.pos += 1;
                self.expr()?;
                match self.peek() {
                    Some(Lexeme::RParen) => { self.pos += 1; Ok(()) }
                    _ => Err(AgError::query("expression", "unbalanced parentheses")),
                }
            }
            _ => Err(AgError::query("expression", "expected a number, band, or '('")),
        }
    }
}
```

- [ ] **Step 4: run → pass.** `cargo test --lib expression` (all tests). `cargo clippy --all-targets -- -D warnings`. (The `Token` enum is `pub`; it will be referenced by `CompiledTileStyle` in B1 — until then add `#[allow(dead_code)]` on items clippy flags as unused, and REMOVE in B1.)

- [ ] **Step 5: commit.** `git add src/render/expression.rs src/render/mod.rs && git commit -m "feat(render): arithmetic band-math expression evaluator"`

---

## Task A2: `Index` enum sugar (in `src/render/expression.rs`)

- [ ] **Step 1: tests first.** Add to the expression.rs test module:

```rust
    #[test]
    fn index_lowers_and_parses() {
        // Each Index variant lowers to a parseable expression.
        for idx in [
            Index::Ndvi { nir: 5, red: 4 },
            Index::Ndwi { green: 3, nir: 5 },
            Index::Ndbi { swir: 6, nir: 5 },
            Index::Evi { nir: 5, red: 4, blue: 2 },
            Index::Savi { nir: 5, red: 4, l: 0.5 },
        ] {
            let s = idx.to_expression();
            assert!(parse_expression(&s).is_ok(), "unparseable: {s}");
        }
    }

    #[test]
    fn ndvi_index_matches_manual_expression_end_to_end() {
        let idx = Index::Ndvi { nir: 5, red: 4 };
        let t = parse_expression(&idx.to_expression()).unwrap();
        let mut bands = vec![0.0; 5];
        bands[4] = 0.5; bands[3] = 0.1;
        assert!((eval_rpn(&t, &bands) - 0.6666667).abs() < 1e-5);
    }

    #[test]
    fn savi_uses_l_factor() {
        // SAVI = (1+L)(NIR-RED)/(NIR+RED+L); with L=0.5, NIR=0.5, RED=0.1:
        // 1.5*0.4/1.1 = 0.5454545
        let idx = Index::Savi { nir: 5, red: 4, l: 0.5 };
        let t = parse_expression(&idx.to_expression()).unwrap();
        let mut bands = vec![0.0; 5];
        bands[4] = 0.5; bands[3] = 0.1;
        assert!((eval_rpn(&t, &bands) - 0.5454545).abs() < 1e-5, "savi={}", eval_rpn(&t, &bands));
    }
```

- [ ] **Step 2: run → fail** (no `Index`).

- [ ] **Step 3: implement.** Add to expression.rs:

```rust
/// Common spectral indices. Band numbers are 1-indexed (as in `b<N>`).
/// Lowers to an arithmetic expression string evaluated by the same engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Index {
    Ndvi { nir: u32, red: u32 },
    Ndwi { green: u32, nir: u32 },
    Ndbi { swir: u32, nir: u32 },
    Evi { nir: u32, red: u32, blue: u32 },
    Savi { nir: u32, red: u32, l: f64 },
}

impl Index {
    pub fn to_expression(&self) -> String {
        match *self {
            Index::Ndvi { nir, red } => format!("(b{nir}-b{red})/(b{nir}+b{red})"),
            Index::Ndwi { green, nir } => format!("(b{green}-b{nir})/(b{green}+b{nir})"),
            Index::Ndbi { swir, nir } => format!("(b{swir}-b{nir})/(b{swir}+b{nir})"),
            Index::Evi { nir, red, blue } => {
                format!("2.5*(b{nir}-b{red})/(b{nir}+6*b{red}-7.5*b{blue}+1)")
            }
            Index::Savi { nir, red, l } => {
                format!("(1+{l})*(b{nir}-b{red})/(b{nir}+b{red}+{l})")
            }
        }
    }
}
```

- [ ] **Step 4: run → pass.** `cargo test --lib expression`, clippy.

- [ ] **Step 5: commit.** `git add src/render/expression.rs && git commit -m "feat(render): typed spectral Index sugar lowering to expressions"`

---

## Task B1: `TileStyle.expression` + compile (store RPN, mutual exclusion)

**Files:** `src/render/pipeline.rs` (add field to `CompiledTileStyle`), `src/style/params.rs`.

- [ ] **Step 1: tests first** (in `params.rs` tests):

```rust
    #[test]
    fn expression_compiles_to_rpn() {
        let s = TileStyle { expression: Some("(b2-b1)/(b2+b1)".into()), ..TileStyle::default() };
        let c = compile(&s).unwrap();
        assert!(!c.expression.is_empty(), "expression RPN should be populated");
    }

    #[test]
    fn expression_and_bidx_are_mutually_exclusive() {
        let s = TileStyle { expression: Some("b1".into()), bidx: vec![1], ..TileStyle::default() };
        assert!(compile(&s).is_err());
    }

    #[test]
    fn expression_and_color_formula_are_mutually_exclusive() {
        let s = TileStyle {
            expression: Some("b1".into()),
            color_formula: Some("gamma rgb 1.2".into()),
            ..TileStyle::default()
        };
        assert!(compile(&s).is_err());
    }

    #[test]
    fn invalid_expression_is_query_error() {
        let s = TileStyle { expression: Some("b1+".into()), ..TileStyle::default() };
        assert!(matches!(compile(&s), Err(AgError::Query { .. })));
    }

    #[test]
    fn expression_parsed_from_query() {
        let s = TileStyle::from_query("expression=b1%2Bb2").unwrap();
        assert_eq!(s.expression.as_deref(), Some("b1+b2"));
    }
```

- [ ] **Step 2: run → fail.**

- [ ] **Step 3: add the `CompiledTileStyle` field.** In `src/render/pipeline.rs`, add to the struct (after `formula`):
```rust
    pub expression: Vec<crate::render::expression::Token>,
```
This requires `CompiledTileStyle` keep `#[derive(Debug, Clone)]` (it does) and `Token` derives `Debug, Clone` (it does). Remove any `#[allow(dead_code)]` left on `expression.rs` items now that they're used.

- [ ] **Step 4: add `TileStyle.expression`.** In `params.rs`: `pub expression: Option<String>,` (after `color_formula`), `expression: None,` in `Default`, and apply_param arm `"expression" => self.expression = Some(v.to_owned()),`.

- [ ] **Step 5: wire `compile`.** In `compile` (`params.rs:162`):
  - Add mutual-exclusion guards near the top (after `validate_tilesize`):
    ```rust
    if style.expression.is_some() {
        if !style.bidx.is_empty() {
            return Err(AgError::query("expression", "expression and bidx are mutually exclusive"));
        }
        if style.color_formula.as_deref().is_some_and(|s| !s.is_empty()) {
            return Err(AgError::query("expression", "expression and color_formula are mutually exclusive"));
        }
    }
    ```
  - Compile the RPN:
    ```rust
    let expression = match &style.expression {
        Some(s) => crate::render::expression::parse_expression(s)?,
        None => Vec::new(),
    };
    ```
  - When `expression` is present, the composite should be a single-band gradient. The existing `composite` selection (colormap/colormap_name → … → greys default) already produces a gradient for the no-bidx single-band case. Ensure expression does NOT trip `auto_rgb`: add `&& style.expression.is_none()` to the `auto_rgb` computation. With `expression` set, `bidx` is empty and the composite falls to the `colormap`/`colormap_name`/greys gradient branch — correct (an index is colormapped like any single band).
  - Add `expression,` to the `Ok(CompiledTileStyle { … })` initializer.

- [ ] **Step 6: run → pass.** `cargo test --lib` (full), clippy.

- [ ] **Step 7: commit.** `git add src/render/pipeline.rs src/style/params.rs && git commit -m "feat(style): compile band-math expression into CompiledTileStyle"`

---

## Task B2: Evaluate expression in `render` (synthetic band + mask)

**Files:** `src/render/pipeline.rs`.

- [ ] **Step 1: test first.** Add to pipeline.rs tests a unit test of a small helper `eval_expression_band` you will add (pure, testable without PNG):

```rust
    #[test]
    fn eval_expression_band_ndvi_and_masks_nonfinite() {
        use crate::render::expression::parse_expression;
        // 2 bands, 1x2. NDVI=(b2-b1)/(b2+b1).
        // pixel0: b1=0.1,b2=0.5 → 0.6667 valid. pixel1: b1=0,b2=0 → 0/0 NaN → invalid.
        let src = ndarray::array![[[0.1, 0.0]], [[0.5, 0.0]]]; // (bands=2,h=1,w=2)
        let rpn = parse_expression("(b2-b1)/(b2+b1)").unwrap();
        let (band, valid) = eval_expression_band(&src.view(), &rpn, None, 1, 2).unwrap();
        assert!((band[[0, 0]] - 0.6666667).abs() < 1e-5);
        assert!(valid[[0, 0]]);
        assert!(!valid[[0, 1]], "0/0 must be masked invalid");
    }

    #[test]
    fn eval_expression_band_rejects_out_of_range_band() {
        use crate::render::expression::parse_expression;
        let src = ndarray::array![[[1.0]]]; // 1 band
        let rpn = parse_expression("b5").unwrap(); // references band 5
        assert!(eval_expression_band(&src.view(), &rpn, None, 1, 1).is_err());
    }

    #[test]
    fn eval_expression_band_masks_nodata_in_referenced_band() {
        use crate::render::expression::parse_expression;
        // b1 nodata sentinel -9999 at pixel0 → invalid even though arithmetic is finite.
        let src = ndarray::array![[[-9999.0, 0.2]], [[0.4, 0.6]]]; // 2 bands,1x2
        let rpn = parse_expression("b1+b2").unwrap();
        let (_band, valid) = eval_expression_band(&src.view(), &rpn, Some(-9999.0), 1, 2).unwrap();
        assert!(!valid[[0, 0]], "nodata in referenced band → invalid");
        assert!(valid[[0, 1]]);
    }
```

NOTE: confirm the exact `Array3<f64>` view type and helper signature you implement match these calls. Adjust the test calls to the real signature if needed, but keep the three asserted behaviors (NDVI value, NaN→invalid, nodata-in-referenced-band→invalid).

- [ ] **Step 2: run → fail.**

- [ ] **Step 3: implement `eval_expression_band`.** Add to `pipeline.rs`:

```rust
/// Evaluate `rpn` per-pixel over the source bands, producing a synthetic
/// single band plus a validity mask. A pixel is valid iff every REFERENCED
/// band is valid (not nodata/NaN) AND the result is finite. Returns
/// `AgError` if the expression references a band index >= `bands`.
fn eval_expression_band(
    src: &ndarray::ArrayView3<f64>,
    rpn: &[crate::render::expression::Token],
    nodata: Option<f64>,
    h: usize,
    w: usize,
) -> AgResult<(Array2<f64>, Array2<bool>)> {
    use crate::render::expression::{eval_rpn, max_band_ref, Token};
    let bands = src.shape()[0];
    if let Some(maxref) = max_band_ref(rpn) {
        if maxref >= bands {
            return Err(AgError::query(
                "expression",
                format!("band b{} out of range for {} bands", maxref + 1, bands),
            ));
        }
    }
    // Which bands does the expression reference (0-indexed)?
    let refs: Vec<usize> = {
        let mut v: Vec<usize> = rpn.iter().filter_map(|t| match t {
            Token::Band(i) => Some(*i),
            _ => None,
        }).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let nd_is_nan = nodata.map(|n| n.is_nan()).unwrap_or(false);
    let mut out = Array2::<f64>::zeros((h, w));
    let mut valid = Array2::<bool>::from_elem((h, w), true);
    let mut px = vec![0.0f64; bands];
    for r in 0..h {
        for c in 0..w {
            for b in 0..bands {
                px[b] = src[[b, r, c]];
            }
            // referenced-band nodata check
            let mut ok = true;
            if let Some(nd) = nodata {
                for &b in &refs {
                    let x = px[b];
                    if (nd_is_nan && x.is_nan()) || (!nd_is_nan && (x - nd).abs() < 1e-9) {
                        ok = false;
                        break;
                    }
                }
            }
            let v = if ok { eval_rpn(rpn, &px) } else { f64::NAN };
            if !ok || !v.is_finite() {
                valid[[r, c]] = false;
                out[[r, c]] = 0.0;
            } else {
                out[[r, c]] = v;
            }
        }
    }
    Ok((out, valid))
}
```

- [ ] **Step 4: route it in `render`.** In `render` (`pipeline.rs:37`), add an expression branch BEFORE the existing `bidx` selection logic (after the `try_u8_rgb_fastpath` early return and `let src = image.pixels.to_f64();`). When `!style.expression.is_empty()`:
```rust
    if !style.expression.is_empty() {
        let nodata = style.nodata.or(image.nodata);
        let (band, valid) = eval_expression_band(&src.view(), &style.expression, nodata, h, w)?;
        let valid_opt = if valid.iter().all(|&b| b) { None } else { Some(valid) };
        let rgba = render_single_band(&band, valid_opt.as_ref(), style);
        return encode_png(&rgba);
    }
```
Place this right after `let src = image.pixels.to_f64();` (so `src`, `h`, `w` are in scope) and before the `let bidx … ` block. `render_single_band` uses `style.composite` (the gradient/colormap) with `comp.vmin/vmax` = the rescale range (set by `effective_compiled` in B3, or explicit rescale).

- [ ] **Step 5: run → pass.** `cargo test --lib` (full), clippy.

- [ ] **Step 6: commit.** `git add src/render/pipeline.rs && git commit -m "feat(render): evaluate band-math expression into a synthetic band"`

---

## Task B3: `effective_compiled` — stretch expression from synthetic band (NOT band stats)

**Files:** `src/tiler.rs`. THE CRITICAL auto-stretch fix (advisor point #1).

- [ ] **Step 1: test first.** Add a unit test for a pure helper `expression_rescale(image, rpn) -> Option<(f64,f64)>` (the synthetic-band finite min/max), testable without the async path:

```rust
    #[test]
    fn expression_rescale_uses_synthetic_band_range_not_band_stats() {
        use crate::pixel::{ImageData, PixelData};
        use crate::render::expression::parse_expression;
        // 2 bands. NDVI over a few pixels → range roughly [-1, 1]-ish, NOT band DN range.
        let pixels = PixelData::F64(ndarray::array![
            [[1000.0, 2000.0, 0.0]],   // b1
            [[3000.0, 2000.0, 0.0]]    // b2
        ]); // (2,1,3)
        let img = ImageData {
            pixels, mask: None,
            crs: crate::crs::Crs::epsg(3857),
            transform: crate::bounds::GeoTransform::from_gdal(&[0.0,1.0,0.0,0.0,0.0,-1.0]),
            nodata: None,
            bbox: crate::bounds::Bbox::new(0.0,0.0,3.0,1.0),
        };
        let rpn = parse_expression("(b2-b1)/(b2+b1)").unwrap();
        let (lo, hi) = expression_rescale(&img, &rpn).expect("finite range");
        // pixel values: (3000-1000)/4000=0.5 ; (2000-2000)/4000=0 ; 0/0 NaN (skipped).
        assert!((lo - 0.0).abs() < 1e-9 && (hi - 0.5).abs() < 1e-9, "lo={lo} hi={hi}");
        // crucially the range is in NDVI units, nowhere near the 1000s DN band range.
        assert!(hi < 1.0);
    }
```

- [ ] **Step 2: run → fail.**

- [ ] **Step 3: implement `expression_rescale`** in `tiler.rs`:
```rust
/// Finite-value (min, max) of the synthetic band an expression produces over
/// `image`. `None` if no finite pixels (caller falls back to a default range).
fn expression_rescale(image: &ImageData, rpn: &[crate::render::expression::Token]) -> Option<(f64, f64)> {
    use crate::render::expression::{eval_rpn, Token};
    let src = image.pixels.to_f64();
    let bands = src.shape()[0];
    let (h, w) = (src.shape()[1], src.shape()[2]);
    let nodata = image.nodata;
    let nd_is_nan = nodata.map(|n| n.is_nan()).unwrap_or(false);
    let refs: Vec<usize> = {
        let mut v: Vec<usize> = rpn.iter().filter_map(|t| match t { Token::Band(i) => Some(*i), _ => None }).collect();
        v.sort_unstable(); v.dedup(); v
    };
    let mut px = vec![0.0f64; bands];
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for r in 0..h {
        for c in 0..w {
            for b in 0..bands { px[b] = src[[b, r, c]]; }
            let mut ok = true;
            if let Some(nd) = nodata {
                for &b in &refs {
                    let x = px[b];
                    if (nd_is_nan && x.is_nan()) || (!nd_is_nan && (x - nd).abs() < 1e-9) { ok = false; break; }
                }
            }
            if !ok { continue; }
            let v = eval_rpn(rpn, &px);
            if v.is_finite() { lo = lo.min(v); hi = hi.max(v); }
        }
    }
    if lo.is_finite() && hi.is_finite() { Some((lo, hi)) } else { None }
}
```

- [ ] **Step 4: wire into `effective_compiled`.** In `effective_compiled` (`tiler.rs:88`), BEFORE the `dataset_stats`-based path, add an expression branch (the early `return` for explicit `rescale` at the top already handles user-supplied ranges, so this only runs when no explicit rescale):
```rust
        if !style.expression.is_empty_expression() {  // see note
            let mut next = (**compiled).clone();
            let (lo, hi) = expression_rescale(image, &compiled.expression).unwrap_or((-1.0, 1.0));
            next.rescale = vec![(lo, hi)];
            next.composite.vmin = Some(vec![lo]);
            next.composite.vmax = Some(vec![hi]);
            next.auto_rgb = false;
            return Ok(std::sync::Arc::new(next));
        }
```
NOTE: `style` is `&TileStyle` (has `expression: Option<String>`); `compiled.expression` is the RPN `Vec<Token>`. Use `style.expression.is_some()` for the guard (not a method) — i.e.:
```rust
        if style.expression.is_some() {
            let mut next = (**compiled).clone();
            let (lo, hi) = expression_rescale(image, &compiled.expression).unwrap_or((-1.0, 1.0));
            ...
        }
```
Place this guard AFTER the existing `if !style.rescale.is_empty() { return Ok(Arc::clone(compiled)); }` early return (so explicit rescale wins) and BEFORE the dataset_stats logic (so stats never drives an expression stretch). Default `(-1.0, 1.0)` when the tile has no finite pixels (all-nodata tile).

- [ ] **Step 5: run → pass.** `cargo test --lib`, clippy.

- [ ] **Step 6: commit.** `git add src/tiler.rs && git commit -m "feat(tiler): stretch expression output from synthetic band, not band stats"`

---

## Task C1: Release 0.0.5

- [ ] **Step 1.** `Cargo.toml`: `version = "0.0.4"` → `version = "0.0.5"`.
- [ ] **Step 2.** `CHANGELOG.md`: new `## [0.0.5] - <YYYY-MM-DD>` below `## [Unreleased]`:
```markdown
## [0.0.5] - 2026-05-30

### Added

- Band-math `expression` style param (e.g. `expression=(b5-b4)/(b5+b4)`): an arithmetic evaluator (`+ - * /`, parens, unary minus, float literals, 1-indexed `b<N>`) computes a synthetic single band that is then rescaled and colormapped. Division-by-zero / non-finite results and nodata in any referenced band render transparent. Mutually exclusive with `bidx` and `color_formula` (#1).
- Typed `Index` spectral-index sugar (`Ndvi`, `Ndwi`, `Ndbi`, `Evi`, `Savi`) lowering to expression strings via `Index::to_expression()`.
- When `expression` is set without explicit `rescale`, the stretch is derived from the computed index's own value range (not the source bands' raw statistics). Pass e.g. `rescale=-1,1` for a fixed normalized-index range.

### Known limitations

- Expression output is single-band; comma-separated multi-band expressions and functions (`sqrt`/`abs`/…) are not supported in this release.
```
- [ ] **Step 3.** `cargo build` (rewrites Cargo.lock to 0.0.5).
- [ ] **Step 4.** Commit: `git add Cargo.toml Cargo.lock CHANGELOG.md && git commit -m "chore(release): 0.0.5 — band-math expression + spectral indices"`

---

## Final verification
- [ ] `cargo test` (full suite green)
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` (clean)
- [ ] `cargo build --examples` (examples compile — adding `TileStyle.expression` and `CompiledTileStyle.expression` must not break any literal; all literals use `..default()` so fine, but confirm)
- [ ] `cargo doc --no-deps` (no NEW warnings beyond the 2 pre-existing)
- [ ] End-to-end sanity (optional, if a fixture allows): a tile request with `expression=(b1-b2)/(b1+b2)` returns a PNG without error.

## Notes
- **Recurring-bug guard:** every new pixel transform here is tested on the masked/nodata/non-finite pixel, not just the happy value (eval mask test, nodata-in-referenced-band test, div-by-zero test).
- **Out of scope (done — all 6 gaps):** after 0.0.5, issues #1–#6 are all closed across 0.0.3/0.0.4/0.0.5.
