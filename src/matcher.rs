use std::collections::VecDeque;

// Byte-exact multi-pattern matcher (Aho-Corasick DFA). ASCII case is
// folded at the boundary — patterns on the way in, text bytes on the
// way through — so checks never allocate and non-ASCII bytes pass
// through untouched.
pub struct Matcher {
    // transitions[node * stride + symbol]; the stride is the alphabet
    // size padded to a power of two so indexing shifts, and symbol ids
    // stay dense below the alphabet size.
    transitions: Vec<u32>,
    output: Vec<bool>,
    alphabet: [usize; 256],
    alphabet_size: usize,
    stride_shift: u32,
}

impl Matcher {
    pub fn new(patterns: &[String]) -> Self {
        let mut alphabet = [usize::MAX; 256];
        let mut alphabet_size = 0;

        // Compress the byte alphabet to only folded bytes used by patterns.
        for pattern in patterns {
            for &byte in pattern.as_bytes() {
                let slot = &mut alphabet[byte.to_ascii_lowercase() as usize];

                if *slot == usize::MAX {
                    *slot = alphabet_size;
                    alphabet_size += 1;
                }
            }
        }

        // Pad the row stride to a power of two so node indexing shifts
        // instead of multiplying on the hot path.
        let stride = alphabet_size.next_power_of_two().max(1);
        let stride_shift = stride.trailing_zeros();
        debug_assert!(stride.is_power_of_two());

        let mut matcher = Self {
            transitions: vec![0; stride],
            output: vec![false],
            alphabet,
            alphabet_size,
            stride_shift,
        };

        // Build the trie over folded bytes.
        for pattern in patterns {
            let mut node = 0usize;

            if pattern.is_empty() {
                matcher.output[0] = true;
                continue;
            }

            for &byte in pattern.as_bytes() {
                let symbol = matcher.alphabet[byte.to_ascii_lowercase() as usize];
                let index = (node << stride_shift) + symbol;
                let next = matcher.transitions[index];

                if next == 0 {
                    let new_node = matcher.output.len() as u32;

                    matcher.output.push(false);
                    matcher.transitions.extend(std::iter::repeat_n(0, stride));

                    matcher.transitions[index] = new_node;
                    node = new_node as usize;
                } else {
                    node = next as usize;
                }
            }

            matcher.output[node] = true;
        }

        if alphabet_size == 0 {
            return matcher;
        }

        // Build failure links and complete the transition table.
        let mut failure = vec![0u32; matcher.output.len()];
        let mut queue = VecDeque::new();

        // Depth-one nodes fail to the root.
        for symbol in 0..alphabet_size {
            let child = matcher.transitions[symbol];

            if child != 0 {
                queue.push_back(child as usize);
            }
        }

        while let Some(node) = queue.pop_front() {
            let fail_node = failure[node] as usize;

            for symbol in 0..alphabet_size {
                let index = (node << stride_shift) + symbol;
                let child = matcher.transitions[index];

                if child != 0 {
                    let fallback = matcher.transitions[(fail_node << stride_shift) + symbol];

                    failure[child as usize] = fallback;

                    // A pattern also ends here if one ends at its failure state.
                    if matcher.output[fallback as usize] {
                        matcher.output[child as usize] = true;
                    }

                    queue.push_back(child as usize);
                } else {
                    // Complete the DFA transition table.
                    matcher.transitions[index] =
                        matcher.transitions[(fail_node << stride_shift) + symbol];
                }
            }
        }

        matcher
    }

    pub fn is_match(&self, text: &str) -> bool {
        if self.output[0] {
            return true;
        }

        if self.alphabet_size == 0 {
            return false;
        }

        let mut state = 0usize;

        for &byte in text.as_bytes() {
            // Fold here instead of lowercasing the whole title upfront.
            let symbol = self.alphabet[byte.to_ascii_lowercase() as usize];

            if symbol == usize::MAX {
                // This byte cannot participate in any pattern.
                state = 0;
                continue;
            }

            state = self.transitions[(state << self.stride_shift) + symbol] as usize;

            if self.output[state] {
                return true;
            }
        }

        false
    }
}
