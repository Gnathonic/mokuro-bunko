//! Text blocks: what a block detector finds on a page, and the box arithmetic every
//! such detector needs (score order, overlap suppression, containers).
//!
//! A block detector boxes whole pieces of text (a balloon's lines together, a sound
//! effect), not single lines. A recognizer that reads blocks takes its crops from
//! these boxes and finds the lines itself.

use crate::error::Result;
use crate::image::BgrImage;
use crate::lines::DetectorInfo;

/// One text block of a page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextBlock {
    /// `[x0, y0, x1, y1]` in page pixels.
    pub bbox: [f32; 4],
    pub score: f32,
}

/// A detector of text blocks. Implementations are shared by the detect workers.
pub trait BlockDetector: Send + Sync + std::fmt::Debug {
    /// The page's text blocks, and what was run.
    fn detect(&self, bgr: &BgrImage) -> Result<(Vec<TextBlock>, DetectorInfo)>;
}

fn area(b: &[f32; 4]) -> f32 {
    (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0)
}

fn intersection(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    (a[2].min(b[2]) - a[0].max(b[0])).max(0.0) * (a[3].min(b[3]) - a[1].max(b[1])).max(0.0)
}

/// Greedy overlap suppression: the boxes in falling score order, each kept unless it
/// overlaps a kept one by more than `iou`.
pub fn suppress(mut boxes: Vec<TextBlock>, iou: f32) -> Vec<TextBlock> {
    boxes.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<TextBlock> = Vec::new();
    for b in boxes {
        let overlaps = kept.iter().any(|k| {
            let shared = intersection(&k.bbox, &b.bbox);
            let union = area(&k.bbox) + area(&b.bbox) - shared;
            union > 0.0 && shared / union > iou
        });
        if !overlaps {
            kept.push(b);
        }
    }
    kept
}

/// Without the boxes that only wrap others: a box holding at least `parts` smaller
/// boxes, each at least `inside` of its area within it, is a box around several
/// blocks (two balloons, a whole panel's text) and its parts are the blocks.
pub fn drop_containers(boxes: Vec<TextBlock>, parts: usize, inside: f32) -> Vec<TextBlock> {
    let wraps = |outer: &TextBlock| {
        boxes
            .iter()
            .filter(|inner| {
                let a = area(&inner.bbox);
                a > 0.0
                    && a < area(&outer.bbox)
                    && intersection(&inner.bbox, &outer.bbox) / a >= inside
            })
            .count()
            >= parts
    };
    boxes.iter().filter(|b| !wraps(b)).copied().collect()
}

/// Without the boxes that lie in a bigger one: a box at least `inside` of whose area is
/// within a bigger box is a part of that block, boxed a second time, and reading both
/// would give its text twice. Run after [`drop_containers`], which takes the boxes that
/// wrap several blocks away first.
pub fn drop_parts(boxes: Vec<TextBlock>, inside: f32) -> Vec<TextBlock> {
    let part = |inner: &TextBlock| {
        let a = area(&inner.bbox);
        boxes.iter().any(|outer| {
            area(&outer.bbox) > a && a > 0.0 && intersection(&inner.bbox, &outer.bbox) / a >= inside
        })
    };
    boxes.iter().filter(|b| !part(b)).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_box_inside_a_bigger_one_is_its_part() {
        // A balloon and its first two columns boxed again, and a block of its own.
        let boxes = vec![
            b(0.0, 0.0, 90.0, 100.0, 0.9),
            b(40.0, 2.0, 88.0, 98.0, 0.6),
            b(200.0, 0.0, 300.0, 100.0, 0.7),
        ];
        assert_eq!(drop_parts(boxes.clone(), 0.8), vec![boxes[0], boxes[2]],);
        // Neighbours that overlap a little are both blocks.
        let near = vec![
            b(0.0, 0.0, 50.0, 100.0, 0.9),
            b(45.0, 0.0, 120.0, 100.0, 0.8),
        ];
        assert_eq!(drop_parts(near.clone(), 0.8), near);
    }

    fn b(x0: f32, y0: f32, x1: f32, y1: f32, score: f32) -> TextBlock {
        TextBlock {
            bbox: [x0, y0, x1, y1],
            score,
        }
    }

    #[test]
    fn the_better_of_two_overlapping_boxes_is_kept() {
        let kept = suppress(
            vec![
                b(0.0, 0.0, 10.0, 10.0, 0.6),
                b(1.0, 0.0, 11.0, 10.0, 0.9),
                b(50.0, 50.0, 60.0, 60.0, 0.4),
            ],
            0.5,
        );
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].score, 0.9);
        assert_eq!(kept[1].score, 0.4);
        // Boxes that only touch are both kept.
        assert_eq!(
            suppress(
                vec![b(0.0, 0.0, 10.0, 10.0, 0.5), b(10.0, 0.0, 20.0, 10.0, 0.5)],
                0.5
            )
            .len(),
            2
        );
    }

    #[test]
    fn a_box_around_two_blocks_goes_and_its_blocks_stay() {
        let boxes = vec![
            b(0.0, 0.0, 100.0, 100.0, 0.9),
            b(5.0, 5.0, 40.0, 95.0, 0.8),
            b(60.0, 5.0, 95.0, 95.0, 0.8),
            b(200.0, 0.0, 300.0, 100.0, 0.7),
        ];
        let kept = drop_containers(boxes.clone(), 2, 0.8);
        assert_eq!(kept, boxes[1..].to_vec());
        // One block inside another is not enough to call the outer one a container.
        let nested = vec![b(0.0, 0.0, 100.0, 100.0, 0.9), b(5.0, 5.0, 40.0, 95.0, 0.8)];
        assert_eq!(drop_containers(nested.clone(), 2, 0.8), nested);
    }
}
