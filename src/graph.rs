//! Commit graph lane layout (the colored "railway" lines in the history view).
//!
//! Commits must be given in topological order (children before parents).
//! For every row we compute which lane (column) the commit node sits in and the
//! line segments to draw in that row. Each row is split in two halves:
//! `Top -> Mid` (lines coming from children above) and `Mid -> Bottom`
//! (lines leaving towards parents below).

use git2::Oid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pos {
    Top,
    Mid,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub from_lane: usize,
    pub from_pos: Pos,
    pub to_lane: usize,
    pub to_pos: Pos,
    /// Lane index used to pick the color.
    pub color: usize,
}

#[derive(Clone, Debug, Default)]
pub struct GraphRow {
    /// Lane of the commit node.
    pub lane: usize,
    pub segments: Vec<Segment>,
    /// Number of lanes used in this row (for width).
    pub width: usize,
}

/// Compute graph rows for commits given as `(oid, parents)` in topological order.
pub fn layout(commits: &[(Oid, Vec<Oid>)]) -> Vec<GraphRow> {
    // Each lane remembers which commit it is waiting for, plus its color.
    let mut lanes: Vec<Option<(Oid, usize)>> = Vec::new();
    let mut next_color = 0usize;
    let mut rows = Vec::with_capacity(commits.len());

    for (oid, parents) in commits {
        let mut segments = Vec::new();

        // Lanes that were waiting for this commit.
        let waiting: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| match l {
                Some((o, _)) if o == oid => Some(i),
                _ => None,
            })
            .collect();

        let (lane, color) = if let Some(&first) = waiting.first() {
            (first, lanes[first].unwrap().1)
        } else {
            // A branch tip: take the first free lane.
            let idx = first_free(&lanes);
            let c = next_color;
            next_color += 1;
            if idx == lanes.len() {
                lanes.push(None);
            }
            (idx, c)
        };

        // Top half: pass-through lanes and lanes merging into this node.
        for (i, l) in lanes.iter().enumerate() {
            if let Some((o, c)) = l {
                if o == oid {
                    segments.push(Segment {
                        from_lane: i,
                        from_pos: Pos::Top,
                        to_lane: lane,
                        to_pos: Pos::Mid,
                        color: *c,
                    });
                } else {
                    segments.push(Segment {
                        from_lane: i,
                        from_pos: Pos::Top,
                        to_lane: i,
                        to_pos: Pos::Bottom,
                        color: *c,
                    });
                }
            }
        }

        // Free every lane that was waiting for this commit.
        for &i in &waiting {
            lanes[i] = None;
        }

        // Bottom half: lines towards the parents.
        for (pi, parent) in parents.iter().enumerate() {
            // Is another lane already heading to this parent?
            let existing = lanes.iter().enumerate().find_map(|(i, l)| match l {
                Some((o, c)) if o == parent => Some((i, *c)),
                _ => None,
            });

            let (target, c) = if let Some(found) = existing.filter(|(i, _)| pi > 0 || *i < lane) {
                // Join an existing lane (for the first parent only when that lane is to the left,
                // so the main line stays straight and side branches fold into it).
                found
            } else if pi == 0 {
                lanes[lane] = Some((*parent, color));
                (lane, color)
            } else {
                let idx = first_free_except(&lanes, lane);
                let c = next_color;
                next_color += 1;
                if idx >= lanes.len() {
                    lanes.resize(idx + 1, None);
                }
                lanes[idx] = Some((*parent, c));
                (idx, c)
            };

            segments.push(Segment {
                from_lane: lane,
                from_pos: Pos::Mid,
                to_lane: target,
                to_pos: Pos::Bottom,
                color: if pi == 0 { color } else { c },
            });
        }

        while matches!(lanes.last(), Some(None)) {
            lanes.pop();
        }

        let width = segments
            .iter()
            .map(|s| s.from_lane.max(s.to_lane) + 1)
            .max()
            .unwrap_or(0)
            .max(lane + 1);

        rows.push(GraphRow {
            lane,
            segments,
            width,
        });
    }
    rows
}

fn first_free(lanes: &[Option<(Oid, usize)>]) -> usize {
    lanes
        .iter()
        .position(|l| l.is_none())
        .unwrap_or(lanes.len())
}

fn first_free_except(lanes: &[Option<(Oid, usize)>], except: usize) -> usize {
    lanes
        .iter()
        .enumerate()
        .position(|(i, l)| l.is_none() && i != except)
        .unwrap_or(lanes.len().max(except + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    #[test]
    fn linear_history_uses_one_lane() {
        let c = vec![
            (oid(3), vec![oid(2)]),
            (oid(2), vec![oid(1)]),
            (oid(1), vec![]),
        ];
        let rows = layout(&c);
        assert!(rows.iter().all(|r| r.lane == 0 && r.width == 1));
    }

    #[test]
    fn merge_opens_second_lane_and_closes_it() {
        // 4 = merge(3, 2); 3 -> 1; 2 -> 1; 1 root
        let c = vec![
            (oid(4), vec![oid(3), oid(2)]),
            (oid(3), vec![oid(1)]),
            (oid(2), vec![oid(1)]),
            (oid(1), vec![]),
        ];
        let rows = layout(&c);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[0].width, 2);
        assert_eq!(rows[1].lane, 0);
        assert_eq!(rows[2].lane, 1);
        // commit 2's parent (1) is already awaited by lane 0 -> line goes to lane 0
        assert!(rows[2]
            .segments
            .iter()
            .any(|s| s.from_lane == 1 && s.to_lane == 0 && s.to_pos == Pos::Bottom));
        assert_eq!(rows[3].lane, 0);
        assert_eq!(rows[3].width, 1);
    }

    #[test]
    fn two_tips_sharing_parent() {
        let c = vec![
            (oid(3), vec![oid(1)]),
            (oid(2), vec![oid(1)]),
            (oid(1), vec![]),
        ];
        let rows = layout(&c);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[2].lane, 0);
    }

    #[test]
    fn main_line_stays_left() {
        let c = vec![
            (oid(5), vec![oid(3)]),
            (oid(4), vec![oid(2)]),
            (oid(3), vec![oid(2)]),
            (oid(2), vec![]),
        ];
        let rows = layout(&c);
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[2].lane, 0);
        assert!(rows[2]
            .segments
            .iter()
            .any(|s| s.from_lane == 0 && s.to_lane == 0 && s.from_pos == Pos::Mid));
        assert_eq!(rows[3].lane, 0);
        // lane 1 folds into the parent node from the top
        assert!(rows[3]
            .segments
            .iter()
            .any(|s| s.from_lane == 1 && s.to_lane == 0));
    }
}
