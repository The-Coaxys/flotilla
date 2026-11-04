//! Window-free mapping between document lines and what is painted: tabs
//! expand to spaces, and styles from overlapping sources (syntax, find
//! matches, selection, diff words) flatten into non-overlapping runs.
use std::ops::Range;

/// A document line as painted, with a map from source byte offsets to
/// painted byte offsets (one entry per source byte, plus the end).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayLine {
    pub text: String,
    map: Vec<usize>,
}

impl DisplayLine {
    pub fn new(source: &str, tab_width: usize) -> Self {
        let tab_width = tab_width.max(1);
        let mut text = String::with_capacity(source.len());
        let mut map = Vec::with_capacity(source.len() + 1);
        let mut column = 0;
        for c in source.chars() {
            for _ in 0..c.len_utf8() {
                map.push(text.len());
            }
            if c == '\t' {
                let spaces = tab_width - column % tab_width;
                text.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            } else {
                text.push(c);
                column += 1;
            }
        }
        map.push(text.len());
        Self { text, map }
    }

    /// Painted offset of a source offset within the line.
    pub fn to_display(&self, source: usize) -> usize {
        self.map[source.min(self.map.len() - 1)]
    }

    pub fn range_to_display(&self, range: Range<usize>) -> Range<usize> {
        self.to_display(range.start)..self.to_display(range.end)
    }

    /// Source offset of the painted column `column` (chars from line start):
    /// the nearest character boundary, rounding inside an expanded tab.
    pub fn source_for_column(&self, column: usize) -> usize {
        let display = self
            .text
            .char_indices()
            .nth(column)
            .map_or(self.text.len(), |(at, _)| at);
        // The last source offset whose painted position is at or before it.
        let mut best = 0;
        for (source, &painted) in self.map.iter().enumerate() {
            if painted <= display {
                best = source;
            } else {
                break;
            }
        }
        // Step back to a character boundary of the source.
        while best > 0 && self.map[best] == self.map[best - 1] {
            best -= 1;
        }
        best
    }

    /// Painted column (chars) of a source offset.
    pub fn column(&self, source: usize) -> usize {
        self.text[..self.to_display(source)].chars().count()
    }
}

/// Flatten layered styles into sorted, non-overlapping runs over `0..len`.
/// Later layers win where they overlap earlier ones; each layer's ranges
/// must lie on character boundaries.
pub fn flatten<S: Clone + PartialEq>(
    len: usize,
    layers: &[&[(Range<usize>, S)]],
) -> Vec<(Range<usize>, S)> {
    let mut slots: Vec<Option<S>> = vec![None; len];
    for layer in layers {
        for (range, style) in layer.iter() {
            for slot in &mut slots[range.start.min(len)..range.end.min(len)] {
                *slot = Some(style.clone());
            }
        }
    }
    let mut runs: Vec<(Range<usize>, S)> = Vec::new();
    let mut index = 0;
    while index < len {
        let Some(style) = slots[index].clone() else {
            index += 1;
            continue;
        };
        let mut end = index + 1;
        while end < len && slots[end].as_ref() == Some(&style) {
            end += 1;
        }
        runs.push((index..end, style));
        index = end;
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_expand_to_the_next_stop_and_offsets_map_both_ways() {
        let line = DisplayLine::new("a\tbc\té", 4);
        assert_eq!(line.text, "a   bc  é");
        assert_eq!(line.to_display(1), 1);
        assert_eq!(line.to_display(2), 4);
        assert_eq!(line.range_to_display(2..4), 4..6);
        assert_eq!(line.to_display(5), 8);
        assert_eq!(line.to_display(8), line.text.len());
        assert_eq!(line.column(2), 4);
        // Clicking inside an expanded tab lands before the tab.
        assert_eq!(line.source_for_column(2), 1);
        assert_eq!(line.source_for_column(4), 2);
        // A column past the end lands at the end.
        assert_eq!(line.source_for_column(99), "a\tbc\té".len());
        // Never inside a multi-byte character.
        assert_eq!(line.source_for_column(8), 5);
    }

    #[test]
    fn later_layers_win_and_runs_merge() {
        let syntax = [(0..4, 'k'), (5..9, 's')];
        let selection = [(2..6, 'S')];
        let runs = flatten(10, &[&syntax, &selection]);
        assert_eq!(runs, vec![(0..2, 'k'), (2..6, 'S'), (6..9, 's')]);
        assert!(flatten::<char>(0, &[&[]]).is_empty());
        assert_eq!(flatten(3, &[&[(0..9, 'x')]]), vec![(0..3, 'x')]);
    }
}
