use std::collections::VecDeque;

pub struct Matcher {
    // transitions[node * alphabet_size + symbol]
    transitions: Vec<u32>,
    output: Vec<bool>,
    alphabet: [usize; 256],
    alphabet_size: usize,
}

impl Matcher {
    pub fn new(patterns: &[String]) -> Self {
        let mut alphabet = [usize::MAX; 256];
        let mut alphabet_size = 0;

        // Compress the byte alphabet to only bytes used by patterns.
        for pattern in patterns {
            for &byte in pattern.as_bytes() {
                let slot = &mut alphabet[byte as usize];

                if *slot == usize::MAX {
                    *slot = alphabet_size;
                    alphabet_size += 1;
                }
            }
        }

        let mut matcher = Self {
            transitions: vec![0; alphabet_size],
            output: vec![false],
            alphabet,
            alphabet_size,
        };

        // Build the trie.
        for pattern in patterns {
            let mut node = 0usize;

            if pattern.is_empty() {
                matcher.output[0] = true;
                continue;
            }

            for &byte in pattern.as_bytes() {
                let symbol = matcher.alphabet[byte as usize];
                let index = node * alphabet_size + symbol;
                let next = matcher.transitions[index];

                if next == 0 {
                    let new_node = matcher.output.len() as u32;

                    matcher.output.push(false);
                    matcher
                        .transitions
                        .extend(std::iter::repeat_n(0, alphabet_size));

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
                let index = node * alphabet_size + symbol;
                let child = matcher.transitions[index];

                if child != 0 {
                    let fallback = matcher.transitions[fail_node * alphabet_size + symbol];

                    failure[child as usize] = fallback;

                    // A pattern also ends here if one ends at its failure state.
                    if matcher.output[fallback as usize] {
                        matcher.output[child as usize] = true;
                    }

                    queue.push_back(child as usize);
                } else {
                    // Complete the DFA transition table.
                    matcher.transitions[index] =
                        matcher.transitions[fail_node * alphabet_size + symbol];
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
            let symbol = self.alphabet[byte as usize];

            if symbol == usize::MAX {
                // This byte cannot participate in any pattern.
                state = 0;
                continue;
            }

            state = self.transitions[state * self.alphabet_size + symbol] as usize;

            if self.output[state] {
                return true;
            }
        }

        false
    }
}
