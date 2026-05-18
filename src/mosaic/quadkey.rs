//! Tile ↔ quadkey helpers (Bing/slippy quadtree encoding).

/// `(z, x, y)` → quadkey string of length `z`.
pub fn tile_to_quadkey(z: u8, x: u32, y: u32) -> String {
    let mut qk = String::with_capacity(z as usize);
    for i in (1..=z).rev() {
        let mut digit = 0u8;
        let mask = 1u32 << (i - 1);
        if x & mask != 0 {
            digit += 1;
        }
        if y & mask != 0 {
            digit += 2;
        }
        qk.push((b'0' + digit) as char);
    }
    qk
}

/// Resolve the asset-key lookups for a requested tile against an index
/// whose keys are quadkeys at `quadkey_zoom`.
///
/// - `z == quadkey_zoom` → exact key.
/// - `z >  quadkey_zoom` → the ancestor key (prefix of length `quadkey_zoom`).
/// - `z <  quadkey_zoom` → prefix-match: caller unions all index keys that
///   start with the requested quadkey (returned as the prefix to match).
pub enum QkLookup {
    /// Exact key OR ancestor key — look up this single key.
    Exact(String),
    /// Union every index key starting with this prefix.
    Prefix(String),
}

pub fn lookup(z: u8, x: u32, y: u32, quadkey_zoom: u8) -> QkLookup {
    let qk = tile_to_quadkey(z, x, y);
    if z == quadkey_zoom {
        QkLookup::Exact(qk)
    } else if z > quadkey_zoom {
        QkLookup::Exact(qk[..quadkey_zoom as usize].to_string())
    } else {
        QkLookup::Prefix(qk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_quadkeys() {
        assert_eq!(tile_to_quadkey(1, 0, 0), "0");
        assert_eq!(tile_to_quadkey(1, 1, 1), "3");
        assert_eq!(tile_to_quadkey(3, 3, 5), "213");
    }

    #[test]
    fn lookup_dispatch() {
        match lookup(8, 12, 34, 8) {
            QkLookup::Exact(k) => assert_eq!(k.len(), 8),
            _ => panic!("z==zoom must be Exact"),
        }
        match lookup(10, 12, 34, 8) {
            QkLookup::Exact(k) => assert_eq!(k.len(), 8), // ancestor truncated
            _ => panic!("z>zoom must be Exact ancestor"),
        }
        match lookup(5, 12, 34, 8) {
            QkLookup::Prefix(k) => assert_eq!(k.len(), 5),
            _ => panic!("z<zoom must be Prefix"),
        }
    }
}
