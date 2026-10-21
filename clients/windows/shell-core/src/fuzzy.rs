//! Fuzzy matching for the command palette. Each whitespace-separated term
//! must match as a subsequence of the candidate. Matches at word starts and in
//! runs score higher.

/// Score of `query` against `text`, or `None` when it does not match.
/// Also returns the matched char indices (of `text`) for highlighting.
pub fn score(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let t: Vec<char> = text.chars().collect();
    let tl: Vec<char> = text.chars().flat_map(|c| c.to_lowercase()).collect();
    if tl.len() != t.len() {
        return None;
    }
    let mut total = 0;
    let mut hits = Vec::new();
    for term in query.split_whitespace() {
        let q: Vec<char> = term.chars().flat_map(|c| c.to_lowercase()).collect();
        let (s, h) = term_score(&q, &t, &tl)?;
        total += s;
        hits.extend(h);
    }
    hits.sort_unstable();
    hits.dedup();
    // Shorter candidates win ties.
    Some((total * 4 - (t.len() as i32).min(60) / 4, hits))
}

fn term_score(q: &[char], t: &[char], tl: &[char]) -> Option<(i32, Vec<usize>)> {
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    // Greedy forward match, then pull the match left to the tightest window.
    let mut idx = Vec::with_capacity(q.len());
    let mut ti = 0;
    for &qc in q {
        while ti < tl.len() && tl[ti] != qc {
            ti += 1;
        }
        if ti == tl.len() {
            return None;
        }
        idx.push(ti);
        ti += 1;
    }
    let mut j = q.len();
    let mut end = *idx.last().unwrap();
    while j > 0 {
        j -= 1;
        let mut k = end;
        while tl[k] != q[j] {
            k -= 1;
        }
        idx[j] = k;
        end = k.saturating_sub(1);
        if k == 0 && j > 0 {
            return None;
        }
    }
    let mut s = 0;
    for (n, &i) in idx.iter().enumerate() {
        s += 10;
        let boundary = i == 0 || !t[i - 1].is_alphanumeric();
        if boundary {
            s += 15;
        }
        if n > 0 && idx[n - 1] + 1 == i {
            s += 12;
        }
        if t[i] == q[n] {
            s += 1;
        }
    }
    s -= (idx.last().unwrap() - idx[0]) as i32 / 2;
    Some((s, idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_right_ranks_first() {
        let items = ["New tab", "Split pane right", "Split pane down", "Close pane", "Toggle sidebar"];
        let mut r: Vec<_> = items
            .iter()
            .filter_map(|i| score("split right", i).map(|(s, _)| (s, *i)))
            .collect();
        r.sort_by(|a, b| b.0.cmp(&a.0));
        assert_eq!(r.first().map(|x| x.1), Some("Split pane right"));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn subsequence_and_boundaries() {
        assert!(score("tsb", "Toggle sidebar").is_some());
        assert!(score("zzz", "Toggle sidebar").is_none());
        let a = score("np", "New pane").unwrap().0;
        let b = score("np", "Open snapshot").unwrap().0;
        assert!(a > b);
        assert!(score("", "anything").is_some());
    }
}
