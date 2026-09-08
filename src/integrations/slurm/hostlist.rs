//! Slurm hostlist expansion.
//!
//! Slurm writes node sets compactly: `node[01-04,07]`, `rack[1-2]node[1-3]`,
//! `a,b[1-2]`. Partition membership is only usable once expanded.
//!
//! Padding is significant: `node[01-03]` yields `node01`, not `node1`.

/// Expand a Slurm hostlist into individual node names.
///
/// Unparseable input yields the input as a single name rather than an error:
/// a hostlist form this parser does not know must not make an entire
/// discovery cycle fail (IMPLEMENTATION.md §155).
pub fn expand(hostlist: &str) -> Vec<String> {
    let hostlist = hostlist.trim();
    if hostlist.is_empty() || hostlist == "(null)" {
        return Vec::new();
    }

    let mut out = Vec::new();
    for element in split_top_level(hostlist) {
        expand_element(&element, &mut out);
    }
    out
}

/// Split on commas that are not inside brackets.
fn split_top_level(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;

    for ch in input.chars() {
        match ch {
            '[' => {
                depth += 1;
                current.push(ch);
            }
            ']' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

/// Expand one element, which may contain several bracket groups.
fn expand_element(element: &str, out: &mut Vec<String>) {
    let Some(open) = element.find('[') else {
        out.push(element.to_string());
        return;
    };
    let Some(close) = element[open..].find(']').map(|i| open + i) else {
        // Unbalanced: keep it verbatim rather than dropping a node.
        out.push(element.to_string());
        return;
    };

    let prefix = &element[..open];
    let ranges = &element[open + 1..close];
    let suffix = &element[close + 1..];

    let mut middles = Vec::new();
    for range in ranges.split(',') {
        match expand_range(range.trim()) {
            Some(values) => middles.extend(values),
            // If any part of the bracket is not understood, keep the element
            // verbatim. Half-expanding it would invent node names that do not
            // exist, which is worse than carrying an odd one through.
            None => {
                out.push(element.to_string());
                return;
            }
        }
    }

    for middle in middles {
        // The suffix may itself contain a bracket group: `rack[1-2]node[1-3]`.
        expand_element(&format!("{prefix}{middle}{suffix}"), out);
    }
}

/// Expand `01-04` or `7`, preserving zero padding.
fn expand_range(range: &str) -> Option<Vec<String>> {
    if let Some((start, end)) = range.split_once('-') {
        let (start, end) = (start.trim(), end.trim());
        if start.is_empty() || end.is_empty() || !start.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if !end.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        // Slurm pads to the width the operator wrote on the *left* of the
        // range: `node[08-11]` is node08..node11, but `node[8-11]` is
        // node8..node11.
        let width = start.len();
        let (start, end): (u64, u64) = (start.parse().ok()?, end.parse().ok()?);
        if end < start {
            return None;
        }
        // Guard against a typo like [1-99999999] turning into an OOM.
        if end - start > 100_000 {
            return None;
        }
        return Some((start..=end).map(|n| format!("{n:0width$}")).collect());
    }

    if !range.is_empty() && range.chars().all(|c| c.is_ascii_digit()) {
        return Some(vec![range.to_string()]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_expands_to_itself() {
        assert_eq!(expand("node01"), ["node01"]);
    }

    #[test]
    fn a_comma_list_expands_to_its_members() {
        assert_eq!(expand("a,b,c"), ["a", "b", "c"]);
        assert_eq!(expand(" a , b "), ["a", "b"]);
    }

    #[test]
    fn a_range_expands_and_keeps_zero_padding() {
        assert_eq!(expand("node[01-04]"), ["node01", "node02", "node03", "node04"]);
        assert_eq!(expand("node[8-11]"), ["node8", "node9", "node10", "node11"]);
    }

    #[test]
    fn padding_follows_the_left_hand_side_of_the_range() {
        // `node[8-11]` is node8..node11; `node[08-11]` is node08..node11.
        assert_eq!(expand("node[8-11]"), ["node8", "node9", "node10", "node11"]);
        assert_eq!(expand("node[08-11]"), ["node08", "node09", "node10", "node11"]);
        assert_eq!(expand("node[008-010]"), ["node008", "node009", "node010"]);
    }

    #[test]
    fn a_partly_unparseable_bracket_is_not_half_expanded() {
        // Inventing node names that do not exist is worse than carrying one
        // odd string through to the operator.
        assert_eq!(expand("node[1-2,x]"), ["node[1-2,x]"]);
    }

    #[test]
    fn a_mixed_bracket_body_expands() {
        assert_eq!(expand("node[01-03,07]"), ["node01", "node02", "node03", "node07"]);
    }

    #[test]
    fn a_suffix_after_the_bracket_is_preserved() {
        assert_eq!(expand("node[1-2]-ib"), ["node1-ib", "node2-ib"]);
    }

    #[test]
    fn two_bracket_groups_expand_as_a_product() {
        assert_eq!(expand("r[1-2]n[1-2]"), ["r1n1", "r1n2", "r2n1", "r2n2"]);
    }

    #[test]
    fn a_top_level_comma_outside_brackets_splits_but_one_inside_does_not() {
        assert_eq!(expand("a,b[1-2]"), ["a", "b1", "b2"]);
        assert_eq!(expand("b[1,3]"), ["b1", "b3"]);
    }

    #[test]
    fn a_single_element_range_works() {
        assert_eq!(expand("node[5-5]"), ["node5"]);
    }

    #[test]
    fn empty_and_null_hostlists_expand_to_nothing() {
        assert!(expand("").is_empty());
        assert!(expand("   ").is_empty());
        assert!(expand("(null)").is_empty());
    }

    #[test]
    fn malformed_input_is_kept_verbatim_rather_than_failing_discovery() {
        // Losing a node from the inventory is worse than carrying an odd name.
        assert_eq!(expand("node[01-"), ["node[01-"]);
        assert_eq!(expand("node[]"), ["node[]"]);
        assert_eq!(expand("node[a-b]"), ["node[a-b]"]);
        assert_eq!(expand("node[4-2]"), ["node[4-2]"]);
    }

    #[test]
    fn an_absurd_range_is_refused_instead_of_exhausting_memory() {
        let expanded = expand("node[1-99999999]");
        assert_eq!(expanded, ["node[1-99999999]"]);
    }

    #[test]
    fn expansion_is_not_quadratic_on_a_realistic_cluster() {
        let expanded = expand("node[0001-2000]");
        assert_eq!(expanded.len(), 2000);
        assert_eq!(expanded[0], "node0001");
        assert_eq!(expanded[1999], "node2000");
    }
}
