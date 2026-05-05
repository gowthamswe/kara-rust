//! Edit-distance helpers shared across diagnostic emitters.
//!
//! Used by the resolver for `did you mean` corrections on undefined names /
//! types, and by the typechecker for `no method named ... did you mean ...`
//! suggestions. Originally lived in `resolver.rs`; promoted to its own module
//! when method-resolution diagnostics needed access to the same logic.

pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut matrix = vec![vec![0usize; n + 1]; m + 1];
    for (i, row) in matrix.iter_mut().enumerate().take(m + 1) {
        row[0] = i;
    }
    #[allow(clippy::needless_range_loop)]
    for j in 0..=n {
        matrix[0][j] = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            matrix[i][j] = (matrix[i - 1][j] + 1)
                .min(matrix[i][j - 1] + 1)
                .min(matrix[i - 1][j - 1] + cost);
        }
    }
    matrix[m][n]
}

/// Return the closest candidate (edit distance ≤ 2) to `name`. Returns
/// `None` when `name` is shorter than 3 characters or no candidate is
/// within tolerance.
pub fn suggest_similar(name: &str, visible: &[&str]) -> Option<String> {
    if name.len() < 3 {
        return None;
    }
    let mut best: Option<(&str, usize)> = None;
    for &candidate in visible {
        if candidate == name {
            continue;
        }
        let dist = levenshtein_distance(name, candidate);
        if dist <= 2 {
            match best {
                None => best = Some((candidate, dist)),
                Some((_, best_dist)) if dist < best_dist => best = Some((candidate, dist)),
                _ => {}
            }
        }
    }
    best.map(|(s, _)| s.to_string())
}
